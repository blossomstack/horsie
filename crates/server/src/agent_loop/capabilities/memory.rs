//! `memory_*`: the spaces an agent can read and write between sessions.
//!
//! Which spaces an agent reaches is a per-agent decision, so the names live on
//! the capability and travel into the layer verbatim; the toolbox refuses a
//! space that is not in that list, so an agent's reach is decided once, here.
//!
//! Setup-only, and both halves of setup matter: the tools are wrapped on and
//! the index of what is already saved is rendered into the prompt. An agent
//! that cannot see a memory exists never loads it.
//!
//! # Why it claims no tool name
//!
//! Its session-side twin claimed the five names [`TOOLS`] lists, so that a
//! `memory_load` could not fall through to the open-namespace capability behind
//! it and be journaled as something else. Here there is nothing to claim: the
//! layer this pushes in [`super::Capability::setup`] answers those calls on the
//! agent's task, and routing them to this mailbox would only stop a tool that
//! already works. So [`super::Capability::handle`] returns `None` and
//! [`super::Capability::layer`] claims nothing, and [`crate::memory::MemoryToolbox`]
//! goes on advertising the five itself.
//!
//! [`TOOLS`] survives the move anyway, because it is what the tests check the
//! equipped layer against — a sixth tool the toolbox grows and this list does
//! not is a real drift, and one that is otherwise silent.

use super::{Decision, Msg, SetupError};
use crate::agent_loop::capabilities::or_empty;
use crate::sessions::runners::loading::{AgentSpec, Loading};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The tools this capability equips, as named by the memory toolbox.
pub const TOOLS: [&str; 5] = [
    "memory_load",
    "memory_create",
    "memory_update",
    "memory_delete",
    "memory_list",
];

/// The memory spaces this agent can reach.
///
/// `Default` is the agent that names none, which equips no layer at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryCapability {
    /// The spaces this agent can reach, by name.
    pub spaces: Vec<String>,
}

impl MemoryCapability {
    #[must_use]
    pub fn new(spaces: Vec<String>) -> Self {
        Self { spaces }
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl MemoryCapability {
    pub fn name(&self) -> &'static str {
        "memory"
    }

    /// Wrap the memory tools on, and render the index of what is already saved.
    ///
    /// Both, or neither. The index is what makes the tools worth having — an
    /// agent that cannot see a memory exists never loads it — so the read
    /// happens here, once, rather than being left to the first `memory_list`.
    ///
    /// No spaces, no layer: an agent that names none has nothing to read, and
    /// the tools would only ever refuse.
    pub async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        if self.spaces.is_empty() {
            return Ok(());
        }
        let Some(service) = loading.memory.clone() else {
            return Err(SetupError {
                capability: self.name(),
                reason: format!(
                    "this agent names memory spaces ({}) but no memory service is configured",
                    self.spaces.join(", ")
                ),
                fatal: false,
            });
        };
        let rows = service
            .memories_in(&self.spaces)
            .await
            .map_err(|e| SetupError {
                capability: self.name(),
                reason: format!("read the memory index: {e}"),
                fatal: false,
            })?;
        spec.say("memory", crate::memory::render_index(&rows, &self.spaces));
        let spaces = self.spaces.clone();
        spec.wrap(move |inner, _| {
            Arc::new(crate::memory::MemoryToolbox::new(
                or_empty(inner),
                service,
                spaces,
            ))
        });
        Ok(())
    }

    /// Nothing reaches this capability through the mailbox; see the module doc.
    pub fn handle(&self, _msg: &Msg) -> Option<Decision> {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{advertised_by, facts, someone_elses};
    use super::*;
    use crate::agent_loop::capabilities::testing::{equipped, loading, spec};
    use crate::agent_loop::capabilities::{Capabilities, Capability};

    /// No spaces named, no layer equipped. If this regresses every agent gets
    /// memory tools whose every call is refused.
    #[tokio::test]
    async fn no_spaces_equips_no_layer() {
        let mut spec = spec();
        MemoryCapability::new(vec![])
            .setup(&loading(), &mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.prompt.is_empty());
        assert!(spec.toolbox().is_none());
    }

    /// The named spaces reach the toolbox, which is what bounds an agent's
    /// reach at run time, and the index reaches the prompt — a memory the agent
    /// is never told about is one it never loads.
    #[tokio::test]
    async fn named_spaces_equip_the_tools_and_the_index() {
        let mut loading = loading();
        loading.memory = Some(Arc::new(crate::memory::MemoryService::new(
            crate::memory::MemoryStore::new(
                crate::db::testing::db().await,
                crate::auth::UserId::new("1"),
            ),
        )));
        let mut spec = spec();
        MemoryCapability::new(vec!["default".into(), "team".into()])
            .setup(&loading, &mut spec)
            .await
            .expect("nothing fatal");
        let index = spec
            .prompt
            .iter()
            .find(|s| s.key == "memory")
            .expect("the index is what makes the tools usable");
        assert!(index.body.contains("# Memories"));
        let names = equipped(spec);
        for tool in TOOLS {
            assert!(names.contains(&tool.to_string()), "{tool} was not equipped");
        }
    }

    /// An agent that asks for memory with no service behind it loses the tools
    /// and keeps its turn — the same call MCP makes, for the same reason.
    #[tokio::test]
    async fn no_service_is_degraded_rather_than_fatal() {
        let mut spec = spec();
        let e = MemoryCapability::new(vec!["default".into()])
            .setup(&loading(), &mut spec)
            .await
            .expect_err("no service is wired");
        assert_eq!(e.capability, "memory");
        assert!(!e.fatal);
        assert!(spec.toolbox().is_none());
    }

    /// What the move changed, pinned. The session-side twin claimed all five
    /// names; here the layer equipped in `setup` answers them on the agent's
    /// task, and claiming them at the mailbox would stop a tool that works.
    #[test]
    fn it_claims_no_tool_name_through_the_mailbox() {
        let c = Capability::Memory(MemoryCapability::new(vec!["default".into()]));
        assert!(
            super::super::testing::Equipped::with(c.clone())
                .command(&someone_elses())
                .is_none()
        );
        assert!(
            advertised_by(&c, &facts()).is_empty(),
            "the five are advertised by the toolbox that will run them"
        );
    }

    /// The space list is config, and config is a fact about the agent: it is
    /// what bounds the agent's reach, so a reload that rebuilt it from settings
    /// could widen or narrow an agent that was already equipped.
    #[test]
    fn the_space_list_survives_a_slice_round_trip() {
        let caps = Capabilities::new(vec![Capability::Memory(MemoryCapability::new(vec![
            "default".into(),
            "team".into(),
        ]))]);
        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let [Capability::Memory(back)] = read.iter().collect::<Vec<_>>()[..] else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.spaces, vec!["default".to_string(), "team".to_string()]);
    }
}
