//! End to end against a fake herdr: the real daemon and the real CLI, a socket
//! that answers like herdr and records every call. It covers what a live
//! round would: launch in a worktree, the brief, checkpoints (revised, sent
//! back, approved), a spawned reviewer in its own tab with its own brief, the
//! child's report reaching the parent, the workflow's required checkpoints,
//! and what the daemon tells the superagent session.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-superagent");
/// Long on purpose: the daemon's socket then takes its short fallback path, as it does
/// under macOS's temp directory, and tests running side by side must not meet there.
const STATE: &str = "state-directory-with-a-deliberately-long-name-so-the-daemon-socket-falls-back-to-its-short-path";

#[derive(Default)]
struct Fake {
    calls: Vec<(String, Value)>,
    workspaces: Vec<Value>,
    panes: Vec<Value>,
    next: u32,
}

impl Fake {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }
    fn pane(&mut self, workspace: &str, tab: &str) -> String {
        let pane_id = self.id("p");
        self.panes.push(json!({ "pane_id": pane_id, "workspace_id": workspace, "tab_id": tab, "label": null, "agent": null, "agent_status": null, "tokens": {} }));
        pane_id
    }
    fn answer(&mut self, method: &str, params: &Value) -> Result<Value, (&'static str, String)> {
        self.calls.push((method.to_string(), params.clone()));
        match method {
            "ping" => Ok(json!({ "type": "pong", "version": "0.9.9-fake" })),
            "workspace.list" => Ok(json!({ "workspaces": self.workspaces })),
            "pane.list" => Ok(json!({ "panes": self.panes })),
            "agent.list" => Ok(
                json!({ "agents": self.panes.iter().filter(|p| !p["agent"].is_null()).map(|p| json!({ "pane_id": p["pane_id"], "agent_status": "working", "completion_seq": 1 })).collect::<Vec<_>>() }),
            ),
            "workspace.create" => {
                let workspace_id = self.id("w");
                let tab_id = self.id("t");
                self.workspaces
                    .push(json!({ "workspace_id": workspace_id, "label": params["label"], "tokens": {} }));
                let pane_id = self.pane(&workspace_id, &tab_id);
                Ok(
                    json!({ "workspace": { "workspace_id": workspace_id }, "tab": { "tab_id": tab_id }, "root_pane": { "pane_id": pane_id } }),
                )
            }
            "tab.create" => {
                let workspace = params["workspace_id"].as_str().unwrap_or("").to_string();
                if !self.workspaces.iter().any(|w| w["workspace_id"] == workspace.as_str()) {
                    return Err(("workspace_not_found", workspace));
                }
                let tab_id = self.id("t");
                let pane_id = self.pane(&workspace, &tab_id);
                Ok(json!({ "tab": { "tab_id": tab_id }, "root_pane": { "pane_id": pane_id } }))
            }
            "agent.start" => {
                let pane = params["pane_id"].as_str().unwrap_or("");
                match self.panes.iter_mut().find(|p| p["pane_id"] == pane) {
                    Some(p) => {
                        p["agent"] = params["kind"].clone();
                        p["agent_status"] = json!("idle");
                        Ok(json!({ "started": true }))
                    }
                    None => Err(("pane_not_found", pane.to_string())),
                }
            }
            "agent.prompt" => {
                let target = params["target"].as_str().unwrap_or("");
                if self.panes.iter().any(|p| p["pane_id"] == target && !p["agent"].is_null()) {
                    Ok(json!({ "delivered": true }))
                } else {
                    Err(("agent_not_ready", target.to_string()))
                }
            }
            // A herdr without this method: the daemon must carry on.
            "workspace.rename" => Err(("unknown_method", method.to_string())),
            _ => Ok(json!({})),
        }
    }
}

fn serve(listener: UnixListener, fake: Arc<Mutex<Fake>>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(&line).unwrap();
        let method = request["method"].as_str().unwrap_or("").to_string();
        let result = fake.lock().unwrap().answer(&method, &request["params"]);
        let response = match result {
            Ok(result) => json!({ "id": request["id"], "result": result }),
            Err((code, message)) => json!({ "id": request["id"], "error": { "code": code, "message": message } }),
        };
        let mut stream = stream;
        let _ = stream.write_all(format!("{response}\n").as_bytes());
    }
}

