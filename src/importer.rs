//! Session importer (phase 5): turns the sessions you already had with
//! Claude Code, Codex and Pi into per-project memory, so the first ticket in a
//! repository does not start from nothing.
//!
//! It is extractive: no model reads the transcript. From each session it
//! keeps what was asked, what was asked next, the last answer and the files
//! that were changed, redacted like every other memory entry. Raw transcripts
//! never leave their place. A session is imported again only when its file
//! changed, and the same session always maps to the same entry.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::memory::MemoryEntry;

/// Larger transcripts are skipped: reading them costs more than they are worth.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Session {
    pub harness: String,
    pub id: String,
    pub cwd: String,
    pub started: String,
    pub branch: String,
    /// What the person typed, in order, without tool results and injected context.
    pub asked: Vec<String>,
    pub last_answer: String,
    pub files: Vec<String>,
}

/// Why a session file produced no entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// Nothing a person typed, or no answer.
    Empty,
    /// A superagent or worker session: its ticket already has a summary.
    Own,
    /// Not inside one of the profile's repositories.
    Outside,
    /// The file has not changed since it was imported.
    Unchanged,
    Unreadable,
    TooOld,
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    [
        value.get(key),
        value.pointer(&format!("/payload/{key}")),
        value.pointer(&format!("/message/{key}")),
    ]
    .into_iter()
    .flatten()
    .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
}

/// Paths a patch touches: `*** Update File: src/a.rs` lines (Codex's apply_patch).
fn patch_files(patch: &str, files: &mut Vec<String>) {
    for line in patch.lines() {
        for marker in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
            if let Some(path) = line.trim().strip_prefix(marker) {
                push_file(files, path.trim());
            }
        }
    }
}

fn push_file(files: &mut Vec<String>, path: &str) {
    if !path.is_empty() && !files.iter().any(|f| f == path) {
        files.push(path.to_string());
    }
}

const WRITING_TOOLS: &[&str] = &[
    "edit",
    "write",
    "multiedit",
    "notebookedit",
    "apply_patch",
    "str_replace",
    "create",
    "str_replace_editor",
];

fn tool_call(name: &str, input: &Value, files: &mut Vec<String>) {
    if !WRITING_TOOLS.contains(&name.to_lowercase().as_str()) {
        return;
    }
    // Arguments arrive as an object, or as a JSON string holding one.
    let parsed;
    let input = match input.as_str() {
        Some(text) => {
            patch_files(text, files);
            parsed = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
            &parsed
        }
        None => input,
    };
    for key in ["file_path", "path", "notebook_path", "filePath"] {
        if let Some(path) = input.get(key).and_then(Value::as_str) {
            push_file(files, path);
        }
    }
    for key in ["input", "patch"] {
        if let Some(patch) = input.get(key).and_then(Value::as_str) {
            patch_files(patch, files);
        }
    }
}

/// The readable text of a message's content, collecting changed files on the way.
fn content_text(content: &Value, files: &mut Vec<String>) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            out.push(text.to_string());
                        }
                    }
                    "tool_use" | "toolCall" | "tool_call" => {
                        let name = part.get("name").and_then(Value::as_str).unwrap_or("");
                        let input = part.get("input").or_else(|| part.get("arguments")).unwrap_or(&Value::Null);
                        tool_call(name, input, files);
                    }
                    _ => {}
                }
            }
            out.join("\n")
        }
        _ => String::new(),
    }
}

/// Text a harness injects into the person's turn: not something they asked.
fn injected(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with('<')
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
        || t.starts_with("This session is being continued")
        || t.starts_with("[superagent daemon]")
}

fn own_session(first: &str) -> bool {
    let t = first.trim_start();
    t.starts_with("You are the superagent for herdr profile")
        || (t.starts_with("Read ") && (t.contains("start on ticket") || t.contains("do the task it gives you for ticket")))
}

