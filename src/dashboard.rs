//! The cockpit's task pane (PLAN §8b). On a terminal it is the interactive
//! cockpit (`cockpit.rs`); this file keeps the plain text list for `--once`,
//! `--plain` and pipes, and the restart into a new build.

use std::io::Write;
use std::time::Duration;

use serde_json::{json, Value};

use crate::ipc;
use crate::paths::Paths;

const PURPLE: &str = "\x1b[1;38;2;168;85;247m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// The pane's width from the tty (works in splits), else `COLUMNS`, else 100.
fn columns() -> usize {
    #[cfg(unix)]
    {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ fills a winsize for a tty fd; a non-tty fails and we fall back.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col >= 20 {
            return size.ws_col as usize;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|&c: &usize| c >= 20)
        .unwrap_or(100)
}

/// Cuts a line to `width` visible characters, keeping escape sequences intact and closing colour at the end.
pub fn cut(line: &str, width: usize) -> String {
    let mut out = String::new();
    let mut visible = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for n in chars.by_ref() {
                out.push(n);
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible >= width {
            out.push_str(RESET);
            return out;
        }
        out.push(c);
        visible += 1;
    }
    out
}

/// Restarts into the binary on PATH: a rebuilt daemon means a rebuilt dashboard.
#[cfg(unix)]
fn restart(profile: &str) -> String {
    use std::os::unix::process::CommandExt;
    let program = if crate::daemon::on_path("herdr-superagent") {
        std::path::PathBuf::from("herdr-superagent")
    } else {
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("herdr-superagent"))
    };
    let error = std::process::Command::new(program).args(["dashboard", "--profile", profile]).exec();
    format!("dashboard restart failed: {error}")
}

