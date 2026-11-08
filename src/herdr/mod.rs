//! A small client for herdr's socket API: newline-delimited JSON over the
//! Unix socket in `HERDR_SOCKET_PATH`, one request per connection. Request and
//! response shapes follow `herdr api schema` for herdr 0.9.x.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

pub const SOURCE: &str = "superagent";
pub const AGENT: &str = "superagent";

#[derive(Debug)]
pub enum HerdrError {
    NotInsideHerdr,
    Io(std::io::Error),
    Protocol(String),
    /// herdr answered with an error: `code` and `message`.
    Api {
        code: String,
        message: String,
    },
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HerdrError::NotInsideHerdr => write!(f, "not inside herdr: HERDR_SOCKET_PATH is not set"),
            HerdrError::Io(e) => write!(f, "herdr socket: {e}"),
            HerdrError::Protocol(e) => write!(f, "herdr protocol: {e}"),
            HerdrError::Api { code, message } => write!(f, "herdr: {code}: {message}"),
        }
    }
}

impl std::error::Error for HerdrError {}

impl From<std::io::Error> for HerdrError {
    fn from(e: std::io::Error) -> Self {
        HerdrError::Io(e)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub active_tab_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

pub struct Herdr {
    socket: PathBuf,
    timeout: Duration,
}

impl Herdr {
    pub fn from_env() -> Result<Herdr, HerdrError> {
        let socket = std::env::var_os("HERDR_SOCKET_PATH")
            .filter(|v| !v.is_empty())
            .ok_or(HerdrError::NotInsideHerdr)?;
        Ok(Herdr {
            socket: PathBuf::from(socket),
            timeout: Duration::from_secs(10),
        })
    }

    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket
    }

    /// Sends one request and returns its `result`, or the API error.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let started = std::time::Instant::now();
        let result = self.call_inner(method, &params);
        let ms = started.elapsed().as_millis();
        match &result {
            Ok(value) => {
                tracing::debug!(target: "herdr", method, ms, params = %params, result = %truncate(&value.to_string(), 2000), "herdr ok")
            }
            Err(error) => tracing::debug!(target: "herdr", method, ms, params = %params, %error, "herdr error"),
        }
        result
    }