/// Reads one transcript (JSON lines) of any of the three harnesses.
pub fn parse(harness: &str, path: &Path, text: &str) -> Result<Session, Skip> {
    let mut session = Session {
        harness: harness.to_string(),
        id: path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string(),
        ..Session::default()
    };
    let mut any = false;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        any = true;
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) || value.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if session.cwd.is_empty() {
            session.cwd = text_of(&value, "cwd").unwrap_or_default();
        }
        if session.started.is_empty() {
            session.started = text_of(&value, "timestamp").unwrap_or_default();
        }
        if session.branch.is_empty() {
            session.branch = value
                .get("gitBranch")
                .or_else(|| value.pointer("/payload/git/branch"))
                .or_else(|| value.pointer("/git/branch"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(kind, "session" | "session_meta") {
            if let Some(id) = value.get("id").or_else(|| value.pointer("/payload/id")).and_then(Value::as_str) {
                session.id = id.to_string();
            }
            continue;
        }
        if let Some(id) = value.get("sessionId").and_then(Value::as_str) {
            session.id = id.to_string();
        }
        // Codex records tool calls as their own items.
        let payload = value.get("payload").unwrap_or(&value);
        if matches!(
            payload.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call")
        ) {
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
            let input = payload.get("arguments").or_else(|| payload.get("input")).unwrap_or(&Value::Null);
            tool_call(name, input, &mut session.files);
            continue;
        }
        let holder = [value.get("message"), value.get("payload"), Some(&value)]
            .into_iter()
            .flatten()
            .find(|c| c.get("role").and_then(Value::as_str).is_some() && c.get("content").is_some());
        let Some(holder) = holder else { continue };
        let role = holder.get("role").and_then(Value::as_str).unwrap_or("");
        let body = content_text(&holder["content"], &mut session.files);
        let body = body.trim();
        match role {
            "user" if !injected(body) => {
                if session.asked.last().map(String::as_str) != Some(body) {
                    session.asked.push(body.to_string());
                }
            }
            "assistant" if !body.is_empty() => session.last_answer = body.to_string(),
            _ => {}
        }
    }
    if !any {
        return Err(Skip::Unreadable);
    }
    match session.asked.first() {
        None => Err(Skip::Empty),
        Some(first) if own_session(first) => Err(Skip::Own),
        Some(_) if session.last_answer.is_empty() => Err(Skip::Empty),
        Some(_) => Ok(session),
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>().trim_end())
    }
}

fn first_line(text: &str, max: usize) -> String {
    clip(text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""), max)
}

/// The memory entry of a session: the same session always gets the same id.
pub fn entry(session: &Session, project: &str) -> MemoryEntry {
    let date: String = session.started.chars().take(10).collect();
    let goal = session.asked.first().cloned().unwrap_or_default();
    let mut body = format!("## Goal\n\n{}\n", clip(&goal, 700));
    if session.asked.len() > 1 {
        body.push_str("\n## What was asked next\n\n");
        for ask in session.asked.iter().skip(1).take(8) {
            body.push_str(&format!("- {}\n", first_line(ask, 160)));
        }
        if session.asked.len() > 9 {
            body.push_str(&format!("- … and {} more\n", session.asked.len() - 9));
        }
    }
    body.push_str(&format!(
        "\n## Outcome (the session's last answer)\n\n{}\n",
        clip(&session.last_answer, 1500)
    ));
    body.push_str("\n## Files and links\n\n");
    let cwd = session.cwd.trim_end_matches('/');
    for file in session.files.iter().take(20) {
        let shown = file.strip_prefix(cwd).map(|f| f.trim_start_matches('/')).unwrap_or(file);
        body.push_str(&format!("- {shown}\n"));
    }
    if session.files.len() > 20 {
        body.push_str(&format!("- … and {} more files\n", session.files.len() - 20));
    }
    if !session.branch.is_empty() {
        body.push_str(&format!("- branch: {}\n", session.branch));
    }
    body.push_str(&format!(
        "- imported from a {} session of {} ({} messages from the person)\n",
        session.harness,
        if date.is_empty() { "unknown date" } else { &date },
        session.asked.len()
    ));
    MemoryEntry {
        id: crate::core::ids::key(&["session", &session.harness, &session.id]),
        project: project.to_string(),
        ticket_id: None,
        assignment_id: None,
        role: format!("session-{}", session.harness),
        title: format!(
            "{} session{}: {}",
            session.harness,
            if date.is_empty() { String::new() } else { format!(" {date}") },
            first_line(&goal, 90)
        ),
        body,
        created_at: if session.started.is_empty() {
            crate::core::clock::now_iso()
        } else {
            crate::core::clock::normalize(&session.started)
        },
    }
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                walk(&path, depth - 1, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// Where each harness keeps its transcripts, honouring the harness's own overrides.
pub fn roots(home: &Path) -> Vec<(String, PathBuf)> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    vec![
        (
            "claude".to_string(),
            var("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude")).join("projects"),
        ),
        (
            "codex".to_string(),
            var("CODEX_HOME").unwrap_or_else(|| home.join(".codex")).join("sessions"),
        ),
        (
            "pi".to_string(),
            var("PI_CODING_AGENT_DIR")
                .unwrap_or_else(|| home.join(".pi").join("agent"))
                .join("sessions"),
        ),
    ]
}

