//! Ticket stage classification and retry eligibility (the prototype's docs/003).

use serde::{Deserialize, Serialize};

use crate::core::model::{AssignmentState, ReportState, Ticket};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersonAction {
    Delivery,
    Answer,
    Review,
    Blocked,
    Failed,
}

/// Who has the ball.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "lowercase")]
pub enum TicketStage {
    Person {
        action: PersonAction,
    },
    Working {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        queued: bool,
    },
    Idle,
    Ended,
}

impl TicketStage {
    /// `person/review`, `working/queued`, `working`, `idle`, `ended`.
    pub fn label(self) -> String {
        match self {
            TicketStage::Person { action } => format!(
                "person/{}",
                match action {
                    PersonAction::Delivery => "delivery",
                    PersonAction::Answer => "answer",
                    PersonAction::Review => "review",
                    PersonAction::Blocked => "blocked",
                    PersonAction::Failed => "failed",
                }
            ),
            TicketStage::Working { queued: true } => "working/queued".to_string(),
            TicketStage::Working { queued: false } => "working".to_string(),
            TicketStage::Idle => "idle".to_string(),
            TicketStage::Ended => "ended".to_string(),
        }
    }
    pub fn needs_person(self) -> bool {
        matches!(self, TicketStage::Person { .. })
    }
}

/// Classifies a ticket's stage; the first matching rule wins.
pub fn ticket_stage(ticket: &Ticket) -> TicketStage {
    if ticket.ended.is_some() {
        return TicketStage::Ended;
    }
    if ticket.delivery.is_some() {
        return TicketStage::Person {
            action: PersonAction::Delivery,
        };
    }
    if ticket.attention.is_some() {
        return TicketStage::Person {
            action: PersonAction::Answer,
        };
    }
    if ticket.checkpoint.is_some() {
        return TicketStage::Person {
            action: PersonAction::Review,
        };
    }
    let assignment = ticket.latest_assignment();
    let report = assignment.and_then(|a| a.report.as_ref()).map(|r| r.state);
    if report == Some(ReportState::Ready) {
        return TicketStage::Person {
            action: PersonAction::Review,
        };
    }
    if matches!(report, Some(ReportState::Blocked) | Some(ReportState::Failed)) && ticket.waits_for_person() {
        let action = if report == Some(ReportState::Blocked) {
            PersonAction::Blocked
        } else {
            PersonAction::Failed
        };
        return TicketStage::Person { action };
    }
    match assignment {
        None => TicketStage::Working { queued: true },
        Some(a) if a.state == AssignmentState::Pending => TicketStage::Working { queued: true },
        Some(a) if report == Some(ReportState::Waiting) || a.state == AssignmentState::Ended => TicketStage::Idle,
        Some(_) => TicketStage::Working { queued: false },
    }
}

/// An open ticket stalled on a blocked or failed report without a newer
/// human follow-up, or with a failed input delivery.
pub fn can_retry(ticket: &Ticket) -> bool {
    ticket.live() && (ticket.waits_for_person() || ticket.delivery.is_some())
}
