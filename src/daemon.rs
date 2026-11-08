//! The daemon: one per profile, in a pane of the superagent workspace. It owns
//! the engine and the store, answers the CLI over its own socket, drains the
//! effect queue, and reports its state to herdr with a resume command so a
//! herdr restart brings it back (PLAN §5.1).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::core::clock;
use crate::core::engine::{Engine, EngineOptions, Question, WorkerReport};
use crate::core::errors::{ErrorCode, UserError};
use crate::core::ids::key;
use crate::core::model::*;
use crate::core::ports::*;
use crate::core::reasons::Reason;
use crate::core::stage::ticket_stage;
use crate::herdr::{self, Herdr};
use crate::paths::Paths;
use crate::profile::{Profile, ProfilePolicy};
use crate::store::sqlite::{Effect, SqliteStore};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const TICK: Duration = Duration::from_secs(2);
const REPORT_REFRESH: Duration = Duration::from_secs(60);

/// The executor port, as a durable effect queue: the engine queues work inside
/// its transaction, the daemon runs it after the commit (prototype `Effects`).
pub struct EffectsExecutor {
    store: Rc<SqliteStore>,
}

impl EffectsExecutor {
    fn enqueue(&self, kind: &str, key: &str, payload: Value) {
        let now = clock::now_iso();
        self.store.effect_enqueue(kind, key, &payload, herdr::seq_now() as i64, &now);
    }
}

impl Executor for EffectsExecutor {
    fn start(&self, ticket: &Ticket, assignment: &Assignment) {
        self.enqueue(
            "launch",
            &format!("{}:{}", ticket.id, assignment.id),
            json!({ "ticket_id": ticket.id, "assignment_id": assignment.id, "role": assignment.role, "mode": ticket.mandate.mode }),
        );
    }
    fn deliver(&self, ticket: &Ticket, assignment: &Assignment) {
        self.enqueue(
            "deliver",
            &ticket.id,
            json!({ "ticket_id": ticket.id, "assignment_id": assignment.id }),
        );
    }
    fn stop(&self, ticket: &Ticket, assignment: &Assignment, reason: &str) {
        self.enqueue(
            "stop",
            &format!("{}:{}", ticket.id, assignment.id),
            json!({ "ticket_id": ticket.id, "assignment_id": assignment.id, "reason": reason }),
        );
    }
}

/// Phase 0 assessor: candidates wait in the store for the superagent session
/// to pick up with `candidates`/`route`. Phase 2 delivers them with `agent.prompt`.
pub struct InboxAssessor;

impl Assessor for InboxAssessor {
    fn request(&self, candidate: &Candidate) {
        tracing::info!(eval = %candidate.eval_id, request = %candidate.request.id, "candidate waits for routing");
    }
}

#[derive(Default)]
struct Flags {
    shutdown: Cell<bool>,
    restart: Cell<bool>,
}

struct Daemon {
    paths: Paths,
    engine: Engine,
    store: Rc<SqliteStore>,
    policy: Rc<ProfilePolicy>,
    herdr: Option<Herdr>,
    pane_id: Option<String>,
    flags: Flags,
    started_at: String,
    last_report: RefCell<(String, Instant)>,
    handlers: HashMap<&'static str, EffectHandler>,
}

/// Runs one due effect; `Err` schedules a retry with backoff.
type EffectHandler = Box<dyn Fn(&Daemon, &Effect) -> Result<(), String>>;

type Handler = Result<Value, UserError>;