/// Every transcript file of the chosen harnesses, as `(harness, path)`.
pub fn discover(home: &Path, harness: Option<&str>) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    for (name, root) in roots(home) {
        if harness.map(|h| h != "all" && h != name).unwrap_or(false) {
            continue;
        }
        let mut files = Vec::new();
        walk(&root, 5, &mut files);
        files.sort();
        found.extend(files.into_iter().map(|f| (name.clone(), f)));
    }
    found
}

/// `mtime:size`, the mark that says whether a file changed since its import.
pub fn mark(path: &Path) -> Option<(String, u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some((format!("{modified}:{}", meta.len()), modified, meta.len()))
}

/// What a run decided about one file.
pub enum Outcome {
    Import(Box<MemoryEntry>, String),
    Skip(Skip),
}

pub struct Options<'a> {
    /// The profile's repositories: sessions elsewhere are left alone unless `all`.
    pub repos: &'a [String],
    pub all: bool,
    /// Only this project key.
    pub project: Option<&'a str>,
    pub since_days: Option<u64>,
}

/// Decides one file: unchanged, skipped, or the entry to store with the file's new mark.
pub fn consider(harness: &str, path: &Path, known_mark: Option<&str>, options: &Options) -> Outcome {
    let Some((stamp, modified, size)) = mark(path) else {
        return Outcome::Skip(Skip::Unreadable);
    };
    if known_mark == Some(stamp.as_str()) {
        return Outcome::Skip(Skip::Unchanged);
    }
    if let Some(days) = options.since_days {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now.saturating_sub(modified) > days * 86_400 {
            return Outcome::Skip(Skip::TooOld);
        }
    }
    if size > MAX_BYTES {
        return Outcome::Skip(Skip::Unreadable);
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Outcome::Skip(Skip::Unreadable);
    };
    let session = match parse(harness, path, &text) {
        Ok(session) => session,
        Err(skip) => return Outcome::Skip(skip),
    };
    let cwd = Path::new(&session.cwd);
    let inside = options.repos.iter().any(|repo| cwd.starts_with(repo));
    if !inside && !options.all {
        return Outcome::Skip(Skip::Outside);
    }
    let project = crate::memory::project_key(&session.cwd)
        .or_else(|| cwd.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .unwrap_or_else(|| crate::memory::NOTES_PROJECT.to_string());
    if options.project.map(|p| p != project).unwrap_or(false) {
        return Outcome::Skip(Skip::Outside);
    }
    Outcome::Import(Box::new(entry(&session, &project)), stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"{"type":"summary","summary":"Login fix"}
{"type":"user","sessionId":"s-1","cwd":"/src/api","gitBranch":"fix/login","timestamp":"2026-10-02T09:00:00.000Z","message":{"role":"user","content":"Fix the flaky login test\nIt fails one run in five."}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Edit","input":{"file_path":"/src/api/auth/session.rs","old_string":"a","new_string":"b"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}
{"type":"user","message":{"role":"user","content":"<system-reminder>ignore</system-reminder>"}}
{"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}
{"type":"user","message":{"role":"user","content":"Also add a regression test"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The flake was a race in the session cache; fixed with a lock and covered by a test. API_KEY=sk-abc123def456ghi789jkl012mno345pqr"}]}}
"#;

    const CODEX: &str = r#"{"timestamp":"2026-10-03T10:00:00Z","type":"session_meta","payload":{"id":"c-9","cwd":"/src/api","git":{"branch":"main"}}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Rename the export module"}]}}
{"type":"event_msg","payload":{"type":"user_message","message":"Rename the export module"}}
{"type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** Update File: src/export.rs\\n*** End Patch\"}"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Renamed to exporter."}]}}
"#;

    const PI: &str = r#"{"type":"session","id":"p-3","timestamp":"2026-10-04T08:00:00Z","cwd":"/src/web"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Why is the build slow?"}]}}
{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","name":"write","arguments":{"path":"notes/build.md"}},{"type":"text","text":"The bundler rebuilds everything; cache it."}]}}
"#;

    #[test]
    fn reads_what_was_asked_answered_and_changed_from_each_harness() {
        let claude = parse("claude", Path::new("/x/s-1.jsonl"), CLAUDE).unwrap();
        assert_eq!(claude.id, "s-1");
        assert_eq!(claude.cwd, "/src/api");
        assert_eq!(claude.branch, "fix/login");
        assert_eq!(
            claude.asked.len(),
            2,
            "tool results, injected context and subagent turns are not the person: {:?}",
            claude.asked
        );
        assert_eq!(claude.files, vec!["/src/api/auth/session.rs"]);
        assert!(claude.last_answer.contains("race in the session cache"));

        let codex = parse("codex", Path::new("/x/rollout-1.jsonl"), CODEX).unwrap();
        assert_eq!(
            (codex.id.as_str(), codex.cwd.as_str(), codex.branch.as_str()),
            ("c-9", "/src/api", "main")
        );
        assert_eq!(
            codex.asked,
            vec!["Rename the export module"],
            "the echoed user message is counted once"
        );
        assert_eq!(codex.files, vec!["src/export.rs"]);

        let pi = parse("pi", Path::new("/x/p.jsonl"), PI).unwrap();
        assert_eq!((pi.id.as_str(), pi.cwd.as_str()), ("p-3", "/src/web"));
        assert_eq!(pi.files, vec!["notes/build.md"]);
        assert!(pi.last_answer.contains("bundler"));
    }

    #[test]
    fn the_entry_is_stable_extractive_and_skips_our_own_sessions() {
        let session = parse("claude", Path::new("/x/s-1.jsonl"), CLAUDE).unwrap();
        let first = entry(&session, "acme/api");
        assert_eq!(first.id, entry(&session, "acme/api").id, "the same session is the same entry");
        assert_eq!(first.title, "claude session 2026-10-02: Fix the flaky login test");
        assert!(first.body.contains("## Goal") && first.body.contains("It fails one run in five."));
        assert!(first.body.contains("- Also add a regression test"));
        assert!(
            first.body.contains("- auth/session.rs"),
            "paths are shown relative to the session's directory"
        );
        assert!(first.body.contains("- branch: fix/login"));
        assert_eq!(first.created_at, "2026-10-02T09:00:00.000Z");
        // The store redacts on the way in, like every other entry.
        let store = crate::store::sqlite::SqliteStore::open_in_memory().unwrap();
        let stored = store.memory_add(&first).unwrap();
        assert!(!stored.body.contains("sk-abc123"));

        let worker = r#"{"type":"user","cwd":"/w","message":{"role":"user","content":"Read /state/briefs/t-1.md and start on ticket t-1."}}
{"type":"assistant","message":{"role":"assistant","content":"ok"}}"#;
        assert_eq!(parse("claude", Path::new("/x/w.jsonl"), worker).unwrap_err(), Skip::Own);
        assert_eq!(
            parse("claude", Path::new("/x/e.jsonl"), "{\"type\":\"summary\"}").unwrap_err(),
            Skip::Empty
        );
        assert_eq!(parse("claude", Path::new("/x/e.jsonl"), "not json").unwrap_err(), Skip::Unreadable);
    }

    #[test]
    fn a_run_imports_only_the_profiles_repositories_and_only_changed_files() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects/-src-api");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s-1.jsonl");
        std::fs::write(&file, CLAUDE).unwrap();
        std::fs::create_dir_all(home.path().join(".pi/agent/sessions/web")).unwrap();
        std::fs::write(home.path().join(".pi/agent/sessions/web/p.jsonl"), PI).unwrap();
        let found = discover(home.path(), None);
        assert_eq!(found.len(), 2);
        assert_eq!(discover(home.path(), Some("pi")).len(), 1);
        let repos = vec!["/src/api".to_string()];
        let options = Options {
            repos: &repos,
            all: false,
            project: None,
            since_days: None,
        };
        let stamp = match consider("claude", &file, None, &options) {
            Outcome::Import(entry, stamp) => {
                assert_eq!(entry.project, "api", "a directory that is gone is keyed by its name");
                stamp
            }
            Outcome::Skip(skip) => panic!("skipped: {skip:?}"),
        };
        assert!(matches!(
            consider("claude", &file, Some(&stamp), &options),
            Outcome::Skip(Skip::Unchanged)
        ));
        let pi = home.path().join(".pi/agent/sessions/web/p.jsonl");
        assert!(matches!(consider("pi", &pi, None, &options), Outcome::Skip(Skip::Outside)));
        let everything = Options { all: true, ..options };
        assert!(matches!(consider("pi", &pi, None, &everything), Outcome::Import(..)));
    }
}
