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

use super::{CapSlice, Capability, Decision, SetupError};
use crate::sessions::runners::action::{AgentSpec, ToolLayer};
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};

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

    /// No spaces, no layer: an agent that names none has nothing to read, and
    /// the tools would only ever refuse.
    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError> {
        if self.spaces.is_empty() {
            return Ok(());
        }
        spec.layers.push(ToolLayer::Memory {
            spaces: self.spaces.clone(),
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
        let mut spec = AgentSpec::default();
        MemoryCapability::new(vec![])
            .setup(&mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.layers.is_empty());
    }

    /// The named spaces reach the layer verbatim, because that list is what
    /// bounds the toolbox at run time.
    #[tokio::test]
    async fn named_spaces_equip_the_layer() {
        let mut spec = AgentSpec::default();
        MemoryCapability::new(vec!["default".into(), "team".into()])
            .setup(&mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.has(&ToolLayer::Memory {
            spaces: vec!["default".into(), "team".into()],
        }));
    }
}