pub fn run(profile_name: &str) -> Result<(), String> {
    let paths = Paths::for_profile(profile_name);
    paths.ensure_dirs().map_err(|e| format!("create profile directories: {e}"))?;
    let profile = Profile::load_or_init(&paths.profile_file())?;

    // One daemon per profile: a live socket is the lock.
    if let Ok(ping) = crate::ipc::ping(&paths.socket_path()) {
        return Err(format!(
            "a daemon for profile '{profile_name}' already runs (pid {}, version {})",
            ping["pid"], ping["version"]
        ));
    }
    let _ = std::fs::remove_file(paths.socket_path());

    let store = Rc::new(SqliteStore::open(&paths.db_path()).map_err(|e| e.to_string())?);
    let policy = Rc::new(ProfilePolicy::new(profile));
    let executor = Rc::new(EffectsExecutor { store: store.clone() });
    let engine = Engine::new(EngineOptions {
        repo: store.clone(),
        policy: policy.clone(),
        assessor: Rc::new(InboxAssessor),
        executor,
        now: Rc::new(clock::now_iso),
        emit: Some(Rc::new(
            |event: DomainEvent| tracing::info!(event = %serde_json::to_string(&event).unwrap_or_default(), "domain event"),
        )),
    });
    let herdr = match Herdr::from_env() {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(%e, "running outside herdr: no pane registration");
            None
        }
    };
    let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|v| !v.is_empty());
    let daemon = Daemon {
        paths,
        engine,
        store,
        policy,
        herdr,
        pane_id,
        flags: Flags::default(),
        started_at: clock::now_iso(),
        last_report: RefCell::new((String::new(), Instant::now() - REPORT_REFRESH)),
        handlers: HashMap::new(),
    };
    daemon.write_info();
    match daemon.store.kv_get("daemon.version") {
        Some(previous) if previous != VERSION => tracing::info!(%previous, current = VERSION, "daemon binary changed since the last run"),
        _ => {}
    }
    daemon.store.kv_set("daemon.version", VERSION);
    daemon.register(true);
    daemon.recover();
    daemon.serve()?;
    daemon.finish()
}

impl Daemon {
    fn write_info(&self) {
        let info = json!({
            "pid": std::process::id(),
            "version": VERSION,
            "profile": self.paths.profile,
            "socket": self.paths.socket_path(),
            "pane_id": self.pane_id,
            "workspace_id": std::env::var("HERDR_WORKSPACE_ID").ok(),
            "started_at": self.started_at,
        });
        if let Err(e) = std::fs::write(self.paths.daemon_info_path(), info.to_string()) {
            tracing::warn!(%e, "could not write daemon.json");
        }
    }

    /// The command herdr runs in this pane after a restart: a plain command
    /// name on PATH, as `resume_argv` requires.
    fn resume_argv(&self) -> Option<Vec<String>> {
        on_path("herdr-superagent").then(|| {
            vec![
                "herdr-superagent".to_string(),
                "daemon".to_string(),
                "--profile".to_string(),
                self.paths.profile.clone(),
            ]
        })
    }

    /// Reports state (and the resume command) to herdr for the daemon's pane.
    fn register(&self, force: bool) {
        let (Some(herdr), Some(pane)) = (&self.herdr, &self.pane_id) else {
            return;
        };
        let (state, message) = self.state_summary();
        {
            let last = self.last_report.borrow();
            if !force && last.0 == message && last.1.elapsed() < REPORT_REFRESH {
                return;
            }
        }
        let resume = self.resume_argv();
        if resume.is_none() {
            tracing::warn!("herdr-superagent is not on PATH: herdr cannot resume the daemon after a restart");
        }
        if let Err(e) = herdr.report_agent(pane, state, &message, herdr::seq_now(), resume.as_deref()) {
            tracing::warn!(%e, "pane.report_agent failed");
        } else if force {
            let title = format!("daemon · {}", self.paths.profile);
            let _ = herdr.report_pane_metadata(pane, Some(&title), &[("sa_role", "daemon"), ("sa_profile", &self.paths.profile)]);
        }
        *self.last_report.borrow_mut() = (message, Instant::now());
    }

