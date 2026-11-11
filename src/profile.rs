//! The profile (PLAN §3): what a context's superagent may use and how it
//! works, in `profile.toml`. `profile set` validates and versions every
//! change, shows a diff, protects what affects review, and replays recent
//! requests to show whose mode would change (PLAN §4).

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::model::*;
use crate::core::ports::{GateResult, Policy};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub description: String,
    /// Set by calibration once the interview is done.
    pub calibrated: bool,
    pub superagent: SuperagentSection,
    pub tools: Tools,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<Repo>,
    pub models: Models,
    pub limits: Limits,
    pub modes: Modes,
    pub routing: Routing,
    pub memory: Memory,
    pub state_repo: StateRepo,
    pub harness_flags: HarnessFlags,
    /// Where work arrives from besides the person typing (issues, pull requests, mail).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source>,
}

/// One place requests come from, polled on its own interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub id: String,
    /// `github` (the `gh` CLI), `command` (a program printing JSON items) or
    /// `session` (the superagent session looks through its MCP servers).
    pub kind: String,
    pub enabled: bool,
    pub every_minutes: u64,
    /// github: `owner/name`; empty means the repository in `project`.
    pub repo: String,
    /// github: `issues` or `prs`.
    pub what: String,
    /// github: a search such as `assignee:@me label:bug`.
    pub query: String,
    pub limit: u32,
    /// command: the program and its arguments.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// session: what to look for, in words ("Jira issues assigned to me in project API").
    pub instructions: String,
    /// The repository items of this source are about.
    pub project: String,
    /// The workflow and mode routing should prefer for this source's items.
    pub workflow: String,
    pub mode: String,
}

impl Default for Source {
    fn default() -> Self {
        Source {
            id: String::new(),
            kind: "session".into(),
            enabled: true,
            every_minutes: 15,
            repo: String::new(),
            what: "issues".into(),
            query: String::new(),
            limit: 20,
            command: Vec::new(),
            instructions: String::new(),
            project: String::new(),
            workflow: String::new(),
            mode: String::new(),
        }
    }
}

/// Flags that turn a harness's own permission prompts off, per mode (PLAN §6).
/// Only `autonomous` has defaults; the other modes keep the prompts on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessFlags {
    /// Mark each new worktree as trusted in the harness's own config, so no trust dialog stops the worker.
    pub pre_trust: bool,
    /// Keep the person's global harness settings (output style, hooks) out of the
    /// session and the workers: Claude then loads project and local settings only.
    pub isolate_settings: bool,
    /// Flags the superagent session always starts with, per harness. The default
    /// takes Claude's own subagents away, so routing cannot turn into doing the work.
    pub session: std::collections::BTreeMap<String, Vec<String>>,
    /// Environment variables exported into the session's and every worker's pane before the
    /// harness starts (never secrets: the profile is plain text).
    pub env: std::collections::BTreeMap<String, String>,
    /// Flags every worker starts with, per harness. The default takes Claude's own
    /// subagents away, so a worker delegates with `spawn`, where you can see it.
    pub worker: std::collections::BTreeMap<String, Vec<String>>,
    pub autonomous: std::collections::BTreeMap<String, Vec<String>>,
    pub checkpoint: std::collections::BTreeMap<String, Vec<String>>,
    pub propose: std::collections::BTreeMap<String, Vec<String>>,
}

