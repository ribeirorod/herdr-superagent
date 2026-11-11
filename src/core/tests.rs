//! The engine's behaviour, with DeepSeek's dsh-superagent core tests as the
//! specification for the parts that were kept (PLAN §5.3).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::core::engine::{Engine, EngineOptions, Question, ReceiveResult, WorkerReport};
use crate::core::errors::{ErrorCode, UserError};
use crate::core::model::*;
use crate::core::ports::*;
use crate::core::reasons::Reason;
use crate::core::stage::{ticket_stage, PersonAction, TicketStage};
use crate::core::tickets::STATUS_REQUEST;
use crate::store::MemoryRepositories;

fn request(id: &str) -> Request {
    Request {
        id: id.to_string(),
        source: "you".into(),
        actor: "alice".into(),
        human: true,
        at: format!("2026-09-23T00:00:{:0>2}Z", id),
        text: format!("message {id}"),
        workflow: None,
        mode: None,
        project: Some("repo".into()),
        meta: serde_json::Value::Null,
    }
}

fn bot(id: &str) -> Request {
    Request {
        human: false,
        actor: "ci".into(),
        source: "ci".into(),
        ..request(id)
    }
}

fn guest(id: &str) -> Request {
    Request {
        actor: "guest".into(),
        ..request(id)
    }
}

#[derive(Default)]
struct TestPolicy {
    version: RefCell<String>,
    paused: Cell<bool>,
}

impl Policy for TestPolicy {
    fn version(&self) -> String {
        self.version.borrow().clone()
    }
    fn paused(&self) -> bool {
        self.paused.get()
    }
    fn gate(&self, request: &Request) -> GateResult {
        if !request.human {
            return GateResult::Inadmissible { reason: "notHuman".into() };
        }
        if request.actor == "alice" {
            GateResult::Admissible {
                strength: Strength::Explicit,
                basis: MandateBasis::Explicit {
                    workflow: request.workflow.clone(),
                },
            }
        } else if request.actor == "guest" {
            GateResult::Admissible {
                strength: Strength::Inferred,
                basis: MandateBasis::Assessed { rules: vec![] },
            }
        } else {
            GateResult::Inadmissible {
                reason: "notListed".into(),
            }
        }
    }
    fn revalidate(&self, _mandate: &Mandate) -> bool {
        self.version.borrow().as_str() != "revoked"
    }
    fn admits_inferred(&self, rules: &[String]) -> bool {
        !rules.is_empty()
    }
    fn eval_timeout_ms(&self) -> i64 {
        60_000
    }
    fn mode_for(&self, _request: &Request, proposed: Option<Mode>) -> Mode {
        proposed.unwrap_or(Mode::Checkpoint)
    }
}

#[derive(Default)]
struct Recorder {
    candidates: RefCell<Vec<Candidate>>,
    released: RefCell<Vec<String>>,
    launches: RefCell<Vec<(String, String)>>,
    deliveries: RefCell<Vec<String>>,
    stops: RefCell<Vec<(String, String, String)>>,
    events: RefCell<Vec<DomainEvent>>,
}

impl Assessor for Recorder {
    fn request(&self, candidate: &Candidate) {
        self.candidates.borrow_mut().push(candidate.clone());
    }
    fn release(&self, eval_id: &str) {
        self.released.borrow_mut().push(eval_id.to_string());
    }
}

impl Executor for Recorder {
    fn start(&self, ticket: &Ticket, assignment: &Assignment) {
        self.launches.borrow_mut().push((ticket.id.clone(), assignment.id.clone()));
    }
    fn deliver(&self, ticket: &Ticket, _assignment: &Assignment) {
        self.deliveries.borrow_mut().push(ticket.id.clone());
    }
    fn stop(&self, ticket: &Ticket, assignment: &Assignment, reason: &str) {
        self.stops
            .borrow_mut()
            .push((ticket.id.clone(), assignment.id.clone(), reason.to_string()));
    }
}

struct Harness {
    repo: Rc<MemoryRepositories>,
    policy: Rc<TestPolicy>,
    recorder: Rc<Recorder>,
    clock: Rc<Cell<i64>>,
    engine: Engine,
}

fn harness() -> Harness {
    let repo = Rc::new(MemoryRepositories::new());
    let policy = Rc::new(TestPolicy {
        version: RefCell::new("v1".into()),
        paused: Cell::new(false),
    });
    let recorder = Rc::new(Recorder::default());
    let clock = Rc::new(Cell::new(0i64));
    let now = {
        let clock = clock.clone();
        Rc::new(move || format!("2026-09-23T00:{:0>2}:{:0>2}.000Z", clock.get() / 60, clock.get() % 60))
    };
    let emit = {
        let recorder = recorder.clone();
        Rc::new(move |event: DomainEvent| recorder.events.borrow_mut().push(event))
    };
    let engine = Engine::new(EngineOptions {
        repo: repo.clone(),
        policy: policy.clone(),
        assessor: recorder.clone(),
        executor: recorder.clone(),
        now,
        emit: Some(emit),
    });
    Harness {
        repo,
        policy,
        recorder,
        clock,
        engine,
    }
}

