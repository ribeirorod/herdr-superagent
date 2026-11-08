//! SQLite `Repositories` plus the durable effect queue. Aggregates are stored
//! as JSON values with a few indexed columns, like the prototype's store. The
//! database is a local cache of the profile's state repository (PLAN §8a);
//! `reindex` can rebuild it.

use std::cell::{Cell, RefCell};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::core::errors::UserError;
use crate::core::model::*;
use crate::core::ports::Repositories;

pub const SCHEMA_VERSION: i64 = 1;

pub struct SqliteStore {
    conn: RefCell<Connection>,
    depth: Cell<usize>,
}

/// A side effect queued inside a transaction and run after it commits. One row
/// per (kind, key): a newer payload replaces an older one that has not run,
/// keeping its place (`seq`) in the queue but bumping its `generation`, so a
/// run of the old payload does not remove the new one.
#[derive(Debug, Clone, PartialEq)]
pub struct Effect {
    pub kind: String,
    pub key: String,
    pub payload: serde_json::Value,
    pub seq: i64,
    pub generation: i64,
    pub attempts: i64,
    pub error: String,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<SqliteStore, UserError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| UserError::internal(format!("create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        let store = SqliteStore {
            conn: RefCell::new(conn),
            depth: Cell::new(0),
        };
        store.migrate()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<SqliteStore, UserError> {
        let store = SqliteStore {
            conn: RefCell::new(Connection::open_in_memory()?),
            depth: Cell::new(0),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), UserError> {
        let conn = self.conn.borrow();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS request (id TEXT PRIMARY KEY, value TEXT NOT NULL, at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS disposition (id TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS evaluation (id TEXT PRIMARY KEY, request_id TEXT NOT NULL, value TEXT NOT NULL, started_at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS ticket (id TEXT PRIMARY KEY, value TEXT NOT NULL, created_at TEXT NOT NULL,
               open INTEGER NOT NULL, active INTEGER NOT NULL, project TEXT);
             CREATE TABLE IF NOT EXISTS decision (id TEXT PRIMARY KEY, request_id TEXT NOT NULL, value TEXT NOT NULL, at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS effect (kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, seq INTEGER NOT NULL,
               generation INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0, error TEXT NOT NULL DEFAULT '',
               due INTEGER NOT NULL, since TEXT NOT NULL, PRIMARY KEY (kind, key));
             CREATE INDEX IF NOT EXISTS request_at ON request(at);
             CREATE INDEX IF NOT EXISTS evaluation_request ON evaluation(request_id);
             CREATE INDEX IF NOT EXISTS ticket_open ON ticket(open);
             CREATE INDEX IF NOT EXISTS ticket_active ON ticket(active);
             CREATE INDEX IF NOT EXISTS decision_at ON decision(at);
             CREATE INDEX IF NOT EXISTS effect_due ON effect(kind, due, seq);",
        )?;
        let version: Option<i64> = conn
            .query_row("SELECT value FROM kv WHERE key = 'schema'", [], |row| row.get::<_, String>(0))
            .optional()?
            .and_then(|v| v.parse().ok());
        match version {
            Some(v) if v > SCHEMA_VERSION => {
                return Err(UserError::internal(format!(
                    "database schema {v} is newer than this binary supports ({SCHEMA_VERSION})"
                )))
            }
            Some(v) if v == SCHEMA_VERSION => {}
            _ => {
                conn.execute(
                    "INSERT OR REPLACE INTO kv (key, value) VALUES ('schema', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
        }
        Ok(())
    }

    fn value<T: DeserializeOwned>(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Option<T> {
        let conn = self.conn.borrow();
        let text: Option<String> = conn.query_row(sql, args, |row| row.get(0)).optional().ok().flatten();
        text.and_then(|t| serde_json::from_str(&t).ok())
    }

    fn values<T: DeserializeOwned>(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Vec<T> {
        let conn = self.conn.borrow();
        let Ok(mut statement) = conn.prepare(sql) else { return Vec::new() };
        let rows = statement.query_map(args, |row| row.get::<_, String>(0));
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).filter_map(|t| serde_json::from_str(&t).ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn json<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
    }

    fn exec(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) {
        if let Err(error) = self.conn.borrow().execute(sql, args) {
            tracing::error!(%error, sql, "store write failed");
        }
    }

    // ---- key/value ----

    pub fn kv_get(&self, key: &str) -> Option<String> {
        self.conn
            .borrow()
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| row.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn kv_set(&self, key: &str, value: &str) {
        self.exec("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", &[&key, &value]);
    }

    // ---- effects ----

    /// Queue an effect; one with the same kind and key keeps its place but takes the new payload.
    pub fn effect_enqueue(&self, kind: &str, key: &str, payload: &serde_json::Value, now_ms: i64, now: &str) {
        let conn = self.conn.borrow();
        let seq: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM effect", [], |row| row.get(0))
            .unwrap_or(1);
        if let Err(error) = conn.execute(
            "INSERT INTO effect (kind, key, value, seq, generation, attempts, error, due, since) VALUES (?1, ?2, ?3, ?4, 0, 0, '', ?5, ?6)
             ON CONFLICT(kind, key) DO UPDATE SET value = excluded.value, generation = effect.generation + 1, due = MIN(effect.due, excluded.due)",
            params![kind, key, Self::json(payload), seq, now_ms, now],
        ) {
            tracing::error!(%error, kind, key, "effect enqueue failed");
        }
    }

    /// Effects of `kind` due at `now_ms`, oldest first.
    pub fn effects_due(&self, kind: &str, now_ms: i64, limit: usize) -> Vec<Effect> {
        let conn = self.conn.borrow();
        let Ok(mut statement) = conn.prepare(
            "SELECT kind, key, value, seq, generation, attempts, error FROM effect WHERE kind = ?1 AND due <= ?2 ORDER BY seq LIMIT ?3",
        ) else {
            return Vec::new();
        };
        let rows = statement.query_map(params![kind, now_ms, limit as i64], |row| {
            Ok(Effect {
                kind: row.get(0)?,
                key: row.get(1)?,
                payload: serde_json::from_str::<serde_json::Value>(&row.get::<_, String>(2)?).unwrap_or(serde_json::Value::Null),
                seq: row.get(3)?,
                generation: row.get(4)?,
                attempts: row.get(5)?,
                error: row.get(6)?,
            })
        });
        rows.map(|r| r.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    pub fn effect_kinds_pending(&self) -> Vec<(String, i64)> {
        let conn = self.conn.borrow();
        let Ok(mut statement) = conn.prepare("SELECT kind, COUNT(*) FROM effect GROUP BY kind ORDER BY kind") else {
            return Vec::new();
        };
        statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .map(|r| r.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Remove an effect that ran, unless a newer payload replaced it meanwhile.
    pub fn effect_done(&self, kind: &str, key: &str, generation: i64) -> bool {
        self.conn
            .borrow()
            .execute(
                "DELETE FROM effect WHERE kind = ?1 AND key = ?2 AND generation = ?3",
                params![kind, key, generation],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Record a failed attempt and when to try again.
    pub fn effect_failed(&self, kind: &str, key: &str, error: &str, retry_at_ms: i64) {
        let error: String = error.chars().take(500).collect();
        self.exec(
            "UPDATE effect SET attempts = attempts + 1, error = ?3, due = ?4 WHERE kind = ?1 AND key = ?2",
            &[&kind, &key, &error, &retry_at_ms],
        );
    }

    pub fn ticket_count(&self) -> (i64, i64) {
        let conn = self.conn.borrow();
        let open: i64 = conn
            .query_row("SELECT COUNT(*) FROM ticket WHERE open = 1", [], |row| row.get(0))
            .unwrap_or(0);
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM ticket", [], |row| row.get(0)).unwrap_or(0);
        (open, total)
    }
}

impl Repositories for SqliteStore {
    fn atomic(&self, work: &mut dyn FnMut() -> Result<(), UserError>) -> Result<(), UserError> {
        let depth = self.depth.get();
        let savepoint = format!("sp{depth}");
        self.conn.borrow().execute_batch(&format!("SAVEPOINT {savepoint}"))?;
        self.depth.set(depth + 1);
        let result = work();
        self.depth.set(depth);
        match &result {
            Ok(()) => self.conn.borrow().execute_batch(&format!("RELEASE {savepoint}"))?,
            Err(_) => self
                .conn
                .borrow()
                .execute_batch(&format!("ROLLBACK TO {savepoint}; RELEASE {savepoint}"))?,
        }
        result
    }

    fn request_get(&self, id: &str) -> Option<Request> {
        self.value("SELECT value FROM request WHERE id = ?1", &[&id])
    }
    fn request_save(&self, request: &Request) {
        self.exec(
            "INSERT OR REPLACE INTO request (id, value, at) VALUES (?1, ?2, ?3)",
            &[&request.id, &Self::json(request), &request.at],
        );
    }
    fn request_list(&self) -> Vec<Request> {
        self.values("SELECT value FROM request ORDER BY at, id", &[])
    }
    fn disposition_get(&self, id: &str) -> Option<RequestDisposition> {
        self.value("SELECT value FROM disposition WHERE id = ?1", &[&id])
    }
    fn disposition_save(&self, id: &str, value: &RequestDisposition) {
        self.exec(
            "INSERT OR REPLACE INTO disposition (id, value) VALUES (?1, ?2)",
            &[&id, &Self::json(value)],
        );
    }

    fn eval_get(&self, id: &str) -> Option<Evaluation> {
        self.value("SELECT value FROM evaluation WHERE id = ?1", &[&id])
    }
    fn eval_by_request(&self, request_id: &str) -> Option<Evaluation> {
        self.value(
            "SELECT value FROM evaluation WHERE request_id = ?1 ORDER BY started_at DESC LIMIT 1",
            &[&request_id],
        )
    }
    fn eval_save(&self, eval: &Evaluation) {
        self.exec(
            "INSERT OR REPLACE INTO evaluation (id, request_id, value, started_at) VALUES (?1, ?2, ?3, ?4)",
            &[&eval.id, &eval.request_id, &Self::json(eval), &eval.started_at],
        );
    }
    fn eval_delete(&self, id: &str) {
        self.exec("DELETE FROM evaluation WHERE id = ?1", &[&id]);
    }
    fn eval_list(&self) -> Vec<Evaluation> {
        self.values("SELECT value FROM evaluation ORDER BY started_at, id", &[])
    }

    fn ticket_get(&self, id: &str) -> Option<Ticket> {
        self.value("SELECT value FROM ticket WHERE id = ?1", &[&id])
    }
    fn ticket_save(&self, ticket: &Ticket) {
        let open = ticket.live() as i64;
        let active = ticket.active() as i64;
        self.exec(
            "INSERT OR REPLACE INTO ticket (id, value, created_at, open, active, project) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            &[&ticket.id, &Self::json(ticket), &ticket.created_at, &open, &active, &ticket.project],
        );
    }
    fn ticket_list(&self) -> Vec<Ticket> {
        self.values("SELECT value FROM ticket ORDER BY created_at, id", &[])
    }
    fn ticket_open(&self) -> Vec<Ticket> {
        self.values("SELECT value FROM ticket WHERE open = 1 ORDER BY created_at, id", &[])
    }
    fn ticket_active(&self) -> Vec<Ticket> {
        self.values("SELECT value FROM ticket WHERE active = 1 ORDER BY created_at, id", &[])
    }

    fn decision_get(&self, id: &str) -> Option<Decision> {
        self.value("SELECT value FROM decision WHERE id = ?1", &[&id])
    }
    fn decision_save(&self, decision: &Decision) {
        self.exec(
            "INSERT OR REPLACE INTO decision (id, request_id, value, at) VALUES (?1, ?2, ?3, ?4)",
            &[&decision.id, &decision.request_id, &Self::json(decision), &decision.at],
        );
    }
    fn decision_list(&self) -> Vec<Decision> {
        self.values("SELECT value FROM decision ORDER BY at, id", &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::errors::ErrorCode;

    fn request(id: &str) -> Request {
        Request {
            id: id.into(),
            source: "you".into(),
            actor: "me".into(),
            human: true,
            at: "2026-09-23T00:00:01.000Z".into(),
            text: "x".into(),
            workflow: None,
            mode: None,
            project: Some("p".into()),
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn roundtrips_and_rolls_back() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .atomic(&mut || {
                store.request_save(&request("r1"));
                Ok(())
            })
            .unwrap();
        assert_eq!(store.request_get("r1").map(|r| r.id), Some("r1".to_string()));
        let failed = store.atomic(&mut || {
            store.request_save(&request("r2"));
            store.atomic(&mut || {
                store.request_save(&request("r3"));
                Ok(())
            })?;
            Err(UserError::new(ErrorCode::Internal))
        });
        assert!(failed.is_err());
        assert_eq!(store.request_list().len(), 1);
    }

    #[test]
    fn effects_coalesce_per_key_and_keep_their_place() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.effect_enqueue("launch", "a", &serde_json::json!({"n": 1}), 100, "t");
        store.effect_enqueue("launch", "b", &serde_json::json!({"n": 2}), 100, "t");
        store.effect_enqueue("launch", "a", &serde_json::json!({"n": 3}), 100, "t");
        let due = store.effects_due("launch", 100, 10);
        assert_eq!(
            due.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "a keeps its place"
        );
        assert_eq!(due[0].payload["n"], 3);
        assert_eq!(due[0].generation, 1);
        // A run of a stale generation does not remove the newer payload.
        assert!(!store.effect_done("launch", "a", 0));
        assert!(store.effect_done("launch", "a", due[0].generation));
        store.effect_failed("launch", "b", "boom", 500);
        assert!(store.effects_due("launch", 100, 10).is_empty());
        assert_eq!(store.effects_due("launch", 500, 10)[0].attempts, 1);
        assert_eq!(store.effect_kinds_pending(), vec![("launch".to_string(), 1)]);
    }

    #[test]
    fn opens_a_file_and_indexes_tickets() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("db.sqlite")).unwrap();
        store.kv_set("daemon.version", "0.1.0");
        assert_eq!(store.kv_get("daemon.version").as_deref(), Some("0.1.0"));
        assert_eq!(store.ticket_count(), (0, 0));
    }
}
