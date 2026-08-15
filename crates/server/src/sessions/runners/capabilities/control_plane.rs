//! `horsie_*`: managing this server from inside a conversation.
//!
//! Granted, never acquired. Only a conversation's main agent is given one — a
//! subagent, a workflow step and a fork all inherit the session's settings, and
//! authority over the server is not something those should carry along with a
//! model name. Holding it as a capability rather than a flag on settings is
//! what makes that true by construction: an agent that was never given the
//! capability has no `horsie_*` layer and nothing to claim its calls, so there
//! is no inherited flag left to read the wrong way.

use super::{CapSlice, Capability, Decision, SetupError};
use crate::sessions::runners::action::{AgentSpec, PromptSection, ToolLayer};
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};

/// The namespace every control-plane tool id starts with. The underscore is
/// part of it: `horsie` alone is somebody else's tool.
pub const PREFIX: &str = "horsie_";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlPlaneCapability;

/// Uninhabited: a control-plane call's effect is the row it wrote, which the
/// server's own tables already record, so there is no arm to fold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {}

#[async_trait::async_trait]
impl Capability for ControlPlaneCapability {
    fn name(&self) -> &'static str {
        "control_plane"
    }

    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError> {
        spec.layers.push(ToolLayer::ControlPlane);
        spec.prompt.push(PromptSection {
            key: "control_plane",
            body: "You can manage this horsie server through the `horsie_*` \
                   tools. Call a resource's tool with an `action`. Changes take \
                   effect immediately and are not confirmed with the user \
                   first, so read before you write when you are unsure which \
                   row you mean."
                .to_string(),
        });
        Ok(())
    }

    /// Claims the `horsie_` namespace, journaling nothing.
    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        let Message::Tool(t) = msg else { return None };
        t.name.starts_with(PREFIX).then(Decision::default)
    }

    fn save(&self) -> CapSlice {
        CapSlice::ControlPlane(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;

    /// The namespace is claimed by name. If this regresses a server-management
    /// call falls through to the runtime capability, which takes anything.
    #[test]
    fn it_claims_the_horsie_namespace() {
        let c = ControlPlaneCapability;
        assert!(
            c.handle(caller(), &tool("horsie_sessions", serde_json::json!({})))
                .is_some()
        );
    }

    /// The prefix includes the underscore, so a tool named exactly `horsie` is
    /// not ours. A boundary a reader can get wrong is a boundary worth a test.
    #[test]
    fn the_bare_name_is_not_mine() {
        let c = ControlPlaneCapability;
        assert!(
            c.handle(caller(), &tool("horsie", serde_json::json!({})))
                .is_none()
        );
    }

    /// Holding the capability is what equips the layer and the prompt that
    /// tells the agent it exists — a tool nobody was told about is not used.
    #[tokio::test]
    async fn it_equips_the_layer_and_a_prompt_section() {
        let mut spec = AgentSpec::default();
        ControlPlaneCapability
            .setup(&mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.has(&ToolLayer::ControlPlane));
        assert!(spec.prompt.iter().any(|p| p.key == "control_plane"));
    }
}
