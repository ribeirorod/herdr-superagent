//! Semantic memory (PLAN §7, phase 5): a vector per memory entry, compared by
//! cosine inside one project and fused with the full-text ranking.
//!
//! Three embedders, cheapest first; `memory.embedder` picks one:
//!
//! * `hash`: built in, no model, no network. Words, word pairs and character
//!   trigrams hashed into a fixed vector. It bridges inflection, compounds and
//!   typos ("retries" ~ "retry", "login-cache" ~ "login cache"); it does not
//!   know that "auth" and "sign-in" mean the same.
//! * `ollama`: a local Ollama server (`memory.endpoint`, `memory.model`),
//!   reached with `curl`. Real meaning, still nothing leaves the machine.
//! * `command`: any program that reads text on stdin and prints a JSON array
//!   of numbers, for another local embedder.
//!
//! Vectors live only in the local database; the state repo carries text.

use std::io::Write;
use std::process::{Command, Stdio};

use rusqlite::params;

use crate::core::errors::UserError;
use crate::memory::MemoryHit;
use crate::profile::Memory;
use crate::store::sqlite::SqliteStore;

pub const HASH_DIM: usize = 512;
/// Below this cosine two texts are unrelated for our purposes.
const MIN_SIMILARITY: f32 = 0.1;

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Common words carry no topic; leaving them out keeps short texts apart.
const STOP: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "was", "were", "are", "but", "not", "you", "have", "has", "had", "into", "then",
    "when", "what", "which", "will", "would", "should", "could", "about", "there", "their", "been", "all", "any", "can", "its", "our",
];

/// A crude stem: enough that "retried", "retries" and "retry", or "caching" and "cache", meet.
fn stem(word: &str) -> String {
    let mut w = word.to_string();
    for (suffix, put) in [("ies", "y"), ("ied", "y"), ("ing", ""), ("ed", ""), ("es", ""), ("s", "")] {
        if w.len() > suffix.len() + 2 && w.ends_with(suffix) {
            w.truncate(w.len() - suffix.len());
            w.push_str(put);
            break;
        }
    }
    if w.len() > 4 && w.ends_with(['e', 'y', 'i']) {
        w.pop();
    }
    w
}

/// The built-in embedder: signed feature hashing, L2-normalised.
pub fn hash_embed(text: &str) -> Vec<f32> {
    let mut vector = vec![0f32; HASH_DIM];
    let mut add = |feature: &str, weight: f32| {
        let hash = fnv(feature.as_bytes());
        let index = (hash % HASH_DIM as u64) as usize;
        let sign = if (hash >> 40) & 1 == 0 { 1.0 } else { -1.0 };
        vector[index] += sign * weight;
    };
    let lower = text.to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOP.contains(w))
        .map(stem)
        .collect();
    for (i, word) in words.iter().enumerate() {
        add(&format!("w:{word}"), 2.0);
        if let Some(next) = words.get(i + 1) {
            add(&format!("b:{word} {next}"), 1.0);
        }
        let chars: Vec<char> = format!("^{word}$").chars().collect();
        for gram in chars.windows(3) {
            add(&format!("g:{}", gram.iter().collect::<String>()), 0.5);
        }
    }
    normalise(&mut vector);
    vector
}

