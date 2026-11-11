//! `start`: the superagent session in the cockpit's left pane, in the
//! harness of your choice, briefed to calibrate or to route (PLAN §2).

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::calibration;
use crate::ensure::{ensure, EnsureReport};
use crate::herdr::Herdr;
use crate::ipc;
use crate::paths::Paths;
use crate::profile::Profile;
use crate::staterepo::StateRepo;

#[derive(Debug, Clone, Serialize)]
pub struct StartReport {
    pub ensure: EnsureReport,
    pub harness: String,
    pub model: String,
    pub session_pane_id: Option<String>,
    /// `started`, `blocked` (the prompt was not delivered), `already_running` or `no_pane`.
    pub session: String,
    /// `calibration` or `brief`: which prompt the session received.
    pub prompt: Option<String>,
    pub lease: Option<String>,
    pub reindexed: Option<usize>,
}

pub fn start(profile_name: &str, harness: Option<String>, model: Option<String>, take_over: bool) -> Result<StartReport, String> {
    let paths = Paths::for_profile(profile_name);
    paths.ensure_dirs().map_err(|e| e.to_string())?;
    let mut profile = Profile::load_or_init(&paths.profile_file())?;
    let harness = harness.unwrap_or_else(|| profile.superagent.harness.clone());
    let model = model.unwrap_or_else(|| profile.superagent.model.clone());
    if harness != profile.superagent.harness || model != profile.superagent.model {
        profile.superagent.harness = harness.clone();
        profile.superagent.model = model.clone();
        profile.save(&paths.profile_file())?;
    }

    // The state repo first: pull what another machine recorded, take the lease.
    let mut lease = None;
    let mut reindexed = None;
    let daemon_running = ipc::ping(&paths.socket_path()).is_ok();
    if let Some(repo) = StateRepo::open_or_init(&profile, &paths.profile_dir())? {
        if !daemon_running {
            let taken = repo.acquire(take_over).map_err(|e| e.to_string())?;
            lease = Some(format!("{} until {}", taken.machine, taken.expires));
            if !paths.db_path().exists() {
                let store = crate::store::sqlite::SqliteStore::open(&paths.db_path()).map_err(|e| e.to_string())?;
                reindexed = Some(repo.reindex(&store)?);
            }
        }
    }

    let skills = calibration::install_skill();
    if !skills.is_empty() {
        tracing::info!(harnesses = ?skills, "installed the superagent skill");
    }
    let report = ensure(profile_name, true)?;
    let herdr = Herdr::from_env().map_err(|e| e.to_string())?;
    let panes = herdr.pane_list().map_err(|e| e.to_string())?;
    let workspace = report.workspace_id.clone().unwrap_or_default();
    let session_pane = panes
        .iter()
        .find(|p| p.workspace_id == workspace && p.label.as_deref() == Some("superagent"))
        .cloned();
    let Some(pane) = session_pane else {
        return Ok(StartReport {
            ensure: report,
            harness,
            model,
            session_pane_id: None,
            session: "no_pane".into(),
            prompt: None,
            lease,
            reindexed,
        });
    };
    let socket = paths.socket_path();
    let _ = ipc::call(
        &socket,
        "session_set",
        json!({ "pane_id": pane.pane_id, "harness": harness, "model": model }),
        Duration::from_secs(5),
    );
    if pane.agent.is_some() {
        let _ = herdr.call("agent.focus", json!({ "target": pane.pane_id }));
        return Ok(StartReport {
            ensure: report,
            harness,
            model,
            session_pane_id: Some(pane.pane_id),
            session: "already_running".into(),
            prompt: None,
            lease,
            reindexed,
        });
    }

    let mut args = calibration::model_args(&harness, &model);
    args.extend(profile.harness_flags.always(&harness, "session", &paths.state_dir()));
    // The pane's shell may have a short PATH: export the login shell's before the harness starts.
    let exports = [
        format!("PATH={}", crate::envpath::full_path()),
        format!("SUPERAGENT_PROFILE={profile_name}"),
        "SUPERAGENT_ROLE=superagent".to_string(),
        // A greyed-out suggested reply reads like a typed approval; approvals only count through `approve`.
        "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=false".to_string(),
    ];
    let _ = herdr.pane_run(&pane.pane_id, &format!("export {}; clear", crate::ensure::shell_words(&exports)));
    std::thread::sleep(Duration::from_millis(800));
    let start = || {
        herdr.call(
            "agent.start",
            json!({ "name": agent_name(profile_name), "kind": harness, "pane_id": pane.pane_id, "args": args, "timeout_ms": 60000 }),
        )
    };
    start().map_err(|e| format!("agent.start {harness} in {}: {e}", pane.pane_id))?;
    // A brand-new pane sometimes swallows the first typed command: check an agent appeared, else start once more.
    let _ = herdr.call_with_timeout(
        "agent.wait",
        json!({ "target": pane.pane_id, "until": ["idle", "done", "blocked", "working"], "timeout_ms": 20000 }),
        Duration::from_secs(25),
    );
    let has_agent = herdr
        .pane_list()
        .ok()
        .and_then(|p| p.into_iter().find(|p| p.pane_id == pane.pane_id))
        .map(|p| p.agent.is_some())
        .unwrap_or(false);
    if !has_agent {
        tracing::warn!(pane = %pane.pane_id, "no agent detected after agent.start; starting once more");
        let _ = herdr.pane_run(&pane.pane_id, "clear");
        std::thread::sleep(Duration::from_millis(800));
        start().map_err(|e| format!("agent.start {harness} in {} (second try): {e}", pane.pane_id))?;
    }

    // The brief goes to a file; the prompt stays one line.
    let (kind, text) = if profile.calibrated {
        ("brief", calibration::brief(&profile, &paths.profile_dir()))
    } else {
        ("calibration", calibration::calibration(profile_name))
    };
    let brief_path = paths.state_dir().join("brief.md");
    std::fs::write(&brief_path, &text).map_err(|e| e.to_string())?;
    let prompt = format!(
        "You are the superagent for herdr profile '{profile_name}'. Read {} and follow it. Start by running `{} status`.",
        brief_path.display(),
        crate::envpath::self_path()
    );
    // The workspace and daemon are up either way: a session that never took the
    // prompt is reported, not an error.
    let session = match prompt_when_ready(&herdr, &pane.pane_id, &prompt) {
        Ok(()) => "started",
        Err(e) => {
            tracing::warn!("{e}");
            "blocked"
        }
    };
    Ok(StartReport {
        ensure: report,
        harness,
        model,
        session_pane_id: Some(pane.pane_id),
        session: session.into(),
        prompt: Some(kind.into()),
        lease,
        reindexed,
    })
}

