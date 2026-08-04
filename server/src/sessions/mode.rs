//! What drives a session's agents: a person talking to one resident main
//! agent, or a workflow definition driving a sequence of step agents.
//!
//! The two differ in where subagent trees hang. An interactive session roots
//! one, at its main agent. A run roots one per step, because a step agent may
//! spawn subagents exactly as a main agent may and they belong to that step —
//! which is why the fold routes a subagent event through
//! [`SessionModeState::tree_of_parent_mut`] and
//! [`SessionModeState::tree_of_node_mut`] rather than at a single tree.
//!
//! Serialized through [`SessionModeWire`] so that snapshots written before this
//! type existed — which carried `subagents` at the top level of
//! [`crate::sessions::session_actor::SessionState`] — still load with their
//! tree. `SessionState` is snapshotted into the journal, so its shape is a
//! durability contract: a plain field move would load every deployed session
//! with an empty subagent tree.

use crate::sessions::subagents::{SubAgentParent, SubAgentTree};
use crate::sessions::workflow::WorkflowRunState;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What drives a session's agents. Fixed at creation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "SessionModeWire", into = "SessionModeWire")]
pub enum SessionModeState {
    /// A person or a routine talks to one resident main agent, which may spawn
    /// subagents.
    Interactive { subagents: SubAgentTree },
    /// A workflow definition drives a sequence of step agents. There is no main
    /// agent; each step roots its own subagent tree.
    Workflow(WorkflowRunState),
}

impl Default for SessionModeState {
    fn default() -> Self {
        Self::Interactive {
            subagents: SubAgentTree::default(),
        }
    }
}

impl SessionModeState {
    /// The subagent tree rooted at this session's main agent. Empty for a run,
    /// whose trees hang off its steps — ask [`Self::tree_of_node`] instead.
    pub fn subagents(&self) -> &SubAgentTree {
        match self {
            Self::Interactive { subagents } => subagents,
            Self::Workflow(_) => empty_tree(),
        }
    }

    /// Mutable access to the interactive tree. `None` for a run.
    pub fn subagents_mut(&mut self) -> Option<&mut SubAgentTree> {
        match self {
            Self::Interactive { subagents } => Some(subagents),
            Self::Workflow(_) => None,
        }
    }

    /// The tree a node with this id belongs to.
    pub fn tree_of_node(&self, id: Uuid) -> Option<&SubAgentTree> {
        match self {
            Self::Interactive { subagents } => subagents.get(&id).map(|_| subagents),
            Self::Workflow(run) => run.tree_of(id).map(|(_, tree)| tree),
        }
    }

    /// The tree a node with this id belongs to, for the fold.
    pub fn tree_of_node_mut(&mut self, id: Uuid) -> Option<&mut SubAgentTree> {
        match self {
            Self::Interactive { subagents } => Some(subagents),
            Self::Workflow(run) => {
                let index = run.tree_of(id).map(|(i, _)| i)?;
                run.steps.get_mut(index as usize).map(|s| &mut s.subagents)
            }
        }
    }

    /// The tree a spawn by `parent` belongs in. In a run, `Main` means the step
    /// currently in flight — the only agent that can be spawning.
    pub fn tree_of_parent_mut(&mut self, parent: SubAgentParent) -> Option<&mut SubAgentTree> {
        match parent {
            SubAgentParent::Main => match self {
                Self::Interactive { subagents } => Some(subagents),
                Self::Workflow(run) => {
                    let index = run.current()?;
                    run.steps.get_mut(index as usize).map(|s| &mut s.subagents)
                }
            },
            SubAgentParent::SubAgent(id) => self.tree_of_node_mut(id),
        }
    }

    /// Every tree this session holds, for readers that span the whole session.
    pub fn trees(&self) -> Vec<&SubAgentTree> {
        match self {
            Self::Interactive { subagents } => vec![subagents],
            Self::Workflow(run) => run.steps.iter().map(|s| &s.subagents).collect(),
        }
    }

    /// The run, when this session is one.
    pub fn run(&self) -> Option<&WorkflowRunState> {
        match self {
            Self::Workflow(run) => Some(run),
            Self::Interactive { .. } => None,
        }
    }

    /// Mutable access to the run, for the event fold.
    pub fn run_mut(&mut self) -> Option<&mut WorkflowRunState> {
        match self {
            Self::Workflow(run) => Some(run),
            Self::Interactive { .. } => None,
        }
    }

    /// Whether this session is a workflow run.
    pub fn is_workflow(&self) -> bool {
        matches!(self, Self::Workflow(_))
    }
}

/// A tree with no nodes, for the modes that root none of their own.
fn empty_tree() -> &'static SubAgentTree {
    static EMPTY: std::sync::OnceLock<SubAgentTree> = std::sync::OnceLock::new();
    EMPTY.get_or_init(SubAgentTree::default)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<WorkflowRunState>,
}

impl From<SessionModeWire> for SessionModeState {
    fn from(w: SessionModeWire) -> Self {
        // The run decides, not the tag: a snapshot carrying one is a run
        // whatever it claims. An unrecognised kind reads as Interactive rather
        // than failing the load — a session that cannot deserialize is a
        // session that cannot be opened at all.
        match w.run {
            Some(run) => Self::Workflow(run),
            None => Self::Interactive {
                subagents: w.subagents,
            },
        }
    }
}

impl From<SessionModeState> for SessionModeWire {
    fn from(m: SessionModeState) -> Self {
        match m {
            SessionModeState::Interactive { subagents } => Self {
                kind: Some("Interactive".to_string()),
                subagents,
                run: None,
            },
            SessionModeState::Workflow(run) => Self {
                kind: Some("Workflow".to_string()),
                subagents: SubAgentTree::default(),
                run: Some(run),
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
