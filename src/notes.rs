//! Curated notes as memory: a folder of markdown files, one fact per file,
//! written by a person or by their own summariser. They are read as they are,
//! never compressed again, and they take the place of imported sessions for
//! their project so the two do not compete in search.
//!
//! A note has frontmatter (`name`, `description`, `type`, the sessions it came
//! from) and a body. `MEMORY.md` is an index, not a fact, and `dropped/` holds
//! what triage rejected: both are skipped.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::daemon::Daemon;
use crate::memory::MemoryEntry;
use crate::profile::NotesSource;

/// Bumped when what a note's entry holds changes, so notes are read again.
const NOTES_VERSION: &str = "n2";

/// The note files of a source: `<path>/*.md`, or `<path>/notes/*.md` when that folder exists.
pub fn files(path: &Path) -> Vec<PathBuf> {
    let dir = if path.join("notes").is_dir() {
        path.join("notes")
    } else {
        path.to_path_buf()
    };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| !n.eq_ignore_ascii_case("MEMORY.md"))
                .unwrap_or(false)
        })
        .collect();
    found.sort();
    found
}

/// A note file as a memory entry. The id follows the file, so an edited note replaces itself.
pub fn entry(file: &Path, text: &str, project: &str) -> Option<MemoryEntry> {
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("note");
    let (front, body) = match text.strip_prefix("---") {
        Some(rest) => match rest.split_once("\n---") {
            Some((front, body)) => (front, body.trim_start_matches('-').trim()),
            None => ("", text.trim()),
        },
        None => ("", text.trim()),
    };
    if body.is_empty() {
        return None;
    }
    let field = |name: &str| {
        front.lines().find_map(|line| {
            line.strip_prefix(&format!("{name}:"))
                .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
                .filter(|v| !v.is_empty())
        })
    };
    let title = field("description")
        .or_else(|| field("name"))
        .unwrap_or_else(|| stem.replace(['-', '_'], " "));
    // Only the note's own text is indexed: frontmatter in the body showed up in search snippets.
    let body = body.to_string();
    let kind = field("type").unwrap_or_default();
    let created_at = std::fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| crate::core::clock::format(t.into()))
        .unwrap_or_else(crate::core::clock::now_iso);
    Some(MemoryEntry {
        id: crate::core::ids::key(&["note", &file.display().to_string()]),
        project: project.to_string(),
        ticket_id: None,
        assignment_id: None,
        role: if kind.is_empty() { "note".into() } else { format!("note-{kind}") },
        title,
        body,
        created_at,
    })
}

/// The memory project of a notes source: its repository's key, else the name as given.
pub fn project_of(source: &NotesSource) -> String {
    crate::memory::project_key(&source.project).unwrap_or_else(|| source.project.clone())
}

impl Daemon {
    /// Reads new and changed notes, forgets notes whose file is gone, and removes imported
    /// sessions of projects that now have notes. Returns what it did.
    pub(crate) fn sync_notes(&self) -> Value {
        let profile = self.policy.snapshot();
        let (mut added, mut removed, mut unchanged) = (0, 0, 0);
        let mut live: Vec<String> = Vec::new();
        let mut covered: Vec<String> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        for source in &profile.memory.notes {
            let root = PathBuf::from(crate::profile::expand_home(&source.path));
            if !root.is_dir() {
                problems.push(format!("{}: not a folder", root.display()));
                continue;
            }
            let project = project_of(source);
            covered.push(project.clone());
            for file in files(&root) {
                let key = format!("notes.{}", file.display());
                live.push(key.clone());
                let Some((mark, _, _)) = crate::importer::mark(&file) else {
                    continue;
                };
                if self.store.kv_get(&key).as_deref() == Some(&format!("{mark}|{project}|{NOTES_VERSION}")) {
                    unchanged += 1;
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&file) else { continue };
                let Some(entry) = entry(&file, &text, &project) else { continue };
                match self.store.memory_add(&entry) {
                    Ok(entry) => {
                        if let Some(repo) = self.state_repo.borrow().as_ref() {
                            let _ = repo.write_curated(&entry);
                        }
                        self.store.kv_set(&key, &format!("{mark}|{project}|{NOTES_VERSION}"));
                        added += 1;
                    }
                    Err(e) => problems.push(format!("{}: {e}", file.display())),
                }
            }
        }
        // A note that was deleted or moved to dropped/ is forgotten.
        let mut gone = Vec::new();
        for (key, value) in self.store.kv_prefix("notes.") {
            if value.is_empty() || live.contains(&key) {
                continue;
            }
            let file = key.trim_start_matches("notes.");
            let id = crate::core::ids::key(&["note", file]);
            if self.store.memory_delete(&id).is_ok() {
                gone.push(id);
                removed += 1;
            }
            self.store.kv_set(&key, "");
        }
        // Notes take the place of imported sessions for their project.
        let mut sessions_removed = 0;
        if !covered.is_empty() {
            if let Ok(ids) = self.store.memory_forget_sessions(Some(&covered)) {
                sessions_removed = ids.len();
                gone.extend(ids);
            }
        }
        if !gone.is_empty() {
            if let Some(repo) = self.state_repo.borrow().as_ref() {
                repo.remove_memory_files(&gone);
            }
        }
        if added + removed + sessions_removed > 0 {
            tracing::info!(added, removed, sessions_removed, "notes synced");
        }
        json!({ "added": added, "unchanged": unchanged, "removed": removed, "sessions_removed": sessions_removed,
            "projects": covered, "problems": problems })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_notes_and_skips_the_index_and_what_was_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        std::fs::create_dir_all(notes.join("dropped")).unwrap();
        let note = "---\nname: orders-incremental\ndescription: The orders model must be incremental on updated_at\ntype: decision\nsessions: [a1, b2]\n---\n\nFull refresh scans 400 GB and times out after 2 h.\n";
        std::fs::write(notes.join("orders-incremental.md"), note).unwrap();
        std::fs::write(notes.join("MEMORY.md"), "- [orders](orders-incremental.md)\n").unwrap();
        std::fs::write(notes.join("dropped").join("noise.md"), "---\nname: noise\n---\nnothing\n").unwrap();
        std::fs::write(notes.join("plain.md"), "A note without frontmatter.\n").unwrap();
        let found = files(dir.path());
        let names: Vec<String> = found.iter().map(|f| f.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["orders-incremental.md", "plain.md"], "found through the notes/ folder");
        assert_eq!(files(&notes).len(), 2, "or given the notes folder itself");

        let made = entry(&found[0], note, "acme/dbt").unwrap();
        assert_eq!(made.title, "The orders model must be incremental on updated_at");
        assert_eq!(made.role, "note-decision");
        assert_eq!(
            made.body, "Full refresh scans 400 GB and times out after 2 h.",
            "the body only, no frontmatter"
        );
        assert_eq!(
            made.id,
            entry(&found[0], "---\nname: x\n---\nedited", "acme/dbt").unwrap().id,
            "an edited note replaces itself"
        );
        let plain = entry(&found[1], "A note without frontmatter.\n", "acme/dbt").unwrap();
        assert_eq!(plain.title, "plain");
        assert!(entry(&found[1], "---\nname: empty\n---\n", "acme/dbt").is_none());
    }
}
