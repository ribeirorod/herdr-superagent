//! Domain model: the concepts and their invariants, independent of any
//! source, profile, harness or herdr.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::core::reasons::Reason;

/// How much a ticket may do on its own (docs/PLAN.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Plans only; nothing runs until a person approves the plan.
    Propose,
    /// Runs, but stops at declared milestones and before risky actions.
    Checkpoint,
    /// End to end; reports when done.
    Autonomous,
}

impl Mode {
    /// The stricter of two modes; a child never runs more autonomously than its parent.
    pub fn stricter(self, other: Mode) -> Mode {
        if self <= other {
            self
        } else {
            other
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Propose => "propose",
            Mode::Checkpoint => "checkpoint",
            Mode::Autonomous => "autonomous",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(value: &str) -> Result<Mode, String> {
        match value {
            "propose" => Ok(Mode::Propose),
            "checkpoint" => Ok(Mode::Checkpoint),
            "autonomous" => Ok(Mode::Autonomous),
            other => Err(format!("unknown mode '{other}' (propose, checkpoint, autonomous)")),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    Explicit,
    Inferred,
}

/// One immutable request for work. `text` and `meta` are untrusted data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    /// Where it came from: `you` (typed or `submit`), later `jira`, `github`, `mail`…
    pub source: String,
    /// Who asked, in the source's vocabulary.
    pub actor: String,
    /// The source says a person wrote it, not a bot, CI or this program.
    pub human: bool,
    pub at: String,
    pub text: String,
    /// A workflow named up front skips routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// A mode asked for up front; the profile's rules may cap it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// The project (repo or directory) the request is about, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Source-native details; the core never interprets them.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub meta: serde_json::Value,
}

/// Where a request went after intake. It grants nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RequestDisposition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Reason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// Tickets it reached as an input (attached by the superagent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attached: Vec<String>,
    /// A person or the superagent sent it to evaluation again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requeued: bool,
    pub at: String,
}

/// How the gate admitted the request a Mandate came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum MandateBasis {
    /// A person asked for it directly.
    Explicit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
    },
    /// The superagent's assessment matched these routing rules.
    Assessed {
        #[serde(default)]
        rules: Vec<String>,
    },
}

/// Immutable authorization produced by an accept; execution is bounded by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mandate {
    pub strength: Strength,
    pub policy_version: String,
    pub basis: MandateBasis,
    pub actor: String,
    pub scope: String,
    pub mode: Mode,
}

/// The evaluation of one request; at most one exists per request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    pub id: String,
    pub request_id: String,
    pub strength: Strength,
    pub basis: MandateBasis,
    pub policy_version: String,
    pub started_at: String,
    /// When the candidate reached the superagent session; the deadline counts from then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered: Option<String>,
    pub deadline: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
    /// How many times it expired in a row.
    #[serde(default)]
    pub expired: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionKind {
    Ignore,
    Accept,
    Attach,
    Stop,
    Unavailable,
}

/// Outcome of one evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub eval_id: String,
    pub request_id: String,
    pub policy_version: String,
    pub at: String,
    pub kind: DecisionKind,
    #[serde(default)]
    pub reason: Vec<Reason>,
    pub strength: Strength,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mandate: Option<Mandate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssignmentState {
    Pending,
    Running,
    Ended,
}

/// What an assignment reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportState {
    Progress,
    Waiting,
    Ready,
    Blocked,
    Failed,
    Dropped,
}

impl ReportState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportState::Progress => "progress",
            ReportState::Waiting => "waiting",
            ReportState::Ready => "ready",
            ReportState::Blocked => "blocked",
            ReportState::Failed => "failed",
            ReportState::Dropped => "dropped",
        }
    }
}

