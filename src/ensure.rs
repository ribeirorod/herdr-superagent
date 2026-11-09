//! `ensure`: idempotent setup run by the plugin's startup hook, by `start`
//! and by every action. It creates the superagent workspace with its cockpit
//! layout when missing, starts the daemon in its pane, and restarts a daemon
//! whose version differs from this binary (herdr does not restart running
//! processes after `plugin update`).

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::daemon::VERSION;
use crate::herdr::{Herdr, HerdrError};
use crate::ipc;
use crate::paths::Paths;
use crate::profile::Profile;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EnsureOutcome {
    /// The daemon of this version was already running.
    Running,
    /// A daemon of another version was told to restart into this binary.
    Restarted { from: String },
    /// The workspace and the daemon pane were created.
    Created,
    /// The workspace existed; the daemon was started in its pane.
    Started,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EnsureReport {
    pub profile: String,
    pub outcome: EnsureOutcome,
    pub workspace_id: Option<String>,
    pub daemon_pane_id: Option<String>,
    pub session_pane_id: Option<String>,
    pub version: String,
    pub warnings: Vec<String>,
}

pub fn workspace_label(profile: &str) -> String {
    format!("superagent · {profile}")
}

/// The argv that runs the daemon: the binary on PATH when available (so herdr
/// can resume it), else this executable's absolute path.
pub fn daemon_command(profile: &str) -> Vec<String> {
    // On the login shell's PATH, which the pane receives; herdr's resume command needs a plain name.
    let program = if crate::envpath::locate("herdr-superagent").is_some() {
        "herdr-superagent".to_string()
    } else {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "herdr-superagent".into())
    };
    vec![program, "daemon".into(), "--profile".into(), profile.into()]
}

pub fn ensure(profile_name: &str, focus: bool) -> Result<EnsureReport, String> {
    let paths = Paths::for_profile(profile_name);
    paths.ensure_dirs().map_err(|e| format!("create profile directories: {e}"))?;
    Profile::load_or_init(&paths.profile_file())?;
    let herdr = Herdr::from_env().map_err(|e| e.to_string())?;
    let mut warnings = Vec::new();
    if crate::envpath::locate("herdr-superagent").is_none() {
        warnings.push("herdr-superagent is not on your shell's PATH: herdr cannot resume the daemon after a restart, and workers cannot call it. Put the binary on PATH.".to_string());
    }
    let mut report = EnsureReport {
        profile: profile_name.to_string(),
        outcome: EnsureOutcome::Running,
        workspace_id: None,
        daemon_pane_id: None,
        session_pane_id: None,
        version: VERSION.to_string(),
        warnings,
    };

    if let Ok(ping) = ipc::ping(&paths.socket_path()) {
        let running = ping["version"].as_str().unwrap_or("").to_string();
        report.daemon_pane_id = ping["pane_id"].as_str().map(str::to_string);
        if running == VERSION {
            report.outcome = EnsureOutcome::Running;
        } else {
            ipc::call(&paths.socket_path(), "shutdown", json!({ "restart": true }), Duration::from_secs(5)).map_err(|e| e.to_string())?;
            wait_for_daemon(&paths, Some(VERSION), Duration::from_secs(20))?;
            report.outcome = EnsureOutcome::Restarted { from: running };
        }
        report.workspace_id = find_workspace(&herdr, profile_name)?.map(|w| w.workspace_id);
        if focus {
            if let Some(id) = &report.workspace_id {
                let _ = herdr.workspace_focus(id);
            }
        }
        return Ok(report);
    }

    let label = workspace_label(profile_name);
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let debug_flag = if crate::logging::debug_enabled(false) { "1" } else { "" };
    // The shell that runs `start` has the full PATH; herdr panes often get a non-login one.
    // herdr panes often get a non-login PATH: give them the login shell's, merged with ours.
    let path_env = crate::envpath::full_path();
    let mut env = vec![
        ("SUPERAGENT_PROFILE", profile_name.to_string()),
        ("HERDR_SUPERAGENT_DEBUG", debug_flag.to_string()),
        ("PATH", path_env),
    ];
    env.extend(forwarded_env());
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let command = daemon_command(profile_name);
    tracing::debug!(target: "ensure", ?command, debug_mode = debug_flag, "daemon command");
    match find_workspace(&herdr, profile_name)? {
        None => {
            let (workspace_id, root_pane) = herdr.workspace_create(&home, &label, &env, focus).map_err(|e| e.to_string())?;
            let panes = herdr.pane_list().map_err(|e| e.to_string())?;
            let tab_id = panes.iter().find(|p| p.pane_id == root_pane).map(|p| p.tab_id.clone());
            let layout = herdr
                .call(
                    "layout.apply",
                    json!({
                        // herdr refuses tab_id together with workspace_id; the tab names the workspace.
                        "tab_id": tab_id,
                        "tab_label": "cockpit",
                        "focus": focus,
                        "root": cockpit_layout(profile_name, &home, paths.state_dir().display().to_string().as_str(), &command),
                    }),
                )
                .map_err(|e| e.to_string())?;
            let _ = herdr.report_workspace_metadata(&workspace_id, &[("sa_profile", profile_name)], crate::herdr::seq_now());
            report.workspace_id = Some(workspace_id);
            let (session, daemon) = layout_panes(&layout);
            report.session_pane_id = session;
            report.daemon_pane_id = daemon;
            report.outcome = EnsureOutcome::Created;
        }
        Some(workspace) => {
            report.workspace_id = Some(workspace.workspace_id.clone());
            let panes = herdr.pane_list().map_err(|e| e.to_string())?;
            let daemon_pane = panes
                .iter()
                .find(|p| p.workspace_id == workspace.workspace_id && p.label.as_deref() == Some("daemon"));
            match daemon_pane {
                Some(pane) => {
                    herdr.pane_run(&pane.pane_id, &shell_words(&command)).map_err(|e| e.to_string())?;
                    report.daemon_pane_id = Some(pane.pane_id.clone());
                }
                None => {
                    let layout = herdr
                        .layout_apply(
                            &workspace.workspace_id,
                            "cockpit",
                            cockpit_layout(profile_name, &home, paths.state_dir().display().to_string().as_str(), &command),
                            focus,
                        )
                        .map_err(|e| e.to_string())?;
                    let (session, daemon) = layout_panes(&layout);
                    report.session_pane_id = session;
                    report.daemon_pane_id = daemon;
                }
            }
            if focus {
                let _ = herdr.workspace_focus(&workspace.workspace_id);
            }
            report.outcome = EnsureOutcome::Started;
        }
    }
    wait_for_daemon(&paths, Some(VERSION), Duration::from_secs(20))?;
    if let Ok(ping) = ipc::ping(&paths.socket_path()) {
        if let Some(pane) = ping["pane_id"].as_str() {
            report.daemon_pane_id = Some(pane.to_string());
        }
    }
    Ok(report)
}

