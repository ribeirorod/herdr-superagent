//! In-memory `Repositories` with transaction rollback by snapshot.

use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::core::errors::UserError;
use crate::core::model::*;
use crate::core::ports::Repositories;

#[derive(Default, Clone)]
struct Tables {
    requests: BTreeMap<String, Request>,
    dispositions: BTreeMap<String, RequestDisposition>,
    evals: BTreeMap<String, Evaluation>,
    tickets: BTreeMap<String, Ticket>,
    decisions: BTreeMap<String, Decision>,
}

#[derive(Default)]
pub struct MemoryRepositories {
    tables: RefCell<Tables>,
    depth: RefCell<usize>,
}

impl MemoryRepositories {
    pub fn new() -> MemoryRepositories {
        MemoryRepositories::default()
    }
}

impl Repositories for MemoryRepositories {
    fn atomic(&self, work: &mut dyn FnMut() -> Result<(), UserError>) -> Result<(), UserError> {
        let snapshot = self.tables.borrow().clone();
        *self.depth.borrow_mut() += 1;
        let result = work();
        *self.depth.borrow_mut() -= 1;
        if result.is_err() {
            *self.tables.borrow_mut() = snapshot;
        }
        result
    }

    fn request_get(&self, id: &str) -> Option<Request> {
        self.tables.borrow().requests.get(id).cloned()
    }
    fn request_save(&self, request: &Request) {
        self.tables.borrow_mut().requests.insert(request.id.clone(), request.clone());
    }
    fn request_list(&self) -> Vec<Request> {
        let tables = self.tables.borrow();
        let mut list: Vec<Request> = tables.requests.values().cloned().collect();
        list.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
        list
    }
    fn disposition_get(&self, id: &str) -> Option<RequestDisposition> {
        self.tables.borrow().dispositions.get(id).cloned()
    }
    fn disposition_save(&self, id: &str, value: &RequestDisposition) {
        self.tables.borrow_mut().dispositions.insert(id.to_string(), value.clone());
    }

    fn eval_get(&self, id: &str) -> Option<Evaluation> {
        self.tables.borrow().evals.get(id).cloned()
    }
    fn eval_by_request(&self, request_id: &str) -> Option<Evaluation> {
        self.tables.borrow().evals.values().find(|e| e.request_id == request_id).cloned()
    }
    fn eval_save(&self, eval: &Evaluation) {
        self.tables.borrow_mut().evals.insert(eval.id.clone(), eval.clone());
    }
    fn eval_delete(&self, id: &str) {
        self.tables.borrow_mut().evals.remove(id);
    }
    fn eval_list(&self) -> Vec<Evaluation> {
        let tables = self.tables.borrow();
        let mut list: Vec<Evaluation> = tables.evals.values().cloned().collect();
        list.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        list
    }

    fn ticket_get(&self, id: &str) -> Option<Ticket> {
        self.tables.borrow().tickets.get(id).cloned()
    }
    fn ticket_save(&self, ticket: &Ticket) {
        self.tables.borrow_mut().tickets.insert(ticket.id.clone(), ticket.clone());
    }
    fn ticket_list(&self) -> Vec<Ticket> {
        let tables = self.tables.borrow();
        let mut list: Vec<Ticket> = tables.tickets.values().cloned().collect();
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        list
    }
    fn ticket_open(&self) -> Vec<Ticket> {
        self.ticket_list().into_iter().filter(|t| t.live()).collect()
    }
    fn ticket_active(&self) -> Vec<Ticket> {
        self.ticket_list().into_iter().filter(|t| t.active()).collect()
    }

    fn decision_get(&self, id: &str) -> Option<Decision> {
        self.tables.borrow().decisions.get(id).cloned()
    }
    fn decision_save(&self, decision: &Decision) {
        self.tables.borrow_mut().decisions.insert(decision.id.clone(), decision.clone());
    }
    fn decision_list(&self) -> Vec<Decision> {
        let tables = self.tables.borrow();
        let mut list: Vec<Decision> = tables.decisions.values().cloned().collect();
        list.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::errors::ErrorCode;

    #[test]
    fn rolls_back_a_failed_transaction() {
        let repo = MemoryRepositories::new();
        let request = Request {
            id: "r1".into(),
            source: "you".into(),
            actor: "me".into(),
            human: true,
            at: "t".into(),
            text: "x".into(),
            workflow: None,
            mode: None,
            project: None,
            meta: serde_json::Value::Null,
        };
        let result = repo.atomic(&mut || {
            repo.request_save(&request);
            Err(UserError::new(ErrorCode::Internal))
        });
        assert!(result.is_err());
        assert!(repo.request_get("r1").is_none());
        repo.atomic(&mut || {
            repo.request_save(&request);
            Ok(())
        })
        .unwrap();
        assert!(repo.request_get("r1").is_some());
    }
}
