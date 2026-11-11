//! What the executor and its workers report about the assignments that run tickets.

use crate::core::engine::{Engine, Question, WorkerReport};
use crate::core::errors::{ErrorCode, UserError};
use crate::core::ids::key;
use crate::core::model::*;
use crate::core::ports::DomainEvent;
use crate::core::tickets::RECOVERY_NOTE;

impl Engine {
    /// The ticket, while it is open and `assignment_id` runs it.
    fn running(&self, ticket_id: &str, assignment_id: &str) -> Result<Option<Ticket>, UserError> {
        let ticket = self.require_ticket(ticket_id)?;
        let runs = ticket.live()
            && ticket
                .assignments
                .iter()
                .any(|a| a.id == assignment_id && a.state == AssignmentState::Running);
        Ok(runs.then_some(ticket))
    }

    pub(crate) fn started_inner(&self, ticket_id: &str, assignment_id: &str) -> Result<(), UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        let now = self.now();
        let Some(assignment) = ticket.assignments.iter_mut().find(|a| a.id == assignment_id) else {
            return Ok(());
        };
        if assignment.state == AssignmentState::Ended {
            return Ok(());
        }
        assignment.state = AssignmentState::Running;
        assignment.started_at.get_or_insert(now.clone());
        ticket.updated_at = now;
        self.repo().ticket_save(&ticket);
        if ticket.live() && !ticket.undelivered.is_empty() {
            if let Some(running) = ticket.running_assignment() {
                self.executor().deliver(&ticket, running);
            }
        }
        self.emit(DomainEvent::AssignmentStarted {
            ticket_id: ticket_id.to_string(),
            assignment_id: assignment_id.to_string(),
        });
        Ok(())
    }

    pub(crate) fn placed_inner(&self, ticket_id: &str, assignment_id: &str, pane_id: &str) -> Result<(), UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        if let Some(assignment) = ticket.assignments.iter_mut().find(|a| a.id == assignment_id) {
            assignment.pane_id = Some(pane_id.to_string());
            ticket.updated_at = self.now();
            self.repo().ticket_save(&ticket);
        }
        Ok(())
    }

    pub(crate) fn delivered_inner(&self, ticket_id: &str, assignment_id: &str, input_ids: &[String]) -> Result<(), UserError> {
        let Some(mut ticket) = self.running(ticket_id, assignment_id)? else {
            return Ok(());
        };
        let now = self.now();
        ticket.undelivered.retain(|id| !input_ids.contains(id));
        ticket.updated_at = now.clone();
        if let Some(running) = ticket.assignment_mut(assignment_id) {
            running.delivered_at = Some(now.clone());
        }
        for input in ticket.inputs.iter_mut() {
            if let TicketInput::Request {
                id,
                delivered_to,
                delivered_at,
                ..
            } = input
            {
                if input_ids.contains(id) {
                    *delivered_to = Some(assignment_id.to_string());
                    *delivered_at = Some(now.clone());
                }
            }
        }
        ticket.delivery = None;
        self.repo().ticket_save(&ticket);
        Ok(())
    }

    pub(crate) fn turn_ended_inner(&self, ticket_id: &str, assignment_id: &str) -> Result<(), UserError> {
        let Some(mut ticket) = self.running(ticket_id, assignment_id)? else {
            return Ok(());
        };
        let now = self.now();
        if let Some(running) = ticket.assignment_mut(assignment_id) {
            running.idle_at = Some(now);
        }
        self.repo().ticket_save(&ticket);
        Ok(())
    }

    pub(crate) fn delivery_failed_inner(&self, ticket_id: &str, assignment_id: &str, error: &str) -> Result<(), UserError> {
        let Some(mut ticket) = self.running(ticket_id, assignment_id)? else {
            return Ok(());
        };
        let error: String = error.chars().take(300).collect();
        ticket.delivery = Some(DeliveryFailure { error, at: self.now() });
        self.repo().ticket_save(&ticket);
        Ok(())
    }

    /// Restarts an assignment interrupted mid-work with a recovery note; one
    /// that had settled (waiting, ready, blocked, failed, and nothing delivered
    /// since) keeps its session and gets what waits for it.
    pub(crate) fn interrupted_inner(&self, ticket_id: &str, assignment_id: &str) -> Result<(), UserError> {
        let Some(mut ticket) = self.running(ticket_id, assignment_id)? else {
            return Ok(());
        };
        let Some(running) = ticket.assignments.iter().find(|a| a.id == assignment_id).cloned() else {
            return Ok(());
        };
        let busy = matches!((&running.delivered_at, &running.report), (Some(delivered), Some(report)) if delivered >= &report.at);
        let settled = running
            .report
            .as_ref()
            .map(|r| {
                matches!(
                    r.state,
                    ReportState::Waiting | ReportState::Ready | ReportState::Blocked | ReportState::Failed
                )
            })
            .unwrap_or(false)
            && !busy;
        if !settled {
            self.restart(&mut ticket, &running.id, "hostRestarted", RECOVERY_NOTE);
        } else if !ticket.undelivered.is_empty() {
            self.executor().deliver(&ticket, &running);
        }
        Ok(())
    }

    pub(crate) fn report_inner(&self, ticket_id: &str, report: WorkerReport) -> Result<Ticket, UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        if !ticket.live() {
            return Err(UserError::new(ErrorCode::TicketEnded));
        }
        if matches!(report.question, Question::Ask(_)) && report.state == ReportState::Ready {
            return Err(UserError::new(ErrorCode::QuestionWithResult));
        }
        let targets_child = report
            .assignment_id
            .as_ref()
            .map(|id| ticket.assignments.iter().any(|a| &a.id == id && !a.is_root()))
            .unwrap_or(false);
        // Checkpoint and propose modes mean a person reviews before the work counts as done.
        if report.state == ReportState::Ready && !targets_child && ticket.mandate.mode != Mode::Autonomous {
            let missing: Vec<&str> = ticket
                .required_checkpoints
                .iter()
                .filter(|name| !ticket.approved_checkpoints.contains(name))
                .map(String::as_str)
                .collect();
            if ticket.approved_checkpoints.is_empty() || !missing.is_empty() {
                let mut error = UserError::new(ErrorCode::CheckpointRequired).with("mode", ticket.mandate.mode.as_str());
                if !missing.is_empty() {
                    error = error.with("checkpoints", &missing.join(", "));
                }
                return Err(error);
            }
        }
        let at = self.now();
        let index = match &report.assignment_id {
            Some(id) => ticket
                .assignments
                .iter()
                .position(|a| &a.id == id)
                .ok_or_else(|| UserError::new(ErrorCode::UnknownAssignment))?,
            None => ticket
                .assignments
                .iter()
                .position(|a| a.state == AssignmentState::Running && a.is_root())
                .or_else(|| ticket.assignments.iter().rposition(|a| a.is_root()))
                .ok_or_else(|| UserError::new(ErrorCode::UnknownAssignment))?,
        };
        let is_child = !ticket.assignments[index].is_root();
        if ticket.assignments[index].state == AssignmentState::Ended {
            return Err(UserError::new(ErrorCode::AssignmentEnded));
        }
        let previous = ticket.attention.clone();
        let attention = match (&report.question, report.state) {
            (_, ReportState::Ready) | (Question::Clear, _) => None,
            (Question::Keep, _) => previous.clone(),
            (Question::Ask(question), _) => match &previous {
                Some(current) if &current.question == question => previous.clone(),
                _ => Some(Attention {
                    id: key(&["attention", ticket_id, question, &at]),
                    question: question.clone(),
                    request_id: Some(ticket.origin.request_id.clone()),
                }),
            },
        };
        ticket.attention = attention;
        self.emit(DomainEvent::Reported {
            ticket_id: ticket_id.to_string(),
            assignment_id: ticket.assignments[index].id.clone(),
            role: ticket.assignments[index].role.clone(),
            state: report.state,
            summary: report.summary.clone(),
            question: match &report.question {
                Question::Ask(q) => Some(q.clone()),
                _ => None,
            },
        });
        ticket.assignments[index].report = Some(Report {
            state: report.state,
            summary: report.summary,
            at: at.clone(),
        });
        ticket.updated_at = at.clone();
        if matches!(report.state, ReportState::Blocked | ReportState::Failed) {
            let held: Vec<String> = ticket
                .undelivered
                .iter()
                .filter(|id| ticket.input(id).map(|input| input.automatic()).unwrap_or(false))
                .cloned()
                .collect();
            ticket.undelivered.retain(|id| !held.contains(id));
        }
        self.repo().ticket_save(&ticket);
        if is_child {
            // A child's final report ends the child and tells the parent; the ticket goes on.
            if matches!(report.state, ReportState::Ready | ReportState::Dropped | ReportState::Failed) {
                let child = ticket.assignments[index].clone();
                let reason = report.state.as_str().to_string();
                if let Some(stored) = ticket.assignment_mut(&child.id) {
                    stored.state = AssignmentState::Ended;
                    stored.ended_at = Some(at.clone());
                    stored.ended_reason = Some(reason.clone());
                }
                self.repo().ticket_save(&ticket);
                self.emit(DomainEvent::AssignmentEnded {
                    ticket_id: ticket_id.to_string(),
                    assignment_id: child.id.clone(),
                    reason: reason.clone(),
                });
                self.executor().stop(&ticket, &child, &reason);
                let summary = child.report.as_ref().map(|r| r.summary.clone()).unwrap_or_default();
                let text = format!(
                    "Your {} ({}) finished with `{}`:\n{}",
                    child.role,
                    &child.id[..8.min(child.id.len())],
                    reason,
                    summary
                );
                let input = TicketInput::Text {
                    id: key(&["input", ticket_id, "child", &child.id, &reason]),
                    text,
                    at,
                };
                self.add_input(&mut ticket, input);
            }
            return self.require_ticket(ticket_id);
        }
        let outcome = match report.state {
            ReportState::Dropped => Some(TicketOutcome::Dropped),
            ReportState::Ready => Some(TicketOutcome::Completed),
            _ => None,
        };
        if let Some(outcome) = outcome {
            self.end_ticket(ticket_id, TicketEndBy::Executor, outcome, Some(at), None)?;
        }
        self.require_ticket(ticket_id)
    }
}