/// Overrides set in the shell that runs `start`, which the panes must share:
/// without the state and config roots the daemon would open another profile
/// directory and listen on another socket.
fn forwarded_env() -> Vec<(&'static str, String)> {
    ["HERDR_SUPERAGENT_STATE_DIR", "HERDR_SUPERAGENT_CONFIG_DIR", "HERDR_SUPERAGENT_LOG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()).map(|v| (key, v)))
        .collect()
}

/// The cockpit (PLAN §8b): the superagent session on the left, the daemon on
/// the right. Phase 6 replaces the daemon pane's output with the dashboard.
fn cockpit_layout(profile: &str, home: &str, state_dir: &str, daemon_command: &[String]) -> Value {
    let debug = if crate::logging::debug_enabled(false) { "1" } else { "" };
    let path_env = crate::envpath::full_path();
    let env = |role: &str| {
        let mut env = json!({ "SUPERAGENT_PROFILE": profile, "SUPERAGENT_ROLE": role, "HERDR_SUPERAGENT_DEBUG": debug, "PATH": path_env });
        for (key, value) in forwarded_env() {
            env[key] = json!(value);
        }
        env
    };
    json!({
        "type": "split",
        "direction": "right",
        "ratio": 0.65,
        "first": { "type": "pane", "label": "superagent", "cwd": home, "env": env("superagent") },
        "second": { "type": "pane", "label": "daemon", "cwd": state_dir, "command": daemon_command, "env": env("daemon") }
    })
}

/// The pane ids of the session and daemon panes from a `layout.apply` result.
fn layout_panes(layout: &Value) -> (Option<String>, Option<String>) {
    fn walk(node: &Value, out: &mut Vec<(String, String)>) {
        if node.get("type").and_then(Value::as_str) == Some("split") {
            if let Some(first) = node.get("first") {
                walk(first, out);
            }
            if let Some(second) = node.get("second") {
                walk(second, out);
            }
        } else if let (Some(label), Some(pane)) = (
            node.get("label").and_then(Value::as_str),
            node.get("pane_id").and_then(Value::as_str),
        ) {
            out.push((label.to_string(), pane.to_string()));
        }
    }
    let mut panes = Vec::new();
    if let Some(root) = layout.get("root").or_else(|| layout.pointer("/layout/root")) {
        walk(root, &mut panes);
    }
    let find = |label: &str| panes.iter().find(|(l, _)| l == label).map(|(_, p)| p.clone());
    (find("superagent"), find("daemon"))
}

fn find_workspace(herdr: &Herdr, profile: &str) -> Result<Option<crate::herdr::WorkspaceInfo>, String> {
    let label = workspace_label(profile);
    let workspaces = herdr.workspace_list().map_err(|e: HerdrError| e.to_string())?;
    Ok(workspaces.into_iter().find(|w| w.label == label))
}

fn wait_for_daemon(paths: &Paths, version: Option<&str>, timeout: Duration) -> Result<Value, String> {
    let start = Instant::now();
    loop {
        if let Ok(ping) = ipc::ping(&paths.socket_path()) {
            if version.map(|v| ping["version"] == v).unwrap_or(true) {
                return Ok(ping);
            }
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "the daemon did not answer on {} within {}s; check the daemon pane",
                paths.socket_path().display(),
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Quotes argv for a POSIX shell line typed into a pane.
pub fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:".contains(c)) {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_words_and_reads_layout_panes() {
        assert_eq!(
            shell_words(&["herdr-superagent".into(), "daemon".into(), "--profile".into(), "my work".into()]),
            "herdr-superagent daemon --profile 'my work'"
        );
        let layout = json!({ "root": { "type": "split", "first": { "type": "pane", "label": "superagent", "pane_id": "w1:p1" }, "second": { "type": "pane", "label": "daemon", "pane_id": "w1:p2" } } });
        assert_eq!(layout_panes(&layout), (Some("w1:p1".into()), Some("w1:p2".into())));
        assert_eq!(workspace_label("work"), "superagent · work");
    }
}
