//! Sources (PLAN §11, phase 4): where work arrives from besides the person
//! typing. A source is polled on its own interval and every new item becomes a
//! request with a stable id, so the same issue is never submitted twice.
//!
//! Three kinds, by how the item is fetched:
//!
//! * `github`: the `gh` CLI lists issues or pull requests;
//! * `command`: any program that prints items as JSON (a Jira or Bitbucket
//!   CLI, a script), for hosts without a built-in kind;
//! * `session`: the superagent session is asked to look through its own MCP
//!   servers (Jira, mail, chat) and submit what is new. This is also the
//!   fallback when a source's tool is not installed.
//!
//! What a source brings in is outside text: it is never accepted without a
//! judgement. The session routes it like any other candidate and names the
//! source as the rule that admits it.

use std::process::Command;
use std::sync::mpsc::Sender;

use serde::Serialize;
use serde_json::{json, Value};

use crate::core::clock;
use crate::core::engine::ReceiveResult;
use crate::core::model::Request;
use crate::daemon::Daemon;
use crate::profile::Source;

/// One thing a source found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub key: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub author: String,
}

/// What a finished poll hands back to the daemon's own thread.
#[derive(Debug)]
pub struct Polled {
    pub source: String,
    pub result: Result<Vec<Item>, String>,
}

/// The first string under any of `names`, with numbers read as strings.
fn field(value: &Value, names: &[&str]) -> String {
    names
        .iter()
        .find_map(|name| match value.pointer(name).or_else(|| value.get(name)) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Items from a tool's output: a JSON array, an object holding one (`issues`,
/// `values`, `items`), or one JSON object per line. Field names follow GitHub,
/// Jira and Bitbucket, so their CLIs work without a mapping.
pub fn parse_items(output: &str) -> Vec<Item> {
    let values: Vec<Value> = match serde_json::from_str::<Value>(output.trim()) {
        Ok(Value::Array(items)) => items,
        Ok(Value::Object(map)) => ["issues", "values", "items", "results", "pullRequests"]
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_array).cloned())
            .unwrap_or_else(|| vec![Value::Object(map)]),
        _ => output
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .collect(),
    };
    values
        .iter()
        .filter_map(|v| {
            let item = Item {
                key: field(v, &["key", "number", "id", "/pullRequest/id"]),
                title: field(v, &["title", "summary", "/fields/summary", "subject"]),
                body: field(v, &["body", "description", "/fields/description", "text", "snippet"]),
                url: field(v, &["url", "html_url", "/links/html/href", "self", "link"]),
                author: field(
                    v,
                    &[
                        "/author/login",
                        "/user/login",
                        "/fields/reporter/displayName",
                        "/author/display_name",
                        "author",
                        "from",
                    ],
                ),
            };
            (!item.key.is_empty() && !item.title.is_empty()).then_some(item)
        })
        .collect()
}

/// The command a source runs, or `None` when the session has to look instead.
pub fn command_for(source: &Source) -> Option<Vec<String>> {
    match source.kind.as_str() {
        "github" => {
            let what = if source.what == "prs" { "pr" } else { "issue" };
            let mut argv = vec![
                "gh".to_string(),
                what.to_string(),
                "list".into(),
                "--json".into(),
                "number,title,body,url,author".into(),
                "--limit".into(),
                source.limit.max(1).to_string(),
            ];
            if !source.repo.is_empty() {
                argv.extend(["--repo".to_string(), source.repo.clone()]);
            }
            if !source.query.is_empty() {
                argv.extend(["--search".to_string(), source.query.clone()]);
            }
            Some(argv)
        }
        "command" if !source.command.is_empty() => Some(source.command.clone()),
        _ => None,
    }
}

/// Whether a source can be fetched by a program on this machine; otherwise it falls back to the session.
pub fn runnable(source: &Source) -> Result<Vec<String>, String> {
    let argv = command_for(source).ok_or_else(|| "the session looks through its own MCP servers".to_string())?;
    match crate::envpath::locate(&argv[0]) {
        Some(_) => Ok(argv),
        None => Err(format!("`{}` is not installed here: the session is asked to look instead", argv[0])),
    }
}

