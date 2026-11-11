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
        /// Print the task list once and exit.
        #[arg(long)]
        once: bool,
        /// The read-only text list instead of the interactive cockpit.
        #[arg(long)]
        plain: bool,
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
    /// Where work arrives from (issues, pull requests, mail): list the profile's sources, or poll now.
    Sources {
        #[command(subcommand)]
        action: Option<SourcesAction>,
    },
    /// The workflows a request can be routed to, from the built-in and the profile's packs.
    Packs {
        /// Show one workflow's steps: `feature`, `software-dev/bugfix`, …
        workflow: Option<String>,
    },
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
    /// Worker: spawn a child (reviewer, builder, researcher) in a sibling tab of this ticket.
    Spawn {
        #[arg(long)]
        role: String,
        #[arg(long)]
        task: String,
        /// propose, checkpoint or autonomous; never more autonomous than yours.
        #[arg(long)]
        mode: Option<String>,
        #[arg(long, env = "SUPERAGENT_TICKET")]
        ticket: String,
        #[arg(long, env = "SUPERAGENT_ASSIGNMENT")]
        parent: Option<String>,
    },
    /// Worker: wait for a child assignment's final report.
    Watch {
        assignment: String,
        #[arg(long, env = "SUPERAGENT_TICKET")]
        ticket: String,
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
    },
    /// Guard shim target: hold a risky action back as a checkpoint in autonomous mode.
    #[command(hide = true)]
    Guard {
        tool: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// The sidebar Agents view for this profile: `view set`, `view clear`, `view snippet`.
    View {
        #[command(subcommand)]
        action: ViewAction,
    },
    /// Worker: stop at a milestone and wait for review.
    Checkpoint {
        name: String,
        #[arg(long, default_value = "")]
        summary: String,
        #[arg(long, env = "SUPERAGENT_TICKET")]
        ticket: String,
        #[arg(long, env = "SUPERAGENT_ASSIGNMENT")]
        assignment: Option<String>,
    },
    /// Approve the checkpoint a ticket waits at.
    Approve {
        ticket: String,
        #[arg(long)]
        comment: Option<String>,
    },
    /// Send the waiting checkpoint back to the worker with what must change.
    RequestChanges { ticket: String, comment: String },
    /// Rename a ticket (its workspace and record follow).
    Retitle { ticket: String, title: String },
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
    /// Close the herdr workspaces of ended tickets (their worktrees stay on disk).
    Tidy {
        /// Only tickets that ended more than this many hours ago.
        #[arg(long, default_value_t = 0)]
        older_than_hours: u64,
    },
    /// Clear this profile's worker titles and `sa_*` tokens from herdr panes and workspaces (the cockpit keeps its own).
    Cleanup {
        /// Clear every `sa_*` token herdr still holds, the cockpit's included, whatever the profile.
        #[arg(long)]
        all: bool,
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
pub enum ViewAction {
    /// Show only this profile's superagent and workers, plus anything blocked, sorted by attention.
    Set,
    /// Restore herdr's normal Agents view.
    Clear,
    /// Print the herdr config snippet that styles the `$sa_*` tokens in the sidebar.
    Snippet,
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
    /// Turn your earlier Claude Code, Codex and Pi sessions into project memory (extractive, redacted, incremental).
    Import {
        /// claude, codex, pi or all.
        #[arg(long, default_value = "all")]
        harness: String,
        /// Only sessions of this repository (a path or a project key).
        #[arg(long)]
        project: Option<String>,
        /// Also sessions outside the profile's repositories.
        #[arg(long)]
        all: bool,
        /// Only sessions changed in the last N days.
        #[arg(long)]
        since_days: Option<u64>,
        /// Show what would be imported without storing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProfileAction {
    /// Set keys: `superagent.model=opus repos[+].path=~/work/api modes.cap=checkpoint`.
    Set {
        assignments: Vec<String>,
        /// Confirm a change to a protected key (modes, limits, routing).
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
    /// The git ref the worker's branch starts from (default: the newest of the checked-out branch, its upstream and origin's default, after a fetch).
    #[arg(long)]
    pub base: Option<String>,
    /// A stable id, so a repeated submit is a no-op.
    #[arg(long)]
    pub id: Option<String>,
    /// The profile source this item came from (issues, mail); such a request always goes through routing.
    #[arg(long)]
    pub source: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum SourcesAction {
    /// Poll one source, or all of them, now.
    Poll { id: Option<String> },
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
    /// The repository this ticket works in (a path); the worker gets its own worktree of it.
    #[arg(long)]
    pub project: Option<String>,
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
    /// The assignment that reports (set for children).
    #[arg(long, env = "SUPERAGENT_ASSIGNMENT")]
    pub assignment: Option<String>,
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
        Command::Dashboard { once, plain } => {
            crate::dashboard::run(&profile, once, plain)?;
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
        Command::Memory {
            action:
                MemoryAction::Import {
                    harness,
                    project,
                    all,
                    since_days,
                    dry_run,
                },
        } => {
            use crate::importer::{consider, discover, Options, Outcome};
            let settings = daemon("profile", json!({}))?;
            let repos: Vec<String> = settings["repos"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r["path"].as_str())
                        .map(crate::profile::expand_home)
                        .collect()
                })
                .unwrap_or_default();
            let marks = daemon("import_marks", json!({}))?;
            let project_key = project.map(|p| crate::memory::project_key(&p).unwrap_or(p));
            let options = Options {
                repos: &repos,
                all,
                project: project_key.as_deref(),
                since_days,
            };
            let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
            let files = discover(&home, Some(&harness));
            let mut skipped: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            let mut by_project: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            let mut batch: Vec<Value> = Vec::new();
            let mut batch_marks = serde_json::Map::new();
            let mut imported = 0usize;
            let mut preview: Vec<String> = Vec::new();
            let flush = |batch: &mut Vec<Value>, batch_marks: &mut serde_json::Map<String, Value>| -> Result<(), String> {
                if !batch.is_empty() && !dry_run {
                    daemon(
                        "memory_import",
                        json!({ "entries": std::mem::take(batch), "marks": std::mem::take(batch_marks) }),
                    )?;
                }
                batch.clear();
                batch_marks.clear();
                Ok(())
            };
            for (name, file) in &files {
                let known = marks.get(file.display().to_string()).and_then(Value::as_str);
                match consider(name, file, known, &options) {
                    Outcome::Import(entry, stamp) => {
                        imported += 1;
                        *by_project.entry(entry.project.clone()).or_default() += 1;
                        if dry_run {
                            preview.push(format!("  {:<28} {}", entry.project, entry.title));
                        }
                        batch.push(serde_json::to_value(&*entry).map_err(|e| e.to_string())?);
                        batch_marks.insert(file.display().to_string(), Value::String(stamp));
                        if batch.len() >= 20 {
                            flush(&mut batch, &mut batch_marks)?;
                        }
                    }
                    Outcome::Skip(skip) => {
                        let name = serde_json::to_value(skip)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_default();
                        *skipped.entry(name).or_default() += 1;
                    }
                }
            }
            flush(&mut batch, &mut batch_marks)?;
            let mut lines = vec![format!(
                "{} {imported} session(s) from {} transcript file(s)",
                if dry_run { "would import" } else { "imported" },
                files.len()
            )];
            for (project, n) in &by_project {
                lines.push(format!("  {project}: {n}"));
            }
            lines.extend(preview.iter().cloned());
            let why = |key: &str| match key {
                "unchanged" => "already imported, unchanged",
                "outside" => "outside the profile's repositories (use --all to include them)",
                "own" => "superagent or worker sessions (their tickets have summaries)",
                "empty" => "nothing asked or nothing answered",
                "too_old" => "older than --since-days",
                _ => "unreadable or too large",
            };
            for (key, n) in &skipped {
                lines.push(format!("  skipped {n}: {}", why(key)));
            }
            if imported > 0 && !dry_run {
                lines.push("search it with `herdr-superagent memory search \"…\" --project <repo path>`; workers get the matching entries in their brief".to_string());
            }
            out(
                json!({ "imported": imported, "files": files.len(), "projects": by_project, "skipped": skipped, "dry_run": dry_run }),
                lines.join("\n"),
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
            let meta = args.base.as_ref().map(|b| json!({ "base": b })).unwrap_or(Value::Null);
            let result = daemon(
                "submit",
                json!({ "text": args.text.replace("\\n", "\n"), "workflow": args.workflow, "mode": args.mode, "project": args.project, "id": args.id, "meta": meta, "source": args.source }),
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
                                let request = &c["request"];
                                let mut line = format!(
                                    "{}  [{}]  {}",
                                    request["id"].as_str().unwrap_or(""),
                                    c["strength"].as_str().unwrap_or(""),
                                    first_line(request["text"].as_str().unwrap_or(""))
                                );
                                if let Some(project) = request["project"].as_str() {
                                    line.push_str(&format!("\n    project: {project}"));
                                }
                                // An item from a source: who sent it, and how to admit it.
                                if let Some(rule) = request["meta"]["rule"].as_str() {
                                    line.push_str(&format!(
                                        "\n    from source `{}` by {} · accept with --rule {rule}",
                                        request["source"].as_str().unwrap_or(""),
                                        request["actor"].as_str().unwrap_or("")
                                    ));
                                    let hint = request["meta"]["workflow_hint"].as_str().unwrap_or("");
                                    if !hint.is_empty() {
                                        line.push_str(&format!(" --workflow {hint}"));
                                    }
                                    let mode = request["meta"]["mode_hint"].as_str().unwrap_or("");
                                    if !mode.is_empty() {
                                        line.push_str(&format!(" --mode {mode}"));
                                    }
                                }
                                line
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
                json!({ "request_id": args.request_id, "eval_id": args.eval, "outcome": outcome, "project": args.project }),
            )?;
            let text = format!(
                "{} → {}{}",
                decision["request_id"].as_str().unwrap_or(""),
                decision["kind"].as_str().unwrap_or(""),
                decision["ticket_id"].as_str().map(|t| format!(" ticket {t}")).unwrap_or_default()
            );
            out(decision, text)
        }
        Command::Sources { action: None } => {
            let list = daemon("sources", json!({}))?;
            let text = match list.as_array().filter(|a| !a.is_empty()) {
                None => "no sources in the profile; add one with `profile set 'sources[+]={ id = \"issues\", kind = \"github\", repo = \"owner/name\", query = \"assignee:@me\", project = \"~/src/name\" }' --yes`".to_string(),
                Some(items) => items
                    .iter()
                    .map(|s| {
                        format!(
                            "{:<14} {:<8} every {:>3} min  {}\n{:<14} via {}\n{:<14} {}",
                            s["id"].as_str().unwrap_or(""),
                            s["kind"].as_str().unwrap_or(""),
                            s["every_minutes"],
                            if s["enabled"].as_bool().unwrap_or(true) { "" } else { "(disabled)" },
                            "",
                            s["via"].as_str().unwrap_or(""),
                            "",
                            s["status"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            out(list, text)
        }
        Command::Sources {
            action: Some(SourcesAction::Poll { id }),
        } => {
            let value = daemon("sources_poll", json!({ "id": id }))?;
            let text = format!(
                "polling {}; new items show up in `candidates`",
                value["polling"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default()
            );
            out(value, text)
        }
        Command::Packs { workflow } => {
            let (packs, problems) = crate::packs::load(&paths.profile_dir());
            for problem in &problems {
                eprintln!("skipped an invalid pack: {problem}");
            }
            match workflow {
                Some(name) => {
                    let chosen =
                        crate::packs::find(&packs, &name).ok_or_else(|| format!("no workflow '{name}'; run `herdr-superagent packs`"))?;
                    let text = chosen.brief_section(&chosen.root_role());
                    out(serde_json::to_value(&chosen).map_err(|e| e.to_string())?, text)
                }
                None => {
                    let mut lines = Vec::new();
                    for pack in &packs {
                        lines.push(format!(
                            "{} v{} ({}): {}",
                            pack.meta.id, pack.meta.version, pack.origin, pack.meta.description
                        ));
                        for w in &pack.workflows {
                            let checkpoints: Vec<&str> = w.steps.iter().filter_map(|s| s.checkpoint.as_deref()).collect();
                            lines.push(format!(
                                "  {:<12} {} [{} steps{}]",
                                w.id,
                                w.description,
                                w.steps.len(),
                                if checkpoints.is_empty() {
                                    String::new()
                                } else {
                                    format!(", checkpoints: {}", checkpoints.join(", "))
                                }
                            ));
                        }
                    }
                    lines.push(format!(
                        "your own packs go in {}/packs/<id>/ (pack.toml, roles/*.md, workflows/*.toml)",
                        paths.profile_dir().display()
                    ));
                    out(serde_json::to_value(&packs).map_err(|e| e.to_string())?, lines.join("\n"))
                }
            }
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
                json!({ "ticket": args.ticket, "state": args.state, "summary": args.summary, "question": args.question, "assignment": args.assignment }),
            )?;
            out(value.clone(), ticket_line(&value))
        }
        Command::Checkpoint {
            name,
            summary,
            ticket,
            assignment,
        } => {
            let value = daemon(
                "checkpoint",
                json!({ "ticket": ticket, "name": name, "summary": summary, "assignment": assignment }),
            )?;
            let line = format!("{}\nStop here: end your turn and wait for approval.", ticket_line(&value));
            out(value, line)
        }
        Command::Spawn {
            role,
            task,
            mode,
            ticket,
            parent,
        } => {
            let child = daemon(
                "spawn",
                json!({ "ticket": ticket, "role": role, "task": task, "mode": mode, "parent": parent }),
            )?;
            let id = child["id"].as_str().unwrap_or("").to_string();
            out(
                child.clone(),
                format!(
                    "spawned {role} {id} (mode {}); it opens in a new tab of this ticket's workspace. Wait with: herdr-superagent watch {id}",
                    child["mode"].as_str().unwrap_or("")
                ),
            )
        }
        Command::Watch {
            assignment,
            ticket,
            timeout,
        } => {
            let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
            loop {
                let value = daemon("ticket", json!({ "ticket": ticket }))?;
                let found = value["assignments"]
                    .as_array()
                    .and_then(|a| {
                        a.iter()
                            .find(|x| x["id"].as_str().map(|id| id.starts_with(assignment.as_str())).unwrap_or(false))
                    })
                    .cloned();
                let Some(found) = found else {
                    return Err(format!("no assignment {assignment} on ticket {ticket}"));
                };
                let state = found["report"]["state"].as_str().unwrap_or("");
                let ended = found["state"] == "ended";
                if ended || matches!(state, "ready" | "failed" | "dropped" | "blocked") || value["stage"] == "ended" {
                    let text = format!(
                        "{} {} · {}: {}",
                        found["role"].as_str().unwrap_or(""),
                        found["id"].as_str().unwrap_or("").chars().take(8).collect::<String>(),
                        if ended {
                            found["ended_reason"].as_str().unwrap_or("ended")
                        } else {
                            state
                        },
                        found["report"]["summary"].as_str().unwrap_or("")
                    );
                    return out(found, text);
                }
                if std::time::Instant::now() > deadline {
                    return Err(format!(
                        "watch timed out after {timeout}s; the child is still {}",
                        if state.is_empty() { "running" } else { state }
                    ));
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        Command::Guard { tool, args } => guard(&paths, &tool, &args),
        Command::View { action } => {
            let herdr = crate::herdr::Herdr::from_env().map_err(|e| e.to_string())?;
            let source = format!(
                "plugin:{}",
                std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "ribeirorod.superagent".into())
            );
            match action {
                ViewAction::Set => {
                    let params = json!({
                        "source": source,
                        "label": format!("superagent · {profile}"),
                        "filter": { "op": "any", "filters": [
                            { "op": "eq", "field": { "token": "sa_profile" }, "value": profile },
                            { "op": "in", "field": "status", "values": ["blocked"] }
                        ] },
                        "sort": [ { "field": "attention", "order": "desc" }, { "field": { "token": "sa_stage" }, "order": "asc" }, { "field": "state_change_seq", "order": "desc" } ]
                    });
                    let value = match herdr.call("agent.view.set", params.clone()) {
                        Ok(v) => v,
                        // Outside a linked plugin herdr refuses plugin sources; fall back to a plain one.
                        Err(_) => {
                            let mut plain = params;
                            plain["source"] = json!("superagent");
                            herdr.call("agent.view.set", plain).map_err(|e| e.to_string())?
                        }
                    };
                    out(
                        value,
                        format!("sidebar shows profile {profile}'s agents and anything blocked; `view clear` restores the normal list"),
                    )
                }
                ViewAction::Clear => {
                    let value = herdr.call("agent.view.clear", json!({})).map_err(|e| e.to_string())?;
                    out(value, "normal Agents view restored".into())
                }
                ViewAction::Snippet => {
                    let snippet = r##"# ~/.config/herdr/config.toml: purple superagent rows; worker rows show ticket and stage
[ui.sidebar.agents]
layout = [
  ["state_icon", { token = "$sa_role", fg = "#a855f7", bold = true, rules = [{ equals = "superagent", fg = "#a855f7", bold = true }, { equals = "daemon", hide = true }, { equals = "dashboard", hide = true }] }, "agent"],
  [{ token = "$sa_task", dim = true }, { token = "$sa_stage", rules = [{ starts_with = "person", fg = "#fbbf24", bold = true }, { equals = "working", fg = "#4ade80" }] }],
]
"##;
                    out(json!({ "snippet": snippet }), snippet.to_string())
                }
            }
        }
        Command::Approve { ticket, comment } => {
            let value = daemon("approve", json!({ "ticket": ticket, "comment": comment }))?;
            out(value.clone(), ticket_line(&value))
        }
        Command::RequestChanges { ticket, comment } => {
            let value = daemon("request_changes", json!({ "ticket": ticket, "comment": comment }))?;
            out(value.clone(), ticket_line(&value))
        }
        Command::Retitle { ticket, title } => {
            let value = daemon("retitle", json!({ "ticket": ticket, "title": title }))?;
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
        Command::Tidy { older_than_hours } => {
            let herdr = crate::herdr::Herdr::from_env().map_err(|e| e.to_string())?;
            let tickets = daemon("tickets", json!({ "all": true }))?;
            let open: std::collections::HashSet<String> = herdr
                .workspace_list()
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|w| w.workspace_id)
                .collect();
            let cutoff = crate::core::clock::plus_ms(&crate::core::clock::now_iso(), -(older_than_hours as i64) * 3_600_000);
            let mut closed = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            for t in tickets.as_array().cloned().unwrap_or_default() {
                let ended = t["ended"]["at"].as_str().unwrap_or("");
                let Some(workspace) = t.pointer("/execution/workspace_id").and_then(Value::as_str) else {
                    continue;
                };
                if ended.is_empty() || ended > cutoff.as_str() || !open.contains(workspace) {
                    continue;
                }
                // A workspace that is already gone, or one herdr refuses to close, must not stop the rest.
                match herdr.call("workspace.close", json!({ "workspace_id": workspace })) {
                    Ok(_) => closed.push(format!("{} ({})", t["id"].as_str().unwrap_or(""), workspace)),
                    Err(e) => skipped.push(format!("{} ({workspace}): {e}", t["id"].as_str().unwrap_or(""))),
                }
                // The worktree goes too when it is clean; its branch stays.
                if let (Some(repo), Some(dir)) = (t["project"].as_str(), t.pointer("/execution/directory").and_then(Value::as_str)) {
                    if let Some(note) = crate::gitbase::remove_worktree(&crate::profile::expand_home(repo), dir) {
                        skipped.push(format!("{}: {note}", t["id"].as_str().unwrap_or("")));
                    }
                }
            }
            for line in &skipped {
                eprintln!("skipped {line}");
            }
            out(
                json!({ "closed": closed, "skipped": skipped }),
                if closed.is_empty() {
                    "nothing to tidy".to_string()
                } else {
                    format!("closed {}", closed.join(", "))
                },
            )
        }
        Command::Cleanup { all } => {
            let herdr = crate::herdr::Herdr::from_env().map_err(|e| e.to_string())?;
            let mine = |tokens: &std::collections::HashMap<String, String>| {
                let ours: Vec<String> = tokens.keys().filter(|k| k.starts_with("sa_")).cloned().collect();
                let cockpit = matches!(
                    tokens.get("sa_role").map(String::as_str),
                    Some("superagent" | "daemon" | "dashboard")
                );
                let matches = all
                    || (!cockpit && tokens.get("sa_profile").map(|p| p == &profile).unwrap_or(false))
                    || (tokens.contains_key("sa_ticket") && !tokens.contains_key("sa_profile"));
                (matches && !ours.is_empty()).then_some(ours)
            };
            let mut cleared = Vec::new();
            for pane in herdr.pane_list().map_err(|e| e.to_string())? {
                if let Some(tokens) = mine(&pane.tokens) {
                    // A pane that closed since the listing has nothing left to clear.
                    match herdr.clear_pane_metadata(&pane.pane_id, &tokens) {
                        Ok(()) => cleared.push(pane.pane_id),
                        Err(e) => tracing::debug!(pane = %pane.pane_id, %e, "cleanup skipped a pane"),
                    }
                }
            }
            for workspace in herdr.workspace_list().map_err(|e| e.to_string())? {
                if let Some(tokens) = mine(&workspace.tokens) {
                    match herdr.clear_workspace_metadata(&workspace.workspace_id, &tokens) {
                        Ok(()) => cleared.push(workspace.workspace_id),
                        Err(e) => tracing::debug!(workspace = %workspace.workspace_id, %e, "cleanup skipped a workspace"),
                    }
                }
            }
            out(
                json!({ "cleared": cleared }),
                if cleared.is_empty() {
                    "nothing to clear".to_string()
                } else {
                    format!("cleared {}", cleared.join(", "))
                },
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

/// The guard shim: run the real tool, or hold the action back as a checkpoint.
fn guard(paths: &Paths, tool: &str, args: &[String]) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;
    let shims = paths.state_dir().join("shims");
    // Everything below, and the real tool once it runs, must see PATH without the shims:
    // the guard's own `git` calls would otherwise be the shim again, without end.
    let clean = crate::guard::path_without(&shims);
    std::env::set_var("PATH", &clean);
    let real = crate::guard::real_tool(tool, &shims).ok_or_else(|| format!("guard: {tool} not found on PATH"))?;
    let ticket = std::env::var("SUPERAGENT_TICKET").ok().filter(|t| !t.is_empty());
    let worktree = std::env::var("SUPERAGENT_WORKTREE")
        .ok()
        .filter(|w| !w.is_empty())
        .map(std::path::PathBuf::from);
    let default = crate::guard::default_branch(worktree.as_deref());
    if let (Some(ticket), Some(reason)) = (&ticket, crate::guard::risk(tool, args, worktree.as_deref(), Some(&default))) {
        let command = format!("{tool} {}", args.join(" "));
        let call = |method: &str, params: Value| ipc::call(&paths.socket_path(), method, params, Duration::from_secs(10));
        // A person approved exactly this command at its guard checkpoint: it runs, once.
        let permitted = call("guard_permit", json!({ "ticket": ticket, "command": command }))
            .ok()
            .and_then(|v| v["permitted"].as_bool())
            .unwrap_or(false);
        if !permitted {
            let held = call(
                "guard_hold",
                json!({ "ticket": ticket, "tool": tool, "command": command, "reason": reason, "assignment": std::env::var("SUPERAGENT_ASSIGNMENT").ok() }),
            );
            match held {
                Ok(_) => eprintln!("superagent guard: `{command}` held back ({reason}). A checkpoint was recorded: end your turn and wait for approval, then run exactly the same command again."),
                Err(e) => eprintln!("superagent guard: `{command}` held back ({reason}); could not record a checkpoint: {e}. Report blocked and end your turn."),
            }
            return Ok(75);
        }
        eprintln!("superagent guard: `{command}` was approved; running it.");
    }
    let error = std::process::Command::new(real).args(args).exec();
    Err(format!("guard: exec {tool}: {error}"))
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
        .and_then(|a| {
            // The ticket's own worker first; a spawned child's report only when it has none yet.
            a.iter()
                .rev()
                .filter(|x| x["parent"].is_null())
                .find_map(|x| x.get("report").filter(|r| !r.is_null()))
                .or_else(|| a.iter().rev().find_map(|x| x.get("report").filter(|r| !r.is_null())))
        })
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
    if let Some(b) = ticket.pointer("/execution/branch").and_then(Value::as_str) {
        lines.push(format!(
            "branch: {b}  (worktree {})",
            ticket.pointer("/execution/directory").and_then(Value::as_str).unwrap_or("")
        ));
    }
    if let Some(base) = ticket.pointer("/execution/base").and_then(Value::as_str) {
        lines.push(format!(
            "base: {base}  ({})",
            ticket
                .pointer("/execution/base_reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
        ));
    }
    if let Some(pack) = ticket["pack"].as_str() {
        let required: Vec<&str> = ticket["required_checkpoints"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let approved: Vec<&str> = ticket["approved_checkpoints"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let marks: Vec<String> = required
            .iter()
            .map(|name| format!("{}{name}", if approved.contains(name) { "✓ " } else { "· " }))
            .collect();
        lines.push(format!(
            "pack: {pack}{}",
            if marks.is_empty() {
                String::new()
            } else {
                format!("   checkpoints: {}", marks.join("  "))
            }
        ));
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
