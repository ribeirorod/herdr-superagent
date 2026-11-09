//! Memory (PLAN §7): per project inside a profile, cheapest first. Layer 1 is
//! the summary file in the state repo; layer 2 is this FTS5 index, rebuilt
//! from layer 1 by `reindex`. Every search carries a project: scope is
//! enforced here, not by agents.

use std::path::Path;
use std::process::Command;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::core::clock;
use crate::core::errors::UserError;
use crate::store::sqlite::SqliteStore;

/// Work outside any repository goes to this bucket, searched only on request.
pub const NOTES_PROJECT: &str = "notes";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    #[serde(default)]
    pub role: String,
    pub title: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryHit {
    pub id: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    pub title: String,
    pub snippet: String,
    pub created_at: String,
}

/// The project key of a path: the `org/repo` slug of its git `origin` when it
/// has one (worktrees map to their repo), else the repository root's name,
/// else the directory's name. `None` for a path that does not exist.
pub fn project_key(path: &str) -> Option<String> {
    let expanded = crate::profile::expand_home(path);
    let dir = Path::new(&expanded);
    if !dir.is_dir() {
        return None;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(url) = git(&["remote", "get-url", "origin"]) {
        if let Some(slug) = slug_of_remote(&url) {
            return Some(slug);
        }
    }
    if let Some(root) = git(&["rev-parse", "--show-toplevel"]) {
        if let Some(name) = Path::new(&root).file_name().and_then(|n| n.to_str()) {
            return Some(name.to_string());
        }
    }
    dir.file_name().and_then(|n| n.to_str()).map(str::to_string)
}

/// `org/repo` from an https or ssh git URL.
pub fn slug_of_remote(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let tail = if let Some((_, rest)) = trimmed.split_once("://") {
        rest.split_once('/')?.1
    } else if let Some((_, rest)) = trimmed.split_once(':') {
        rest
    } else {
        return None;
    };
    let parts: Vec<&str> = tail.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    Some(format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]).to_lowercase())
}

/// Masks values that look like secrets before a summary is written anywhere.
pub fn redact(text: &str) -> String {
    let key_words = [
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "api-key",
        "private_key",
        "authorization",
        "bearer",
    ];
    text.lines()
        .map(|line| {
            let lower = line.to_lowercase();
            let mut out = line.to_string();
            if key_words.iter().any(|w| lower.contains(w)) {
                // key = value / key: value / "key": "value" → keep the key, mask the value.
                if let Some(pos) = out.find(['=', ':']) {
                    let (head, tail) = out.split_at(pos + 1);
                    if tail.trim().len() > 3 {
                        out = format!("{head} [redacted]");
                    }
                }
            }
            // Long opaque tokens (hex, base64, JWT-like) anywhere in the line.
            out.split_whitespace()
                .map(|word| {
                    let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.');
                    let opaque = core.len() >= 32
                        && core.chars().all(|c| c.is_ascii_alphanumeric() || "-_.=".contains(c))
                        && core.chars().any(|c| c.is_ascii_digit())
                        && core.chars().any(|c| c.is_ascii_alphabetic())
                        && !core.contains('/')
                        && !core.starts_with("http");
                    if opaque {
                        word.replace(core, "[redacted]")
                    } else {
                        word.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl SqliteStore {
    pub fn memory_migrate(&self) -> Result<(), UserError> {
        self.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory (id TEXT PRIMARY KEY, project TEXT NOT NULL, ticket_id TEXT, assignment_id TEXT,
               role TEXT NOT NULL DEFAULT '', title TEXT NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS memory_project ON memory(project, created_at);
             CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(title, body, content='memory', content_rowid='rowid');
             CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN
               INSERT INTO memory_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body); END;
             CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN
               INSERT INTO memory_fts(memory_fts, rowid, title, body) VALUES ('delete', old.rowid, old.title, old.body); END;
             CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory BEGIN
               INSERT INTO memory_fts(memory_fts, rowid, title, body) VALUES ('delete', old.rowid, old.title, old.body);
               INSERT INTO memory_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body); END;",
        )
    }

    /// Stores an entry (idempotent by id); the body is redacted first.
    pub fn memory_add(&self, entry: &MemoryEntry) -> Result<MemoryEntry, UserError> {
        let entry = MemoryEntry {
            body: redact(&entry.body),
            title: redact(&entry.title),
            ..entry.clone()
        };
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO memory (id, project, ticket_id, assignment_id, role, title, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET project = excluded.project, title = excluded.title, body = excluded.body",
                params![entry.id, entry.project, entry.ticket_id, entry.assignment_id, entry.role, entry.title, entry.body, entry.created_at],
            )?;
            Ok(())
        })?;
        Ok(entry)
    }

    /// Full-text search inside one project, newest first among equal ranks.
    pub fn memory_search(&self, project: &str, query: &str, limit: usize) -> Result<Vec<MemoryHit>, UserError> {
        let query = fts_query(query);
        if query.is_empty() {
            return self.memory_recent(project, limit);
        }
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT m.id, m.project, m.ticket_id, m.title, snippet(memory_fts, 1, '[', ']', '…', 24), m.created_at
                 FROM memory_fts JOIN memory m ON m.rowid = memory_fts.rowid
                 WHERE memory_fts MATCH ?1 AND m.project = ?2
                 ORDER BY bm25(memory_fts), m.created_at DESC LIMIT ?3",
            )?;
            let rows = statement.query_map(params![query, project, limit as i64], |row| {
                Ok(MemoryHit {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    ticket_id: row.get(2)?,
                    title: row.get(3)?,
                    snippet: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?;
            Ok(rows.filter_map(Result::ok).collect())
        })
    }

    pub fn memory_recent(&self, project: &str, limit: usize) -> Result<Vec<MemoryHit>, UserError> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare("SELECT id, project, ticket_id, title, substr(body, 1, 160), created_at FROM memory WHERE project = ?1 ORDER BY created_at DESC LIMIT ?2")?;
            let rows = statement.query_map(params![project, limit as i64], |row| {
                Ok(MemoryHit { id: row.get(0)?, project: row.get(1)?, ticket_id: row.get(2)?, title: row.get(3)?, snippet: row.get(4)?, created_at: row.get(5)? })
            })?;
            Ok(rows.filter_map(Result::ok).collect())
        })
    }

    pub fn memory_get(&self, id: &str) -> Option<MemoryEntry> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, project, ticket_id, assignment_id, role, title, body, created_at FROM memory WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(MemoryEntry {
                            id: row.get(0)?,
                            project: row.get(1)?,
                            ticket_id: row.get(2)?,
                            assignment_id: row.get(3)?,
                            role: row.get(4)?,
                            title: row.get(5)?,
                            body: row.get(6)?,
                            created_at: row.get(7)?,
                        })
                    },
                )
                .ok())
        })
        .ok()
        .flatten()
    }

    pub fn memory_count(&self) -> i64 {
        self.with_conn(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM memory", [], |row| row.get(0))?))
            .unwrap_or(0)
    }
}