impl Harness {
    fn tick(&self, seconds: i64) {
        self.clock.set(self.clock.get() + seconds);
    }
    fn ticket(&self, id: &str) -> Ticket {
        self.repo.ticket_get(id).expect("ticket")
    }
    fn running(&self, id: &str) -> Assignment {
        self.ticket(id).running_assignment().cloned().expect("running assignment")
    }
    /// Simulate the executor: start queued assignments and deliver waiting inputs.
    fn settle(&self) -> Vec<String> {
        let mut delivered = Vec::new();
        loop {
            let launch = self.recorder.launches.borrow_mut().pop();
            if let Some((ticket, assignment)) = launch {
                self.engine.assignment_started(&ticket, &assignment).unwrap();
                continue;
            }
            let delivery = self.recorder.deliveries.borrow_mut().pop();
            let Some(ticket_id) = delivery else { return delivered };
            let ticket = self.ticket(&ticket_id);
            let Some(assignment) = ticket.running_assignment() else { continue };
            if !ticket.live() {
                continue;
            }
            for input in ticket.undelivered.clone() {
                delivered.push(input.clone());
                self.engine.inputs_delivered(&ticket_id, &assignment.id, &[input]).unwrap();
            }
        }
    }
    fn pump(&self) {
        self.engine.evaluate_due().unwrap();
        self.settle();
    }
    fn last_candidate(&self) -> Candidate {
        self.recorder.candidates.borrow().last().cloned().expect("candidate")
    }
    fn accept(&self, candidate: &Candidate) -> Decision {
        let outcome = Outcome::Accept {
            title: "Fix".into(),
            scope: "small".into(),
            reason: Reason::said("requested"),
            rules: vec!["rule".into()],
            workflow: None,
            mode: None,
            name: None,
            note: None,
            audit: None,
        };
        let decision = self
            .engine
            .decide(
                &candidate.request.id,
                DecisionInput {
                    eval_id: Some(candidate.eval_id.clone()),
                    outcome,
                },
            )
            .unwrap();
        self.settle();
        decision
    }
    /// Receive an explicit request, pump, accept the candidate, settle; the ticket id.
    fn accepted(&self, id: &str) -> String {
        self.engine.receive(request(id)).unwrap();
        self.pump();
        self.accept(&self.last_candidate()).ticket_id.unwrap()
    }
    /// A worker's report; a `ready` report in a reviewed mode first goes through an approved checkpoint, as a real worker would.
    fn report(&self, ticket: &str, state: ReportState, summary: &str) -> Ticket {
        if state == ReportState::Ready {
            let current = self.ticket(ticket);
            if current.mandate.mode != Mode::Autonomous && current.approved_checkpoints.is_empty() {
                if current.checkpoint.is_none() {
                    self.engine.checkpoint(ticket, "plan", "plan").unwrap();
                }
                self.engine.approve(ticket, None).unwrap();
            }
        }
        self.engine
            .report(
                ticket,
                WorkerReport {
                    state,
                    summary: summary.into(),
                    question: Question::Keep,
                    assignment_id: None,
                },
            )
            .unwrap()
    }
    fn report_q(&self, ticket: &str, state: ReportState, question: Question) -> Result<Ticket, UserError> {
        self.engine.report(
            ticket,
            WorkerReport {
                state,
                summary: "x".into(),
                question,
                assignment_id: None,
            },
        )
    }
    fn event_types(&self) -> Vec<String> {
        self.recorder
            .events
            .borrow()
            .iter()
            .map(|e| serde_json::to_value(e).unwrap()["type"].as_str().unwrap().to_string())
            .collect()
    }
}

// ---- intake ----

#[test]
fn stores_a_request_once_and_records_gate_filtering() {
    let h = harness();
    assert_eq!(h.engine.receive(request("1")).unwrap(), ReceiveResult::Stored);
    assert_eq!(h.engine.receive(request("1")).unwrap(), ReceiveResult::Duplicate);
    assert_eq!(h.engine.receive(bot("2")).unwrap(), ReceiveResult::Filtered);
    assert_eq!(h.repo.disposition_get("2").unwrap().filtered.unwrap().code, "notHuman");
    assert!(h.repo.eval_by_request("2").is_none());
    assert!(h.repo.eval_by_request("1").is_some());
    assert_eq!(h.recorder.candidates.borrow().len(), 1);
}

#[test]
fn normalizes_request_timestamps() {
    let h = harness();
    h.engine.receive(request("5")).unwrap();
    assert_eq!(h.repo.request_get("5").unwrap().at, "2026-09-23T00:00:05.000Z");
}

// ---- evaluation ----

#[test]
fn accept_creates_a_ticket_with_a_mandate_and_starts_an_assignment() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let ticket = h.ticket(&ticket_id);
    assert_eq!(ticket.mandate.strength, Strength::Explicit);
    assert_eq!(ticket.mandate.mode, Mode::Checkpoint);
    assert_eq!(ticket.mandate.policy_version, "v1");
    assert_eq!(ticket.assignments.len(), 1);
    assert_eq!(ticket.assignments[0].state, AssignmentState::Running);
    assert_eq!(ticket_stage(&ticket), TicketStage::Working { queued: false });
    let decision = h
        .repo
        .decision_get(&ticket.decision_id)
        .expect("the ticket's decision is stored under its id");
    assert_eq!(decision.kind, DecisionKind::Accept);
    assert_eq!(h.repo.decision_list().len(), 1);
    assert!(h.repo.eval_by_request("1").is_none());
    assert_eq!(h.recorder.released.borrow().len(), 1);
    assert!(h.event_types().contains(&"ticket_created".to_string()));
}

