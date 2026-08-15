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

use super::{CapEvent, CapSlice, Capability, Decision, SetupError, or_empty};
use crate::sessions::ask_tool::AskUserToolbox;
use crate::sessions::runners::ids::AgentId;
use crate::sessions::runners::loading::{AgentSpec, Loading};
use crate::sessions::runners::message::{AskMsg, Caller, Message};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Appended to an unattended run's system prompt (a routine). It has no
/// `ask_user` tool, so the prompt says why rather than leaving the model to
/// discover that a tool it was told about is missing.
const UNATTENDED_PROMPT_SUFFIX: &str = "# Unattended run\n\
This session was started by a routine, not by a person, and nobody is reading it while \
it runs. There is no ask_user tool: a question would park the run with nobody to answer \
it. Work from the instructions you were given — where they leave a choice open, make the \
reasonable one, say which you made and why, and carry on. Your final message is the \
report; make it self-contained.";

/// Appended for a step that did not declare itself interactive. Deliberately
/// says nothing about whether anyone is watching, because somebody usually is:
/// this step simply is not the one that asks.
const NOT_INTERACTIVE_PROMPT_SUFFIX: &str = "# No questions in this step\n\
This step has no ask_user tool. Work from the input you were given — where it leaves a \
choice open, make the reasonable one, say which you made and why, and carry on. Your \
result is what the rest of the run sees; make it self-contained.";

/// The tool this capability answers for.
pub const TOOL: &str = "ask_user";

/// Why this agent may not ask, when it may not.
///
/// Two reasons, not one flag, because they are different facts and the model is
/// told which. Collapsing them once put "this session was started by a routine"
/// in front of a step running in a session somebody was sitting and watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mute {
    /// A routine's run. Nobody is reading it, so a question parks it against
    /// nobody.
    Unattended,
    /// A workflow step that did not declare itself interactive. Says nothing
    /// about whether anyone is watching — usually somebody is.
    NotInteractive,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AskUserCapability {
    /// The agent waiting on an answer, if one is. Also the addressee an
    /// arriving answer is routed to.
    pub pending: Option<AgentId>,
    /// `Some` when this agent may not ask, and why. `None` is the ordinary
    /// conversation that can.
    pub mute: Option<Mute>,
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

    /// A routine's run: nobody is reading it.
    #[must_use]
    pub fn unattended() -> Self {
        Self {
            pending: None,
            mute: Some(Mute::Unattended),
        }
    }

    /// A workflow step that did not declare itself interactive.
    #[must_use]
    pub fn not_interactive() -> Self {
        Self {
            pending: None,
            mute: Some(Mute::NotInteractive),
        }
    }
}

#[async_trait::async_trait]
impl Capability for AskUserCapability {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    async fn setup(&self, _loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        // Skip the layer entirely when unattended: a tool that is never
        // equipped cannot be called, whereas a tool that is equipped and
        // refused still costs the model a turn to discover that. The paragraph
        // goes in its place — a model that was told the tool exists and finds
        // it missing spends a turn working out why.
        match self.mute {
            Some(Mute::Unattended) => spec.say("unattended", UNATTENDED_PROMPT_SUFFIX),
            Some(Mute::NotInteractive) => {
                spec.say("not_interactive", NOT_INTERACTIVE_PROMPT_SUFFIX);
            }
            None => spec.wrap(|inner, _| Arc::new(AskUserToolbox::new(or_empty(inner)))),
        }
        Ok(())
    }

    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision> {
        // Unattended declines by both routes, not just by not equipping the
        // layer: a plugin or a resumed transcript could still put the call in
        // front of us, and taking it would park an agent nobody can free.
        if self.mute.is_some() {
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
        let mut spec = spec();
        c.setup(&loading(), &mut spec)
            .await
            .expect("nothing to acquire");
        assert!(
            c.handle(caller(), &tool(TOOL, serde_json::json!({"questions": []})))
                .is_none()
        );
        // Told why, rather than left to discover a tool it was told about is
        // missing.
        assert_eq!(
            spec.prompt.iter().map(|s| s.key).collect::<Vec<_>>(),
            vec!["unattended"]
        );
        assert!(spec.toolbox().is_none());
    }

    /// A step that did not declare itself interactive is muted for a different
    /// reason than a routine is, and the model is told which. One flag for both
    /// told a step in a session somebody was watching that "this session was
    /// started by a routine, and nobody is reading it" — plainly false.
    #[tokio::test]
    async fn a_non_interactive_step_is_not_told_it_is_unattended() {
        let c = AskUserCapability::not_interactive();
        let mut spec = spec();
        c.setup(&loading(), &mut spec)
            .await
            .expect("nothing to acquire");

        assert_eq!(
            spec.prompt.iter().map(|s| s.key).collect::<Vec<_>>(),
            vec!["not_interactive"]
        );
        let said = &spec.prompt[0].body;
        assert!(
            !said.contains("routine") && !said.contains("nobody is reading"),
            "a muted step was told nobody is watching: {said}"
        );
        // Still equips nothing and still declines, exactly as unattended does —
        // the reason differs, the refusal does not.
        assert!(spec.toolbox().is_none());
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

    /// An attended session does equip the tool — the counterpart that stops
    /// the unattended test passing for the wrong reason.
    #[tokio::test]
    async fn an_attended_session_equips_the_tool() {
        let mut spec = spec();
        AskUserCapability::new()
            .setup(&loading(), &mut spec)
            .await
            .expect("nothing to acquire");
        assert_eq!(equipped(spec), vec![TOOL]);
    }
}