/// Plain words → an FTS5 query of prefix terms joined with OR, so a question
/// like "sqlite-vec vs pgvector?" matches summaries that mention either.
pub fn fts_query(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3)
        .map(|w| format!("\"{}\"*", w.to_lowercase()))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub fn now_entry(
    project: &str,
    ticket_id: Option<String>,
    assignment_id: Option<String>,
    role: &str,
    title: &str,
    body: &str,
) -> MemoryEntry {
    let created_at = clock::now_iso();
    let id = crate::core::ids::key(&[
        "memory",
        project,
        ticket_id.as_deref().unwrap_or(""),
        assignment_id.as_deref().unwrap_or(""),
        title,
        &created_at,
    ]);
    MemoryEntry {
        id,
        project: project.to_string(),
        ticket_id,
        assignment_id,
        role: role.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_redaction() {
        assert_eq!(
            slug_of_remote("git@github.com:Ribeirorod/Herdr-Superagent.git").as_deref(),
            Some("ribeirorod/herdr-superagent")
        );
        assert_eq!(
            slug_of_remote("https://bitbucket.org/yoshien/payments-api").as_deref(),
            Some("yoshien/payments-api")
        );
        assert_eq!(slug_of_remote("nonsense"), None);
        let text = "API_KEY=sk-abc123def456ghi789jkl012mno345pqr\nnormal line about tokens of work\nAuthorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let out = redact(text);
        assert!(out.contains("API_KEY= [redacted]"), "{out}");
        assert!(out.contains("Authorization: [redacted]"), "{out}");
        assert!(!out.contains("eyJhbGci"));
        assert_eq!(fts_query("sqlite-vec vs pgvector?"), "\"sqlite\"* OR \"vec\"* OR \"pgvector\"*");
    }

    #[test]
    fn searches_inside_one_project_only() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.memory_migrate().unwrap();
        store
            .memory_add(&now_entry(
                "acme/api",
                Some("t-1".into()),
                None,
                "worker",
                "t-1 worker: fix login retry",
                "Login flake came from a race in the session cache. Fixed by locking.",
            ))
            .unwrap();
        store
            .memory_add(&now_entry(
                "acme/web",
                None,
                None,
                "worker",
                "web: login page",
                "Login page restyle, nothing about retries.",
            ))
            .unwrap();
        let hits = store.memory_search("acme/api", "login race", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ticket_id.as_deref(), Some("t-1"));
        assert!(hits[0].snippet.contains("[") && hits[0].snippet.to_lowercase().contains("login"));
        assert!(store.memory_search("acme/web", "race", 5).unwrap().is_empty());
        assert_eq!(
            store.memory_search("acme/web", "", 5).unwrap().len(),
            1,
            "an empty query lists recent entries"
        );
        assert_eq!(store.memory_count(), 2);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            project_key(&dir.path().display().to_string()),
            dir.path().file_name().map(|n| n.to_string_lossy().to_string())
        );
        assert_eq!(project_key("/definitely/missing"), None);
    }
}
