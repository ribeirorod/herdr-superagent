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
    /// Debug mode: log every herdr request, daemon call and effect to the per-process log files.
    #[arg(long, global = true)]
    pub debug: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Make sure the superagent workspace and daemon exist (idempotent; run by the plugin's startup hook).
    Ensure,
    /// Open the cockpit and start the superagent session (calibration on first start).
    Start(StartArgs),
    /// Stop the daemon of a profile: finish the batch, push, release the lease.
    Stop,
    /// Inventory for calibration: harnesses, their MCP servers, repos, herdr.
    Doctor,
    /// Commit and push the state repo now.
    Sync,
    /// Rebuild the local database from the state repo's ticket records.
    Reindex,
    /// Per-project memory: `memory add --title … --summary-file …`, `memory search "…"`, `memory show <id>`.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// The read-only task list (the cockpit's right pane). `--once` prints one frame.
    Dashboard {
        #[arg(long)]
        once: bool,
    },
    /// Show the log files: `logs`, `logs --process daemon --lines 200`, `logs --follow`.
    Logs {
        /// daemon, cli, ensure or all.
        #[arg(long, default_value = "all")]
        process: String,
        #[arg(long, default_value_t = 200)]
        lines: usize,
        /// Keep printing new lines (like tail -f).
        #[arg(long)]
        follow: bool,
    },
    /// Collect logs, daemon state, profile, doctor and the herdr snapshot into one archive to share.
    DebugBundle {
        /// Where to write the archive (default: ~/herdr-superagent-debug-<profile>-<time>.tar.gz).
        #[arg(long)]
        output: Option<String>,
    },
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
    /// Show or change the profile: `profile`, `profile set key=value …`, `profile undo`.
    Profile {
        #[command(subcommand)]
        action: Option<ProfileAction>,
    },
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
    /// Break another machine's lease on this profile's state repo.
    #[arg(long)]
    pub take_over: bool,
}

#[derive(Subcommand, Debug)]
pub enum MemoryAction {
    /// Record a session summary (for the current ticket) or a note (with --project).
    Add {
        #[arg(long)]
        title: String,
        /// A markdown file with the summary.
        #[arg(long, conflicts_with = "text")]
        summary_file: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long, env = "SUPERAGENT_TICKET")]
        ticket: Option<String>,
        /// A project key (org/repo) or a repository path; defaults to the ticket's project.
        #[arg(long)]
        project: Option<String>,
        #[arg(long, env = "SUPERAGENT_ROLE", default_value = "worker")]
        role: String,
    },
    /// Search this project's memory (the ticket's project unless --project is given).
    Search {
        query: String,
        #[arg(long, env = "SUPERAGENT_TICKET")]
        ticket: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 8)]
        limit: u64,
    },
    /// Print one memory entry in full.
    Show { id: String },
}

