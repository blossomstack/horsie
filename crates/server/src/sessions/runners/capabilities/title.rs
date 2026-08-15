//! `set_session_title`: the agent naming the conversation it is in.
//!
//! A conversation's own capability. A fork holds one too and names *itself*,
//! not the session it lives in — the model should not have to know which kind
//! of conversation it is in to name it.

use super::{CapEvent, CapSlice, Capability, Decision, SetupError, or_empty};
use crate::sessions::runners::ids::RunnerId;
use crate::sessions::runners::loading::{AgentSpec, Loading};
use crate::sessions::runners::message::{Caller, Message};
use crate::sessions::title_tool::{SessionTitleToolbox, normalize_session_title};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Appended to a fork's system prompt.
///
/// A fork is a conversation, so almost nothing a subagent is told applies: it
/// can ask the user, and it owes nobody a report. What it does need is to know
/// it is one of several under one session sharing one workspace, and that its
/// title is how a person tells them apart — which is why this paragraph is
/// here, with the tool it tells the fork to call, rather than with
/// [`super::fork`], which answers for *making* a fork.
const FORK_PROMPT_SUFFIX: &str = "# Forked conversation\n\
You are a fork: a conversation branched from another one in this session, carrying its \
history up to the branch point. You share one workspace with it — what you change on disk \
is what it sees. Name yourself with set_session_title as soon as the new direction is \
clear; that title is how a person tells this conversation from the one it came from.";

/// The tool this capability answers for.
pub const TOOL: &str = "set_session_title";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleCapability {
    /// The name recorded so far, if any.
    pub title: Option<String>,
    /// The fork this names, if it names one. `None` titles the session.
    pub fork: Option<RunnerId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Set { name: String },
}

/// The tool's arguments. Deserialised here so the schema and this type are one
/// declaration rather than two that can drift.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub title: String,
}

impl TitleCapability {
    #[must_use]
    pub fn for_fork(fork: RunnerId) -> Self {
        Self {
            title: None,
            fork: Some(fork),
        }
    }
}