fn run(argv: &[String], cwd: Option<&str>) -> Result<Vec<Item>, String> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).env("PATH", crate::envpath::full_path());
    if let Some(dir) = cwd.filter(|d| std::path::Path::new(d).is_dir()) {
        command.current_dir(dir);
    }
    let output = command.output().map_err(|e| format!("{}: {e}", argv[0]))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{} exited {}: {}",
            argv[0],
            output.status.code().unwrap_or(-1),
            stderr.trim().lines().next().unwrap_or("")
        ));
    }
    Ok(parse_items(&String::from_utf8_lossy(&output.stdout)))
}

/// The request a source item becomes. Its text is data; routing judges it.
pub fn request_for(source: &Source, item: &Item, now: &str) -> Request {
    let mut text = item.title.clone();
    if !item.url.is_empty() {
        text.push_str(&format!("\n{}", item.url));
    }
    if !item.body.trim().is_empty() {
        let body: String = item.body.chars().take(4000).collect();
        text.push_str(&format!("\n\n{}", body.trim()));
    }
    Request {
        id: format!("{}:{}", source.id, item.key),
        source: source.id.clone(),
        actor: if item.author.is_empty() {
            source.kind.clone()
        } else {
            item.author.clone()
        },
        human: true,
        at: now.to_string(),
        text,
        workflow: None,
        mode: None,
        project: (!source.project.is_empty()).then(|| crate::profile::expand_home(&source.project)),
        meta: json!({
            "source": source.id, "kind": source.kind, "key": item.key, "url": item.url,
            "workflow_hint": source.workflow, "mode_hint": source.mode, "rule": format!("source:{}", source.id),
        }),
    }
}

/// What the session is asked when a source has no program to run.
pub fn session_prompt(source: &Source, bin: &str, profile: &str, why: &str) -> String {
    let what = if source.instructions.trim().is_empty() {
        match source.kind.as_str() {
            "github" => format!("open GitHub {} matching `{}` in `{}`", source.what, source.query, source.repo),
            _ => format!("new items of source `{}`", source.id),
        }
    } else {
        source.instructions.trim().to_string()
    };
    let mut submit = format!(
        "{bin} --profile {profile} submit \"<title>\\n<url>\\n\\n<what it asks>\" --source {} --id {}:<key>",
        source.id, source.id
    );
    if !source.project.is_empty() {
        submit.push_str(&format!(" --project {}", source.project));
    }
    format!(
        "[superagent daemon] Source `{}` is due ({why}). Look for: {what}. Use your MCP servers or connectors, read only. For each item you have not submitted before, run:\n{submit}\nThe `--id` makes a repeat harmless. Then route what arrived like any other candidate; accept with `--rule source:{}`.",
        source.id, source.id
    )
}

impl Daemon {
    /// Starts the polls that are due, each on its own thread: a slow `gh` must not stall the daemon.
    pub(crate) fn poll_sources(&self, tx: &Sender<Polled>, only: Option<&str>) -> Vec<String> {
        let profile = self.policy.snapshot();
        let now_ms = crate::herdr::seq_now() as i64;
        let mut started = Vec::new();
        for source in profile.sources.iter().filter(|s| s.enabled) {
            let forced = only == Some(source.id.as_str());
            if only.is_some() && !forced {
                continue;
            }
            let last: i64 = self
                .store
                .kv_get(&format!("source.{}.last", source.id))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if !forced && now_ms - last < (source.every_minutes.max(1) as i64) * 60_000 {
                continue;
            }
            self.store.kv_set(&format!("source.{}.last", source.id), &now_ms.to_string());
            started.push(source.id.clone());
            match runnable(source) {
                Ok(argv) => {
                    let (tx, id) = (tx.clone(), source.id.clone());
                    let cwd = (!source.project.is_empty()).then(|| crate::profile::expand_home(&source.project));
                    std::thread::spawn(move || {
                        let _ = tx.send(Polled {
                            source: id,
                            result: run(&argv, cwd.as_deref()),
                        });
                    });
                }
                Err(why) => {
                    self.store
                        .kv_set(&format!("source.{}.status", source.id), &format!("asked the session: {why}"));
                    self.tell_session(
                        &format!("source:{}", source.id),
                        &session_prompt(source, &crate::envpath::self_path(), &self.paths.profile, &why),
                    );
                }
            }
        }
        started
    }