fn is_tty() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty only inspects the descriptor.
        unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 && libc::isatty(libc::STDIN_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The cockpit's task pane. On a terminal it is interactive; `--plain`, `--once`
/// and a pipe get the text list.
pub fn run(profile: &str, once: bool, plain: bool) -> Result<(), String> {
    let paths = Paths::for_profile(profile);
    if !once && !plain && is_tty() {
        if let (Ok(herdr), Ok(pane)) = (crate::herdr::Herdr::from_env(), std::env::var("HERDR_PANE_ID")) {
            let _ = herdr.report_pane_metadata(
                &pane,
                Some(&format!("tasks · {profile}")),
                &[("sa_role", "dashboard"), ("sa_profile", profile)],
            );
        }
        return match crate::cockpit::run(profile)? {
            #[cfg(unix)]
            true => Err(restart(profile)),
            _ => Ok(()),
        };
    }
    #[cfg(unix)]
    if !once {
        // Ctrl-C would close the pane (no shell underneath); the pane's close action quits the dashboard.
        unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    }
    if !once {
        if let (Ok(herdr), Ok(pane)) = (crate::herdr::Herdr::from_env(), std::env::var("HERDR_PANE_ID")) {
            let _ = herdr.report_pane_metadata(
                &pane,
                Some(&format!("tasks · {profile}")),
                &[("sa_role", "dashboard"), ("sa_profile", profile)],
            );
        }
    }
    loop {
        // A rebuilt daemon means a rebuilt dashboard: exec the binary on PATH, like the daemon does.
        if !once {
            if let Ok(ping) = ipc::ping(&paths.socket_path()) {
                let build = ping["build"].as_str().unwrap_or("");
                if !build.is_empty() && build != crate::daemon::BUILD {
                    #[cfg(unix)]
                    return Err(restart(profile));
                }
            }
        }
        let width = columns();
        let frame = render(profile, &paths)
            .lines()
            .map(|l| cut(l, width))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let mut out = std::io::stdout().lock();
        if !once {
            let _ = write!(out, "\x1b[2J\x1b[H");
        }
        let _ = out.write_all(frame.as_bytes());
        let _ = out.flush();
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn render(profile: &str, paths: &Paths) -> String {
    let status = ipc::call(&paths.socket_path(), "status", json!({}), Duration::from_secs(3));
    let tickets = ipc::call(&paths.socket_path(), "tickets", json!({ "all": true }), Duration::from_secs(3));
    let mut lines = vec![format!("{PURPLE}◆ SUPERAGENT · {profile}{RESET}")];
    let (Ok(status), Ok(tickets)) = (status, tickets) else {
        lines.push(format!(
            "{DIM}daemon not running — herdr-superagent start --profile {profile}{RESET}"
        ));
        return lines.join("\n") + "\n";
    };
    lines.push(format!(
        "{DIM}workers {}/{} · candidates {} · memory {}{}{RESET}",
        status["workers"]["running"],
        status["workers"]["max"],
        status["candidates"],
        status["memory"],
        status["lease"]
            .as_str()
            .map(|l| format!(" · lease {}", l.split(" until ").next().unwrap_or(l)))
            .unwrap_or_default()
    ));
    lines.push(String::new());
    let all: Vec<Value> = tickets.as_array().cloned().unwrap_or_default();
    let today: String = crate::core::clock::now_iso().chars().take(10).collect();
    let section = |title: &str, colour: &str, items: Vec<&Value>, lines: &mut Vec<String>| {
        if items.is_empty() {
            return;
        }
        lines.push(format!("{BOLD}{colour}{title} ({}){RESET}", items.len()));
        for t in items {
            let report = t["assignments"]
                .as_array()
                .and_then(|a| a.iter().rev().find_map(|x| x.get("report").filter(|r| !r.is_null())))
                .map(|r| {
                    r["summary"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(40)
                        .collect::<String>()
                })
                .unwrap_or_default();
            let pane = if t["stage"] == "ended" {
                "-"
            } else {
                t["assignments"]
                    .as_array()
                    .and_then(|a| a.last())
                    .and_then(|a| a["pane_id"].as_str())
                    .unwrap_or("-")
            };
            let question = t
                .pointer("/attention/question")
                .and_then(Value::as_str)
                .map(|q| format!(" ? {}", q.chars().take(40).collect::<String>()))
                .unwrap_or_default();
            let checkpoint = t
                .pointer("/checkpoint/name")
                .and_then(Value::as_str)
                .map(|c| format!(" ⏸ {c}"))
                .unwrap_or_default();
            let agents = t["assignments"]
                .as_array()
                .map(|a| a.iter().filter(|x| x["state"] == "running").count())
                .unwrap_or(0);
            let crew = if agents > 1 {
                format!(" · {agents} agents")
            } else {
                String::new()
            };
            let branch = t
                .pointer("/execution/branch")
                .and_then(Value::as_str)
                .filter(|_| t["stage"] == "ended")
                .map(|b| format!(" ⎇ {b}"))
                .unwrap_or_default();
            let tail = if report.is_empty() {
                String::new()
            } else {
                format!("{DIM}  {report}{RESET}")
            };
            lines.push(format!(
                "  {} {:<9} {:<11} {:<6} {}{}{}{}{}{}",
                t["id"].as_str().unwrap_or(""),
                t["stage"].as_str().unwrap_or("").split('/').next_back().unwrap_or(""),
                t["mode"].as_str().unwrap_or(""),
                pane,
                t["title"].as_str().unwrap_or("").chars().take(34).collect::<String>(),
                checkpoint,
                question,
                crew,
                branch,
                tail
            ));
        }
        lines.push(String::new());
    };
    let stage = |t: &Value| t["stage"].as_str().unwrap_or("").to_string();
    section(
        "NEEDS YOU",
        YELLOW,
        all.iter().filter(|t| stage(t).starts_with("person/")).collect(),
        &mut lines,
    );
    section(
        "RUNNING",
        GREEN,
        all.iter().filter(|t| stage(t) == "working" || stage(t) == "idle").collect(),
        &mut lines,
    );
    section(
        "QUEUED",
        DIM,
        all.iter().filter(|t| stage(t) == "working/queued").collect(),
        &mut lines,
    );
    section(
        "DONE TODAY",
        DIM,
        all.iter()
            .filter(|t| stage(t) == "ended" && t["updated_at"].as_str().map(|u| u.starts_with(&today)).unwrap_or(false))
            .collect(),
        &mut lines,
    );
    if all.is_empty() {
        lines.push(format!("{DIM}no tickets yet — herdr-superagent submit \"…\"{RESET}"));
    }
    lines.push(format!(
        "{DIM}approve <id> · stop-ticket <id> · ticket <id>   (refreshes every 2 s){RESET}"
    ));
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_visible_width_and_keeps_escapes() {
        let line = format!("{PURPLE}abcdef{RESET}");
        assert_eq!(cut(&line, 3), format!("{PURPLE}abc{RESET}"));
        assert_eq!(cut("plain text", 50), "plain text");
    }
}