#[test]
fn records_an_accept_with_its_ticket_together_or_none_of_them() {
    let h = harness();
    h.engine.receive(request("1")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    // An inferred candidate accepted without matched rules fails; nothing is recorded.
    h.engine.receive(guest("2")).unwrap();
    h.pump();
    let inferred = h.last_candidate();
    let outcome = Outcome::Accept {
        title: "x".into(),
        scope: "s".into(),
        reason: Reason::said("r"),
        rules: vec![],
        workflow: None,
        mode: None,
        name: None,
        note: None,
        audit: None,
    };
    let error = h
        .engine
        .decide(
            "2",
            DecisionInput {
                eval_id: Some(inferred.eval_id.clone()),
                outcome,
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotExplicit);
    assert!(h.repo.ticket_list().is_empty());
    assert!(h.repo.eval_by_request("2").is_some());
    // With rules it is accepted.
    let outcome = Outcome::Accept {
        title: "x".into(),
        scope: "s".into(),
        reason: Reason::said("r"),
        rules: vec!["r1".into()],
        workflow: None,
        mode: None,
        name: None,
        note: None,
        audit: None,
    };
    let decision = h
        .engine
        .decide(
            "2",
            DecisionInput {
                eval_id: Some(inferred.eval_id),
                outcome,
            },
        )
        .unwrap();
    assert_eq!(decision.strength, Strength::Inferred);
    assert_eq!(
        h.ticket(decision.ticket_id.as_deref().unwrap()).mandate.basis,
        MandateBasis::Assessed { rules: vec!["r1".into()] }
    );
    h.accept(&candidate);
    assert_eq!(h.repo.ticket_list().len(), 2);
}

#[test]
fn refuses_to_decide_a_stale_evaluation_and_accepts_a_request_once() {
    let h = harness();
    h.engine.receive(request("1")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    let error = h
        .engine
        .decide(
            "1",
            DecisionInput {
                eval_id: Some("old".into()),
                outcome: Outcome::Ignore {
                    reason: Reason::said("no"),
                    audit: None,
                },
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::EvalFinished);
    let decision = h.accept(&candidate);
    // Deciding again returns the ticket's decision instead of a second ticket.
    let again = h
        .engine
        .decide(
            "1",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Ignore {
                    reason: Reason::said("no"),
                    audit: None,
                },
            },
        )
        .unwrap();
    assert_eq!(again.id, decision.id);
    assert_eq!(h.repo.ticket_list().len(), 1);
    // Even the finished evaluation's id, replayed, returns that ticket's decision.
    let replayed = h
        .engine
        .decide(
            "1",
            DecisionInput {
                eval_id: Some(candidate.eval_id),
                outcome: Outcome::Ignore {
                    reason: Reason::said("no"),
                    audit: None,
                },
            },
        )
        .unwrap();
    assert_eq!(replayed.id, decision.id);
    // A finished evaluation named for a request that started no ticket is refused.
    h.engine.receive(request("2")).unwrap();
    h.pump();
    let second = h.last_candidate();
    h.engine
        .decide(
            "2",
            DecisionInput {
                eval_id: Some(second.eval_id.clone()),
                outcome: Outcome::Ignore {
                    reason: Reason::said("no"),
                    audit: None,
                },
            },
        )
        .unwrap();
    let error = h
        .engine
        .decide(
            "2",
            DecisionInput {
                eval_id: Some(second.eval_id),
                outcome: Outcome::Ignore {
                    reason: Reason::said("no"),
                    audit: None,
                },
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::EvalFinished);
}

#[test]
fn counts_the_deadline_from_delivery_requeues_twice_then_records_unavailable() {
    let h = harness();
    h.engine.receive(request("1")).unwrap();
    let eval = h.repo.eval_by_request("1").unwrap();
    // Undelivered: nothing expires however long it takes.
    h.tick(600);
    h.engine.evaluate_due().unwrap();
    assert_eq!(h.repo.eval_by_request("1").unwrap().expired, 0);
    h.engine.candidate_delivered(&eval.id).unwrap();
    h.tick(61);
    h.engine.evaluate_due().unwrap();
    let requeued = h.repo.eval_by_request("1").unwrap();
    assert_eq!(requeued.expired, 1);
    assert!(requeued.delivered.is_none());
    assert_eq!(h.recorder.candidates.borrow().len(), 2);
    h.engine.candidate_delivered(&eval.id).unwrap();
    h.tick(61);
    h.engine.evaluate_due().unwrap();
    assert_eq!(h.repo.eval_by_request("1").unwrap().expired, 2);
    h.engine.candidate_delivered(&eval.id).unwrap();
    h.tick(61);
    h.engine.evaluate_due().unwrap();
    assert!(h.repo.eval_by_request("1").is_none());
    let decision = h.repo.decision_list().pop().unwrap();
    assert_eq!(decision.kind, DecisionKind::Unavailable);
    assert_eq!(decision.reason[0].code, "timeout");
    // A dismissed problem no longer shows; only unavailable decisions can be dismissed.
    assert!(h.engine.dismiss(&decision.id).unwrap().dismissed);
    // The request can be sent to evaluation again by a person.
    assert!(h.engine.reevaluate("1").unwrap());
    assert!(h.repo.eval_by_request("1").is_some());
    assert!(!h.engine.reevaluate("1").unwrap());
}

#[test]
fn lets_no_evaluation_expire_while_paused_and_shifts_its_deadline() {
    let h = harness();
    h.engine.receive(request("1")).unwrap();
    let eval = h.repo.eval_by_request("1").unwrap();
    h.engine.candidate_delivered(&eval.id).unwrap();
    h.policy.paused.set(true);
    h.tick(30);
    h.engine.evaluate_due().unwrap();
    h.tick(300);
    h.engine.evaluate_due().unwrap();
    assert_eq!(h.repo.eval_by_request("1").unwrap().expired, 0);
    h.policy.paused.set(false);
    h.engine.evaluate_due().unwrap();
    let eval = h.repo.eval_by_request("1").unwrap();
    assert_eq!(eval.expired, 0);
    assert!(eval.paused_at.is_none());
    // 30 s were used before the pause; the remaining 30 s run out after the shift.
    h.tick(31);
    h.engine.evaluate_due().unwrap();
    assert_eq!(h.repo.eval_by_request("1").unwrap().expired, 1);
}

#[test]
fn does_not_start_evaluations_while_paused_and_starts_them_after() {
    let h = harness();
    h.policy.paused.set(true);
    h.engine.receive(request("1")).unwrap();
    assert!(h.repo.eval_by_request("1").is_none());
    h.engine.evaluate_due().unwrap();
    assert!(h.repo.eval_by_request("1").is_none());
    h.policy.paused.set(false);
    h.engine.evaluate_due().unwrap();
    assert!(h.repo.eval_by_request("1").is_some());
    h.engine.evaluate_due().unwrap();
    assert_eq!(h.recorder.candidates.borrow().len(), 1, "a second pass starts none again");
}

#[test]
fn decides_an_explicit_request_no_evaluation_holds_directly_and_refuses_inadmissible_ones() {
    let h = harness();
    h.policy.paused.set(true);
    h.engine.receive(request("1")).unwrap();
    h.engine.receive(bot("2")).unwrap();
    h.policy.paused.set(false);
    let outcome = Outcome::Accept {
        title: "Fix".into(),
        scope: "s".into(),
        reason: Reason::code("explicitRouted"),
        rules: vec![],
        workflow: Some("research".into()),
        mode: Some(Mode::Autonomous),
        name: None,
        note: Some("start here".into()),
        audit: None,
    };
    let decision = h.engine.decide("1", DecisionInput { eval_id: None, outcome }).unwrap();
    let ticket = h.ticket(decision.ticket_id.as_deref().unwrap());
    assert_eq!(ticket.mandate.mode, Mode::Autonomous);
    assert_eq!(
        ticket.mandate.basis,
        MandateBasis::Explicit {
            workflow: Some("research".into())
        }
    );
    assert_eq!(ticket.workflow.as_deref(), Some("research"));
    assert!(matches!(&ticket.inputs[0], TicketInput::Note { text, .. } if text == "start here"));
    assert_eq!(ticket.undelivered.len(), 1);
    let error = h
        .engine
        .decide(
            "2",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Ignore {
                    reason: Reason::said("x"),
                    audit: None,
                },
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotAdmissible);
    assert_eq!(
        h.engine
            .decide(
                "9",
                DecisionInput {
                    eval_id: None,
                    outcome: Outcome::Ignore {
                        reason: Reason::said("x"),
                        audit: None
                    }
                }
            )
            .unwrap_err()
            .code,
        ErrorCode::UnknownRequest
    );
    // A started evaluation is not started twice by the next pass.
    h.engine.evaluate_due().unwrap();
    assert!(h.repo.eval_by_request("1").is_none());
}

#[test]
fn restarts_evaluations_in_progress_under_a_new_profile_version() {
    let h = harness();
    h.engine.receive(request("1")).unwrap();
    let before = h.repo.eval_by_request("1").unwrap();
    h.engine.candidate_delivered(&before.id).unwrap();
    *h.policy.version.borrow_mut() = "v2".into();
    h.engine.policy_changed("v1", "v2", &HashMap::new()).unwrap();
    let after = h.repo.eval_by_request("1").unwrap();
    assert_eq!(after.policy_version, "v2");
    assert!(after.delivered.is_none());
    assert_eq!(h.recorder.candidates.borrow().len(), 2);
}

// ---- stop, attach and resume ----

#[test]
fn stops_a_live_ticket_only_on_an_explicit_request_and_records_how_it_ended() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.engine.receive(guest("2")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    let decision = h
        .engine
        .decide(
            "2",
            DecisionInput {
                eval_id: Some(candidate.eval_id),
                outcome: Outcome::Stop {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("humanStop"),
                },
            },
        )
        .unwrap();
    assert_eq!(decision.kind, DecisionKind::Ignore);
    assert_eq!(decision.reason[0].code, "stopNotExplicit");
    assert!(h.ticket(&ticket_id).live());
    h.engine.receive(request("3")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    let decision = h
        .engine
        .decide(
            "3",
            DecisionInput {
                eval_id: Some(candidate.eval_id),
                outcome: Outcome::Stop {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("humanStop"),
                },
            },
        )
        .unwrap();
    assert_eq!(decision.kind, DecisionKind::Stop);
    let ticket = h.ticket(&ticket_id);
    let ended = ticket.ended.clone().unwrap();
    assert_eq!((ended.by, ended.outcome), (TicketEndBy::Human, TicketOutcome::Stopped));
    assert_eq!(ended.reason.as_deref(), Some("Human asked to stop"));
    assert_eq!(ticket.outcome(), Some(TicketOutcome::Stopped));
    assert!(ticket.assignments.iter().all(|a| a.state == AssignmentState::Ended));
    assert_eq!(h.recorder.stops.borrow().len(), 1);
    // A stop on an unknown or ended ticket is recorded as ignored.
    h.engine.receive(request("4")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    let decision = h
        .engine
        .decide(
            "4",
            DecisionInput {
                eval_id: Some(candidate.eval_id),
                outcome: Outcome::Stop {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("humanStop"),
                },
            },
        )
        .unwrap();
    assert_eq!(
        (decision.kind, decision.reason[0].code.as_str()),
        (DecisionKind::Ignore, "stopUnknown")
    );
    assert_eq!(h.engine.stop_ticket("nope", None).unwrap_err().code, ErrorCode::UnknownTicket);
}

#[test]
fn attaches_a_request_to_a_ticket_with_its_note_and_refuses_a_stopped_or_revoked_one() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.engine.receive(request("2")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    let decision = h
        .engine
        .decide(
            "2",
            DecisionInput {
                eval_id: Some(candidate.eval_id),
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed").with("ticket", &ticket_id),
                    note: Some("same bug".into()),
                },
            },
        )
        .unwrap();
    assert_eq!(decision.kind, DecisionKind::Attach);
    let ticket = h.ticket(&ticket_id);
    assert!(
        matches!(&ticket.inputs[0], TicketInput::Request { request_id, human: true, note: Some(note), .. } if request_id == "2" && note == "same bug")
    );
    assert_eq!(h.repo.disposition_get("2").unwrap().attached, vec![ticket_id.clone()]);
    assert!(h.repo.eval_by_request("2").is_none());
    h.settle();
    assert!(h.ticket(&ticket_id).undelivered.is_empty());
    // A note for a request the worker already has goes out as a note of its own.
    h.engine
        .decide(
            "2",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: Some("also check tests".into()),
                },
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(h.ticket(&ticket_id).inputs.last().unwrap(), TicketInput::Note { text, .. } if text == "also check tests"));
    // A stopped ticket refuses; a ticket whose mandate the profile no longer holds too.
    h.engine.stop_ticket(&ticket_id, None).unwrap();
    h.engine.receive(request("3")).unwrap();
    h.pump();
    let error = h
        .engine
        .decide(
            "3",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::TicketStopped);
    let other = h.accepted("5");
    h.report(&other, ReportState::Ready, "done");
    *h.policy.version.borrow_mut() = "revoked".into();
    let error = h
        .engine
        .decide(
            "3",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: other.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::MandateRevoked);
    assert!(
        h.repo.eval_by_request("3").is_some(),
        "the refused attach leaves the evaluation in place"
    );
}

#[test]
fn resumes_an_ended_ticket_on_a_persons_message_under_the_same_mandate_without_a_decision() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.report(&ticket_id, ReportState::Ready, "done");
    assert_eq!(h.ticket(&ticket_id).outcome(), Some(TicketOutcome::Completed));
    let decisions = h.repo.decision_list().len();
    h.engine.receive(request("2")).unwrap();
    h.pump();
    h.engine
        .decide(
            "2",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap();
    let ticket = h.ticket(&ticket_id);
    assert!(ticket.live());
    assert_eq!(ticket.assignments.len(), 2);
    assert_eq!(ticket.assignments[1].state, AssignmentState::Pending);
    // The resumed marker goes ahead of the message.
    assert!(matches!(ticket.input(&ticket.undelivered[0]).unwrap(), TicketInput::Resumed { request_id, .. } if request_id == "2"));
    assert!(matches!(ticket.input(&ticket.undelivered[1]).unwrap(), TicketInput::Request { request_id, .. } if request_id == "2"));
    assert_eq!(h.repo.decision_list().len(), decisions + 1, "only the attach decision was recorded");
    assert!(h.event_types().contains(&"ticket_resumed".to_string()));
    h.settle();
    assert_eq!(h.ticket(&ticket_id).assignments[1].state, AssignmentState::Running);
}

#[test]
fn keeps_a_ticket_a_person_stopped_ended_and_sends_a_message_it_cannot_take_back_to_evaluation() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.engine.stop_ticket(&ticket_id, Some("enough".into())).unwrap();
    h.engine.receive(request("2")).unwrap();
    h.pump();
    assert_eq!(
        h.engine
            .decide(
                "2",
                DecisionInput {
                    eval_id: None,
                    outcome: Outcome::Attach {
                        ticket_id: ticket_id.clone(),
                        reason: Reason::code("routed"),
                        note: None
                    }
                }
            )
            .unwrap_err()
            .code,
        ErrorCode::TicketStopped
    );
    assert!(!h.ticket(&ticket_id).live());
    // A ticket that ended while a message waited for it, under a mandate the profile no longer holds, lets the message go.
    let other = h.accepted("3");
    h.report(&other, ReportState::Ready, "done");
    h.engine.receive(request("4")).unwrap();
    h.pump();
    h.policy.paused.set(true);
    h.engine
        .decide(
            "4",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: other.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap();
    assert!(!h.ticket(&other).live(), "paused: it resumes on the next pass");
    h.policy.paused.set(false);
    *h.policy.version.borrow_mut() = "revoked".into();
    h.engine.evaluate_due().unwrap();
    assert!(!h.ticket(&other).live());
    assert!(h.ticket(&other).undelivered.is_empty());
    assert!(h.repo.disposition_get("4").unwrap().requeued);
    *h.policy.version.borrow_mut() = "v3".into();
    h.engine.evaluate_due().unwrap();
    assert!(h.repo.eval_by_request("4").is_some(), "the let-go message is evaluated again");
}

#[test]
fn resumes_on_the_next_pass_a_ticket_a_message_reached_while_paused() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.report(&ticket_id, ReportState::Dropped, "nothing to do");
    h.engine.receive(request("2")).unwrap();
    h.pump();
    h.policy.paused.set(true);
    h.engine
        .decide(
            "2",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap();
    assert!(!h.ticket(&ticket_id).live());
    h.policy.paused.set(false);
    h.pump();
    assert!(h.ticket(&ticket_id).live());
    h.pump();
    assert_eq!(h.ticket(&ticket_id).assignments.len(), 2, "resumes once however often it pumps");
}

// ---- inputs and delivery ----

#[test]
fn keeps_inputs_undelivered_until_an_assignment_runs_and_asks_to_launch_a_failed_launch_again() {
    let h = harness();
    h.engine.receive(request("1")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    let outcome = Outcome::Accept {
        title: "Fix".into(),
        scope: "s".into(),
        reason: Reason::said("r"),
        rules: vec![],
        workflow: None,
        mode: None,
        name: None,
        note: None,
        audit: None,
    };
    let ticket_id = h
        .engine
        .decide(
            "1",
            DecisionInput {
                eval_id: Some(candidate.eval_id),
                outcome,
            },
        )
        .unwrap()
        .ticket_id
        .unwrap();
    // The launch is pending; a note waits.
    h.engine.note(&ticket_id, "context").unwrap();
    assert_eq!(h.ticket(&ticket_id).undelivered.len(), 1);
    assert_eq!(
        h.recorder.launches.borrow().len(),
        2,
        "each input asks to launch the pending assignment again"
    );
    assert!(h.recorder.deliveries.borrow().is_empty());
    h.settle();
    let ticket = h.ticket(&ticket_id);
    assert!(ticket.undelivered.is_empty());
    assert!(ticket.running_assignment().unwrap().delivered_at.is_some());
}

#[test]
fn takes_delivery_reports_only_from_the_assignment_that_runs_the_open_ticket() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let first = h.running(&ticket_id).id;
    h.engine.note(&ticket_id, "a").unwrap();
    let input = h.ticket(&ticket_id).undelivered[0].clone();
    h.engine
        .inputs_delivered(&ticket_id, "someone-else", std::slice::from_ref(&input))
        .unwrap();
    assert_eq!(h.ticket(&ticket_id).undelivered, vec![input.clone()]);
    h.engine.delivery_failed(&ticket_id, &first, "pane gone").unwrap();
    assert_eq!(
        ticket_stage(&h.ticket(&ticket_id)),
        TicketStage::Person {
            action: PersonAction::Delivery
        }
    );
    h.engine.inputs_delivered(&ticket_id, &first, &[input]).unwrap();
    let ticket = h.ticket(&ticket_id);
    assert!(ticket.undelivered.is_empty());
    assert!(ticket.delivery.is_none(), "a delivery clears the failure");
    h.engine.turn_ended(&ticket_id, &first).unwrap();
    assert!(h.ticket(&ticket_id).running_assignment().unwrap().idle_at.is_some());
    h.engine.turn_ended(&ticket_id, "someone-else").unwrap();
    assert_eq!(h.engine.turn_ended("nope", &first).unwrap_err().code, ErrorCode::UnknownTicket);
}

#[test]
fn holds_back_automatic_input_from_a_blocked_worker_until_a_person_writes() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.engine.note(&ticket_id, "bot 2").unwrap();
    h.report(&ticket_id, ReportState::Blocked, "needs a decision");
    assert_eq!(
        ticket_stage(&h.ticket(&ticket_id)),
        TicketStage::Person {
            action: PersonAction::Blocked
        }
    );
    h.engine.note(&ticket_id, "bot 3").unwrap();
    let delivered = h.settle();
    assert!(delivered.is_empty());
    let ticket = h.ticket(&ticket_id);
    assert_eq!(ticket.inputs.len(), 2);
    assert!(ticket.undelivered.is_empty(), "held inputs wait off the queue");
    h.tick(1);
    h.engine.receive(request("4")).unwrap();
    h.pump();
    h.engine
        .decide(
            "4",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap();
    assert!(!h.ticket(&ticket_id).waits_for_person());
    assert_eq!(ticket_stage(&h.ticket(&ticket_id)), TicketStage::Working { queued: false });
    h.engine.note(&ticket_id, "bot 5").unwrap();
    let delivered = h.settle();
    assert_eq!(delivered.len(), 2, "the person's message and the later note go out");
}

#[test]
fn stops_waiting_for_a_person_on_a_message_a_better_report_or_a_retry() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.report(&ticket_id, ReportState::Failed, "broke");
    assert!(h.ticket(&ticket_id).waits_for_person());
    assert_eq!(
        ticket_stage(&h.ticket(&ticket_id)),
        TicketStage::Person {
            action: PersonAction::Failed
        }
    );
    h.tick(1);
    h.report(&ticket_id, ReportState::Progress, "found a way");
    assert!(!h.ticket(&ticket_id).waits_for_person());
    h.tick(1);
    h.report(&ticket_id, ReportState::Blocked, "needs access");
    assert!(h.ticket(&ticket_id).waits_for_person());
    let retried = h.engine.retry_ticket(&ticket_id).unwrap();
    assert!(!retried.waits_for_person());
    assert_eq!(retried.assignments[0].ended_reason.as_deref(), Some("retried"));
    assert_eq!(retried.assignments[1].state, AssignmentState::Pending);
    assert_eq!(h.recorder.stops.borrow().last().unwrap().2, "retried");
    assert_eq!(h.recorder.launches.borrow().last().unwrap().1, retried.assignments[1].id);
}

#[test]
fn retries_only_a_stalled_ticket_and_asks_only_a_waiting_worker_for_its_status() {
    let h = harness();
    let ticket_id = h.accepted("1");
    assert_eq!(h.engine.retry_ticket(&ticket_id).unwrap_err().code, ErrorCode::NotRetryable);
    assert!(!h.engine.request_status(&ticket_id).unwrap());
    assert!(h.ticket(&ticket_id).inputs.is_empty());
    h.report(&ticket_id, ReportState::Waiting, "idle");
    assert_eq!(ticket_stage(&h.ticket(&ticket_id)), TicketStage::Idle);
    assert!(h.engine.request_status(&ticket_id).unwrap());
    assert!(matches!(&h.ticket(&ticket_id).inputs[0], TicketInput::Text { text, .. } if text == STATUS_REQUEST));
    h.engine.stop_ticket(&ticket_id, None).unwrap();
    assert_eq!(h.engine.retry_ticket(&ticket_id).unwrap_err().code, ErrorCode::NotRetryable);
    // A failed delivery makes a ticket retryable, and the new assignment takes the input.
    let other = h.accepted("2");
    let running = h.running(&other).id;
    h.engine.note(&other, "ctx").unwrap();
    h.engine.delivery_failed(&other, &running, "no pane").unwrap();
    let retried = h.engine.retry_ticket(&other).unwrap();
    assert!(retried.delivery.is_none());
    assert_eq!(retried.undelivered.len(), 1);
}

#[test]
fn hands_inputs_an_ended_assignment_got_after_its_last_report_to_the_next_one() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.report(&ticket_id, ReportState::Progress, "working");
    h.tick(1);
    h.engine.note(&ticket_id, "late context").unwrap();
    h.settle();
    assert!(h.ticket(&ticket_id).undelivered.is_empty());
    h.tick(1);
    h.report(&ticket_id, ReportState::Failed, "crashed");
    // The failed report is after the note, so the note was taken: a retry does not repeat it.
    let retried = h.engine.retry_ticket(&ticket_id).unwrap();
    assert!(retried.undelivered.is_empty());
    h.settle();
    h.tick(1);
    h.engine.receive(request("2")).unwrap();
    h.pump();
    h.engine
        .decide(
            "2",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap();
    h.settle();
    let running = h.running(&ticket_id);
    assert!(running.delivered_at.is_some());
    // Interrupted while busy with the message: the restart hands the message to the new assignment.
    h.engine.assignment_interrupted(&ticket_id, &running.id).unwrap();
    let ticket = h.ticket(&ticket_id);
    assert_eq!(ticket.assignments.len(), 3);
    assert_eq!(ticket.undelivered.len(), 2);
    assert!(matches!(ticket.input(&ticket.undelivered[0]).unwrap(), TicketInput::Text { text, .. } if text.contains("restarted")));
    assert!(matches!(ticket.input(&ticket.undelivered[1]).unwrap(), TicketInput::Request { request_id, .. } if request_id == "2"));
}

// ---- interrupted assignments ----

#[test]
fn restarts_an_assignment_interrupted_mid_work_with_a_note_and_hands_a_waiting_one_what_waits() {
    let h = harness();
    let interrupted = h.accepted("1");
    let first = h.running(&interrupted).id;
    let waiting = h.accepted("2");
    h.report(&waiting, ReportState::Waiting, "done for now");
    h.engine.note(&waiting, "ctx").unwrap();
    h.recorder.launches.borrow_mut().clear();
    h.recorder.deliveries.borrow_mut().clear();
    for id in [&interrupted, &waiting] {
        let running = h.running(id).id;
        h.engine.assignment_interrupted(id, &running).unwrap();
    }
    // A fact about an assignment that no longer runs is dropped.
    h.engine.assignment_interrupted(&interrupted, &first).unwrap();
    let ticket = h.ticket(&interrupted);
    assert_eq!(ticket.assignments[0].state, AssignmentState::Ended);
    assert_eq!(ticket.assignments[0].ended_reason.as_deref(), Some("hostRestarted"));
    assert_eq!(ticket.assignments[1].state, AssignmentState::Pending);
    assert!(matches!(&ticket.inputs[0], TicketInput::Text { text, .. } if text.contains("restarted")));
    assert_eq!(ticket.undelivered, vec![ticket.inputs[0].id().to_string()]);
    assert_eq!(
        h.recorder.launches.borrow().as_slice(),
        &[(interrupted.clone(), ticket.assignments[1].id.clone())]
    );
    assert_eq!(h.ticket(&waiting).assignments.len(), 1, "a worker that waits keeps its session");
    assert_eq!(h.recorder.deliveries.borrow().as_slice(), std::slice::from_ref(&waiting));
}

#[test]
fn restarts_a_worker_that_reported_but_then_got_a_persons_message() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.report(&ticket_id, ReportState::Waiting, "done for now");
    h.tick(1);
    h.engine.receive(request("2")).unwrap();
    h.pump();
    h.engine
        .decide(
            "2",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap();
    h.settle();
    let running = h.running(&ticket_id);
    assert!(running.delivered_at.as_deref() > running.report.as_ref().map(|r| r.at.as_str()));
    h.engine.assignment_interrupted(&ticket_id, &running.id).unwrap();
    let states: Vec<AssignmentState> = h.ticket(&ticket_id).assignments.iter().map(|a| a.state).collect();
    assert_eq!(states, vec![AssignmentState::Ended, AssignmentState::Pending]);
}

// ---- worker reports ----

#[test]
fn asks_keeps_replaces_and_clears_the_question_for_a_person() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let asked = h
        .report_q(&ticket_id, ReportState::Progress, Question::Ask("Which color?".into()))
        .unwrap();
    assert_eq!(asked.attention.as_ref().unwrap().question, "Which color?");
    assert_eq!(asked.attention.as_ref().unwrap().request_id.as_deref(), Some("1"));
    assert_eq!(
        ticket_stage(&asked),
        TicketStage::Person {
            action: PersonAction::Answer
        }
    );
    let kept = h.report_q(&ticket_id, ReportState::Blocked, Question::Keep).unwrap();
    assert_eq!(kept.attention, asked.attention);
    h.tick(1);
    let same = h
        .report_q(&ticket_id, ReportState::Progress, Question::Ask("Which color?".into()))
        .unwrap();
    assert_eq!(same.attention, asked.attention, "repeating the same text keeps the question");
    let replaced = h
        .report_q(&ticket_id, ReportState::Progress, Question::Ask("Which size?".into()))
        .unwrap();
    assert_eq!(replaced.attention.as_ref().unwrap().question, "Which size?");
    assert_ne!(replaced.attention.as_ref().unwrap().id, asked.attention.as_ref().unwrap().id);
    let cleared = h.report_q(&ticket_id, ReportState::Progress, Question::Clear).unwrap();
    assert!(cleared.attention.is_none());
    assert_eq!(Question::from_option(Some("".into())), Question::Clear);
    assert_eq!(Question::from_option(None), Question::Keep);
}

#[test]
fn ends_the_ticket_on_a_final_report_and_clears_its_question() {
    let outcome = |state: ReportState| {
        let h = harness();
        let ticket_id = h.accepted("1");
        h.report_q(&ticket_id, ReportState::Progress, Question::Ask("Merge it?".into()))
            .unwrap();
        h.report(&ticket_id, state, "done");
        h.ticket(&ticket_id)
    };
    let dropped = outcome(ReportState::Dropped);
    assert_eq!(
        dropped.ended.as_ref().map(|e| (e.by, e.outcome)),
        Some((TicketEndBy::Executor, TicketOutcome::Dropped))
    );
    assert!(dropped.ended.as_ref().unwrap().reason.is_none());
    let ready = outcome(ReportState::Ready);
    assert_eq!(ready.outcome(), Some(TicketOutcome::Completed));
    assert!(ready.attention.is_none());
    assert_eq!(ticket_stage(&ready), TicketStage::Ended);
    let failed = outcome(ReportState::Failed);
    assert!(failed.live());
    assert_eq!(failed.attention.as_ref().unwrap().question, "Merge it?");
}

#[test]
fn refuses_a_question_with_a_result_and_a_report_on_an_ended_ticket() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let error = h.report_q(&ticket_id, ReportState::Ready, Question::Ask("Q?".into())).unwrap_err();
    assert_eq!(error.code, ErrorCode::QuestionWithResult);
    h.engine.stop_ticket(&ticket_id, None).unwrap();
    assert_eq!(
        h.report_q(&ticket_id, ReportState::Progress, Question::Keep).unwrap_err().code,
        ErrorCode::TicketEnded
    );
    assert_eq!(h.engine.note(&ticket_id, "x").unwrap_err().code, ErrorCode::TicketEnded);
}

#[test]
fn checkpoint_mode_needs_an_approved_checkpoint_before_ready() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let error = h.report_q(&ticket_id, ReportState::Ready, Question::Keep).unwrap_err();
    assert_eq!(error.code, ErrorCode::CheckpointRequired);
    h.engine.checkpoint(&ticket_id, "plan", "the plan").unwrap();
    h.engine.approve(&ticket_id, None).unwrap();
    assert_eq!(h.ticket(&ticket_id).approved_checkpoints, vec!["plan".to_string()]);
    assert_eq!(
        h.report(&ticket_id, ReportState::Ready, "done").outcome(),
        Some(TicketOutcome::Completed)
    );
}