    /// Turns a finished poll into requests; returns how many were new.
    pub(crate) fn sources_polled(&self, polled: Polled) -> usize {
        let profile = self.policy.snapshot();
        let Some(source) = profile.sources.iter().find(|s| s.id == polled.source) else {
            return 0;
        };
        let now = clock::now_iso();
        let items = match polled.result {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(source = %source.id, %error, "source poll failed");
                self.store
                    .kv_set(&format!("source.{}.status", source.id), &format!("failed {now}: {error}"));
                return 0;
            }
        };
        let mut new = 0;
        for item in &items {
            match self.engine.receive(request_for(source, item, &now)) {
                Ok(ReceiveResult::Stored) => new += 1,
                Ok(_) => {}
                Err(e) => tracing::warn!(source = %source.id, key = %item.key, %e, "could not receive a source item"),
            }
        }
        tracing::info!(source = %source.id, items = items.len(), new, "source polled");
        self.store.kv_set(
            &format!("source.{}.status", source.id),
            &format!("ok {now}: {} items, {new} new", items.len()),
        );
        new
    }

    /// The sources with how each is fetched and how its last poll went.
    pub(crate) fn sources_view(&self) -> Value {
        let profile = self.policy.snapshot();
        Value::Array(
            profile
                .sources
                .iter()
                .map(|s| {
                    let via = match runnable(s) {
                        Ok(argv) => argv.join(" "),
                        Err(why) => format!("session ({why})"),
                    };
                    json!({
                        "id": s.id, "kind": s.kind, "enabled": s.enabled, "every_minutes": s.every_minutes, "via": via,
                        "project": s.project, "workflow": s.workflow,
                        "status": self.store.kv_get(&format!("source.{}.status", s.id)).unwrap_or_else(|| "not polled yet".into()),
                    })
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(kind: &str) -> Source {
        Source {
            id: "gh-bugs".into(),
            kind: kind.into(),
            repo: "acme/api".into(),
            query: "label:bug assignee:@me".into(),
            project: "/src/api".into(),
            workflow: "bugfix".into(),
            ..Source::default()
        }
    }

    #[test]
    fn reads_items_in_the_shapes_github_jira_and_bitbucket_print() {
        let gh = r#"[{"number":12,"title":"Crash on login","body":"steps…","url":"https://github.com/acme/api/issues/12","author":{"login":"ana"}}]"#;
        let items = parse_items(gh);
        assert_eq!((items[0].key.as_str(), items[0].author.as_str()), ("12", "ana"));
        let jira = r#"{"issues":[{"key":"API-7","self":"https://jira/rest/7","fields":{"summary":"Export fails","description":"…","reporter":{"displayName":"Bo"}}}]}"#;
        let items = parse_items(jira);
        assert_eq!(
            (items[0].key.as_str(), items[0].title.as_str(), items[0].author.as_str()),
            ("API-7", "Export fails", "Bo")
        );
        let lines = "{\"id\": 3, \"title\": \"PR three\", \"links\": {\"html\": {\"href\": \"https://bb/3\"}}}\nnot json\n{\"id\": 4}";
        let items = parse_items(lines);
        assert_eq!(items.len(), 1, "an item without a title is skipped");
        assert_eq!(items[0].url, "https://bb/3");
    }

    #[test]
    fn a_source_item_is_an_outside_request_with_a_stable_id() {
        let source = sample("github");
        let argv = command_for(&source).unwrap();
        assert_eq!(&argv[..3], ["gh", "issue", "list"]);
        assert!(argv.contains(&"acme/api".to_string()) && argv.contains(&"label:bug assignee:@me".to_string()));
        let item = Item {
            key: "12".into(),
            title: "Crash on login".into(),
            body: "steps".into(),
            url: "https://x/12".into(),
            author: "ana".into(),
        };
        let request = request_for(&source, &item, "now");
        assert_eq!(request.id, "gh-bugs:12");
        assert_eq!(request.source, "gh-bugs");
        assert!(request.workflow.is_none(), "a source never skips routing");
        assert_eq!(request.project.as_deref(), Some("/src/api"));
        assert_eq!(request.meta["workflow_hint"], "bugfix");
        assert!(command_for(&sample("session")).is_none());
        let prompt = session_prompt(&sample("session"), "herdr-superagent", "work", "mcp");
        assert!(prompt.contains("--source gh-bugs --id gh-bugs:<key>") && prompt.contains("--rule source:gh-bugs"));
    }
}
