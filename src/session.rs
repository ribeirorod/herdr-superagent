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
    /// `started`, `already_running` or `no_pane`.
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

    let args = calibration::model_args(&harness, &model);
    // herdr agent names are unique across the session: one per profile.
    let slug: String = profile_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    herdr
        .call(
            "agent.start",
            json!({ "name": format!("superagent-{slug}"), "kind": harness, "pane_id": pane.pane_id, "args": args, "timeout_ms": 60000 }),
        )
        .map_err(|e| format!("agent.start {harness} in {}: {e}", pane.pane_id))?;

    // The brief goes to a file; the prompt stays one line.
    let (kind, text) = if profile.calibrated {
        ("brief", calibration::brief(&profile, &paths.profile_dir()))
    } else {
        (
            "calibration",
            calibration::CALIBRATION_PROMPT.replace("`SUPERAGENT_PROFILE`", &format!("`{profile_name}`")),
        )
    };
    let brief_path = paths.state_dir().join("brief.md");
    std::fs::write(&brief_path, &text).map_err(|e| e.to_string())?;
    let prompt = format!(
        "You are the superagent for herdr profile '{profile_name}'. Read {} and follow it. Start by running `herdr-superagent status`.",
        brief_path.display()
    );
    prompt_when_ready(&herdr, &pane.pane_id, &prompt)?;
    Ok(StartReport {
        ensure: report,
        harness,
        model,
        session_pane_id: Some(pane.pane_id),
        session: "started".into(),
        prompt: Some(kind.into()),
        lease,
        reindexed,
    })
}

/// `agent.start` can return with `launch_pending` before the harness is ready
/// and `agent.prompt` then answers `agent_not_ready`: wait for an idle agent,
/// then prompt, retrying a few times.
fn prompt_when_ready(herdr: &Herdr, pane_id: &str, prompt: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let _ = herdr.call(
        "agent.wait",
        json!({ "target": pane_id, "until": ["idle", "done"], "timeout_ms": 90000 }),
    );
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match herdr.call("agent.prompt", json!({ "target": pane_id, "text": prompt })) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                if !(last.contains("agent_not_ready") || last.contains("not an active") || last.contains("agent_blocked")) {
                    return Err(format!("agent.prompt: {last}"));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    Err(format!(
        "agent.prompt: the superagent session did not become ready in time ({last}); paste the prompt into pane {pane_id} by hand"
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
