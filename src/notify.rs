//! What the daemon tells the superagent session, unasked: requests that wait
//! for routing, tickets that need the person, and the switch from calibration
//! to routing. Only what needs a decision comes back; progress stays in the
//! worker panes and on the dashboard (PLAN §2).

use serde_json::{json, Value};

use crate::core::clock;
use crate::core::model::*;
use crate::core::ports::Repositories;
use crate::core::stage::{ticket_stage, PersonAction, TicketStage};
use crate::daemon::Daemon;
use crate::executor::Outcome;
use crate::store::sqlite::Effect;

/// A request the person typed is submitted by the session itself, which routes it in the
/// same turn; it is only announced when that has not happened after this long. Requests
/// from elsewhere (a source, a shell) are announced at once.
const CANDIDATE_GRACE_MS: i64 = 90_000;

fn candidate_grace_ms() -> i64 {
    std::env::var("HERDR_SUPERAGENT_ROUTING_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CANDIDATE_GRACE_MS)
}
/// An ended ticket is announced only when it ended this recently (not history on a first run).
const ENDED_WINDOW_MS: i64 = 10 * 60_000;
const PREFIX: &str = "[superagent daemon]";

/// What identifies the state a ticket was last announced in.
pub fn marker(ticket: &Ticket) -> Option<String> {
    match ticket_stage(ticket) {
        TicketStage::Ended => ticket.ended.as_ref().map(|e| format!("ended:{}", e.at)),
        TicketStage::Person { action } => {
            let detail = match action {
                PersonAction::Review => ticket
                    .checkpoint
                    .as_ref()
                    .map(|c| c.id.clone())
                    .or_else(|| ticket.latest_report().map(|r| r.at.clone())),
                PersonAction::Answer => ticket.attention.as_ref().map(|a| a.id.clone()),
                PersonAction::Delivery => ticket.delivery.as_ref().map(|d| d.at.clone()),
                PersonAction::Blocked | PersonAction::Failed => ticket.latest_report().map(|r| r.at.clone()),
            };
            Some(format!("{}:{}", ticket_stage(ticket).label(), detail.unwrap_or_default()))
        }
        _ => None,
    }
}

/// The message for a ticket that needs the person or just ended.
pub fn ticket_message(ticket: &Ticket, bin: &str) -> String {
    let id = &ticket.id;
    let head = format!("{PREFIX} Ticket {id} ({})", ticket.title);
    let summary = |text: &str| {
        let text = text.trim();
        if text.is_empty() {
            String::new()
        } else {
            format!("\n{text}")
        }
    };
    match ticket_stage(ticket) {
        TicketStage::Ended => {
            let ended = ticket.ended.as_ref();
            let outcome = ended.map(|e| format!("{:?}", e.outcome).to_lowercase()).unwrap_or_default();
            let branch = ticket
                .execution
                .as_ref()
                .and_then(|e| e.branch.as_deref())
                .map(|b| format!(" Branch: {b}."))
                .unwrap_or_default();
            let last = ticket.latest_report().map(|r| r.summary.clone()).unwrap_or_default();
            format!("{head} ended: {outcome}.{branch}{}\nTell the person the result in two lines.", summary(&last))
        }
        TicketStage::Person { action } => match action {
            PersonAction::Review => match &ticket.checkpoint {
                Some(checkpoint) => format!(
                    "{head} waits at checkpoint `{}`.{}\nSummarise it for the person. Only they decide: `{bin} approve {id}` or `{bin} request-changes {id} \"…\"`. Never approve for them.",
                    checkpoint.name,
                    summary(&checkpoint.summary)
                ),
                None => format!(
                    "{head} is ready for review.{}",
                    summary(&ticket.latest_report().map(|r| r.summary.clone()).unwrap_or_default())
                ),
            },
            PersonAction::Answer => format!(
                "{head} asks the person:{}\nPut the question to them; pass the answer on with `{bin} note {id} \"…\"` or let them type in the worker pane.",
                summary(&ticket.attention.as_ref().map(|a| a.question.clone()).unwrap_or_default())
            ),
            PersonAction::Blocked | PersonAction::Failed => format!(
                "{head} is {}.{}\nTell the person what it needs. `{bin} retry {id}` restarts it, `{bin} stop-ticket {id}` ends it.",
                if action == PersonAction::Blocked { "blocked" } else { "failed" },
                summary(&ticket.latest_report().map(|r| r.summary.clone()).unwrap_or_default())
            ),
            PersonAction::Delivery => format!(
                "{head}: a message could not be delivered to its worker.{}\n`{bin} retry {id}` tries again.",
                summary(&ticket.delivery.as_ref().map(|d| d.error.clone()).unwrap_or_default())
            ),
        },
        _ => String::new(),
    }
}

impl Daemon {
    /// Queues a message for the superagent session; the same key replaces an undelivered one.
    pub(crate) fn tell_session(&self, key: &str, text: &str) {
        self.store.effect_enqueue(
            "session",
            key,
            &json!({ "text": text }),
            crate::herdr::seq_now() as i64,
            &clock::now_iso(),
        );
    }

    pub(crate) fn session_effect(&self, effect: &Effect) -> Outcome {
        let text = effect.payload.get("text").and_then(Value::as_str).unwrap_or("");
        let Some(pane) = self.store.kv_get("session.pane_id").filter(|p| !p.is_empty()) else {
            return Outcome::Done;
        };
        let Some(herdr) = &self.herdr else { return Outcome::Defer(60_000) };
        if text.is_empty() {
            return Outcome::Done;
        }
        match herdr.call("agent.prompt", json!({ "target": pane, "text": text })) {
            Ok(_) => Outcome::Done,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("agent_blocked") {
                    Outcome::Defer(30_000)
                } else if msg.contains("agent_not_ready") || msg.contains("not an active") || msg.contains("launch") {
                    Outcome::Defer(5_000)
                } else if effect.attempts >= 5 {
                    // No session to tell (closed pane): the dashboard and the toast still show it.
                    tracing::warn!(key = %effect.key, %msg, "gave up telling the session");
                    Outcome::Done
                } else {
                    Outcome::Fail(format!("agent.prompt session: {msg}"))
                }
            }
        }
    }

    /// Once per tick: tells the session what newly needs a decision.
    pub(crate) fn announce(&self) {
        if !self.policy.snapshot().calibrated || self.store.kv_get("session.pane_id").is_none() {
            return;
        }
        let bin = crate::envpath::self_path();
        let now = clock::now_iso();
        // Requests nobody routed yet.
        let waiting: Vec<String> = self
            .store
            .eval_list()
            .into_iter()
            .filter(|e| {
                let own = self.store.request_get(&e.request_id).map(|r| r.source == "you").unwrap_or(true);
                e.paused_at.is_none() && (!own || clock::millis_between(&e.started_at, &now) >= candidate_grace_ms())
            })
            .filter(|e| self.store.kv_get(&format!("told.eval.{}.{}", e.id, e.expired)).is_none())
            .map(|e| {
                self.store.kv_set(&format!("told.eval.{}.{}", e.id, e.expired), &now);
                // The session has it now: its deadline starts, and the scorer takes over if it lapses twice.
                let _ = self.engine.candidate_delivered(&e.id);
                e.request_id
            })
            .collect();
        if !waiting.is_empty() {
            self.tell_session(
                "candidates",
                &format!(
                    "{PREFIX} {} request(s) wait for your routing. Run `{bin} candidates` and route each one.",
                    waiting.len()
                ),
            );
        }
        for ticket in self.store.ticket_list() {
            let Some(mark) = marker(&ticket) else { continue };
            let key = format!("told.ticket.{}", ticket.id);
            if self.store.kv_get(&key).as_deref() == Some(mark.as_str()) {
                continue;
            }
            self.store.kv_set(&key, &mark);
            if let Some(ended) = &ticket.ended {
                if clock::millis_between(&ended.at, &now) > ENDED_WINDOW_MS {
                    continue;
                }
            }
            self.tell_session(&format!("ticket:{}", ticket.id), &ticket_message(&ticket, &bin));
        }
    }

    /// Calibration just finished: the session still follows the calibration
    /// prompt, so hand it the routing brief (it would otherwise do the work itself).
    pub(crate) fn switch_to_routing(&self) {
        let profile = self.policy.snapshot();
        let brief_path = self.paths.state_dir().join("brief.md");
        if let Err(e) = std::fs::write(&brief_path, crate::calibration::brief(&profile, &self.paths.profile_dir())) {
            tracing::warn!(%e, "could not write the routing brief");
            return;
        }
        self.tell_session(
            "routing-brief",
            &format!(
                "{PREFIX} Calibration is recorded. The calibration instructions no longer apply. Read {} now and follow it from here on: you route requests into tickets with `submit` and `route`, and you never do the work yourself.",
                brief_path.display()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> Ticket {
        let mut ticket = crate::brief::tests::sample(Mode::Checkpoint);
        ticket.title = "Fix login".into();
        ticket.inputs.clear();
        ticket.undelivered.clear();
        ticket.assignments.push(Assignment {
            id: "a1".into(),
            role: "worker".into(),
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
            task: None,
            mode: None,
        });
        ticket
    }

    #[test]
    fn a_checkpoint_is_announced_once_and_again_when_it_changes() {
        let mut t = ticket();
        assert_eq!(marker(&t), None, "a working ticket is not announced");
        t.checkpoint = Some(Checkpoint {
            id: "c1".into(),
            name: "plan".into(),
            summary: "three steps".into(),
            at: "2026-11-11T10:01:00.000Z".into(),
        });
        let first = marker(&t).unwrap();
        let text = ticket_message(&t, "herdr-superagent");
        assert!(text.contains("checkpoint `plan`") && text.contains("three steps") && text.contains("request-changes t-abc123"));
        t.checkpoint.as_mut().unwrap().id = "c2".into();
        assert_ne!(marker(&t).unwrap(), first, "a revised checkpoint is announced again");
    }
}
