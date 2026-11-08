//! The domain core: Requests become Tickets with a Mandate, Tickets run as
//! Assignments in panes, Assignments end with Reports. The core is pure data
//! and rules; it talks to the world only through the ports in `ports.rs`.
//!
//! It re-implements the kept part of DeepSeek's dsh-superagent prototype (see
//! docs/PLAN.md §5.3 and docs/rejected.md) around a single Request concept.

pub mod assignments;
pub mod clock;
pub mod engine;
pub mod errors;
pub mod evals;
pub mod ids;
pub mod model;
pub mod ports;
pub mod reasons;
pub mod stage;
pub mod tickets;

#[cfg(test)]
mod tests;
