//! Tickets: their lifecycle from an accept to their end, their inputs,
//! checkpoints, retries, restarts and resumes.

use std::collections::HashMap;

use crate::core::engine::Engine;
use crate::core::errors::{ErrorCode, UserError};
use crate::core::ids::{key, short};
use crate::core::model::*;
use crate::core::ports::DomainEvent;
use crate::core::stage::can_retry;

pub const STATUS_REQUEST: &str = "Report the current ticket status with `herdr-superagent report`. This is a status-only request: do not resume implementation or perform external actions.";
pub const RECOVERY_NOTE: &str = "herdr-superagent restarted during this ticket. Inspect the current worktree, git history and remote state before continuing. Do not repeat an already completed external operation. Finish the original assignment and report its status.";

/// What an accept gives a new ticket, beyond its request and mandate.
pub struct NewTicket {
    pub decision_id: String,
    pub title: String,
    pub workflow: Option<String>,
    pub name: Option<String>,
    pub note: Option<String>,
}

impl Engine {
    pub(crate) fn create_ticket(&self, request: &Request, mandate: Mandate, new: NewTicket) -> Result<Ticket, UserError> {
        let now = self.now();
        let NewTicket {
            decision_id,
            title,
            workflow,
            name,
            note,
        } = new;
        let mut id = short("t", &[&request.id, &decision_id]);
        if self.repo().ticket_get(&id).is_some() {
            id = format!("t-{}", key(&[&request.id, &decision_id]));
        }
        let mut ticket = Ticket {
            id: id.clone(),
            title,
            decision_id,
            mandate,
            project: request.project.clone(),
            workflow,
            ended: None,
            attention: None,
            checkpoint: None,
            assignments: Vec::new(),
            inputs: Vec::new(),
            undelivered: Vec::new(),
            created_at: now.clone(),
            updated_at: now.clone(),
            origin: Origin {
                request_id: request.id.clone(),
            },
            execution: name.map(|name| Execution {
                name,
                directory: String::new(),
                assignment: String::new(),
            }),
            delivery: None,
        };
        if let Some(note) = note {
            let input = TicketInput::Note {
                id: key(&["input", &id, "note", &now, &note]),
                text: note,
                at: now.clone(),
            };
            ticket.undelivered.push(input.id().to_string());
            ticket.inputs.push(input);
        }
        let assignment = self.new_assignment(&mut ticket, "worker", None);
        self.repo().ticket_save(&ticket);
        self.executor().start(&ticket, &assignment);
        Ok(ticket)
    }

    pub(crate) fn new_assignment(&self, ticket: &mut Ticket, role: &str, parent: Option<String>) -> Assignment {
        let n = ticket.assignments.len().to_string();
        let assignment = Assignment {
            id: key(&["assignment", &ticket.id, &n]),
            role: role.to_string(),
            state: AssignmentState::Pending,
            created_at: self.now(),
            started_at: None,
            ended_at: None,
            ended_reason: None,
            report: None,
            delivered_at: None,
            idle_at: None,
            pane_id: None,
            parent,
        };
        ticket.assignments.push(assignment.clone());
        assignment
    }

    /// Adds an input and hands it on: to the running assignment, or by
    /// starting a pending one. Automatic inputs are held while the worker
    /// waits for a person.
    pub(crate) fn add_input(&self, ticket: &mut Ticket, input: TicketInput) {
        if ticket.input(input.id()).is_some() {
            return;
        }
        let held = ticket.live() && input.automatic() && ticket.waits_for_person();
        if !held {
            ticket.undelivered.push(input.id().to_string());
        }
        ticket.inputs.push(input);
        ticket.updated_at = self.now();
        self.repo().ticket_save(ticket);
        if !ticket.live() || held {
            return;
        }
        if let Some(running) = ticket.running_assignment() {
            self.executor().deliver(ticket, running);
        } else if let Some(latest) = ticket.latest_assignment() {
            if latest.state == AssignmentState::Pending && !ticket.waits_for_person() {
                self.executor().start(ticket, latest);
            }
        }
    }

