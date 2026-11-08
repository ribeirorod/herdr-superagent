//! Structured reason codes for decisions, filtered requests and ended
//! assignments. Codes are open: policies and sources may emit their own.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Built-in reason templates, by code.
pub const REASONS: &[(&str, &str)] = &[
    ("said", "{text}"),
    ("timeout", "Assessment timed out"),
    ("policyChanged", "Profile changed during assessment"),
    ("noAdmissible", "No admissible request"),
    ("startedTicket", "Already started ticket {ticket}"),
    ("acceptedBefore", "Request {request} was accepted before"),
    ("routed", "Routed to ticket {ticket}"),
    ("explicitRouted", "Explicit request routed by the superagent"),
    ("humanStop", "Human asked to stop"),
    ("retried", "Retried by a person"),
    ("hostRestarted", "herdr-superagent restarted"),
    ("stopUnknown", "Stop target is unknown or already ended"),
    ("stopNotExplicit", "A stop needs an explicit request"),
    (
        "directionUnsettled",
        "Implementation intent or target direction is not established, or a design decision is still open",
    ),
    ("noRuleMatched", "No enabled routing rule matched"),
    ("observed", "Observe mode: would accept ({text})"),
    ("approved", "Checkpoint approved by a person"),
    ("notHuman", "Not a person's request"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub code: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
    /// The request of a batch the reason applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl Reason {
    pub fn code(code: &str) -> Reason {
        Reason {
            code: code.to_string(),
            values: BTreeMap::new(),
            request_id: None,
        }
    }
    pub fn with(mut self, name: &str, value: &str) -> Reason {
        self.values.insert(name.to_string(), value.to_string());
        self
    }
    pub fn said(text: &str) -> Reason {
        Reason::code("said").with("text", text)
    }
}

/// Formats reasons in English, prefixed by the request id when present.
pub fn reason_text(reasons: &[Reason]) -> String {
    reasons
        .iter()
        .map(|reason| {
            let template = REASONS
                .iter()
                .find(|(code, _)| *code == reason.code)
                .map(|(_, template)| *template)
                .unwrap_or(reason.code.as_str());
            let mut text = template.to_string();
            for (name, value) in &reason.values {
                text = text.replace(&format!("{{{name}}}"), value);
            }
            match &reason.request_id {
                Some(id) => format!("{id}: {text}"),
                None => text,
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_templates_and_prefixes_request_ids() {
        let mut reason = Reason::code("routed").with("ticket", "t-1");
        reason.request_id = Some("r1".into());
        assert_eq!(reason_text(&[reason, Reason::said("hi")]), "r1: Routed to ticket t-1; hi");
        assert_eq!(reason_text(&[Reason::code("custom")]), "custom");
    }
}
