//! Intake and evaluation: a request is stored once, gated, and evaluated by the
//! superagent session under a deadline; the decision creates, attaches to or
//! stops a ticket.

use crate::core::clock;
use crate::core::engine::{Engine, ReceiveResult};
use crate::core::errors::{ErrorCode, UserError};
use crate::core::ids::key;
use crate::core::model::*;
use crate::core::ports::*;
use crate::core::reasons::Reason;
use crate::core::tickets::NewTicket;

/// How many times an expired evaluation is queued again before it is unavailable.
pub const REQUEUES: u32 = 2;

/// What a decision records, before the evaluation's own facts are added.
struct DecisionDraft {
    kind: DecisionKind,
    reason: Vec<Reason>,
    ticket_id: Option<String>,
    mandate: Option<Mandate>,
    audit: Option<serde_json::Value>,
    /// A decision id chosen up front, so a ticket can point at it.
    id: Option<String>,
}

impl Engine {
    pub(crate) fn receive_inner(&self, received: Request) -> Result<ReceiveResult, UserError> {
        let mut request = received;
        request.at = clock::normalize(&request.at);
        if self.repo().request_get(&request.id).is_some() {
            return Ok(ReceiveResult::Duplicate);
        }
        let at = self.now();
        self.repo().request_save(&request);
        match self.policy().gate(&request) {
            GateResult::Inadmissible { reason } => {
                self.repo().disposition_save(
                    &request.id,
                    &RequestDisposition {
                        filtered: Some(Reason::code(&reason)),
                        at,
                        ..Default::default()
                    },
                );
                Ok(ReceiveResult::Filtered)
            }
            GateResult::Admissible { .. } => {
                self.repo()
                    .disposition_save(&request.id, &RequestDisposition { at, ..Default::default() });
                self.emit(DomainEvent::RequestReceived {
                    request_id: request.id.clone(),
                });
                if !self.policy().paused() {
                    self.start_eval(&request)?;
                }
                Ok(ReceiveResult::Stored)
            }
        }
    }

    /// Requests that wait for an evaluation: gated in, undecided or sent back, and not held by one.
    fn pending_requests(&self) -> Vec<Request> {
        self.repo()
            .request_list()
            .into_iter()
            .filter(|request| {
                let Some(disposition) = self.repo().disposition_get(&request.id) else {
                    return false;
                };
                disposition.filtered.is_none()
                    && disposition.eval_id.is_none()
                    && (disposition.decision_id.is_none() || disposition.requeued)
                    && disposition.attached.is_empty()
            })
            .collect()
    }

    fn start_eval(&self, request: &Request) -> Result<Option<Evaluation>, UserError> {
        let GateResult::Admissible { strength, basis } = self.policy().gate(request) else {
            return Ok(None);
        };
        let now = self.now();
        let eval = Evaluation {
            id: key(&["eval", &request.id, &now]),
            request_id: request.id.clone(),
            strength,
            basis,
            policy_version: self.policy().version(),
            started_at: now.clone(),
            delivered: None,
            deadline: clock::plus_ms(&now, self.policy().eval_timeout_ms()),
            paused_at: None,
            expired: 0,
        };
        self.repo().eval_save(&eval);
        let mut disposition = self.repo().disposition_get(&request.id).unwrap_or(RequestDisposition {
            at: now,
            ..Default::default()
        });
        disposition.eval_id = Some(eval.id.clone());
        self.repo().disposition_save(&request.id, &disposition);
        self.emit(DomainEvent::EvalStarted {
            eval_id: eval.id.clone(),
            request_id: request.id.clone(),
            strength,
        });
        if let Some(candidate) = self.candidate_of(&eval) {
            self.assessor().request(&candidate);
        }
        Ok(Some(eval))
    }

    fn candidate_of(&self, eval: &Evaluation) -> Option<Candidate> {
        let request = self.repo().request_get(&eval.request_id)?;
        let tickets = self
            .repo()
            .ticket_list()
            .into_iter()
            .filter(|ticket| request.project.is_none() || ticket.project == request.project)
            .collect();
        Some(Candidate {
            eval_id: eval.id.clone(),
            request,
            strength: eval.strength,
            policy_version: eval.policy_version.clone(),
            tickets,
        })
    }