#[test]
fn a_ready_report_shows_review_until_the_ticket_ends() {
    let h = harness();
    let ticket_id = h.accepted("1");
    h.engine.checkpoint(&ticket_id, "plan", "p").unwrap();
    h.engine.approve(&ticket_id, None).unwrap();
    let ticket = h.report(&ticket_id, ReportState::Ready, "done");
    assert_eq!(ticket_stage(&ticket), TicketStage::Ended);
    assert_eq!(ticket.latest_report().unwrap().state, ReportState::Ready);
}

// ---- checkpoints (PLAN §6) ----

#[test]
fn a_checkpoint_waits_for_review_and_an_approval_resumes_the_worker() {
    let h = harness();
    let ticket_id = h.accepted("1");
    assert_eq!(h.engine.approve(&ticket_id, None).unwrap_err().code, ErrorCode::NoCheckpoint);
    let ticket = h.engine.checkpoint(&ticket_id, "plan", "Plan: three steps").unwrap();
    assert_eq!(
        ticket_stage(&ticket),
        TicketStage::Person {
            action: PersonAction::Review
        }
    );
    assert_eq!(ticket.checkpoint.as_ref().unwrap().name, "plan");
    assert_eq!(
        ticket.running_assignment().unwrap().report.as_ref().unwrap().state,
        ReportState::Waiting
    );
    // Interrupted at a checkpoint, the worker keeps its session.
    let running = h.running(&ticket_id).id;
    h.engine.assignment_interrupted(&ticket_id, &running).unwrap();
    assert_eq!(h.ticket(&ticket_id).assignments.len(), 1);
    // The scope changed while it waited: the worker posts the plan again and the summary follows.
    let revised = h.engine.checkpoint(&ticket_id, "plan", "Plan: one repo only").unwrap();
    assert_eq!(revised.checkpoint.as_ref().unwrap().summary, "Plan: one repo only");
    // Sent back, it is not approved and the worker hears why.
    let back = h.engine.request_changes(&ticket_id, "drop step 3").unwrap();
    assert!(back.checkpoint.is_none() && back.approved_checkpoints.is_empty());
    assert_eq!(h.engine.request_changes(&ticket_id, "x").unwrap_err().code, ErrorCode::NoCheckpoint);
    h.engine.checkpoint(&ticket_id, "plan", "Plan: two steps").unwrap();
    // A guard checkpoint holds a risky action back and is not replaced by another name.
    assert_eq!(h.engine.retitle(&ticket_id, "  Fix   login only ").unwrap().title, "Fix login only");
    let approved = h.engine.approve(&ticket_id, Some("looks good".into())).unwrap();
    assert!(approved.checkpoint.is_none());
    assert_eq!(ticket_stage(&approved), TicketStage::Idle, "idle until the worker reports progress");
    assert!(
        matches!(approved.inputs.last().unwrap(), TicketInput::Text { text, .. } if text.contains("'plan' approved") && text.contains("looks good"))
    );
    assert_eq!(h.settle().len(), 2, "the change request and the approval");
    assert!(h.event_types().iter().any(|t| t == "checkpoint_reached"));
    assert!(h.event_types().iter().any(|t| t == "checkpoint_approved"));
    h.engine.stop_ticket(&ticket_id, None).unwrap();
    assert_eq!(h.engine.checkpoint(&ticket_id, "x", "y").unwrap_err().code, ErrorCode::TicketEnded);
}

