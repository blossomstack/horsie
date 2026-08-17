//! The MCP servers an agent is connected to.
//!
//! Which servers an agent gets is a per-agent decision, so the names live on
//! the capability rather than being read back out of settings when the turn is
//! assembled. Setup-only: it acquires the connections and puts them in the
//! spec, and everything after that is somebody else's.
//!
//! # It equips an ingredient, not a layer
//!
//! The one capability that hands another one a part rather than wrapping it,
//! and the reason [`Capabilities::equip`](super::Capabilities::equip) folds
//! front-to-back: an MCP tool is gated by the session's `allowed_tools` exactly
//! like a runtime tool, and that gate is applied inside
//! [`crate::agent_loop::DefaultToolboxFactory::for_agent`] — so the connections
//! have to reach it *before* it builds the base toolbox, rather than wrapping
//! it afterwards. That ordering is a property of the capability list, and this
//! capability has to sort ahead of `runtime` in it.
//!
//! # Why it claims no tool name
//!
//! Its session-side twin claimed the `mcp__` prefix — the ids an MCP server
//! contributes are namespaced `mcp__<server>__<tool>` — purely so a call could
//! not fall through to the open-namespace capability behind it and be journaled
//! as something else. Here that is not a claim worth making: an MCP call is
//! answered by the toolbox built out of [`AgentSpec::mcp`], which runs on the
//! agent's task, so routing it to this mailbox would only park a call that has
//! nothing to journal and nobody to answer it. So [`Capability::handle`]
//! returns `None` and [`Capability::tools`] stays empty, and the tools are
//! advertised by the toolbox that will actually run them.

use super::{CapEvent, CapSlice, Capability, Decision, Msg, SetupError};
use crate::sessions::runners::loading::{AgentSpec, Loading};
use serde::{Deserialize, Serialize};

/// The MCP servers this agent is connected to.
///
/// `Default` is the agent that names none, which connects to nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpCapability {
    /// The servers this agent is connected to, by name.
    pub servers: Vec<String>,
}

/// Uninhabited: an MCP call's effect is on the far side of the server, so there
/// is no fact about this agent to record and no arm for [`Capability::apply`]
/// to fold.
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
    /// the base toolbox. See the module doc for why this must run first.
    ///
    /// No servers, no connections: an agent that names none connects to none.
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

    /// Nothing reaches this capability through the mailbox; see the module doc.
    fn handle(&self, _msg: &Msg) -> Option<Decision> {
        None
    }

    fn apply(&mut self, event: &CapEvent) {
        // `let ... else` rather than a match with an arm per sibling: every
        // capability is offered every event.
        let CapEvent::Mcp(event) = event else {
            return;
        };
        // And [`Event`] is uninhabited, so there is provably nothing to fold.
        match *event {}
    }

    fn save(&self) -> CapSlice {
        CapSlice::Mcp(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::call;
    use super::*;
    use crate::agent_loop::capabilities::Capabilities;
    use crate::agent_loop::capabilities::testing::{loading, spec};

    /// No servers named, nothing connected and nothing asked of the service.
    /// If this regresses every agent pays for a connection round it never
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

    /// An agent that names servers with no service behind them loses its MCP
    /// tools and keeps its turn. The whole point of a non-fatal setup error:
    /// the agent still starts, and the caller reports what it starts without.
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

    /// What the move changed, pinned. The session-side twin claimed the `mcp__`
    /// prefix; here an MCP call belongs to the toolbox built from
    /// [`AgentSpec::mcp`], and claiming it at the mailbox would route a call to
    /// a capability with nothing to journal and no way to answer it.
    #[test]
    fn it_claims_no_tool_name_through_the_mailbox() {
        let c = McpCapability::new(vec!["github".into()]);
        assert!(
            c.handle(&Msg::Tool(&call("mcp__github__list_issues")))
                .is_none()
        );
        assert!(c.handle(&Msg::Tool(&call("bash"))).is_none());
        assert!(
            c.tools().is_empty(),
            "the tools are advertised by the toolbox that will run them"
        );
    }

    /// The server list is config, and config is a fact about the agent: a
    /// reload that rebuilt this from settings would reconnect a different set
    /// than the one the agent was equipped with.
    #[test]
    fn the_server_list_survives_a_slice_round_trip() {
        let caps = Capabilities::new(vec![Box::new(McpCapability::new(vec![
            "github".into(),
            "docs".into(),
        ]))]);
        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::Mcp(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.servers, vec!["github".to_string(), "docs".to_string()]);
    }
}