impl Default for HarnessFlags {
    fn default() -> Self {
        let mut autonomous = std::collections::BTreeMap::new();
        autonomous.insert("claude".to_string(), vec!["--dangerously-skip-permissions".to_string()]);
        autonomous.insert("codex".to_string(), vec!["--full-auto".to_string()]);
        let mut no_subagents = std::collections::BTreeMap::new();
        no_subagents.insert(
            "claude".to_string(),
            vec!["--disallowedTools".to_string(), "Task,Agent".to_string()],
        );
        HarnessFlags {
            pre_trust: false,
            isolate_settings: false,
            env: Default::default(),
            session: no_subagents.clone(),
            worker: no_subagents,
            autonomous,
            checkpoint: Default::default(),
            propose: Default::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SuperagentSection {
    /// `claude`, `codex` or `pi`: the harness of the superagent session.
    pub harness: String,
    /// The model for the superagent session; empty means the harness default.
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tools {
    pub code_host: String,
    pub issues: String,
    pub chat: String,
    pub mail: String,
    /// `mcp`: through the harness's own MCP servers and connectors.
    pub access: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Repo {
    pub path: String,
    pub default_branch: String,
    pub notes: String,
}

/// Which harness and model per kind of task, with fallbacks in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Models {
    pub hard: Tier,
    pub small: Tier,
    pub review: Tier,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tier {
    #[serde(rename = "use")]
    pub use_: Vec<HarnessModel>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessModel {
    pub harness: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_workers: u32,
    /// Close an ended ticket's workspace after this many minutes; 0 keeps it until `tidy`.
    pub tidy_after_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Modes {
    pub default: Mode,
    /// The most autonomous mode any ticket may run in.
    pub cap: Mode,
    /// Rules that cap the mode of matching requests (PLAN §6).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ModeRule>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeRule {
    #[serde(rename = "match")]
    pub match_: RuleMatch,
    pub mode: Option<Mode>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// A project path, with `*` as a wildcard and `~` as home.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Routing {
    /// `always`, `ambiguous` or `never`.
    pub routing: String,
    /// How long the superagent session may take on a candidate before it is requeued.
    pub timeout_seconds: u64,
    /// Processing paused: nothing is evaluated or resumed.
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub scope: String,
    /// Search by meaning as well as by words (a vector per entry, local only).
    pub semantic: bool,
    /// `hash` (built in, no model), `ollama` (a local Ollama server) or `command` (a local program).
    pub embedder: String,
    /// ollama: the embedding model.
    pub model: String,
    /// ollama: where the server listens.
    pub endpoint: String,
    /// command: a program that reads text on stdin and prints a JSON array of numbers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// `builtin`: the brief carries what this project's tickets and notes learned.
    /// `claude-mem`: retrieval is left to claude-mem in the worker's harness; the brief
    /// names the project to search and carries no memory of its own. Ticket summaries
    /// are recorded either way: they are the ticket's record, and what moves between machines.
    pub backend: String,
    /// Folders of curated notes (one fact per markdown file) read into memory, per project.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<NotesSource>,
    /// Leading text a hook puts in front of your prompts; `memory import` drops it.
    /// Preambles that open several sessions are detected without being listed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub import_strip: Vec<String>,
}

/// A folder of notes someone (or a summariser) already wrote for a repository.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotesSource {
    /// The folder with the `*.md` notes, or its parent when the notes are in `notes/`.
    pub path: String,
    /// The repository the notes are about (a path), or a project key.
    pub project: String,
}

/// The per-profile state repository (PLAN §8a).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StateRepo {
    /// `none`, `local` (a git repo in the profile directory) or `remote`.
    pub kind: String,
    pub remote: String,
    pub branch: String,
}

impl HarnessFlags {
    /// `NAME=value` exports for a pane: the profile's own, plus what the memory backend needs.
    pub fn exports(&self, memory: &Memory) -> Vec<String> {
        let mut env = self.env.clone();
        if memory.backend == "claude-mem" {
            // A ticket's worktree must land in its repository's project, not in one of its own.
            env.entry("CLAUDE_MEM_PROJECT_NAME_SOURCE".into())
                .or_insert_with(|| "git-remote".into());
        }
        env.into_iter()
            .filter(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .map(|(name, value)| format!("{name}={value}"))
            .collect()
    }

    /// The flags a harness starts with besides its model and mode: `who` is `session` or `worker`.
    pub fn always(&self, harness: &str, who: &str, state_dir: &Path) -> Vec<String> {
        let table = if who == "session" { &self.session } else { &self.worker };
        let mut flags = table.get(harness).cloned().unwrap_or_default();
        if harness == "claude" {
            // The plugin's own CLI never needs a permission prompt: reports, checkpoints and
            // routing would otherwise stop at one each time. Everything else still asks.
            let bin = crate::envpath::self_path();
            flags.extend(["--allowedTools".to_string(), format!("Bash(herdr-superagent:*),Bash({bin}:*)")]);
            // Briefs live in the plugin's state directory, outside the pane's own folder.
            flags.extend(["--add-dir".to_string(), state_dir.display().to_string()]);
        }
        if self.isolate_settings && harness == "claude" {
            flags.extend(["--setting-sources".to_string(), "project,local".to_string()]);
        }
        flags
    }
}

impl Default for SuperagentSection {
    fn default() -> Self {
        // Routing needs judgement, not the most expensive model; calibration changes this.
        SuperagentSection {
            harness: "claude".into(),
            model: "sonnet".into(),
        }
    }
}

impl Default for Models {
    fn default() -> Self {
        let claude = |model: &str| Tier {
            use_: vec![HarnessModel {
                harness: "claude".into(),
                model: model.into(),
            }],
        };
        Models {
            hard: claude("opus"),
            small: claude("haiku"),
            review: claude("sonnet"),
        }
    }
}
impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_workers: 3,
            tidy_after_minutes: 60,
        }
    }
}
impl Default for Modes {
    fn default() -> Self {
        Modes {
            default: Mode::Checkpoint,
            cap: Mode::Autonomous,
            rules: Vec::new(),
        }
    }
}
impl Default for Routing {
    fn default() -> Self {
        Routing {
            routing: "ambiguous".into(),
            timeout_seconds: 300,
            paused: false,
        }
    }
}
impl Default for Memory {
    fn default() -> Self {
        Memory {
            scope: "project".into(),
            semantic: false,
            embedder: "hash".into(),
            model: "nomic-embed-text".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            command: Vec::new(),
            backend: "builtin".into(),
            notes: Vec::new(),
            import_strip: Vec::new(),
        }
    }
}
impl Default for StateRepo {
    fn default() -> Self {
        StateRepo {
            kind: "none".into(),
            remote: String::new(),
            branch: "main".into(),
        }
    }
}

/// Keys only you may change, from your own shell with `--yes`: everything
/// that affects which mode a request resolves to.
pub const PROTECTED_PREFIXES: &[&str] = &["modes", "limits", "routing", "sources"];

impl Profile {
    pub fn new(id: &str) -> Profile {
        Profile {
            id: id.to_string(),
            ..Profile::default()
        }
    }

    /// Loads the profile file, writing the defaults first when it is missing.
    pub fn load_or_init(path: &Path) -> Result<Profile, String> {
        if !path.exists() {
            let id = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("default");
            let profile = Profile::new(id);
            profile.save(path)?;
            return Ok(profile);
        }
        Profile::parse(&std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?)
    }

    pub fn parse(text: &str) -> Result<Profile, String> {
        let profile: Profile = toml::from_str(text).map_err(|e| format!("profile.toml: {e}"))?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !["claude", "codex", "pi"].contains(&self.superagent.harness.as_str()) && !self.superagent.harness.is_empty() {
            // Other herdr kinds work too; warn only on the empty value.
        }
        if self.superagent.harness.trim().is_empty() {
            return Err("superagent.harness must name a harness (claude, codex, pi, …)".into());
        }
        if self.limits.max_workers == 0 {
            return Err("limits.max_workers must be at least 1".into());
        }
        if !["always", "ambiguous", "never"].contains(&self.routing.routing.as_str()) {
            return Err(format!(
                "routing.routing must be always, ambiguous or never, not '{}'",
                self.routing.routing
            ));
        }
        if self.routing.timeout_seconds < 10 {
            return Err("routing.timeout_seconds must be at least 10".into());
        }
        if !["none", "local", "remote"].contains(&self.state_repo.kind.as_str()) {
            return Err(format!(
                "state_repo.kind must be none, local or remote, not '{}'",
                self.state_repo.kind
            ));
        }
        if self.state_repo.kind == "remote" && self.state_repo.remote.trim().is_empty() {
            return Err("state_repo.remote is required when state_repo.kind is remote".into());
        }
        if self.memory.scope != "project" {
            return Err("memory.scope must be 'project' (memory never crosses projects)".into());
        }
        if !["builtin", "claude-mem"].contains(&self.memory.backend.as_str()) {
            return Err(format!(
                "memory.backend must be builtin or claude-mem, not '{}'",
                self.memory.backend
            ));
        }
        for notes in &self.memory.notes {
            if notes.path.trim().is_empty() || notes.project.trim().is_empty() {
                return Err("every [[memory.notes]] entry needs a path and a project".into());
            }
        }
        if !["hash", "ollama", "command"].contains(&self.memory.embedder.as_str()) {
            return Err(format!(
                "memory.embedder must be hash, ollama or command, not '{}'",
                self.memory.embedder
            ));
        }
        if self.memory.embedder == "command" && self.memory.command.is_empty() {
            return Err("memory.embedder = \"command\" needs memory.command = [\"program\", …]".into());
        }
        if self.memory.embedder == "ollama" {
            // Embeddings carry the text: only a local server is allowed.
            let host = self
                .memory
                .endpoint
                .split("://")
                .nth(1)
                .unwrap_or("")
                .split(['/', ':'])
                .next()
                .unwrap_or("");
            if !["127.0.0.1", "localhost", "[::1]", "::1"].contains(&host) {
                return Err(format!(
                    "memory.endpoint must be a local server (127.0.0.1 or localhost), not '{}'",
                    self.memory.endpoint
                ));
            }
        }
        for repo in &self.repos {
            if repo.path.trim().is_empty() {
                return Err("every [[repos]] entry needs a path".into());
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for source in &self.sources {
            if source.id.trim().is_empty()
                || !source.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                || source.id == "you"
            {
                return Err(format!(
                    "every [[sources]] entry needs an id of letters, digits, - and _ (got '{}')",
                    source.id
                ));
            }
            if !seen.insert(source.id.clone()) {
                return Err(format!("source id '{}' is used twice", source.id));
            }
            if !["github", "command", "session"].contains(&source.kind.as_str()) {
                return Err(format!(
                    "sources.{}: kind must be github, command or session, not '{}'",
                    source.id, source.kind
                ));
            }
            if source.kind == "command" && source.command.is_empty() {
                return Err(format!(
                    "sources.{}: kind command needs `command = [\"program\", \"arg\", …]`",
                    source.id
                ));
            }
            if source.kind == "session" && source.instructions.trim().is_empty() {
                return Err(format!(
                    "sources.{}: kind session needs `instructions` saying what to look for",
                    source.id
                ));
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, self.to_toml()).map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// The policy version: a digest of the profile's content.
    pub fn version(&self) -> String {
        let digest = Sha256::digest(self.to_toml().as_bytes());
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }

    /// The mode a request resolves to: the proposal (or the default), capped by
    /// the first matching rule and by the profile's cap.
    pub fn mode_for(&self, request: &Request, proposed: Option<Mode>) -> Mode {
        let mut mode = proposed.unwrap_or(self.modes.default);
        for rule in &self.modes.rules {
            if rule.matches(request) {
                if let Some(cap) = rule.mode {
                    mode = mode.stricter(cap);
                }
                break;
            }
        }
        mode.stricter(self.modes.cap)
    }
}

impl ModeRule {
    pub fn matches(&self, request: &Request) -> bool {
        let workflow_ok = match &self.match_.workflow {
            Some(w) => request.workflow.as_deref() == Some(w.as_str()),
            None => true,
        };
        let project_ok = match &self.match_.project {
            Some(pattern) => request
                .project
                .as_deref()
                .map(|p| glob_match(&expand_home(pattern), &expand_home(p)))
                .unwrap_or(false),
            None => true,
        };
        (self.match_.workflow.is_some() || self.match_.project.is_some()) && workflow_ok && project_ok
    }
}

pub fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

// ---- profile set / undo ----

/// One `key=value` assignment from the command line. Values are TOML when they
/// parse as TOML (numbers, booleans, arrays, inline tables), else strings.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub key: String,
    pub value: toml::Value,
}

impl Assignment {
    pub fn parse(text: &str) -> Result<Assignment, String> {
        let (key, raw) = text.split_once('=').ok_or_else(|| format!("expected key=value, got '{text}'"))?;
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' || c == '[' || c == ']' || c == '+')
        {
            return Err(format!("invalid key '{key}'"));
        }
        let raw = raw.trim();
        let value = toml::from_str::<toml::Table>(&format!("v = {raw}"))
            .ok()
            .and_then(|t| t.get("v").cloned())
            .unwrap_or_else(|| toml::Value::String(raw.to_string()));
        Ok(Assignment {
            key: key.to_string(),
            value,
        })
    }

    pub fn protected(&self) -> bool {
        let head = self.key.split(['.', '[']).next().unwrap_or("");
        PROTECTED_PREFIXES.contains(&head)
    }
}

/// Sets `key` (dotted, with `[n]` for array items; `repos[+]` appends) in a TOML document.
fn set_path(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<(), String> {
    let mut parts: Vec<String> = Vec::new();
    for segment in key.split('.') {
        let (name, rest) = match segment.find('[') {
            Some(i) => (&segment[..i], &segment[i..]),
            None => (segment, ""),
        };
        if !name.is_empty() {
            parts.push(name.to_string());
        }
        let mut rest = rest;
        while let Some(end) = rest.find(']') {
            parts.push(format!("[{}", &rest[1..end]));
            rest = &rest[end + 1..];
        }
    }
    let mut current = root;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        if let Some(index) = part.strip_prefix('[') {
            let array = match current {
                toml::Value::Array(a) => a,
                other => {
                    *other = toml::Value::Array(Vec::new());
                    other.as_array_mut().ok_or("array")?
                }
            };
            let idx = if index == "+" {
                array.push(toml::Value::Table(toml::Table::new()));
                array.len() - 1
            } else {
                index.parse::<usize>().map_err(|_| format!("bad index '{index}' in '{key}'"))?
            };
            if idx >= array.len() {
                return Err(format!("index {idx} out of range in '{key}' (len {})", array.len()));
            }
            if last {
                array[idx] = value;
                return Ok(());
            }
            current = &mut array[idx];
        } else {
            let table = match current {
                toml::Value::Table(t) => t,
                other => {
                    *other = toml::Value::Table(toml::Table::new());
                    other.as_table_mut().ok_or("table")?
                }
            };
            if last {
                table.insert(part.clone(), value);
                return Ok(());
            }
            current = table.entry(part.clone()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
        }
    }
    Err(format!("empty key '{key}'"))
}

/// Applies assignments to the profile text and validates the result.
pub fn apply(text: &str, assignments: &[Assignment]) -> Result<Profile, String> {
    let mut root: toml::Value = toml::from_str(text).map_err(|e| format!("profile.toml: {e}"))?;
    for assignment in assignments {
        set_path(&mut root, &assignment.key, assignment.value.clone())?;
    }
    let profile: Profile = root.try_into().map_err(|e| format!("profile.toml: {e}"))?;
    profile.validate()?;
    Ok(profile)
}

/// Line diff between two TOML texts, `-`/`+` lines only.
pub fn diff(before: &str, after: &str) -> Vec<String> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    // LCS table for small files.
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| format!("- {l}")));
    out.extend(b[j..].iter().map(|l| format!("+ {l}")));
    out
}

/// A request whose mode would change under a new profile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModeChange {
    pub request_id: String,
    pub text: String,
    pub before: Mode,
    pub after: Mode,
}

/// Replays requests, each with the mode that was proposed for it (the
/// superagent's or the request's own), against both profiles (PLAN §4: the last 50).
pub fn mode_changes(before: &Profile, after: &Profile, requests: &[(Request, Option<Mode>)]) -> Vec<ModeChange> {
    requests
        .iter()
        .rev()
        .take(50)
        .filter_map(|(request, proposed)| {
            let proposed = proposed.or(request.mode);
            let (b, a) = (before.mode_for(request, proposed), after.mode_for(request, proposed));
            (b != a).then(|| ModeChange {
                request_id: request.id.clone(),
                text: request.text.lines().next().unwrap_or("").chars().take(60).collect(),
                before: b,
                after: a,
            })
        })
        .collect()
}

/// Versioned profile files on disk: `profile.toml` plus `history/<n>-<version>.toml` for undo.
pub struct ProfileStore {
    pub path: PathBuf,
}

impl ProfileStore {
    pub fn new(path: PathBuf) -> ProfileStore {
        ProfileStore { path }
    }
    fn history_dir(&self) -> PathBuf {
        self.path.with_file_name("history")
    }
    pub fn read_text(&self) -> Result<String, String> {
        Profile::load_or_init(&self.path)?;
        std::fs::read_to_string(&self.path).map_err(|e| format!("read {}: {e}", self.path.display()))
    }
    /// Saves a new profile, keeping the previous text in history.
    pub fn commit(&self, previous: &str, profile: &Profile) -> Result<(), String> {
        let dir = self.history_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        let previous_version = Profile::parse(previous).map(|p| p.version()).unwrap_or_else(|_| "invalid".into());
        std::fs::write(dir.join(format!("{n:04}-{previous_version}.toml")), previous).map_err(|e| e.to_string())?;
        profile.save(&self.path)
    }
    /// Restores the latest history entry; returns the restored text.
    pub fn undo(&self) -> Result<Option<String>, String> {
        let dir = self.history_dir();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        entries.sort();
        let Some(latest) = entries.pop() else { return Ok(None) };
        let text = std::fs::read_to_string(&latest).map_err(|e| e.to_string())?;
        Profile::parse(&text)?;
        std::fs::write(&self.path, &text).map_err(|e| e.to_string())?;
        std::fs::remove_file(&latest).map_err(|e| e.to_string())?;
        Ok(Some(text))
    }
}

/// The engine's `Policy`, read from a profile. The profile can be replaced
/// while the daemon runs (`profile set` → `profile_reload`).
pub struct ProfilePolicy {
    profile: RefCell<Profile>,
    /// Where the profile's own workflow packs live.
    profile_dir: PathBuf,
}

impl ProfilePolicy {
    pub fn new(profile: Profile, profile_dir: &Path) -> ProfilePolicy {
        ProfilePolicy {
            profile: RefCell::new(profile),
            profile_dir: profile_dir.to_path_buf(),
        }
    }
    /// The workflow packs as they are on disk now.
    pub fn packs(&self) -> Vec<crate::packs::Pack> {
        let (packs, problems) = crate::packs::load(&self.profile_dir);
        for problem in problems {
            tracing::warn!(%problem, "skipping an invalid workflow pack");
        }
        packs
    }
    pub fn snapshot(&self) -> Profile {
        self.profile.borrow().clone()
    }
    pub fn replace(&self, profile: Profile) {
        *self.profile.borrow_mut() = profile;
    }
}

impl Policy for ProfilePolicy {
    fn version(&self) -> String {
        self.profile.borrow().version()
    }
    fn paused(&self) -> bool {
        self.profile.borrow().routing.paused
    }
    fn gate(&self, request: &Request) -> GateResult {
        if !request.human {
            return GateResult::Inadmissible { reason: "notHuman".into() };
        }
        if request.source == "you" {
            GateResult::Admissible {
                strength: Strength::Explicit,
                basis: MandateBasis::Explicit {
                    workflow: request.workflow.clone(),
                },
            }
        } else {
            GateResult::Admissible {
                strength: Strength::Inferred,
                basis: MandateBasis::Assessed { rules: Vec::new() },
            }
        }
    }
    fn revalidate(&self, mandate: &Mandate) -> bool {
        mandate.mode <= self.profile.borrow().modes.cap
    }
    fn admits_inferred(&self, rules: &[String]) -> bool {
        !rules.is_empty()
    }
    fn eval_timeout_ms(&self) -> i64 {
        (self.profile.borrow().routing.timeout_seconds as i64) * 1000
    }
    fn mode_for(&self, request: &Request, proposed: Option<Mode>) -> Mode {
        self.profile.borrow().mode_for(request, proposed)
    }
    fn plan_for(&self, request: &Request, workflow: Option<&str>, title: &str) -> WorkPlan {
        let packs = self.packs();
        let in_repo = request
            .project
            .as_deref()
            .map(|p| Path::new(&expand_home(p)).join(".git").exists())
            .unwrap_or(false);
        // A named workflow the packs know wins; an unknown name is kept as a label and
        // runs as the nearest thing the text suggests.
        let chosen = workflow.and_then(|name| crate::packs::find(&packs, name)).or_else(|| {
            let (id, _) = crate::packs::score(&packs, &format!("{title}\n{}", request.text), in_repo);
            crate::packs::find(&packs, &id)
        });
        match chosen {
            Some(chosen) => WorkPlan {
                workflow: Some(
                    workflow
                        .filter(|w| crate::packs::find(&packs, w).is_none())
                        .map(str::to_string)
                        .unwrap_or_else(|| chosen.workflow.id.clone()),
                ),
                pack: Some(chosen.label()),
                root_role: chosen.root_role(),
                required_checkpoints: chosen.required_checkpoints(),
            },
            None => WorkPlan {
                workflow: workflow.map(str::to_string),
                pack: None,
                root_role: "worker".into(),
                required_checkpoints: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(workflow: Option<&str>, project: Option<&str>) -> Request {
        Request {
            id: "1".into(),
            source: "you".into(),
            actor: "me".into(),
            human: true,
            at: "t".into(),
            text: "x".into(),
            workflow: workflow.map(str::to_string),
            mode: None,
            project: project.map(str::to_string),
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn defaults_roundtrip_and_version_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work").join("profile.toml");
        let profile = Profile::load_or_init(&path).unwrap();
        assert!(path.exists());
        assert_eq!(profile.id, "work");
        let mut changed = profile.clone();
        changed.modes.cap = Mode::Checkpoint;
        assert_ne!(changed.version(), profile.version());
        changed.save(&path).unwrap();
        assert_eq!(Profile::load_or_init(&path).unwrap(), changed);
    }

    #[test]
    fn rules_cap_matching_requests_and_the_cap_caps_everything() {
        let mut profile = Profile::default();
        profile.modes.rules = vec![
            ModeRule {
                match_: RuleMatch {
                    workflow: Some("research".into()),
                    project: None,
                },
                mode: Some(Mode::Autonomous),
            },
            ModeRule {
                match_: RuleMatch {
                    workflow: None,
                    project: Some("~/scratch/*".into()),
                },
                mode: Some(Mode::Autonomous),
            },
            ModeRule {
                match_: RuleMatch {
                    workflow: Some("deploy".into()),
                    project: None,
                },
                mode: Some(Mode::Propose),
            },
        ];
        assert_eq!(
            profile.mode_for(&request(Some("research"), None), Some(Mode::Autonomous)),
            Mode::Autonomous
        );
        assert_eq!(
            profile.mode_for(&request(Some("deploy"), None), Some(Mode::Autonomous)),
            Mode::Propose
        );
        assert_eq!(profile.mode_for(&request(None, None), None), Mode::Checkpoint);
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            profile.mode_for(&request(None, Some(&format!("{home}/scratch/x"))), Some(Mode::Autonomous)),
            Mode::Autonomous
        );
        profile.modes.cap = Mode::Checkpoint;
        assert_eq!(
            profile.mode_for(&request(Some("research"), None), Some(Mode::Autonomous)),
            Mode::Checkpoint
        );
        assert!(glob_match("a*c", "abbbc") && !glob_match("a*c", "abd"));
    }

    #[test]
    fn set_applies_dotted_keys_arrays_and_validates() {
        let text = Profile::new("work").to_toml();
        let assignments = [
            Assignment::parse("superagent.harness=codex").unwrap(),
            Assignment::parse("limits.max_workers=5").unwrap(),
            Assignment::parse("repos[+].path=~/work/api").unwrap(),
            Assignment::parse("repos[0].default_branch=main").unwrap(),
            Assignment::parse(r#"models.hard.use=[{ harness = "claude", model = "opus" }, { harness = "codex", model = "gpt-5" }]"#)
                .unwrap(),
            Assignment::parse("calibrated=true").unwrap(),
        ];
        let profile = apply(&text, &assignments).unwrap();
        assert_eq!(profile.superagent.harness, "codex");
        assert_eq!(profile.limits.max_workers, 5);
        assert_eq!(profile.repos[0].path, "~/work/api");
        assert_eq!(profile.repos[0].default_branch, "main");
        assert_eq!(profile.models.hard.use_[1].model, "gpt-5");
        assert_eq!(Profile::default().models.small.use_[0].model, "haiku");
        assert!(profile.calibrated);
        assert!(assignments[1].protected() && !assignments[2].protected() && !assignments[0].protected());
        assert!(apply(&text, &[Assignment::parse("limits.max_workers=0").unwrap()])
            .unwrap_err()
            .contains("max_workers"));
        assert!(apply(&text, &[Assignment::parse("routing.routing=sometimes").unwrap()])
            .unwrap_err()
            .contains("routing"));
        assert!(apply(&text, &[Assignment::parse("memory.scope=global").unwrap()])
            .unwrap_err()
            .contains("project"));
        assert!(Assignment::parse("nokey").is_err());
        let changed = diff(&text, &profile.to_toml());
        assert!(changed.iter().any(|l| l == "- harness = \"claude\"") && changed.iter().any(|l| l == "+ harness = \"codex\""));
    }

    #[test]
    fn replays_requests_to_show_mode_changes_and_undoes_from_history() {
        let before = Profile::default();
        let mut after = before.clone();
        after.modes.cap = Mode::Checkpoint;
        let mut autonomous = request(Some("research"), None);
        autonomous.mode = Some(Mode::Autonomous);
        let changes = mode_changes(
            &before,
            &after,
            &[
                (request(None, None), None),
                (autonomous, None),
                (request(Some("x"), None), Some(Mode::Autonomous)),
            ],
        );
        assert_eq!(changes.len(), 2, "the request's own mode and the superagent's proposal both count");
        assert_eq!((changes[1].before, changes[1].after), (Mode::Autonomous, Mode::Checkpoint));

        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().join("p").join("profile.toml"));
        let text = store.read_text().unwrap();
        let next = apply(&text, &[Assignment::parse("superagent.model=opus").unwrap()]).unwrap();
        store.commit(&text, &next).unwrap();
        assert_eq!(Profile::load_or_init(&store.path).unwrap().superagent.model, "opus");
        assert!(store.undo().unwrap().is_some());
        assert_eq!(Profile::load_or_init(&store.path).unwrap().superagent.model, "sonnet");
        assert!(store.undo().unwrap().is_none());
    }
}
