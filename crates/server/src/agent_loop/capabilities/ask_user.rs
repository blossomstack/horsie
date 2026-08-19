//! `ask_user`: the agent stopping to put a question to the person.
//!
//! **The capability that forced this move.** Its session-side twin held nothing
//! but config, because the fact it wanted — the `tool_call_id` the agent is
//! parked on — is a pointer into a transcript the session does not hold and
//! cannot write. So the park lived in the agent as `AgentState::asks` while the
//! capability that owned the tool lived on the session, and the two could never
//! be joined. Here they are the same object: [`AskUserCapability::pending`] is
//! the park, folded from this capability's own events, in this agent's journal.
//!
//! # What it does with a call
//!
//! An unmuted ask journals the question and answers [`Act::Park`]: no tool
//! result, the turn ends, and the dangling `tool_use` *is* the parked agent. The
//! answer arrives against it, possibly days later, on a process that has since
//! rehydrated the session.
//!
//! A muted agent is told why, in words, and nothing is journaled — a refusal is
//! not a fact about the agent. It is refused rather than declined because
//! declining hands the call to the next capability, and the last of those is the
//! open-namespace sandbox, which claims every name: the model would be answered
//! by the sandbox and never learn why its question went nowhere. A muted agent
//! also claims no `ask_user` in its [`super::Capability::layer`], so in practice the
//! call only arrives from a plugin or a resumed transcript.
//!
//! # Abandonment stays in `queued_turn`
//!
//! A park does not survive a person typing something else — "never mind, do
//! this instead" — and every abandoned call still gets a result so nothing
//! dangles on the wire. That rule is
//! [`queued_turn`](crate::agent_loop::queued_turn), with its text in
//! [`ABANDONED_ASK_RESULT`](crate::agent_loop::ABANDONED_ASK_RESULT), and **it
//! is not reimplemented here**. It is a rule about the *queue* — which arriving
//! item is entitled to override a park, and which merely waits — and this
//! capability has no queue and should not be given one.
//!
//! What this capability does own is its own bookkeeping, and it hangs it on
//! [`TurnEvent::Began`] rather than [`TurnEvent::Ended`]:
//!
//! - `Ended` is the wrong hook, and not by a little. A park *ends its own turn*
//!   — that is what [`Act::Park`] means — so every ask is followed immediately
//!   by `Ended`. Clearing there would throw away the park it had just made.
//! - `Began` is exactly right, and it is the rule the actor already writes for
//!   `AgentState::asks`: a turn beginning ends the park either way, because the
//!   questions were answered or they were abandoned, and a result for every call
//!   was recorded before the turn started. An answered park has already cleared
//!   itself here (see [`Act::Resume`]), so a turn that begins with `pending`
//!   still full is one the queue abandoned.
//!
//! So the queue decides *whether* a park is abandoned and *what the model is
//! told*; this decides nothing and only stops holding what is no longer held.

use super::{Act, CapCommand, Decision, Mailbox, Msg, SetupError, TurnEvent};
use crate::agent_loop::state::AgentDomainEvent;
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::agent_loop::{AnswerError, AskAnswer};
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use horsie_agentcore::{AskLifecycle, LifecycleEvent, ToolSpec, Toolbox};
use horsie_models::agent::ToolResultInput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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

/// What the model asked this capability to do.
pub enum Command {
    /// `ask_user`, with the question still in the arguments it arrived in: a
    /// muted agent refuses without reading them, and reading them is the
    /// decision rather than the routing.
    Ask { input: Value },
}

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

/// Whether this agent may ask, and nothing else.
///
/// Config: what the runner chose when it equipped the agent. What the agent has
/// *asked* is [`AskUserState`], on [`AgentState`](crate::agent_loop::AgentState).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AskUserCapability {
    /// `Some` when this agent may not ask, and why. `None` is the ordinary
    /// conversation that can.
    pub mute: Option<Mute>,
}

