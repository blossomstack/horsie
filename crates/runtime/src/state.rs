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
//!
//! The map can be mirrored to a file ([`RuntimeState::with_file`]). A vendor
//! that can respawn a runtime hands it a path there, so an agent restart or a
//! hibernate does not silently reset the agent's working directory and
//! environment underneath it.

use serde::{Deserialize, Serialize};
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

#[derive(Default, Serialize, Deserialize)]
struct AgentEnv {
    /// Working-directory override; `None` = resolve per call from `workspace`.
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// Env overlay: `Some(v)` = set, `None` = unset.
    #[serde(default)]
    env: HashMap<String, Option<String>>,
}

#[derive(Default)]
pub struct RuntimeState {
    agents: Mutex<HashMap<String, AgentEnv>>,
    /// Where the map is mirrored, if anywhere. `None` keeps it in memory only,
    /// which is correct for a runtime nobody can respawn.
    file: Option<PathBuf>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A state map mirrored to `path`, loaded from it if it already exists.
    ///
    /// A file that cannot be read or parsed is treated as absent. Losing a cwd
    /// override is an inconvenience the agent can recover from in one tool
    /// call; a runtime that refuses to start is an outage.
    #[must_use]
    pub fn with_file(path: PathBuf) -> Self {
        let agents = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            agents: Mutex::new(agents),
            file: Some(path),
        }
    }

    /// Mirror the map to disk, if this state is file-backed.
    ///
    /// Best effort by design: the caller is part-way through a tool call it
    /// has already applied in memory, and failing that call because a mirror
    /// could not be written would trade a real capability for a bookkeeping
    /// one. Takes the map the caller already has locked rather than re-locking.
    fn persist(&self, agents: &HashMap<String, AgentEnv>) {
        let Some(path) = &self.file else { return };
        if let Ok(bytes) = serde_json::to_vec(agents) {
            let _ = std::fs::write(path, bytes);
        }
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
        self.persist(&agents);
    }

    /// Record an env set (`Some`) or unset (`None`) for the agent.
    pub fn apply_env(&self, agent: &str, name: String, value: Option<String>) {
        let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        agents
            .entry(agent.to_string())
            .or_default()
            .env
            .insert(name, value);
        self.persist(&agents);
    }

    /// Drop an agent's state.
    ///
    /// Not called yet, and deliberately so: this map lives for one runtime
    /// connection, which today serves exactly one agent, so it holds exactly
    /// one entry. It is the seam for subagents — once several agents share a
    /// runtime, a finished subagent's cwd/env should be dropped here rather
    /// than held until the connection closes.
    pub fn forget(&self, agent: &str) {
        let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        agents.remove(agent);
        self.persist(&agents);
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

    /// The property the whole persistence change exists for: a runtime that is
    /// killed and started again comes back with the agent's cwd and env intact,
    /// rather than silently resetting them underneath a running session.
    #[test]
    fn state_round_trips_through_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agents.json");

        let state = RuntimeState::with_file(path.clone());
        state.set_cwd("a", Some(PathBuf::from("/sub")));
        state.apply_env("a", "SET_VAR".into(), Some("1".into()));
        state.apply_env("a", "GONE_VAR".into(), None);

        let revived = RuntimeState::with_file(path);
        assert_eq!(
            revived.effective_dir("a", Path::new("/root")),
            PathBuf::from("/sub")
        );
        let overlay = revived.env_overlay("a");
        assert_eq!(overlay.sets, vec![("SET_VAR".to_string(), "1".to_string())]);
        assert_eq!(overlay.unsets, vec!["GONE_VAR".to_string()]);
    }

    #[test]
    fn forget_is_persisted_too() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agents.json");
        let state = RuntimeState::with_file(path.clone());
        state.set_cwd("a", Some(PathBuf::from("/a")));
        state.set_cwd("b", Some(PathBuf::from("/b")));
        state.forget("a");
        assert_eq!(RuntimeState::with_file(path).tracked_agents(), 1);
    }

    /// A truncated or hand-edited file must not stop the runtime from starting.
    #[test]
    fn a_corrupt_file_starts_empty_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agents.json");
        std::fs::write(&path, b"{not json").unwrap();
        let state = RuntimeState::with_file(path);
        assert_eq!(state.tracked_agents(), 0);
    }

    /// An absent file is a normal cold start, not an error.
    #[test]
    fn a_missing_file_starts_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let state = RuntimeState::with_file(tmp.path().join("absent.json"));
        assert_eq!(state.tracked_agents(), 0);
        state.set_cwd("a", Some(PathBuf::from("/a")));
        assert!(tmp.path().join("absent.json").exists(), "created on write");
    }

    #[test]
    fn an_in_memory_state_writes_nothing() {
        let state = RuntimeState::new();
        state.set_cwd("a", Some(PathBuf::from("/a")));
        assert_eq!(
            state.effective_dir("a", Path::new("/root")),
            PathBuf::from("/a")
        );
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