    /// Whether a request may be attached to a ticket: not one a person stopped,
    /// nor one whose mandate the profile no longer holds.
    pub(crate) fn attachable(&self, ticket_id: &str) -> Result<Ticket, UserError> {
        let ticket = self.require_ticket(ticket_id)?;
        if let Some(ended) = &ticket.ended {
            if ended.by == TicketEndBy::Human {
                return Err(UserError::new(ErrorCode::TicketStopped));
            }
            if !self.policy().revalidate(&ticket.mandate) {
                return Err(UserError::new(ErrorCode::MandateRevoked));
            }
        }
        Ok(ticket)
    }

    /// Attach a request to a ticket as an input with the routing note. A note
    /// for a request the ticket already has goes out as a note of its own.
    pub(crate) fn attach(&self, ticket_id: &str, request: &Request, note: Option<String>) -> Result<(), UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        let now = self.now();
        let id = key(&["input", ticket_id, "request", &request.id]);
        if ticket.input(&id).is_some() {
            if let Some(note) = note {
                let input = TicketInput::Note {
                    id: key(&["input", ticket_id, "note", &now, &note]),
                    text: note,
                    at: now,
                };
                self.add_input(&mut ticket, input);
            }
            return Ok(());
        }
        let input = TicketInput::Request {
            id,
            request_id: request.id.clone(),
            at: now,
            human: request.human,
            note,
            delivered_to: None,
            delivered_at: None,
        };
        self.add_input(&mut ticket, input);
        Ok(())
    }

    /// Resume an ended ticket a person's message reached, under its mandate.
    pub(crate) fn wake(&self, ticket_id: &str) -> Result<(), UserError> {
        let Some(mut ticket) = self.repo().ticket_get(ticket_id) else {
            return Ok(());
        };
        if ticket.live() || ticket.undelivered.is_empty() {
            return Ok(());
        }
        if ticket.ended.as_ref().map(|e| e.by) == Some(TicketEndBy::Human) || self.policy().paused() {
            return Ok(());
        }
        let first = ticket
            .undelivered
            .iter()
            .filter_map(|id| ticket.input(id))
            .find(|input| input.is_human_request())
            .cloned();
        let Some(TicketInput::Request { request_id, .. }) = first else {
            return Ok(());
        };
        if !self.policy().revalidate(&ticket.mandate) {
            // The mandate no longer holds: what waited goes back to evaluation.
            let waiting: Vec<String> = std::mem::take(&mut ticket.undelivered);
            self.repo().ticket_save(&ticket);
            for id in waiting {
                if let Some(TicketInput::Request {
                    request_id, human: true, ..
                }) = ticket.input(&id)
                {
                    self.let_go(&request_id.clone())?;
                }
            }
            return Ok(());
        }
        let at = self.now();
        ticket.ended = None;
        ticket.updated_at = at.clone();
        let assignment = self.new_assignment(&mut ticket, "worker", None);
        let resumed = TicketInput::Resumed {
            id: key(&["input", ticket_id, "resumed", &request_id]),
            request_id: request_id.clone(),
            at,
        };
        ticket.undelivered.insert(0, resumed.id().to_string());
        ticket.inputs.push(resumed);
        self.repo().ticket_save(&ticket);
        self.executor().start(&ticket, &assignment);
        self.emit(DomainEvent::TicketResumed {
            ticket_id: ticket_id.to_string(),
            request_id,
        });
        Ok(())
    }

    fn end_assignment(&self, ticket: &mut Ticket, assignment_id: &str, reason: &str) {
        let now = self.now();
        if let Some(assignment) = ticket.assignments.iter_mut().find(|a| a.id == assignment_id) {
            assignment.state = AssignmentState::Ended;
            assignment.ended_at = Some(now);
            assignment.ended_reason = Some(reason.to_string());
        }
        self.emit(DomainEvent::AssignmentEnded {
            ticket_id: ticket.id.clone(),
            assignment_id: assignment_id.to_string(),
            reason: reason.to_string(),
        });
    }

    /// Inputs the ended assignment received after its last report go to the next one.
    fn requeue_untaken(&self, ticket: &mut Ticket, assignment_id: &str) {
        let report_at = ticket
            .assignments
            .iter()
            .find(|a| a.id == assignment_id)
            .and_then(|a| a.report.as_ref())
            .map(|r| r.at.clone())
            .unwrap_or_default();
        let untaken: Vec<String> = ticket
            .inputs
            .iter()
            .filter_map(|input| match input {
                TicketInput::Request {
                    id,
                    delivered_to: Some(to),
                    delivered_at: Some(at),
                    ..
                } if to == assignment_id && *at > report_at => Some(id.clone()),
                _ => None,
            })
            .collect();
        for id in untaken {
            if !ticket.undelivered.contains(&id) {
                ticket.undelivered.push(id);
            }
        }
    }

    pub(crate) fn retry_inner(&self, ticket_id: &str) -> Result<Ticket, UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        let Some(latest) = ticket.latest_assignment().cloned() else {
            return Err(UserError::new(ErrorCode::NotRetryable));
        };
        if !can_retry(&ticket) {
            return Err(UserError::new(ErrorCode::NotRetryable));
        }
        self.end_assignment(&mut ticket, &latest.id, "retried");
        self.requeue_untaken(&mut ticket, &latest.id);
        ticket.delivery = None;
        let assignment = self.new_assignment(&mut ticket, &latest.role, latest.parent.clone());
        ticket.updated_at = self.now();
        self.repo().ticket_save(&ticket);
        self.executor().stop(&ticket, &latest, "retried");
        self.executor().start(&ticket, &assignment);
        Ok(ticket)
    }

    /// Ends the running assignment and starts a new one with a note ahead of what waits.
    pub(crate) fn restart(&self, ticket: &mut Ticket, running_id: &str, reason: &str, note: &str) {
        self.end_assignment(ticket, running_id, reason);
        self.requeue_untaken(ticket, running_id);
        let role = ticket
            .assignments
            .iter()
            .find(|a| a.id == running_id)
            .map(|a| a.role.clone())
            .unwrap_or_else(|| "worker".into());
        let assignment = self.new_assignment(ticket, &role, None);
        let at = self.now();
        let input = TicketInput::Text {
            id: key(&["input", &ticket.id, "restart", &assignment.id]),
            text: note.to_string(),
            at: at.clone(),
        };
        ticket.undelivered.insert(0, input.id().to_string());
        ticket.inputs.push(input);
        ticket.updated_at = at;
        self.repo().ticket_save(ticket);
        self.executor().start(ticket, &assignment);
    }

    pub(crate) fn end_ticket(
        &self,
        ticket_id: &str,
        by: TicketEndBy,
        outcome: TicketOutcome,
        at: Option<String>,
        reason: Option<String>,
    ) -> Result<(), UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        if !ticket.live() {
            return Ok(());
        }
        let at = at.unwrap_or_else(|| self.now());
        let stopping: Vec<Assignment> = ticket
            .assignments
            .iter()
            .filter(|a| a.state != AssignmentState::Ended)
            .cloned()
            .collect();
        let reason = reason.filter(|r| !r.is_empty());
        // A person's messages the worker never got go back to evaluation.
        let let_go: Vec<String> = ticket
            .undelivered
            .iter()
            .filter_map(|id| match ticket.input(id) {
                Some(TicketInput::Request {
                    request_id, human: true, ..
                }) if *request_id != ticket.origin.request_id => Some(request_id.clone()),
                _ => None,
            })
            .collect();
        ticket.ended = Some(Ended {
            at: at.clone(),
            by,
            outcome,
            reason: reason.clone(),
        });
        ticket.undelivered.clear();
        ticket.attention = None;
        ticket.checkpoint = None;
        ticket.delivery = None;
        ticket.updated_at = self.now();
        for assignment in ticket.assignments.iter_mut().filter(|a| a.state != AssignmentState::Ended) {
            assignment.state = AssignmentState::Ended;
            assignment.ended_at = Some(at.clone());
        }
        self.repo().ticket_save(&ticket);
        for request_id in let_go {
            self.let_go(&request_id)?;
        }
        self.emit(DomainEvent::TicketEnded {
            ticket_id: ticket_id.to_string(),
            by,
            outcome,
            reason: reason.clone(),
        });
        let why = reason.unwrap_or_else(|| format!("{outcome:?}").to_lowercase());
        for assignment in stopping {
            self.executor().stop(&ticket, &assignment, &why);
        }
        Ok(())
    }

    pub(crate) fn note_inner(&self, ticket_id: &str, text: &str) -> Result<Ticket, UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        if !ticket.live() {
            return Err(UserError::new(ErrorCode::TicketEnded));
        }
        let at = self.now();
        let input = TicketInput::Note {
            id: key(&["input", ticket_id, "note", &at, text]),
            text: text.to_string(),
            at,
        };
        self.add_input(&mut ticket, input);
        self.require_ticket(ticket_id)
    }

    pub(crate) fn request_status_inner(&self, ticket_id: &str) -> Result<bool, UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        let waiting =
            ticket.live() && ticket.running_assignment().and_then(|a| a.report.as_ref()).map(|r| r.state) == Some(ReportState::Waiting);
        if !waiting {
            return Ok(false);
        }
        let at = self.now();
        let input = TicketInput::Text {
            id: format!("status:{ticket_id}:{}", ticket.updated_at),
            text: STATUS_REQUEST.to_string(),
            at,
        };
        self.add_input(&mut ticket, input);
        Ok(true)
    }

    pub(crate) fn checkpoint_inner(&self, ticket_id: &str, name: &str, summary: &str) -> Result<Ticket, UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        if !ticket.live() {
            return Err(UserError::new(ErrorCode::TicketEnded));
        }
        if let Some(open) = &ticket.checkpoint {
            return Err(UserError::new(ErrorCode::CheckpointOpen).with("checkpoint", &open.name));
        }
        let at = self.now();
        let assignment = ticket
            .assignments
            .iter_mut()
            .rev()
            .find(|a| a.state != AssignmentState::Ended)
            .ok_or_else(|| UserError::new(ErrorCode::AssignmentEnded))?;
        // The worker's turn ends here: the assignment is settled until a person approves.
        assignment.report = Some(Report {
            state: ReportState::Waiting,
            summary: summary.to_string(),
            at: at.clone(),
        });
        ticket.checkpoint = Some(Checkpoint {
            id: key(&["checkpoint", ticket_id, name, &at]),
            name: name.to_string(),
            summary: summary.to_string(),
            at: at.clone(),
        });
        ticket.updated_at = at;
        self.repo().ticket_save(&ticket);
        self.emit(DomainEvent::CheckpointReached {
            ticket_id: ticket_id.to_string(),
            name: name.to_string(),
        });
        Ok(ticket)
    }

    pub(crate) fn approve_inner(&self, ticket_id: &str, comment: Option<String>) -> Result<Ticket, UserError> {
        let mut ticket = self.require_ticket(ticket_id)?;
        if !ticket.live() {
            return Err(UserError::new(ErrorCode::TicketEnded));
        }
        let Some(checkpoint) = ticket.checkpoint.take() else {
            return Err(UserError::new(ErrorCode::NoCheckpoint));
        };
        let at = self.now();
        let mut text = format!("Checkpoint '{}' approved. Continue with the next step.", checkpoint.name);
        if let Some(comment) = comment.filter(|c| !c.trim().is_empty()) {
            text.push_str("\nReviewer's comment: ");
            text.push_str(&comment);
        }
        let input = TicketInput::Text {
            id: key(&["input", ticket_id, "approve", &checkpoint.id]),
            text,
            at,
        };
        self.add_input(&mut ticket, input);
        self.emit(DomainEvent::CheckpointApproved {
            ticket_id: ticket_id.to_string(),
            name: checkpoint.name,
        });
        self.require_ticket(ticket_id)
    }

    pub(crate) fn policy_changed_inner(&self, from: &str, to: &str, effects: &HashMap<String, PolicyEffect>) -> Result<(), UserError> {
        for (ticket_id, effect) in effects {
            let Some(mut ticket) = self.repo().ticket_get(ticket_id) else {
                continue;
            };
            if !ticket.live() {
                continue;
            }
            let at = self.now();
            let input = TicketInput::PolicyChanged {
                id: key(&["input", ticket_id, "policy", from, to, &at]),
                from: from.to_string(),
                to: to.to_string(),
                at,
                effect: Some(effect.clone()),
            };
            self.add_input(&mut ticket, input);
        }
        self.evals_policy_changed()?;
        self.emit(DomainEvent::PolicyChanged {
            from: from.to_string(),
            to: to.to_string(),
        });
        Ok(())
    }
}
