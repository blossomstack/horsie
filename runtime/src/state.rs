//! Per-caller shell-like state for tool execution: a working-directory
//! override and an env overlay, keyed by the session id stamped on each tool
//! call. Callers sharing one runtime (an agent and its subagents, sessions on
//! a shared local daemon) are isolated by identity; unidentified callers
//! (`None`) share a default bucket. Entries live for the runtime process's
//! lifetime — bounded by the number of distinct callers attaching to it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Env changes to apply to a spawned command. Named fields so call sites
/// can't swap sets and unsets.
#[derive(Default)]
pub struct EnvOverlay {
    pub sets: Vec<(String, String)>,
    pub unsets: Vec<String>,
}

impl EnvOverlay {
    /// Apply the overlay to a child process command: sets win over the
    /// inherited environment, unsets remove even inherited variables.
    pub fn apply_to(&self, command: &mut tokio::process::Command) {
        for (name, value) in &self.sets {
            command.env(name, value);
        }
        for name in &self.unsets {
            command.env_remove(name);
        }
    }
}

#[derive(Default)]
struct SessionEnv {
    /// Working-directory override; `None` = resolve per call from `workspace`.
    cwd: Option<PathBuf>,
    /// Env overlay: `Some(v)` = set, `None` = unset.
    env: HashMap<String, Option<String>>,
}

#[derive(Default)]
pub struct RuntimeState {
    sessions: Mutex<HashMap<Option<String>, SessionEnv>>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The caller's cwd override if set, else `fallback`.
    pub fn effective_dir(&self, session: &Option<String>, fallback: &Path) -> PathBuf {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        match sessions.get(session).and_then(|s| s.cwd.clone()) {
            Some(dir) => dir,
            None => fallback.to_path_buf(),
        }
    }

    /// Store (`Some`) or clear (`None`) the caller's cwd override.
    pub fn set_cwd(&self, session: &Option<String>, dir: Option<PathBuf>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.entry(session.clone()).or_default().cwd = dir;
    }

    /// Record an env set (`Some`) or unset (`None`) for the caller.
    pub fn apply_env(&self, session: &Option<String>, name: String, value: Option<String>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions
            .entry(session.clone())
            .or_default()
            .env
            .insert(name, value);
    }

    /// The caller's accumulated env overlay.
    pub fn env_overlay(&self, session: &Option<String>) -> EnvOverlay {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let mut overlay = EnvOverlay::default();
        if let Some(env) = sessions.get(session) {
            for (name, value) in &env.env {
                match value {
                    Some(v) => overlay.sets.push((name.clone(), v.clone())),
                    None => overlay.unsets.push(name.clone()),
                }
            }
        }
        overlay
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    fn s(name: &str) -> Option<String> {
        Some(name.to_string())
    }

    #[test]
    fn no_override_returns_the_fallback() {
        let state = RuntimeState::new();
        assert_eq!(
            state.effective_dir(&None, Path::new("/root")),
            PathBuf::from("/root")
        );
    }

    #[test]
    fn cwd_override_wins_and_reset_clears_it() {
        let state = RuntimeState::new();
        state.set_cwd(&s("a"), Some(PathBuf::from("/sub")));
        assert_eq!(
            state.effective_dir(&s("a"), Path::new("/root")),
            PathBuf::from("/sub")
        );
        state.set_cwd(&s("a"), None);
        assert_eq!(
            state.effective_dir(&s("a"), Path::new("/root")),
            PathBuf::from("/root")
        );
    }

    #[test]
    fn sessions_are_isolated_and_none_is_the_default_bucket() {
        let state = RuntimeState::new();
        state.set_cwd(&s("a"), Some(PathBuf::from("/a")));
        state.set_cwd(&None, Some(PathBuf::from("/default")));
        assert_eq!(
            state.effective_dir(&s("b"), Path::new("/root")),
            PathBuf::from("/root")
        );
        assert_eq!(
            state.effective_dir(&None, Path::new("/root")),
            PathBuf::from("/default")
        );
    }

    #[test]
    fn env_overlay_accumulates_sets_and_unsets_per_session() {
        let state = RuntimeState::new();
        state.apply_env(&s("a"), "SET_VAR".into(), Some("1".into()));
        state.apply_env(&s("a"), "GONE_VAR".into(), None);
        let overlay = state.env_overlay(&s("a"));
        assert_eq!(overlay.sets, vec![("SET_VAR".to_string(), "1".to_string())]);
        assert_eq!(overlay.unsets, vec!["GONE_VAR".to_string()]);
        let other = state.env_overlay(&s("b"));
        assert!(other.sets.is_empty() && other.unsets.is_empty());
    }
}