struct World {
    dir: tempfile::TempDir,
    herdr_socket: PathBuf,
    fake: Arc<Mutex<Fake>>,
    daemon: Child,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl World {
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("HERDR_SOCKET_PATH", &self.herdr_socket)
            .env("HERDR_SUPERAGENT_STATE_DIR", self.dir.path().join(STATE))
            .env("HERDR_SUPERAGENT_CONFIG_DIR", self.dir.path().join("config"))
            .env("HOME", self.dir.path().join("home"))
            .env("HERDR_SUPERAGENT_ROUTING_GRACE_MS", "1000")
            .env_remove("HERDR_PANE_ID")
            .env_remove("SUPERAGENT_TICKET")
            .env_remove("SUPERAGENT_ASSIGNMENT")
            .env_remove("SUPERAGENT_PROFILE")
            .args(["--profile", "e2e"]);
        command
    }

    /// Runs the CLI with `--json`; panics with its stderr when it fails.
    fn cli(&self, args: &[&str]) -> Value {
        let (ok, value, stderr) = self.try_cli(args);
        assert!(ok, "`{}` failed: {stderr}", args.join(" "));
        value
    }

    fn try_cli(&self, args: &[&str]) -> (bool, Value, String) {
        let output = self.command().arg("--json").args(args).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        (
            output.status.success(),
            serde_json::from_str(stdout.trim()).unwrap_or(Value::String(stdout)),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// One request on the daemon's own socket, for what the CLI does through `start`.
    fn ipc(&self, method: &str, params: Value) -> Value {
        let info: Value =
            serde_json::from_str(&std::fs::read_to_string(self.dir.path().join(STATE).join("profiles/e2e/daemon.json")).unwrap()).unwrap();
        let mut stream = UnixStream::connect(info["socket"].as_str().unwrap()).unwrap();
        stream
            .write_all(format!("{}\n", json!({ "id": 1, "method": method, "params": params })).as_bytes())
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// The prompts a pane received, in order.
    fn prompts(&self, pane: &str) -> Vec<String> {
        self.calls("agent.prompt")
            .iter()
            .filter(|p| p["target"] == pane)
            .map(|p| p["text"].as_str().unwrap_or("").to_string())
            .collect()
    }

    fn wait<T>(&self, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn world(profile_toml: &str) -> World {
    let dir = tempfile::tempdir().unwrap();
    let herdr_socket = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_socket).unwrap();
    let fake = Arc::new(Mutex::new(Fake::default()));
    let served = fake.clone();
    std::thread::spawn(move || serve(listener, served));
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let profile_dir = dir.path().join("config/profiles/e2e");
    std::fs::create_dir_all(&profile_dir).unwrap();
    std::fs::write(profile_dir.join("profile.toml"), profile_toml).unwrap();
    let mut world = World {
        dir,
        herdr_socket,
        fake,
        daemon: Command::new("true").spawn().unwrap(),
    };
    world.daemon = world
        .command()
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    world.wait("the daemon", || world.try_cli(&["status"]).0.then_some(()));
    world
}

fn repo(world: &World) -> PathBuf {
    let repo = world.dir.path().join("invoices");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "invoices\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

#[test]
fn a_feature_ticket_runs_from_request_to_review_with_a_spawned_reviewer() {
    let seed = tempfile::tempdir().unwrap();
    let _ = seed;
    let mut world = world("id = \"e2e\"\ncalibrated = true\n");
    let repo = repo(&world);
    // The profile learns the repository, so a request that names it gets it as its project.
    world.cli(&[
        "profile",
        "set",
        &format!("repos=[{{ path = \"{}\", default_branch = \"main\" }}]", repo.display()),
        "--yes",
    ]);
    // A superagent session exists: the daemon has someone to tell.
    let session_pane = world.fake.lock().unwrap().pane("w-cockpit", "t-cockpit");
    world.fake.lock().unwrap().panes.last_mut().unwrap()["agent"] = json!("claude");
    world.ipc("session_set", json!({ "pane_id": session_pane, "harness": "claude" }));

    // 1. A request without a workflow waits for routing; the session is told, and routes it.
    let submitted = world.cli(&["submit", "Add CSV export to invoices"]);
    let request_id = submitted["request_id"].as_str().unwrap().to_string();
    world.wait("the routing message", || {
        world
            .prompts(&session_pane)
            .iter()
            .any(|t| t.contains("wait for your routing"))
            .then_some(())
    });
    let decision = world.cli(&["route", &request_id, "accept", "--title", "Add CSV export", "--workflow", "feature"]);
    let ticket_id = decision["ticket_id"].as_str().unwrap().to_string();
    let ticket = world.cli(&["ticket", &ticket_id]);
    assert_eq!(
        ticket["project"],
        repo.display().to_string(),
        "the repository named in the request is the ticket's project"
    );
    assert_eq!(ticket["pack"], "software-dev@1/feature");
    assert_eq!(ticket["required_checkpoints"], json!(["plan", "implemented"]));

    // 2. The worker launches in its own workspace and worktree, and is pointed at its brief.
    let (worker_pane, root) = world.wait("the worker's brief", || {
        let ticket = world.cli(&["ticket", &ticket_id]);
        let root = ticket["assignments"][0].clone();
        let pane = root["pane_id"].as_str()?.to_string();
        world
            .prompts(&pane)
            .iter()
            .any(|t| t.contains("Read ") && t.contains("start on ticket"))
            .then_some((pane, root))
    });
    let root_id = root["id"].as_str().unwrap().to_string();
    let ticket = world.cli(&["ticket", &ticket_id]);
    let worktree = PathBuf::from(ticket["execution"]["directory"].as_str().unwrap());
    assert!(worktree.join(".git").is_file(), "a linked worktree, not the person's checkout");
    assert_ne!(worktree, repo);
    let start = &world.calls("agent.start")[0];
    let args: Vec<&str> = start["args"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(
        args.contains(&"--disallowedTools") && args.contains(&"Task,Agent"),
        "workers start without the harness's own subagents: {args:?}"
    );
    let brief_file = world.prompts(&worker_pane)[0].split_whitespace().nth(1).unwrap().to_string();
    let brief = std::fs::read_to_string(&brief_file).unwrap();
    assert!(brief.contains("## Workflow: feature") && brief.contains("spawn --role reviewer"));
    assert!(brief.contains("Look at all of it before you plan") && brief.contains(&format!("git -C {} status -sb", repo.display())));

    // 3. Ready is refused before a checkpoint; the plan checkpoint reaches the session.
    let worker = |args: &[&str]| {
        let mut all = args.to_vec();
        all.extend(["--ticket", &ticket_id]);
        world.try_cli(&all)
    };
    let (ok, _, stderr) = worker(&["report", "--state", "ready", "--summary", "done"]);
    assert!(!ok && stderr.to_lowercase().contains("checkpoint"), "{stderr}");
    assert!(worker(&["checkpoint", "plan", "--summary", "Plan: export all 96 invoices"]).0);
    world.wait("the checkpoint message", || {
        world
            .prompts(&session_pane)
            .iter()
            .any(|t| t.contains("checkpoint `plan`") && t.contains("96 invoices"))
            .then_some(())
    });
    // The scope narrows: the worker revises the same checkpoint, and the session hears the new summary.
    assert!(worker(&["checkpoint", "plan", "--summary", "Plan: export paid invoices only"]).0);
    world.wait("the revised checkpoint message", || {
        world
            .prompts(&session_pane)
            .iter()
            .any(|t| t.contains("paid invoices only"))
            .then_some(())
    });
    // Sent back once, then approved.
    world.cli(&["request-changes", &ticket_id, "include the VAT column"]);
    world.wait("the change request in the worker pane", || {
        world
            .prompts(&worker_pane)
            .iter()
            .any(|t| t.contains("NOT approved") && t.contains("VAT column"))
            .then_some(())
    });
    assert!(worker(&["checkpoint", "plan", "--summary", "Plan: paid invoices with VAT"]).0);
    world.cli(&["retitle", &ticket_id, "Add CSV export of paid invoices"]);
    world.cli(&["approve", &ticket_id, "--comment", "go"]);
    world.wait("the approval in the worker pane", || {
        world
            .prompts(&worker_pane)
            .iter()
            .any(|t| t.contains("'plan' approved"))
            .then_some(())
    });

    // 4. The worker spawns a reviewer: a new tab in the same workspace, with its own brief.
    let (ok, child, stderr) = worker(&[
        "spawn",
        "--role",
        "reviewer",
        "--task",
        "Review the diff of this branch",
        "--parent",
        &root_id,
    ]);
    assert!(ok, "{stderr}");
    let child_id = child["id"].as_str().unwrap().to_string();
    let child_pane = world.wait("the reviewer's brief", || {
        let ticket = world.cli(&["ticket", &ticket_id]);
        let pane = ticket["assignments"].as_array()?.iter().find(|a| a["id"] == child_id.as_str())?["pane_id"]
            .as_str()?
            .to_string();
        world
            .prompts(&pane)
            .iter()
            .any(|t| t.contains("do the task it gives you"))
            .then_some(pane)
    });
    assert_ne!(child_pane, worker_pane);
    let tab = &world.calls("tab.create")[0];
    assert_eq!(tab["label"], "reviewer");
    assert_eq!(tab["workspace_id"], ticket["execution"]["workspace_id"]);
    assert_eq!(
        tab["cwd"],
        worktree.display().to_string(),
        "the reviewer works in the same worktree"
    );
    let child_brief = std::fs::read_to_string(world.prompts(&child_pane)[0].split_whitespace().nth(1).unwrap()).unwrap();
    assert!(child_brief.contains("Review the diff of this branch") && child_brief.contains("## Your role: reviewer"));
    assert_eq!(
        world.prompts(&worker_pane).iter().filter(|t| t.contains("start on ticket")).count(),
        1,
        "the root is briefed once"
    );

    // 5. The reviewer's final report ends the child and reaches the parent's pane.
    assert!(
        worker(&[
            "report",
            "--state",
            "ready",
            "--summary",
            "one finding: missing test",
            "--assignment",
            &child_id
        ])
        .0
    );
    world.wait("the review in the worker pane", || {
        world
            .prompts(&worker_pane)
            .iter()
            .any(|t| t.contains("Your reviewer") && t.contains("missing test"))
            .then_some(())
    });
    assert!(
        world.cli(&["ticket", &ticket_id])["ended"].is_null(),
        "a child's report does not end the ticket"
    );

    // 6. The workflow's second checkpoint is still required; after it, ready ends the ticket.
    let (ok, _, stderr) = worker(&["report", "--state", "ready", "--summary", "done"]);
    assert!(!ok && stderr.contains("implemented"), "{stderr}");
    assert!(worker(&["checkpoint", "implemented", "--summary", "tests pass"]).0);
    world.cli(&["approve", &ticket_id]);
    assert!(worker(&["report", "--state", "ready", "--summary", "branch sa pushed"]).0);
    let ended = world.cli(&["ticket", &ticket_id]);
    assert_eq!(ended["stage"], "ended");
    assert_eq!(ended["title"], "Add CSV export of paid invoices");
    world.wait("the result message", || {
        world
            .prompts(&session_pane)
            .iter()
            .any(|t| t.contains("ended: completed"))
            .then_some(())
    });
    world.daemon.kill().unwrap();
}

#[test]
fn without_a_session_the_scorer_routes_and_routing_never_skips_the_session() {
    let world = world("id = \"e2e\"\ncalibrated = true\n\n[routing]\nrouting = \"never\"\n");
    let submitted = world.cli(&["submit", "Research which CSV libraries handle 1 GB files"]);
    let ticket_id = submitted["ticket_id"]
        .as_str()
        .expect("routing = never accepts at once")
        .to_string();
    let ticket = world.cli(&["ticket", &ticket_id]);
    assert_eq!(ticket["workflow"], "research");
    assert_eq!(ticket["assignments"][0]["role"], "researcher");
    assert!(ticket["project"].is_null());
}

#[test]
fn a_source_item_becomes_a_candidate_that_only_its_rule_admits() {
    let items = r#"[{"number":12,"title":"Crash on login","body":"steps","url":"https://example.invalid/12","author":{"login":"ana"}}]"#;
    let fixture = tempfile::tempdir().unwrap();
    let file = fixture.path().join("items.json");
    std::fs::write(&file, items).unwrap();
    let profile = format!(
        "id = \"e2e\"\ncalibrated = true\n\n[[sources]]\nid = \"issues\"\nkind = \"command\"\ncommand = [\"cat\", \"{}\"]\nworkflow = \"bugfix\"\n\n[[sources]]\nid = \"jira\"\nkind = \"session\"\ninstructions = \"Jira issues assigned to me\"\n",
        file.display()
    );
    let world = world(&profile);
    let session_pane = world.fake.lock().unwrap().pane("w-cockpit", "t-cockpit");
    world.fake.lock().unwrap().panes.last_mut().unwrap()["agent"] = json!("claude");
    world.ipc("session_set", json!({ "pane_id": session_pane, "harness": "claude" }));
    world.cli(&["sources", "poll"]);
    let candidate = world.wait("the source item as a candidate", || {
        let list = world.cli(&["candidates"]);
        list.as_array()?.iter().find(|c| c["request"]["id"] == "issues:12").cloned()
    });
    assert_eq!(candidate["strength"], "inferred");
    assert_eq!(candidate["request"]["actor"], "ana");
    // Polling again brings nothing new.
    world.cli(&["sources", "poll", "issues"]);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(world.cli(&["candidates"]).as_array().unwrap().len(), 1);
    // Outside text is not accepted on its own word.
    let (ok, _, stderr) = world.try_cli(&["route", "issues:12", "accept", "--title", "Crash on login"]);
    assert!(!ok, "accepted without a rule: {stderr}");
    let decision = world.cli(&[
        "route",
        "issues:12",
        "accept",
        "--title",
        "Crash on login",
        "--rule",
        "source:issues",
        "--workflow",
        "bugfix",
    ]);
    assert!(decision["ticket_id"].is_string());
    // A source without a program asks the session to look.
    world.wait("the session being asked to look at Jira", || {
        world
            .prompts(&session_pane)
            .iter()
            .any(|t| t.contains("Source `jira` is due") && t.contains("--source jira --id jira:<key>"))
            .then_some(())
    });
    let sources = world.cli(&["sources"]);
    assert!(sources[0]["status"].as_str().unwrap().starts_with("ok "), "{sources}");
}

#[test]
fn an_autonomous_worker_is_held_at_a_risky_command_until_a_person_approves_it() {
    let world = world("id = \"e2e\"\ncalibrated = true\n");
    let repo = repo(&world);
    let submitted = world.cli(&[
        "submit",
        "Tidy the README",
        "--workflow",
        "general",
        "--mode",
        "autonomous",
        "--project",
        repo.to_str().unwrap(),
    ]);
    let ticket_id = submitted["ticket_id"].as_str().unwrap().to_string();
    let ticket = world.wait("the worker", || {
        let ticket = world.cli(&["ticket", &ticket_id]);
        ticket["assignments"][0]["pane_id"].is_string().then_some(ticket)
    });
    assert_eq!(ticket["mode"], "autonomous");
    let worktree = ticket["execution"]["directory"].as_str().unwrap().to_string();
    let shims = world.dir.path().join(STATE).join("profiles/e2e/shims");
    // The worker's shell has the shims first on PATH, as the daemon exported them.
    let exported = world
        .calls("pane.send_input")
        .iter()
        .any(|p| p["text"].as_str().unwrap_or("").contains(&format!("PATH={}", shims.display())));
    assert!(exported, "the shims lead the worker's PATH");
    let run = |args: &[&str]| {
        let path = format!("{}:{}", shims.display(), std::env::var("PATH").unwrap());
        let mut command = world.command();
        // `world.command()` is the plugin binary; the shim is what a worker's shell would find as `git`.
        let _ = &mut command;
        Command::new(shims.join("git"))
            .args(args)
            .current_dir(&worktree)
            .env("PATH", path)
            .env("SUPERAGENT_TICKET", &ticket_id)
            .env("SUPERAGENT_WORKTREE", &worktree)
            .env("SUPERAGENT_PROFILE", "e2e")
            .env("HERDR_SUPERAGENT_STATE_DIR", world.dir.path().join(STATE))
            .env("HERDR_SUPERAGENT_CONFIG_DIR", world.dir.path().join("config"))
            .env("HERDR_SOCKET_PATH", &world.herdr_socket)
            .output()
            .unwrap()
    };
    // Harmless commands pass straight through to the real git.
    let status = run(&["status", "-sb"]);
    assert!(
        status.status.success() && String::from_utf8_lossy(&status.stdout).contains("sa/"),
        "{status:?}"
    );
    // A push to the default branch becomes a checkpoint instead of running.
    let held = run(&["push", "origin", "main"]);
    assert_eq!(held.status.code(), Some(75), "{}", String::from_utf8_lossy(&held.stderr));
    let waiting = world.cli(&["ticket", &ticket_id]);
    assert_eq!(waiting["checkpoint"]["name"], "guard-git");
    assert_eq!(waiting["stage"], "person/review");
    // Approved, exactly that command runs once (and fails only because the test repo has no remote).
    world.cli(&["approve", &ticket_id]);
    let allowed = run(&["push", "origin", "main"]);
    assert_ne!(allowed.status.code(), Some(75), "an approved command is not held again");
    assert!(String::from_utf8_lossy(&allowed.stderr).contains("was approved"));
    let again = run(&["push", "origin", "main"]);
    assert_eq!(again.status.code(), Some(75), "the approval was for one run");
}

#[test]
fn an_imported_session_reaches_the_brief_of_the_next_ticket_in_that_repository() {
    let world = world("id = \"e2e\"\ncalibrated = true\n\n[memory]\nsemantic = true\n");
    let repo = repo(&world);
    world.cli(&["profile", "set", &format!("repos=[{{ path = \"{}\" }}]", repo.display()), "--yes"]);
    // A session the person had by hand, before the superagent existed.
    let sessions = world.dir.path().join("home/.claude/projects/-invoices");
    std::fs::create_dir_all(&sessions).unwrap();
    let line = |value: Value| format!("{value}\n");
    let transcript = line(
        json!({ "type": "user", "sessionId": "old-1", "cwd": repo, "timestamp": "2026-10-01T09:00:00.000Z",
            "message": { "role": "user", "content": "Why do retried exports duplicate invoice rows?" } }),
    ) + &line(json!({ "type": "assistant", "message": { "role": "assistant", "content": [
            { "type": "text", "text": "The exporter retried without an idempotency key, so each retry appended the rows again. Fixed in export/writer.rs by keying on invoice id." },
            { "type": "tool_use", "name": "Edit", "input": { "file_path": repo.join("export/writer.rs") } } ] } }));
    std::fs::write(sessions.join("old-1.jsonl"), transcript).unwrap();
    // Elsewhere on the machine: not this profile's business.
    let other = world.dir.path().join("home/.claude/projects/-private");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("p.jsonl"),
        line(json!({ "type": "user", "cwd": "/somewhere/private", "message": { "role": "user", "content": "Plan the migration of my private photo archive to the new disk" } }))
            + &line(json!({ "type": "assistant", "message": { "role": "assistant", "content": "Copy by year with rsync, verify checksums, then switch the mount; nothing is deleted before that." } })),
    )
    .unwrap();

    let dry = world.cli(&["memory", "import", "--dry-run"]);
    assert_eq!(dry["imported"], 1);
    assert_eq!(world.cli(&["status"])["memory"], 0, "a dry run stores nothing");
    let imported = world.cli(&["memory", "import"]);
    assert_eq!(imported["imported"], 1, "{imported}");
    assert_eq!(imported["skipped"]["outside"], 1);
    let again = world.cli(&["memory", "import"]);
    assert_eq!(
        (again["imported"].as_u64(), again["skipped"]["unchanged"].as_u64()),
        (Some(0), Some(1))
    );

    // The next ticket in that repository: its brief carries what the old session learned,
    // found although the request shares no exact word form with it ("retry" vs "retried", "duplicated" vs "duplicate").
    let submitted = world.cli(&[
        "submit",
        "Duplicated rows after an export retry",
        "--workflow",
        "bugfix",
        "--project",
        repo.to_str().unwrap(),
    ]);
    let ticket_id = submitted["ticket_id"].as_str().unwrap().to_string();
    let brief = world.wait("the brief", || {
        let ticket = world.cli(&["ticket", &ticket_id]);
        let pane = ticket["assignments"][0]["pane_id"].as_str()?.to_string();
        let prompt = world.prompts(&pane).into_iter().find(|t| t.contains("start on ticket"))?;
        std::fs::read_to_string(prompt.split_whitespace().nth(1)?).ok()
    });
    assert!(
        brief.contains("claude session 2026-10-01: Why do retried exports duplicate invoice rows?"),
        "{brief}"
    );
    let found = world.cli(&["memory", "search", "idempotency key", "--project", repo.to_str().unwrap()]);
    assert_eq!(found["hits"].as_array().unwrap().len(), 1);
    assert_eq!(found["semantic"], "hash:512");
}

#[test]
fn imports_are_cleaned_of_hooks_probes_and_duplicates_and_curated_notes_take_their_place() {
    let world = world("id = \"e2e\"\ncalibrated = true\n");
    let repo = repo(&world);
    world.cli(&["profile", "set", &format!("repos=[{{ path = \"{}\" }}]", repo.display()), "--yes"]);
    let line = |value: Value| format!("{value}\n");
    let exchange = |cwd: &Path, day: &str, ask: &str, answer: &str| {
        line(
            json!({ "type": "user", "cwd": cwd, "timestamp": format!("2026-10-{day}T09:00:00.000Z"), "message": { "role": "user", "content": ask } }),
        ) + &line(json!({ "type": "assistant", "message": { "role": "assistant", "content": answer } }))
    };
    let claude = world.dir.path().join("home/.claude/projects/-invoices");
    let pi = world.dir.path().join("home/.pi/agent/sessions/invoices");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::create_dir_all(&pi).unwrap();
    let hook = "CAVEMAN MODE ACTIVE. Respond terse, drop articles.\n\n";
    let asks = [
        (
            "01",
            "Why does the nightly export time out on large tenants?",
            "It loads every invoice into memory; stream the rows in batches of 5000 instead.",
        ),
        (
            "02",
            "Should the VAT column be gross or net in the export?",
            "Net, with the rate in its own column, because the finance import recomputes gross.",
        ),
        (
            "03",
            "Rename the exporter module to writer and keep the old import path working",
            "Renamed, with a re-export from the old path so callers keep working until the next release.",
        ),
    ];
    for (day, ask, answer) in asks {
        std::fs::write(
            claude.join(format!("s-{day}.jsonl")),
            exchange(&repo, day, &format!("{hook}{ask}"), answer),
        )
        .unwrap();
    }
    // The first conversation was driven from Pi, which recorded it too.
    std::fs::write(
        pi.join("p-01.jsonl"),
        line(json!({ "type": "session", "id": "p-01", "timestamp": "2026-10-01T09:00:00.000Z", "cwd": repo }))
            + &line(json!({ "type": "message", "message": { "role": "user", "content": [{ "type": "text", "text": asks[0].1 }] } }))
            + &line(json!({ "type": "message", "message": { "role": "assistant", "content": [{ "type": "text", "text": asks[0].2 }] } })),
    )
    .unwrap();
    std::fs::write(claude.join("probe.jsonl"), exchange(&repo, "04", "hi", "Hello! How can I help?")).unwrap();
    std::fs::write(
        claude.join("agent.jsonl"),
        exchange(
            &repo,
            "05",
            "You are a graphify extraction subagent. Read the files and return the nodes as JSON.",
            "{\"nodes\": []} with a long explanation of every file that was read and what it holds",
        ),
    )
    .unwrap();

    let imported = world.cli(&["memory", "import"]);
    assert_eq!(imported["skipped"]["probe"], 2, "{imported}");
    assert_eq!(
        world.cli(&["status"])["memory"],
        3,
        "three conversations, the one seen twice stored once"
    );
    let found = world.cli(&["memory", "search", "nightly export time out", "--project", repo.to_str().unwrap()]);
    let hit = &found["hits"][0];
    assert!(
        hit["title"]
            .as_str()
            .unwrap()
            .ends_with("Why does the nightly export time out on large tenants?"),
        "{hit}"
    );
    let entry = world.cli(&["memory", "show", hit["id"].as_str().unwrap()]);
    assert!(
        !entry["body"].as_str().unwrap().contains("CAVEMAN"),
        "the hook's preamble is not what was asked"
    );
    assert!(entry["body"].as_str().unwrap().contains("- folder: invoices"));

    // The person's own notes for this repository: one fact per file, an index and a dropped folder.
    let notes = world.dir.path().join("session-memory/out/invoices/notes");
    std::fs::create_dir_all(notes.join("dropped")).unwrap();
    std::fs::write(
        notes.join("export-streams-in-batches.md"),
        "---\nname: export-streams-in-batches\ndescription: The export must stream rows in batches of 5000\ntype: decision\nsessions: [s-01]\n---\n\nLoading every invoice into memory timed out for large tenants.\n",
    )
    .unwrap();
    std::fs::write(notes.join("MEMORY.md"), "- [export](export-streams-in-batches.md)\n").unwrap();
    std::fs::write(notes.join("dropped").join("noise.md"), "---\nname: noise\n---\nnot a fact\n").unwrap();
    world.cli(&[
        "profile",
        "set",
        &format!(
            "memory.notes=[{{ path = \"{}\", project = \"{}\" }}]",
            notes.parent().unwrap().display(),
            repo.display()
        ),
    ]);
    let synced = world.cli(&["memory", "notes"]);
    assert_eq!(
        (synced["added"].as_u64(), synced["sessions_removed"].as_u64()),
        (Some(1), Some(3)),
        "{synced}"
    );
    assert_eq!(world.cli(&["status"])["memory"], 1, "the notes took the sessions' place");
    assert_eq!(world.cli(&["memory", "notes"])["unchanged"], 1);
    let again = world.cli(&["memory", "import"]);
    assert_eq!(again["imported"], 0, "{again}");
    // A session that changes later is still not imported beside the notes.
    std::fs::write(
        claude.join("s-06.jsonl"),
        exchange(
            &repo,
            "06",
            "Add a currency column to the export file format",
            "Added as ISO 4217 codes in a new last column, so older readers ignore it.",
        ),
    )
    .unwrap();
    let later = world.cli(&["memory", "import"]);
    assert_eq!(
        (later["imported"].as_u64(), later["skipped"]["covered"].as_u64()),
        (Some(0), Some(1)),
        "{later}"
    );

    // A ticket in that repository starts from the note.
    let submitted = world.cli(&[
        "submit",
        "The export of a large tenant times out",
        "--workflow",
        "bugfix",
        "--project",
        repo.to_str().unwrap(),
    ]);
    let ticket_id = submitted["ticket_id"].as_str().unwrap().to_string();
    let brief = world.wait("the brief", || {
        let ticket = world.cli(&["ticket", &ticket_id]);
        let pane = ticket["assignments"][0]["pane_id"].as_str()?.to_string();
        let prompt = world.prompts(&pane).into_iter().find(|t| t.contains("start on ticket"))?;
        std::fs::read_to_string(prompt.split_whitespace().nth(1)?).ok()
    });
    assert!(brief.contains("The export must stream rows in batches of 5000"), "{brief}");
    // A note that is moved to dropped/ is forgotten.
    std::fs::rename(
        notes.join("export-streams-in-batches.md"),
        notes.join("dropped").join("export-streams-in-batches.md"),
    )
    .unwrap();
    assert_eq!(world.cli(&["memory", "notes"])["removed"], 1);
    assert_eq!(world.cli(&["status"])["memory"], 0);
}
