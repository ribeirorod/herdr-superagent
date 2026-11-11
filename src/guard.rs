//! Guard rails for workers, in every mode (PLAN §6). Two layers:
//!
//! * shims on the worker's PATH for `git`, `gh`, `bb` and `rm` that call
//!   `herdr-superagent guard`: a risky invocation becomes a checkpoint instead
//!   of running, everything else is passed through to the real tool;
//! * a `pre-push` hook in force for the worker's pane, which git itself runs
//!   however it was started: no protected branch (the default branch, `main`,
//!   `master`) is updated on a remote without an approved checkpoint.
//!
//! Rails, not a sandbox: the remote's own branch protection is the wall.

use std::path::{Path, PathBuf};

pub const GUARDED: &[&str] = &["git", "gh", "bb", "rm"];

/// A git invocation split into what matters: where it runs, the subcommand, and its arguments.
struct GitCall<'a> {
    /// `-C <dir>`: the repository it acts on instead of the current directory.
    dir: Option<&'a str>,
    sub: &'a str,
    args: Vec<&'a str>,
}

fn git_call<'a>(words: &[&'a str]) -> GitCall<'a> {
    let mut dir = None;
    let mut i = 0;
    while i < words.len() {
        match words[i] {
            "-C" => {
                dir = words.get(i + 1).copied();
                i += 2;
            }
            // Global options that take a value.
            "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => i += 2,
            w if w.starts_with('-') => i += 1,
            _ => break,
        }
    }
    GitCall {
        dir,
        sub: words.get(i).copied().unwrap_or(""),
        args: words.get(i + 1..).map(|a| a.to_vec()).unwrap_or_default(),
    }
}

/// Subcommands that change a repository.
const GIT_WRITES: &[&str] = &[
    "add",
    "am",
    "apply",
    "branch",
    "checkout",
    "cherry-pick",
    "clean",
    "commit",
    "merge",
    "mv",
    "pull",
    "push",
    "rebase",
    "reset",
    "restore",
    "revert",
    "rm",
    "stash",
    "switch",
    "tag",
    "update-ref",
    "worktree",
];

/// Whether `name` is a branch nobody pushes or merges to without approval.
fn protected(name: &str, default: &str) -> bool {
    let name = name.trim_start_matches("refs/heads/");
    name == default || name == "main" || name == "master"
}

