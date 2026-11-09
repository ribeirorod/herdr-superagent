//! Logging: stderr for the pane or the shell, plus one log file per process
//! kind under `<state>/logs/<profile>/`, so a debug session can be shared as
//! one bundle. `--debug` (or `HERDR_SUPERAGENT_DEBUG=1`) turns the log files up
//! to `debug`, which records every herdr request and response, every daemon
//! IPC call and every effect.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

pub fn debug_enabled(flag: bool) -> bool {
    flag || std::env::var("HERDR_SUPERAGENT_DEBUG")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// The log file for a process kind (`daemon`, `cli`, `ensure`, …) of a profile.
pub fn log_file(paths: &crate::paths::Paths, process: &str) -> PathBuf {
    paths.logs_dir().join(format!("{process}.log"))
}

/// Installs the subscriber. Returns the log file path when one could be opened.
pub fn init(paths: &crate::paths::Paths, process: &str, debug_mode: bool) -> Option<PathBuf> {
    // The shell sees warnings only; the daemon pane sees info; the file sees everything the mode allows.
    // Debug mode raises the file only: on stderr it buries the shell (`HERDR_SUPERAGENT_LOG` still can).
    let level = if process == "daemon" { "info" } else { "warn" };
    let stderr_filter = EnvFilter::try_from_env("HERDR_SUPERAGENT_LOG").unwrap_or_else(|_| EnvFilter::new(level));
    let stderr_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter);

    let path = log_file(paths, process);
    let file = std::fs::create_dir_all(paths.logs_dir())
        .ok()
        .and_then(|_| OpenOptions::new().create(true).append(true).open(&path).ok());
    let file_layer = file.map(|file: File| {
        fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_thread_ids(false)
            .with_writer(Mutex::new(file))
            .with_filter(EnvFilter::new(if debug_mode { "debug" } else { "info" }))
    });
    tracing_subscriber::registry().with(stderr_layer).with(file_layer).init();
    tracing::info!(
        process,
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        debug_mode,
        argv = %std::env::args().collect::<Vec<_>>().join(" "),
        cwd = %std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        herdr_socket = %std::env::var("HERDR_SOCKET_PATH").unwrap_or_default(),
        herdr_pane = %std::env::var("HERDR_PANE_ID").unwrap_or_default(),
        path = %std::env::var("PATH").unwrap_or_default(),
        "process start"
    );
    Some(path)
}

/// Rotates a log file that grew past `max_bytes` by keeping one `.1` copy.
pub fn rotate(path: &PathBuf, max_bytes: u64) {
    if std::fs::metadata(path).map(|m| m.len() > max_bytes).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}