    fn call_inner(&self, method: &str, params: &Value) -> Result<Value, HerdrError> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;
            let mut stream = UnixStream::connect(&self.socket)?;
            stream.set_read_timeout(Some(self.timeout))?;
            stream.set_write_timeout(Some(self.timeout))?;
            let id = format!("sa_{}", std::process::id());
            let request = json!({ "id": id, "method": method, "params": params });
            let mut line = serde_json::to_string(&request).map_err(|e| HerdrError::Protocol(e.to_string()))?;
            line.push('\n');
            stream.write_all(line.as_bytes())?;
            let mut reader = BufReader::new(stream);
            let mut response = String::new();
            reader.read_line(&mut response)?;
            if response.trim().is_empty() {
                return Err(HerdrError::Protocol(format!("empty response to {method}")));
            }
            let value: Value = serde_json::from_str(&response).map_err(|e| HerdrError::Protocol(format!("{e}: {response}")))?;
            if let Some(error) = value.get("error") {
                return Err(HerdrError::Api {
                    code: error.get("code").and_then(Value::as_str).unwrap_or("error").to_string(),
                    message: error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                });
            }
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        }
        #[cfg(not(unix))]
        {
            let _ = (method, params);
            Err(HerdrError::Protocol("named pipes are not supported yet; use HERDR_BIN_PATH".into()))
        }
    }

    pub fn ping(&self) -> Result<String, HerdrError> {
        let result = self.call("ping", json!({}))?;
        Ok(result.get("version").and_then(Value::as_str).unwrap_or("").to_string())
    }

    pub fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, HerdrError> {
        let result = self.call("workspace.list", json!({}))?;
        let list = result.get("workspaces").cloned().unwrap_or(Value::Array(vec![]));
        serde_json::from_value(list).map_err(|e| HerdrError::Protocol(format!("workspace.list: {e}")))
    }

    /// Creates a workspace; returns its id and its root pane id.
    pub fn workspace_create(&self, cwd: &str, label: &str, env: &[(&str, &str)], focus: bool) -> Result<(String, String), HerdrError> {
        let env: serde_json::Map<String, Value> = env.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect();
        let result = self.call(
            "workspace.create",
            json!({ "cwd": cwd, "label": label, "focus": focus, "env": env }),
        )?;
        let workspace = result
            .pointer("/workspace/workspace_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let pane = result
            .pointer("/root_pane/pane_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if workspace.is_empty() {
            return Err(HerdrError::Protocol(format!("workspace.create returned no workspace id: {result}")));
        }
        Ok((workspace, pane))
    }

    pub fn workspace_focus(&self, workspace_id: &str) -> Result<(), HerdrError> {
        self.call("workspace.focus", json!({ "workspace_id": workspace_id })).map(|_| ())
    }

    pub fn pane_list(&self) -> Result<Vec<PaneInfo>, HerdrError> {
        let result = self.call("pane.list", json!({}))?;
        let list = result.get("panes").cloned().unwrap_or(Value::Array(vec![]));
        serde_json::from_value(list).map_err(|e| HerdrError::Protocol(format!("pane.list: {e}")))
    }

    /// Applies a declarative tab layout in a workspace; returns the layout description.
    pub fn layout_apply(&self, workspace_id: &str, tab_label: &str, root: Value, focus: bool) -> Result<Value, HerdrError> {
        self.call(
            "layout.apply",
            json!({ "workspace_id": workspace_id, "tab_label": tab_label, "focus": focus, "root": root }),
        )
    }

    /// Types a command into a pane's shell and presses Enter.
    pub fn pane_run(&self, pane_id: &str, command: &str) -> Result<(), HerdrError> {
        self.call("pane.send_input", json!({ "pane_id": pane_id, "text": command, "keys": ["Enter"] }))
            .map(|_| ())
    }

    /// Reports the superagent's own state for a pane, with the command that resumes it after a herdr restart.
    pub fn report_agent(
        &self,
        pane_id: &str,
        state: &str,
        message: &str,
        seq: u64,
        resume_argv: Option<&[String]>,
    ) -> Result<(), HerdrError> {
        let mut params = json!({ "pane_id": pane_id, "source": SOURCE, "agent": AGENT, "state": state, "message": message, "seq": seq });
        if let Some(argv) = resume_argv {
            params["resume_argv"] = json!(argv);
        }
        self.call("pane.report_agent", params).map(|_| ())
    }

    pub fn release_agent(&self, pane_id: &str, seq: u64) -> Result<(), HerdrError> {
        self.call(
            "pane.release_agent",
            json!({ "pane_id": pane_id, "source": SOURCE, "agent": AGENT, "seq": seq }),
        )
        .map(|_| ())
    }

    /// Display-only pane metadata: a title and sidebar tokens such as `sa_role`.
    pub fn report_pane_metadata(&self, pane_id: &str, title: Option<&str>, tokens: &[(&str, &str)]) -> Result<(), HerdrError> {
        let tokens: serde_json::Map<String, Value> = tokens.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect();
        let mut params = json!({ "pane_id": pane_id, "source": SOURCE, "agent": AGENT, "tokens": tokens });
        if let Some(title) = title {
            params["title"] = Value::String(title.to_string());
        }
        self.call("pane.report_metadata", params).map(|_| ())
    }

    pub fn report_workspace_metadata(&self, workspace_id: &str, tokens: &[(&str, &str)], seq: u64) -> Result<(), HerdrError> {
        let tokens: serde_json::Map<String, Value> = tokens.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect();
        self.call(
            "workspace.report_metadata",
            json!({ "workspace_id": workspace_id, "source": SOURCE, "tokens": tokens, "seq": seq }),
        )
        .map(|_| ())
    }

    pub fn notify(&self, title: &str, body: Option<&str>) -> Result<(), HerdrError> {
        let mut params = json!({ "title": title });
        if let Some(body) = body {
            params["body"] = Value::String(body.to_string());
        }
        self.call("notification.show", params).map(|_| ())
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}… ({} bytes)", &text[..i], text.len()),
        None => text.to_string(),
    }
}

/// A monotonic `seq` for agent reports: milliseconds since the epoch.
pub fn seq_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn sends_one_request_per_connection_and_parses_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let id = request["id"].clone();
                let response = if request["method"] == "ping" {
                    json!({ "id": id, "result": { "type": "pong", "version": "0.9.5", "protocol": 7 } })
                } else {
                    json!({ "id": id, "error": { "code": "unknown_method", "message": "nope" } })
                };
                let mut stream = stream;
                stream.write_all(format!("{response}\n").as_bytes()).unwrap();
            }
        });
        let herdr = Herdr {
            socket: path,
            timeout: Duration::from_secs(2),
        };
        assert_eq!(herdr.ping().unwrap(), "0.9.5");
        match herdr.call("bogus", json!({})) {
            Err(HerdrError::Api { code, .. }) => assert_eq!(code, "unknown_method"),
            other => panic!("unexpected {other:?}"),
        }
        server.join().unwrap();
    }
}