/// The park this agent is holding.
///
/// Fields private to this file, which is what makes the park unreachable except
/// through the two questions below: nothing else can add a question, and
/// nothing else can decide an answer resumes one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AskUserState {
    /// The questions this agent is parked on: `tool_call_id` -> question.
    ///
    /// Folded from this agent's own events, so it survives an offload and a
    /// restart — the answer may arrive days after the process that asked is
    /// gone. Ordered by call id rather than by arrival because the map *is* the
    /// order results are produced in, and an order that depended on insertion
    /// would not survive the round trip that recovers it.
    #[serde(default)]
    pending: BTreeMap<String, String>,
}

impl AskUserState {
    /// The agent asked; the run is parked on `call`.
    pub(crate) fn asked(&mut self, call: String, question: String) {
        self.pending.insert(call, question);
    }

    /// These calls were answered.
    pub(crate) fn answered(&mut self, calls: &[String]) {
        for call in calls {
            self.pending.remove(call);
        }
    }

    /// The park did not survive the turn that began without it.
    pub(crate) fn abandoned(&mut self) {
        self.pending.clear();
    }

    /// Why this answer set cannot resume the park, if it cannot.
    ///
    /// The rule and the diagnostic in one place. [`super::Capability::handle`] cannot
    /// return an error — a refused answer is not a decision, so it journals
    /// nothing and produces nothing — but the person who typed the answer is
    /// owed better than silence, so the same check is reachable by name for
    /// whoever replies to them.
    ///
    /// All or nothing: a half-answered park could not resume anyway, because
    /// the next provider call would carry a `tool_use` with no result.
    #[must_use]
    pub fn answer_error(&self, answers: &[AskAnswer]) -> Option<AnswerError> {
        if self.pending.is_empty() {
            return Some(AnswerError::NothingPending);
        }
        let pending: BTreeSet<&str> = self.pending.keys().map(String::as_str).collect();
        let answered: BTreeSet<&str> = answers.iter().map(|a| a.tool_call_id.as_str()).collect();
        if pending == answered {
            return None;
        }
        // `BTreeSet::difference` is already ordered, so the message reads the
        // same on every run.
        Some(AnswerError::Incomplete {
            missing: pending
                .difference(&answered)
                .map(|c| (*c).to_string())
                .collect(),
            unexpected: answered
                .difference(&pending)
                .map(|c| (*c).to_string())
                .collect(),
        })
    }
}

