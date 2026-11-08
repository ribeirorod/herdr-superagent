//! The daemon's own socket, for the CLI: one JSON line in, one JSON line out,
//! `{ "id", "method", "params" }` → `{ "id", "result" } | { "id", "error": { "code", "message" } }`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Debug)]
pub enum IpcError {
    NotRunning,
    Io(std::io::Error),
    Protocol(String),
    Daemon { code: String, message: String },
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcError::NotRunning => write!(
                f,
                "the superagent daemon is not running (run `herdr-superagent ensure` inside herdr)"
            ),
            IpcError::Io(e) => write!(f, "daemon socket: {e}"),
            IpcError::Protocol(e) => write!(f, "daemon protocol: {e}"),
            IpcError::Daemon { code, message } => write!(f, "{code}: {message}"),
        }
    }
}

impl std::error::Error for IpcError {}

/// Sends one request to the daemon behind `socket`.
pub fn call(socket: &Path, method: &str, params: Value, timeout: Duration) -> Result<Value, IpcError> {
    let started = std::time::Instant::now();
    let result = call_inner(socket, method, &params, timeout);
    let ms = started.elapsed().as_millis();
    match &result {
        Ok(value) => tracing::debug!(target: "ipc", method, ms, params = %params, result = %value, "daemon ok"),
        Err(error) => tracing::debug!(target: "ipc", method, ms, params = %params, %error, "daemon error"),
    }
    result
}

fn call_inner(socket: &Path, method: &str, params: &Value, timeout: Duration) -> Result<Value, IpcError> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let mut stream = match UnixStream::connect(socket) {
            Ok(stream) => stream,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                return Err(IpcError::NotRunning)
            }
            Err(e) => return Err(IpcError::Io(e)),
        };
        stream.set_read_timeout(Some(timeout)).map_err(IpcError::Io)?;
        stream.set_write_timeout(Some(timeout)).map_err(IpcError::Io)?;
        let request = json!({ "id": format!("c{}", std::process::id()), "method": method, "params": params });
        let mut line = request.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(IpcError::Io)?;
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).map_err(IpcError::Io)?;
        let value: Value = serde_json::from_str(&response).map_err(|e| IpcError::Protocol(format!("{e}: {response:?}")))?;
        if let Some(error) = value.get("error") {
            return Err(IpcError::Daemon {
                code: error.get("code").and_then(Value::as_str).unwrap_or("error").to_string(),
                message: error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
            });
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, method, params, timeout);
        Err(IpcError::Protocol("named pipes are not supported yet".into()))
    }
}

/// `ping` the daemon: its version, pid and pane, when it answers.
pub fn ping(socket: &Path) -> Result<Value, IpcError> {
    call(socket, "ping", json!({}), Duration::from_secs(3))
}
