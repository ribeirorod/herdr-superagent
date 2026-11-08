//! Where a profile's config and state live.
//!
//! Config:  `$XDG_CONFIG_HOME/herdr-superagent/profiles/<name>/profile.toml`
//! State:   `$XDG_DATA_HOME/herdr-superagent/profiles/<name>/{db.sqlite,daemon.sock,daemon.json}`
//!
//! `HERDR_SUPERAGENT_CONFIG_DIR` and `HERDR_SUPERAGENT_STATE_DIR` override the
//! roots, for tests and throwaway sessions.

use std::path::PathBuf;

pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone)]
pub struct Paths {
    pub profile: String,
    config_root: PathBuf,
    state_root: PathBuf,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &[&str]) -> PathBuf {
    match std::env::var_os(var).filter(|v| !v.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => fallback.iter().fold(home(), |path, part| path.join(part)),
    }
}

impl Paths {
    pub fn for_profile(profile: &str) -> Paths {
        let config_root = std::env::var_os("HERDR_SUPERAGENT_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", &[".config"]).join("herdr-superagent"));
        let state_root = std::env::var_os("HERDR_SUPERAGENT_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_DATA_HOME", &[".local", "share"]).join("herdr-superagent"));
        Paths {
            profile: profile.to_string(),
            config_root,
            state_root,
        }
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.config_root.join("profiles").join(&self.profile)
    }
    pub fn profile_file(&self) -> PathBuf {
        self.profile_dir().join("profile.toml")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.state_root.join("profiles").join(&self.profile)
    }
    pub fn db_path(&self) -> PathBuf {
        self.state_dir().join("db.sqlite")
    }
    /// The daemon's socket. Unix socket paths are limited to about 100 bytes
    /// (104 on macOS), so a long state directory falls back to a short
    /// per-user path under the temp directory.
    pub fn socket_path(&self) -> PathBuf {
        let preferred = self.state_dir().join("daemon.sock");
        if preferred.as_os_str().len() < 90 {
            return preferred;
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        std::env::temp_dir()
            .join(format!("herdr-superagent-{user}"))
            .join(format!("{}.sock", self.profile))
    }
    pub fn daemon_info_path(&self) -> PathBuf {
        self.state_dir().join("daemon.json")
    }
    /// One log file per process kind: `daemon.log`, `cli.log`, `ensure.log`.
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir().join("logs")
    }
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.profile_dir())?;
        std::fs::create_dir_all(self.state_dir())?;
        if let Some(parent) = self.socket_path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

/// A profile name must be a plain slug: it names directories and a herdr workspace.
pub fn validate_profile(name: &str) -> Result<(), String> {
    let ok = !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid profile name '{name}': use letters, digits, '-' or '_' (max 40)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_profile_names() {
        assert!(validate_profile("work").is_ok());
        assert!(validate_profile("side_projects-2").is_ok());
        assert!(validate_profile("").is_err());
        assert!(validate_profile("a b").is_err());
        assert!(validate_profile("../x").is_err());
    }

    #[test]
    fn lays_out_profile_paths() {
        let paths = Paths {
            profile: "work".into(),
            config_root: PathBuf::from("/c"),
            state_root: PathBuf::from("/s"),
        };
        assert_eq!(paths.profile_file(), PathBuf::from("/c/profiles/work/profile.toml"));
        assert_eq!(paths.socket_path(), PathBuf::from("/s/profiles/work/daemon.sock"));
        let long = Paths {
            profile: "work".into(),
            config_root: PathBuf::from("/c"),
            state_root: PathBuf::from("/s".repeat(60)),
        };
        assert!(long.socket_path().as_os_str().len() < 90);
        assert!(long.socket_path().ends_with("work.sock"));
    }
}
