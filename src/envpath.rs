//! The PATH the user's shell has, wherever it is configured. herdr panes and
//! plugin commands often start with a non-login PATH, so instead of guessing
//! install directories, ask the login shell once and merge its PATH with ours.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Runs `$SHELL -lic 'printf %s "$PATH"'` with a timeout; `None` when it fails.
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    // -l sources the profile, -i the rc file: PATH is set in either, depending on the shell and the person.
    let mut child = Command::new(&shell)
        .args(["-lic", "printf '%s' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("HERDR_SUPERAGENT_DEBUG")
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(5) => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let output = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Interactive shells may print banners; the PATH is the last line that looks like one.
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| line.contains('/') && !line.contains(' '))
        .map(str::to_string)
}

/// The current PATH followed by the login shell's entries that are missing from it.
pub fn full_path() -> String {
    static FULL: OnceLock<String> = OnceLock::new();
    FULL.get_or_init(|| {
        let current = std::env::var("PATH").unwrap_or_default();
        let mut entries: Vec<String> = std::env::split_paths(&current)
            .map(|p| p.display().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if let Some(login) = login_shell_path() {
            for entry in login.split(':').filter(|e| !e.is_empty()) {
                if !entries.iter().any(|e| e == entry) {
                    entries.push(entry.to_string());
                }
            }
        }
        entries.join(":")
    })
    .clone()
}

/// This binary's absolute path, symlinks resolved: what briefs tell agents to
/// run when a bare `herdr-superagent` is not on their shell's PATH.
pub fn self_path() -> String {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "herdr-superagent".to_string())
}

/// Finds an executable on the full PATH: (path, on the current PATH).
pub fn locate(name: &str) -> Option<(String, bool)> {
    let current = std::env::var("PATH").unwrap_or_default();
    if let Some(found) = std::env::split_paths(&current).map(|d| d.join(name)).find(|p| p.is_file()) {
        return Some((found.display().to_string(), true));
    }
    std::env::split_paths(&full_path())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .map(|p| (p.display().to_string(), false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_path_keeps_the_current_entries_first_and_finds_binaries() {
        let current = std::env::var("PATH").unwrap_or_default();
        let full = full_path();
        assert!(full.starts_with(&current) || current.is_empty());
        let (path, on_path) = locate("sh").expect("sh exists");
        assert!(path.ends_with("/sh") && on_path);
        assert!(locate("definitely-not-a-binary-xyz").is_none());
    }
}