// ---- profile changes ----

#[test]
fn tells_only_the_live_tickets_a_profile_change_bears_on() {
    let h = harness();
    let live = h.accepted("1");
    let ended = h.accepted("2");
    h.report(&ended, ReportState::Ready, "done");
    let untouched = h.accepted("3");
    let mut effects = HashMap::new();
    effects.insert(
        live.clone(),
        PolicyEffect {
            rules: vec![RuleChange {
                id: "r1".into(),
                change: "changed".into(),
                instruction: Some("be careful".into()),
            }],
            proactive_withdrawn: false,
        },
    );
    effects.insert(ended.clone(), PolicyEffect::default());
    *h.policy.version.borrow_mut() = "v2".into();
    h.engine.policy_changed("v1", "v2", &effects).unwrap();
    assert!(
        matches!(&h.ticket(&live).inputs[0], TicketInput::PolicyChanged { from, to, effect: Some(effect), .. } if from == "v1" && to == "v2" && effect.rules[0].id == "r1")
    );
    assert!(!h
        .ticket(&ended)
        .inputs
        .iter()
        .any(|i| matches!(i, TicketInput::PolicyChanged { .. })));
    assert!(h.ticket(&untouched).inputs.is_empty());
    assert!(h.event_types().iter().any(|t| t == "policy_changed"));
}

