//! The interactive cockpit (PLAN §8b, phase 6): the task list of the
//! cockpit's right pane, driven from the keyboard and the mouse. What needs
//! you comes first; a ticket is approved, sent back, noted, renamed, stopped,
//! retried or opened without leaving the pane.
//!
//! State and input handling are plain functions over JSON from the daemon, so
//! they are tested without a terminal; drawing goes through ratatui, which
//! only repaints cells that changed.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::{json, Value};

use crate::ipc;
use crate::paths::Paths;

const PURPLE: Color = Color::Rgb(168, 85, 247);
const REFRESH: Duration = Duration::from_secs(2);

/// One line of the list.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Section(String, usize),
    Ticket(usize),
    Candidate(usize),
}

/// What the footer is asking for.
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    /// A yes/no question before something that cannot be taken back.
    Confirm { question: String, then: Pending },
    /// A line of text.
    Text { label: String, buffer: String, then: Pending },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    Approve(String),
    Stop(String),
    Changes(String),
    Note(String),
    Retitle(String),
    Ignore(String),
}

/// A call for the daemon, or herdr, that an input produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Daemon(&'static str, Value),
    /// Focus the ticket's workspace (and pane) in herdr.
    Open {
        workspace: Option<String>,
        pane: Option<String>,
    },
}

#[derive(Default)]
pub struct App {
    pub profile: String,
    pub status: Value,
    pub tickets: Vec<Value>,
    pub candidates: Vec<Value>,
    pub rows: Vec<Row>,
    /// Index into `rows` of the selected ticket or candidate.
    pub selected: usize,
    pub scroll: usize,
    pub prompt: Option<Prompt>,
    pub flash: String,
    pub help: bool,
    pub online: bool,
    /// Where the list and the buttons were drawn, for the mouse.
    pub list_area: Rect,
    pub buttons: Vec<(Rect, char)>,
}