    pub(crate) fn candidate_inner(&self, request_id: &str) -> Option<Candidate> {
        let eval = self.repo().eval_by_request(request_id)?;
        self.candidate_of(&eval)
    }

    pub(crate) fn candidate_delivered_inner(&self, eval_id: &str) -> Result<(), UserError> {
        let Some(mut eval) = self.repo().eval_get(eval_id) else {
            return Ok(());
        };
        if eval.delivered.is_some() {
            return Ok(());
        }
        let now = self.now();
        eval.delivered = Some(now.clone());
        eval.deadline = clock::plus_ms(&now, self.policy().eval_timeout_ms());
        self.repo().eval_save(&eval);
        Ok(())
    }

    pub(crate) fn evaluate_due_inner(&self) -> Result<(), UserError> {
        let paused = self.policy().paused();
        let now = self.now();
        for mut eval in self.repo().eval_list() {
            if paused {
                if eval.paused_at.is_none() {
                    eval.paused_at = Some(now.clone());
                    self.repo().eval_save(&eval);
                }
                continue;
            }
            if let Some(paused_at) = eval.paused_at.take() {
                let shift = clock::millis_between(&paused_at, &now);
                eval.deadline = clock::plus_ms(&eval.deadline, shift);
                self.repo().eval_save(&eval);
            }
            // An undelivered candidate never expires: nobody has seen it yet.
            if eval.delivered.is_none() || eval.deadline > now {
                continue;
            }
            eval.expired += 1;
            if eval.expired <= REQUEUES {
                eval.delivered = None;
                eval.started_at = now.clone();
                eval.deadline = clock::plus_ms(&now, self.policy().eval_timeout_ms());
                self.repo().eval_save(&eval);
                self.emit(DomainEvent::RequestRequeued {
                    request_id: eval.request_id.clone(),
                });
                if let Some(candidate) = self.candidate_of(&eval) {
                    self.assessor().request(&candidate);
                }
            } else {
                self.record_decision(
                    &eval,
                    DecisionDraft {
                        kind: DecisionKind::Unavailable,
                        reason: vec![Reason::code("timeout")],
                        ticket_id: None,
                        mandate: None,
                        audit: None,
                        id: None,
                    },
                )?;
            }
        }
        if paused {
            return Ok(());
        }
        for ticket in self.repo().ticket_active() {
            if !ticket.live() && !ticket.undelivered.is_empty() {
                self.wake(&ticket.id)?;
            }
        }
        for request in self.pending_requests() {
            self.start_eval(&request)?;
        }
        Ok(())
    }

    /// Records a decision, closes the evaluation and updates the request's disposition.
    fn record_decision(&self, eval: &Evaluation, draft: DecisionDraft) -> Result<Decision, UserError> {
        let now = self.now();
        let kind = draft.kind;
        let decision = Decision {
            id: draft.id.unwrap_or_else(|| key(&["decision", &eval.id, &now])),
            eval_id: eval.id.clone(),
            request_id: eval.request_id.clone(),
            policy_version: self.policy().version(),
            at: now.clone(),
            kind,
            reason: draft.reason,
            strength: eval.strength,
            ticket_id: draft.ticket_id,
            mandate: draft.mandate,
            audit: draft.audit,
            dismissed: false,
        };
        self.repo().decision_save(&decision);
        let mut disposition = self.repo().disposition_get(&eval.request_id).unwrap_or(RequestDisposition {
            at: now,
            ..Default::default()
        });
        disposition.eval_id = None;
        disposition.decision_id = Some(decision.id.clone());
        disposition.requeued = false;
        self.repo().disposition_save(&eval.request_id, &disposition);
        if self.repo().eval_get(&eval.id).is_some() {
            self.repo().eval_delete(&eval.id);
        }
        self.assessor().release(&eval.id);
        self.emit(DomainEvent::DecisionRecorded {
            decision_id: decision.id.clone(),
            kind,
            request_id: eval.request_id.clone(),
        });
        Ok(decision)
    }