#[test]
fn launches_a_failed_launch_again_for_a_persons_message_but_not_for_a_profile_change() {
    let h = harness();
    h.engine.receive(request("1")).unwrap();
    h.pump();
    let candidate = h.last_candidate();
    let outcome = Outcome::Accept {
        title: "Fix".into(),
        scope: "s".into(),
        reason: Reason::said("r"),
        rules: vec![],
        workflow: None,
        mode: None,
        name: None,
        note: None,
        audit: None,
    };
    let ticket_id = h
        .engine
        .decide(
            "1",
            DecisionInput {
                eval_id: Some(candidate.eval_id),
                outcome,
            },
        )
        .unwrap()
        .ticket_id
        .unwrap();
    h.report(&ticket_id, ReportState::Failed, "the prepare command failed");
    h.recorder.launches.borrow_mut().clear();
    let mut effects = HashMap::new();
    effects.insert(ticket_id.clone(), PolicyEffect::default());
    *h.policy.version.borrow_mut() = "v2".into();
    h.engine.policy_changed("v1", "v2", &effects).unwrap();
    assert!(h.recorder.launches.borrow().is_empty());
    h.tick(1);
    h.engine.receive(request("2")).unwrap();
    h.pump();
    h.engine
        .decide(
            "2",
            DecisionInput {
                eval_id: None,
                outcome: Outcome::Attach {
                    ticket_id: ticket_id.clone(),
                    reason: Reason::code("routed"),
                    note: None,
                },
            },
        )
        .unwrap();
    assert_eq!(h.recorder.launches.borrow().len(), 1);
    assert_eq!(h.recorder.launches.borrow()[0].0, ticket_id);
}

