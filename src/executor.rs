//! The executor (PLAN §8): runs assignments in herdr panes. `launch` makes a
//! worktree or workspace, starts the tier's harness and writes the brief;
//! `deliver` prompts the worker with what waits for it; the observer turns
//! herdr's pane status into reports when the worker says nothing itself.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::brief;
use crate::core::engine::{Question, WorkerReport};
use crate::core::model::*;
use crate::core::ports::Repositories;
use crate::core::stage::ticket_stage;
use crate::daemon::Daemon;
use crate::ensure::shell_words;
use crate::memory::{project_key, NOTES_PROJECT};
use crate::profile::Profile;
use crate::store::sqlite::Effect;

/// What a handler did with an effect.
pub enum Outcome {
    Done,
    /// Not ready yet (queue full, agent still starting): try again later, no failure counted.
    Defer(i64),
    Fail(String),
}

/// How long an unknown status may last before the worker counts as lost.
pub const UNKNOWN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long a worker may run without any herdr agent in its pane.
pub const NO_AGENT_TIMEOUT: Duration = Duration::from_secs(90);

/// What the observer last saw of a worker pane.
#[derive(Debug, Clone)]
pub struct WorkerSeen {
    pub status: String,
    pub completion_seq: Option<u64>,
    pub unknown_since: Option<Instant>,
    pub no_agent_since: Option<Instant>,
    /// The completion_seq we already asked a status report for.
    pub status_asked_for: Option<u64>,
    pub title: String,
}

/// What the observer wants done for a worker, decided by `assess` (pure, tested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    TurnEnded,
    ReportBlocked,
    ReportProgress(String),
    /// The turn ended with no report since the last delivery: record `waiting` and ask for a status.
    ReportWaitingAndAsk,
    ReportFailed(String),
}

/// Decides what a status change means for a running worker.
///
/// `report_since_delivery`: the worker reported after the last message it got.
pub fn assess(
    prev: Option<&WorkerSeen>,
    status: &str,
    completion_seq: Option<u64>,
    report_since_delivery: bool,
    latest_report: Option<ReportState>,
    now: Instant,
    seen: &mut WorkerSeen,
) -> Vec<Action> {
    let mut actions = Vec::new();
    seen.status = status.to_string();
    seen.completion_seq = completion_seq;
    match status {
        "unknown" => {
            let since = seen.unknown_since.get_or_insert(now);
            if now.duration_since(*since) >= UNKNOWN_TIMEOUT {
                actions.push(Action::ReportFailed("agent not detected for 5 minutes".into()));
            }
        }
        _ => seen.unknown_since = None,
    }
    match status {
        "blocked" => {
            if latest_report != Some(ReportState::Blocked) {
                actions.push(Action::ReportBlocked);
            }
        }
        "working" => {
            if prev.map(|p| p.status == "blocked").unwrap_or(false) {
                actions.push(Action::ReportProgress("continued after a dialog".into()));
            }
        }
        "idle" | "done" => {
            let turn_ended = matches!((prev.and_then(|p| p.completion_seq), completion_seq), (Some(a), Some(b)) if b > a)
                || (prev.map(|p| p.status == "working").unwrap_or(false) && completion_seq.is_some());
            if turn_ended {
                actions.push(Action::TurnEnded);
                if !report_since_delivery && seen.status_asked_for != completion_seq {
                    seen.status_asked_for = completion_seq;
                    actions.push(Action::ReportWaitingAndAsk);
                }
            }
        }
        _ => {}
    }
    actions
}

/// Flags that turn a harness's own permission prompts off, per mode (PLAN §6).
pub fn mode_flags(profile: &Profile, harness: &str, mode: Mode) -> Vec<String> {
    let table: &BTreeMap<String, Vec<String>> = match mode {
        Mode::Autonomous => &profile.harness_flags.autonomous,
        Mode::Checkpoint => &profile.harness_flags.checkpoint,
        Mode::Propose => &profile.harness_flags.propose,
    };
    table.get(harness).cloned().unwrap_or_default()
}