    pub(crate) fn decide_inner(&self, request_id: &str, input: DecisionInput) -> Result<Decision, UserError> {
        let request = self.require_request(request_id)?;
        // A request that already started a ticket returns that ticket's decision.
        if let Some(started) = self.started_ticket(request_id) {
            return Ok(started);
        }
        let held = self.repo().eval_by_request(request_id);
        if let (Some(named), Some(current)) = (&input.eval_id, &held) {
            if named != &current.id {
                return Err(UserError::new(ErrorCode::EvalFinished).with("eval", named));
            }
        }
        if let (Some(named), None) = (&input.eval_id, &held) {
            return Err(UserError::new(ErrorCode::EvalFinished).with("eval", named));
        }
        let eval = match held {
            Some(eval) => eval,
            // A direct evaluation of a request no evaluation holds, such as an explicit request the superagent routes.
            None => match self.policy().gate(&request) {
                GateResult::Inadmissible { reason } => return Err(UserError::new(ErrorCode::NotAdmissible).with("reason", &reason)),
                GateResult::Admissible { strength, basis } => {
                    let now = self.now();
                    Evaluation {
                        id: key(&["eval", &request.id, "direct", &now]),
                        request_id: request.id.clone(),
                        strength,
                        basis,
                        policy_version: self.policy().version(),
                        started_at: now.clone(),
                        delivered: Some(now.clone()),
                        deadline: now,
                        paused_at: None,
                        expired: 0,
                    }
                }
            },
        };
        match input.outcome {
            Outcome::Ignore { reason, audit } => self.record_decision(
                &eval,
                DecisionDraft {
                    kind: DecisionKind::Ignore,
                    reason: vec![reason],
                    ticket_id: None,
                    mandate: None,
                    audit,
                    id: None,
                },
            ),
            Outcome::Unavailable { reason, audit } => self.record_decision(
                &eval,
                DecisionDraft {
                    kind: DecisionKind::Unavailable,
                    reason: vec![reason],
                    ticket_id: None,
                    mandate: None,
                    audit,
                    id: None,
                },
            ),
            Outcome::Stop { ticket_id, reason } => {
                let live = self.repo().ticket_get(&ticket_id).map(|t| t.live()).unwrap_or(false);
                if !live {
                    return self.record_decision(
                        &eval,
                        DecisionDraft {
                            kind: DecisionKind::Ignore,
                            reason: vec![Reason::code("stopUnknown")],
                            ticket_id: Some(ticket_id),
                            mandate: None,
                            audit: None,
                            id: None,
                        },
                    );
                }
                if eval.strength != Strength::Explicit {
                    return self.record_decision(
                        &eval,
                        DecisionDraft {
                            kind: DecisionKind::Ignore,
                            reason: vec![Reason::code("stopNotExplicit")],
                            ticket_id: Some(ticket_id),
                            mandate: None,
                            audit: None,
                            id: None,
                        },
                    );
                }
                self.end_ticket(
                    &ticket_id,
                    TicketEndBy::Human,
                    TicketOutcome::Stopped,
                    None,
                    Some(crate::core::reasons::reason_text(std::slice::from_ref(&reason))),
                )?;
                self.record_decision(
                    &eval,
                    DecisionDraft {
                        kind: DecisionKind::Stop,
                        reason: vec![reason],
                        ticket_id: Some(ticket_id),
                        mandate: None,
                        audit: None,
                        id: None,
                    },
                )
            }
            Outcome::Attach { ticket_id, reason, note } => {
                self.attachable(&ticket_id)?;
                self.attach(&ticket_id, &request, note)?;
                self.wake(&ticket_id)?;
                let decision = self.record_decision(
                    &eval,
                    DecisionDraft {
                        kind: DecisionKind::Attach,
                        reason: vec![reason],
                        ticket_id: Some(ticket_id.clone()),
                        mandate: None,
                        audit: None,
                        id: None,
                    },
                )?;
                let mut disposition = self.repo().disposition_get(request_id).unwrap_or_default();
                if !disposition.attached.contains(&ticket_id) {
                    disposition.attached.push(ticket_id);
                }
                self.repo().disposition_save(request_id, &disposition);
                Ok(decision)
            }
            Outcome::Accept {
                title,
                scope,
                reason,
                rules,
                workflow,
                mode,
                name,
                note,
                audit,
            } => {
                if eval.strength == Strength::Inferred && !self.policy().admits_inferred(&rules) {
                    return Err(UserError::new(ErrorCode::NotExplicit));
                }
                let basis = match &eval.basis {
                    MandateBasis::Explicit { .. } => MandateBasis::Explicit {
                        workflow: workflow.clone().or(request.workflow.clone()),
                    },
                    MandateBasis::Assessed { .. } => MandateBasis::Assessed { rules: rules.clone() },
                };
                let mandate = Mandate {
                    strength: eval.strength,
                    policy_version: self.policy().version(),
                    basis,
                    actor: request.actor.clone(),
                    scope,
                    mode: self.policy().mode_for(&request, mode.or(request.mode)),
                };
                let decision_id = key(&["decision", &eval.id, &self.now()]);
                let workflow = workflow.or(request.workflow.clone());
                let plan = self.policy().plan_for(&request, workflow.as_deref(), &title);
                let new = NewTicket {
                    decision_id: decision_id.clone(),
                    title,
                    workflow,
                    name,
                    note,
                    plan,
                };
                let ticket = self.create_ticket(&request, mandate.clone(), new)?;
                let decision = self.record_decision(
                    &eval,
                    DecisionDraft {
                        kind: DecisionKind::Accept,
                        reason: vec![reason],
                        ticket_id: Some(ticket.id.clone()),
                        mandate: Some(mandate),
                        audit,
                        id: Some(decision_id),
                    },
                )?;
                self.emit(DomainEvent::TicketCreated {
                    ticket_id: ticket.id,
                    decision_id: decision.id.clone(),
                    strength: eval.strength,
                });
                Ok(decision)
            }
        }
    }