/// Why an invocation is held back, if it is.
pub fn risk(tool: &str, args: &[String], worktree: Option<&Path>, default_branch: Option<&str>) -> Option<String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let default = default_branch.unwrap_or("main");
    match tool {
        "git" => {
            let call = git_call(&words);
            let has = |flags: &[&str]| call.args.iter().any(|w| flags.contains(w));
            // Writing to another checkout (the person's own) is never the worker's to do.
            if let (Some(dir), Some(worktree)) = (call.dir, worktree) {
                let target = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
                let home = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
                if !target.starts_with(&home) && GIT_WRITES.contains(&call.sub) {
                    return Some(format!("git {} in {dir}, outside the ticket's worktree", call.sub));
                }
            }
            let here = call.dir.map(Path::new).or(worktree);
            match call.sub {
                "push" => {
                    if call
                        .args
                        .iter()
                        .any(|w| *w == "--force" || *w == "-f" || w.starts_with("--force-with-lease") || *w == "--delete" || *w == "-d")
                    {
                        return Some("git push with --force or --delete".into());
                    }
                    if has(&["--all", "--mirror", "--branches"]) {
                        return Some("git push of every branch".into());
                    }
                    let targets: Vec<&str> = call.args.iter().filter(|w| !w.starts_with('-')).copied().collect();
                    // `git push origin main`, `git push origin HEAD:main`, or a bare push while on a protected branch.
                    let named = targets
                        .iter()
                        .skip(1)
                        .any(|t| protected(t.rsplit(':').next().unwrap_or(t).trim_start_matches('+'), default));
                    let implied = targets.len() <= 1 && on_protected_branch(here, default);
                    (named || implied).then(|| format!("git push to a protected branch ({default})"))
                }
                // Anything that moves a protected branch locally: the next push would carry it.
                "merge" | "rebase" | "commit" | "cherry-pick" | "revert" | "pull" | "am" if on_protected_branch(here, default) => {
                    Some(format!("git {} on a protected branch ({default})", call.sub))
                }
                "update-ref" if call.args.iter().any(|w| protected(w, default) && w.starts_with("refs/heads/")) => {
                    Some("git update-ref of a protected branch".into())
                }
                "branch" if has(&["-f", "--force", "-M"]) && call.args.iter().any(|w| protected(w, default)) => {
                    Some("git branch -f on a protected branch".into())
                }
                "checkout" | "switch" if has(&["-B", "-C", "--force-create"]) && call.args.iter().any(|w| protected(w, default)) => {
                    Some("resetting a protected branch".into())
                }
                "reset" if has(&["--hard"]) => Some("git reset --hard".into()),
                "clean" if call.args.iter().any(|w| w.starts_with("-f") || w.starts_with("-x")) => Some("git clean -f".into()),
                "branch" if has(&["-D", "--delete", "-d"]) => Some("git branch delete".into()),
                _ => None,
            }
        }
        "gh" | "bb" => {
            let joined = words.join(" ");
            let risky = [
                "pr merge",
                "pullrequest merge",
                "repo delete",
                "release create",
                "repo create",
                "secret set",
                "workflow run",
            ];
            if let Some(r) = risky.iter().find(|r| joined.starts_with(*r)) {
                return Some(format!("{tool} {r}"));
            }
            // The same merge through the raw API.
            (words.first() == Some(&"api") && joined.contains("/merge")).then(|| format!("{tool} api merge"))
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

/// What a push would update, from the lines git gives a pre-push hook
/// (`<local ref> <local sha> <remote ref> <remote sha>`): the protected branches among them.
pub fn protected_pushes(hook_input: &str, default: &str) -> Vec<String> {
    hook_input
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter(|remote| remote.starts_with("refs/heads/") && protected(remote, default))
        .map(str::to_string)
        .collect()
}

/// Client-side hooks git knows; each gets a wrapper so the repository's own keep running.
const HOOKS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "post-rewrite",
    "pre-auto-gc",
    "reference-transaction",
];

/// Writes a hooks directory for a ticket: a `pre-push` that asks the guard before a
/// protected branch is updated, and wrappers that run the repository's own hooks.
/// It is put in force for the worker's pane only, through git's environment, so the
/// repository itself is not touched.
pub fn write_hooks(dir: &Path, binary: &str, repo: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // Where the repository's own hooks are: its configured path, else `.git/hooks`.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let own = match git(&["config", "--get", "core.hooksPath"]) {
        Some(path) if Path::new(&path).is_absolute() => format!("'{path}'"),
        // A relative path is relative to the top of whichever checkout runs the hook.
        Some(path) => format!("\"$(git rev-parse --show-toplevel)\"/'{path}'"),
        None => {
            let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .unwrap_or_else(|| repo.join(".git").display().to_string());
            format!("'{common}/hooks'")
        }
    };
    for name in HOOKS {
        let guard = if *name == "pre-push" {
            format!("input=$(cat)\nprintf '%s\\n' \"$input\" | '{binary}' guard-push \"$@\" || exit 1\n")
        } else {
            String::new()
        };
        let pass = if *name == "pre-push" {
            "printf '%s\\n' \"$input\" | \"$own\" \"$@\"".to_string()
        } else {
            "exec \"$own\" \"$@\"".to_string()
        };
        let script = format!(
            "#!/bin/sh\n# herdr-superagent: protected branches are not pushed without approval; the repository's own hook runs after.\n{guard}own={own}/{name}\nif [ -x \"$own\" ]; then\n  {pass}\nfi\n"
        );
        let path = dir.join(name);
        std::fs::write(&path, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(dir.to_path_buf())
}

/// Whether the checkout at `dir` has a protected branch checked out.
fn on_protected_branch(dir: Option<&Path>, default: &str) -> bool {
    let Some(dir) = dir else { return false };
    std::process::Command::new("git")
        .args(["-C", &dir.display().to_string(), "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .map(|o| protected(String::from_utf8_lossy(&o.stdout).trim(), default))
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
        // The repository is named with -C: the subcommand is still found, and writes elsewhere are held.
        assert!(risk("git", &a(&["-C", ".", "push", "origin", "main"]), None, Some("main")).is_some());
        assert!(
            risk(
                "git",
                &a(&["-c", "user.name=x", "push", "origin", "HEAD:refs/heads/master"]),
                None,
                Some("develop")
            )
            .is_some(),
            "main and master are protected whatever the default is"
        );
        assert!(risk("git", &a(&["push", "origin", "develop"]), None, Some("develop")).is_some());
        assert!(risk("git", &a(&["push", "--all", "origin"]), None, Some("main")).is_some());
        assert!(risk("git", &a(&["push", "origin", "+sa/1:main"]), None, Some("main")).is_some());
        assert!(risk(
            "git",
            &a(&["-C", "/somewhere/else", "commit", "-m", "x"]),
            Some(dir.path()),
            Some("main")
        )
        .is_some());
        assert!(
            risk("git", &a(&["-C", "/somewhere/else", "log"]), Some(dir.path()), Some("main")).is_none(),
            "reading elsewhere is fine"
        );
        assert!(risk("git", &a(&["update-ref", "refs/heads/main", "HEAD"]), None, Some("main")).is_some());
        assert!(risk("git", &a(&["branch", "-f", "main", "HEAD"]), None, Some("main")).is_some());
        assert!(risk("git", &a(&["switch", "-C", "main"]), None, Some("main")).is_some());
        assert!(risk("git", &a(&["switch", "-c", "feature/x"]), None, Some("main")).is_none());
        assert!(risk("gh", &a(&["api", "-X", "PUT", "repos/o/r/pulls/3/merge"]), None, None).is_some());
        assert!(
            risk("gh", &a(&["pr", "create", "--fill"]), None, None).is_none(),
            "opening a pull request is how work is delivered"
        );
        assert_eq!(
            protected_pushes(
                "refs/heads/sa/1 abc refs/heads/sa/1 000\nrefs/heads/sa/1 abc refs/heads/main def\nrefs/tags/v1 abc refs/tags/v1 000\n",
                "main"
            ),
            vec!["refs/heads/main"]
        );
        assert!(protected_pushes("refs/heads/sa/1 abc refs/heads/sa/1 000\n", "main").is_empty());
        let shims = write_shims(&dir.path().join("shims"), "/usr/local/bin/herdr-superagent").unwrap();
        assert!(shims.join("git").is_file());
        assert!(std::fs::read_to_string(shims.join("rm")).unwrap().contains("guard rm --"));
    }
}
