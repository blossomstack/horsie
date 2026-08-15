//! The MCP servers a session is connected to.
//!
//! Which servers an agent gets is a per-agent decision, so the names live on
//! the capability rather than being read back out of settings when the turn is
//! assembled. A prefix rather than a fixed list of tools: what an MCP server
//! offers is only known once it has been discovered, and the tool ids it
//! contributes are namespaced `mcp__<server>__<tool>` precisely so they cannot
//! collide with runtime tools. Claiming that prefix here is what stops an MCP
//! call falling through to the open-namespace capability behind it, which
//! would take it and journal nothing.

use super::{CapSlice, Capability, Decision, SetupError};
use crate::sessions::runners::action::{AgentSpec, ToolLayer};
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};

/// The namespace every MCP tool id starts with.
pub const PREFIX: &str = "mcp__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCapability {
    /// The servers this agent is connected to, by name.
    pub servers: Vec<String>,
}

/// Uninhabited: an MCP call's effect is on the far side of the server, so
/// there is no session-level fact to record and no arm for
/// [`Capability::apply`] to fold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {}

impl McpCapability {
    #[must_use]
    pub fn new(servers: Vec<String>) -> Self {
        Self { servers }
    }
}

#[async_trait::async_trait]
impl Capability for McpCapability {
    fn name(&self) -> &'static str {
        "mcp"
    }

    /// No servers, no layer: a session that names none connects to none, and
    /// an empty layer would advertise a namespace nothing answers for.
    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError> {
        if self.servers.is_empty() {
            return Ok(());
        }
        spec.layers.push(ToolLayer::Mcp {
            servers: self.servers.clone(),
        });
        Ok(())
    }

    /// Claims the `mcp__` namespace, journaling nothing. The empty decision is
    /// what says "taken, with nothing to record".
    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        let Message::Tool(t) = msg else { return None };
        t.name.starts_with(PREFIX).then(Decision::default)
    }

    fn save(&self) -> CapSlice {
        CapSlice::Mcp(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;

    /// The whole namespace is claimed by name. If this regresses an MCP call
    /// falls through to the runtime capability, which takes anything.
    #[test]
    fn it_claims_the_mcp_namespace() {
        let c = McpCapability::new(vec!["github".into()]);
        assert!(
            c.handle(
                caller(),
                &tool("mcp__github__list_issues", serde_json::json!({}))
            )
            .is_some()
        );
    }

    /// And only that namespace: a capability that swallowed `bash` would take
    /// a sandbox call the runtime layer is the one equipped to run.
    #[test]
    fn another_tool_is_not_mine() {
        let c = McpCapability::new(vec!["github".into()]);
        assert!(
            c.handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
    }

    /// No servers named, no layer equipped. If this regresses every session
    /// gets an MCP layer wrapping nothing.
    #[tokio::test]
    async fn no_servers_equips_no_layer() {
        let mut spec = AgentSpec::default();
        McpCapability::new(vec![])
            .setup(&mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.layers.is_empty());
    }

    /// The named servers reach the layer verbatim, because the layer is what
    /// the context provider connects.
    #[tokio::test]
    async fn named_servers_equip_the_layer() {
        let mut spec = AgentSpec::default();
        McpCapability::new(vec!["github".into(), "docs".into()])
            .setup(&mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.has(&ToolLayer::Mcp {
            servers: vec!["github".into(), "docs".into()],
        }));
    }
}
