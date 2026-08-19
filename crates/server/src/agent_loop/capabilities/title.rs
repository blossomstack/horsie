//! `set_session_title`: the agent naming the conversation it is in.
//!
//! A conversation's own capability. A fork holds one too and names *itself*,
//! not the session it lives in — the model should not have to know which kind
//! of conversation it is in to name it.
//!
//! # The first capability that genuinely needs the session
//!
//! Everything else moved here because the fact it wanted was the agent's. This
//! one is the other way round: a session's name is the *session's* state, and no
//! agent can write it. So the tool is answered in two halves — the call decides
//! and asks with [`Act::Ask`], and the session's [`SessionReply`] is what
//! finally answers the model, possibly on a later process.
//!
//! That is why the name in flight is journaled rather than held in a field. Two
//! things need it after the asking turn is over: the confirmation the model
//! reads, which quotes the name back, and [`super::Capability::handle`] itself — a
//! reply is *offered* around the capabilities, so the only way this one can
//! recognise an answer to its own request is to have recorded the call it made.
//!
//! # Which conversation gets renamed
//!
//! [`SessionRequest::SetTitle`] names no target, and does not need one: the
//! session knows which of its conversations the asking agent belongs to, and
//! that is the same fact the old `SetTitle` command carried its `agent` field
//! for. [`TitleCapability::fork`] therefore only says *that* this agent is a
//! fork, which is what the prompt below turns on.

use super::{Act, CapCommand, Decision, Mailbox, Msg, SessionReply, SessionRequest, SetupError};
use crate::agent_loop::state::AgentDomainEvent;
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::sessions::runners::ids::RunnerId;
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use horsie_agentcore::{ToolSpec, Toolbox};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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

/// Maximum session title length in Unicode characters.
pub(crate) const SESSION_TITLE_MAX_CHARS: usize = 60;

/// Why a model-supplied title was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionTitleError {
    Empty,
    Multiline,
    TooLong { max: usize },
}

impl std::fmt::Display for SessionTitleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionTitleError::Empty => write!(f, "session title must not be empty"),
            SessionTitleError::Multiline => {
                write!(f, "session title must be a single line")
            }
            SessionTitleError::TooLong { max } => {
                write!(f, "session title must be at most {max} characters")
            }
        }
    }
}

/// Normalize and validate a model-supplied title. This is the authoritative
/// validation; the JSON schema is only model-facing documentation.
///
/// It lives with the capability that owns the tool, and the session applies the
/// same rule to whatever reaches it by another road — a name that never passed
/// through here must not become one a conversation is called.
pub(crate) fn normalize_session_title(input: &str) -> Result<String, SessionTitleError> {
    let title = input.trim();
    if title.is_empty() {
        return Err(SessionTitleError::Empty);
    }
    if title.chars().any(|c| c == '\n' || c == '\r') {
        return Err(SessionTitleError::Multiline);
    }
    if title.chars().count() > SESSION_TITLE_MAX_CHARS {
        return Err(SessionTitleError::TooLong {
            max: SESSION_TITLE_MAX_CHARS,
        });
    }
    Ok(title.to_string())
}

/// Which conversation this agent's `set_session_title` names.
///
/// Config, and the whole of it: what a rename is *waiting on* is
/// [`TitleState`], on [`AgentState`](crate::agent_loop::AgentState).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleCapability {
    /// The fork this names, if it names one. `None` titles the session.
    pub fork: Option<RunnerId>,
}

/// The renames this agent has in flight.
///
/// Fields private to this file: a name asked for and not yet accepted is this
/// capability's business alone, and the two things read out of it — the
/// confirmation text and "is this reply mine?" — are asked by name below.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleState {
    /// Names asked for and not yet answered: `tool_call_id` -> name.
    ///
    /// Folded from this agent's own events, so it survives an offload and a
    /// restart — the session's answer may arrive after the process that asked
    /// is gone.
    #[serde(default)]
    pending: BTreeMap<String, String>,
}

