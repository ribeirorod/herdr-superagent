//! Workflow packs (PLAN §9): roles, workflows and checkpoints as files. The
//! engine knows nothing about software or research; a pack says which roles a
//! kind of work has, in which order they act, and where a person reviews.
//!
//! Built-in packs are compiled in; a directory `packs/<id>/` in the profile
//! adds packs or replaces a built-in one with the same id.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// `(pack, relative path, content)` of every built-in pack file.
const BUILTIN: &[(&str, &str, &str)] = &[
    ("general", "pack.toml", include_str!("../packs/general/pack.toml")),
    ("general", "roles/worker.md", include_str!("../packs/general/roles/worker.md")),
    (
        "general",
        "roles/researcher.md",
        include_str!("../packs/general/roles/researcher.md"),
    ),
    ("general", "roles/reviewer.md", include_str!("../packs/general/roles/reviewer.md")),
    (
        "general",
        "workflows/general.toml",
        include_str!("../packs/general/workflows/general.toml"),
    ),
    (
        "general",
        "workflows/research.toml",
        include_str!("../packs/general/workflows/research.toml"),
    ),
    (
        "general",
        "workflows/writing.toml",
        include_str!("../packs/general/workflows/writing.toml"),
    ),
    ("software-dev", "pack.toml", include_str!("../packs/software-dev/pack.toml")),
    (
        "software-dev",
        "roles/worker.md",
        include_str!("../packs/software-dev/roles/worker.md"),
    ),
    (
        "software-dev",
        "roles/planner.md",
        include_str!("../packs/software-dev/roles/planner.md"),
    ),
    (
        "software-dev",
        "roles/builder.md",
        include_str!("../packs/software-dev/roles/builder.md"),
    ),
    (
        "software-dev",
        "roles/reviewer.md",
        include_str!("../packs/software-dev/roles/reviewer.md"),
    ),
    (
        "software-dev",
        "workflows/feature.toml",
        include_str!("../packs/software-dev/workflows/feature.toml"),
    ),
    (
        "software-dev",
        "workflows/bugfix.toml",
        include_str!("../packs/software-dev/workflows/bugfix.toml"),
    ),
    (
        "software-dev",
        "workflows/code-review.toml",
        include_str!("../packs/software-dev/workflows/code-review.toml"),
    ),
];

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PackMeta {
    pub id: String,
    pub version: String,
    pub description: String,
    pub default_workflow: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Role {
    pub name: String,
    /// `hard`, `small` or `review`: which model tier of the profile runs it.
    pub tier: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Match {
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Step {
    pub role: String,
    pub goal: String,
    /// The worker stops here for review, under this name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// The step runs as a child in a sibling pane (`spawn`), not inline.
    pub spawn: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risk: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Workflow {
    pub id: String,
    pub description: String,
    #[serde(rename = "match")]
    pub match_: Match,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Pack {
    pub meta: PackMeta,
    pub roles: BTreeMap<String, Role>,
    pub workflows: Vec<Workflow>,
    /// `builtin` or the directory it was read from.
    pub origin: String,
}

/// A workflow with the pack it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Chosen {
    pub pack: String,
    pub version: String,
    pub workflow: Workflow,
    pub roles: BTreeMap<String, Role>,
}

impl Chosen {
    /// `software-dev@1/feature`, recorded on the ticket.
    pub fn label(&self) -> String {
        format!("{}@{}/{}", self.pack, self.version, self.workflow.id)
    }

    /// The checkpoints a person must approve before the ticket may end.
    pub fn required_checkpoints(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for step in &self.workflow.steps {
            if let Some(name) = &step.checkpoint {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.get(name)
    }

    /// The role of the ticket's own (root) worker: the first step's.
    pub fn root_role(&self) -> String {
        self.workflow
            .steps
            .first()
            .map(|s| s.role.clone())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "worker".into())
    }

    /// The steps as the worker's brief shows them.
    pub fn brief_section(&self, role: &str) -> String {
        let mut out = format!(
            "## Workflow: {} ({})\n\n{}\n\n",
            self.workflow.id,
            self.label(),
            self.workflow.description
        );
        for (n, step) in self.workflow.steps.iter().enumerate() {
            let mut line = format!("{}. **{}**: {}", n + 1, step.role, step.goal);
            if step.spawn || (step.role != role && !step.role.is_empty()) {
                line.push_str(&format!(
                    " → another role: `herdr-superagent spawn --role {} --task \"…\"`, then `watch` it",
                    step.role
                ));
            }
            if let Some(name) = &step.checkpoint {
                line.push_str(&format!(" → then `herdr-superagent checkpoint {name}` and end your turn"));
            }
            if !step.risk.is_empty() {
                line.push_str(&format!(" (risk: {})", step.risk.join(", ")));
            }
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str("\nFollow the steps in order. A step for another role is not yours to do inline: spawn it, so the person can watch it in its own pane.\n");
        out
    }
}

fn parse_role(name: &str, text: &str) -> Role {
    let mut role = Role {
        name: name.to_string(),
        tier: "hard".into(),
        prompt: text.trim().to_string(),
    };
    if let Some(rest) = text.strip_prefix("---") {
        if let Some((front, body)) = rest.split_once("\n---") {
            for line in front.lines() {
                if let Some((key, value)) = line.split_once(':') {
                    let value = value.trim().trim_matches('"').to_string();
                    match key.trim() {
                        "name" if !value.is_empty() => role.name = value,
                        "tier" if !value.is_empty() => role.tier = value,
                        _ => {}
                    }
                }
            }
            role.prompt = body.trim().to_string();
        }
    }
    role
}

fn add_file(pack: &mut Pack, relative: &str, content: &str) -> Result<(), String> {
    if relative == "pack.toml" {
        pack.meta = toml::from_str(content).map_err(|e| format!("pack.toml: {e}"))?;
    } else if let Some(name) = relative.strip_prefix("roles/").and_then(|n| n.strip_suffix(".md")) {
        let role = parse_role(name, content);
        pack.roles.insert(role.name.clone(), role);
    } else if let Some(name) = relative.strip_prefix("workflows/").and_then(|n| n.strip_suffix(".toml")) {
        let mut workflow: Workflow = toml::from_str(content).map_err(|e| format!("{relative}: {e}"))?;
        if workflow.id.is_empty() {
            workflow.id = name.to_string();
        }
        if workflow.steps.is_empty() {
            return Err(format!("{relative}: a workflow needs at least one step"));
        }
        pack.workflows.retain(|w| w.id != workflow.id);
        pack.workflows.push(workflow);
    }
    Ok(())
}

fn builtin() -> Vec<Pack> {
    let mut packs: BTreeMap<&str, Pack> = BTreeMap::new();
    for (id, relative, content) in BUILTIN {
        let pack = packs.entry(id).or_insert_with(|| Pack {
            origin: "builtin".into(),
            ..Pack::default()
        });
        if let Err(e) = add_file(pack, relative, content) {
            tracing::error!(pack = id, %e, "built-in pack file is invalid");
        }
    }
    packs.into_values().collect()
}

fn read_dir_pack(dir: &Path) -> Result<Pack, String> {
    let mut pack = Pack {
        origin: dir.display().to_string(),
        ..Pack::default()
    };
    let meta = std::fs::read_to_string(dir.join("pack.toml")).map_err(|e| format!("{}: {e}", dir.join("pack.toml").display()))?;
    add_file(&mut pack, "pack.toml", &meta)?;
    for sub in ["roles", "workflows"] {
        let Ok(entries) = std::fs::read_dir(dir.join(sub)) else { continue };
        let mut files: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        files.sort();
        for file in files {
            let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let content = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            add_file(&mut pack, &format!("{sub}/{name}"), &content)?;
        }
    }
    if pack.meta.id.is_empty() {
        pack.meta.id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("pack").to_string();
    }
    if pack.workflows.is_empty() {
        return Err(format!("{}: no workflows", dir.display()));
    }
    Ok(pack)
}

/// The built-in packs plus the profile's own; a profile pack replaces a built-in one with its id.
/// Invalid packs are skipped and named in the second value.
pub fn load(profile_dir: &Path) -> (Vec<Pack>, Vec<String>) {
    let mut packs = builtin();
    let mut problems = Vec::new();
    if let Ok(entries) = std::fs::read_dir(profile_dir.join("packs")) {
        let mut dirs: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            match read_dir_pack(&dir) {
                Ok(pack) => {
                    packs.retain(|p| p.meta.id != pack.meta.id);
                    packs.push(pack);
                }
                Err(e) => problems.push(e),
            }
        }
    }
    (packs, problems)
}

fn chosen(pack: &Pack, workflow: &Workflow) -> Chosen {
    Chosen {
        pack: pack.meta.id.clone(),
        version: pack.meta.version.clone(),
        workflow: workflow.clone(),
        roles: pack.roles.clone(),
    }
}

/// Finds a workflow by `pack/workflow`, by workflow id, or by pack id (its default workflow).
pub fn find(packs: &[Pack], name: &str) -> Option<Chosen> {
    let name = name.trim();
    if let Some((pack_id, workflow_id)) = name.split_once('/') {
        let pack = packs.iter().find(|p| p.meta.id == pack_id)?;
        return pack.workflows.iter().find(|w| w.id == workflow_id).map(|w| chosen(pack, w));
    }
    for pack in packs.iter().rev() {
        if let Some(workflow) = pack.workflows.iter().find(|w| w.id == name) {
            return Some(chosen(pack, workflow));
        }
    }
    let pack = packs.iter().find(|p| p.meta.id == name)?;
    pack.workflows
        .iter()
        .find(|w| w.id == pack.meta.default_workflow)
        .or(pack.workflows.first())
        .map(|w| chosen(pack, w))
}

/// The deterministic router (PLAN §8): the workflow whose keywords the text
/// names most, with the pack's order breaking ties. No keyword at all means
/// `feature` for a repository and `general` otherwise.
pub fn score(packs: &[Pack], text: &str, in_repo: bool) -> (String, usize) {
    let lower = format!(" {} ", text.to_lowercase());
    let mut best: Option<(String, usize)> = None;
    for pack in packs {
        for workflow in &pack.workflows {
            let hits = workflow
                .match_
                .keywords
                .iter()
                .filter(|k| {
                    let k = k.to_lowercase();
                    lower.match_indices(k.as_str()).any(|(i, _)| {
                        let before = lower[..i].chars().next_back().map(|c| !c.is_alphanumeric()).unwrap_or(true);
                        let after = lower[i + k.len()..].chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(true);
                        before && (after || k.ends_with(' '))
                    })
                })
                .count();
            if hits > 0 && best.as_ref().map(|(_, b)| hits > *b).unwrap_or(true) {
                best = Some((workflow.id.clone(), hits));
            }
        }
    }
    best.unwrap_or_else(|| (if in_repo { "feature".into() } else { "general".into() }, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_packs_load_and_resolve_by_every_name() {
        let dir = tempfile::tempdir().unwrap();
        let (packs, problems) = load(dir.path());
        assert!(problems.is_empty());
        assert_eq!(packs.len(), 2);
        let feature = find(&packs, "software-dev").unwrap();
        assert_eq!(feature.workflow.id, "feature");
        assert_eq!(feature.label(), "software-dev@1/feature");
        assert_eq!(feature.required_checkpoints(), vec!["plan", "implemented"]);
        assert_eq!(find(&packs, "bugfix").unwrap().pack, "software-dev");
        assert_eq!(find(&packs, "general/research").unwrap().root_role(), "researcher");
        assert_eq!(feature.role("reviewer").unwrap().tier, "review");
        assert!(find(&packs, "nope").is_none());
        let brief = feature.brief_section("worker");
        assert!(brief.contains("spawn --role reviewer") && brief.contains("checkpoint plan"));
    }

    #[test]
    fn the_scorer_routes_by_keywords_and_falls_back_by_place() {
        let dir = tempfile::tempdir().unwrap();
        let (packs, _) = load(dir.path());
        assert_eq!(score(&packs, "Fix the crash when the login token is expired", true).0, "bugfix");
        assert_eq!(
            score(&packs, "Research which queue libraries we could use and compare them", false).0,
            "research"
        );
        assert_eq!(score(&packs, "Add an endpoint to export invoices", true).0, "feature");
        assert_eq!(
            score(&packs, "prefix padding", true),
            ("feature".into(), 0),
            "words are matched whole"
        );
        assert_eq!(score(&packs, "hello", false).0, "general");
    }

    #[test]
    fn a_profile_pack_replaces_a_built_in_one_and_bad_packs_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("packs").join("general");
        std::fs::create_dir_all(mine.join("workflows")).unwrap();
        std::fs::create_dir_all(mine.join("roles")).unwrap();
        std::fs::write(
            mine.join("pack.toml"),
            "id = \"general\"\nversion = \"7\"\ndefault_workflow = \"general\"\n",
        )
        .unwrap();
        std::fs::write(
            mine.join("workflows").join("general.toml"),
            "steps = [{ role = \"worker\", goal = \"do it\" }]\n",
        )
        .unwrap();
        std::fs::write(mine.join("roles").join("worker.md"), "---\ntier: small\n---\nBe brief.\n").unwrap();
        let broken = dir.path().join("packs").join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        let (packs, problems) = load(dir.path());
        assert_eq!(problems.len(), 1);
        let general = find(&packs, "general").unwrap();
        assert_eq!(general.version, "7");
        assert!(general.required_checkpoints().is_empty());
        assert_eq!(general.role("worker").unwrap().tier, "small");
        assert_eq!(general.role("worker").unwrap().prompt, "Be brief.");
    }
}
