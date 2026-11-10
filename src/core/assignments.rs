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
        let runs = ticket.live() && ticket.running_assignment().map(|a| a.id.as_str()) == Some(assignment_id);
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
        if let Some(running) = ticket.running_assignment_mut() {
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
        if let Some(running) = ticket.running_assignment_mut() {
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
        let Some(running) = ticket.running_assignment().cloned() else {
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
        // Checkpoint and propose modes mean a person reviews before the work counts as done.
        if report.state == ReportState::Ready && ticket.mandate.mode != Mode::Autonomous && ticket.approved_checkpoints.is_empty() {
            return Err(UserError::new(ErrorCode::CheckpointRequired).with("mode", ticket.mandate.mode.as_str()));
        }
        let at = self.now();
        let index = ticket
            .assignments
            .iter()
            .position(|a| a.state == AssignmentState::Running)
            .or_else(|| ticket.assignments.len().checked_sub(1))
            .ok_or_else(|| UserError::new(ErrorCode::UnknownAssignment))?;
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
