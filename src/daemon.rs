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
use crate::staterepo::{StateRepo, RENEW_SECONDS};
use crate::store::sqlite::{Effect, SqliteStore};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Commit and build time of this binary: `ensure` restarts a daemon whose build differs.
pub const BUILD: &str = env!("HERDR_SUPERAGENT_BUILD");
const TICK: Duration = Duration::from_secs(2);
const REPORT_REFRESH: Duration = Duration::from_secs(60);
/// Commits to the state repo are batched (PLAN §8a: at most once a minute).
const COMMIT_INTERVAL: Duration = Duration::from_secs(60);

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
pub(crate) struct Flags {
    shutdown: Cell<bool>,
    restart: Cell<bool>,
}

pub(crate) struct Daemon {
    pub(crate) paths: Paths,
    pub(crate) engine: Engine,
    pub(crate) store: Rc<SqliteStore>,
    pub(crate) policy: Rc<ProfilePolicy>,
    pub(crate) herdr: Option<Herdr>,
    pub(crate) pane_id: Option<String>,
    pub(crate) flags: Flags,
    pub(crate) started_at: String,
    pub(crate) last_report: RefCell<(String, Instant)>,
    pub(crate) handlers: HashMap<&'static str, EffectHandler>,
    /// Domain events since the last tick, for the ticket records.
    pub(crate) events: Rc<RefCell<Vec<DomainEvent>>>,
    pub(crate) state_repo: RefCell<Option<StateRepo>>,
    pub(crate) dirty_tickets: RefCell<std::collections::BTreeSet<String>>,
    pub(crate) last_commit: Cell<Instant>,
    pub(crate) last_renew: Cell<Instant>,
    pub(crate) last_tidy: Cell<Instant>,
    /// Finished source polls come back here from their threads.
    pub(crate) polled: (
        std::sync::mpsc::Sender<crate::sources::Polled>,
        std::sync::mpsc::Receiver<crate::sources::Polled>,
    ),
    pub(crate) needs_you_seen: Cell<usize>,
    pub(crate) profile_mtime: Cell<Option<std::time::SystemTime>>,
    /// What the observer last saw of each worker pane.
    pub(crate) workers: RefCell<HashMap<String, crate::executor::WorkerSeen>>,
}

/// Runs one due effect: done, deferred (not ready yet), or failed (retry with backoff).
type EffectHandler = Box<dyn Fn(&Daemon, &Effect) -> crate::executor::Outcome>;

type Handler = Result<Value, UserError>;

