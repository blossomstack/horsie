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
use crate::sessions::runners::loading::{AgentSpec, Loading};
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

    /// Connect the named servers and leave them for the runtime to build into
    /// the base toolbox.
    ///
    /// The one capability that hands another an ingredient rather than a layer,
    /// and the reason setup runs front-to-back: an MCP tool is gated by the
    /// session's `allowed_tools` exactly like a runtime tool, and that gate is
    /// applied inside [`crate::agent_loop::DefaultToolboxFactory::for_agent`] —
    /// so the connections have to reach it before it builds, rather than
    /// wrapping it afterwards.
    ///
    /// No servers, no connections: a session that names none connects to none.
    ///
    /// Never fatal. A server that will not connect costs the agent some tools,
    /// not its turn — and the ones that failed are carried in `unavailable`, so
    /// a call for one is answered with why rather than "no such tool".
    async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        if self.servers.is_empty() {
            return Ok(());
        }
        let Some(service) = loading.mcp.as_ref() else {
            return Err(SetupError {
                capability: self.name(),
                reason: format!(
                    "this agent names MCP servers ({}) but no MCP service is configured",
                    self.servers.join(", ")
                ),
                fatal: false,
            });
        };
        loading.progress("connecting_tools", None).await;
        spec.mcp = service
            .toolboxes_for(&self.servers)
            .await
            .map_err(|e| SetupError {
                capability: self.name(),
                reason: format!("build MCP toolboxes: {e}"),
                fatal: false,
            })?;
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

    /// No servers named, nothing connected and nothing asked of the service.
    /// If this regresses every session pays for a connection round it never
    /// wanted.
    #[tokio::test]
    async fn no_servers_connects_nothing() {
        let mut spec = spec();
        McpCapability::new(vec![])
            .setup(&loading(), &mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.mcp.is_empty());
    }

    /// A session that names servers with no service behind them loses its MCP
    /// tools and keeps its turn. The whole point of a non-fatal setup error:
    /// the agent still starts, and the caller reports what it started without.
    ///
    /// The counterpart — that the named servers reach the service verbatim —
    /// needs a live MCP service, and lives in `mcp::service`'s own tests where
    /// one is stood up. What is testable here is which way the failure goes.
    #[tokio::test]
    async fn no_service_is_degraded_rather_than_fatal() {
        let mut spec = spec();
        let e = McpCapability::new(vec!["github".into(), "docs".into()])
            .setup(&loading(), &mut spec)
            .await
            .expect_err("no service is wired");
        assert_eq!(e.capability, "mcp");
        assert!(
            !e.fatal,
            "a server that will not connect costs tools, not the turn"
        );
        assert!(e.reason.contains("github") && e.reason.contains("docs"));
        assert!(spec.mcp.is_empty());
    }
}
