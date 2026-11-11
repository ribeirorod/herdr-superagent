//! Ports the core depends on. Profiles, the superagent session, executors and
//! stores are project-specific; the engine only knows these traits.

use serde::{Deserialize, Serialize};

use crate::core::errors::UserError;
use crate::core::model::*;
use crate::core::reasons::Reason;

/// Gate admission: inadmissible with a reason, or admissible with a strength
/// and the basis the Mandate will record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResult {
    Inadmissible { reason: String },
    Admissible { strength: Strength, basis: MandateBasis },
}

/// Versioned rules from the profile; `gate` decides who may originate work.
pub trait Policy {
    fn version(&self) -> String;
    fn paused(&self) -> bool;
    fn gate(&self, request: &Request) -> GateResult;
    /// Re-check a mandate under this policy, such as before an ended ticket resumes.
    fn revalidate(&self, mandate: &Mandate) -> bool;
    /// Whether an inferred candidate may be accepted on the rules its assessment matched.
    fn admits_inferred(&self, rules: &[String]) -> bool;
    /// How long the superagent may take before an evaluation is recorded unavailable.
    fn eval_timeout_ms(&self) -> i64;
    /// The mode a ticket runs in: the request's or the superagent's proposal, capped by the profile's rules.
    fn mode_for(&self, request: &Request, proposed: Option<Mode>) -> Mode;
    /// The workflow a ticket runs under and what it demands; a named workflow wins, else the policy chooses.
    fn plan_for(&self, request: &Request, workflow: Option<&str>, _title: &str) -> WorkPlan {
        let _ = request;
        WorkPlan {
            workflow: workflow.map(str::to_string),
            pack: None,
            root_role: "worker".into(),
            required_checkpoints: Vec::new(),
        }
    }
}

/// What the superagent session decides about a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Outcome {
    Ignore {
        reason: Reason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audit: Option<serde_json::Value>,
    },
    Unavailable {
        reason: Reason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audit: Option<serde_json::Value>,
    },
    Accept {
        title: String,
        scope: String,
        reason: Reason,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rules: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<Mode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audit: Option<serde_json::Value>,
    },
    Attach {
        ticket_id: String,
        reason: Reason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Stop {
        ticket_id: String,
        reason: Reason,
    },
}

/// The outcome of an evaluation the superagent names, so a stale one is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_id: Option<String>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Semantic judgement by the superagent session. `request` queues the
/// candidate, idempotently per evaluation; the outcome returns through `Engine::decide`.
pub trait Assessor {
    fn request(&self, candidate: &Candidate);
    /// Called whenever an evaluation ends, decided or not.
    fn release(&self, _eval_id: &str) {}
}

/// Runs assignments in herdr panes. The engine calls it inside its commands,
/// so each method only queues work, which reports back through the engine.
pub trait Executor {
    fn start(&self, ticket: &Ticket, assignment: &Assignment);
    fn deliver(&self, ticket: &Ticket, assignment: &Assignment);
    fn stop(&self, ticket: &Ticket, assignment: &Assignment, reason: &str);
}

/// What the engine reports after a change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    RequestReceived {
        request_id: String,
    },
    EvalStarted {
        eval_id: String,
        request_id: String,
        strength: Strength,
    },
    DecisionRecorded {
        decision_id: String,
        kind: DecisionKind,
        request_id: String,
    },
    TicketCreated {
        ticket_id: String,
        decision_id: String,
        strength: Strength,
    },
    TicketResumed {
        ticket_id: String,
        request_id: String,
    },
    TicketEnded {
        ticket_id: String,
        by: TicketEndBy,
        outcome: TicketOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    AssignmentStarted {
        ticket_id: String,
        assignment_id: String,
    },
    AssignmentEnded {
        ticket_id: String,
        assignment_id: String,
        reason: String,
    },
    CheckpointReached {
        ticket_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        summary: String,
        /// It replaced a checkpoint that was still waiting (a revised plan).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        revised: bool,
    },
    CheckpointApproved {
        ticket_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
    },
    /// A person sent a checkpoint back instead of approving it.
    ChangesRequested {
        ticket_id: String,
        name: String,
        comment: String,
    },
    /// A worker's report, so the record tells the whole story.
    Reported {
        ticket_id: String,
        assignment_id: String,
        role: String,
        state: ReportState,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        question: Option<String>,
    },
    /// Context or a follow-up that reached the ticket: `note`, `request`, `spawn`.
    InputAdded {
        ticket_id: String,
        kind: String,
        text: String,
    },
    Retitled {
        ticket_id: String,
        from: String,
        to: String,
    },
    RequestRequeued {
        request_id: String,
    },
    PolicyChanged {
        from: String,
        to: String,
    },
}

/// Durable state. Every method is synchronous so `atomic` can wrap a unit of work.
pub trait Repositories {
    /// Runs `work` in a transaction (nested inside the current one if any); an
    /// error rolls its changes back.
    fn atomic(&self, work: &mut dyn FnMut() -> Result<(), UserError>) -> Result<(), UserError>;

    fn request_get(&self, id: &str) -> Option<Request>;
    fn request_save(&self, request: &Request);
    fn request_list(&self) -> Vec<Request>;
    fn disposition_get(&self, id: &str) -> Option<RequestDisposition>;
    fn disposition_save(&self, id: &str, value: &RequestDisposition);

    fn eval_get(&self, id: &str) -> Option<Evaluation>;
    fn eval_by_request(&self, request_id: &str) -> Option<Evaluation>;
    fn eval_save(&self, eval: &Evaluation);
    fn eval_delete(&self, id: &str);
    /// Evaluations in progress.
    fn eval_list(&self) -> Vec<Evaluation>;

    fn ticket_get(&self, id: &str) -> Option<Ticket>;
    fn ticket_save(&self, ticket: &Ticket);
    fn ticket_list(&self) -> Vec<Ticket>;
    fn ticket_open(&self) -> Vec<Ticket>;
    /// Tickets with undelivered inputs or a pending or running assignment.
    fn ticket_active(&self) -> Vec<Ticket>;

    fn decision_get(&self, id: &str) -> Option<Decision>;
    fn decision_save(&self, decision: &Decision);
    fn decision_list(&self) -> Vec<Decision>;
}