#[cfg(test)]
/// What this state holds, for the tests that assert on it.
///
/// `#[cfg(test)]` because nothing in production reads it: the decisions that
/// need it are in this file and take `&self`. An accessor kept for a caller
/// that does not exist is how a private field stops being private.
impl AskUserState {
    /// The questions still waiting, in the order their results are produced.
    #[must_use]
    pub(crate) fn pending(&self) -> &BTreeMap<String, String> {
        &self.pending
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

    /// The model called `ask_user`.
    fn asked(&self, call: &str, input: &Value) -> Decision {
        if let Some(mute) = self.mute {
            // Journals nothing: a refusal is not a fact about the agent.
            return Decision::reply(call, mute.refusal());
        }
        let question = input
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Decision::record(vec![AgentDomainEvent::AskUserAsked {
            call: call.to_string(),
            question: question.clone(),
        }])
        // The question has to reach the agent's log or it never reaches the
        // person: a client reads an ask as `AskRecorded`, and a capability's
        // own events append nothing readable.
        .then(Act::Record(Box::new(LifecycleEvent::AskRecorded(
            AskLifecycle {
                tool_call_id: Some(call.to_string()),
                question: question.clone(),
            },
        ))))
        .then(Act::Park {
            call: call.to_string(),
            note: question,
        })
    }

    /// Somebody answered.
    ///
    /// `None` — "not mine" — for anything this park cannot be resumed by, which
    /// covers both arms of [`AnswerError`]: an agent parked on nothing has no
    /// claim on an answer, and one whose questions these are not cannot start a
    /// turn from them.
    fn answered(state: &AskUserState, answers: &[AskAnswer]) -> Option<Decision> {
        if state.answer_error(answers).is_some() {
            return None;
        }
        let by_call: BTreeMap<&str, &str> = answers
            .iter()
            .map(|a| (a.tool_call_id.as_str(), a.text.as_str()))
            .collect();
        // Driven by `pending` rather than by the answers, so the results come
        // out in the order the park holds them and a call cannot be answered
        // twice.
        let results: Vec<ToolResultInput> = state
            .pending
            .keys()
            .map(|call| ToolResultInput {
                tool_call_id: call.clone(),
                output: by_call.get(call.as_str()).unwrap_or(&"").to_string(),
                is_error: false,
            })
            .collect();
        Some(
            Decision::record(vec![AgentDomainEvent::AskUserAnswered {
                calls: state.pending.keys().cloned().collect(),
            }])
            .then(Act::Resume { results }),
        )
    }
}

impl AskUserCapability {
    /// Nothing when muted, which is the whole of "a muted agent has no
    /// `ask_user`".
    fn claims(&self) -> Vec<ClaimedTool> {
        if self.mute.is_some() {
            return Vec::new();
        }
        vec![ClaimedTool::new(
            ToolSpec {
                name: TOOL.to_string(),
                description:
                    "Pause and ask the user a clarifying question before continuing, when \
                their intent is ambiguous or a decision needs their input. Optional -- for an \
                ordinary reply, just answer normally instead of calling this. Omit `choices` for \
                an open question; supply `choices` to suggest answers, and set `multiple` when \
                several may be picked at once."
                        .to_string(),
                input_schema: json!({
                "type": "object",
                "required": ["question"],
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to put to the user."
                    },
                    "choices": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional suggested answers. The user can always reply in \
                            their own words instead, so treat these as suggestions and expect an \
                            answer that is not in the list."
                    },
                    "multiple": {
                        "type": "boolean",
                        "description": "Set true when the user may pick any number of the \
                            choices; omit or set false when exactly one applies. Has no effect \
                            without `choices`."
                    }
                }
                }),
            },
            |input, to| CapCommand::AskUser(Command::Ask { input }, to),
        )]
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl AskUserCapability {
    pub fn name(&self) -> &'static str {
        "ask_user"
    }

    /// A muted agent is equipped with the paragraph instead of the tool.
    ///
    /// A tool that is never advertised cannot be called, whereas one that is
    /// advertised and refused costs the model a turn to discover that — and a
    /// model that was told the tool exists and finds it missing spends a turn
    /// working out why.
    ///
    /// An unmuted one equips nothing here either: the tool is claimed by this
    /// capability's own [`super::Capability::layer`], which is what routes the call to
    /// this actor's mailbox, where the park can be journaled. A layer pushed
    /// here in `setup` runs on the agent's task and could do neither.
    pub async fn setup(&self, _loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        match self.mute {
            Some(Mute::Unattended) => spec.say("unattended", UNATTENDED_PROMPT_SUFFIX),
            Some(Mute::NotInteractive) => {
                spec.say("not_interactive", NOT_INTERACTIVE_PROMPT_SUFFIX);
            }
            None => {}
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

    pub fn command(&self, _state: &AskUserState, cmd: &CapCommand) -> Option<Decision> {
        let CapCommand::AskUser(cmd, to) = cmd else {
            return None;
        };
        let Command::Ask { input } = cmd;
        Some(self.asked(&to.call, input))
    }

    pub fn handle(&self, state: &AskUserState, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::Answer(answers) => Self::answered(state, answers),
            // The park did not survive; see the module doc for why this is
            // `Began` and not `Ended`.
            Msg::Turn(TurnEvent::Began) if !state.pending.is_empty() => {
                Some(Decision::record(vec![AgentDomainEvent::AskUserAbandoned]))
            }
            // Nothing to re-ask: this capability's park is answered by a person,
            // not by the session, so a load leaves it exactly where it was.
            Msg::Turn(_)
            | Msg::Child(_)
            | Msg::Reply(_)
            | Msg::Woke { .. }
            | Msg::Concluded
            | Msg::Loaded
            | Msg::TurnProposed => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{FakeCapability, facts};
    use super::*;
    use crate::agent_loop::capabilities::testing::{
        Equipped, advertised_by, answering, equipped, loading, spec, specs_of,
    };
    use crate::agent_loop::capabilities::{Capabilities, Capability};

    /// The command the `ask_user` layer builds for a question.
    fn ask(id: &str, question: &str) -> CapCommand {
        CapCommand::AskUser(
            Command::Ask {
                input: json!({ "question": question }),
            },
            answering(id),
        )
    }

    fn answer(id: &str, text: &str) -> AskAnswer {
        AskAnswer {
            tool_call_id: id.into(),
            text: text.into(),
        }
    }

    /// An agent that may ask, with nothing asked yet.
    fn attended() -> Equipped {
        Equipped::with(Capability::AskUser(AskUserCapability::new()))
    }

    /// Park on these questions, the only way there is to get there.
    fn parked(questions: &[(&str, &str)]) -> Equipped {
        let mut c = attended();
        for (id, q) in questions {
            c.did(&ask(id, q));
        }
        c
    }

    fn refusal(d: &Decision) -> String {
        assert!(
            d.events.is_empty(),
            "a refusal is not a fact about the agent"
        );
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
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
        assert!(
            advertised_by(&Capability::AskUser(c.clone()), &facts()).is_empty(),
            "a muted agent advertises no ask_user"
        );

        // And a call that arrives anyway is refused in words rather than
        // declined — see the test below for what declining would cost.
        let mut muted = Equipped::with(Capability::AskUser(c));
        let d = muted
            .command(&ask("t1", "which?"))
            .expect("mine even when muted");
        let said = refusal(&d);
        assert!(said.contains("routine"), "{said}");
        muted.fold(&d);
        assert!(
            muted.0.ask_user.pending().is_empty(),
            "a refused ask parks nothing"
        );
    }

    /// **A muted agent that asks anyway must be told no.** Declining its own
    /// command leaves nothing to answer it — the actor fails the call with a
    /// bug's diagnostic instead of the sentence saying why the question went
    /// nowhere.
    ///
    /// The fake beside it is the open-namespace capability that used to be able
    /// to swallow this. It cannot any more: a command names its capability, so
    /// which one answers is no longer a race the last claimant can win. What is
    /// still this capability's to get right is claiming its own.
    #[test]
    fn a_muted_ask_is_claimed_rather_than_declined() {
        for c in [
            AskUserCapability::unattended(),
            AskUserCapability::not_interactive(),
        ] {
            let state = crate::agent_loop::AgentState {
                capabilities: Capabilities::new(vec![
                    Capability::AskUser(c),
                    Capability::Fake(FakeCapability::new(TOOL)),
                ]),
                ..crate::agent_loop::AgentState::default()
            };
            let d = super::super::dispatch(&state, &ask("t1", "which?"))
                .expect("a muted ask that answers nobody");
            assert!(!refusal(&d).is_empty());
        }
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
        assert!(advertised_by(&Capability::AskUser(c.clone()), &facts()).is_empty());
        let said = refusal(
            &Equipped::with(Capability::AskUser(c))
                .command(&ask("t1", "which?"))
                .expect("mine even when muted"),
        );
        assert!(
            said.contains("step") && !said.contains("routine"),
            "a muted step was refused as though it were a routine: {said}"
        );
    }

    /// An attended session does advertise the tool — the counterpart that stops
    /// the unattended test passing for the wrong reason.
    ///
    /// Through its own layer, which dispatches to the mailbox, rather than a
    /// layer pushed in `setup`: one of those runs on the agent's task, where
    /// there is no mailbox to journal a park on.
    #[tokio::test]
    async fn an_attended_session_advertises_the_tool_without_equipping_a_layer() {
        let mut spec = spec();
        let c = AskUserCapability::new();
        c.setup(&loading(), &mut spec)
            .await
            .expect("nothing to acquire");
        assert!(spec.prompt.is_empty());
        assert_eq!(
            equipped(spec),
            Vec::<String>::new(),
            "the tool is dispatched through the mailbox, not through a layer"
        );
        assert_eq!(
            advertised_by(&Capability::AskUser(c.clone()), &facts()),
            vec![TOOL]
        );
    }

    /// The schema has to keep saying that `choices` are suggestions: a model
    /// that reads them as a closed set stops accepting the answer a person
    /// actually typed. `multiple` has to stay expressible for the same reason —
    /// a multi-select asked as a single choice loses every answer but one.
    #[test]
    fn the_advertised_schema_offers_multi_select_and_a_free_text_fallback() {
        let spec = specs_of(&Capability::AskUser(AskUserCapability::new()), &facts()).remove(0);
        let props = spec
            .input_schema
            .get("properties")
            .expect("schema has properties");
        assert_eq!(
            props.get("multiple").and_then(|m| m.get("type")),
            Some(&json!("boolean"))
        );
        assert_eq!(
            spec.input_schema.get("required"),
            Some(&json!(["question"])),
            "a plain free-text question is still one field"
        );
        let choices_doc = props
            .get("choices")
            .and_then(|c| c.get("description"))
            .and_then(Value::as_str)
            .expect("choices is documented");
        assert!(
            choices_doc.contains("own words"),
            "the model must be told choices are suggestions, not a constraint: {choices_doc}"
        );
    }

    /// The reason the capability moved: the question and the call it is parked
    /// on are journaled here, against the transcript that holds the call.
    #[test]
    fn an_ask_journals_the_question_and_parks() {
        let mut c = attended();
        let d = c.command(&ask("call-1", "which database?")).expect("mine");

        let [AgentDomainEvent::AskUserAsked { call, question }] = d.events.as_slice() else {
            panic!("expected one Asked event, got {:?}", d.events);
        };
        assert_eq!(call, "call-1");
        assert_eq!(question, "which database?");
        // The record comes before the park: a question the person never sees is
        // a park nobody can end.
        let [Act::Record(_), Act::Park { call, note }] = d.acts.as_slice() else {
            panic!("an ask that did not record and park: {:?}", d.acts);
        };
        assert_eq!(call, "call-1");
        assert_eq!(
            note, "which database?",
            "the actor's own park record has to say what is being waited for"
        );

        c.fold(&d);
        assert_eq!(
            c.0.ask_user.pending().get("call-1").map(String::as_str),
            Some("which database?")
        );
    }

    /// Answered together, resumed together: one result per parked call, in the
    /// order the park holds them, and none of them an error.
    #[test]
    fn a_complete_answer_set_resumes_with_one_result_per_question() {
        let mut c = parked(&[("call-1", "which?"), ("call-2", "which model?")]);
        let d = c
            .handle(&Msg::Answer(&[
                answer("call-2", "opus"),
                answer("call-1", "main"),
            ]))
            .expect("mine");

        let [Act::Resume { results }] = d.acts.as_slice() else {
            panic!("a complete answer set that did not resume: {:?}", d.acts);
        };
        assert_eq!(
            results
                .iter()
                .map(|r| (r.tool_call_id.as_str(), r.output.as_str(), r.is_error))
                .collect::<Vec<_>>(),
            vec![("call-1", "main", false), ("call-2", "opus", false)],
            "results must follow the park's order, not the answers'"
        );

        c.fold(&d);
        assert!(
            c.0.ask_user.pending().is_empty(),
            "an answered park is over"
        );
    }

    /// Resuming on half the answers would send the provider a `tool_use` with
    /// no result, which is the 400 the all-or-nothing rule exists to stop.
    #[test]
    fn an_incomplete_answer_set_is_refused_and_journals_nothing() {
        let c = parked(&[("call-1", "which?"), ("call-2", "which model?")]);
        assert!(
            c.handle(&Msg::Answer(&[answer("call-1", "main")]))
                .is_none(),
            "half a park cannot resume"
        );
        assert_eq!(
            c.0.ask_user.answer_error(&[answer("call-1", "main")]),
            Some(AnswerError::Incomplete {
                missing: vec!["call-2".to_string()],
                unexpected: vec![],
            }),
            "the person who answered is owed the diagnostic"
        );
        assert_eq!(
            c.0.ask_user.pending().len(),
            2,
            "a refused answer changes nothing"
        );
    }

    /// An answer for a call this agent is not parked on, and the park it does
    /// hold is left exactly where it was.
    #[test]
    fn an_answer_naming_a_call_that_is_not_pending_is_refused() {
        let c = parked(&[("call-1", "which?")]);
        let answers = [answer("call-1", "main"), answer("call-9", "who asked?")];
        assert!(c.handle(&Msg::Answer(&answers)).is_none());
        assert_eq!(
            c.0.ask_user.answer_error(&answers),
            Some(AnswerError::Incomplete {
                missing: vec![],
                unexpected: vec!["call-9".to_string()],
            })
        );
        // And an agent parked on nothing has no claim on an answer at all.
        let fresh = attended();
        assert!(
            fresh
                .handle(&Msg::Answer(&[answer("call-1", "main")]))
                .is_none()
        );
        assert_eq!(
            fresh.0.ask_user.answer_error(&[answer("call-1", "main")]),
            Some(AnswerError::NothingPending)
        );
    }

    /// A park outlives the turn that made it — that is what parking *is* — so
    /// the turn ending must not clear it. Hung on `Ended`, this capability
    /// would throw away every question the instant it asked it.
    #[test]
    fn the_turn_that_parked_does_not_end_the_park() {
        let mut c = parked(&[("call-1", "which?")]);
        for ended in [TurnEvent::Ended, TurnEvent::Failed, TurnEvent::Cancelled] {
            let d = c.handle(&Msg::Turn(ended));
            assert!(d.is_none(), "{ended:?} touched the park");
        }
        // A turn *beginning* is the other story: the questions were answered or
        // the queue abandoned them, and a result for every call was recorded
        // before it started.
        let d = c
            .handle(&Msg::Turn(TurnEvent::Began))
            .expect("the park is over");
        assert!(matches!(
            d.events.as_slice(),
            [AgentDomainEvent::AskUserAbandoned]
        ));
        assert!(d.acts.is_empty(), "abandoning is `queued_turn`'s to act on");
        c.fold(&d);
        assert!(c.0.ask_user.pending().is_empty());
        // And with nothing parked it has no opinion about a turn at all.
        assert!(c.handle(&Msg::Turn(TurnEvent::Began)).is_none());
    }

    /// The park is the state that made this capability move, so losing it in
    /// the journal loses the agent: the answer arrives days later, on a process
    /// that has since rehydrated the session.
    #[test]
    fn a_park_survives_the_journal_round_trip() {
        let c = parked(&[("call-1", "which?"), ("call-2", "which model?")]);

        let written = serde_json::to_string(&c.0).expect("write");
        let back: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        assert_eq!(
            back.ask_user
                .pending()
                .iter()
                .map(|(c, q)| (c.as_str(), q.as_str()))
                .collect::<Vec<_>>(),
            vec![("call-1", "which?"), ("call-2", "which model?")],
            "a reload that lost the park leaves the model waiting for ever"
        );

        // And a muted one keeps its reason, which is what the model is told.
        let muted = Capabilities::new(vec![Capability::AskUser(AskUserCapability::unattended())]);
        let read: Capabilities =
            serde_json::from_str(&serde_json::to_string(&muted).expect("write")).expect("read");
        let [Capability::AskUser(back)] = read.iter().collect::<Vec<_>>()[..] else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.mute, Some(Mute::Unattended));
    }
}
