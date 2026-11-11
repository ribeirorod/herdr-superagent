//! Guard shims for autonomous mode (PLAN §6): thin wrappers on the worker's
//! PATH for `git`, `gh`, `bb` and `rm` that call `herdr-superagent guard`.
//! A risky invocation becomes a checkpoint instead of running; everything
//! else is passed through to the real tool. A guard rail, not a sandbox.

use std::path::{Path, PathBuf};

pub const GUARDED: &[&str] = &["git", "gh", "bb", "rm"];

/// Why an invocation is held back, if it is.
pub fn risk(tool: &str, args: &[String], worktree: Option<&Path>, default_branch: Option<&str>) -> Option<String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match tool {
        "git" => {
            let sub = words.iter().find(|w| !w.starts_with('-')).copied().unwrap_or("");
            match sub {
                "push" => {
                    if words
                        .iter()
                        .any(|w| *w == "--force" || *w == "-f" || w.starts_with("--force-with-lease") || *w == "--delete" || *w == "-d")
                    {
                        return Some("git push with --force or --delete".into());
                    }
                    let default = default_branch.unwrap_or("main");
                    let targets: Vec<&str> = words
                        .iter()
                        .skip_while(|w| **w != "push")
                        .skip(1)
                        .filter(|w| !w.starts_with('-'))
                        .copied()
                        .collect();
                    // `git push origin main`, `git push origin HEAD:main`, or a bare push while on the default branch.
                    let to_default = targets
                        .iter()
                        .skip(1)
                        .any(|t| t.rsplit(':').next() == Some(default) || *t == default || t.rsplit(':').next() == Some("master"))
                        || (targets.len() <= 1 && default_branch_is_current(worktree, default));
                    to_default.then(|| format!("git push to the default branch ({default})"))
                }
                "reset" if words.contains(&"--hard") => Some("git reset --hard".into()),
                "clean" if words.iter().any(|w| w.starts_with("-f") || w.starts_with("-x")) => Some("git clean -f".into()),
                "branch" if words.iter().any(|w| *w == "-D" || *w == "--delete" || *w == "-d") => Some("git branch delete".into()),
                _ => None,
            }
        }
        "gh" | "bb" => {
            let joined = words.join(" ");
            let risky = [
                "pr merge",
                "repo delete",
                "release create",
                "repo create",
                "secret set",
                "workflow run",
            ];
            risky.iter().find(|r| joined.starts_with(*r)).map(|r| format!("{tool} {r}"))
        }
        "rm" => {
            let recursive = words.iter().any(|w| w.starts_with('-') && (w.contains('r') || w.contains('R')));
            if !recursive {
                return None;
            }
            let Some(worktree) = worktree else {
                return Some("rm -r outside a known worktree".into());
            };
            let worktree = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
            for target in words.iter().filter(|w| !w.starts_with('-')) {
                let path = PathBuf::from(target);
                let absolute = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir().unwrap_or_default().join(path)
                };
                let absolute = std::fs::canonicalize(&absolute).unwrap_or(absolute);
                if !absolute.starts_with(&worktree) || absolute == worktree {
                    return Some(format!("rm -r of {} outside the worktree", absolute.display()));
                }
            }
            None
        }
        _ => None,
    }
}

fn default_branch_is_current(worktree: Option<&Path>, default: &str) -> bool {
    let Some(dir) = worktree else { return false };
    std::process::Command::new("git")
        .args(["-C", &dir.display().to_string(), "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == default)
        .unwrap_or(false)
}

/// The repository's default branch from `origin/HEAD`, else `main`.
pub fn default_branch(worktree: Option<&Path>) -> String {
    let Some(dir) = worktree else { return "main".into() };
    std::process::Command::new("git")
        .args([
            "-C",
            &dir.display().to_string(),
            "symbolic-ref",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .rsplit('/')
                .next()
                .unwrap_or("main")
                .to_string()
        })
        .unwrap_or_else(|| "main".into())
}

/// The real tool, found on PATH outside the shims directory.
pub fn real_tool(tool: &str, shims: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| std::fs::canonicalize(dir).map(|d| d != shims).unwrap_or(true) && dir != shims)
        .map(|dir| dir.join(tool))
        .find(|p| p.is_file())
}

/// PATH without the shims directory.
pub fn path_without(shims: &Path) -> std::ffi::OsString {
    let canonical = std::fs::canonicalize(shims).unwrap_or_else(|_| shims.to_path_buf());
    let path = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|dir| dir != shims && std::fs::canonicalize(dir).map(|d| d != canonical).unwrap_or(true))
        .collect();
    std::env::join_paths(kept).unwrap_or(path)
}

/// Writes the shim scripts into `dir`; returns the directory to prepend to PATH.
pub fn write_shims(dir: &Path, binary: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for tool in GUARDED {
        let path = dir.join(tool);
        std::fs::write(&path, format!("#!/bin/sh\n# herdr-superagent guard shim: risky actions become checkpoints in autonomous mode.\nexec '{binary}' guard {tool} -- \"$@\"\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn flags_risky_invocations_and_passes_the_rest() {
        assert!(risk("git", &a(&["push", "--force", "origin", "sa/1"]), None, Some("main")).is_some());
        assert!(risk("git", &a(&["push", "origin", "main"]), None, Some("main")).is_some());
        assert!(risk("git", &a(&["push", "origin", "HEAD:main"]), None, Some("main")).is_some());
        assert!(risk("git", &a(&["push", "-u", "origin", "sa/1"]), None, Some("main")).is_none());
        assert!(risk("git", &a(&["status"]), None, None).is_none());
        assert!(risk("git", &a(&["reset", "--hard", "HEAD~1"]), None, None).is_some());
        assert!(risk("gh", &a(&["pr", "merge", "12"]), None, None).is_some());
        assert!(risk("gh", &a(&["pr", "view", "12"]), None, None).is_none());
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("build");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(risk("rm", &a(&["-rf", &inside.display().to_string()]), Some(dir.path()), None).is_none());
        assert!(risk("rm", &a(&["-rf", "/tmp"]), Some(dir.path()), None).is_some());
        assert!(
            risk("rm", &a(&["-rf", &dir.path().display().to_string()]), Some(dir.path()), None).is_some(),
            "the worktree itself"
        );
        assert!(risk("rm", &a(&["file.txt"]), Some(dir.path()), None).is_none());
        let shims = write_shims(&dir.path().join("shims"), "/usr/local/bin/herdr-superagent").unwrap();
        assert!(shims.join("git").is_file());
        assert!(std::fs::read_to_string(shims.join("rm")).unwrap().contains("guard rm --"));
    }
}
