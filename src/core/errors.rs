//! User-facing errors: a caller's request is invalid (`refused`) or names
//! missing state (`missing`). CLI handlers map them to exit codes; the daemon
//! IPC returns them as `{code, message}`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    UnknownTicket,
    UnknownRequest,
    UnknownAssignment,
    UnknownEval,
    UnknownDecision,
    TicketEnded,
    AssignmentEnded,
    EvalFinished,
    NotAdmissible,
    NotExplicit,
    TicketStopped,
    MandateRevoked,
    NotRetryable,
    QuestionWithResult,
    NoCheckpoint,
    CheckpointOpen,
    CheckpointRequired,
    AcceptedBefore,
    Paused,
    Internal,
}

impl ErrorCode {
    fn describe(self) -> (bool, &'static str) {
        match self {
            ErrorCode::UnknownTicket => (true, "Unknown ticket"),
            ErrorCode::UnknownRequest => (true, "Unknown request"),
            ErrorCode::UnknownAssignment => (true, "Unknown assignment"),
            ErrorCode::UnknownEval => (true, "Unknown evaluation"),
            ErrorCode::UnknownDecision => (true, "Unknown decision"),
            ErrorCode::TicketEnded => (false, "Ticket has ended"),
            ErrorCode::AssignmentEnded => (false, "Assignment has ended; a newer assignment owns this ticket"),
            ErrorCode::EvalFinished => (false, "Evaluation {eval} has finished or a later one replaced it; decide the request's current evaluation"),
            ErrorCode::NotAdmissible => (false, "Request is not admissible: {reason}"),
            ErrorCode::NotExplicit => (false, "An inferred candidate is accepted only in autonomous routing with enabled rules its assessment matched"),
            ErrorCode::TicketStopped => (false, "A person stopped this ticket; new work needs a new accept"),
            ErrorCode::MandateRevoked => (false, "The current profile no longer holds this ticket's mandate; new work needs a new accept"),
            ErrorCode::NotRetryable => (false, "Only an open ticket can be retried, and only when its worker last reported failed or blocked and no person has sent a message since, or when delivering an input to its worker failed"),
            ErrorCode::QuestionWithResult => (false, "A question for a person cannot come with a ready result; report it separately"),
            ErrorCode::NoCheckpoint => (false, "Ticket has no checkpoint waiting for review"),
            ErrorCode::CheckpointOpen => (false, "Ticket already waits at checkpoint {checkpoint}; approve it first"),
            ErrorCode::CheckpointRequired => (false, "This ticket runs in {mode} mode: a ready report needs an approved checkpoint first (start with `checkpoint plan`)"),
            ErrorCode::AcceptedBefore => (false, "Request {request} already started ticket {ticket}"),
            ErrorCode::Paused => (false, "Processing is paused"),
            ErrorCode::Internal => (false, "{text}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserError {
    pub code: ErrorCode,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
}

impl UserError {
    pub fn new(code: ErrorCode) -> UserError {
        UserError {
            code,
            values: BTreeMap::new(),
        }
    }
    pub fn with(mut self, name: &str, value: &str) -> UserError {
        self.values.insert(name.to_string(), value.to_string());
        self
    }
    pub fn internal(text: impl fmt::Display) -> UserError {
        UserError::new(ErrorCode::Internal).with("text", &text.to_string())
    }
    /// True when the error names missing state rather than refusing a request.
    pub fn missing(&self) -> bool {
        self.code.describe().0
    }
    pub fn message(&self) -> String {
        let mut text = self.code.describe().1.to_string();
        for (name, value) in &self.values {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }
    pub fn code_name(&self) -> String {
        serde_json::to_value(self.code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| format!("{:?}", self.code))
    }
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for UserError {}

impl From<rusqlite::Error> for UserError {
    fn from(error: rusqlite::Error) -> Self {
        UserError::internal(format!("database: {error}"))
    }
}

impl From<serde_json::Error> for UserError {
    fn from(error: serde_json::Error) -> Self {
        UserError::internal(format!("json: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_values_and_classifies() {
        let error = UserError::new(ErrorCode::EvalFinished).with("eval", "e1");
        assert!(error.message().starts_with("Evaluation e1 has finished"));
        assert!(!error.missing());
        assert!(UserError::new(ErrorCode::UnknownTicket).missing());
        assert_eq!(error.code_name(), "evalFinished");
    }
}
