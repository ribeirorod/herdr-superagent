//! The command line: `herdr-superagent <verb>`, for you, for the superagent
//! session and for workers (PLAN §5.4). Most verbs talk to the daemon over
//! its socket; `ensure`, `start` and `daemon` talk to herdr.

use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use crate::ipc;
use crate::paths::{validate_profile, Paths, DEFAULT_PROFILE};

#[derive(Parser, Debug)]
#[command(
    name = "herdr-superagent",
    version,
    about = "A herdr-aware superagent: routes your requests into tickets, runs workers in panes, lets you review."
)]
pub struct Cli {
    /// Profile (work, personal, …). Defaults to $SUPERAGENT_PROFILE or "default".
    #[arg(long, global = true, env = "SUPERAGENT_PROFILE")]
    pub profile: Option<String>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Make sure the superagent workspace and daemon exist (idempotent; run by the plugin's startup hook).
    Ensure,
    /// Open the superagent workspace, starting what is missing.
    Start(StartArgs),
    /// Stop the daemon of a profile.
    Stop,
    /// The daemon's status: tickets by stage, pending candidates and effects.
    Status,
    /// Run the daemon in the foreground (herdr runs this in the daemon pane).
    Daemon,
    /// Hand the superagent a request.
    Submit(SubmitArgs),
    /// Candidates waiting for the superagent session to route them.
    Candidates,
    /// Route a candidate: accept it into a ticket, attach it to one, stop one, or ignore it.
    Route(RouteArgs),
    /// List tickets.
    Tickets {
        /// Include ended tickets.
        #[arg(long)]
        all: bool,
    },
    /// Show one ticket.
    Ticket { ticket: String },
    /// Worker: report the ticket's status.
    Report(ReportArgs),
    /// Worker: stop at a milestone and wait for review.
    Checkpoint {
        name: String,
        #[arg(long, default_value = "")]
        summary: String,
        #[arg(long, env = "SUPERAGENT_TICKET")]
        ticket: String,
    },
    /// Approve the checkpoint a ticket waits at.
    Approve {
        ticket: String,
        #[arg(long)]
        comment: Option<String>,
    },
    /// Stop a ticket; it receives nothing more.
    StopTicket {
        ticket: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Retry a ticket whose worker failed or is blocked.
    Retry { ticket: String },
    /// Give a ticket's worker a note: context that asks for nothing.
    Note { ticket: String, text: String },
    /// Send a request to evaluation again.
    Reevaluate { request_id: String },
    /// Show the profile.
    Profile,
    /// Dispatch a herdr plugin action (reads HERDR_PLUGIN_ACTION_ID).
    #[command(hide = true)]
    Action,
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Harness for the superagent session (claude, codex, pi). Saved to the profile.
    #[arg(long)]
    pub harness: Option<String>,
    /// Model for the superagent session. Saved to the profile.
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Args, Debug)]
pub struct SubmitArgs {
    pub text: String,
    /// Name the workflow to skip routing.
    #[arg(long)]
    pub workflow: Option<String>,
    /// propose, checkpoint or autonomous.
    #[arg(long)]
    pub mode: Option<String>,
    /// The project (repo path) the request is about.
    #[arg(long)]
    pub project: Option<String>,
    /// A stable id, so a repeated submit is a no-op.
    #[arg(long)]
    pub id: Option<String>,
}

#[derive(Args, Debug)]
pub struct RouteArgs {
    pub request_id: String,
    /// accept | attach | stop | ignore
    pub decision: String,
    #[arg(long)]
    pub eval: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub ticket: Option<String>,
    #[arg(long)]
    pub workflow: Option<String>,
    #[arg(long)]
    pub mode: Option<String>,
    #[arg(long, default_value = "")]
    pub reason: String,
    #[arg(long)]
    pub note: Option<String>,
    /// Routing rules the assessment matched (needed to accept an inferred candidate).
    #[arg(long = "rule")]
    pub rules: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// progress | waiting | ready | blocked | failed | dropped
    #[arg(long)]
    pub state: String,
    #[arg(long, default_value = "")]
    pub summary: String,
    /// A question for a person. Repeat the same text to keep it, pass "" to clear it.
    #[arg(long)]
    pub question: Option<String>,
    #[arg(long, env = "SUPERAGENT_TICKET")]
    pub ticket: String,
}

pub fn run(cli: Cli) -> Result<i32, String> {
    let profile = cli.profile.clone().unwrap_or_else(|| DEFAULT_PROFILE.to_string());
    validate_profile(&profile)?;
    let paths = Paths::for_profile(&profile);
    let out = |value: Value, human: String| -> Result<i32, String> {
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        } else {
            println!("{human}");
        }
        Ok(0)
    };
    let daemon = |method: &str, params: Value| -> Result<Value, String> {
        ipc::call(&paths.socket_path(), method, params, Duration::from_secs(15)).map_err(|e| e.to_string())
    };

