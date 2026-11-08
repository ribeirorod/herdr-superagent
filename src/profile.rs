//! The profile file (PLAN §3), as much of it as phase 0 needs, and the
//! `Policy` the engine reads from it. Calibration and the full schema come in
//! phase 1.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::model::*;
use crate::core::ports::{GateResult, Policy};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub superagent: SuperagentSection,
    pub limits: Limits,
    pub modes: Modes,
    pub routing: Routing,
    pub memory: Memory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SuperagentSection {
    /// `claude`, `codex` or `pi`: the harness of the superagent session.
    pub harness: String,
    /// The model for the superagent session; empty means the harness default.
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_workers: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Modes {
    pub default: Mode,
    /// The most autonomous mode any ticket may run in.
    pub cap: Mode,
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
    pub semantic: bool,
}

impl Default for SuperagentSection {
    fn default() -> Self {
        SuperagentSection {
            harness: "claude".into(),
            model: String::new(),
        }
    }
}
impl Default for Limits {
    fn default() -> Self {
        Limits { max_workers: 3 }
    }
}
impl Default for Modes {
    fn default() -> Self {
        Modes {
            default: Mode::Checkpoint,
            cap: Mode::Autonomous,
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
        }
    }
}

impl Profile {
    /// Loads the profile file, writing the defaults first when it is missing.
    pub fn load_or_init(path: &Path) -> Result<Profile, String> {
        if !path.exists() {
            let profile = Profile::default();
            profile.save(path)?;
            return Ok(profile);
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| format!("serialize profile: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// The policy version: a digest of the profile's content.
    pub fn version(&self) -> String {
        let text = toml::to_string(self).unwrap_or_default();
        let digest = Sha256::digest(text.as_bytes());
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }
}

/// The engine's `Policy`, read from a profile. The profile can be replaced
/// while the daemon runs (`profile set`, phase 1).
pub struct ProfilePolicy {
    profile: std::cell::RefCell<Profile>,
}

impl ProfilePolicy {
    pub fn new(profile: Profile) -> ProfilePolicy {
        ProfilePolicy {
            profile: std::cell::RefCell::new(profile),
        }
    }
    pub fn snapshot(&self) -> Profile {
        self.profile.borrow().clone()
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
    fn mode_for(&self, _request: &Request, proposed: Option<Mode>) -> Mode {
        let profile = self.profile.borrow();
        proposed.unwrap_or(profile.modes.default).stricter(profile.modes.cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_and_version_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        let profile = Profile::load_or_init(&path).unwrap();
        assert!(path.exists());
        assert_eq!(profile, Profile::default());
        let mut changed = profile.clone();
        changed.modes.cap = Mode::Checkpoint;
        assert_ne!(changed.version(), profile.version());
        changed.save(&path).unwrap();
        assert_eq!(Profile::load_or_init(&path).unwrap(), changed);
    }

    #[test]
    fn policy_caps_modes_and_gates_sources() {
        let mut profile = Profile::default();
        profile.modes.cap = Mode::Checkpoint;
        let policy = ProfilePolicy::new(profile);
        let mut request = Request {
            id: "1".into(),
            source: "you".into(),
            actor: "me".into(),
            human: true,
            at: "t".into(),
            text: "x".into(),
            workflow: None,
            mode: None,
            project: None,
            meta: serde_json::Value::Null,
        };
        assert_eq!(policy.mode_for(&request, Some(Mode::Autonomous)), Mode::Checkpoint);
        assert!(matches!(
            policy.gate(&request),
            GateResult::Admissible {
                strength: Strength::Explicit,
                ..
            }
        ));
        request.source = "jira".into();
        assert!(matches!(
            policy.gate(&request),
            GateResult::Admissible {
                strength: Strength::Inferred,
                ..
            }
        ));
        request.human = false;
        assert!(matches!(policy.gate(&request), GateResult::Inadmissible { .. }));
    }
}