impl std::str::FromStr for ReportState {
    type Err = String;
    fn from_str(value: &str) -> Result<ReportState, String> {
        match value {
            "progress" => Ok(ReportState::Progress),
            "waiting" => Ok(ReportState::Waiting),
            "ready" => Ok(ReportState::Ready),
            "blocked" => Ok(ReportState::Blocked),
            "failed" => Ok(ReportState::Failed),
            "dropped" => Ok(ReportState::Dropped),
            other => Err(format!("unknown report state '{other}'")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub state: ReportState,
    pub summary: String,
    pub at: String,
}

/// One run of one role in one pane, under a ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    /// `worker`, `reviewer`, … from the workflow pack; `worker` by default.
    #[serde(default = "default_role")]
    pub role: String,
    pub state: AssignmentState,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    /// When inputs last reached the worker; one at or after `report.at` means
    /// the worker has been busy with inputs since that report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    /// When a turn of its worker last ended with nothing left to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_at: Option<String>,
    /// The herdr pane that runs it, once the executor placed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The assignment that spawned this one, for children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

fn default_role() -> String {
    "worker".to_string()
}

/// How a profile change bears on a ticket.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PolicyEffect {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleChange>,
    /// Autonomous or inferred work is no longer accepted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proactive_withdrawn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleChange {
    pub id: String,
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

/// What reaches a ticket's worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TicketInput {
    /// A request attached to the ticket; `human` when a person wrote it.
    Request {
        id: String,
        request_id: String,
        at: String,
        #[serde(default)]
        human: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered_at: Option<String>,
    },
    /// The profile changed in a way that bears on the ticket.
    PolicyChanged {
        id: String,
        from: String,
        to: String,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<PolicyEffect>,
    },
    /// A person's message resumed the ended ticket under its unchanged mandate.
    Resumed { id: String, request_id: String, at: String },
    /// Text from the engine: a recovery note, a status request, an approval.
    Text { id: String, text: String, at: String },
    /// A fact the superagent gave the ticket: context, which asks for nothing.
    Note { id: String, text: String, at: String },
}

impl TicketInput {
    pub fn id(&self) -> &str {
        match self {
            TicketInput::Request { id, .. }
            | TicketInput::PolicyChanged { id, .. }
            | TicketInput::Resumed { id, .. }
            | TicketInput::Text { id, .. }
            | TicketInput::Note { id, .. } => id,
        }
    }
    pub fn at(&self) -> &str {
        match self {
            TicketInput::Request { at, .. }
            | TicketInput::PolicyChanged { at, .. }
            | TicketInput::Resumed { at, .. }
            | TicketInput::Text { at, .. }
            | TicketInput::Note { at, .. } => at,
        }
    }
    /// A message no person wrote: a note, or a request from a bot or a source.
    pub fn automatic(&self) -> bool {
        match self {
            TicketInput::Note { .. } => true,
            TicketInput::Request { human, .. } => !human,
            _ => false,
        }
    }
    pub fn is_human_request(&self) -> bool {
        matches!(self, TicketInput::Request { human: true, .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketEndBy {
    Executor,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketOutcome {
    Completed,
    Dropped,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    pub at: String,
    pub by: TicketEndBy,
    pub outcome: TicketOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A question for a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// A declared milestone the worker stopped at, waiting for review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    /// The ticket's workspace label, as people see it in herdr.
    pub name: String,
    /// The worktree (or project directory) every role of the ticket works in.
    pub directory: String,
    /// The git branch the ticket's worktree is on, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The path of the latest brief.
    pub assignment: String,
    /// The herdr workspace that holds the ticket: one per ticket, one tab per assignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryFailure {
    pub error: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub request_id: String,
}

/// An authorized work item with its assignments and inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: String,
    pub title: String,
    pub decision_id: String,
    pub mandate: Mandate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<Attention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Checkpoint>,
    /// Names of the checkpoints a person approved, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_checkpoints: Vec<String>,
    pub assignments: Vec<Assignment>,
    pub inputs: Vec<TicketInput>,
    /// Inputs not yet handed to a running assignment.
    pub undelivered: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
    /// The latest failed delivery of an input to its worker, until one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryFailure>,
}

impl Ticket {
    pub fn live(&self) -> bool {
        self.ended.is_none()
    }
    pub fn outcome(&self) -> Option<TicketOutcome> {
        self.ended.as_ref().map(|ended| ended.outcome)
    }
    pub fn running_assignment(&self) -> Option<&Assignment> {
        self.assignments.iter().find(|a| a.state == AssignmentState::Running)
    }
    pub fn running_assignment_mut(&mut self) -> Option<&mut Assignment> {
        self.assignments.iter_mut().find(|a| a.state == AssignmentState::Running)
    }
    pub fn latest_assignment(&self) -> Option<&Assignment> {
        self.assignments.last()
    }
    /// The latest report any assignment made: what the ticket last said.
    pub fn latest_report(&self) -> Option<&Report> {
        self.assignments.iter().rev().find_map(|a| a.report.as_ref())
    }
    pub fn input(&self, id: &str) -> Option<&TicketInput> {
        self.inputs.iter().find(|input| input.id() == id)
    }
    /// Whether the ticket has a pending or running assignment, or undelivered inputs.
    pub fn active(&self) -> bool {
        !self.undelivered.is_empty() || self.latest_assignment().map(|a| a.state != AssignmentState::Ended).unwrap_or(false)
    }
    /// Whether the ticket waits for a person: its latest assignment reported
    /// blocked or failed, and no person's message reached it since.
    pub fn waits_for_person(&self) -> bool {
        let Some(report) = self.latest_assignment().and_then(|a| a.report.as_ref()) else {
            return false;
        };
        if report.state != ReportState::Blocked && report.state != ReportState::Failed {
            return false;
        }
        !self
            .inputs
            .iter()
            .any(|input| input.is_human_request() && input.at() > report.at.as_str())
    }
}

/// The candidate the superagent session assesses: bounded facts, never authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub eval_id: String,
    pub request: Request,
    pub strength: Strength,
    pub policy_version: String,
    /// Every ticket on the same project, including ended ones, for duplicate judgement.
    pub tickets: Vec<Ticket>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_order_by_autonomy() {
        assert_eq!(Mode::Autonomous.stricter(Mode::Checkpoint), Mode::Checkpoint);
        assert_eq!(Mode::Propose.stricter(Mode::Autonomous), Mode::Propose);
        assert_eq!("checkpoint".parse::<Mode>(), Ok(Mode::Checkpoint));
        assert!("loose".parse::<Mode>().is_err());
    }

    #[test]
    fn inputs_know_what_is_automatic() {
        let note = TicketInput::Note {
            id: "n".into(),
            text: "x".into(),
            at: "t".into(),
        };
        let bot = TicketInput::Request {
            id: "r".into(),
            request_id: "1".into(),
            at: "t".into(),
            human: false,
            note: None,
            delivered_to: None,
            delivered_at: None,
        };
        let person = TicketInput::Request {
            id: "r".into(),
            request_id: "1".into(),
            at: "t".into(),
            human: true,
            note: None,
            delivered_to: None,
            delivered_at: None,
        };
        assert!(note.automatic() && bot.automatic() && !person.automatic());
        assert!(person.is_human_request());
    }
}