    match cli.command {
        Command::Daemon => {
            crate::daemon::run(&profile)?;
            Ok(0)
        }
        Command::Ensure => {
            let report = crate::ensure::ensure(&profile, false)?;
            let value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
            out(value, ensure_text(&report))
        }
        Command::Start(args) => {
            if args.harness.is_some() || args.model.is_some() {
                let mut saved = crate::profile::Profile::load_or_init(&paths.profile_file())?;
                if let Some(h) = args.harness {
                    saved.superagent.harness = h;
                }
                if let Some(m) = args.model {
                    saved.superagent.model = m;
                }
                saved.save(&paths.profile_file())?;
            }
            let report = crate::ensure::ensure(&profile, true)?;
            let value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
            let mut text = ensure_text(&report);
            text.push_str("\nThe superagent session (agent.start in the left pane) comes with phase 1.");
            out(value, text)
        }
        Command::Stop => {
            let result = daemon("shutdown", json!({ "restart": false }))?;
            out(result, format!("stopping the daemon of profile '{profile}'"))
        }
        Command::Status => {
            let status = daemon("status", json!({}))?;
            out(status.clone(), status_text(&status))
        }
        Command::Submit(args) => {
            let result = daemon(
                "submit",
                json!({ "text": args.text, "workflow": args.workflow, "mode": args.mode, "project": args.project, "id": args.id }),
            )?;
            let text = match result.get("ticket_id").and_then(Value::as_str) {
                Some(ticket) => format!("ticket {ticket} created (request {})", result["request_id"].as_str().unwrap_or("")),
                None => format!(
                    "request {} {}; waiting for the superagent to route it",
                    result["request_id"].as_str().unwrap_or(""),
                    result["received"].as_str().unwrap_or("")
                ),
            };
            out(result, text)
        }
        Command::Candidates => {
            let list = daemon("candidates", json!({}))?;
            let text = list
                .as_array()
                .map(|items| {
                    if items.is_empty() {
                        "no candidates waiting".to_string()
                    } else {
                        items
                            .iter()
                            .map(|c| {
                                format!(
                                    "{}  [{}]  {}",
                                    c["request"]["id"].as_str().unwrap_or(""),
                                    c["strength"].as_str().unwrap_or(""),
                                    first_line(c["request"]["text"].as_str().unwrap_or(""))
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                })
                .unwrap_or_default();
            out(list, text)
        }
        Command::Route(args) => {
            let reason = if args.reason.is_empty() {
                json!({ "code": "explicitRouted" })
            } else {
                json!({ "code": "said", "values": { "text": args.reason } })
            };
            let outcome = match args.decision.as_str() {
                "accept" => {
                    json!({ "kind": "accept", "title": args.title.clone().unwrap_or_else(|| "Untitled".into()), "scope": "routed", "reason": reason, "rules": args.rules, "workflow": args.workflow, "mode": args.mode, "note": args.note })
                }
                "attach" => {
                    json!({ "kind": "attach", "ticket_id": args.ticket.clone().ok_or("attach needs --ticket")?, "reason": reason, "note": args.note })
                }
                "stop" => json!({ "kind": "stop", "ticket_id": args.ticket.clone().ok_or("stop needs --ticket")?, "reason": reason }),
                "ignore" => json!({ "kind": "ignore", "reason": reason }),
                other => return Err(format!("unknown decision '{other}' (accept, attach, stop, ignore)")),
            };
            let decision = daemon(
                "decide",
                json!({ "request_id": args.request_id, "eval_id": args.eval, "outcome": outcome }),
            )?;
            let text = format!(
                "{} → {}{}",
                decision["request_id"].as_str().unwrap_or(""),
                decision["kind"].as_str().unwrap_or(""),
                decision["ticket_id"].as_str().map(|t| format!(" ticket {t}")).unwrap_or_default()
            );
            out(decision, text)
        }
        Command::Tickets { all } => {
            let list = daemon("tickets", json!({ "all": all }))?;
            let text = list
                .as_array()
                .map(|items| {
                    if items.is_empty() {
                        "no tickets".to_string()
                    } else {
                        items.iter().map(ticket_line).collect::<Vec<_>>().join("\n")
                    }
                })
                .unwrap_or_default();
            out(list, text)
        }
        Command::Ticket { ticket } => {
            let value = daemon("ticket", json!({ "ticket": ticket }))?;
            out(value.clone(), ticket_detail(&value))
        }
        Command::Report(args) => {
            let value = daemon(
                "report",
                json!({ "ticket": args.ticket, "state": args.state, "summary": args.summary, "question": args.question }),
            )?;
            out(value.clone(), ticket_line(&value))
        }
        Command::Checkpoint { name, summary, ticket } => {
            let value = daemon("checkpoint", json!({ "ticket": ticket, "name": name, "summary": summary }))?;
            let line = format!("{}\nStop here: end your turn and wait for approval.", ticket_line(&value));
            out(value, line)
        }
        Command::Approve { ticket, comment } => {
            let value = daemon("approve", json!({ "ticket": ticket, "comment": comment }))?;
            out(value.clone(), ticket_line(&value))
        }
        Command::StopTicket { ticket, reason } => {
            let value = daemon("stop_ticket", json!({ "ticket": ticket, "reason": reason }))?;
            out(value.clone(), ticket_line(&value))
        }
        Command::Retry { ticket } => {
            let value = daemon("retry", json!({ "ticket": ticket }))?;
            out(value.clone(), ticket_line(&value))
        }
        Command::Note { ticket, text } => {
            let value = daemon("note", json!({ "ticket": ticket, "text": text }))?;
            out(value.clone(), ticket_line(&value))
        }
        Command::Reevaluate { request_id } => {
            let value = daemon("reevaluate", json!({ "request_id": request_id }))?;
            out(value.clone(), format!("queued: {}", value["queued"]))
        }
        Command::Profile => {
            let value = daemon("profile", json!({})).or_else(|_| {
                crate::profile::Profile::load_or_init(&paths.profile_file())
                    .and_then(|p| serde_json::to_value(p).map_err(|e| e.to_string()))
            })?;
            let text = toml::to_string_pretty(&value).unwrap_or_default();
            out(value, text)
        }
        Command::Action => action(&profile, &paths),
    }
}

/// Plugin actions from the manifest, dispatched on `HERDR_PLUGIN_ACTION_ID`.
fn action(profile: &str, paths: &Paths) -> Result<i32, String> {
    let id = std::env::var("HERDR_PLUGIN_ACTION_ID").unwrap_or_default();
    let action = id.rsplit('.').next().unwrap_or("");
    let context: Value = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or(Value::Null);
    let call =
        |method: &str, params: Value| ipc::call(&paths.socket_path(), method, params, Duration::from_secs(15)).map_err(|e| e.to_string());
    match action {
        "start" => {
            crate::ensure::ensure(profile, true)?;
            Ok(0)
        }
        "submit" => {
            crate::ensure::ensure(profile, false)?;
            let text = context
                .get("selected_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                return Err("select the request text in a pane first".into());
            }
            let cwd = context
                .pointer("/pane/cwd")
                .or_else(|| context.get("cwd"))
                .and_then(Value::as_str)
                .map(str::to_string);
            call("submit", json!({ "text": text, "project": cwd }))?;
            Ok(0)
        }
        "status" => {
            let status = call("status", json!({}))?;
            println!("{}", status_text(&status));
            Ok(0)
        }
        other => Err(format!("unknown action '{other}'")),
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").chars().take(80).collect()
}

fn ensure_text(report: &crate::ensure::EnsureReport) -> String {
    let mut text = format!(
        "profile {}: {}  (workspace {}, daemon pane {}, version {})",
        report.profile,
        match &report.outcome {
            crate::ensure::EnsureOutcome::Running => "daemon running".to_string(),
            crate::ensure::EnsureOutcome::Restarted { from } => format!("daemon restarted from {from}"),
            crate::ensure::EnsureOutcome::Created => "workspace and daemon created".to_string(),
            crate::ensure::EnsureOutcome::Started => "daemon started".to_string(),
        },
        report.workspace_id.as_deref().unwrap_or("?"),
        report.daemon_pane_id.as_deref().unwrap_or("?"),
        report.version
    );
    for warning in &report.warnings {
        text.push_str("\nwarning: ");
        text.push_str(warning);
    }
    text
}

fn status_text(status: &Value) -> String {
    let mut lines = vec![format!(
        "profile {} · daemon {} (pid {}) · {} open / {} tickets · {} need you · {} candidates{}",
        status["profile"].as_str().unwrap_or(""),
        status["version"].as_str().unwrap_or(""),
        status["pid"],
        status["tickets"]["open"],
        status["tickets"]["total"],
        status["needs_you"],
        status["candidates"],
        if status["paused"].as_bool().unwrap_or(false) {
            " · PAUSED"
        } else {
            ""
        }
    )];
    if let Some(stages) = status["stages"].as_object() {
        let mut parts: Vec<String> = stages.iter().map(|(k, v)| format!("{k} {v}")).collect();
        parts.sort();
        if !parts.is_empty() {
            lines.push(format!("stages: {}", parts.join(", ")));
        }
    }
    if let Some(effects) = status["effects"].as_array().filter(|e| !e.is_empty()) {
        let parts: Vec<String> = effects
            .iter()
            .map(|e| format!("{} {}", e["kind"].as_str().unwrap_or(""), e["pending"]))
            .collect();
        lines.push(format!("effects waiting for an executor (phase 2): {}", parts.join(", ")));
    }
    lines.join("\n")
}

fn ticket_line(ticket: &Value) -> String {
    let report = ticket["assignments"]
        .as_array()
        .and_then(|a| a.iter().rev().find_map(|x| x.get("report").filter(|r| !r.is_null())))
        .map(|r| {
            format!(
                "{}: {}",
                r["state"].as_str().unwrap_or(""),
                first_line(r["summary"].as_str().unwrap_or(""))
            )
        })
        .unwrap_or_default();
    format!(
        "{}  {:<16} {:<11} {}  {}",
        ticket["id"].as_str().unwrap_or(""),
        ticket["stage"].as_str().unwrap_or(""),
        ticket["mode"].as_str().unwrap_or(""),
        first_line(ticket["title"].as_str().unwrap_or("")),
        report
    )
}

fn ticket_detail(ticket: &Value) -> String {
    let mut lines = vec![ticket_line(ticket)];
    if let Some(q) = ticket.pointer("/attention/question").and_then(Value::as_str) {
        lines.push(format!("question: {q}"));
    }
    if let Some(c) = ticket.get("checkpoint").filter(|c| !c.is_null()) {
        lines.push(format!(
            "checkpoint '{}': {}",
            c["name"].as_str().unwrap_or(""),
            c["summary"].as_str().unwrap_or("")
        ));
    }
    if let Some(inputs) = ticket["inputs"].as_array() {
        let undelivered = ticket["undelivered"].as_array().map(|u| u.len()).unwrap_or(0);
        lines.push(format!("inputs: {} ({undelivered} undelivered)", inputs.len()));
    }
    if let Some(assignments) = ticket["assignments"].as_array() {
        for a in assignments {
            lines.push(format!(
                "  assignment {} {} {}{}",
                &a["id"].as_str().unwrap_or("")[..8.min(a["id"].as_str().unwrap_or("").len())],
                a["role"].as_str().unwrap_or(""),
                a["state"].as_str().unwrap_or(""),
                a["pane_id"].as_str().map(|p| format!(" pane {p}")).unwrap_or_default()
            ));
        }
    }
    lines.join("\n")
}
