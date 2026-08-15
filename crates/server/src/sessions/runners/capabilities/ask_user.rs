//! `ask_user`: the agent stopping to put a question to the person.
//!
//! The state here is only *who is parked*, because that is the one fact the
//! session cannot recover by looking anywhere else: an answer arrives addressed
//! to whichever capability recorded the ask, and with nothing recorded there is
//! no addressee. Parking the agent is the agent's own doing — this capability
//! neither cancels a run nor resumes one, so there is no second place that can
//! disagree with the runtime about whether a turn is waiting.
//!
//! An unattended session equips no `ask_user` at all rather than equipping it
//! and refusing the call. Offering a tool whose answer will never come is how a
//! routine run ends up parked for ever against nobody, and a layer that is not
//! pushed cannot be called.

use super::{CapEvent, CapSlice, Capability, Decision, SetupError};
use crate::sessions::runners::action::{AgentSpec, ToolLayer};
use crate::sessions::runners::ids::AgentId;
use crate::sessions::runners::message::{AskMsg, Caller, Message};
use serde::{Deserialize, Serialize};

/// The tool this capability answers for.
pub const TOOL: &str = "ask_user";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AskUserCapability {
    /// The agent waiting on an answer, if one is. Also the addressee an
    /// arriving answer is routed to.
    pub pending: Option<AgentId>,
    /// Nobody is there to answer: no `ask_user` layer, and no route to it.
    pub unattended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Asked { agent: AgentId },
    Answered,
}

impl AskUserCapability {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// For a run with no person attached — a routine, a workflow step that did
    /// not declare itself interactive.
    #[must_use]
    pub fn unattended() -> Self {
        Self {
            pending: None,
            unattended: true,
        }
    }
}

#[async_trait::async_trait]
impl Capability for AskUserCapability {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError> {
        // Skip the layer entirely when unattended: a tool that is never
        // equipped cannot be called, whereas a tool that is equipped and
        // refused still costs the model a turn to discover that.
        if !self.unattended {
            spec.layers.push(ToolLayer::AskUser);
        }
        Ok(())
    }

    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision> {
        // Unattended declines by both routes, not just by not equipping the
        // layer: a plugin or a resumed transcript could still put the call in
        // front of us, and taking it would park an agent nobody can free.
        if self.unattended {
            return None;
        }
        match msg {
            Message::Tool(t) if t.name == TOOL => {
                Some(Decision::record(vec![CapEvent::AskUser(Event::Asked {
                    agent: caller.agent,
                })]))
            }
            // An answer with no question is not ours. Returning `None` lets it
            // fall through to the one place that shouts about an unclaimed
            // message, rather than being swallowed as a no-op here.
            Message::Ask(AskMsg::Answered { .. }) if self.pending.is_some() => {
                Some(Decision::record(vec![CapEvent::AskUser(Event::Answered)]))
            }
            Message::Tool(_) | Message::Command(_) | Message::Child(_) | Message::Ask(_) => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        if let CapEvent::AskUser(mine) = event {
            match mine {
                Event::Asked { agent } => self.pending = Some(*agent),
                Event::Answered => self.pending = None,
            }
        }
    }

    fn save(&self) -> CapSlice {
        CapSlice::AskUser(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::sessions::runners::message::Message;

    /// The one that matters for routines: an unattended run must neither be
    /// offered `ask_user` nor be able to reach it, because the answer that
    /// would free the agent is never coming.
    #[tokio::test]
    async fn unattended_equips_nothing_and_declines_the_call() {
        let c = AskUserCapability::unattended();
        let mut spec = AgentSpec::default();
        c.setup(&mut spec).await.expect("nothing to acquire");
        assert!(!spec.has(&ToolLayer::AskUser));
        assert!(
            c.handle(caller(), &tool(TOOL, serde_json::json!({"questions": []})))
                .is_none()
        );
    }

    /// An answer nobody asked for falls through instead of being taken. If this
    /// regresses, a stray answer is silently absorbed by whichever capability
    /// happens to sit first, and the misroute stops being visible.
    #[test]
    fn an_answer_with_nothing_pending_is_not_mine() {
        let c = AskUserCapability::new();
        assert!(
            c.handle(
                caller(),
                &Message::Ask(AskMsg::Answered { answers: vec![] })
            )
            .is_none()
        );
    }

    /// Asking records the addressee and answering clears it. Without the clear,
    /// the capability claims every later answer for an agent that has moved on.
    #[test]
    fn asking_then_answering_round_trips_the_addressee() {
        let mut c = AskUserCapability::new();
        let who = caller();
        let d = c
            .handle(who, &tool(TOOL, serde_json::json!({"questions": []})))
            .expect("mine");
        assert!(
            d.actions.is_empty(),
            "parking is the agent's own doing, so there is nothing to ask the session for"
        );
        c.apply(&d.events[0]);
        assert_eq!(c.pending, Some(who.agent));

        let d = c
            .handle(who, &Message::Ask(AskMsg::Answered { answers: vec![] }))
            .expect("mine, now that one is pending");
        c.apply(&d.events[0]);
        assert_eq!(c.pending, None);
    }

    /// An attended session does equip the layer — the counterpart that stops
    /// the unattended test passing for the wrong reason.
    #[tokio::test]
    async fn an_attended_session_equips_the_layer() {
        let mut spec = AgentSpec::default();
        AskUserCapability::new()
            .setup(&mut spec)
            .await
            .expect("nothing to acquire");
        assert!(spec.has(&ToolLayer::AskUser));
    }
}