pub fn run(profile_name: &str) -> Result<(), String> {
    let paths = Paths::for_profile(profile_name);
    paths.ensure_dirs().map_err(|e| format!("create profile directories: {e}"))?;
    let profile = Profile::load_or_init(&paths.profile_file())?;

    // One daemon per profile: a live socket is the lock (a failed ping here is the expected answer).
    tracing::debug!(target: "daemon", "lock check: pinging an earlier daemon on this profile's socket");
    if let Ok(ping) = crate::ipc::ping(&paths.socket_path()) {
        return Err(format!(
            "a daemon for profile '{profile_name}' already runs (pid {}, version {})",
            ping["pid"], ping["version"]
        ));
    }
    let _ = std::fs::remove_file(paths.socket_path());

    let fresh_db = !paths.db_path().exists();
    let store = Rc::new(SqliteStore::open(&paths.db_path()).map_err(|e| e.to_string())?);
    let state_repo = StateRepo::open_or_init(&profile, &paths.profile_dir())?;
    if let Some(repo) = &state_repo {
        // `start` takes the lease before launching the daemon; a daemon that
        // finds another live machine on it must not run.
        let me = crate::staterepo::machine_name();
        if let Some(lease) = repo.lease() {
            if lease.machine != me && lease.expires > clock::now_iso() {
                return Err(format!(
                    "profile '{profile_name}' is running on '{}' (lease until {}); use `herdr-superagent start --take-over` to move it here",
                    lease.machine, lease.expires
                ));
            }
        }
        if let Err(e) = repo.acquire(false) {
            tracing::warn!(%e, "could not take the lease");
        }
    }
    if let (true, Some(repo)) = (fresh_db, &state_repo) {
        let memories = repo.reindex_memory(store.as_ref());
        match repo.reindex(store.as_ref()) {
            Ok(n) => tracing::info!(tickets = n, memories, "rebuilt the database from the state repo"),
            Err(e) => tracing::warn!(%e, "reindex failed"),
        }
    }
    let policy = Rc::new(ProfilePolicy::new(profile, &paths.profile_dir()));
    let executor = Rc::new(EffectsExecutor { store: store.clone() });
    let events: Rc<RefCell<Vec<DomainEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = events.clone();
    let engine = Engine::new(EngineOptions {
        repo: store.clone(),
        policy: policy.clone(),
        assessor: Rc::new(InboxAssessor),
        executor,
        now: Rc::new(clock::now_iso),
        emit: Some(Rc::new(move |event: DomainEvent| {
            tracing::info!(event = %serde_json::to_string(&event).unwrap_or_default(), "domain event");
            sink.borrow_mut().push(event);
        })),
    });
    let herdr = match Herdr::from_env() {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(%e, "running outside herdr: no pane registration");
            None
        }
    };
    let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|v| !v.is_empty());
    let profile_mtime = std::fs::metadata(paths.profile_file()).and_then(|m| m.modified()).ok();
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
        events,
        state_repo: RefCell::new(state_repo),
        dirty_tickets: RefCell::new(std::collections::BTreeSet::new()),
        last_commit: Cell::new(Instant::now()),
        last_renew: Cell::new(Instant::now()),
        last_tidy: Cell::new(Instant::now()),
        polled: std::sync::mpsc::channel(),
        needs_you_seen: Cell::new(0),
        profile_mtime: Cell::new(profile_mtime),
        workers: RefCell::new(HashMap::new()),
    };
    let mut daemon = daemon;
    daemon.install_handlers();
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
        // A periodic refresh also re-applies the cockpit's titles and tokens, in case they were cleared.
        let force = force || self.last_report.borrow().1.elapsed() >= REPORT_REFRESH;
        {}
        let resume = self.resume_argv();
        if resume.is_none() {
            tracing::warn!("herdr-superagent is not on PATH: herdr cannot resume the daemon after a restart");
        }
        tracing::debug!(target: "daemon", pane, state, %message, resume = ?resume, "reporting state to herdr");
        if let Err(e) = herdr.report_agent(pane, state, &message, herdr::seq_now(), resume.as_deref()) {
            tracing::warn!(%e, "pane.report_agent failed");
        } else if force {
            let title = format!("daemon · {}", self.paths.profile);
            let _ = herdr.report_pane_metadata(pane, Some(&title), &[("sa_role", "daemon"), ("sa_profile", &self.paths.profile)]);
            if let Some(session) = self.store.kv_get("session.pane_id").filter(|p| !p.is_empty()) {
                let title = format!("◆ SUPERAGENT · {}", self.paths.profile);
                let _ = herdr.report_pane_metadata(
                    &session,
                    Some(&title),
                    &[("sa_role", "superagent"), ("sa_profile", &self.paths.profile)],
                );
            }
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
        // `blocked` would read as a stuck daemon: the count goes in the message and a toast instead.
        let state = if working > 0 { "working" } else { "idle" };
        let previous = self.needs_you_seen.replace(needs_you);
        if needs_you > previous {
            if let Some(herdr) = &self.herdr {
                let _ = herdr.notify(
                    &format!("superagent · {}: {needs_you} need you", self.paths.profile),
                    Some("open the cockpit to review"),
                );
            }
        }
        let mut message = format!("{} · {} open", self.paths.profile, tickets.len());
        if needs_you > 0 {
            message.push_str(&format!(" · {needs_you} need you"));
        }
        (state, message)
    }

    /// Assignments that were running when the daemon last stopped. A pane that
    /// still exists with its agent is re-attached (herdr restores
    /// conversations); otherwise the engine restarts the assignment.
    fn recover(&self) {
        let panes = self.herdr.as_ref().and_then(|h| h.pane_list().ok()).unwrap_or_default();
        for ticket in self.store.ticket_active() {
            if let Some(running) = ticket.running_assignment() {
                let alive = running
                    .pane_id
                    .as_ref()
                    .and_then(|id| panes.iter().find(|p| &p.pane_id == id))
                    .map(|p| p.agent.is_some())
                    .unwrap_or(false);
                if alive {
                    tracing::info!(ticket = %ticket.id, pane = ?running.pane_id, "re-attached to a live worker pane");
                    if !ticket.undelivered.is_empty() {
                        self.store.effect_enqueue(
                            "deliver",
                            &ticket.id,
                            &json!({ "ticket_id": ticket.id, "assignment_id": running.id }),
                            herdr::seq_now() as i64,
                            &clock::now_iso(),
                        );
                    }
                    continue;
                }
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
        tracing::info!(profile = %self.paths.profile, version = VERSION, build = BUILD, socket = %self.paths.socket_path().display(), "superagent daemon ready");
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
        // On macOS an accepted connection inherits the listener's non-blocking mode: a request
        // larger than one socket buffer (a memory import) then fails mid-read.
        let _ = stream.set_nonblocking(false);
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
                let started = Instant::now();
                let outcome = self.dispatch(&method, &params);
                let ms = started.elapsed().as_millis();
                match &outcome {
                    Ok(result) => tracing::debug!(target: "daemon", method = %method, ms, params = %params, result = %result, "request ok"),
                    Err(error) => tracing::info!(target: "daemon", method = %method, ms, params = %params, %error, "request refused"),
                }
                match outcome {
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
        self.route_unrouted();
        if !self.policy.paused() {
            self.poll_sources(&self.polled.0, None);
        }
        while let Ok(polled) = self.polled.1.try_recv() {
            self.sources_polled(polled);
        }
        self.drain_effects();
        self.observe_workers();
        self.embed_backlog();
        self.announce();
        self.auto_tidy();
        self.watch_profile();
        self.flush_records();
        self.register(false);
    }

    /// Closes the workspaces of tickets that ended `limits.tidy_after_minutes` ago (PLAN §8b).
    fn auto_tidy(&self) {
        let minutes = self.policy.snapshot().limits.tidy_after_minutes;
        let Some(herdr) = &self.herdr else { return };
        if minutes == 0 || self.last_tidy.get().elapsed() < Duration::from_secs(60) {
            return;
        }
        self.last_tidy.set(Instant::now());
        let cutoff = clock::plus_ms(&clock::now_iso(), -(minutes as i64) * 60_000);
        let Ok(open) = herdr.workspace_list() else { return };
        for ticket in self.store.ticket_list() {
            let (Some(ended), Some(execution)) = (&ticket.ended, &ticket.execution) else {
                continue;
            };
            let Some(workspace) = &execution.workspace_id else { continue };
            if ended.at > cutoff || !open.iter().any(|w| &w.workspace_id == workspace) {
                continue;
            }
            match herdr.call("workspace.close", json!({ "workspace_id": workspace })) {
                Ok(_) => {
                    tracing::info!(ticket = %ticket.id, workspace, "closed the ended ticket's workspace");
                    if let Some(repo) = ticket.project.as_deref() {
                        if let Some(note) = crate::gitbase::remove_worktree(&crate::profile::expand_home(repo), &execution.directory) {
                            tracing::info!(ticket = %ticket.id, %note, "worktree kept");
                        }
                    }
                }
                Err(e) => tracing::debug!(ticket = %ticket.id, %e, "tidy skipped"),
            }
        }
    }

    /// `profile set` from another process: reload and tell the engine.
    fn watch_profile(&self) {
        let mtime = std::fs::metadata(self.paths.profile_file()).and_then(|m| m.modified()).ok();
        if mtime == self.profile_mtime.get() {
            return;
        }
        self.profile_mtime.set(mtime);
        if let Err(e) = self.reload_profile() {
            tracing::warn!(%e, "profile reload failed");
        }
    }

    fn reload_profile(&self) -> Result<Value, UserError> {
        let before = self.policy.snapshot();
        let after = Profile::load_or_init(&self.paths.profile_file()).map_err(UserError::internal)?;
        let (from, to) = (before.version(), after.version());
        if from == to {
            return Ok(json!({ "changed": false, "version": to }));
        }
        // Tickets whose mode the new rules no longer allow are told so.
        let mut effects = HashMap::new();
        for ticket in self.store.ticket_open() {
            if ticket.mandate.mode > after.modes.cap {
                effects.insert(
                    ticket.id.clone(),
                    PolicyEffect {
                        rules: Vec::new(),
                        proactive_withdrawn: true,
                    },
                );
            }
        }
        self.policy.replace(after.clone());
        self.engine.policy_changed(&from, &to, &effects)?;
        tracing::info!(%from, %to, tickets = effects.len(), "profile reloaded");
        if !before.calibrated && after.calibrated {
            self.switch_to_routing();
        }
        if self.state_repo.borrow().is_none() {
            if let Ok(repo) = StateRepo::open_or_init(&after, &self.paths.profile_dir()) {
                *self.state_repo.borrow_mut() = repo;
            }
        }
        Ok(json!({ "changed": true, "from": from, "to": to, "tickets_told": effects.len() }))
    }

    /// Ticket records and events go to the state repo; commits are batched.
    fn flush_records(&self) {
        let events: Vec<DomainEvent> = std::mem::take(&mut *self.events.borrow_mut());
        let repo = self.state_repo.borrow();
        let Some(repo) = repo.as_ref() else { return };
        for event in &events {
            let ticket_id = match event {
                DomainEvent::TicketCreated { ticket_id, .. }
                | DomainEvent::TicketResumed { ticket_id, .. }
                | DomainEvent::TicketEnded { ticket_id, .. }
                | DomainEvent::AssignmentStarted { ticket_id, .. }
                | DomainEvent::AssignmentEnded { ticket_id, .. }
                | DomainEvent::CheckpointReached { ticket_id, .. }
                | DomainEvent::CheckpointApproved { ticket_id, .. }
                | DomainEvent::ChangesRequested { ticket_id, .. }
                | DomainEvent::Reported { ticket_id, .. }
                | DomainEvent::InputAdded { ticket_id, .. }
                | DomainEvent::Retitled { ticket_id, .. } => Some(ticket_id.clone()),
                _ => None,
            };
            if let Some(id) = ticket_id {
                if let Some(ticket) = self.store.ticket_get(&id) {
                    if let Err(e) = repo.append_event(&ticket, event) {
                        tracing::warn!(%e, "could not append event");
                    }
                }
                self.dirty_tickets.borrow_mut().insert(id);
            }
        }
        let dirty: Vec<String> = std::mem::take(&mut *self.dirty_tickets.borrow_mut()).into_iter().collect();
        for id in dirty {
            if let Some(ticket) = self.store.ticket_get(&id) {
                if let Err(e) = repo.write_ticket(&ticket) {
                    tracing::warn!(%e, ticket = %id, "could not write ticket record");
                }
            }
        }
        if self.last_commit.get().elapsed() >= COMMIT_INTERVAL {
            self.last_commit.set(Instant::now());
            // Reports and notes change tickets without a lifecycle event: refresh the open ones.
            for ticket in self.store.ticket_open() {
                let _ = repo.write_ticket(&ticket);
            }
            self.sync_repo(repo, "superagent: ticket records");
        }
        if self.last_renew.get().elapsed() >= Duration::from_secs(RENEW_SECONDS as u64) {
            self.last_renew.set(Instant::now());
            if let Err(e) = repo.renew() {
                tracing::error!(%e, "lease renew failed: another machine may hold this profile");
                if let Some(herdr) = &self.herdr {
                    let _ = herdr.notify("superagent lease lost", Some(&e.to_string()));
                }
            }
        }
    }

    fn sync_repo(&self, repo: &StateRepo, message: &str) -> Value {
        let committed = match repo.commit(message) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%e, "state repo commit failed");
                return json!({ "committed": false, "error": e });
            }
        };
        let pushed = match repo.push() {
            Ok(result) => format!("{result:?}").to_lowercase(),
            Err(e) => {
                tracing::warn!(%e, "state repo push failed");
                e
            }
        };
        json!({ "committed": committed, "push": pushed })
    }

    /// Runs due effects of every kind that has a handler. Kinds without one
    /// (worker launch, delivery and stop until phase 2) stay queued.
    fn drain_effects(&self) {
        let now_ms = herdr::seq_now() as i64;
        for (kind, handler) in &self.handlers {
            for effect in self.store.effects_due(kind, now_ms, 8) {
                tracing::debug!(target: "daemon", kind, key = %effect.key, attempts = effect.attempts, payload = %effect.payload, "running effect");
                match handler(self, &effect) {
                    crate::executor::Outcome::Done => {
                        self.store.effect_done(kind, &effect.key, effect.generation);
                    }
                    crate::executor::Outcome::Defer(ms) => {
                        self.store.effect_defer(kind, &effect.key, now_ms + ms);
                    }
                    crate::executor::Outcome::Fail(error) => {
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
        self.flush_records();
        if let Some(repo) = self.state_repo.borrow().as_ref() {
            for ticket in self.store.ticket_list() {
                let _ = repo.write_ticket(&ticket);
            }
            self.sync_repo(repo, "superagent: stop");
            if !self.flags.restart.get() {
                if let Err(e) = repo.release() {
                    tracing::warn!(%e, "lease release failed");
                }
            }
        }
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
                json!({ "version": VERSION, "build": BUILD, "pid": std::process::id(), "profile": self.paths.profile, "pane_id": self.pane_id, "started_at": self.started_at }),
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
                    assignment_id: opt_param(params, "assignment"),
                };
                Ok(ticket_view(&self.engine.report(ticket, report)?))
            }
            "spawn" => {
                let ticket = str_param(params, "ticket")?;
                let parent = match opt_param(params, "parent") {
                    Some(p) => p,
                    None => self
                        .store
                        .ticket_get(ticket)
                        .and_then(|t| t.running_assignment().map(|a| a.id.clone()))
                        .ok_or_else(|| UserError::new(ErrorCode::UnknownAssignment))?,
                };
                let mode = match opt_param(params, "mode") {
                    Some(m) => Some(m.parse::<Mode>().map_err(UserError::internal)?),
                    None => None,
                };
                let child = self
                    .engine
                    .spawn(ticket, &parent, str_param(params, "role")?, str_param(params, "task")?, mode)?;
                Ok(serde_json::to_value(child)?)
            }
            "checkpoint" if opt_param(params, "assignment").is_some() => Ok(ticket_view(&self.engine.checkpoint_as(
                str_param(params, "ticket")?,
                &opt_param(params, "assignment").unwrap_or_default(),
                str_param(params, "name")?,
                params.get("summary").and_then(Value::as_str).unwrap_or(""),
            )?)),
            "checkpoint" => Ok(ticket_view(&self.engine.checkpoint(
                str_param(params, "ticket")?,
                str_param(params, "name")?,
                params.get("summary").and_then(Value::as_str).unwrap_or(""),
            )?)),
            "approve" => {
                let ticket_id = str_param(params, "ticket")?;
                let waiting = self.store.ticket_get(ticket_id).and_then(|t| t.checkpoint.map(|c| c.name));
                let ticket = self.engine.approve(ticket_id, opt_param(params, "comment"))?;
                // An approved guard checkpoint lets exactly the command it held back through, once.
                if waiting.map(|name| name.starts_with("guard-")).unwrap_or(false) {
                    if let Some(held) = self.store.kv_get(&format!("guard.held.{ticket_id}")).filter(|c| !c.is_empty()) {
                        self.store.kv_set(&format!("guard.permit.{ticket_id}"), &held);
                    }
                }
                self.store.kv_set(&format!("guard.held.{ticket_id}"), "");
                Ok(ticket_view(&ticket))
            }
            "guard_hold" => {
                let ticket_id = str_param(params, "ticket")?;
                let command = str_param(params, "command")?;
                let name = format!("guard-{}", str_param(params, "tool")?);
                let summary = format!(
                    "guard held back `{command}`: {}. Approve to let the worker run exactly this command once.",
                    params.get("reason").and_then(Value::as_str).unwrap_or("risky")
                );
                let ticket = match opt_param(params, "assignment") {
                    Some(assignment) => self.engine.checkpoint_as(ticket_id, &assignment, &name, &summary),
                    None => self.engine.checkpoint(ticket_id, &name, &summary),
                }?;
                self.store.kv_set(&format!("guard.held.{ticket_id}"), command);
                Ok(ticket_view(&ticket))
            }
            "guard_permit" => {
                let ticket_id = str_param(params, "ticket")?;
                let key = format!("guard.permit.{ticket_id}");
                let permitted = self.store.kv_get(&key).as_deref() == Some(str_param(params, "command")?);
                if permitted {
                    self.store.kv_set(&key, "");
                }
                Ok(json!({ "permitted": permitted }))
            }
            "stop_ticket" => Ok(ticket_view(
                &self.engine.stop_ticket(str_param(params, "ticket")?, opt_param(params, "reason"))?,
            )),
            "sources" => Ok(self.sources_view()),
            "sources_poll" => {
                let only = opt_param(params, "id");
                if let Some(id) = &only {
                    if !self.policy.snapshot().sources.iter().any(|s| &s.id == id) {
                        return Err(UserError::internal(format!("no source '{id}' in the profile")));
                    }
                }
                let mut started = Vec::new();
                match &only {
                    Some(id) => started.extend(self.poll_sources(&self.polled.0, Some(id))),
                    None => {
                        for source in self.policy.snapshot().sources.iter().filter(|s| s.enabled) {
                            started.extend(self.poll_sources(&self.polled.0, Some(&source.id)));
                        }
                    }
                }
                Ok(json!({ "polling": started }))
            }
            "request_changes" => Ok(ticket_view(
                &self
                    .engine
                    .request_changes(str_param(params, "ticket")?, str_param(params, "comment")?)?,
            )),
            "retitle" => self.retitle(str_param(params, "ticket")?, str_param(params, "title")?),
            "retry" => Ok(ticket_view(&self.engine.retry_ticket(str_param(params, "ticket")?)?)),
            "note" => Ok(ticket_view(
                &self.engine.note(str_param(params, "ticket")?, str_param(params, "text")?)?,
            )),
            "memory_add" => self.memory_add(params),
            "memory_get" => Ok(serde_json::to_value(self.store.memory_get(str_param(params, "id")?))?),
            "memory_search" => self.memory_search(params),
            "import_marks" => Ok(Value::Object(
                self.store
                    .kv_prefix("import.")
                    .into_iter()
                    .map(|(k, v)| (k.trim_start_matches("import.").to_string(), Value::String(v)))
                    .collect(),
            )),
            "memory_import" => self.memory_import(params),
            "profile" => {
                let mut value = serde_json::to_value(self.policy.snapshot())?;
                value["profile_path"] = json!(self.paths.profile_file());
                value["profile_dir"] = json!(self.paths.profile_dir());
                Ok(value)
            }
            "profile_reload" => self.reload_profile(),
            "requests" => {
                // Each request with the mode proposed for it: its ticket's mandate when it started one.
                let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let list = self.store.request_list();
                let start = list.len().saturating_sub(limit);
                let items: Vec<Value> = list[start..]
                    .iter()
                    .map(|request| {
                        let proposed = self
                            .store
                            .disposition_get(&request.id)
                            .and_then(|d| d.decision_id)
                            .and_then(|id| self.store.decision_get(&id))
                            .and_then(|d| d.mandate.map(|m| m.mode))
                            .or(request.mode);
                        json!({ "request": request, "proposed": proposed })
                    })
                    .collect();
                Ok(Value::Array(items))
            }
            "session_set" => {
                let pane = str_param(params, "pane_id")?;
                self.store.kv_set("session.pane_id", pane);
                self.store
                    .kv_set("session.harness", params.get("harness").and_then(Value::as_str).unwrap_or(""));
                if let Some(herdr) = &self.herdr {
                    let title = format!("◆ SUPERAGENT · {}", self.paths.profile);
                    let _ = herdr.report_pane_metadata(
                        pane,
                        Some(&title),
                        &[("sa_role", "superagent"), ("sa_profile", &self.paths.profile)],
                    );
                }
                Ok(json!({ "pane_id": pane }))
            }
            "sync" => match self.state_repo.borrow().as_ref() {
                Some(repo) => {
                    for ticket in self.store.ticket_list() {
                        let _ = repo.write_ticket(&ticket);
                    }
                    self.last_commit.set(Instant::now());
                    Ok(self.sync_repo(repo, params.get("message").and_then(Value::as_str).unwrap_or("superagent: sync")))
                }
                None => Ok(json!({ "committed": false, "push": "no state repo configured" })),
            },
            "lease" => Ok(match self.state_repo.borrow().as_ref() {
                Some(repo) => serde_json::to_value(repo.lease())?,
                None => Value::Null,
            }),
            "reindex" => match self.state_repo.borrow().as_ref() {
                Some(repo) => Ok(
                    json!({ "tickets": repo.reindex(self.store.as_ref()).map_err(UserError::internal)?, "memories": repo.reindex_memory(self.store.as_ref()) }),
                ),
                None => Err(UserError::internal("no state repo configured")),
            },
            other => Err(UserError::internal(format!("unknown method '{other}'"))),
        }
    }

    /// Renames a ticket, its workspace and its record.
    fn retitle(&self, ticket_id: &str, title: &str) -> Handler {
        let mut ticket = self.engine.retitle(ticket_id, title)?;
        let label = crate::executor::workspace_label(&ticket.title, &ticket.id);
        if let Some(execution) = ticket.execution.as_mut() {
            execution.name = label.clone();
            if let (Some(herdr), Some(workspace)) = (&self.herdr, &execution.workspace_id) {
                if let Err(e) = herdr.call("workspace.rename", json!({ "workspace_id": workspace, "label": label })) {
                    tracing::debug!(%e, "workspace.rename not available; the label keeps the old title");
                }
            }
            self.store.ticket_save(&ticket);
        }
        Ok(ticket_view(&ticket))
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
            "build": BUILD,
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
            "workers": { "running": self.store.ticket_open().iter().filter(|t| t.running_assignment().map(|a| a.pane_id.is_some()).unwrap_or(false)).count(), "max": self.policy.snapshot().limits.max_workers },
            "memory": self.store.memory_count(),
            "session_pane_id": self.store.kv_get("session.pane_id"),
            "harness": self.store.kv_get("session.harness").filter(|h| !h.is_empty()).unwrap_or_else(|| self.policy.snapshot().superagent.harness),
            "state_repo": self.policy.snapshot().state_repo.kind,
            "lease": self.state_repo.borrow().as_ref().and_then(|r| r.lease()).map(|l| format!("{} until {}", l.machine, l.expires)),
        }))
    }

    /// The memory project a request names: `project` (a key or a path), else the ticket's repo, else the notes bucket.
    fn memory_project(&self, params: &Value) -> String {
        if let Some(project) = opt_param(params, "project") {
            return crate::memory::project_key(&project).unwrap_or(project);
        }
        if let Some(ticket) = opt_param(params, "ticket").and_then(|id| self.store.ticket_get(&id)) {
            return self.ticket_project(&ticket);
        }
        crate::memory::NOTES_PROJECT.to_string()
    }

    /// The profile's embedder, when semantic memory is on.
    pub(crate) fn embedder(&self) -> Option<crate::embed::Embedder> {
        let memory = self.policy.snapshot().memory;
        memory.semantic.then(|| crate::embed::Embedder::new(&memory))
    }

    /// A few missing vectors per tick, so turning semantic memory on (or importing sessions) never stalls the daemon.
    fn embed_backlog(&self) {
        if let Some(embedder) = self.embedder() {
            let written = self.store.vector_backfill(&embedder, 16);
            if written > 0 {
                tracing::debug!(written, model = %embedder.name, "embedded memory entries");
            }
        }
    }

    fn memory_add(&self, params: &Value) -> Handler {
        let title = str_param(params, "title")?;
        let body = str_param(params, "body")?;
        let project = self.memory_project(params);
        let ticket = opt_param(params, "ticket").and_then(|id| self.store.ticket_get(&id));
        let assignment_id = ticket.as_ref().and_then(|t| t.latest_assignment().map(|a| a.id.clone()));
        let role = opt_param(params, "role").unwrap_or_else(|| "worker".into());
        let entry = crate::memory::now_entry(&project, ticket.as_ref().map(|t| t.id.clone()), assignment_id, &role, title, body);
        let entry = self.store.memory_add(&entry)?;
        let mut file = None;
        if let Some(repo) = self.state_repo.borrow().as_ref() {
            let written = match &ticket {
                Some(ticket) => repo.write_summary(ticket, &entry),
                None => repo.write_note(&entry),
            };
            match written {
                Ok(path) => file = Some(path.display().to_string()),
                Err(e) => tracing::warn!(%e, "could not write the summary file"),
            }
            if let Some(ticket) = &ticket {
                self.dirty_tickets.borrow_mut().insert(ticket.id.clone());
            }
        }
        Ok(json!({ "id": entry.id, "project": entry.project, "ticket_id": entry.ticket_id, "file": file }))
    }

    /// Stores a batch of imported session summaries with the marks of the files they came from.
    fn memory_import(&self, params: &Value) -> Handler {
        let entries: Vec<crate::memory::MemoryEntry> = serde_json::from_value(params.get("entries").cloned().unwrap_or(json!([])))?;
        let mut stored = 0;
        for entry in &entries {
            let entry = self.store.memory_add(entry)?;
            stored += 1;
            if let Some(repo) = self.state_repo.borrow().as_ref() {
                if let Err(e) = repo.write_session(&entry) {
                    tracing::warn!(%e, "could not write the session summary file");
                }
            }
        }
        if let Some(marks) = params.get("marks").and_then(Value::as_object) {
            for (file, mark) in marks {
                self.store.kv_set(&format!("import.{file}"), mark.as_str().unwrap_or(""));
            }
        }
        Ok(json!({ "stored": stored }))
    }

    fn memory_search(&self, params: &Value) -> Handler {
        let project = self.memory_project(params);
        let query = params.get("query").and_then(Value::as_str).unwrap_or("");
        let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize;
        let embedder = self.embedder();
        let hits = self.store.memory_search_hybrid(&project, query, limit, embedder.as_ref())?;
        Ok(json!({ "project": project, "hits": hits, "semantic": embedder.map(|e| e.name) }))
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
            actor: opt_param(params, "actor")
                .or_else(|| opt_param(params, "source"))
                .unwrap_or_else(|| "you".into()),
            human: params.get("human").and_then(Value::as_bool).unwrap_or(true),
            at: now,
            text: text.clone(),
            workflow: workflow.clone(),
            mode,
            project: opt_param(params, "project"),
            meta: params.get("meta").cloned().unwrap_or(Value::Null),
        };
        let received = self.engine.receive(request.clone())?;
        self.settle_project(&id, None, "");
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
        } else if self.policy.snapshot().routing.routing == "never" {
            // Routing is off: the deterministic scorer picks the workflow at once.
            if let Some(decision) = self.accept_by_scorer(&id, "routingOff")? {
                result["ticket_id"] = json!(decision.ticket_id);
                result["decision_id"] = json!(decision.id);
            }
        } else if let Some(eval) = self.store.eval_by_request(&id) {
            result["eval_id"] = json!(eval.id);
        }
        self.register(true);
        Ok(result)
    }

    /// A ticket that targets a repository must be created with it, or its worker
    /// gets no worktree and reads the person's checkout directly. The project is the
    /// one routing named, else the one the request already has, else the profile
    /// repository the request names.
    fn settle_project(&self, request_id: &str, named: Option<String>, title: &str) {
        let Some(mut request) = self.store.request_get(request_id) else {
            return;
        };
        let project = named
            .map(|p| crate::profile::expand_home(&p))
            .or_else(|| request.project.clone())
            .or_else(|| infer_project(&self.policy.snapshot(), &format!("{title}\n{}", request.text)));
        if project.is_some() && project != request.project {
            tracing::info!(request = %request_id, project = ?project, "request settled on a project");
            request.project = project;
            self.store.request_save(&request);
        }
    }

    /// Accepts a person's own request with the workflow the keyword scorer picks
    /// (PLAN §8): the path when routing is off or the session never answered.
    fn accept_by_scorer(&self, request_id: &str, why: &str) -> Result<Option<Decision>, UserError> {
        let Some(request) = self.store.request_get(request_id) else {
            return Ok(None);
        };
        let Some(eval) = self.store.eval_by_request(request_id) else {
            return Ok(None);
        };
        if eval.strength != Strength::Explicit {
            // Outside text (an issue, a mail) is data: it never becomes work without a judgement.
            return Ok(None);
        }
        let title: String = request.text.lines().next().unwrap_or("").chars().take(72).collect();
        let outcome = Outcome::Accept {
            title,
            scope: "scored".into(),
            reason: Reason::code(why),
            rules: Vec::new(),
            workflow: None,
            mode: request.mode,
            name: None,
            note: None,
            audit: None,
        };
        let decision = self.engine.decide(request_id, DecisionInput { eval_id: None, outcome })?;
        tracing::info!(request = %request_id, ticket = ?decision.ticket_id, why, "accepted by the scorer");
        Ok(Some(decision))
    }

    /// Requests the session did not route in time were recorded `unavailable`: the scorer takes over.
    fn route_unrouted(&self) {
        let unavailable: Vec<String> = self
            .events
            .borrow()
            .iter()
            .filter_map(|event| match event {
                DomainEvent::DecisionRecorded {
                    kind: DecisionKind::Unavailable,
                    request_id,
                    ..
                } => Some(request_id.clone()),
                _ => None,
            })
            .collect();
        // Nobody to route (no superagent session was ever started here): do not let requests wait forever.
        if self.store.kv_get("session.pane_id").filter(|p| !p.is_empty()).is_none() {
            let now = clock::now_iso();
            for eval in self.store.eval_list() {
                if eval.paused_at.is_none() && clock::millis_between(&eval.started_at, &now) >= 20_000 {
                    if let Err(e) = self.accept_by_scorer(&eval.request_id, "noSession") {
                        tracing::warn!(request = %eval.request_id, %e, "scorer routing failed");
                    }
                }
            }
        }
        for request_id in unavailable {
            if self.store.kv_get(&format!("fallback.{request_id}")).is_some() {
                continue;
            }
            self.store.kv_set(&format!("fallback.{request_id}"), &clock::now_iso());
            match self.engine.reevaluate(&request_id) {
                Ok(true) => {
                    if let Err(e) = self.accept_by_scorer(&request_id, "routerUnavailable") {
                        tracing::warn!(request = %request_id, %e, "fallback routing failed");
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(request = %request_id, %e, "could not requeue an unrouted request"),
            }
        }
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
        if let Outcome::Accept { title, .. } = &outcome {
            self.settle_project(request_id, opt_param(params, "project"), title);
        }
        let decision = self.engine.decide(request_id, DecisionInput { eval_id, outcome })?;
        self.register(true);
        Ok(serde_json::to_value(decision)?)
    }
}

/// The one profile repository a text names: by its path, or by its directory
/// name as a whole word. Two candidates mean no answer.
pub fn infer_project(profile: &Profile, text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .filter(|w| !w.is_empty())
        .collect();
    let mut found: Vec<String> = Vec::new();
    for repo in &profile.repos {
        let path = crate::profile::expand_home(&repo.path);
        let name = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        let hit = lower.contains(&path.to_lowercase())
            || lower.contains(&repo.path.to_lowercase())
            || (name.len() >= 3 && words.iter().any(|w| w.trim_matches('.') == name));
        if hit && !found.contains(&path) {
            found.push(path);
        }
    }
    (found.len() == 1).then(|| found.remove(0))
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
