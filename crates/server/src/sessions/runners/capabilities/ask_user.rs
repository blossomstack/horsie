//! `ask_user`: the agent stopping to put a question to the person.
//!
//! **This capability holds no state at all.** It is pure config: it decides
//! what to equip, and it answers for the tool name.
//!
//! It used to hold the agent parked on a question, as the addressee an arriving
//! answer would be routed to. That entry could never be written —
//! [`AskUserToolbox`] stops the run rather than sending a command, so a
//! `Tool("ask_user")` never reaches this `handle` — and it was never needed
//! either: an answer arrives naming the agent it is for, and the session maps
//! an agent to its runner already. Synthesising a tool call to populate the
//! field would have put a lie in the journal to keep a redundant index alive,
//! so the field went instead, and with it `ask_user::Event`, the `CapEvent`
//! arm, and the third routing mode they needed.
//!
//! A muted agent equips no `ask_user` layer at all rather than equipping one
//! that refuses: a tool that is not pushed cannot be called, whereas one that
//! is equipped and refused costs the model a turn to discover that. The call is
//! still *claimed* here, though — a plugin or a resumed transcript can put one
//! in front of us anyway, and declining it would hand it to the open-namespace
//! runtime capability behind us, which claims every name. The model would then
//! be answered by the sandbox instead of being told why it may not ask.

use super::{CapSlice, Capability, Decision, SetupError, or_empty};
use crate::sessions::ask_tool::AskUserToolbox;
use crate::sessions::runners::loading::{AgentSpec, Loading};
use crate::sessions::runners::message::{Caller, Message};
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

/// Pure config: one field, and no folded state whatsoever.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AskUserCapability {
    /// `Some` when this agent may not ask, and why. `None` is the ordinary
    /// conversation that can.
    pub mute: Option<Mute>,
}

impl Mute {
    /// What the model is told when it calls a tool it was not given.
    ///
    /// Short, and it names the reason: a refusal the model cannot act on is a
    /// retry loop, and these two are the only reasons there are.
    const fn refusal(self) -> &'static str {
        match self {
            Self::Unattended => {
                "`ask_user` is not available: this session was started by a routine and nobody is \
                 reading it, so a question would park the run for ever. Make the reasonable \
                 choice, say which you made and why, and carry on."
            }
            Self::NotInteractive => {
                "`ask_user` is not available in this step, which did not declare itself \
                 interactive. Work from the input you were given, make the reasonable choice, say \
                 which you made and why, and carry on."
            }
        }
    }
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
            mute: Some(Mute::Unattended),
        }
    }

    /// A workflow step that did not declare itself interactive.
    #[must_use]
    pub fn not_interactive() -> Self {
        Self {
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

    /// Claims `ask_user`, and journals nothing whichever way it answers.
    ///
    /// A muted agent is told why, rather than declined. Declining hands the
    /// call to the next capability, and the last one is the open-namespace
    /// runtime that claims every name — so the model would be answered by the
    /// sandbox and never told it may not ask.
    ///
    /// An unmuted one is claimed and nothing more. Parking is the agent's own
    /// doing, and an arriving answer names its agent rather than being routed
    /// to a pending entry here, so there is no fact left for this to record. In
    /// practice the call does not arrive at all — [`AskUserToolbox`] stops the
    /// run instead of sending a command — and claiming the name is what stops
    /// the sandbox layer from taking it if it ever does.
    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        match msg {
            Message::Tool(t) if t.name == TOOL => Some(match self.mute {
                Some(mute) => Decision::reply(mute.refusal()),
                None => Decision::default(),
            }),
            Message::Tool(_) | Message::Command(_) | Message::Child(_) => None,
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
    use crate::sessions::runners::action::Action;
    use crate::sessions::runners::capabilities::Capabilities;
    use crate::sessions::runners::capabilities::runtime::RuntimeCapability;

    fn ask() -> Message {
        tool(TOOL, serde_json::json!({"questions": []}))
    }

    fn refusal(d: &Decision) -> String {
        assert!(
            d.events.is_empty(),
            "a refusal is not a fact about the session"
        );
        let [Action::Reply { text }] = d.actions.as_slice() else {
            panic!("expected one reply, got {:?}", d.actions);
        };
        text.clone()
    }

    /// The one that matters for routines: an unattended run is offered no
    /// `ask_user` tool, because the answer that would free the agent is never
    /// coming.
    #[tokio::test]
    async fn unattended_equips_nothing_and_refuses_the_call() {
        let c = AskUserCapability::unattended();
        let mut spec = spec();
        c.setup(&loading(), &mut spec)
            .await
            .expect("nothing to acquire");
        // Told why, rather than left to discover a tool it was told about is
        // missing.
        assert_eq!(
            spec.prompt.iter().map(|s| s.key).collect::<Vec<_>>(),
            vec!["unattended"]
        );
        assert!(spec.toolbox().is_none());

        // And a call that arrives anyway is refused in words rather than
        // declined — see the test below for what declining would cost.
        let said = refusal(&c.handle(caller(), &ask()).expect("mine even when muted"));
        assert!(said.contains("routine"), "{said}");
    }

    /// **A muted agent that asks anyway must be told no.** Declining the call
    /// hands it to the next capability, and the last one is the open-namespace
    /// runtime that claims every name — so the model would be answered by the
    /// sandbox and never learn why its question went nowhere.
    #[test]
    fn a_muted_ask_is_claimed_rather_than_left_to_the_sandbox() {
        for c in [
            AskUserCapability::unattended(),
            AskUserCapability::not_interactive(),
        ] {
            let caps = Capabilities::new(vec![Box::new(c), Box::new(RuntimeCapability::default())]);
            let taker = caps
                .iter()
                .find_map(|c| c.handle(caller(), &ask()).map(|d| (c.name(), d)));
            let Some(("ask_user", d)) = taker else {
                panic!("the sandbox layer swallowed the question: {taker:?}");
            };
            assert!(!refusal(&d).is_empty());
        }
    }

    /// An agent that *may* ask has its call claimed and nothing recorded:
    /// parking is the agent's own doing, and an arriving answer names the agent
    /// it is for rather than being routed to a pending entry here.
    ///
    /// In practice this call never arrives — `AskUserToolbox` stops the run
    /// instead of sending a command, which is exactly why the pending entry
    /// this capability used to hold could never be written.
    #[test]
    fn an_attended_ask_is_claimed_and_journals_nothing() {
        let d = AskUserCapability::new()
            .handle(caller(), &ask())
            .expect("mine");
        assert!(d.events.is_empty());
        assert!(d.actions.is_empty());
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
        // Still equips nothing, exactly as unattended does — and the refusal
        // says the other reason, not this one.
        assert!(spec.toolbox().is_none());
        let said = refusal(&c.handle(caller(), &ask()).expect("mine even when muted"));
        assert!(
            said.contains("step") && !said.contains("routine"),
            "a muted step was refused as though it were a routine: {said}"
        );
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
