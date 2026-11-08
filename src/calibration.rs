//! Calibration support (PLAN §4): the inventory the superagent reads before
//! it interviews you, and the prompts handed to the superagent session.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::profile::{expand_home, Profile};

pub const CALIBRATION_PROMPT: &str = include_str!("../skills/superagent/calibration.md");
pub const BRIEF_TEMPLATE: &str = include_str!("../skills/superagent/brief.md");

pub const HARNESSES: &[&str] = &["claude", "codex", "pi"];

/// Where harnesses usually live when a non-login shell (such as a herdr pane) lacks them on PATH.
pub const COMMON_BIN_DIRS: &[&str] = &[
    "~/.local/bin",
    "~/.npm-global/bin",
    "~/.cargo/bin",
    "~/.opencode/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
];

#[derive(Debug, Clone, Serialize)]
pub struct HarnessInfo {
    pub name: String,
    pub installed: bool,
    /// Found on this process's PATH (a herdr pane may lack the shell's PATH).
    pub on_path: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// MCP servers configured for this harness; `None` when the layout is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoInfo {
    pub path: String,
    pub exists: bool,
    pub git: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Doctor {
    pub profile: String,
    pub calibrated: bool,
    pub inside_herdr: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herdr_version: Option<String>,
    pub harnesses: Vec<HarnessInfo>,
    pub repos: Vec<RepoInfo>,
    pub state_repo: String,
    pub superagent_on_path: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superagent_path: Option<String>,
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

/// `which`, then the usual install directories: (path, on_path).
pub fn locate(name: &str) -> Option<(String, bool)> {
    if let Some(path) = which(name) {
        return Some((path, true));
    }
    COMMON_BIN_DIRS
        .iter()
        .map(|dir| Path::new(&expand_home(dir)).join(name))
        .find(|p| p.is_file())
        .map(|p| (p.display().to_string(), false))
}

fn run(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let output = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// MCP servers per harness, read where each harness keeps them.
fn mcp_servers(harness: &str, home: &Path) -> (Option<Vec<String>>, Option<String>) {
    match harness {
        "claude" => {
            // `claude mcp list` prints "name: command …" lines; fall back to ~/.claude.json.
            // Lines look like `name: <url or command> (HTTP) - ✓ Connected`; names may contain colons.
            if let Some(out) = run("claude", &["mcp", "list"], Duration::from_secs(20)) {
                let names: Vec<String> = out
                    .lines()
                    .filter(|line| !line.starts_with("Checking"))
                    .filter_map(|line| {
                        let (name, rest) = line.split_once(": ")?;
                        let name = name.trim();
                        if name.is_empty() {
                            return None;
                        }
                        let down = rest.contains("Failed") || rest.contains("Not configured");
                        Some(if down { format!("{name} (down)") } else { name.to_string() })
                    })
                    .collect();
                return (Some(names), None);
            }
            let text = std::fs::read_to_string(home.join(".claude.json")).ok();
            let names = text
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.get("mcpServers").and_then(|m| m.as_object()).map(|m| m.keys().cloned().collect()))
                .unwrap_or_default();
            (Some(names), Some("read from ~/.claude.json".into()))
        }
        "codex" => {
            let text = std::fs::read_to_string(home.join(".codex").join("config.toml")).ok();
            let names = text
                .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
                .and_then(|v| v.get("mcp_servers").and_then(|m| m.as_table()).map(|m| m.keys().cloned().collect()))
                .unwrap_or_default();
            (Some(names), Some("read from ~/.codex/config.toml".into()))
        }
        "pi" => {
            let settings = home.join(".pi").join("agent").join("settings.json");
            match std::fs::read_to_string(&settings)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            {
                Some(v) => {
                    let names = v
                        .get("mcpServers")
                        .or_else(|| v.get("mcp"))
                        .and_then(|m| m.as_object())
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    (Some(names), Some("read from ~/.pi/agent/settings.json".into()))
                }
                None => (None, Some("pi keeps MCP in extensions; no settings file found to read".into())),
            }
        }
        _ => (None, None),
    }
}

pub fn doctor(profile: &Profile) -> Doctor {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let harnesses = HARNESSES
        .iter()
        .map(|name| {
            let found = locate(name);
            let (path, on_path) = match &found {
                Some((p, on)) => (Some(p.clone()), *on),
                None => (None, false),
            };
            let (mcp_servers, mut note) = if on_path { mcp_servers(name, &home) } else { (None, None) };
            if found.is_some() && !on_path {
                note = Some(format!(
                    "installed at {} but not on this pane's PATH; `start` now passes your shell's PATH to new panes",
                    path.clone().unwrap_or_default()
                ));
            }
            HarnessInfo {
                name: name.to_string(),
                installed: found.is_some(),
                on_path,
                path,
                mcp_servers,
                note,
            }
        })
        .collect();
    let repos = profile
        .repos
        .iter()
        .map(|repo| {
            let path = expand_home(&repo.path);
            let dir = Path::new(&path);
            let git = dir.join(".git").exists();
            let branch = git
                .then(|| {
                    run("git", &["-C", &path, "rev-parse", "--abbrev-ref", "HEAD"], Duration::from_secs(5)).map(|b| b.trim().to_string())
                })
                .flatten();
            RepoInfo {
                path: repo.path.clone(),
                exists: dir.is_dir(),
                git,
                branch,
            }
        })
        .collect();
    let herdr = crate::herdr::Herdr::from_env().ok();
    Doctor {
        profile: profile.id.clone(),
        calibrated: profile.calibrated,
        inside_herdr: herdr.is_some(),
        herdr_version: herdr.and_then(|h| h.ping().ok()),
        harnesses,
        repos,
        state_repo: profile.state_repo.kind.clone(),
        superagent_on_path: which("herdr-superagent").is_some(),
        superagent_path: locate("herdr-superagent").map(|(p, _)| p),
    }
}

/// The brief for a calibrated profile's session, with the profile filled in.
pub fn brief(profile: &Profile, profile_dir: &Path) -> String {
    BRIEF_TEMPLATE
        .replace("{profile}", &profile.id)
        .replace("{profile_dir}", &profile_dir.display().to_string())
}

/// The model argument each harness takes.
pub fn model_args(harness: &str, model: &str) -> Vec<String> {
    if model.trim().is_empty() {
        return Vec::new();
    }
    match harness {
        "codex" => vec!["-m".into(), model.into()],
        _ => vec!["--model".into(), model.into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_harnesses_and_repos() {
        let mut profile = Profile::new("work");
        profile.repos.push(crate::profile::Repo {
            path: "/definitely/not/here".into(),
            ..Default::default()
        });
        let report = doctor(&profile);
        assert_eq!(report.harnesses.len(), 3);
        assert!(!report.repos[0].exists);
        assert_eq!(model_args("codex", "gpt-5"), vec!["-m", "gpt-5"]);
        assert_eq!(model_args("claude", "opus"), vec!["--model", "opus"]);
        assert!(model_args("pi", "").is_empty());
        assert!(brief(&profile, Path::new("/p")).contains("profile `work`"));
        assert!(CALIBRATION_PROMPT.contains("profile set"));
    }
}