/// The herdr agent name of a profile's session. herdr names are unique among
/// live agents and must match `[a-z][a-z0-9_-]{0,31}`.
fn agent_name(profile: &str) -> String {
    format!("superagent-{}", profile.to_ascii_lowercase()).chars().take(32).collect()
}

/// Waits for the session, then prompts it. `agent.start` can return before the
/// harness is ready (`agent_not_ready`), and the harness can stop at a dialog
/// only the person can answer (`agent_blocked`: folder trust, login,
/// permissions): then herdr notifies them and we wait up to ten minutes.
fn prompt_when_ready(herdr: &Herdr, pane_id: &str, prompt: &str) -> Result<(), String> {
    let wait = |ms: u64| {
        let _ = herdr.call_with_timeout(
            "agent.wait",
            json!({ "target": pane_id, "until": ["idle", "done"], "timeout_ms": ms }),
            Duration::from_millis(ms + 5000),
        );
    };
    // No blind wait: the prompt is tried at once and retried while the harness boots,
    // so the brief lands the moment the session can take it.
    let mut deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut notified = false;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match herdr.call("agent.prompt", json!({ "target": pane_id, "text": prompt })) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                if last.contains("agent_blocked") && !notified {
                    notified = true;
                    let _ = herdr.notify(
                        "superagent session needs you",
                        Some(&format!("Answer the prompt in pane {pane_id}; the brief follows.")),
                    );
                    wait(600_000);
                    deadline = std::time::Instant::now() + Duration::from_secs(60);
                    continue;
                }
                if !(last.contains("agent_not_ready") || last.contains("not an active") || last.contains("agent_blocked")) {
                    return Err(format!("agent.prompt: {last}; paste this into pane {pane_id}: {prompt}"));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    Err(format!(
        "the superagent session did not become ready ({last}); paste this into pane {pane_id}: {prompt}"
    ))
}

/// `stop`: the daemon finishes its batch, releases the lease and exits; the
/// session pane stays, so the conversation can be read.
pub fn stop(profile_name: &str) -> Result<Value, String> {
    let paths = Paths::for_profile(profile_name);
    ipc::call(
        &paths.socket_path(),
        "shutdown",
        json!({ "restart": false }),
        Duration::from_secs(10),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_names_fit_herdr() {
        assert_eq!(super::agent_name("work"), "superagent-work");
        assert_eq!(super::agent_name("My_Work"), "superagent-my_work");
        assert_eq!(super::agent_name(&"x".repeat(40)).len(), 32);
    }
}