fn normalise(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Numbers from an embedder's JSON: a bare array, or one under `embedding` / `embeddings[0]` / `data[0].embedding`.
pub fn parse_vector(output: &str) -> Option<Vec<f32>> {
    let value: serde_json::Value = serde_json::from_str(output.trim()).ok()?;
    let array = value
        .as_array()
        .or_else(|| value.get("embedding").and_then(|v| v.as_array()))
        .or_else(|| value.pointer("/embeddings/0").and_then(|v| v.as_array()))
        .or_else(|| value.pointer("/data/0/embedding").and_then(|v| v.as_array()))?;
    let mut vector: Vec<f32> = array.iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect();
    if vector.is_empty() {
        return None;
    }
    normalise(&mut vector);
    Some(vector)
}

fn pipe(program: &str, args: &[String], input: &str) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .env("PATH", crate::envpath::full_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child.wait_with_output().map_err(|e| format!("{program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim().lines().next().unwrap_or("")
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// The embedder a profile asks for, as `(name recorded with each vector, function)`.
pub struct Embedder {
    pub name: String,
    settings: Memory,
}

impl Embedder {
    pub fn new(settings: &Memory) -> Embedder {
        let name = match settings.embedder.as_str() {
            "ollama" => format!("ollama:{}", settings.model),
            "command" => format!("command:{}", settings.command.first().cloned().unwrap_or_default()),
            _ => format!("hash:{HASH_DIM}"),
        };
        Embedder {
            name,
            settings: settings.clone(),
        }
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        let text: String = text.chars().take(8000).collect();
        match self.settings.embedder.as_str() {
            "ollama" => {
                let body = serde_json::json!({ "model": self.settings.model, "prompt": text }).to_string();
                let url = format!("{}/api/embeddings", self.settings.endpoint.trim_end_matches('/'));
                let args: Vec<String> = [
                    "-sS",
                    "-m",
                    "20",
                    "-H",
                    "Content-Type: application/json",
                    "--data-binary",
                    "@-",
                    &url,
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                let out = pipe("curl", &args, &body)?;
                parse_vector(&out).ok_or_else(|| format!("ollama returned no embedding: {}", out.chars().take(160).collect::<String>()))
            }
            "command" => {
                let (program, args) = self.settings.command.split_first().ok_or("memory.command is empty")?;
                let out = pipe(program, args, &text)?;
                parse_vector(&out).ok_or_else(|| format!("{program} printed no JSON array of numbers"))
            }
            _ => Ok(hash_embed(&text)),
        }
    }
}

fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

impl SqliteStore {
    pub fn vector_migrate(&self) -> Result<(), UserError> {
        self.execute_batch("CREATE TABLE IF NOT EXISTS memory_vec (id TEXT PRIMARY KEY, model TEXT NOT NULL, vec BLOB NOT NULL);")
    }

    pub fn vector_put(&self, id: &str, model: &str, vector: &[f32]) -> Result<(), UserError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO memory_vec (id, model, vec) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET model = excluded.model, vec = excluded.vec",
                params![id, model, to_blob(vector)],
            )?;
            Ok(())
        })
    }

    /// Entries of a project without a vector from `model`: `(id, title, body)`.
    pub fn vector_missing(&self, model: &str, limit: usize) -> Vec<(String, String, String)> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT m.id, m.title, m.body FROM memory m LEFT JOIN memory_vec v ON v.id = m.id AND v.model = ?1 WHERE v.id IS NULL LIMIT ?2",
            )?;
            let rows = statement.query_map(params![model, limit as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
    }

    /// The entries of one project nearest to `query`, with their similarity.
    pub fn vector_search(&self, project: &str, model: &str, query: &[f32], limit: usize) -> Vec<(MemoryHit, f32)> {
        let rows: Vec<(MemoryHit, Vec<u8>)> = self
            .with_conn(|conn| {
                let mut statement = conn.prepare(
                    "SELECT m.id, m.project, m.ticket_id, m.title, substr(m.body, 1, 200), m.created_at, v.vec
                     FROM memory m JOIN memory_vec v ON v.id = m.id WHERE m.project = ?1 AND v.model = ?2",
                )?;
                let rows = statement.query_map(params![project, model], |row| {
                    Ok((
                        MemoryHit {
                            id: row.get(0)?,
                            project: row.get(1)?,
                            ticket_id: row.get(2)?,
                            title: row.get(3)?,
                            snippet: row.get(4)?,
                            created_at: row.get(5)?,
                        },
                        row.get::<_, Vec<u8>>(6)?,
                    ))
                })?;
                Ok(rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        let mut scored: Vec<(MemoryHit, f32)> = rows
            .into_iter()
            .map(|(hit, blob)| {
                let score = cosine(query, &from_blob(&blob));
                (hit, score)
            })
            .filter(|(_, score)| *score > MIN_SIMILARITY)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
    }

    /// Full-text and vector rankings fused by reciprocal rank: an entry both find comes first,
    /// and one only meaning finds still shows up.
    pub fn memory_search_hybrid(
        &self,
        project: &str,
        query: &str,
        limit: usize,
        embedder: Option<&Embedder>,
    ) -> Result<Vec<MemoryHit>, UserError> {
        let text_hits = self.memory_search(project, query, limit * 2)?;
        let Some(embedder) = embedder.filter(|_| !query.trim().is_empty()) else {
            return Ok(text_hits.into_iter().take(limit).collect());
        };
        let vector = match embedder.embed(query) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e, "embedding the query failed; full-text results only");
                return Ok(text_hits.into_iter().take(limit).collect());
            }
        };
        let near = self.vector_search(project, &embedder.name, &vector, limit * 2);
        let mut fused: Vec<(MemoryHit, f32)> = Vec::new();
        let mut add = |hit: MemoryHit, rank: usize, by_meaning: bool| {
            let score = 1.0 / (60.0 + rank as f32);
            match fused.iter_mut().find(|(h, _)| h.id == hit.id) {
                Some((existing, total)) => {
                    *total += score;
                    // The full-text snippet shows the matching words; keep it when there is one.
                    if !by_meaning {
                        existing.snippet = hit.snippet;
                    }
                }
                None => fused.push((hit, score)),
            }
        };
        for (rank, hit) in text_hits.into_iter().enumerate() {
            add(hit, rank, false);
        }
        for (rank, (hit, _)) in near.into_iter().enumerate() {
            add(hit, rank, true);
        }
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(fused.into_iter().take(limit).map(|(hit, _)| hit).collect())
    }

    /// Computes missing vectors; returns how many were written.
    pub fn vector_backfill(&self, embedder: &Embedder, limit: usize) -> usize {
        let mut written = 0;
        for (id, title, body) in self.vector_missing(&embedder.name, limit) {
            match embedder.embed(&format!("{title}\n{body}")) {
                Ok(vector) => {
                    if self.vector_put(&id, &embedder.name, &vector).is_ok() {
                        written += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(%e, "embedding failed; will retry on the next backfill");
                    break;
                }
            }
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::now_entry;

    #[test]
    fn the_hash_embedder_bridges_inflection_and_keeps_topics_apart() {
        let a = hash_embed("Retrying the login request after the session cache expired");
        let b = hash_embed("login retries when session caches expire");
        let c = hash_embed("Quarterly invoice export to CSV for the finance team");
        assert!(cosine(&a, &b) > 0.35, "related: {}", cosine(&a, &b));
        assert!(cosine(&a, &c) < 0.15, "unrelated: {}", cosine(&a, &c));
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-4);
        assert_eq!(parse_vector("{\"embedding\": [3, 4]}").unwrap(), vec![0.6, 0.8]);
        assert_eq!(parse_vector("[0, 2]").unwrap(), vec![0.0, 1.0]);
        assert!(parse_vector("{\"error\": \"no model\"}").is_none());
    }

    #[test]
    fn hybrid_search_finds_what_full_text_misses_and_stays_inside_the_project() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.memory_migrate().unwrap();
        store.vector_migrate().unwrap();
        let embedder = Embedder::new(&Memory {
            semantic: true,
            ..Memory::default()
        });
        for (project, title, body) in [
            (
                "acme/api",
                "t-1: flaky sign-in",
                "Retried logins failed because session caching expired early.",
            ),
            ("acme/api", "t-2: invoices", "CSV export of invoices for finance."),
            ("acme/web", "t-9: login page", "Login retries on the web client."),
        ] {
            store.memory_add(&now_entry(project, None, None, "worker", title, body)).unwrap();
        }
        assert_eq!(store.vector_backfill(&embedder, 100), 3);
        assert_eq!(store.vector_backfill(&embedder, 100), 0, "nothing is embedded twice");
        // "retry" and "cache" appear only as "Retried" and "caching": full text has no match, meaning does.
        assert!(store.memory_search("acme/api", "retry cache expiry", 5).unwrap().is_empty());
        let hits = store
            .memory_search_hybrid("acme/api", "retry cache expiry", 5, Some(&embedder))
            .unwrap();
        assert_eq!(hits.first().map(|h| h.title.as_str()), Some("t-1: flaky sign-in"));
        assert!(hits.iter().all(|h| h.project == "acme/api"), "memory never crosses projects");
        // Without an embedder it is the full-text search it always was.
        assert!(store
            .memory_search_hybrid("acme/api", "retry cache expiry", 5, None)
            .unwrap()
            .is_empty());
    }
}