// ---- candidates ----

#[test]
fn a_candidate_carries_the_tickets_of_its_project_for_duplicate_judgement() {
    let h = harness();
    let same = h.accepted("1");
    h.engine
        .receive(Request {
            project: Some("other".into()),
            ..request("2")
        })
        .unwrap();
    h.pump();
    let other = h.accept(&h.last_candidate()).ticket_id.unwrap();
    h.engine.receive(request("3")).unwrap();
    let candidate = h.engine.candidate("3").unwrap();
    let ids: Vec<&str> = candidate.tickets.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec![same.as_str()]);
    assert_ne!(same, other);
    assert!(h.engine.candidate("nope").is_none());
    h.engine
        .receive(Request {
            project: None,
            ..request("4")
        })
        .unwrap();
    assert_eq!(h.engine.candidate("4").unwrap().tickets.len(), 2, "no project: every ticket");
}

#[test]
fn a_failed_transaction_leaves_nothing_behind() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let before = h.ticket(&ticket_id);
    let error = h
        .engine
        .report(
            &ticket_id,
            WorkerReport {
                state: ReportState::Ready,
                summary: "x".into(),
                question: Question::Ask("Q?".into()),
                assignment_id: None,
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::QuestionWithResult);
    assert_eq!(h.ticket(&ticket_id), before);
}

