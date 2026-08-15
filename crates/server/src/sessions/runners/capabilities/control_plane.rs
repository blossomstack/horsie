//! `horsie_*`: managing this server from inside a conversation.
//!
//! Granted, never acquired. Only a conversation's main agent is given one — a
//! subagent, a workflow step and a fork all inherit the session's settings, and
//! authority over the server is not something those should carry along with a
//! model name. Holding it as a capability rather than a flag on settings is
//! what makes that true by construction: an agent that was never given the
//! capability has no `horsie_*` layer and nothing to claim its calls, so there
//! is no inherited flag left to read the wrong way.

use super::{CapSlice, Capability, Decision, SetupError, or_empty};
use crate::control::toolbox::ControlToolbox;
use crate::sessions::runners::loading::{AgentSpec, Loading};
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

    /// Wrap the `horsie_*` tools on, and render the index of what they reach.
    ///
    /// The index is built from the same [`ControlToolbox`] the layer will be —
    /// the tools an agent is told about and the tools it has are one list, read
    /// off one object, so a resource added to `crate::control` cannot appear in
    /// one and not the other.
    async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        let Some(services) = loading.services.clone() else {
            return Err(SetupError {
                capability: self.name(),
                reason: "this agent asks for the control plane but no services are wired"
                    .to_string(),
                fatal: false,
            });
        };
        // Over nothing, and thrown away: `command_index` reads the operation
        // table, which is the same whatever the layer ends up wrapping, and the
        // index has to be in the prompt before the inner toolbox exists.
        let index = ControlToolbox::new(
            or_empty(None),
            services.clone(),
            crate::control::operations(),
        )
        .command_index();
        spec.say(
            "control_plane",
            format!(
                "## Managing this horsie server\n\n\
                 You can manage this server through the `horsie_*` tools: {index}\n\n\
                 Call a resource's tool with an `action`. Changes take effect \
                 immediately and are not confirmed with the user first, so read before \
                 you write when you are unsure which row you mean."
            ),
        );
        spec.wrap(move |inner, _| {
            Arc::new(ControlToolbox::new(
                or_empty(inner),
                services,
                crate::control::operations(),
            ))
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

    /// Holding the capability is what equips the tools and the prompt that
    /// tells the agent they exist — a tool nobody was told about is not used.
    #[tokio::test]
    async fn it_equips_the_tools_and_a_prompt_section() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let mut loading = loading();
        loading.services = Some(state.services().await);

        let mut spec = spec();
        ControlPlaneCapability
            .setup(&loading, &mut spec)
            .await
            .expect("nothing fatal");
        let section = spec
            .prompt
            .iter()
            .find(|p| p.key == "control_plane")
            .expect("the agent is told what it can manage")
            .body
            .clone();
        let names = equipped(spec);
        assert!(
            names.iter().any(|n| n.starts_with(PREFIX)),
            "no control tool was equipped: {names:?}"
        );
        // The index names every resource the tools reach: told about and
        // equipped with are one list, read off one object.
        for tool in names.iter().filter(|n| n.starts_with(PREFIX)) {
            let resource = tool.trim_start_matches(PREFIX);
            assert!(
                section.contains(resource),
                "{resource} is equipped but not in the index: {section}",
            );
        }
    }

    /// No services, no tools — and the turn goes on. An agent that cannot reach
    /// the control plane is degraded, not broken.
    #[tokio::test]
    async fn no_services_is_degraded_rather_than_fatal() {
        let mut spec = spec();
        let e = ControlPlaneCapability
            .setup(&loading(), &mut spec)
            .await
            .expect_err("nothing is wired");
        assert!(!e.fatal);
        assert!(spec.prompt.is_empty());
        assert!(spec.toolbox().is_none());
    }
}
