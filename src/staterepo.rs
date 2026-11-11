//! The per-profile state repository (PLAN §8a): plain-text ticket records,
//! the profile itself and a lease, in a git repo that is the profile
//! directory. The SQLite database is a local cache rebuilt by `reindex`.

#[cfg(test)]
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::core::clock;
use crate::core::model::Ticket;
use crate::core::ports::{DomainEvent, Repositories};
use crate::core::stage::ticket_stage;
use crate::profile::Profile;

/// The lease lasts three renew intervals (PLAN §8a).
pub const RENEW_SECONDS: i64 = 5 * 60;
pub const LEASE_SECONDS: i64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub machine: String,
    pub pid: u32,
    pub since: String,
    pub expires: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PushResult {
    Pushed,
    NoRemote,
    /// The remote moved: another machine wrote first.
    Rejected,
}

#[derive(Debug)]
pub enum LeaseError {
    Held(Lease),
    Git(String),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Held(lease) => write!(
                f,
                "profile is running on '{}' (pid {}, lease until {}); stop it there, wait for the lease to expire, or use --take-over",
                lease.machine, lease.pid, lease.expires
            ),
            LeaseError::Git(e) => write!(f, "state repo: {e}"),
        }
    }
}

pub struct StateRepo {
    pub dir: PathBuf,
    pub remote: Option<String>,
    pub branch: String,
}

