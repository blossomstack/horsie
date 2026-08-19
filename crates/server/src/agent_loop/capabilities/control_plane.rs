//! `horsie_*`: managing this server from inside a conversation.
//!
//! Granted, never acquired. Only a conversation's main agent is given one — a
//! subagent, a workflow step and a fork all inherit the session's settings, and
//! authority over the server is not something those should carry along with a
//! model name. Holding it as a capability rather than a flag on settings is
//! what makes that true by construction: an agent that was never given the
//! capability has no `horsie_*` layer and nothing to claim its calls, so there
//! is no inherited flag left to read the wrong way.
//!
//! # Setup only, and nothing through the mailbox
//!
//! A control-plane call reaches the server's own tables and comes straight back
//! with a value, so it wants none of what this actor's mailbox is for: no park,
//! no journal entry, no request to the session. The whole capability is
//! therefore the layer [`super::Capability::setup`] pushes, which runs on the agent's
//! own task — [`super::Capability::layer`] claims nothing and so does
//! [`super::Capability::handle`].
//!
//! That is the one difference from its session-side twin, which had to claim
//! the `horsie_` prefix to stop the open-namespace capability behind it from
//! swallowing the call. Here the call never becomes a message at all: the layer
//! answers it before the actor is involved.

use super::{Decision, Msg, SetupError};
use crate::agent_loop::capabilities::or_empty;
use crate::control::toolbox::ControlToolbox;
use crate::sessions::runners::loading::{AgentSpec, Loading};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The namespace every control-plane tool id starts with. The underscore is
/// part of it: `horsie` alone is somebody else's tool.
pub const PREFIX: &str = "horsie_";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlPlaneCapability;

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl ControlPlaneCapability {
    pub fn name(&self) -> &'static str {
        "control_plane"
    }

    /// Wrap the `horsie_*` tools on, and render the index of what they reach.
    ///
    /// The index is built from the same [`ControlToolbox`] the layer will be —
    /// the tools an agent is told about and the tools it has are one list, read
    /// off one object, so a resource added to `crate::control` cannot appear in
    /// one and not the other.
    pub async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
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

    /// Nothing. The layer answers every `horsie_*` call on the agent's own
    /// task, so none of them reaches this mailbox — see the module doc.
    pub fn handle(&self, _msg: &Msg) -> Option<Decision> {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::Capability;
    use crate::agent_loop::capabilities::testing::{
        advertised_by, equipped, facts, loading, someone_elses, spec,
    };

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

    /// The whole capability is its layer: it advertises nothing through the
    /// mailbox and claims nothing. A `horsie_*` name advertised here would be
    /// dispatched to `handle`, which answers nothing — the model would be left
    /// waiting on a tool call that never returns.
    #[test]
    fn it_claims_nothing_through_the_mailbox() {
        let c = Capability::ControlPlane(ControlPlaneCapability);
        assert!(advertised_by(&c, &facts()).is_empty());
        assert!(
            super::super::testing::Equipped::with(c.clone())
                .command(&someone_elses())
                .is_none(),
            "a capability with no commands claimed one"
        );
    }
}