#[async_trait::async_trait]
impl Capability for TitleCapability {
    fn name(&self) -> &'static str {
        "title"
    }

    /// One tool, two targets. A fork names *itself*; every other conversation
    /// names the session — the model never has to know which it is in.
    async fn setup(&self, _loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        let session = _loading.session.clone();
        // The target says what is renamed; this says who asked, which is the
        // only thing the session can route a reply on.
        let agent = _loading.agent;
        match self.fork {
            Some(fork) => {
                spec.wrap(move |inner, _| {
                    Arc::new(SessionTitleToolbox::for_fork(
                        or_empty(inner),
                        session,
                        agent,
                        fork.as_uuid(),
                    ))
                });
                // What a fork is, and why naming itself matters. Its own
                // paragraph rather than the generic one below: a fork already
                // knows it should name itself, what it needs told is that the
                // conversation it branched from is still there beside it.
                spec.say("fork_role", FORK_PROMPT_SUFFIX);
            }
            None => {
                spec.wrap(move |inner, _| {
                    Arc::new(SessionTitleToolbox::new(or_empty(inner), session, agent))
                });
                spec.say(
                    "title",
                    "Name this conversation with `set_session_title` once you \
                     know what it is about.",
                );
            }
        }
        Ok(())
    }

    /// Validation lives here, with the capability that owns the tool, rather
    /// than in the command handler that used to receive it: a name the session
    /// would refuse must not reach the log as one it accepted.
    ///
    /// The reply is the *normalized* title and nothing else.
    /// [`SessionTitleToolbox`] wraps whatever comes back in
    /// `Session title set to "…"`, so a sentence here would reach the model
    /// doubled.
    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        let Message::Tool(t) = msg else { return None };
        if t.name != TOOL {
            return None;
        }
        let req: Request = match super::parse(&t.name, &t.input) {
            Ok(req) => req,
            Err(refusal) => return Some(refusal),
        };
        let name = match normalize_session_title(&req.title) {
            Ok(name) => name,
            // A refusal journals nothing: the session was never renamed.
            Err(error) => return Some(Decision::reply(error.to_string())),
        };
        Some(Decision {
            events: vec![CapEvent::Title(Event::Set { name: name.clone() })],
            actions: vec![crate::sessions::runners::action::Action::Reply { text: name }],
        })
    }

    fn apply(&mut self, event: &CapEvent) {
        if let CapEvent::Title(Event::Set { name }) = event {
            self.title = Some(name.clone());
        }
    }

    fn save(&self) -> CapSlice {
        CapSlice::Title(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn setting_a_title_records_it() {
        let mut c = TitleCapability::default();
        let d = c
            .handle(
                caller(),
                &tool(TOOL, serde_json::json!({"title": "the flake"})),
            )
            .expect("mine");
        c.apply(&d.events[0]);
        assert_eq!(c.title.as_deref(), Some("the flake"));
    }

    /// The reply is the bare title, not a sentence about it: the toolbox
    /// renders `Session title set to "…"` around whatever comes back, so prose
    /// here reaches the model doubled. Normalization happens here too, so what
    /// is journaled and what is answered are the same string.
    #[test]
    fn the_reply_is_the_normalized_title_alone() {
        let d = TitleCapability::default()
            .handle(
                caller(),
                &tool(TOOL, serde_json::json!({"title": "  Fix café login ☕  "})),
            )
            .expect("mine");
        let CapEvent::Title(Event::Set { name }) = &d.events[0] else {
            panic!("expected a set, got {:?}", d.events[0]);
        };
        assert_eq!(name, "Fix café login ☕");
        let crate::sessions::runners::action::Action::Reply { text } = &d.actions[0] else {
            panic!("expected a reply, got {:?}", d.actions[0]);
        };
        assert_eq!(text, "Fix café login ☕");
    }

    /// A title the session would refuse is refused here, and journals nothing:
    /// a name that never took must not replay as one that did.
    #[test]
    fn an_invalid_title_is_refused_and_records_nothing() {
        for bad in ["   ", "one\ntwo"] {
            let d = TitleCapability::default()
                .handle(caller(), &tool(TOOL, serde_json::json!({ "title": bad })))
                .expect("mine");
            assert!(d.events.is_empty(), "{bad:?} was journaled");
            let crate::sessions::runners::action::Action::Reply { text } = &d.actions[0] else {
                panic!("expected a reply, got {:?}", d.actions[0]);
            };
            assert!(text.contains("session title must"), "{text}");
        }
    }

    /// Both variants equip the one tool: the model calls `set_session_title`
    /// whichever kind of conversation it is in, and the capability is what
    /// decides whether that renames the session or the fork.
    #[tokio::test]
    async fn either_variant_equips_the_tool() {
        for cap in [
            TitleCapability::default(),
            TitleCapability::for_fork(RunnerId::new_v4()),
        ] {
            let mut spec = spec();
            cap.setup(&loading(), &mut spec)
                .await
                .expect("nothing to acquire");
            assert_eq!(equipped(spec), vec![TOOL]);
        }
    }

    /// A fork is told it is one, and told it by the capability that owns the
    /// tool the paragraph tells it to call. The plain conversation gets the
    /// plain nudge instead — telling every conversation it is a branch of
    /// something is how a model starts apologising for a fork that never
    /// happened.
    #[tokio::test]
    async fn only_a_fork_is_told_it_is_one() {
        let mut forked = spec();
        TitleCapability::for_fork(RunnerId::new_v4())
            .setup(&loading(), &mut forked)
            .await
            .expect("nothing to acquire");
        let keys: Vec<&str> = forked.prompt.iter().map(|s| s.key).collect();
        assert_eq!(keys, vec!["fork_role"]);
        assert!(forked.prompt[0].body.starts_with("# Forked conversation"));

        let mut plain = spec();
        TitleCapability::default()
            .setup(&loading(), &mut plain)
            .await
            .expect("nothing to acquire");
        let keys: Vec<&str> = plain.prompt.iter().map(|s| s.key).collect();
        assert_eq!(keys, vec!["title"]);
    }

    #[test]
    fn another_tool_is_not_mine() {
        assert!(
            TitleCapability::default()
                .handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
    }
}