#[derive(Subcommand, Debug)]
pub enum ProfileAction {
    /// Set keys: `superagent.model=opus repos[+].path=~/work/api modes.cap=checkpoint`.
    Set {
        assignments: Vec<String>,
        /// Confirm a change to a protected key (modes, repos, limits, routing).
        #[arg(long)]
        yes: bool,
        /// Show the diff and the mode replay without saving.
        #[arg(long)]
        dry_run: bool,
    },
    /// Revert the last `profile set`.
    Undo,
    /// Print the profile directory path.
    Path,
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
            let report = crate::session::start(&profile, args.harness, args.model, args.take_over)?;
            let value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
            let mut text = ensure_text(&report.ensure);
            text.push_str(&format!(
                "\nsession: {} ({} {}){}{}",
                report.session,
                report.harness,
                if report.model.is_empty() {
                    "default model".to_string()
                } else {
                    report.model.clone()
                },
                report.prompt.as_ref().map(|p| format!(", briefed for {p}")).unwrap_or_default(),
                report.session_pane_id.as_ref().map(|p| format!(", pane {p}")).unwrap_or_default()
            ));
            if let Some(lease) = &report.lease {
                text.push_str(&format!("\nlease: {lease}"));
            }
            if let Some(n) = report.reindexed {
                text.push_str(&format!("\nreindexed {n} tickets from the state repo"));
            }
            out(value, text)
        }
        Command::Stop => {
            let result = crate::session::stop(&profile)?;
            out(
                result,
                format!("stopping the daemon of profile '{profile}' (batch committed, lease released)"),
            )
        }
        Command::Doctor => {
            let loaded = crate::profile::Profile::load_or_init(&paths.profile_file())?;
            let report = crate::calibration::doctor(&loaded);
            let value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
            out(value, doctor_text(&report))
        }
        Command::Sync => {
            let value = daemon("sync", json!({}))?;
            out(
                value.clone(),
                format!("committed: {} · push: {}", value["committed"], value["push"].as_str().unwrap_or("")),
            )
        }
        Command::Reindex => {
            let value = daemon("reindex", json!({}))?;
            out(value.clone(), format!("reindexed {} tickets", value["tickets"]))
        }
        Command::Dashboard { once } => {
            crate::dashboard::run(&profile, once)?;
            Ok(0)
        }
        Command::Memory {
            action:
                MemoryAction::Add {
                    title,
                    summary_file,
                    text,
                    ticket,
                    project,
                    role,
                },
        } => {
            let body = match (summary_file, text) {
                (Some(file), _) => std::fs::read_to_string(&file).map_err(|e| format!("read {file}: {e}"))?,
                (None, Some(text)) => text,
                (None, None) => return Err("memory add needs --summary-file or --text".into()),
            };
            let value = daemon(
                "memory_add",
                json!({ "title": title, "body": body, "ticket": ticket, "project": project, "role": role }),
            )?;
            out(
                value.clone(),
                format!(
                    "remembered {} in project {}{}",
                    value["id"].as_str().unwrap_or(""),
                    value["project"].as_str().unwrap_or(""),
                    value["file"].as_str().map(|f| format!(" ({f})")).unwrap_or_default()
                ),
            )
        }
        Command::Memory {
            action:
                MemoryAction::Search {
                    query,
                    ticket,
                    project,
                    limit,
                },
        } => {
            let value = daemon(
                "memory_search",
                json!({ "query": query, "ticket": ticket, "project": project, "limit": limit }),
            )?;
            let hits = value["hits"].as_array().cloned().unwrap_or_default();
            let mut text = format!("project {} · {} hits", value["project"].as_str().unwrap_or(""), hits.len());
            for h in &hits {
                text.push_str(&format!(
                    "\n{}  {}  {}\n    {}",
                    h["id"].as_str().unwrap_or("").chars().take(8).collect::<String>(),
                    h["created_at"].as_str().unwrap_or("").chars().take(10).collect::<String>(),
                    h["title"].as_str().unwrap_or(""),
                    h["snippet"].as_str().unwrap_or("").replace('\n', " ")
                ));
            }
            out(value, text)
        }
        Command::Memory {
            action: MemoryAction::Show { id },
        } => {
            let value = daemon("memory_get", json!({ "id": id }))?;
            if value.is_null() {
                return Err(format!("no memory entry {id}"));
            }
            out(
                value.clone(),
                format!(
                    "# {}\n{}",
                    value["title"].as_str().unwrap_or(""),
                    value["body"].as_str().unwrap_or("")
                ),
            )
        }
        Command::Logs { process, lines, follow } => {
            let files: Vec<std::path::PathBuf> = if process == "all" {
                let mut list: Vec<_> = std::fs::read_dir(paths.logs_dir())
                    .map(|d| {
                        d.filter_map(Result::ok)
                            .map(|e| e.path())
                            .filter(|p| p.extension().map(|e| e == "log").unwrap_or(false))
                            .collect()
                    })
                    .unwrap_or_default();
                list.sort();
                list
            } else {
                vec![crate::logging::log_file(&paths, &process)]
            };
            if files.is_empty() {
                return Err(format!("no logs yet under {}", paths.logs_dir().display()));
            }
            for file in &files {
                println!("==> {} <==", file.display());
                let text = std::fs::read_to_string(file).unwrap_or_default();
                let all: Vec<&str> = text.lines().collect();
                for line in &all[all.len().saturating_sub(lines)..] {
                    println!("{line}");
                }
            }
            if follow {
                let mut offsets: Vec<u64> = files.iter().map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)).collect();
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    for (file, offset) in files.iter().zip(offsets.iter_mut()) {
                        let Ok(len) = std::fs::metadata(file).map(|m| m.len()) else {
                            continue;
                        };
                        if len > *offset {
                            use std::io::{Read, Seek};
                            if let Ok(mut f) = std::fs::File::open(file) {
                                let _ = f.seek(std::io::SeekFrom::Start(*offset));
                                let mut buf = String::new();
                                let _ = f.read_to_string(&mut buf);
                                print!("{buf}");
                            }
                            *offset = len;
                        } else if len < *offset {
                            *offset = 0;
                        }
                    }
                }
            }
            Ok(0)
        }
        Command::DebugBundle { output } => {
            let value = debug_bundle(&profile, &paths, output)?;
            out(
                value.clone(),
                format!(
                    "wrote {}\nshare this file; it holds logs, daemon.json, profile.toml, doctor, status, tickets and the herdr snapshot (no credentials)",
                    value["archive"].as_str().unwrap_or("")
                ),
            )
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
        Command::Profile { action: None } => {
            let loaded = crate::profile::Profile::load_or_init(&paths.profile_file())?;
            let mut value = serde_json::to_value(&loaded).map_err(|e| e.to_string())?;
            value["profile_path"] = json!(paths.profile_file());
            value["profile_dir"] = json!(paths.profile_dir());
            value["version"] = json!(loaded.version());
            out(
                value,
                format!(
                    "# {} (version {})\n{}",
                    paths.profile_file().display(),
                    loaded.version(),
                    loaded.to_toml()
                ),
            )
        }
        Command::Profile {
            action: Some(ProfileAction::Path),
        } => out(
            json!({ "profile_dir": paths.profile_dir() }),
            paths.profile_dir().display().to_string(),
        ),
        Command::Profile {
            action: Some(ProfileAction::Undo),
        } => {
            let store = crate::profile::ProfileStore::new(paths.profile_file());
            let before = store.read_text()?;
            match store.undo()? {
                Some(after) => {
                    let _ = daemon("profile_reload", json!({}));
                    let lines = crate::profile::diff(&before, &after);
                    out(json!({ "undone": true, "diff": lines }), format!("undone:\n{}", lines.join("\n")))
                }
                None => out(json!({ "undone": false }), "nothing to undo".into()),
            }
        }
        Command::Profile {
            action: Some(ProfileAction::Set { assignments, yes, dry_run }),
        } => {
            if assignments.is_empty() {
                return Err("profile set needs at least one key=value".into());
            }
            let parsed = assignments
                .iter()
                .map(|a| crate::profile::Assignment::parse(a))
                .collect::<Result<Vec<_>, _>>()?;
            let protected: Vec<String> = parsed.iter().filter(|a| a.protected()).map(|a| a.key.clone()).collect();
            let store = crate::profile::ProfileStore::new(paths.profile_file());
            let before_text = store.read_text()?;
            let before = crate::profile::Profile::parse(&before_text)?;
            let after = crate::profile::apply(&before_text, &parsed)?;
            let lines = crate::profile::diff(&before_text, &after.to_toml());
            // Replay recent requests against the new rules (PLAN §4).
            let requests: Vec<(crate::core::model::Request, Option<crate::core::model::Mode>)> = daemon("requests", json!({ "limit": 50 }))
                .ok()
                .and_then(|v| v.as_array().cloned())
                .map(|items| {
                    items
                        .into_iter()
                        .filter_map(|item| {
                            let request = serde_json::from_value(item.get("request")?.clone()).ok()?;
                            let proposed = item.get("proposed").and_then(|p| serde_json::from_value(p.clone()).ok());
                            Some((request, proposed))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let changes = crate::profile::mode_changes(&before, &after, &requests);
            let mut text = if lines.is_empty() {
                "no change".to_string()
            } else {
                lines.join("\n")
            };
            if !changes.is_empty() {
                text.push_str("\nmode changes for recent requests:");
                for c in &changes {
                    text.push_str(&format!("\n  {}  {} → {}  {}", c.request_id, c.before, c.after, c.text));
                }
            }
            if !protected.is_empty() && !yes {
                text.push_str(&format!(
                    "\nrefused: {} {} protected (affects how much review a request gets). Run this from your own shell with --yes.",
                    protected.join(", "),
                    if protected.len() == 1 { "is" } else { "are" }
                ));
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({ "saved": false, "protected": protected, "diff": lines, "mode_changes": changes })
                        )
                        .unwrap_or_default()
                    );
                } else {
                    println!("{text}");
                }
                return Ok(3);
            }
            if dry_run || lines.is_empty() {
                return out(
                    json!({ "saved": false, "dry_run": dry_run, "diff": lines, "mode_changes": changes }),
                    format!("{text}\n(not saved)"),
                );
            }
            store.commit(&before_text, &after)?;
            let reload = daemon("profile_reload", json!({})).ok();
            text.push_str(&format!(
                "\nsaved {} (version {} → {}){}",
                paths.profile_file().display(),
                before.version(),
                after.version(),
                if reload.is_some() { ", daemon reloaded" } else { "" }
            ));
            out(
                json!({ "saved": true, "diff": lines, "mode_changes": changes, "from": before.version(), "to": after.version() }),
                text,
            )
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

/// Collects everything needed to debug a session into one tar.gz.
fn debug_bundle(profile: &str, paths: &Paths, output: Option<String>) -> Result<Value, String> {
    let stamp = crate::core::clock::now_iso().replace([':', '.'], "-");
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let archive = output.unwrap_or_else(|| format!("{home}/herdr-superagent-debug-{profile}-{stamp}.tar.gz"));
    let dir = std::env::temp_dir().join(format!("herdr-superagent-debug-{profile}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let copy = |from: &std::path::Path, name: &str| {
        if from.is_file() {
            let _ = std::fs::copy(from, dir.join(name));
        }
    };
    let write = |name: &str, text: String| {
        let _ = std::fs::write(dir.join(name), text);
    };
    if let Ok(entries) = std::fs::read_dir(paths.logs_dir()) {
        for entry in entries.filter_map(Result::ok) {
            copy(&entry.path(), &entry.file_name().to_string_lossy());
        }
    }
    copy(&paths.daemon_info_path(), "daemon.json");
    copy(&paths.profile_file(), "profile.toml");
    copy(&paths.profile_dir().join("calibration.md"), "calibration.md");
    let loaded = crate::profile::Profile::load_or_init(&paths.profile_file())?;
    write(
        "doctor.json",
        serde_json::to_string_pretty(&crate::calibration::doctor(&loaded)).unwrap_or_default(),
    );
    let status = ipc::call(&paths.socket_path(), "status", json!({}), Duration::from_secs(5))
        .map(|v| v.to_string())
        .unwrap_or_else(|e| format!("daemon not reachable: {e}"));
    write("status.json", status);
    let tickets = ipc::call(&paths.socket_path(), "tickets", json!({ "all": true }), Duration::from_secs(5))
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
        .unwrap_or_default();
    write("tickets.json", tickets);
    let herdr = match crate::herdr::Herdr::from_env() {
        Ok(h) => {
            let snapshot = h
                .call("session.snapshot", json!({}))
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                .unwrap_or_else(|e| e.to_string());
            write("herdr-snapshot.json", snapshot);
            let plugins = h
                .call("plugin.list", json!({}))
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                .unwrap_or_else(|e| e.to_string());
            write("herdr-plugins.json", plugins);
            format!("socket {} version {}", h.socket_path().display(), h.ping().unwrap_or_default())
        }
        Err(e) => e.to_string(),
    };
    let env: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("HERDR_") || k.starts_with("SUPERAGENT_"))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    write(
        "environment.txt",
        format!(
            "herdr-superagent {}\nprofile {}\nos {} {}\nherdr: {}\nmachine {}\nbinary {}\non PATH: {}\n\n{}\n",
            crate::daemon::VERSION,
            profile,
            std::env::consts::OS,
            std::env::consts::ARCH,
            herdr,
            crate::staterepo::machine_name(),
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            crate::daemon::on_path("herdr-superagent"),
            env.join("\n")
        ),
    );
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(dir.parent().unwrap_or(&dir))
        .arg(dir.file_name().unwrap_or_default())
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    if !status.success() {
        return Err("tar failed".into());
    }
    Ok(json!({ "archive": archive }))
}

fn doctor_text(report: &crate::calibration::Doctor) -> String {
    let mut lines = vec![format!(
        "profile {} · {} · herdr: {}{} · herdr-superagent on PATH: {}",
        report.profile,
        if report.calibrated { "calibrated" } else { "NOT calibrated" },
        if report.inside_herdr { "inside" } else { "not inside" },
        report.herdr_version.as_ref().map(|v| format!(" {v}")).unwrap_or_default(),
        if report.superagent_on_path { "yes" } else { "NO" }
    )];
    for h in &report.harnesses {
        let mcp = match &h.mcp_servers {
            Some(list) if list.is_empty() => "no MCP servers".to_string(),
            Some(list) => format!("MCP: {}", list.join(", ")),
            None => "MCP: unknown".to_string(),
        };
        lines.push(format!(
            "  {:<7} {}{}{}",
            h.name,
            if h.installed && h.on_path {
                "installed"
            } else if h.installed {
                "installed, NOT on PATH"
            } else {
                "not installed"
            },
            if h.on_path { format!(" · {mcp}") } else { String::new() },
            h.note.as_ref().map(|n| format!(" ({n})")).unwrap_or_default()
        ));
    }
    for r in &report.repos {
        lines.push(format!(
            "  repo {:<32} {}{}",
            r.path,
            if r.exists { "exists" } else { "MISSING" },
            r.branch.as_ref().map(|b| format!(" · {b}")).unwrap_or_default()
        ));
    }
    lines.push(format!("  state repo: {}", report.state_repo));
    lines.join("\n")
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
    if let Some(lease) = status["lease"].as_str() {
        lines.push(format!(
            "state repo {} · lease {}",
            status["state_repo"].as_str().unwrap_or(""),
            lease
        ));
    }
    if let Some(effects) = status["effects"].as_array().filter(|e| !e.is_empty()) {
        let parts: Vec<String> = effects
            .iter()
            .map(|e| format!("{} {}", e["kind"].as_str().unwrap_or(""), e["pending"]))
            .collect();
        lines.push(format!("effects queued: {}", parts.join(", ")));
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
