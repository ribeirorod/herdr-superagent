//! Where a worker starts (the base of its ticket branch). A worktree made
//! from the repo's checked-out HEAD inherits whatever is stale there; instead
//! the base is a decision, recorded and explained to the worker: a ref the
//! request names, else the newest of the checked-out branch, its upstream and
//! the remote's default branch, after a fetch.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// The ref the ticket branch starts from, e.g. `origin/main` or `feature/x`.
    pub reference: String,
    pub commit: String,
    pub reason: String,
    /// Recent branches with ahead/behind, for the brief.
    pub overview: String,
}

fn git(repo: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git fetch origin` with a timeout; failures are fine (offline, no remote).
pub fn fetch(repo: &str, timeout: Duration) -> bool {
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fetch", "-q", "--prune", "origin"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return false,
        }
    }
}

/// The remote's default branch (`origin/HEAD`), else `origin/main`/`origin/master`/local `main`/`master`.
pub fn default_ref(repo: &str) -> Option<String> {
    if let Some(head) = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        return Some(head);
    }
    ["origin/main", "origin/master", "main", "master"]
        .iter()
        .map(|r| r.to_string())
        .find(|r| git(repo, &["rev-parse", "--verify", "--quiet", r]).is_some())
}

fn commit_time(repo: &str, reference: &str) -> Option<i64> {
    git(repo, &["log", "-1", "--format=%ct", reference]).and_then(|t| t.parse().ok())
}

/// Recent branches with their tracking state, plus how the local default compares to the remote one.
pub fn overview(repo: &str) -> String {
    let mut lines = Vec::new();
    if let Some(branches) = git(
        repo,
        &[
            "branch",
            "-vv",
            "--sort=-committerdate",
            "--format=%(refname:short)\t%(committerdate:short)\t%(upstream:short)\t%(upstream:track)\t%(subject)",
        ],
    ) {
        lines.push("recent local branches (newest first):".to_string());
        for line in branches.lines().take(8) {
            let parts: Vec<&str> = line.split('\t').collect();
            lines.push(format!(
                "  {} ({}){}{} · {}",
                parts.first().unwrap_or(&""),
                parts.get(1).unwrap_or(&""),
                parts
                    .get(2)
                    .filter(|u| !u.is_empty())
                    .map(|u| format!(" → {u}"))
                    .unwrap_or_default(),
                parts.get(3).filter(|t| !t.is_empty()).map(|t| format!(" {t}")).unwrap_or_default(),
                parts.get(4).unwrap_or(&"").chars().take(60).collect::<String>()
            ));
        }
    }
    if let Some(remote) = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        let local = remote.trim_start_matches("origin/").to_string();
        if let Some(counts) = git(repo, &["rev-list", "--left-right", "--count", &format!("{local}...{remote}")]) {
            let mut it = counts.split_whitespace();
            let (ahead, behind) = (it.next().unwrap_or("0"), it.next().unwrap_or("0"));
            lines.push(format!("local {local} is {ahead} ahead and {behind} behind {remote}"));
        }
    }
    lines.join("\n")
}

/// Chooses the base for a new ticket branch.
pub fn resolve(repo: &str, requested: Option<&str>) -> Base {
    let fetched = fetch(repo, Duration::from_secs(20));
    let overview = overview(repo);
    let exists = |r: &str| git(repo, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")]).is_some();
    let describe = |r: &str| git(repo, &["log", "-1", "--format=%h %cs", r]).unwrap_or_default();
    if let Some(wanted) = requested.filter(|r| !r.trim().is_empty()) {
        for candidate in [wanted.to_string(), format!("origin/{wanted}")] {
            if exists(&candidate) {
                return Base {
                    reference: candidate.clone(),
                    commit: describe(&candidate),
                    reason: format!("the request names `{wanted}`"),
                    overview,
                };
            }
        }
        // Fall through with a note: the wanted ref does not exist.
        let mut base = resolve(repo, None);
        base.reason = format!("the request named `{wanted}`, which does not exist here; {}", base.reason);
        return base;
    }
    let current = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");
    let upstream = git(repo, &["rev-parse", "--abbrev-ref", "@{upstream}"]);
    let default = default_ref(repo);
    let mut candidates: Vec<(String, &str)> = Vec::new();
    if let Some(d) = &default {
        candidates.push((d.clone(), "the remote's default branch"));
    }
    if let Some(u) = &upstream {
        candidates.push((u.clone(), "the upstream of the checked-out branch"));
    }
    if let Some(c) = &current {
        candidates.push((c.clone(), "the checked-out branch"));
    }
    candidates.dedup_by(|a, b| a.0 == b.0);
    let mut best: Option<(String, &str, i64)> = None;
    for (reference, what) in &candidates {
        if let Some(t) = commit_time(repo, reference) {
            if best.as_ref().map(|b| t > b.2).unwrap_or(true) {
                best = Some((reference.clone(), what, t));
            }
        }
    }
    match best {
        Some((reference, what, _)) => {
            let others: Vec<String> = candidates
                .iter()
                .filter(|(r, _)| r != &reference)
                .map(|(r, w)| format!("{r} ({w})"))
                .collect();
            let reason = format!(
                "{what} `{reference}` has the newest commit{}{}",
                if others.is_empty() {
                    String::new()
                } else {
                    format!(", ahead of {}", others.join(" and "))
                },
                if fetched {
                    "; origin was fetched first"
                } else {
                    "; origin could not be fetched (offline or no remote)"
                }
            );
            Base {
                reference: reference.clone(),
                commit: describe(&reference),
                reason,
                overview,
            }
        }
        None => Base {
            reference: "HEAD".into(),
            commit: describe("HEAD"),
            reason: "no branch could be inspected; HEAD as checked out".into(),
            overview,
        },
    }
}

/// `git worktree add -b <branch> <path> <base>` (or reusing an existing branch).
pub fn add_worktree(repo: &str, branch: &str, path: &Path, base: &str) -> Result<(), String> {
    if path.join(".git").exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let exists = git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_some();
    let mut args = vec!["-C".to_string(), repo.to_string(), "worktree".into(), "add".into()];
    // On another machine the ticket's branch exists only on the remote: continue from there.
    let pushed = format!("origin/{branch}");
    let base = if !exists && git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{pushed}")]).is_some() {
        pushed.as_str()
    } else {
        base
    };
    if exists {
        args.extend([path.display().to_string(), branch.to_string()]);
    } else {
        args.extend(["-b".to_string(), branch.to_string(), path.display().to_string(), base.to_string()]);
    }
    let output = Command::new("git").args(&args).output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Removes a ticket's worktree when it is one of the repository's and has nothing
/// uncommitted; the branch stays. Returns why it was kept, if it was.
pub fn remove_worktree(repo: &str, directory: &str) -> Option<String> {
    let dir = std::path::Path::new(directory);
    if directory.is_empty() || !dir.exists() || !std::path::Path::new(repo).join(".git").exists() {
        return None;
    }
    // Only a linked worktree has a `.git` file; the person's own checkout has a directory and is never touched.
    if !dir.join(".git").is_file() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["-C", repo, "worktree", "remove", directory])
        .output();
    match output {
        Ok(out) if out.status.success() => None,
        Ok(out) => Some(format!(
            "worktree {directory} kept: {}",
            String::from_utf8_lossy(&out.stderr).trim().lines().next().unwrap_or("git refused")
        )),
        Err(e) => Some(format!("worktree {directory} kept: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn prefers_the_newest_of_default_upstream_and_current_and_honours_a_request() {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin.git");
        Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&origin)
            .status()
            .unwrap();
        let repo = root.path().join("repo");
        Command::new("git").args(["clone", "-q"]).arg(&origin).arg(&repo).status().unwrap();
        sh(&repo, &["checkout", "-q", "-b", "main"]);
        sh(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        sh(&repo, &["push", "-q", "-u", "origin", "main"]);
        // Someone else advances origin/main by two commits; local main stays behind.
        let other = root.path().join("other");
        Command::new("git").args(["clone", "-q"]).arg(&origin).arg(&other).status().unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        sh(&other, &["commit", "-q", "--allow-empty", "-m", "two"]);
        sh(&other, &["commit", "-q", "--allow-empty", "-m", "three"]);
        sh(&other, &["push", "-q", "origin", "main"]);
        sh(&repo, &["remote", "set-head", "origin", "main"]);
        let repo_s = repo.display().to_string();
        let base = resolve(&repo_s, None);
        assert_eq!(base.reference, "origin/main", "{}", base.reason);
        assert!(base.reason.contains("newest commit"));
        assert!(base.overview.contains("behind origin/main"), "{}", base.overview);
        // A feature branch the person is on, newer than origin/main, wins.
        std::thread::sleep(Duration::from_millis(1100));
        sh(&repo, &["checkout", "-q", "-b", "feature/x"]);
        sh(&repo, &["commit", "-q", "--allow-empty", "-m", "wip"]);
        assert_eq!(resolve(&repo_s, None).reference, "feature/x");
        // A requested ref wins over everything; a missing one falls back with a note.
        assert_eq!(resolve(&repo_s, Some("main")).reference, "main");
        let missing = resolve(&repo_s, Some("nope"));
        assert_eq!(missing.reference, "feature/x");
        assert!(missing.reason.contains("does not exist"));
        let wt = root.path().join("wt");
        add_worktree(&repo_s, "sa/1", &wt, "origin/main").unwrap();
        assert_eq!(
            git(&wt.display().to_string(), &["log", "-1", "--format=%s"]).as_deref(),
            Some("three")
        );
    }
}