    /// `blocked` when a ticket needs you, `working` when one works, else `idle`.
    fn state_summary(&self) -> (&'static str, String) {
        let tickets = self.store.ticket_open();
        let needs_you = tickets.iter().filter(|t| ticket_stage(t).needs_person()).count();
        let working = tickets
            .iter()
            .filter(|t| matches!(ticket_stage(t), crate::core::stage::TicketStage::Working { .. }))
            .count();
        let state = if needs_you > 0 {
            "blocked"
        } else if working > 0 {
            "working"
        } else {
            "idle"
        };
        let mut message = format!("{} · {} open", self.paths.profile, tickets.len());
        if needs_you > 0 {
            message.push_str(&format!(" · {needs_you} need you"));
        }
        (state, message)
    }

    /// Assignments that were running when the daemon last stopped.
    fn recover(&self) {
        for ticket in self.store.ticket_active() {
            if let Some(running) = ticket.running_assignment() {
                if let Err(e) = self.engine.assignment_interrupted(&ticket.id, &running.id) {
                    tracing::warn!(ticket = %ticket.id, %e, "recovery failed");
                }
            }
        }
    }

    #[cfg(unix)]
    fn serve(&self) -> Result<(), String> {
        use std::os::unix::net::UnixListener;
        let listener =
            UnixListener::bind(self.paths.socket_path()).map_err(|e| format!("bind {}: {e}", self.paths.socket_path().display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(self.paths.socket_path(), std::fs::Permissions::from_mode(0o600));
        }
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        tracing::info!(profile = %self.paths.profile, version = VERSION, socket = %self.paths.socket_path().display(), "superagent daemon ready");
        let mut last_tick = Instant::now() - TICK;
        loop {
            match listener.accept() {
                Ok((stream, _)) => self.handle_connection(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => tracing::warn!(%e, "accept failed"),
            }
            if self.flags.shutdown.get() {
                return Ok(());
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                self.tick();
            }
        }
    }

    #[cfg(not(unix))]
    fn serve(&self) -> Result<(), String> {
        Err("the daemon needs a Unix socket; Windows support is planned".into())
    }

    #[cfg(unix)]
    fn handle_connection(&self, stream: std::os::unix::net::UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            return;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let method = request.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let params = request.get("params").cloned().unwrap_or(json!({}));
                match self.dispatch(&method, &params) {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err(error) => {
                        json!({ "id": id, "error": { "code": error.code_name(), "message": error.message(), "missing": error.missing() } })
                    }
                }
            }
            Err(e) => json!({ "id": null, "error": { "code": "invalid_request", "message": e.to_string() } }),
        };
        let mut stream = stream;
        let _ = stream.write_all(format!("{response}\n").as_bytes());
    }

    fn tick(&self) {
        if let Err(e) = self.engine.evaluate_due() {
            tracing::warn!(%e, "evaluate_due failed");
        }
        self.drain_effects();
        self.register(false);
    }

    /// Runs due effects of every kind that has a handler. Kinds without one
    /// (worker launch, delivery and stop until phase 2) stay queued.
    fn drain_effects(&self) {
        let now_ms = herdr::seq_now() as i64;
        for (kind, handler) in &self.handlers {
            for effect in self.store.effects_due(kind, now_ms, 8) {
                match handler(self, &effect) {
                    Ok(()) => {
                        self.store.effect_done(kind, &effect.key, effect.generation);
                    }
                    Err(error) => {
                        let backoff = (2i64.pow(effect.attempts.min(8) as u32) * 1000).min(300_000);
                        tracing::warn!(kind, key = %effect.key, %error, "effect failed");
                        self.store.effect_failed(kind, &effect.key, &error, now_ms + backoff);
                    }
                }
            }
        }
    }

