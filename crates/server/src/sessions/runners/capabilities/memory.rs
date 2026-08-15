//! `memory_*`: the spaces an agent can read and write between sessions.
//!
//! Which spaces an agent reaches is a per-agent decision, so the names live on
//! the capability and travel into the layer verbatim; the toolbox refuses a
//! space that is not in that list, so an agent's reach is decided once, here.
//!
//! Unlike MCP and the control plane this is a fixed set of tool names rather
//! than a prefix — [`crate::memory::MemoryToolbox`] advertises exactly five,
//! and matching what the code names means a sixth cannot start being claimed
//! by a prefix before anything exists to answer it.

use super::{CapSlice, Capability, Decision, SetupError, or_empty};
use crate::sessions::runners::loading::{AgentSpec, Loading};
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The tools this capability answers for, as named by the memory toolbox.
pub const TOOLS: [&str; 5] = [
    "memory_load",
    "memory_create",
    "memory_update",
    "memory_delete",
    "memory_list",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCapability {
    /// The spaces this agent can reach, by name.
    pub spaces: Vec<String>,
}

/// Uninhabited: a memory write lands in the memory store, which is the record,
/// so there is no session-level fact to journal and no arm to fold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {}

impl MemoryCapability {
    #[must_use]
    pub fn new(spaces: Vec<String>) -> Self {
        Self { spaces }
    }
}

#[async_trait::async_trait]
impl Capability for MemoryCapability {
    fn name(&self) -> &'static str {
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
    async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
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

    /// Claims the five memory tools, journaling nothing.
    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        let Message::Tool(t) = msg else { return None };
        TOOLS.contains(&t.name.as_str()).then(Decision::default)
    }

    fn save(&self) -> CapSlice {
        CapSlice::Memory(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;

    /// Every tool the memory toolbox advertises is claimed here. If one is
    /// missed it falls through to the runtime capability, which takes anything
    /// and journals nothing, so the miss is silent.
    #[test]
    fn it_claims_every_memory_tool() {
        let c = MemoryCapability::new(vec!["default".into()]);
        for name in TOOLS {
            assert!(
                c.handle(caller(), &tool(name, serde_json::json!({})))
                    .is_some(),
                "{name} was not claimed"
            );
        }
    }

    /// And nothing else: a capability that swallowed `bash` would take a
    /// sandbox call the runtime layer is the one equipped to run.
    #[test]
    fn another_tool_is_not_mine() {
        let c = MemoryCapability::new(vec!["default".into()]);
        assert!(
            c.handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
    }

    /// No spaces named, no layer equipped. If this regresses every session
    /// gets memory tools whose every call is refused.
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

    /// A session that asks for memory with no service behind it loses the tools
    /// and keeps its turn — the same call MCP makes, for the same reason.
    #[tokio::test]
    async fn no_service_is_degraded_rather_than_fatal() {
        let mut spec = spec();
        let e = MemoryCapability::new(vec!["default".into()])
            .setup(&loading(), &mut spec)
            .await
            .expect_err("no service is wired");
        assert!(!e.fatal);
        assert!(spec.toolbox().is_none());
    }
}