fn stage(ticket: &Value) -> &str {
    ticket["stage"].as_str().unwrap_or("")
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

impl App {
    pub fn new(profile: &str) -> App {
        App {
            profile: profile.to_string(),
            ..App::default()
        }
    }

    /// Replaces the data and keeps the selection on the same ticket.
    pub fn load(&mut self, status: Value, tickets: Vec<Value>, candidates: Vec<Value>) {
        let keep = self.selected_id();
        self.status = status;
        self.tickets = tickets;
        self.candidates = candidates;
        self.online = true;
        let today: String = crate::core::clock::now_iso().chars().take(10).collect();
        let mut rows = Vec::new();
        let section = |title: &str, picks: Vec<Row>, rows: &mut Vec<Row>| {
            if !picks.is_empty() {
                rows.push(Row::Section(title.to_string(), picks.len()));
                rows.extend(picks);
            }
        };
        let pick = |test: &dyn Fn(&Value) -> bool| -> Vec<Row> {
            self.tickets
                .iter()
                .enumerate()
                .filter(|(_, t)| test(t))
                .map(|(i, _)| Row::Ticket(i))
                .collect()
        };
        section("NEEDS YOU", pick(&|t| stage(t).starts_with("person/")), &mut rows);
        section("TO ROUTE", (0..self.candidates.len()).map(Row::Candidate).collect(), &mut rows);
        section("RUNNING", pick(&|t| matches!(stage(t), "working" | "idle")), &mut rows);
        section("QUEUED", pick(&|t| stage(t) == "working/queued"), &mut rows);
        section(
            "DONE TODAY",
            pick(&|t| stage(t) == "ended" && t["updated_at"].as_str().map(|u| u.starts_with(&today)).unwrap_or(false)),
            &mut rows,
        );
        self.rows = rows;
        let again = keep.and_then(|id| self.rows.iter().position(|row| self.row_id(row).as_deref() == Some(id.as_str())));
        self.selected = again.unwrap_or(self.selected);
        self.clamp();
    }

    fn row_id(&self, row: &Row) -> Option<String> {
        match row {
            Row::Ticket(i) => self.tickets.get(*i).and_then(|t| t["id"].as_str()).map(str::to_string),
            Row::Candidate(i) => self
                .candidates
                .get(*i)
                .and_then(|c| c["request"]["id"].as_str())
                .map(|id| format!("request:{id}")),
            Row::Section(..) => None,
        }
    }

    fn selected_id(&self) -> Option<String> {
        self.rows.get(self.selected).and_then(|row| self.row_id(row))
    }

    /// The selection always rests on a ticket or a candidate, never on a heading.
    fn clamp(&mut self) {
        let selectable: Vec<usize> = (0..self.rows.len())
            .filter(|i| !matches!(self.rows[*i], Row::Section(..)))
            .collect();
        if selectable.is_empty() {
            self.selected = 0;
            return;
        }
        if !selectable.contains(&self.selected) {
            self.selected = *selectable
                .iter()
                .find(|i| **i >= self.selected)
                .unwrap_or(selectable.last().unwrap_or(&0));
        }
    }

    fn step(&mut self, down: bool) {
        let selectable: Vec<usize> = (0..self.rows.len())
            .filter(|i| !matches!(self.rows[*i], Row::Section(..)))
            .collect();
        let Some(at) = selectable.iter().position(|i| *i == self.selected) else {
            self.clamp();
            return;
        };
        let next = if down {
            (at + 1).min(selectable.len() - 1)
        } else {
            at.saturating_sub(1)
        };
        self.selected = selectable[next];
    }

    pub fn ticket(&self) -> Option<&Value> {
        match self.rows.get(self.selected) {
            Some(Row::Ticket(i)) => self.tickets.get(*i),
            _ => None,
        }
    }

    pub fn candidate(&self) -> Option<&Value> {
        match self.rows.get(self.selected) {
            Some(Row::Candidate(i)) => self.candidates.get(*i),
            _ => None,
        }
    }

    fn ticket_id(&self) -> Option<String> {
        self.ticket().and_then(|t| t["id"].as_str()).map(str::to_string)
    }

    /// The keys that do something for the selection, as `(key, label)`.
    pub fn keys(&self) -> Vec<(char, &'static str)> {
        if let Some(ticket) = self.ticket() {
            let mut keys = Vec::new();
            let live = stage(ticket) != "ended";
            if !ticket["checkpoint"].is_null() {
                keys.push(('a', "approve"));
                keys.push(('c', "changes"));
            }
            keys.push(('o', "open"));
            if live {
                keys.push(('n', "note"));
            }
            keys.push(('t', "retitle"));
            if matches!(stage(ticket), "person/blocked" | "person/failed" | "person/delivery") {
                keys.push(('r', "retry"));
            }
            if live {
                keys.push(('s', "stop"));
            }
            keys
        } else if self.candidate().is_some() {
            vec![('a', "accept"), ('i', "ignore")]
        } else {
            Vec::new()
        }
    }

    fn act(&mut self, key: char) -> Option<Action> {
        if !self.keys().iter().any(|(k, _)| *k == key) {
            return None;
        }
        if let Some(candidate) = self.candidate().cloned() {
            let request = &candidate["request"];
            let id = request["id"].as_str().unwrap_or("").to_string();
            return match key {
                'a' => {
                    let title: String = first_line(request["text"].as_str().unwrap_or("")).chars().take(72).collect();
                    let rules: Vec<Value> = request["meta"]["rule"].as_str().map(|r| vec![json!(r)]).unwrap_or_default();
                    let hint = request["meta"]["workflow_hint"].as_str().filter(|h| !h.is_empty());
                    Some(Action::Daemon(
                        "decide",
                        json!({ "request_id": id, "outcome": { "kind": "accept", "title": title, "scope": "cockpit",
                            "reason": { "code": "explicitRouted" }, "rules": rules, "workflow": hint } }),
                    ))
                }
                _ => {
                    self.prompt = Some(Prompt::Text {
                        label: "why ignore it".into(),
                        buffer: String::new(),
                        then: Pending::Ignore(id),
                    });
                    None
                }
            };
        }
        let id = self.ticket_id()?;
        let ticket = self.ticket()?.clone();
        let text = |label: &str, then: Pending, buffer: &str| Prompt::Text {
            label: label.to_string(),
            buffer: buffer.to_string(),
            then,
        };
        match key {
            'a' => {
                self.prompt = Some(Prompt::Confirm {
                    question: format!(
                        "approve checkpoint '{}' of {id}?",
                        ticket["checkpoint"]["name"].as_str().unwrap_or("")
                    ),
                    then: Pending::Approve(id),
                })
            }
            's' => {
                self.prompt = Some(Prompt::Confirm {
                    question: format!("stop {id}? its worker receives nothing more"),
                    then: Pending::Stop(id),
                })
            }
            'c' => self.prompt = Some(text("what must change", Pending::Changes(id), "")),
            'n' => self.prompt = Some(text("note to the worker", Pending::Note(id), "")),
            't' => self.prompt = Some(text("new title", Pending::Retitle(id), ticket["title"].as_str().unwrap_or(""))),
            'r' => return Some(Action::Daemon("retry", json!({ "ticket": id }))),
            'o' => {
                let pane = ticket["assignments"]
                    .as_array()
                    .and_then(|a| a.iter().rev().find_map(|x| x["pane_id"].as_str()))
                    .map(str::to_string);
                return Some(Action::Open {
                    workspace: ticket
                        .pointer("/execution/workspace_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    pane,
                });
            }
            _ => {}
        }
        None
    }

    fn finish(pending: Pending, text: &str) -> Action {
        match pending {
            Pending::Approve(id) => Action::Daemon("approve", json!({ "ticket": id })),
            Pending::Stop(id) => Action::Daemon("stop_ticket", json!({ "ticket": id, "reason": "stopped from the cockpit" })),
            Pending::Changes(id) => Action::Daemon("request_changes", json!({ "ticket": id, "comment": text })),
            Pending::Note(id) => Action::Daemon("note", json!({ "ticket": id, "text": text })),
            Pending::Retitle(id) => Action::Daemon("retitle", json!({ "ticket": id, "title": text })),
            Pending::Ignore(id) => Action::Daemon(
                "decide",
                json!({ "request_id": id, "outcome": { "kind": "ignore", "reason": { "code": "said", "values": { "text": text } } } }),
            ),
        }
    }

    /// One key press: moves, opens a prompt, or returns what to do.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        self.flash.clear();
        if let Some(prompt) = self.prompt.take() {
            return match prompt {
                Prompt::Confirm { question, then } => match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(App::finish(then, "")),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => None,
                    _ => {
                        self.prompt = Some(Prompt::Confirm { question, then });
                        None
                    }
                },
                Prompt::Text { label, mut buffer, then } => match key.code {
                    KeyCode::Esc => None,
                    KeyCode::Enter if buffer.trim().is_empty() => None,
                    KeyCode::Enter => Some(App::finish(then, buffer.trim())),
                    code => {
                        match code {
                            KeyCode::Backspace => {
                                buffer.pop();
                            }
                            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => buffer.clear(),
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => buffer.push(c),
                            _ => {}
                        }
                        self.prompt = Some(Prompt::Text { label, buffer, then });
                        None
                    }
                },
            };
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                self.clamp();
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.rows.len().saturating_sub(1);
                self.clamp();
            }
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Esc => self.help = false,
            KeyCode::Enter => return self.act(if self.candidate().is_some() { 'a' } else { 'o' }),
            KeyCode::Char('q') => self.flash = "the cockpit stays open; close the pane to quit".into(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => return self.act(c),
            _ => {}
        }
        None
    }

    /// A click selects a row or presses a footer button; the wheel moves the selection.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.step(true),
            MouseEventKind::ScrollUp => self.step(false),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.prompt.is_some() {
                    return None;
                }
                let hit = |r: &Rect| mouse.column >= r.x && mouse.column < r.x + r.width && mouse.row >= r.y && mouse.row < r.y + r.height;
                if let Some((_, key)) = self.buttons.iter().find(|(rect, _)| hit(rect)).cloned() {
                    return self.act(key);
                }
                if hit(&self.list_area) {
                    let index = self.scroll + (mouse.row - self.list_area.y) as usize;
                    if matches!(self.rows.get(index), Some(Row::Ticket(_) | Row::Candidate(_))) {
                        if self.selected == index {
                            // A second click on the selection opens it.
                            return self.act(if self.candidate().is_some() { 'a' } else { 'o' });
                        }
                        self.selected = index;
                    }
                }
            }
            _ => {}
        }
        None
    }
}