pub fn machine_name() -> String {
    std::env::var("HERDR_SUPERAGENT_MACHINE")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

impl StateRepo {
    /// The profile's state repo, if its profile asks for one; initialised on first use.
    pub fn open_or_init(profile: &Profile, dir: &Path) -> Result<Option<StateRepo>, String> {
        let remote = match profile.state_repo.kind.as_str() {
            "none" => return Ok(None),
            "local" => None,
            "remote" => Some(profile.state_repo.remote.clone()),
            other => return Err(format!("unknown state_repo.kind '{other}'")),
        };
        let repo = StateRepo {
            dir: dir.to_path_buf(),
            remote,
            branch: profile.state_repo.branch.clone(),
        };
        repo.init()?;
        Ok(Some(repo))
    }

    pub fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args([
                "-c",
                "user.name=herdr-superagent",
                "-c",
                "user.email=superagent@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if output.status.success() {
            Ok(stdout)
        } else {
            Err(format!(
                "git {}: {}",
                args.join(" "),
                if stderr.is_empty() { stdout } else { stderr }
            ))
        }
    }

    fn init(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        if !self.dir.join(".git").exists() {
            self.git(&["init", "-q", "-b", &self.branch])?;
        }
        if let Some(remote) = &self.remote {
            match self.git(&["remote", "get-url", "origin"]) {
                Ok(url) if url == *remote => {}
                Ok(_) => {
                    self.git(&["remote", "set-url", "origin", remote])?;
                }
                Err(_) => {
                    self.git(&["remote", "add", "origin", remote])?;
                }
            }
            // A fresh checkout takes the remote's history before anything local exists,
            // so the first pull cannot collide with files seeded here.
            let has_commits = self.git(&["rev-parse", "--verify", "HEAD"]).is_ok();
            if !has_commits && self.git(&["fetch", "-q", "origin", &self.branch]).is_ok() {
                self.git(&["checkout", "-q", "-B", &self.branch, &format!("origin/{}", self.branch)])?;
            } else {
                let _ = self.pull();
            }
        }
        let ignore = self.dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "*.sqlite\n*.sqlite-*\n*.sock\ndaemon.json\n*.log\n").map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(self.dir.join("tickets")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(self.dir.join("memory")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(self.dir.join(".superagent")).map_err(|e| e.to_string())?;
        if self.git(&["rev-parse", "--verify", "HEAD"]).is_err() {
            self.git(&["add", "-A"])?;
            self.git(&["commit", "-q", "--allow-empty", "-m", "superagent: initialise profile state"])?;
        }
        Ok(())
    }

    /// Fast-forward pull; an error when the histories diverged.
    pub fn pull(&self) -> Result<(), String> {
        if self.remote.is_none() {
            return Ok(());
        }
        match self.git(&["pull", "-q", "--ff-only", "origin", &self.branch]) {
            Ok(_) => Ok(()),
            // An empty remote has nothing to pull yet.
            Err(e) if e.contains("couldn't find remote ref") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Stages everything and commits when something changed.
    pub fn commit(&self, message: &str) -> Result<bool, String> {
        self.git(&["add", "-A"])?;
        if self.git(&["diff", "--cached", "--quiet"]).is_ok() {
            return Ok(false);
        }
        self.git(&["commit", "-q", "-m", message])?;
        Ok(true)
    }

    /// Pushes as a fast-forward only; `Rejected` means another machine wrote first.
    pub fn push(&self) -> Result<PushResult, String> {
        if self.remote.is_none() {
            return Ok(PushResult::NoRemote);
        }
        match self.git(&["push", "-q", "origin", &format!("{0}:{0}", self.branch)]) {
            Ok(_) => Ok(PushResult::Pushed),
            Err(e)
                if e.contains("rejected")
                    || e.contains("fetch first")
                    || e.contains("non-fast-forward")
                    || e.contains("failed to push") =>
            {
                Ok(PushResult::Rejected)
            }
            Err(e) => Err(e),
        }
    }

    fn lease_path(&self) -> PathBuf {
        self.dir.join(".superagent").join("lease.toml")
    }

    pub fn lease(&self) -> Option<Lease> {
        let text = std::fs::read_to_string(self.lease_path()).ok()?;
        toml::from_str(&text).ok()
    }

    fn write_lease(&self, lease: &Lease) -> Result<(), String> {
        if let Some(parent) = self.lease_path().parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(self.lease_path(), toml::to_string(lease).unwrap_or_default()).map_err(|e| e.to_string())
    }

    /// Takes the lease for this machine: pull, check, write, commit, push as a
    /// fast-forward of what was pulled. A rejected push means another machine
    /// got there first.
    pub fn acquire(&self, take_over: bool) -> Result<Lease, LeaseError> {
        self.pull().map_err(LeaseError::Git)?;
        let now = clock::now_iso();
        let me = machine_name();
        if let Some(current) = self.lease() {
            let live = current.expires > now && (current.machine != me || pid_alive(current.pid));
            if live && current.machine != me && !take_over {
                return Err(LeaseError::Held(current));
            }
        }
        let lease = Lease {
            machine: me,
            pid: std::process::id(),
            since: now.clone(),
            expires: clock::plus_ms(&now, LEASE_SECONDS * 1000),
        };
        self.write_lease(&lease).map_err(LeaseError::Git)?;
        self.commit(&format!("lease: {} takes profile", lease.machine))
            .map_err(LeaseError::Git)?;
        match self.push().map_err(LeaseError::Git)? {
            PushResult::Rejected => {
                // Someone else moved the branch: take what they wrote and report who holds it.
                let _ = self.git(&["reset", "-q", "--hard", &format!("origin/{}", self.branch)]);
                let _ = self.pull();
                match self.lease() {
                    Some(other) if other.machine != lease.machine => Err(LeaseError::Held(other)),
                    _ => Err(LeaseError::Git("lease push rejected; retry".into())),
                }
            }
            _ => Ok(lease),
        }
    }

    /// Extends this machine's lease; a lease another machine took over is reported.
    pub fn renew(&self) -> Result<(), LeaseError> {
        let me = machine_name();
        match self.lease() {
            Some(current) if current.machine != me => return Err(LeaseError::Held(current)),
            _ => {}
        }
        let now = clock::now_iso();
        let lease = Lease {
            machine: me,
            pid: std::process::id(),
            since: self.lease().map(|l| l.since).unwrap_or_else(|| now.clone()),
            expires: clock::plus_ms(&now, LEASE_SECONDS * 1000),
        };
        self.write_lease(&lease).map_err(LeaseError::Git)?;
        self.commit("lease: renew").map_err(LeaseError::Git)?;
        if self.push().map_err(LeaseError::Git)? == PushResult::Rejected {
            self.pull().map_err(LeaseError::Git)?;
            if let Some(other) = self.lease().filter(|l| l.machine != lease.machine) {
                return Err(LeaseError::Held(other));
            }
            let _ = self.push();
        }
        Ok(())
    }

    pub fn release(&self) -> Result<(), String> {
        if self.lease().map(|l| l.machine == machine_name()).unwrap_or(false) {
            let _ = std::fs::remove_file(self.lease_path());
        }
        self.commit("lease: release")?;
        let _ = self.push()?;
        Ok(())
    }

    // ---- ticket records ----

    fn ticket_dir(&self, ticket: &Ticket) -> PathBuf {
        let date: String = ticket.created_at.chars().take(10).collect();
        let slug: String = ticket
            .title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .take(4)
            .collect::<Vec<_>>()
            .join("-");
        let id = ticket.id.trim_start_matches("t-");
        // A renamed ticket keeps the directory it already has.
        let suffix = format!("-{id}");
        if let Ok(entries) = std::fs::read_dir(self.dir.join("tickets")) {
            for entry in entries.filter_map(Result::ok) {
                if entry.file_name().to_string_lossy().ends_with(&suffix) {
                    return entry.path();
                }
            }
        }
        self.dir.join("tickets").join(format!("{date}-{slug}-{id}"))
    }

    /// Writes `ticket.md` (for people) and `ticket.json` (the full record).
    pub fn write_ticket(&self, ticket: &Ticket) -> Result<PathBuf, String> {
        let dir = self.ticket_dir(ticket);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("ticket.json"), serde_json::to_string_pretty(ticket).unwrap_or_default()).map_err(|e| e.to_string())?;
        let report = ticket
            .latest_report()
            .map(|r| format!("{}: {}", r.state.as_str(), r.summary))
            .unwrap_or_else(|| "none yet".into());
        let md = format!(
            "---\nid: {id}\ntitle: {title}\nstage: {stage}\nmode: {mode}\nstrength: {strength}\nproject: {project}\nworkflow: {workflow}\ncreated: {created}\nupdated: {updated}\nended: {ended}\n---\n\n# {title}\n\n- **Stage:** {stage}\n- **Mode:** {mode}\n- **Mandate:** {strength}, profile {policy}, actor {actor}\n- **Latest report:** {report}\n- **Assignments:** {assignments}\n- **Inputs:** {inputs} ({undelivered} undelivered)\n{question}{checkpoint}",
            id = ticket.id,
            title = ticket.title,
            stage = ticket_stage(ticket).label(),
            mode = ticket.mandate.mode,
            strength = format!("{:?}", ticket.mandate.strength).to_lowercase(),
            project = ticket.project.as_deref().unwrap_or("-"),
            workflow = ticket.workflow.as_deref().unwrap_or("-"),
            created = ticket.created_at,
            updated = ticket.updated_at,
            ended = ticket.ended.as_ref().map(|e| format!("{:?} ({:?})", e.outcome, e.by).to_lowercase()).unwrap_or_else(|| "-".into()),
            policy = ticket.mandate.policy_version,
            actor = ticket.mandate.actor,
            report = report,
            assignments = ticket.assignments.len(),
            inputs = ticket.inputs.len(),
            undelivered = ticket.undelivered.len(),
            question = ticket.attention.as_ref().map(|a| format!("- **Question for you:** {}\n", a.question)).unwrap_or_default(),
            checkpoint = ticket.checkpoint.as_ref().map(|c| format!("- **Waiting at checkpoint:** {} — {}\n", c.name, c.summary)).unwrap_or_default(),
        );
        std::fs::write(dir.join("ticket.md"), md).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir.join("summaries")).map_err(|e| e.to_string())?;
        Ok(dir)
    }

    /// Writes a session summary beside the ticket: `summaries/NN-<role>.md` with frontmatter.
    pub fn write_summary(&self, ticket: &Ticket, entry: &crate::memory::MemoryEntry) -> Result<PathBuf, String> {
        let dir = self.ticket_dir(ticket).join("summaries");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) + 1;
        let role: String = entry.role.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let path = dir.join(format!("{n:02}-{}.md", if role.is_empty() { "worker" } else { &role }));
        let text = format!(
            "---\nid: {}\nproject: {}\nticket: {}\nassignment: {}\nrole: {}\ntitle: {}\ncreated: {}\n---\n\n{}\n",
            entry.id,
            entry.project,
            ticket.id,
            entry.assignment_id.as_deref().unwrap_or(""),
            entry.role,
            entry.title,
            entry.created_at,
            entry.body.trim_end()
        );
        std::fs::write(&path, text).map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// Writes a note outside any ticket: `memory/<project>/notes/<id>.md`.
    pub fn write_note(&self, entry: &crate::memory::MemoryEntry) -> Result<PathBuf, String> {
        let project: String = entry.project.chars().map(|c| if c == '/' { '_' } else { c }).collect();
        let dir = self.dir.join("memory").join(project).join("notes");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{}.md", entry.id));
        let text = format!(
            "---\nid: {}\nproject: {}\nticket: \nassignment: \nrole: {}\ntitle: {}\ncreated: {}\n---\n\n{}\n",
            entry.id,
            entry.project,
            entry.role,
            entry.title,
            entry.created_at,
            entry.body.trim_end()
        );
        std::fs::write(&path, text).map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// Loads every summary and note file into the memory index; returns how many.
    pub fn reindex_memory(&self, store: &crate::store::sqlite::SqliteStore) -> usize {
        let mut count = 0;
        let mut files = Vec::new();
        collect_md(&self.dir.join("tickets"), &mut files);
        collect_md(&self.dir.join("memory"), &mut files);
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if let Some(entry) = parse_summary(&text) {
                if store.memory_add(&entry).is_ok() {
                    count += 1;
                }
            }
        }
        count
    }

    /// Appends an event to the ticket's `events.jsonl`.
    pub fn append_event(&self, ticket: &Ticket, event: &DomainEvent) -> Result<(), String> {
        use std::io::Write;
        let dir = self.ticket_dir(ticket);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))
            .map_err(|e| e.to_string())?;
        let line = serde_json::json!({ "at": clock::now_iso(), "event": event });
        writeln!(file, "{line}").map_err(|e| e.to_string())
    }

    /// Rebuilds the local database from the ticket records; returns how many were loaded.
    pub fn reindex(&self, repo: &dyn Repositories) -> Result<usize, String> {
        let mut count = 0;
        let tickets = self.dir.join("tickets");
        let Ok(entries) = std::fs::read_dir(&tickets) else { return Ok(0) };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path().join("ticket.json");
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            match serde_json::from_str::<Ticket>(&text) {
                Ok(ticket) => {
                    repo.ticket_save(&ticket);
                    count += 1;
                }
                Err(e) => tracing::warn!(path = %path.display(), %e, "skipping unreadable ticket record"),
            }
        }
        Ok(count)
    }

    /// Ticket ids that have records in the repo.
    #[cfg(test)]
    pub fn ticket_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(self.dir.join("tickets")) {
            for entry in entries.filter_map(Result::ok) {
                if let Ok(text) = std::fs::read_to_string(entry.path().join("ticket.json")) {
                    if let Some(id) = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .and_then(|v| v["id"].as_str().map(str::to_string))
                    {
                        ids.insert(id);
                    }
                }
            }
        }
        ids
    }
}

fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_md(&path, out);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) && path.file_name().map(|n| n != "ticket.md").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Parses a summary or note file written by `write_summary`/`write_note`.
fn parse_summary(text: &str) -> Option<crate::memory::MemoryEntry> {
    let rest = text.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;
    let field = |name: &str| {
        front
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")).map(|v| v.trim().to_string()))
    };
    let opt = |name: &str| field(name).filter(|v| !v.is_empty());
    Some(crate::memory::MemoryEntry {
        id: field("id")?,
        project: field("project")?,
        ticket_id: opt("ticket"),
        assignment_id: opt("assignment"),
        role: field("role").unwrap_or_default(),
        title: field("title")?,
        body: body.trim().to_string(),
        created_at: field("created")?,
    })
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        || Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::*;

    fn profile(kind: &str, remote: &str) -> Profile {
        let mut profile = Profile::new("work");
        profile.state_repo.kind = kind.into();
        profile.state_repo.remote = remote.into();
        profile
    }

    fn ticket(id: &str) -> Ticket {
        Ticket {
            id: id.into(),
            title: "Fix the flaky login test".into(),
            decision_id: "d".into(),
            mandate: Mandate {
                strength: Strength::Explicit,
                policy_version: "v".into(),
                basis: MandateBasis::Explicit { workflow: None },
                actor: "you".into(),
                scope: "s".into(),
                mode: Mode::Checkpoint,
            },
            project: Some("~/repo".into()),
            workflow: None,
            ended: None,
            attention: None,
            checkpoint: None,
            approved_checkpoints: Vec::new(),
            pack: None,
            required_checkpoints: Vec::new(),
            assignments: vec![],
            inputs: vec![],
            undelivered: vec![],
            created_at: "2026-11-08T10:00:00.000Z".into(),
            updated_at: "2026-11-08T10:00:00.000Z".into(),
            origin: Origin { request_id: "r".into() },
            execution: None,
            delivery: None,
        }
    }

    #[test]
    fn local_repo_records_tickets_and_reindexes_them() {
        let dir = tempfile::tempdir().unwrap();
        let repo = StateRepo::open_or_init(&profile("local", ""), dir.path()).unwrap().unwrap();
        assert!(dir.path().join(".git").exists());
        let written = repo.write_ticket(&ticket("t-abc123")).unwrap();
        assert!(written.ends_with("2026-11-08-fix-the-flaky-login-abc123"));
        repo.append_event(
            &ticket("t-abc123"),
            &DomainEvent::TicketCreated {
                ticket_id: "t-abc123".into(),
                decision_id: "d".into(),
                strength: Strength::Explicit,
            },
        )
        .unwrap();
        assert!(repo.commit("ticket t-abc123: created").unwrap());
        assert!(!repo.commit("nothing").unwrap());
        assert_eq!(repo.push().unwrap(), PushResult::NoRemote);
        let store = crate::store::MemoryRepositories::new();
        assert_eq!(repo.reindex(&store).unwrap(), 1);
        assert_eq!(store.ticket_get("t-abc123").unwrap().title, "Fix the flaky login test");
        assert_eq!(repo.ticket_ids().into_iter().collect::<Vec<_>>(), vec!["t-abc123".to_string()]);
        let md = std::fs::read_to_string(written.join("ticket.md")).unwrap();
        assert!(md.contains("stage: working/queued") && md.contains("# Fix the flaky login test"));
        let entry = crate::memory::now_entry(
            "acme/api",
            Some("t-abc123".into()),
            Some("a1".into()),
            "worker",
            "t-abc123 worker: fix login",
            "Goal: fix.\nWhat was done: locked the cache.",
        );
        let summary = repo.write_summary(&ticket("t-abc123"), &entry).unwrap();
        assert!(summary.ends_with("summaries/01-worker.md"));
        repo.write_note(&crate::memory::now_entry(
            "acme/api",
            None,
            None,
            "you",
            "api conventions",
            "PRs need two approvals.",
        ))
        .unwrap();
        let sqlite = crate::store::sqlite::SqliteStore::open_in_memory().unwrap();
        assert_eq!(repo.reindex_memory(&sqlite), 2);
        assert_eq!(
            sqlite.memory_search("acme/api", "cache", 5).unwrap()[0].ticket_id.as_deref(),
            Some("t-abc123")
        );
    }

    #[test]
    fn the_lease_moves_between_machines_as_a_fast_forward() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("origin.git");
        Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        let remote = bare.display().to_string();
        let a = root.path().join("machine-a");
        let b = root.path().join("machine-b");

        std::env::set_var("HERDR_SUPERAGENT_MACHINE", "alpha");
        let repo_a = StateRepo::open_or_init(&profile("remote", &remote), &a).unwrap().unwrap();
        let lease = repo_a.acquire(false).unwrap();
        assert_eq!(lease.machine, "alpha");
        assert_eq!(repo_a.push().unwrap(), PushResult::Pushed);

        std::env::set_var("HERDR_SUPERAGENT_MACHINE", "beta");
        let repo_b = StateRepo::open_or_init(&profile("remote", &remote), &b).unwrap().unwrap();
        match repo_b.acquire(false) {
            Err(LeaseError::Held(held)) => assert_eq!(held.machine, "alpha"),
            other => panic!("expected the lease to be held, got {other:?}"),
        }
        let taken = repo_b.acquire(true).unwrap();
        assert_eq!(taken.machine, "beta");

        // alpha's next renew sees beta's lease and reports it.
        std::env::set_var("HERDR_SUPERAGENT_MACHINE", "alpha");
        repo_a.pull().unwrap();
        match repo_a.renew() {
            Err(LeaseError::Held(held)) => assert_eq!(held.machine, "beta"),
            other => panic!("expected beta to hold the lease, got {other:?}"),
        }

        std::env::set_var("HERDR_SUPERAGENT_MACHINE", "beta");
        repo_b.write_ticket(&ticket("t-1")).unwrap();
        repo_b.commit("ticket t-1").unwrap();
        repo_b.release().unwrap();
        assert!(repo_b.lease().is_none());

        // alpha picks up the ticket beta recorded.
        std::env::set_var("HERDR_SUPERAGENT_MACHINE", "alpha");
        repo_a.pull().unwrap();
        assert_eq!(repo_a.ticket_ids().len(), 1);
        repo_a.acquire(false).map_err(|e| e.to_string()).unwrap();
        std::env::remove_var("HERDR_SUPERAGENT_MACHINE");
    }
}
