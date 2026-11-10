//! The worker brief: what a worker pane reads first (PLAN §8).

use crate::core::model::{Assignment, Mode, Request, Ticket, TicketInput};
use crate::memory::MemoryHit;

pub const WORKER_TEMPLATE: &str = include_str!("../skills/superagent/worker.md");

pub fn mode_rules(mode: Mode) -> &'static str {
    match mode {
        Mode::Propose => "**Propose mode: plan only.** Investigate, write the plan, then `herdr-superagent checkpoint plan --summary \"…\"` and end your turn. Do not change files, run migrations, push or send anything until the plan is approved.",
        Mode::Checkpoint => "**Checkpoint mode.** Your **first checkpoint is `plan`**, before you change any file: investigate, write the plan, run `herdr-superagent checkpoint plan --summary \"…\"` and end your turn. After approval, work and stop again at milestones (`checkpoint implemented`, `checkpoint tested`) and before any risky action (pushing, deploying, deleting, sending messages, spending). A `ready` report is refused until a checkpoint was approved. The harness's own permission prompts stay on.",
        Mode::Autonomous => "**Autonomous mode.** Run end to end and report when done. Still: never push to a default branch, never delete outside your worktree, never send messages or spend money unless the request says so explicitly.",
    }
}

/// What a worker does with its branch (PLAN §8a: push work in progress at each checkpoint).
pub fn branch_rules(ticket: &Ticket) -> String {
    let start = ticket
        .execution
        .as_ref()
        .and_then(|e| e.base.as_deref())
        .map(|base| {
            let reason = ticket.execution.as_ref().and_then(|e| e.base_reason.as_deref()).unwrap_or("");
            format!(
                "**Where you start:** your branch was cut from `{base}` because {reason}\n\nVerify this before planning: `git status -sb`, `git log --oneline -5`, and the branches above. If the request implies another branch (a feature branch someone is on, a release branch), move your branch there first: `git fetch origin && git reset --hard <ref>`. Never work on top of stale code because it happened to be checked out.\n\n"
            )
        })
        .unwrap_or_default();
    match ticket.execution.as_ref().and_then(|e| e.branch.as_deref()) {
        Some(branch) => format!(
            "{start}You work in a worktree on branch `{branch}`. Commit as you go. At every checkpoint and before your final report, push the branch (`git push -u origin {branch}`) when the repository has a remote; never push to the default branch, and never open a pull request unless the request asks for one. Put the branch and the last commit in your final report."
        ),
        None => "This ticket has no repository branch. Put the files you produced in your final report.".to_string(),
    }
}

pub fn render(profile: &str, ticket: &Ticket, request: Option<&Request>, assignment: &Assignment, hits: &[MemoryHit]) -> String {
    let role = assignment.role.as_str();
    let mode = assignment.mode.unwrap_or(ticket.mandate.mode);
    let task = match &assignment.task {
        Some(task) => format!(
            "## Your task from the parent\n\nYou are a child of assignment `{}`. Do only this, then `report --state ready` (your report ends your assignment, not the ticket, and reaches the parent):\n\n{task}\n",
            assignment.parent.as_deref().map(|p| &p[..8.min(p.len())]).unwrap_or("?")
        ),
        None => String::new(),
    };
    let request_text = request.map(|r| r.text.clone()).unwrap_or_else(|| ticket.title.clone());
    let notes: Vec<String> = ticket
        .inputs
        .iter()
        .filter_map(|input| match input {
            TicketInput::Note { text, .. } => Some(format!("- note from the superagent: {text}")),
            TicketInput::Request { note: Some(note), .. } => Some(format!("- routing note: {note}")),
            _ => None,
        })
        .collect();
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!("## Notes\n\n{}\n", notes.join("\n"))
    };
    let memory = if hits.is_empty() {
        "## Memory\n\nNo earlier tickets of this project are recorded yet.".to_string()
    } else {
        let lines: Vec<String> = hits
            .iter()
            .map(|h| {
                format!(
                    "- **{}** ({}): {}",
                    h.title,
                    h.ticket_id.as_deref().unwrap_or("note"),
                    h.snippet.replace('\n', " ")
                )
            })
            .collect();
        format!(
            "## Memory: what this project's earlier tickets learned\n\n{}\n\nRun `herdr-superagent memory search \"…\"` for more.",
            lines.join("\n")
        )
    };
    WORKER_TEMPLATE
        .replace("{ticket}", &ticket.id)
        .replace("{role}", role)
        .replace("{profile}", profile)
        .replace("{title}", &ticket.title)
        .replace("{mode}", mode.as_str())
        .replace("{task}", &task)
        .replace("{assignment}", &assignment.id)
        .replace("{workflow}", ticket.workflow.as_deref().unwrap_or("general"))
        .replace("{project}", ticket.project.as_deref().unwrap_or("none (no repository)"))
        .replace("{mode_rules}", mode_rules(mode))
        .replace("{request}", &request_text)
        .replace("{branch}", &branch_rules(ticket))
        .replace("{notes}", &notes)
        .replace("{memory}", &memory)
        .replace("{bin}", &crate::envpath::self_path())
}