impl TitleState {
    /// The agent asked the session for `name` on this call.
    pub(crate) fn asked(&mut self, call: String, name: String) {
        self.pending.insert(call, name);
    }

    /// The session took it, so the rename is no longer in flight.
    ///
    /// The accepted name is not kept: the session owns what a conversation is
    /// called, and a second copy here would be a second writer of one fact.
    pub(crate) fn set(&mut self, call: &str, _name: String) {
        self.pending.remove(call);
    }

    /// The session would not take it, so the request is no longer in flight.
    pub(crate) fn refused(&mut self, call: &str) {
        self.pending.remove(call);
    }
}

#[cfg(test)]
/// What this state holds, for the tests that assert on it.
///
/// `#[cfg(test)]` because nothing in production reads it: the decisions that
/// need it are in this file and take `&self`. An accessor kept for a caller
/// that does not exist is how a private field stops being private.
impl TitleState {
    /// The renames still waiting on the session.
    #[must_use]
    pub(crate) fn pending(&self) -> &BTreeMap<String, String> {
        &self.pending
    }
}

/// The tool's arguments. Deserialised here so the schema and this type are one
/// declaration rather than two that can drift.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub title: String,
}

/// What the model asked this capability to do.
pub enum Command {
    /// `set_session_title`, with the name still unread: a name the session
    /// would refuse is a refusal the model has to see.
    Rename { input: Value },
}

impl TitleCapability {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn for_fork(fork: RunnerId) -> Self {
        Self { fork: Some(fork) }
    }

    /// The model called `set_session_title`.
    ///
    /// Validation lives here, with the capability that owns the tool, rather
    /// than with whoever receives the request: a name the session would refuse
    /// must not reach the log as one it accepted. What is asked for is the
    /// *normalized* name, so what is journaled, what the session is told and
    /// what the model is finally shown are one string.
    fn asked(&self, call: &str, input: &Value) -> Decision {
        // A capability that owns a tool name owns every call to it, including
        // the malformed ones: declining would hand the call to the next
        // capability, and the last of those claims every name.
        let request: Request = match serde_json::from_value(input.clone()) {
            Ok(request) => request,
            Err(e) => {
                return Decision::reply(
                    call,
                    format!("`{TOOL}` was called with arguments it cannot read: {e}"),
                );
            }
        };
        let name = match normalize_session_title(&request.title) {
            Ok(name) => name,
            // A refusal journals nothing: nothing was renamed.
            Err(error) => return Decision::reply(call, error.to_string()),
        };
        Decision::record(vec![AgentDomainEvent::TitleAsked {
            call: call.to_string(),
            name: name.clone(),
        }])
        .then(Act::Ask(SessionRequest::SetTitle {
            call: call.to_string(),
            title: name,
        }))
    }

    /// The session answered a rename this capability asked for.
    ///
    /// The tool call has been dangling since the asking turn, so both arms end
    /// in [`Act::Answer`] — a refusal the model cannot see is a tool call that
    /// never returns.
    fn answered(state: &TitleState, reply: &SessionReply) -> Decision {
        match reply {
            SessionReply::Done { call } => {
                let name = state.pending.get(call).cloned().unwrap_or_default();
                Decision::record(vec![AgentDomainEvent::TitleSet {
                    call: call.clone(),
                    name: name.clone(),
                }])
                .then(Act::Answer {
                    call: call.clone(),
                    // The sentence the session-owned toolbox used to render
                    // around the bare name it was handed. There is no toolbox
                    // in the way now, so it is written once, here.
                    text: format!("Session title set to \"{name}\"."),
                })
            }
            // Verbatim: the session is the only thing that knows why, and a
            // reason reworded here is a reason the model cannot act on.
            SessionReply::Refused { call, reason } => {
                Decision::record(vec![AgentDomainEvent::TitleRefused { call: call.clone() }]).then(
                    Act::Answer {
                        call: call.clone(),
                        text: reason.clone(),
                    },
                )
            }
        }
    }