    /// The accept decision of a request that started a ticket, if any.
    fn started_ticket(&self, request_id: &str) -> Option<Decision> {
        let disposition = self.repo().disposition_get(request_id)?;
        let decision = self.repo().decision_get(disposition.decision_id.as_deref()?)?;
        (decision.kind == DecisionKind::Accept && decision.ticket_id.is_some()).then_some(decision)
    }

    pub(crate) fn reevaluate_inner(&self, request_id: &str) -> Result<bool, UserError> {
        let request = self.require_request(request_id)?;
        if self.repo().eval_by_request(request_id).is_some() {
            return Ok(false);
        }
        let mut disposition = self.repo().disposition_get(request_id).unwrap_or_default();
        disposition.requeued = true;
        disposition.filtered = None;
        disposition.attached.clear();
        self.repo().disposition_save(request_id, &disposition);
        if !self.policy().paused() {
            self.start_eval(&request)?;
        }
        Ok(true)
    }

    pub(crate) fn dismiss_inner(&self, decision_id: &str) -> Result<Decision, UserError> {
        let mut decision = self
            .repo()
            .decision_get(decision_id)
            .ok_or_else(|| UserError::new(ErrorCode::UnknownDecision).with("decision", decision_id))?;
        if decision.kind != DecisionKind::Unavailable {
            return Err(UserError::new(ErrorCode::UnknownDecision).with("decision", decision_id));
        }
        decision.dismissed = true;
        self.repo().decision_save(&decision);
        Ok(decision)
    }

    /// Sends a request that waited on a ticket back to evaluation.
    pub(crate) fn let_go(&self, request_id: &str) -> Result<(), UserError> {
        if self.repo().request_get(request_id).is_none() {
            return Ok(());
        }
        let mut disposition = self.repo().disposition_get(request_id).unwrap_or_default();
        disposition.requeued = true;
        disposition.attached.clear();
        self.repo().disposition_save(request_id, &disposition);
        self.emit(DomainEvent::RequestRequeued {
            request_id: request_id.to_string(),
        });
        Ok(())
    }

    /// Evaluations in progress start again under a new profile version.
    pub(crate) fn evals_policy_changed(&self) -> Result<(), UserError> {
        let now = self.now();
        for mut eval in self.repo().eval_list() {
            if eval.policy_version == self.policy().version() {
                continue;
            }
            eval.policy_version = self.policy().version();
            eval.started_at = now.clone();
            eval.delivered = None;
            eval.deadline = clock::plus_ms(&now, self.policy().eval_timeout_ms());
            self.repo().eval_save(&eval);
            if let Some(candidate) = self.candidate_of(&eval) {
                self.assessor().request(&candidate);
            }
        }
        Ok(())
    }
}
