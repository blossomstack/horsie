//! Per-agent shell-like state for tool execution: a working-directory override
//! and an env overlay, keyed by the agent id stamped on each tool call.
//!
//! Agents sharing one runtime — an agent and its subagents — are isolated by
//! identity. The id is required on the wire, so there is no unkeyed bucket for
//! callers to fall into by accident.
//!
//! One instance per runtime connection, so its lifetime is the connection's.
//! Today that means one entry (a session's main agent); subagents are what make
//! the map worth having, and what [`RuntimeState::forget`] exists for.

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
struct AgentEnv {
    /// Working-directory override; `None` = resolve per call from `workspace`.
    cwd: Option<PathBuf>,
    /// Env overlay: `Some(v)` = set, `None` = unset.
    env: HashMap<String, Option<String>>,
}

#[derive(Default)]
pub struct RuntimeState {
    agents: Mutex<HashMap<String, AgentEnv>>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The agent's cwd override if set, else `fallback`.
    pub fn effective_dir(&self, agent: &str, fallback: &Path) -> PathBuf {
        let agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        match agents.get(agent).and_then(|s| s.cwd.clone()) {
            Some(dir) => dir,
            None => fallback.to_path_buf(),
        }
    }

    /// Store (`Some`) or clear (`None`) the agent's cwd override.
    pub fn set_cwd(&self, agent: &str, dir: Option<PathBuf>) {
        let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        agents.entry(agent.to_string()).or_default().cwd = dir;
    }

    /// Record an env set (`Some`) or unset (`None`) for the agent.
    pub fn apply_env(&self, agent: &str, name: String, value: Option<String>) {
        let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        agents
            .entry(agent.to_string())
            .or_default()
            .env
            .insert(name, value);
    }

    /// Drop an agent's state.
    ///
    /// Not called yet, and deliberately so: this map lives for one runtime
    /// connection, which today serves exactly one agent, so it holds exactly
    /// one entry. It is the seam for subagents — once several agents share a
    /// runtime, a finished subagent's cwd/env should be dropped here rather
    /// than held until the connection closes.
    pub fn forget(&self, agent: &str) {
        self.agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(agent);
    }

    /// How many agents currently hold state. Test observability for [`Self::forget`].
    #[must_use]
    pub fn tracked_agents(&self) -> usize {
        self.agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The agent's accumulated env overlay.
    pub fn env_overlay(&self, agent: &str) -> EnvOverlay {
        let agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        let mut overlay = EnvOverlay::default();
        if let Some(env) = agents.get(agent) {
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

    #[test]
    fn no_override_returns_the_fallback() {
        let state = RuntimeState::new();
        assert_eq!(
            state.effective_dir("a", Path::new("/root")),
            PathBuf::from("/root")
        );
    }

    #[test]
    fn cwd_override_wins_and_reset_clears_it() {
        let state = RuntimeState::new();
        state.set_cwd("a", Some(PathBuf::from("/sub")));
        assert_eq!(
            state.effective_dir("a", Path::new("/root")),
            PathBuf::from("/sub")
        );
        state.set_cwd("a", None);
        assert_eq!(
            state.effective_dir("a", Path::new("/root")),
            PathBuf::from("/root")
        );
    }

    /// The property the whole feature rests on: an agent and its subagents, or
    /// two sessions on a shared daemon, never see each other's cwd.
    #[test]
    fn agents_are_isolated_from_one_another() {
        let state = RuntimeState::new();
        state.set_cwd("a", Some(PathBuf::from("/a")));
        assert_eq!(
            state.effective_dir("b", Path::new("/root")),
            PathBuf::from("/root")
        );
        assert_eq!(
            state.effective_dir("a", Path::new("/root")),
            PathBuf::from("/a")
        );
    }

    #[test]
    fn env_overlay_accumulates_sets_and_unsets_per_agent() {
        let state = RuntimeState::new();
        state.apply_env("a", "SET_VAR".into(), Some("1".into()));
        state.apply_env("a", "GONE_VAR".into(), None);
        let overlay = state.env_overlay("a");
        assert_eq!(overlay.sets, vec![("SET_VAR".to_string(), "1".to_string())]);
        assert_eq!(overlay.unsets, vec!["GONE_VAR".to_string()]);
        let other = state.env_overlay("b");
        assert!(other.sets.is_empty() && other.unsets.is_empty());
    }

    /// The seam subagents will need: one agent's state can be dropped without
    /// disturbing another's.
    #[test]
    fn forget_drops_an_agents_state() {
        let state = RuntimeState::new();
        state.set_cwd("a", Some(PathBuf::from("/a")));
        state.apply_env("a", "V".into(), Some("1".into()));
        state.set_cwd("b", Some(PathBuf::from("/b")));
        assert_eq!(state.tracked_agents(), 2);

        state.forget("a");
        assert_eq!(state.tracked_agents(), 1);
        assert_eq!(
            state.effective_dir("a", Path::new("/root")),
            PathBuf::from("/root"),
            "a forgotten agent falls back to per-call resolution"
        );
        assert!(state.env_overlay("a").sets.is_empty());
        assert_eq!(
            state.effective_dir("b", Path::new("/root")),
            PathBuf::from("/b"),
            "forgetting one agent must not disturb another"
        );
    }
}