/// The harness and model for a role: the first installed entry of the role's
/// tier, else the superagent's own harness.
pub fn pick_tier(profile: &Profile, role: &str) -> (String, String) {
    let tier = match role {
        "reviewer" => &profile.models.review.use_,
        "small" => &profile.models.small.use_,
        _ => &profile.models.hard.use_,
    };
    for entry in tier {
        if crate::envpath::locate(&entry.harness).is_some() {
            return (entry.harness.clone(), entry.model.clone());
        }
    }
    (profile.superagent.harness.clone(), String::new())
}

/// The short id people see: `t-7fdb40` → `7fdb40`.
pub fn short_id(ticket_id: &str) -> String {
    ticket_id.trim_start_matches("t-").to_string()
}

/// The ticket's workspace label: the title, trimmed, plus the short id.
pub fn workspace_label(title: &str, ticket_id: &str) -> String {
    let mut text: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 32 {
        text = text.chars().take(31).collect::<String>().trim_end().to_string() + "…";
    }
    if text.is_empty() {
        text = "ticket".into();
    }
    format!("{text} · {}", short_id(ticket_id))
}

/// The tab label of an assignment: its role, numbered when the role ran before (`worker #2`).
pub fn tab_label(ticket: &Ticket, assignment_id: &str) -> String {
    let role = ticket
        .assignments
        .iter()
        .find(|a| a.id == assignment_id)
        .map(|a| a.role.as_str())
        .unwrap_or("worker");
    let nth = ticket
        .assignments
        .iter()
        .take_while(|a| a.id != assignment_id)
        .filter(|a| a.role == role)
        .count()
        + 1;
    if nth == 1 {
        role.to_string()
    } else {
        format!("{role} #{nth}")
    }
}

/// A herdr agent name for a worker: `<profile>-<short id>-<role>`, fitted to `[a-z][a-z0-9_-]{0,31}`.
pub fn worker_name(profile: &str, ticket_id: &str, tab: &str) -> String {
    let clean = |s: &str| {
        s.to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    let mut name = format!("{}-{}-{}", clean(profile), short_id(ticket_id), clean(tab).replace("--", "-"));
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name = format!("sa-{name}");
    }
    name.chars().take(32).collect::<String>().trim_end_matches('-').to_string()
}

