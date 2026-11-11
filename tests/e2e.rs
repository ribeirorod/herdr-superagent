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
            .env("HERDR_SUPERAGENT_STATE_DIR", self.dir.path().join("state"))
            .env("HERDR_SUPERAGENT_CONFIG_DIR", self.dir.path().join("config"))
            .env("HOME", self.dir.path().join("home"))
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
            serde_json::from_str(&std::fs::read_to_string(self.dir.path().join("state/profiles/e2e/daemon.json")).unwrap()).unwrap();
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