#[test]
fn stages_follow_the_table_in_order() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let mut ticket = h.ticket(&ticket_id);
    assert_eq!(ticket_stage(&ticket).label(), "working");
    ticket.assignments[0].state = AssignmentState::Pending;
    assert_eq!(ticket_stage(&ticket).label(), "working/queued");
    ticket.assignments[0].state = AssignmentState::Running;
    ticket.assignments[0].report = Some(Report {
        state: ReportState::Waiting,
        summary: "s".into(),
        at: "t".into(),
    });
    assert_eq!(ticket_stage(&ticket).label(), "idle");
    ticket.checkpoint = Some(Checkpoint {
        id: "c".into(),
        name: "plan".into(),
        summary: "s".into(),
        at: "t".into(),
    });
    assert_eq!(ticket_stage(&ticket).label(), "person/review");
    ticket.attention = Some(Attention {
        id: "a".into(),
        question: "q".into(),
        request_id: None,
    });
    assert_eq!(ticket_stage(&ticket).label(), "person/answer");
    ticket.delivery = Some(DeliveryFailure {
        error: "e".into(),
        at: "t".into(),
    });
    assert_eq!(ticket_stage(&ticket).label(), "person/delivery");
    assert!(ticket_stage(&ticket).needs_person());
}

// ---- delegation (phase 3) ----

#[test]
fn a_spawned_child_runs_beside_its_parent_and_its_final_report_tells_the_parent() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let root = h.running(&ticket_id).id;
    let child = h
        .engine
        .spawn(&ticket_id, &root, "reviewer", "Review the diff", Some(Mode::Autonomous))
        .unwrap();
    assert_eq!(child.parent.as_deref(), Some(root.as_str()));
    assert_eq!(
        child.mode,
        Some(Mode::Checkpoint),
        "a child is never more autonomous than its parent"
    );
    assert_eq!(child.task.as_deref(), Some("Review the diff"));
    assert_eq!(h.recorder.launches.borrow().last().unwrap().1, child.id);
    h.settle();
    let ticket = h.ticket(&ticket_id);
    assert_eq!(ticket.running_assignments().count(), 2);
    assert_eq!(ticket.running_assignment().unwrap().id, root, "the root still runs the ticket");
    // The child's ready report does not end the ticket; it ends the child and reaches the parent.
    h.engine
        .report(
            &ticket_id,
            WorkerReport {
                state: ReportState::Ready,
                summary: "LGTM, two nits".into(),
                question: Question::Keep,
                assignment_id: Some(child.id.clone()),
            },
        )
        .unwrap();
    let ticket = h.ticket(&ticket_id);
    assert!(ticket.live());
    let stored = ticket.assignments.iter().find(|a| a.id == child.id).unwrap();
    assert_eq!(
        (stored.state, stored.ended_reason.as_deref()),
        (AssignmentState::Ended, Some("ready"))
    );
    assert!(ticket
        .undelivered
        .iter()
        .any(|id| matches!(ticket.input(id), Some(TicketInput::Text { text, .. }) if text.contains("reviewer") && text.contains("LGTM"))));
    assert_eq!(ticket_stage(&ticket), TicketStage::Working { queued: false });
    assert_eq!(h.recorder.stops.borrow().last().unwrap().1, child.id);
    // Spawning under an ended assignment or an ended ticket is refused.
    assert_eq!(
        h.engine.spawn(&ticket_id, &child.id, "x", "y", None).unwrap_err().code,
        ErrorCode::AssignmentEnded
    );
    h.engine.stop_ticket(&ticket_id, None).unwrap();
    assert_eq!(
        h.engine.spawn(&ticket_id, &root, "x", "y", None).unwrap_err().code,
        ErrorCode::TicketEnded
    );
}

#[test]
fn a_root_ready_report_ends_a_ticket_with_a_running_child_too() {
    let h = harness();
    let ticket_id = h.accepted("1");
    let root = h.running(&ticket_id).id;
    let child = h.engine.spawn(&ticket_id, &root, "reviewer", "Review", None).unwrap();
    h.settle();
    h.report(&ticket_id, ReportState::Ready, "done");
    let ticket = h.ticket(&ticket_id);
    assert_eq!(ticket.outcome(), Some(TicketOutcome::Completed));
    assert!(ticket.assignments.iter().all(|a| a.state == AssignmentState::Ended));
    assert!(h.recorder.stops.borrow().iter().any(|(_, a, _)| a == &child.id));
}