    /// Every rename asked for and never answered, asked again.
    ///
    /// `Asked` is journaled before the request goes out, so one still in the
    /// fold may never have reached the session — and the tool call it was made
    /// from has been dangling ever since.
    ///
    /// Nothing is journaled: the [`AgentDomainEvent::TitleAsked`] this reads is
    /// still the only fact.
    fn reloaded(state: &TitleState) -> Option<Decision> {
        if state.pending.is_empty() {
            return None;
        }
        Some(
            state
                .pending
                .iter()
                .fold(Decision::default(), |d, (call, name)| {
                    d.then(Act::Ask(SessionRequest::SetTitle {
                        call: call.clone(),
                        title: name.clone(),
                    }))
                }),
        )
    }
}

impl TitleCapability {
    /// Claimed by this capability's own layer, which dispatches to the mailbox:
    /// answering the call means asking the session and waiting for a reply, and
    /// a layer pushed in `setup` runs on the agent's task where there is
    /// nothing to ask with.
    fn claims(&self) -> Vec<ClaimedTool> {
        vec![ClaimedTool::new(
            ToolSpec {
                name: TOOL.to_string(),
                description: "Rename this session at any point with a concise, specific, \
                single-line title. The latest successful call wins."
                    .to_string(),
                input_schema: json!({
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": SESSION_TITLE_MAX_CHARS,
                        "description": "A concise single-line session title, at most 60 characters. The latest successful call renames the session."
                    }
                }
                }),
            },
            |input, to| CapCommand::Title(Command::Rename { input }, to),
        )]
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl TitleCapability {
    pub fn name(&self) -> &'static str {
        "title"
    }

    /// One tool, two kinds of conversation. A fork names *itself*; every other
    /// conversation names the session — and the model never has to know which
    /// it is in, so the only difference here is which paragraph it is given.
    pub async fn setup(&self, _loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        match self.fork {
            // What a fork is, and why naming itself matters. Its own paragraph
            // rather than the generic one below: a fork already knows it should
            // name itself, what it needs told is that the conversation it
            // branched from is still there beside it.
            Some(_) => spec.say("fork_role", FORK_PROMPT_SUFFIX),
            None => spec.say(
                "title",
                "Name this conversation with `set_session_title` once you \
                 know what it is about.",
            ),
        }
        Ok(())
    }

    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        _facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.claims(), mailbox)
    }

    pub fn command(&self, _state: &TitleState, cmd: &CapCommand) -> Option<Decision> {
        let CapCommand::Title(cmd, to) = cmd else {
            return None;
        };
        let Command::Rename { input } = cmd;
        Some(self.asked(&to.call, input))
    }

    pub fn handle(&self, state: &TitleState, msg: &Msg) -> Option<Decision> {
        match msg {
            // Replies are offered around, so this claims only the ones it can
            // account for: a call it recorded asking about.
            Msg::Reply(reply) if state.pending.contains_key(reply.call()) => {
                Some(Self::answered(state, reply))
            }
            // The crash window, and the cheapest case of it: a rename journaled
            // and never answered is asked again, with the call the model is
            // still parked on and the name already recorded. No dedupe is owed
            // in return — setting a session's title twice to the same string is
            // the same session.
            Msg::Loaded => Self::reloaded(state),
            Msg::Turn(_)
            | Msg::Answer(_)
            | Msg::Child(_)
            | Msg::Woke { .. }
            | Msg::Concluded
            | Msg::Reply(_)
            | Msg::TurnProposed => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::testing::{
        Equipped, advertised_by, answering, equipped, facts, loading, someone_elses, spec, specs_of,
    };
    use crate::agent_loop::capabilities::{Capabilities, Capability};

    /// A conversation that can name itself, with nothing asked yet.
    fn conversation() -> Equipped {
        Equipped::with(Capability::Title(TitleCapability::new()))
    }

    /// Ask for a name, the way the layer that claims `set_session_title`
    /// would.
    fn set(c: &Equipped, id: &str, title: &str) -> Decision {
        renamed(c, id, json!({ "title": title }))
    }

    fn renamed(c: &Equipped, id: &str, input: Value) -> Decision {
        c.command(&CapCommand::Title(Command::Rename { input }, answering(id)))
            .expect("mine")
    }

    /// Ask for this name, the only way there is to get a request in flight.
    fn asked(id: &str, title: &str) -> Equipped {
        let mut c = conversation();
        let d = set(&c, id, title);
        c.fold(&d);
        c
    }

    /// The names this agent is still waiting on an answer to.
    fn in_flight(c: &Equipped) -> Vec<(&str, &str)> {
        c.0.title
            .pending()
            .iter()
            .map(|(call, name)| (call.as_str(), name.as_str()))
            .collect()
    }

    fn answer(d: &Decision) -> String {
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        text.clone()
    }

    /// The call decides and asks; the session renames. Nothing is called
    /// anything yet — journaling the name here would replay a rename the
    /// session may still refuse.
    #[test]
    fn setting_a_title_asks_the_session() {
        let mut c = conversation();
        let d = set(&c, "call-1", "the flake");

        let [AgentDomainEvent::TitleAsked { call, name }] = d.events.as_slice() else {
            panic!("expected one Asked event, got {:?}", d.events);
        };
        assert_eq!(call, "call-1");
        assert_eq!(name, "the flake");
        let [Act::Ask(SessionRequest::SetTitle { call, title })] = d.acts.as_slice() else {
            panic!("a title that did not ask the session: {:?}", d.acts);
        };
        assert_eq!(call, "call-1");
        assert_eq!(title, "the flake");

        c.fold(&d);
        assert_eq!(
            in_flight(&c),
            vec![("call-1", "the flake")],
            "the session has not answered yet"
        );
    }

    /// **The crash window.** `Asked` is journaled before the request goes out,
    /// so one still in the fold may never have reached the session — and the
    /// model has been parked on that call ever since. The load asks again,
    /// under the same call and for the same name.
    ///
    /// No dedupe comes back the other way, and none is owed: naming a session
    /// the same thing twice leaves one session with one name.
    #[test]
    fn a_rename_the_session_never_answered_is_asked_again_on_load() {
        let c = asked("call-1", "the flake");

        // The cut: the reply is never folded, and what comes back is read off
        // the journal the way a new process reads it.
        let written = serde_json::to_string(&c.0).expect("write");
        let reloaded: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");

        let d = super::super::broadcast(&reloaded, &Msg::Loaded);
        assert!(d.events.is_empty(), "a re-ask is not a second rename");
        let [Act::Ask(SessionRequest::SetTitle { call, title })] = d.acts.as_slice() else {
            panic!("expected exactly one re-ask, got {:?}", d.acts);
        };
        assert_eq!(call, "call-1", "the answer would reach nobody");
        assert_eq!(title, "the flake");
    }

    /// And a rename the session already answered is not asked for again, or
    /// every load renames the session to whatever it was last called.
    #[test]
    fn a_rename_the_session_answered_is_not_asked_again() {
        let mut c = asked("call-1", "the flake");
        let d = c
            .handle(&Msg::Reply(&SessionReply::Done {
                call: "call-1".into(),
            }))
            .expect("mine");
        c.fold(&d);
        assert!(c.handle(&Msg::Loaded).is_none());
    }

    /// The session took it: the conversation is named, and the model — whose
    /// call has been dangling since the asking turn — is told so.
    #[test]
    fn the_session_taking_it_names_the_conversation_and_answers_the_model() {
        let mut c = asked("call-1", "the flake");
        let d = c
            .handle(&Msg::Reply(&SessionReply::Done {
                call: "call-1".into(),
            }))
            .expect("mine");
        assert_eq!(answer(&d), "Session title set to \"the flake\".");
        let [AgentDomainEvent::TitleSet { call, name }] = d.events.as_slice() else {
            panic!("expected one Set event, got {:?}", d.events);
        };
        assert_eq!((call.as_str(), name.as_str()), ("call-1", "the flake"));

        c.fold(&d);
        assert!(in_flight(&c).is_empty(), "an answered request is over");
    }

    /// **A refusal is passed back to the model verbatim.** The session is the
    /// only thing that knows why it would not take the name, so the reason
    /// reaches the model unedited — and nothing is renamed by a request that
    /// was refused.
    #[test]
    fn a_refused_rename_is_passed_back_to_the_model_verbatim() {
        let mut c = asked("call-1", "the flake");
        let reason = "this session belongs to a routine and cannot be renamed";
        let d = c
            .handle(&Msg::Reply(&SessionReply::Refused {
                call: "call-1".into(),
                reason: reason.into(),
            }))
            .expect("mine");
        assert_eq!(answer(&d), reason, "the session's reason was reworded");
        assert!(
            matches!(d.events.as_slice(), [AgentDomainEvent::TitleRefused { .. }]),
            "a refused name was journaled as one that took: {:?}",
            d.events
        );

        c.fold(&d);
        assert!(
            in_flight(&c).is_empty(),
            "a refused request stayed in flight for ever"
        );
    }

    /// A reply for a call this capability never made is not its own. Replies
    /// are offered around, so claiming one would answer for whichever
    /// capability actually asked.
    #[test]
    fn a_reply_for_someone_elses_request_is_not_mine() {
        let c = asked("call-1", "the flake");
        assert!(
            c.handle(&Msg::Reply(&SessionReply::Done {
                call: "call-9".into()
            }))
            .is_none()
        );
        // And one that has asked nothing at all claims nothing.
        assert!(
            conversation()
                .handle(&Msg::Reply(&SessionReply::Done {
                    call: "call-1".into()
                }))
                .is_none()
        );
    }

    /// Normalization happens before the ask, so what is journaled, what the
    /// session is told and what the model is shown are the same string.
    #[test]
    fn the_asked_name_is_the_normalized_one() {
        let mut c = conversation();
        let d = set(&c, "call-1", "  Fix café login ☕  ");
        let [Act::Ask(SessionRequest::SetTitle { title, .. })] = d.acts.as_slice() else {
            panic!("expected an ask, got {:?}", d.acts);
        };
        assert_eq!(title, "Fix café login ☕");

        c.fold(&d);
        let d = c
            .handle(&Msg::Reply(&SessionReply::Done {
                call: "call-1".into(),
            }))
            .expect("mine");
        assert_eq!(answer(&d), "Session title set to \"Fix café login ☕\".");
    }

    /// A title the session would refuse is refused here, and journals nothing:
    /// a name that never took must not replay as one that did. Refused rather
    /// than declined, because the capability behind this one claims every name.
    #[test]
    fn an_invalid_title_is_refused_and_records_nothing() {
        for bad in ["   ", "one\ntwo"] {
            let d = set(&conversation(), "call-1", bad);
            assert!(d.events.is_empty(), "{bad:?} was journaled");
            let text = answer(&d);
            assert!(text.contains("session title must"), "{text}");
        }
        // Arguments that are not a title at all are the same story: owned,
        // answered, and journaled nowhere.
        let d = renamed(&conversation(), "call-1", json!({}));
        assert!(d.events.is_empty());
        assert!(answer(&d).contains("cannot read"));
    }

    /// Both variants advertise the one tool: the model calls
    /// `set_session_title` whichever kind of conversation it is in, and the
    /// session is what decides which conversation that renames.
    ///
    /// Through its own layer, which dispatches to the mailbox, rather than a
    /// layer pushed in `setup`: one of those runs on the agent's task, where
    /// there is nothing to ask the session with.
    #[tokio::test]
    async fn either_variant_advertises_the_tool_without_equipping_a_layer() {
        for cap in [
            TitleCapability::new(),
            TitleCapability::for_fork(RunnerId::new_v4()),
        ] {
            let mut spec = spec();
            cap.setup(&loading(), &mut spec)
                .await
                .expect("nothing to acquire");
            assert_eq!(
                advertised_by(&Capability::Title(cap.clone()), &facts()),
                vec![TOOL]
            );
            assert_eq!(
                equipped(spec),
                Vec::<String>::new(),
                "the tool is dispatched through the mailbox, not through a layer"
            );
        }
    }

    /// The schema is only model-facing documentation, so it has to quote the
    /// limit the validation above actually enforces.
    #[test]
    fn the_advertised_schema_quotes_the_limit_it_enforces() {
        let spec = specs_of(&Capability::Title(TitleCapability::new()), &facts()).remove(0);
        assert_eq!(spec.name, TOOL);
        assert_eq!(
            spec.input_schema["properties"]["title"]["maxLength"],
            json!(SESSION_TITLE_MAX_CHARS)
        );
        assert_eq!(spec.input_schema["required"], json!(["title"]));
    }

    #[test]
    fn normalize_title_trims_and_accepts_unicode() {
        let title = normalize_session_title("  Fix café login ☕  ").unwrap();
        assert_eq!(title, "Fix café login ☕");
    }

    #[test]
    fn normalize_title_rejects_empty_multiline_and_too_long() {
        assert_eq!(
            normalize_session_title("   "),
            Err(SessionTitleError::Empty)
        );
        assert_eq!(
            normalize_session_title("one\ntwo"),
            Err(SessionTitleError::Multiline)
        );
        assert_eq!(
            normalize_session_title("one\rtwo"),
            Err(SessionTitleError::Multiline)
        );
        assert_eq!(
            normalize_session_title(&"é".repeat(61)),
            Err(SessionTitleError::TooLong { max: 60 })
        );
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
        TitleCapability::new()
            .setup(&loading(), &mut plain)
            .await
            .expect("nothing to acquire");
        let keys: Vec<&str> = plain.prompt.iter().map(|s| s.key).collect();
        assert_eq!(keys, vec!["title"]);
    }

    #[test]
    fn another_capabilitys_command_is_not_mine() {
        assert!(conversation().command(&someone_elses()).is_none());
    }

    /// The name in flight is what the reply is answered with, and the reply may
    /// land on a process that has since rehydrated the session — so losing it
    /// in the journal loses both the confirmation and the capability's claim on
    /// its own reply.
    #[test]
    fn the_request_in_flight_survives_the_journal_round_trip() {
        let mut c = asked("call-1", "the flake");
        let d = c
            .handle(&Msg::Reply(&SessionReply::Done {
                call: "call-1".into(),
            }))
            .expect("mine");
        c.fold(&d);
        // One taken, one still in flight.
        let d = set(&c, "call-2", "the other one");
        c.fold(&d);

        let written = serde_json::to_string(&c.0).expect("write");
        let back: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        assert_eq!(
            back.title
                .pending()
                .iter()
                .map(|(call, name)| (call.as_str(), name.as_str()))
                .collect::<Vec<_>>(),
            vec![("call-2", "the other one")],
            "a reload that lost the request in flight leaves the model parked"
        );

        // And a fork keeps knowing it is one, which is what its prompt turns on.
        let fork = RunnerId::new_v4();
        let forked = Capabilities::new(vec![Capability::Title(TitleCapability::for_fork(fork))]);
        let read: Capabilities =
            serde_json::from_str(&serde_json::to_string(&forked).expect("write")).expect("read");
        let [Capability::Title(back)] = read.iter().collect::<Vec<_>>()[..] else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.fork, Some(fork));
    }
}