/// The text delivered to a running worker for the inputs that wait for it.
pub fn delivery_text(
    ticket: &Ticket,
    requests: &dyn Fn(&str) -> Option<Request>,
    first: Option<&std::path::Path>,
) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    if let Some(brief) = first {
        parts.push(format!("Read {} and start on ticket {}.", brief.display(), ticket.id));
    }
    let mut ids = Vec::new();
    for id in &ticket.undelivered {
        let Some(input) = ticket.input(id) else { continue };
        ids.push(id.clone());
        match input {
            TicketInput::Request { request_id, note, .. } => {
                let text = requests(request_id)
                    .map(|r| r.text)
                    .unwrap_or_else(|| format!("request {request_id}"));
                match note {
                    Some(note) => parts.push(format!("New message for this ticket (superagent's note: {note}):\n{text}")),
                    None => parts.push(format!("New message for this ticket:\n{text}")),
                }
            }
            TicketInput::Text { text, .. } => parts.push(text.clone()),
            TicketInput::Note { text, .. } => parts.push(format!("Context from the superagent (no action required): {text}")),
            TicketInput::Resumed { .. } => parts
                .push("This ticket was resumed by a person's message. Re-read the state of your worktree before continuing.".to_string()),
            TicketInput::PolicyChanged { from, to, effect, .. } => {
                let withdrawn = effect.as_ref().map(|e| e.proactive_withdrawn).unwrap_or(false);
                parts.push(format!(
                    "The profile changed ({from} → {to}).{}",
                    if withdrawn {
                        " Autonomous work is no longer allowed: checkpoint before any further action."
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    (parts.join("\n\n"), ids)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::model::*;

    pub(crate) fn sample(mode: Mode) -> Ticket {
        ticket(mode)
    }

    fn ticket(mode: Mode) -> Ticket {
        Ticket {
            id: "t-abc123".into(),
            title: "Fix the flaky login test".into(),
            decision_id: "d".into(),
            mandate: Mandate {
                strength: Strength::Explicit,
                policy_version: "v".into(),
                basis: MandateBasis::Explicit { workflow: None },
                actor: "you".into(),
                scope: "s".into(),
                mode,
            },
            project: Some("acme/api".into()),
            workflow: Some("software-dev".into()),
            ended: None,
            attention: None,
            checkpoint: None,
            approved_checkpoints: Vec::new(),
            assignments: vec![],
            inputs: vec![TicketInput::Note {
                id: "n1".into(),
                text: "tests live in tests/login".into(),
                at: "t".into(),
            }],
            undelivered: vec!["n1".into()],
            created_at: "t".into(),
            updated_at: "t".into(),
            origin: Origin { request_id: "r1".into() },
            execution: None,
            delivery: None,
        }
    }

    #[test]
    fn renders_mode_rules_notes_and_memory() {
        let hits = vec![MemoryHit {
            id: "m".into(),
            project: "acme/api".into(),
            ticket_id: Some("t-old".into()),
            title: "t-old worker: login cache".into(),
            snippet: "race in [login] cache".into(),
            created_at: "t".into(),
        }];
        let worker = Assignment {
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
        };
        let text = render("work", &ticket(Mode::Propose), None, &worker, &hits);
        assert!(text.contains("# Worker brief · ticket t-abc123 · worker"));
        assert!(text.contains("Propose mode: plan only"));
        assert!(text.contains("note from the superagent: tests live in tests/login"));
        assert!(text.contains("t-old worker: login cache"));
        assert!(text.contains("SUPERAGENT_TICKET"));
        let child = Assignment {
            id: "a2".into(),
            role: "reviewer".into(),
            parent: Some("a1".into()),
            task: Some("Review the diff".into()),
            mode: Some(Mode::Autonomous),
            ..worker.clone()
        };
        let text = render("work", &ticket(Mode::Checkpoint), None, &child, &[]);
        assert!(text.contains("Your task from the parent") && text.contains("Review the diff") && text.contains("Autonomous mode"));
        let (delivery, ids) = delivery_text(&ticket(Mode::Checkpoint), &|_| None, Some(std::path::Path::new("/b/brief.md")));
        assert!(delivery.starts_with("Read /b/brief.md and start on ticket t-abc123."));
        assert!(delivery.contains("Context from the superagent"));
        assert_eq!(ids, vec!["n1".to_string()]);
    }
}
