//! The Engine: the single transactional entry point of the core. It
//! coordinates evaluations, tickets and assignments inside repository
//! transactions and queues side effects through the ports.

use std::collections::HashMap;
use std::rc::Rc;

use crate::core::errors::{ErrorCode, UserError};
use crate::core::model::*;
use crate::core::ports::*;

pub struct EngineOptions {
    pub repo: Rc<dyn Repositories>,
    pub policy: Rc<dyn Policy>,
    pub assessor: Rc<dyn Assessor>,
    pub executor: Rc<dyn Executor>,
    pub now: Rc<dyn Fn() -> String>,
    pub emit: Option<Rc<dyn Fn(DomainEvent)>>,
}

/// What a worker (or the executor on its behalf) reports about an assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerReport {
    pub state: ReportState,
    pub summary: String,
    pub question: Question,
    /// The assignment that reports; `None` means the ticket's running root assignment.
    pub assignment_id: Option<String>,
}

/// A worker's question for a person: asked, kept, or cleared.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Question {
    #[default]
    Keep,
    Clear,
    Ask(String),
}

impl Question {
    /// `None` keeps, `Some("")` clears, text asks (the CLI's `--question` semantics).
    pub fn from_option(value: Option<String>) -> Question {
        match value {
            None => Question::Keep,
            Some(text) if text.trim().is_empty() => Question::Clear,
            Some(text) => Question::Ask(text),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveResult {
    Stored,
    Duplicate,
    Filtered,
}

pub struct Engine {
    pub(crate) options: EngineOptions,
}

impl Engine {
    pub fn new(options: EngineOptions) -> Engine {
        Engine { options }
    }

    pub(crate) fn repo(&self) -> &dyn Repositories {
        self.options.repo.as_ref()
    }
    pub(crate) fn policy(&self) -> &dyn Policy {
        self.options.policy.as_ref()
    }
    pub(crate) fn executor(&self) -> &dyn Executor {
        self.options.executor.as_ref()
    }
    pub(crate) fn assessor(&self) -> &dyn Assessor {
        self.options.assessor.as_ref()
    }
    pub(crate) fn now(&self) -> String {
        (self.options.now)()
    }
    pub(crate) fn emit(&self, event: DomainEvent) {
        if let Some(emit) = &self.options.emit {
            emit(event);
        }
    }
    pub(crate) fn require_ticket(&self, id: &str) -> Result<Ticket, UserError> {
        self.repo()
            .ticket_get(id)
            .ok_or_else(|| UserError::new(ErrorCode::UnknownTicket).with("ticket", id))
    }
    pub(crate) fn require_request(&self, id: &str) -> Result<Request, UserError> {
        self.repo()
            .request_get(id)
            .ok_or_else(|| UserError::new(ErrorCode::UnknownRequest).with("request", id))
    }

    /// Runs `work` in one repository transaction and returns its value.
    pub(crate) fn atomic<T>(&self, work: impl FnOnce() -> Result<T, UserError>) -> Result<T, UserError> {
        let mut work = Some(work);
        let mut out = None;
        self.repo().atomic(&mut || {
            let work = work.take().ok_or_else(|| UserError::internal("transaction body ran twice"))?;
            out = Some(work()?);
            Ok(())
        })?;
        out.ok_or_else(|| UserError::internal("transaction produced no value"))
    }

    // ---- Sources and the superagent session ----

    /// Take in a request: store it once, gate it, and queue it for evaluation.
    pub fn receive(&self, request: Request) -> Result<ReceiveResult, UserError> {
        self.atomic(|| self.receive_inner(request))
    }

    /// The clock: expire or requeue overdue evaluations, resume ended tickets a
    /// person's message reached while paused, and start waiting evaluations.
    pub fn evaluate_due(&self) -> Result<(), UserError> {
        self.atomic(|| self.evaluate_due_inner())
    }

    /// The candidate of the evaluation that holds a request, as the superagent sees it.
    pub fn candidate(&self, request_id: &str) -> Option<Candidate> {
        self.candidate_inner(request_id)
    }

    /// The candidate reached the superagent session: its deadline counts from now.
    pub fn candidate_delivered(&self, eval_id: &str) -> Result<(), UserError> {
        self.atomic(|| self.candidate_delivered_inner(eval_id))
    }

    /// Decide a request: accept it into a ticket, attach it to one, stop one, or ignore it.
    pub fn decide(&self, request_id: &str, input: DecisionInput) -> Result<Decision, UserError> {
        self.atomic(|| self.decide_inner(request_id, input))
    }

    /// Queue a stored request for a fresh evaluation; false while one holds it.
    pub fn reevaluate(&self, request_id: &str) -> Result<bool, UserError> {
        self.atomic(|| self.reevaluate_inner(request_id))
    }

    /// A person dismisses the problem an unavailable decision shows.
    pub fn dismiss(&self, decision_id: &str) -> Result<Decision, UserError> {
        self.atomic(|| self.dismiss_inner(decision_id))
    }

    // ---- People ----

    pub fn stop_ticket(&self, ticket_id: &str, reason: Option<String>) -> Result<Ticket, UserError> {
        self.atomic(|| {
            self.end_ticket(ticket_id, TicketEndBy::Human, TicketOutcome::Stopped, None, reason)?;
            self.require_ticket(ticket_id)
        })
    }

    pub fn retry_ticket(&self, ticket_id: &str) -> Result<Ticket, UserError> {
        self.atomic(|| self.retry_inner(ticket_id))
    }

    pub fn note(&self, ticket_id: &str, text: &str) -> Result<Ticket, UserError> {
        self.atomic(|| self.note_inner(ticket_id, text))
    }

    pub fn retitle(&self, ticket_id: &str, title: &str) -> Result<Ticket, UserError> {
        self.atomic(|| self.retitle_inner(ticket_id, title))
    }

    /// Send the waiting checkpoint back to the worker with what must change.
    pub fn request_changes(&self, ticket_id: &str, comment: &str) -> Result<Ticket, UserError> {
        self.atomic(|| self.request_changes_inner(ticket_id, comment))
    }

    /// Ask a worker that waits for its status; any other worker is left alone.
    pub fn request_status(&self, ticket_id: &str) -> Result<bool, UserError> {
        self.atomic(|| self.request_status_inner(ticket_id))
    }

    /// Approve the checkpoint a ticket waits at; the worker continues.
    pub fn approve(&self, ticket_id: &str, comment: Option<String>) -> Result<Ticket, UserError> {
        self.atomic(|| self.approve_inner(ticket_id, comment))
    }

    // ---- Workers, through the CLI ----

    pub fn report(&self, ticket_id: &str, report: WorkerReport) -> Result<Ticket, UserError> {
        self.atomic(|| self.report_inner(ticket_id, report))
    }

    /// A worker stops at a declared milestone and waits for review.
    pub fn checkpoint(&self, ticket_id: &str, name: &str, summary: &str) -> Result<Ticket, UserError> {
        self.atomic(|| self.checkpoint_inner(ticket_id, name, summary, None))
    }

    /// `checkpoint` for a named assignment (a child's checkpoint also waits for review).
    pub fn checkpoint_as(&self, ticket_id: &str, assignment_id: &str, name: &str, summary: &str) -> Result<Ticket, UserError> {
        self.atomic(|| self.checkpoint_inner(ticket_id, name, summary, Some(assignment_id)))
    }

    /// A worker spawns a child assignment (a reviewer, a builder) under its ticket, in the
    /// same mode or a stricter one.
    pub fn spawn(
        &self,
        ticket_id: &str,
        parent_assignment_id: &str,
        role: &str,
        task: &str,
        mode: Option<Mode>,
    ) -> Result<Assignment, UserError> {
        self.atomic(|| self.spawn_inner(ticket_id, parent_assignment_id, role, task, mode))
    }

    // ---- The executor ----

    pub fn assignment_started(&self, ticket_id: &str, assignment_id: &str) -> Result<(), UserError> {
        self.atomic(|| self.started_inner(ticket_id, assignment_id))
    }

    /// The executor placed an assignment in a herdr pane.
    pub fn assignment_placed(&self, ticket_id: &str, assignment_id: &str, pane_id: &str) -> Result<(), UserError> {
        self.atomic(|| self.placed_inner(ticket_id, assignment_id, pane_id))
    }

    pub fn inputs_delivered(&self, ticket_id: &str, assignment_id: &str, input_ids: &[String]) -> Result<(), UserError> {
        self.atomic(|| self.delivered_inner(ticket_id, assignment_id, input_ids))
    }

    pub fn turn_ended(&self, ticket_id: &str, assignment_id: &str) -> Result<(), UserError> {
        self.atomic(|| self.turn_ended_inner(ticket_id, assignment_id))
    }

    pub fn delivery_failed(&self, ticket_id: &str, assignment_id: &str, error: &str) -> Result<(), UserError> {
        self.atomic(|| self.delivery_failed_inner(ticket_id, assignment_id, error))
    }

    /// An assignment was running when the daemon stopped.
    pub fn assignment_interrupted(&self, ticket_id: &str, assignment_id: &str) -> Result<(), UserError> {
        self.atomic(|| self.interrupted_inner(ticket_id, assignment_id))
    }

    // ---- Settings ----

    /// The profile changed: each open ticket in `effects` is told how, and
    /// evaluations in progress start again under the new version.
    pub fn policy_changed(&self, from: &str, to: &str, effects: &HashMap<String, PolicyEffect>) -> Result<(), UserError> {
        self.atomic(|| self.policy_changed_inner(from, to, effects))
    }
}