/// `git worktree add` of `branch` at `path` (reusing the branch when it exists).
pub fn add_worktree(repo: &str, branch: &str, path: &std::path::Path) -> Result<(), String> {
    if path.join(".git").exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let exists = std::process::Command::new("git")
        .args(["-C", repo, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let mut args = vec!["-C".to_string(), repo.to_string(), "worktree".into(), "add".into()];
    if exists {
        args.extend([path.display().to_string(), branch.to_string()]);
    } else {
        args.extend(["-b".to_string(), branch.to_string(), path.display().to_string()]);
    }
    let output = std::process::Command::new("git").args(&args).output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Marks `directory` as trusted in the harness's own configuration, so a new
/// worktree does not stop the worker at a trust dialog. Claude keeps that in
/// `~/.claude.json` under `projects.<path>.hasTrustDialogAccepted`.
pub fn pre_trust(harness: &str, directory: &str) {
    pre_trust_in(harness, directory, std::env::var("HOME").ok().map(std::path::PathBuf::from));
}

fn pre_trust_in(harness: &str, directory: &str, home: Option<std::path::PathBuf>) {
    if harness != "claude" || directory.is_empty() {
        return;
    }
    let Some(home) = home else { return };
    let path = home.join(".claude.json");
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    let Some(object) = root.as_object_mut() else { return };
    let projects = object.entry("projects").or_insert_with(|| json!({}));
    let Some(projects) = projects.as_object_mut() else { return };
    let entry = projects.entry(directory.to_string()).or_insert_with(|| json!({}));
    let Some(entry) = entry.as_object_mut() else { return };
    if entry.get("hasTrustDialogAccepted") == Some(&Value::Bool(true)) {
        return;
    }
    entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    match serde_json::to_string_pretty(&root) {
        Ok(text) => match std::fs::write(&path, text) {
            Ok(()) => tracing::debug!(target: "executor", directory, "pre-trusted the worktree for claude"),
            Err(e) => tracing::warn!(%e, "could not pre-trust the worktree in ~/.claude.json"),
        },
        Err(e) => tracing::warn!(%e, "could not serialise ~/.claude.json"),
    }
}

impl Daemon {
    pub(crate) fn install_handlers(&mut self) {
        self.handlers.insert("launch", Box::new(|d, e| d.launch(e)));
        self.handlers.insert("deliver", Box::new(|d, e| d.deliver(e)));
        self.handlers.insert("stop", Box::new(|d, e| d.stop_worker(e)));
    }

    fn running_workers(&self) -> usize {
        self.store
            .ticket_open()
            .iter()
            .filter(|t| t.running_assignment().map(|a| a.pane_id.is_some()).unwrap_or(false))
            .count()
    }

    /// The memory project of a ticket: its repository's slug, else the notes bucket.
    pub(crate) fn ticket_project(&self, ticket: &Ticket) -> String {
        ticket
            .project
            .as_deref()
            .and_then(project_key)
            .unwrap_or_else(|| NOTES_PROJECT.to_string())
    }

    fn launch(&self, effect: &Effect) -> Outcome {
        let ticket_id = effect.payload["ticket_id"].as_str().unwrap_or("").to_string();
        let assignment_id = effect.payload["assignment_id"].as_str().unwrap_or("").to_string();
        let Some(mut ticket) = self.store.ticket_get(&ticket_id) else {
            return Outcome::Done;
        };
        let Some(assignment) = ticket.assignments.iter().find(|a| a.id == assignment_id).cloned() else {
            return Outcome::Done;
        };
        if !ticket.live() || assignment.state != AssignmentState::Pending {
            return Outcome::Done;
        }
        let Some(herdr) = &self.herdr else { return Outcome::Defer(60_000) };
        let profile = self.policy.snapshot();
        if self.running_workers() >= profile.limits.max_workers as usize {
            return Outcome::Defer(10_000);
        }
        if effect.attempts >= 4 {
            let _ = self.engine.report(
                &ticket_id,
                WorkerReport {
                    state: ReportState::Failed,
                    summary: format!("could not launch a worker: {}", effect.error),
                    question: Question::Keep,
                },
            );
            return Outcome::Done;
        }
        let role = assignment.role.clone();
        let (harness, model) = pick_tier(&profile, &role);
        let label = workspace_label(&ticket.title, &ticket.id);
        let tab = tab_label(&ticket, &assignment.id);

        // Where the worker lives: the ticket's workspace, one tab per assignment (PLAN §8b).
        // The first assignment creates it: a worktree of the project's repo, else a workspace.
        let project = ticket
            .project
            .as_deref()
            .map(crate::profile::expand_home)
            .filter(|p| std::path::Path::new(p).is_dir());
        let is_repo = project
            .as_deref()
            .map(|p| std::path::Path::new(p).join(".git").exists())
            .unwrap_or(false);
        let existing = ticket.execution.as_ref().and_then(|e| e.workspace_id.clone()).filter(|id| {
            herdr
                .workspace_list()
                .map(|list| list.iter().any(|w| &w.workspace_id == id))
                .unwrap_or(false)
        });
        let env = json!({ "SUPERAGENT_TICKET": ticket.id, "SUPERAGENT_ROLE": role, "SUPERAGENT_PROFILE": self.paths.profile });
        let (created, reused) = match &existing {
            Some(workspace_id) => {
                let cwd = ticket
                    .execution
                    .as_ref()
                    .map(|e| e.directory.clone())
                    .filter(|d| !d.is_empty())
                    .or(project.clone());
                (
                    herdr.call(
                        "tab.create",
                        json!({ "workspace_id": workspace_id, "label": tab, "cwd": cwd, "focus": false, "env": env }),
                    ),
                    true,
                )
            }
            None if is_repo => {
                // Our own `git worktree add`, under the profile's state dir, then a plain workspace on it:
                // herdr's worktree.create would also open a primary workspace for the repo.
                let repo = project.clone().unwrap_or_default();
                let branch = format!("sa/{}", short_id(&ticket.id));
                let path = self.paths.state_dir().join("worktrees").join(short_id(&ticket.id));
                match add_worktree(&repo, &branch, &path) {
                    Ok(()) => (
                        herdr.call(
                            "workspace.create",
                            json!({ "cwd": path.display().to_string(), "label": label, "focus": false, "env": env }),
                        ),
                        false,
                    ),
                    Err(e) => return Outcome::Fail(format!("git worktree: {e}")),
                }
            }
            None => {
                let cwd = project.clone().unwrap_or_else(|| self.paths.profile_dir().display().to_string());
                (
                    herdr.call(
                        "workspace.create",
                        json!({ "cwd": cwd, "label": label, "focus": false, "env": env }),
                    ),
                    false,
                )
            }
        };
        let created = match created {
            Ok(v) => v,
            Err(e) => return Outcome::Fail(format!("create place: {e}")),
        };
        let pane_id = created
            .pointer("/root_pane/pane_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let workspace_id = if reused {
            existing.clone().unwrap_or_default()
        } else {
            created
                .pointer("/workspace/workspace_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let directory = if reused {
            ticket.execution.as_ref().map(|e| e.directory.clone()).unwrap_or_default()
        } else if is_repo {
            self.paths
                .state_dir()
                .join("worktrees")
                .join(short_id(&ticket.id))
                .display()
                .to_string()
        } else {
            project.clone().unwrap_or_default()
        };
        if profile.harness_flags.pre_trust {
            pre_trust(&harness, &directory);
        }
        if pane_id.is_empty() {
            return Outcome::Fail(format!("no pane in the created place: {created}"));
        }
        // The first tab carries the role's name like the later ones.
        if !reused {
            if let Some(tab_id) = created.pointer("/tab/tab_id").and_then(Value::as_str) {
                let _ = herdr.call("tab.rename", json!({ "tab_id": tab_id, "label": tab }));
            }
        }

        // The brief: request, notes, mode rules, branch, memory of this project.
        let branch = if reused {
            ticket.execution.as_ref().and_then(|e| e.branch.clone())
        } else {
            is_repo.then(|| format!("sa/{}", short_id(&ticket.id)))
        };
        ticket.execution = Some(Execution {
            name: label.clone(),
            directory: directory.clone(),
            assignment: String::new(),
            workspace_id: (!workspace_id.is_empty()).then_some(workspace_id.clone()),
            branch: branch.clone(),
        });
        let project_key = self.ticket_project(&ticket);
        let hits = self.store.memory_search(&project_key, &ticket.title, 5).unwrap_or_default();
        let request = self.store.request_get(&ticket.origin.request_id);
        let text = brief::render(&self.paths.profile, &ticket, request.as_ref(), &role, &hits);
        let brief_path =
            self.paths
                .state_dir()
                .join("briefs")
                .join(format!("{}-{}.md", ticket.id, &assignment.id[..8.min(assignment.id.len())]));
        if let Some(parent) = brief_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&brief_path, text) {
            return Outcome::Fail(format!("write brief: {e}"));
        }

        // The worker's environment, through its shell, since agent.start takes none.
        let mut exports = vec![
            format!("SUPERAGENT_TICKET={}", ticket.id),
            format!("SUPERAGENT_ROLE={role}"),
            format!("SUPERAGENT_PROFILE={}", self.paths.profile),
            format!("PATH={}", crate::envpath::full_path()),
        ];
        for key in [
            "HERDR_SUPERAGENT_STATE_DIR",
            "HERDR_SUPERAGENT_CONFIG_DIR",
            "HERDR_SUPERAGENT_DEBUG",
        ] {
            if let Ok(value) = std::env::var(key) {
                if !value.is_empty() {
                    exports.push(format!("{key}={value}"));
                }
            }
        }
        let line = format!("export {}; clear", shell_words(&exports));
        if let Err(e) = herdr.pane_run(&pane_id, &line) {
            return Outcome::Fail(format!("export env: {e}"));
        }
        std::thread::sleep(Duration::from_millis(300));

        let mut args = crate::calibration::model_args(&harness, &model);
        args.extend(mode_flags(&profile, &harness, ticket.mandate.mode));
        let name = worker_name(&self.paths.profile, &ticket.id, &tab);
        if let Err(e) = herdr.call(
            "agent.start",
            json!({ "name": name, "kind": harness, "pane_id": pane_id, "args": args, "timeout_ms": 60000 }),
        ) {
            let msg = e.to_string();
            // A dialog at startup (trust, login) is the person's to answer; the deliver effect keeps retrying.
            if !msg.contains("agent_not_ready") {
                return Outcome::Fail(format!("agent.start {harness}: {msg}"));
            }
        }
        let _ = herdr.report_pane_metadata(
            &pane_id,
            Some(&format!("{tab} · working")),
            &[
                ("sa_task", label.as_str()),
                ("sa_stage", "working"),
                ("sa_ticket", ticket.id.as_str()),
                ("sa_profile", self.paths.profile.as_str()),
            ],
        );
        if !workspace_id.is_empty() && !reused {
            let _ = herdr.report_workspace_metadata(
                &workspace_id,
                &[("sa_ticket", &ticket.id), ("sa_profile", &self.paths.profile)],
                crate::herdr::seq_now(),
            );
        }

        ticket.execution = Some(Execution {
            name: label,
            directory,
            assignment: brief_path.display().to_string(),
            workspace_id: (!workspace_id.is_empty()).then_some(workspace_id),
            branch,
        });
        self.store.ticket_save(&ticket);
        let _ = self.engine.assignment_placed(&ticket.id, &assignment.id, &pane_id);
        let _ = self.engine.assignment_started(&ticket.id, &assignment.id);
        // The first delivery carries the brief pointer even when nothing else waits.
        self.store.effect_enqueue(
            "deliver",
            &ticket.id,
            &json!({ "ticket_id": ticket.id, "assignment_id": assignment.id }),
            crate::herdr::seq_now() as i64 + 2000,
            &crate::core::clock::now_iso(),
        );
        self.workers.borrow_mut().insert(
            pane_id.clone(),
            WorkerSeen {
                status: "starting".into(),
                completion_seq: None,
                unknown_since: None,
                no_agent_since: None,
                status_asked_for: None,
                title: String::new(),
            },
        );
        tracing::info!(target: "executor", ticket = %ticket.id, %harness, %model, pane = %pane_id, mode = %ticket.mandate.mode, "worker launched");
        Outcome::Done
    }

    fn deliver(&self, effect: &Effect) -> Outcome {
        let ticket_id = effect.payload["ticket_id"].as_str().unwrap_or("").to_string();
        let Some(ticket) = self.store.ticket_get(&ticket_id) else {
            return Outcome::Done;
        };
        if !ticket.live() {
            return Outcome::Done;
        }
        let Some(running) = ticket.running_assignment().cloned() else {
            return Outcome::Defer(5_000);
        };
        let Some(pane_id) = running.pane_id.clone() else {
            return Outcome::Defer(5_000);
        };
        let Some(herdr) = &self.herdr else { return Outcome::Defer(60_000) };
        let brief = running
            .delivered_at
            .is_none()
            .then(|| ticket.execution.as_ref().map(|e| std::path::PathBuf::from(&e.assignment)))
            .flatten();
        let (text, ids) = brief::delivery_text(&ticket, &|id| self.store.request_get(id), brief.as_deref());
        if text.trim().is_empty() {
            return Outcome::Done;
        }
        match herdr.call("agent.prompt", json!({ "target": pane_id, "text": text })) {
            Ok(_) => {
                let _ = self.engine.inputs_delivered(&ticket_id, &running.id, &ids);
                tracing::info!(target: "executor", ticket = %ticket_id, pane = %pane_id, inputs = ids.len(), "delivered");
                Outcome::Done
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("agent_blocked") {
                    return Outcome::Defer(30_000);
                }
                if msg.contains("agent_not_ready") || msg.contains("not an active") || msg.contains("launch") {
                    return Outcome::Defer(5_000);
                }
                let _ = self.engine.delivery_failed(&ticket_id, &running.id, &msg);
                Outcome::Fail(format!("agent.prompt: {msg}"))
            }
        }
    }

    fn stop_worker(&self, effect: &Effect) -> Outcome {
        let ticket_id = effect.payload["ticket_id"].as_str().unwrap_or("").to_string();
        let assignment_id = effect.payload["assignment_id"].as_str().unwrap_or("");
        let reason = effect.payload["reason"].as_str().unwrap_or("ended");
        let Some(ticket) = self.store.ticket_get(&ticket_id) else {
            return Outcome::Done;
        };
        let Some(pane) = ticket
            .assignments
            .iter()
            .find(|a| a.id == assignment_id)
            .and_then(|a| a.pane_id.clone())
        else {
            return Outcome::Done;
        };
        if let Some(herdr) = &self.herdr {
            // The pane stays, so the conversation can be read; its title says it is over.
            let tab = tab_label(&ticket, assignment_id);
            let _ = herdr.report_pane_metadata(&pane, Some(&format!("{tab} · {reason}")), &[("sa_stage", "ended")]);
        }
        self.workers.borrow_mut().remove(&pane);
        Outcome::Done
    }

    /// Reads herdr's agent list once per tick and turns status changes into
    /// reports where the worker said nothing (PLAN §8, turn-end fallback).
    pub(crate) fn observe_workers(&self) {
        let Some(herdr) = &self.herdr else { return };
        let running: Vec<(Ticket, Assignment)> = self
            .store
            .ticket_open()
            .into_iter()
            .filter_map(|t| t.running_assignment().filter(|a| a.pane_id.is_some()).cloned().map(|a| (t, a)))
            .collect();
        if running.is_empty() {
            return;
        }
        let agents = match herdr.call("agent.list", json!({})) {
            Ok(v) => v.get("agents").and_then(Value::as_array).cloned().unwrap_or_default(),
            Err(e) => {
                tracing::debug!(target: "executor", %e, "agent.list failed");
                return;
            }
        };
        let panes = match herdr.pane_list() {
            Ok(p) => p,
            Err(_) => return,
        };
        let now = Instant::now();
        for (ticket, assignment) in running {
            let pane_id = assignment.pane_id.clone().unwrap_or_default();
            let mut workers = self.workers.borrow_mut();
            let prev = workers.get(&pane_id).cloned();
            let mut seen = prev.clone().unwrap_or(WorkerSeen {
                status: "starting".into(),
                completion_seq: None,
                unknown_since: None,
                no_agent_since: None,
                status_asked_for: None,
                title: String::new(),
            });
            if !panes.iter().any(|p| p.pane_id == pane_id) {
                drop(workers);
                tracing::warn!(target: "executor", ticket = %ticket.id, pane = %pane_id, "worker pane is gone");
                let _ = self.engine.report(
                    &ticket.id,
                    WorkerReport {
                        state: ReportState::Failed,
                        summary: "the worker's pane was closed".into(),
                        question: Question::Keep,
                    },
                );
                self.workers.borrow_mut().remove(&pane_id);
                continue;
            }
            let agent = agents.iter().find(|a| a["pane_id"].as_str() == Some(pane_id.as_str()));
            let actions = match agent {
                None => {
                    let since = *seen.no_agent_since.get_or_insert(now);
                    if now.duration_since(since) >= NO_AGENT_TIMEOUT && assignment.delivered_at.is_some() {
                        vec![Action::ReportFailed("the worker's agent exited".into())]
                    } else {
                        Vec::new()
                    }
                }
                Some(agent) => {
                    seen.no_agent_since = None;
                    let status = agent["agent_status"].as_str().unwrap_or("unknown");
                    let completion_seq = agent["completion_seq"].as_u64();
                    let report_since_delivery = match (&assignment.report, &assignment.delivered_at) {
                        (Some(report), Some(delivered)) => report.at >= *delivered,
                        (Some(_), None) => true,
                        (None, _) => false,
                    };
                    assess(
                        prev.as_ref(),
                        status,
                        completion_seq,
                        report_since_delivery,
                        assignment.report.as_ref().map(|r| r.state),
                        now,
                        &mut seen,
                    )
                }
            };
            workers.insert(pane_id.clone(), seen);
            drop(workers);
            for action in actions {
                tracing::info!(target: "executor", ticket = %ticket.id, pane = %pane_id, ?action, "observer");
                let result = match action {
                    Action::TurnEnded => self.engine.turn_ended(&ticket.id, &assignment.id).map(|_| ()),
                    Action::ReportBlocked => self
                        .engine
                        .report(
                            &ticket.id,
                            WorkerReport {
                                state: ReportState::Blocked,
                                summary: "the harness is waiting for you (permission or question dialog)".into(),
                                question: Question::Keep,
                            },
                        )
                        .map(|_| ()),
                    Action::ReportProgress(summary) => self
                        .engine
                        .report(
                            &ticket.id,
                            WorkerReport {
                                state: ReportState::Progress,
                                summary,
                                question: Question::Keep,
                            },
                        )
                        .map(|_| ()),
                    Action::ReportWaitingAndAsk => {
                        let tail = herdr
                            .call("agent.read", json!({ "target": pane_id, "source": "recent", "lines": 12 }))
                            .ok()
                            .and_then(|v| v.get("text").and_then(Value::as_str).map(str::to_string))
                            .unwrap_or_default();
                        let summary = format!("turn ended without a report. Last lines:\n{}", tail.trim());
                        self.engine
                            .report(
                                &ticket.id,
                                WorkerReport {
                                    state: ReportState::Waiting,
                                    summary,
                                    question: Question::Keep,
                                },
                            )
                            .and_then(|_| self.engine.request_status(&ticket.id))
                            .map(|_| ())
                    }
                    Action::ReportFailed(summary) => self
                        .engine
                        .report(
                            &ticket.id,
                            WorkerReport {
                                state: ReportState::Failed,
                                summary,
                                question: Question::Keep,
                            },
                        )
                        .map(|_| ()),
                };
                if let Err(e) = result {
                    tracing::debug!(target: "executor", %e, "observer action refused");
                }
            }
            // The pane title follows the stage.
            if let Some(fresh) = self.store.ticket_get(&ticket.id) {
                let stage = ticket_stage(&fresh).label();
                let title = format!(
                    "{} · {}",
                    tab_label(&fresh, &assignment.id),
                    stage.split('/').next_back().unwrap_or(&stage)
                );
                let mut workers = self.workers.borrow_mut();
                if let Some(seen) = workers.get_mut(&pane_id) {
                    if seen.title != title {
                        seen.title = title.clone();
                        let _ = herdr.report_pane_metadata(&pane_id, Some(&title), &[("sa_stage", &stage)]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(status: &str, seq: Option<u64>) -> WorkerSeen {
        WorkerSeen {
            status: status.into(),
            completion_seq: seq,
            unknown_since: None,
            no_agent_since: None,
            status_asked_for: None,
            title: String::new(),
        }
    }

    #[test]
    fn a_turn_that_ends_without_a_report_records_waiting_and_asks_once() {
        let now = Instant::now();
        let prev = seen("working", Some(3));
        let mut cur = prev.clone();
        let actions = assess(Some(&prev), "idle", Some(4), false, None, now, &mut cur);
        assert_eq!(actions, vec![Action::TurnEnded, Action::ReportWaitingAndAsk]);
        // The same seq seen again asks nothing more.
        let again = assess(
            Some(&cur),
            "idle",
            Some(4),
            false,
            Some(ReportState::Waiting),
            now,
            &mut cur.clone(),
        );
        assert!(again.is_empty());
        // A turn that ended with a report only records the turn end.
        let mut cur2 = seen("working", Some(4));
        assert_eq!(
            assess(
                Some(&seen("working", Some(4))),
                "idle",
                Some(5),
                true,
                Some(ReportState::Progress),
                now,
                &mut cur2
            ),
            vec![Action::TurnEnded]
        );
    }

    #[test]
    fn blocked_and_unknown_become_reports() {
        let now = Instant::now();
        let mut cur = seen("working", None);
        assert_eq!(
            assess(Some(&seen("working", None)), "blocked", None, false, None, now, &mut cur),
            vec![Action::ReportBlocked]
        );
        assert!(assess(
            Some(&cur),
            "blocked",
            None,
            false,
            Some(ReportState::Blocked),
            now,
            &mut cur.clone()
        )
        .is_empty());
        let mut after = cur.clone();
        assert_eq!(
            assess(Some(&cur), "working", None, false, Some(ReportState::Blocked), now, &mut after),
            vec![Action::ReportProgress("continued after a dialog".into())]
        );
        let mut unknown = seen("working", None);
        assert!(assess(None, "unknown", None, false, None, now, &mut unknown).is_empty());
        assert!(unknown.unknown_since.is_some());
        let later = now + UNKNOWN_TIMEOUT + Duration::from_secs(1);
        assert_eq!(
            assess(Some(&unknown), "unknown", None, false, None, later, &mut unknown.clone()),
            vec![Action::ReportFailed("agent not detected for 5 minutes".into())]
        );
    }

    #[test]
    fn adds_a_worktree_once_and_pre_trusts_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        let wt = dir.path().join("worktrees").join("abc123");
        add_worktree(&repo.display().to_string(), "sa/abc123", &wt).unwrap();
        assert!(wt.join(".git").exists());
        add_worktree(&repo.display().to_string(), "sa/abc123", &wt).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{"projects":{"/other":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        pre_trust_in("claude", &wt.display().to_string(), Some(home.clone()));
        let json: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(json["projects"][wt.display().to_string()]["hasTrustDialogAccepted"], true);
        assert_eq!(json["projects"]["/other"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn tiers_flags_and_names() {
        let mut profile = Profile::new("work");
        profile.models.hard.use_ = vec![
            crate::profile::HarnessModel {
                harness: "definitely-missing-harness".into(),
                model: "x".into(),
            },
            crate::profile::HarnessModel {
                harness: "sh".into(),
                model: "fast".into(),
            },
        ];
        assert_eq!(pick_tier(&profile, "worker"), ("sh".into(), "fast".into()));
        assert_eq!(pick_tier(&profile, "reviewer").1, "sonnet", "the default review tier");
        assert_eq!(
            mode_flags(&profile, "claude", Mode::Autonomous),
            vec!["--dangerously-skip-permissions"]
        );
        assert!(mode_flags(&profile, "claude", Mode::Checkpoint).is_empty());
        assert_eq!(worker_name("work", "t-abc123", "worker"), "work-abc123-worker");
        assert_eq!(worker_name("9side", "t-abc123", "worker #2"), "sa-9side-abc123-worker-2");
        assert_eq!(
            workspace_label("Fix the flaky login test", "t-abc123"),
            "Fix the flaky login test · abc123"
        );
        assert!(workspace_label(&"x".repeat(60), "t-1").starts_with(&"x".repeat(31)));
        let mut ticket = crate::brief::tests::sample(Mode::Checkpoint);
        ticket.assignments = vec![
            Assignment {
                id: "a1".into(),
                role: "worker".into(),
                state: AssignmentState::Ended,
                created_at: "t".into(),
                started_at: None,
                ended_at: None,
                ended_reason: None,
                report: None,
                delivered_at: None,
                idle_at: None,
                pane_id: None,
                parent: None,
            },
            Assignment {
                id: "a2".into(),
                role: "reviewer".into(),
                state: AssignmentState::Running,
                created_at: "t".into(),
                started_at: None,
                ended_at: None,
                ended_reason: None,
                report: None,
                delivered_at: None,
                idle_at: None,
                pane_id: None,
                parent: None,
            },
            Assignment {
                id: "a3".into(),
                role: "worker".into(),
                state: AssignmentState::Pending,
                created_at: "t".into(),
                started_at: None,
                ended_at: None,
                ended_reason: None,
                report: None,
                delivered_at: None,
                idle_at: None,
                pane_id: None,
                parent: None,
            },
        ];
        assert_eq!(tab_label(&ticket, "a1"), "worker");
        assert_eq!(tab_label(&ticket, "a2"), "reviewer");
        assert_eq!(tab_label(&ticket, "a3"), "worker #2");
    }
}