    fn finish(&self) -> Result<(), String> {
        let _ = std::fs::remove_file(self.paths.socket_path());
        if self.flags.restart.get() {
            tracing::info!("restarting into the binary on PATH");
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let program = if on_path("herdr-superagent") {
                    std::path::PathBuf::from("herdr-superagent")
                } else {
                    std::env::current_exe().map_err(|e| e.to_string())?
                };
                let error = std::process::Command::new(program)
                    .args(["daemon", "--profile", &self.paths.profile])
                    .exec();
                return Err(format!("exec failed: {error}"));
            }
        }
        if let (Some(herdr), Some(pane)) = (&self.herdr, &self.pane_id) {
            let _ = herdr.release_agent(pane, herdr::seq_now());
        }
        tracing::info!("superagent daemon stopped");
        Ok(())
    }

    // ---- IPC methods ----

    fn dispatch(&self, method: &str, params: &Value) -> Handler {
        match method {
            "ping" => Ok(
                json!({ "version": VERSION, "pid": std::process::id(), "profile": self.paths.profile, "pane_id": self.pane_id, "started_at": self.started_at }),
            ),
            "status" => self.status(),
            "shutdown" => {
                self.flags.shutdown.set(true);
                self.flags
                    .restart
                    .set(params.get("restart").and_then(Value::as_bool).unwrap_or(false));
                Ok(json!({ "stopping": true, "restart": self.flags.restart.get() }))
            }
            "submit" => self.submit(params),
            "candidates" => self.candidates(),
            "decide" => self.decide(params),
            "reevaluate" => Ok(json!({ "queued": self.engine.reevaluate(str_param(params, "request_id")?)? })),
            "tickets" => {
                let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                let tickets = if all { self.store.ticket_list() } else { self.store.ticket_open() };
                Ok(Value::Array(tickets.iter().map(ticket_view).collect()))
            }
            "ticket" => Ok(ticket_view(&self.engine_ticket(str_param(params, "ticket")?)?)),
            "report" => {
                let ticket = str_param(params, "ticket")?;
                let state: ReportState = str_param(params, "state")?.parse().map_err(|e: String| UserError::internal(e))?;
                let report = WorkerReport {
                    state,
                    summary: params.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
                    question: Question::from_option(params.get("question").and_then(Value::as_str).map(str::to_string)),
                };
                Ok(ticket_view(&self.engine.report(ticket, report)?))
            }
            "checkpoint" => Ok(ticket_view(&self.engine.checkpoint(
                str_param(params, "ticket")?,
                str_param(params, "name")?,
                params.get("summary").and_then(Value::as_str).unwrap_or(""),
            )?)),
            "approve" => Ok(ticket_view(
                &self.engine.approve(str_param(params, "ticket")?, opt_param(params, "comment"))?,
            )),
            "stop_ticket" => Ok(ticket_view(
                &self.engine.stop_ticket(str_param(params, "ticket")?, opt_param(params, "reason"))?,
            )),
            "retry" => Ok(ticket_view(&self.engine.retry_ticket(str_param(params, "ticket")?)?)),
            "note" => Ok(ticket_view(
                &self.engine.note(str_param(params, "ticket")?, str_param(params, "text")?)?,
            )),
            "profile" => Ok(serde_json::to_value(self.policy.snapshot())?),
            other => Err(UserError::internal(format!("unknown method '{other}'"))),
        }
    }

    fn engine_ticket(&self, id: &str) -> Result<Ticket, UserError> {
        self.store
            .ticket_get(id)
            .ok_or_else(|| UserError::new(ErrorCode::UnknownTicket).with("ticket", id))
    }

    fn status(&self) -> Handler {
        let tickets = self.store.ticket_list();
        let mut stages: HashMap<String, usize> = HashMap::new();
        for ticket in &tickets {
            *stages.entry(ticket_stage(ticket).label()).or_default() += 1;
        }
        let (open, total) = self.store.ticket_count();
        let effects: Vec<Value> = self
            .store
            .effect_kinds_pending()
            .into_iter()
            .map(|(k, n)| json!({ "kind": k, "pending": n }))
            .collect();
        Ok(json!({
            "profile": self.paths.profile,
            "version": VERSION,
            "pid": std::process::id(),
            "started_at": self.started_at,
            "paused": self.policy.paused(),
            "policy_version": self.policy.version(),
            "herdr": self.herdr.is_some(),
            "pane_id": self.pane_id,
            "tickets": { "open": open, "total": total },
            "needs_you": tickets.iter().filter(|t| ticket_stage(t).needs_person()).count(),
            "stages": stages,
            "candidates": self.store.eval_list().len(),
            "effects": effects,
        }))
    }

    fn submit(&self, params: &Value) -> Handler {
        let text = str_param(params, "text")?.trim().to_string();
        if text.is_empty() {
            return Err(UserError::internal("submit needs text"));
        }
        let now = clock::now_iso();
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| key(&["request", &text, &now]));
        let workflow = opt_param(params, "workflow");
        let mode = match opt_param(params, "mode") {
            Some(m) => Some(m.parse::<Mode>().map_err(UserError::internal)?),
            None => None,
        };
        let request = Request {
            id: id.clone(),
            source: opt_param(params, "source").unwrap_or_else(|| "you".into()),
            actor: opt_param(params, "actor").unwrap_or_else(|| "you".into()),
            human: params.get("human").and_then(Value::as_bool).unwrap_or(true),
            at: now,
            text: text.clone(),
            workflow: workflow.clone(),
            mode,
            project: opt_param(params, "project"),
            meta: params.get("meta").cloned().unwrap_or(Value::Null),
        };
        let received = self.engine.receive(request.clone())?;
        let mut result = json!({ "request_id": id, "received": format!("{received:?}").to_lowercase() });
        // A request that names its workflow skips routing (PLAN §8).
        if let Some(workflow) = workflow {
            let title: String = text.lines().next().unwrap_or("").chars().take(72).collect();
            let outcome = Outcome::Accept {
                title,
                scope: "explicit".into(),
                reason: Reason::code("explicitRouted"),
                rules: Vec::new(),
                workflow: Some(workflow),
                mode,
                name: None,
                note: None,
                audit: None,
            };
            let decision = self.engine.decide(&id, DecisionInput { eval_id: None, outcome })?;
            result["ticket_id"] = json!(decision.ticket_id);
            result["decision_id"] = json!(decision.id);
        } else if let Some(eval) = self.store.eval_by_request(&id) {
            result["eval_id"] = json!(eval.id);
        }
        self.register(true);
        Ok(result)
    }

    fn candidates(&self) -> Handler {
        let list: Vec<Value> = self
            .store
            .eval_list()
            .iter()
            .filter_map(|eval| self.engine.candidate(&eval.request_id))
            .map(|candidate| {
                json!({
                    "eval_id": candidate.eval_id,
                    "request": candidate.request,
                    "strength": candidate.strength,
                    "policy_version": candidate.policy_version,
                    "tickets": candidate.tickets.iter().map(|t| json!({ "id": t.id, "title": t.title, "stage": ticket_stage(t).label() })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(Value::Array(list))
    }

    fn decide(&self, params: &Value) -> Handler {
        let request_id = str_param(params, "request_id")?;
        let eval_id = opt_param(params, "eval_id");
        let outcome: Outcome = serde_json::from_value(
            params
                .get("outcome")
                .cloned()
                .ok_or_else(|| UserError::internal("decide needs an outcome"))?,
        )?;
        if let Some(eval) = self.store.eval_by_request(request_id) {
            self.engine.candidate_delivered(&eval.id)?;
        }
        let decision = self.engine.decide(request_id, DecisionInput { eval_id, outcome })?;
        self.register(true);
        Ok(serde_json::to_value(decision)?)
    }
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, UserError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| UserError::internal(format!("missing parameter '{name}'")))
}

fn opt_param(params: &Value, name: &str) -> Option<String> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// A ticket with its stage label, for the CLI and the dashboard.
pub fn ticket_view(ticket: &Ticket) -> Value {
    let mut value = serde_json::to_value(ticket).unwrap_or(Value::Null);
    value["stage"] = Value::String(ticket_stage(ticket).label());
    value["mode"] = Value::String(ticket.mandate.mode.to_string());
    value
}

/// Whether `name` resolves to an executable on PATH.
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}