fn stage_style(stage: &str) -> Style {
    match stage {
        s if s.starts_with("person/") => Style::default().fg(Color::Yellow),
        "working" => Style::default().fg(Color::Green),
        "ended" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

fn ticket_line(ticket: &Value, width: usize) -> Line<'static> {
    let stage_text = stage(ticket);
    let short = stage_text.split('/').next_back().unwrap_or("").to_string();
    let mark = match stage_text {
        s if s.starts_with("person/") => "●",
        "working" => "◐",
        "ended" => "✓",
        _ => "○",
    };
    let mut extra = String::new();
    if let Some(name) = ticket.pointer("/checkpoint/name").and_then(Value::as_str) {
        extra.push_str(&format!(" ⏸ {name}"));
    }
    if ticket.pointer("/attention/question").is_some_and(|q| !q.is_null()) {
        extra.push_str(" ?");
    }
    let agents = ticket["assignments"]
        .as_array()
        .map(|a| a.iter().filter(|x| x["state"] == "running").count())
        .unwrap_or(0);
    if agents > 1 {
        extra.push_str(&format!(" · {agents} agents"));
    }
    let id = ticket["id"].as_str().unwrap_or("").trim_start_matches("t-").to_string();
    let fixed = 2 + 7 + 1 + 9 + 1 + 5 + 1 + extra.chars().count();
    let title = clip(ticket["title"].as_str().unwrap_or(""), width.saturating_sub(fixed).max(8));
    Line::from(vec![
        Span::styled(format!("{mark} "), stage_style(stage_text)),
        Span::styled(format!("{id:<7} "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{short:<9} "), stage_style(stage_text)),
        Span::styled(
            format!(
                "{:<5} ",
                match ticket["mode"].as_str().unwrap_or("") {
                    "propose" => "plan",
                    "checkpoint" => "check",
                    "autonomous" => "auto",
                    other => other,
                }
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(title),
        Span::styled(extra, Style::default().fg(Color::Yellow)),
    ])
}

fn candidate_line(candidate: &Value, width: usize) -> Line<'static> {
    let request = &candidate["request"];
    let from = request["source"].as_str().unwrap_or("you");
    let text = clip(
        first_line(request["text"].as_str().unwrap_or("")),
        width.saturating_sub(6 + from.len()).max(8),
    );
    Line::from(vec![
        Span::styled("◇ ", Style::default().fg(Color::Cyan)),
        Span::styled(format!("{from}  "), Style::default().fg(Color::DarkGray)),
        Span::raw(text),
    ])
}

/// What the lower panel says about the selection: the thing to decide first.
pub fn detail(app: &App) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let label = |name: &str, value: String| Line::from(vec![Span::styled(format!("{name:<11}"), dim), Span::raw(value)]);
    if let Some(candidate) = app.candidate() {
        let request = &candidate["request"];
        let mut lines = vec![label("request", request["id"].as_str().unwrap_or("").to_string())];
        lines.push(label(
            "from",
            format!(
                "{} · {}{}",
                request["source"].as_str().unwrap_or(""),
                request["actor"].as_str().unwrap_or(""),
                request["project"].as_str().map(|p| format!(" · {p}")).unwrap_or_default()
            ),
        ));
        lines.push(Line::default());
        lines.extend(
            request["text"]
                .as_str()
                .unwrap_or("")
                .lines()
                .take(12)
                .map(|l| Line::raw(l.to_string())),
        );
        return lines;
    }
    let Some(ticket) = app.ticket() else {
        return vec![Line::styled("Nothing here yet. Type a request to the superagent on the left.", dim)];
    };
    let mut lines = Vec::new();
    if let Some(checkpoint) = ticket.get("checkpoint").filter(|c| !c.is_null()) {
        lines.push(Line::styled(
            format!("Waiting at checkpoint '{}'", checkpoint["name"].as_str().unwrap_or("")),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        lines.extend(
            checkpoint["summary"]
                .as_str()
                .unwrap_or("")
                .lines()
                .map(|l| Line::raw(l.to_string())),
        );
        lines.push(Line::default());
    }
    if let Some(question) = ticket.pointer("/attention/question").and_then(Value::as_str) {
        lines.push(Line::styled(
            "Question for you",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        lines.extend(question.lines().map(|l| Line::raw(l.to_string())));
        lines.push(Line::default());
    }
    if let Some(error) = ticket.pointer("/delivery/error").and_then(Value::as_str) {
        lines.push(label("delivery", error.to_string()));
    }
    let report = ticket["assignments"]
        .as_array()
        .and_then(|a| a.iter().rev().find_map(|x| x.get("report").filter(|r| !r.is_null())));
    if let Some(report) = report {
        lines.push(label(
            "report",
            format!(
                "{}: {}",
                report["state"].as_str().unwrap_or(""),
                report["summary"].as_str().unwrap_or("")
            ),
        ));
    }
    lines.push(label(
        "ticket",
        format!(
            "{} · {} · {}",
            ticket["id"].as_str().unwrap_or(""),
            stage(ticket),
            ticket["mode"].as_str().unwrap_or("")
        ),
    ));
    if let Some(pack) = ticket["pack"].as_str() {
        let approved: Vec<&str> = ticket["approved_checkpoints"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let marks: Vec<String> = ticket["required_checkpoints"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|name| format!("{}{name}", if approved.contains(&name) { "✓ " } else { "· " }))
                    .collect()
            })
            .unwrap_or_default();
        lines.push(label("workflow", format!("{pack}   {}", marks.join("  "))));
    }
    if let Some(project) = ticket["project"].as_str() {
        lines.push(label("project", project.to_string()));
    }
    if let Some(branch) = ticket.pointer("/execution/branch").and_then(Value::as_str) {
        let base = ticket.pointer("/execution/base").and_then(Value::as_str).unwrap_or("");
        lines.push(label(
            "branch",
            format!("{branch}{}", if base.is_empty() { String::new() } else { format!("  from {base}") }),
        ));
    }
    if let Some(assignments) = ticket["assignments"].as_array() {
        let crew: Vec<String> = assignments
            .iter()
            .map(|a| {
                format!(
                    "{} {}{}",
                    a["role"].as_str().unwrap_or(""),
                    a["state"].as_str().unwrap_or(""),
                    a["pane_id"].as_str().map(|p| format!(" ({p})")).unwrap_or_default()
                )
            })
            .collect();
        lines.push(label("agents", crew.join(" · ")));
    }
    lines
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let detail_lines = detail(app);
    let detail_height = (detail_lines.len() as u16 + 2).clamp(4, (area.height / 2).max(4));
    let [header, list, panel, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(detail_height),
        Constraint::Length(2),
    ])
    .areas(area);

    // Header: who, and the numbers that matter.
    let dim = Style::default().fg(Color::DarkGray);
    let needs = app.status["needs_you"].as_u64().unwrap_or(0);
    let mut stats = vec![Span::styled(
        format!("◆ SUPERAGENT · {}", app.profile),
        Style::default().fg(PURPLE).add_modifier(Modifier::BOLD),
    )];
    if !app.online {
        stats.push(Span::styled(
            format!("   daemon not running: herdr-superagent start --profile {}", app.profile),
            Style::default().fg(Color::Red),
        ));
    }
    let second = if app.online {
        Line::from(vec![
            Span::styled(
                format!("needs you {needs}"),
                if needs > 0 {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    dim
                },
            ),
            Span::styled(
                format!(
                    " · workers {}/{} · to route {} · memory {}{}{}",
                    app.status["workers"]["running"],
                    app.status["workers"]["max"],
                    app.status["candidates"],
                    app.status["memory"],
                    if app.status["paused"].as_bool().unwrap_or(false) {
                        " · PAUSED"
                    } else {
                        ""
                    },
                    app.status["lease"]
                        .as_str()
                        .map(|l| format!(" · lease {}", l.split(" until ").next().unwrap_or(l)))
                        .unwrap_or_default()
                ),
                dim,
            ),
        ])
    } else {
        Line::default()
    };
    frame.render_widget(Paragraph::new(vec![Line::from(stats), second]), header);

    // The list, scrolled so the selection stays visible.
    let height = list.height as usize;
    if app.selected < app.scroll {
        app.scroll = app.selected.saturating_sub(1);
    } else if height > 0 && app.selected >= app.scroll + height {
        app.scroll = app.selected + 1 - height;
    }
    app.list_area = list;
    let width = list.width as usize;
    let lines: Vec<Line> = app
        .rows
        .iter()
        .enumerate()
        .skip(app.scroll)
        .take(height)
        .map(|(index, row)| {
            let line = match row {
                Row::Section(title, count) => Line::styled(
                    format!("{title} ({count})"),
                    match title.as_str() {
                        "NEEDS YOU" => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        "TO ROUTE" => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        "RUNNING" => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                        _ => dim.add_modifier(Modifier::BOLD),
                    },
                ),
                Row::Ticket(i) => ticket_line(&app.tickets[*i], width),
                Row::Candidate(i) => candidate_line(&app.candidates[*i], width),
            };
            if index == app.selected && !matches!(row, Row::Section(..)) {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect();
    if app.rows.is_empty() && app.online {
        frame.render_widget(
            Paragraph::new(Line::styled("no tickets yet: type a request to the superagent on the left", dim)),
            list,
        );
    } else {
        frame.render_widget(Paragraph::new(lines), list);
    }

    // The selection in full, or the help.
    let (title, body) = if app.help {
        (
            " keys ",
            vec![
                Line::raw("↑↓ j k  move      enter / o  open the ticket's pane      ?  this help"),
                Line::raw("a  approve the checkpoint      c  send it back with what must change"),
                Line::raw("n  note to the worker          t  retitle      r  retry      s  stop"),
                Line::raw("on a request to route:  a  accept (the workflow is chosen for you)   i  ignore"),
                Line::raw("mouse: click selects, click again opens, wheel moves, footer buttons act"),
            ],
        )
    } else {
        (" selected ", detail_lines)
    };
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::TOP).title(title).border_style(dim)),
        panel,
    );

    // Footer: a prompt when one is open, else the buttons for this selection.
    app.buttons.clear();
    let footer_line = match &app.prompt {
        Some(Prompt::Confirm { question, .. }) => Line::from(vec![
            Span::styled(question.clone(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw("  [y] yes  [n] no"),
        ]),
        Some(Prompt::Text { label, buffer, .. }) => Line::from(vec![
            Span::styled(
                format!("{label}: "),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{buffer}█")),
            Span::styled("   enter sends · esc cancels", dim),
        ]),
        None => {
            let mut spans = Vec::new();
            let mut x = footer.x;
            for (key, name) in app.keys() {
                let text = format!("[{key}] {name}");
                let len = text.chars().count() as u16;
                app.buttons.push((Rect::new(x, footer.y, len, 1), key));
                spans.push(Span::styled(text, Style::default().fg(PURPLE)));
                spans.push(Span::raw("  "));
                x += len + 2;
            }
            spans.push(Span::styled("[?] keys", dim));
            Line::from(spans)
        }
    };
    let flash = Line::styled(app.flash.clone(), Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(vec![footer_line, flash]), footer);
}

fn fetch(paths: &Paths, app: &mut App) {
    let call = |method: &str, params: Value| ipc::call(&paths.socket_path(), method, params, Duration::from_secs(3));
    match (
        call("status", json!({})),
        call("tickets", json!({ "all": true })),
        call("candidates", json!({})),
    ) {
        (Ok(status), Ok(tickets), Ok(candidates)) => app.load(
            status,
            tickets.as_array().cloned().unwrap_or_default(),
            candidates.as_array().cloned().unwrap_or_default(),
        ),
        _ => app.online = false,
    }
}

fn perform(paths: &Paths, app: &mut App, action: Action) {
    match action {
        Action::Daemon(method, params) => match ipc::call(&paths.socket_path(), method, params, Duration::from_secs(10)) {
            Ok(_) => app.flash = format!("{} done", method.replace('_', " ")),
            Err(e) => app.flash = format!("{}: {e}", method.replace('_', " ")),
        },
        Action::Open { workspace, pane } => match crate::herdr::Herdr::from_env() {
            Ok(herdr) => {
                let focused = workspace.as_deref().map(|w| herdr.workspace_focus(w));
                if let Some(pane) = &pane {
                    let _ = herdr.call("agent.focus", json!({ "target": pane }));
                }
                app.flash = match focused {
                    Some(Ok(())) => "opened".into(),
                    Some(Err(e)) => format!("open: {e}"),
                    None => "this ticket has no workspace yet".into(),
                };
            }
            Err(e) => app.flash = e.to_string(),
        },
    }
}

/// Runs the cockpit until the pane closes. Returns `Ok(true)` when the daemon's
/// build changed and the caller should restart into the new binary.
pub fn run(profile: &str) -> Result<bool, String> {
    let paths = Paths::for_profile(profile);
    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), event::EnableMouseCapture);
    let mut app = App::new(profile);
    let mut fetched = Instant::now() - REFRESH;
    let outcome = loop {
        if fetched.elapsed() >= REFRESH {
            fetched = Instant::now();
            fetch(&paths, &mut app);
            if let Some(build) = app.status["build"].as_str() {
                if app.online && !build.is_empty() && build != crate::daemon::BUILD {
                    break Ok(true);
                }
            }
        }
        if let Err(e) = terminal.draw(|frame| draw(frame, &mut app)) {
            break Err(format!("draw: {e}"));
        }
        match event::poll(Duration::from_millis(250)) {
            Ok(true) => {
                let action = match event::read() {
                    // Ctrl-C would close the pane, which has no shell under it.
                    Ok(Event::Key(key)) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.prompt = None;
                        None
                    }
                    Ok(Event::Key(key)) => app.on_key(key),
                    Ok(Event::Mouse(mouse)) => app.on_mouse(mouse),
                    Ok(_) => None,
                    Err(e) => break Err(format!("input: {e}")),
                };
                if let Some(action) = action {
                    perform(&paths, &mut app, action);
                    fetch(&paths, &mut app);
                    fetched = Instant::now();
                }
            }
            Ok(false) => {}
            Err(e) => break Err(format!("input: {e}")),
        }
    };
    let _ = ratatui::crossterm::execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app() -> App {
        let mut app = App::new("work");
        let now = crate::core::clock::now_iso();
        app.load(
            json!({ "needs_you": 1, "workers": { "running": 1, "max": 3 }, "candidates": 1, "memory": 4 }),
            vec![
                json!({ "id": "t-run111", "title": "Build the export", "stage": "working", "mode": "autonomous", "updated_at": now,
                    "assignments": [{ "role": "worker", "state": "running", "pane_id": "p2", "report": { "state": "progress", "summary": "half way" } }],
                    "execution": { "workspace_id": "w2", "branch": "sa/run111", "base": "origin/main (abc)" } }),
                json!({ "id": "t-plan22", "title": "Fix login retries", "stage": "person/review", "mode": "checkpoint", "updated_at": now,
                    "checkpoint": { "name": "plan", "summary": "1. reproduce\n2. lock the cache" },
                    "pack": "software-dev@1/bugfix", "required_checkpoints": ["plan"], "approved_checkpoints": [],
                    "assignments": [{ "role": "worker", "state": "running", "pane_id": "p1" }],
                    "execution": { "workspace_id": "w1" } }),
                json!({ "id": "t-done33", "title": "Old thing", "stage": "ended", "mode": "checkpoint", "updated_at": now, "assignments": [] }),
            ],
            vec![json!({ "request": { "id": "issues:12", "source": "issues", "actor": "ana", "text": "Crash on login\nsteps", "meta": { "rule": "source:issues", "workflow_hint": "bugfix" } } })],
        );
        app
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn what_needs_you_comes_first_and_is_selected() {
        let mut app = app();
        assert_eq!(app.rows[0], Row::Section("NEEDS YOU".into(), 1));
        assert_eq!(app.ticket().unwrap()["id"], "t-plan22", "the selection starts on what needs you");
        let text = screen(&mut app, 80, 24);
        assert!(
            text.contains("◆ SUPERAGENT · work") && text.contains("needs you 1 · workers 1/3 · to route 1"),
            "{text}"
        );
        let order: Vec<usize> = ["NEEDS YOU (1)", "TO ROUTE (1)", "RUNNING (1)", "DONE TODAY (1)"]
            .iter()
            .map(|s| text.find(s).unwrap_or_else(|| panic!("{s} missing in\n{text}")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]));
        assert!(text.contains("Waiting at checkpoint 'plan'") && text.contains("2. lock the cache"));
        assert!(text.contains("[a] approve  [c] changes  [o] open"), "{text}");
        if std::env::var_os("COCKPIT_SHOW").is_some() {
            println!("{text}");
        }
        // A narrow split still fits: nothing wraps into the next row.
        let narrow = screen(&mut app, 40, 24);
        assert!(narrow.lines().all(|l| l.chars().count() <= 40));
        assert!(narrow.contains("plan22"));
    }

    #[test]
    fn keys_approve_only_after_a_yes_and_send_changes_with_text() {
        let mut app = app();
        assert_eq!(app.on_key(key(KeyCode::Char('a'))), None, "approval asks first");
        assert!(matches!(app.prompt, Some(Prompt::Confirm { .. })));
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), None);
        assert!(app.prompt.is_none(), "no is no");
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            Some(Action::Daemon("approve", json!({ "ticket": "t-plan22" })))
        );
        app.on_key(key(KeyCode::Char('c')));
        for c in "drop step 2".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert!(screen(&mut app, 80, 24).contains("what must change: drop step 2"));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Daemon(
                "request_changes",
                json!({ "ticket": "t-plan22", "comment": "drop step 2" })
            ))
        );
        // Enter opens the ticket's workspace and pane.
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Open {
                workspace: Some("w1".into()),
                pane: Some("p1".into())
            })
        );
        // A key that does not apply to the selection does nothing.
        assert_eq!(app.on_key(key(KeyCode::Char('r'))), None);
        assert!(app.prompt.is_none());
    }

    #[test]
    fn moving_skips_headings_and_a_request_can_be_routed_from_here() {
        let mut app = app();
        app.on_key(key(KeyCode::Down));
        let candidate = app.candidate().expect("the request to route comes next");
        assert_eq!(candidate["request"]["id"], "issues:12");
        assert_eq!(app.keys(), vec![('a', "accept"), ('i', "ignore")]);
        match app.on_key(key(KeyCode::Char('a'))) {
            Some(Action::Daemon("decide", params)) => {
                assert_eq!(params["request_id"], "issues:12");
                assert_eq!(
                    params["outcome"]["rules"],
                    json!(["source:issues"]),
                    "a source item is admitted by its rule"
                );
                assert_eq!(params["outcome"]["workflow"], "bugfix");
                assert_eq!(params["outcome"]["title"], "Crash on login");
            }
            other => panic!("unexpected {other:?}"),
        }
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.ticket().unwrap()["id"], "t-run111");
        assert!(!app.keys().iter().any(|(k, _)| *k == 'a'), "nothing to approve on a running ticket");
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Daemon(
                "stop_ticket",
                json!({ "ticket": "t-run111", "reason": "stopped from the cockpit" })
            ))
        );
        // A refresh keeps the selection on the same ticket even when the order changes.
        let (status, mut tickets, candidates) = (app.status.clone(), app.tickets.clone(), app.candidates.clone());
        tickets.reverse();
        app.load(status, tickets, candidates);
        assert_eq!(app.ticket().unwrap()["id"], "t-run111");
    }

    #[test]
    fn the_mouse_selects_opens_and_presses_buttons() {
        let mut app = app();
        let text = screen(&mut app, 80, 24);
        let row = text.lines().position(|l| l.contains("Build the export")).unwrap() as u16;
        let click = |column: u16, row: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(app.on_mouse(click(5, row)), None);
        assert_eq!(app.ticket().unwrap()["id"], "t-run111");
        assert!(
            matches!(app.on_mouse(click(5, row)), Some(Action::Open { .. })),
            "a second click opens"
        );
        // The footer buttons are drawn for the new selection; clicking `[s] stop` asks first.
        let text = screen(&mut app, 80, 24);
        let footer = text.lines().position(|l| l.contains("[s] stop")).unwrap();
        let column = text.lines().nth(footer).unwrap().find("[s] stop").unwrap() as u16;
        assert_eq!(app.on_mouse(click(column + 1, footer as u16)), None);
        assert!(matches!(&app.prompt, Some(Prompt::Confirm { then: Pending::Stop(id), .. }) if id == "t-run111"));
    }

    #[test]
    fn a_stopped_daemon_is_said_plainly() {
        let mut app = App::new("work");
        let text = screen(&mut app, 80, 12);
        assert!(text.contains("daemon not running: herdr-superagent start --profile work"));
    }
}
