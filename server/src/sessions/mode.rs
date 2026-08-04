//! What drives a session's agents. One variant today; workflow runs add a
//! second.
//!
//! Serialized through [`SessionModeWire`] so that snapshots written before this
//! type existed — which carried `subagents` at the top level of
//! [`crate::sessions::session_actor::SessionState`] — still load with their
//! tree. `SessionState` is snapshotted into the journal, so its shape is a
//! durability contract: a plain field move would load every deployed session
//! with an empty subagent tree.

use crate::sessions::subagents::SubAgentTree;
use serde::{Deserialize, Serialize};

/// What drives a session's agents. Fixed at creation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "SessionModeWire", into = "SessionModeWire")]
pub enum SessionModeState {
    /// A person or a routine talks to one resident main agent, which may spawn
    /// subagents.
    Interactive { subagents: SubAgentTree },
}

impl Default for SessionModeState {
    fn default() -> Self {
        Self::Interactive {
            subagents: SubAgentTree::default(),
        }
    }
}

impl SessionModeState {
    /// The subagent tree rooted at this session's main agent.
    pub fn subagents(&self) -> &SubAgentTree {
        match self {
            Self::Interactive { subagents } => subagents,
        }
    }

    /// Mutable access, for the event fold.
    pub fn subagents_mut(&mut self) -> &mut SubAgentTree {
        match self {
            Self::Interactive { subagents } => subagents,
        }
    }
}

/// The serialized shape. `kind` is absent in every snapshot written before this
/// type existed; those carry only `subagents` and are read as `Interactive`.
/// Snapshots written from here always carry `kind`, so the fallback is
/// read-only.
#[derive(Serialize, Deserialize)]
struct SessionModeWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default)]
    subagents: SubAgentTree,
}

impl From<SessionModeWire> for SessionModeState {
    fn from(w: SessionModeWire) -> Self {
        // Only one kind exists. An unknown one from a future version reads as
        // Interactive rather than failing the whole snapshot load — a session
        // that cannot deserialize is a session that cannot be opened at all.
        Self::Interactive {
            subagents: w.subagents,
        }
    }
}

impl From<SessionModeState> for SessionModeWire {
    fn from(m: SessionModeState) -> Self {
        match m {
            SessionModeState::Interactive { subagents } => Self {
                kind: Some("Interactive".to_string()),
                subagents,
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::subagents::SubAgentParent;

    /// A snapshot written before `mode` existed carries `subagents` at the top
    /// level. It must load with its tree intact — anything else silently drops
    /// every subagent of every deployed session.
    #[test]
    fn a_pre_mode_snapshot_keeps_its_subagent_tree() {
        let legacy = serde_json::json!({
            "subagents": {
                "nodes": {
                    "3f1a2b4c-0000-4000-8000-000000000001": {
                        "parent": "Main",
                        "label": "reader",
                        "task": "read the file",
                        "depth": 1,
                        "status": "Completed",
                        "output": "done",
                        "error": null,
                        "notified": true
                    }
                }
            }
        });
        let mode: SessionModeState = serde_json::from_value(legacy).unwrap();
        let id = uuid::Uuid::parse_str("3f1a2b4c-0000-4000-8000-000000000001").unwrap();
        let node = mode.subagents().get(&id).unwrap();
        assert_eq!(node.label, "reader");
        assert_eq!(node.parent, SubAgentParent::Main);
    }

    /// A snapshot with no subagents at all — the overwhelmingly common case.
    #[test]
    fn an_empty_legacy_snapshot_loads_as_interactive() {
        let mode: SessionModeState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(mode.subagents().is_empty());
    }

    /// The new shape round-trips.
    #[test]
    fn the_tagged_shape_round_trips() {
        let mode = SessionModeState::default();
        let json = serde_json::to_value(&mode).unwrap();
        assert_eq!(json["kind"], "Interactive");
        let back: SessionModeState = serde_json::from_value(json).unwrap();
        assert!(back.subagents().is_empty());
    }
}
