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
//! also claims no `ask_user` in its [`Capability::layer`], so in practice the
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

use super::{Act, CapEvent, CapSlice, Capability, Decision, Msg, SetupError, TurnEvent};
use crate::agent_loop::toolbox::claiming;
use crate::agent_loop::{AnswerError, AskAnswer};
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use crate::sessions::runners::message::ToolCall;
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

/// What this capability records: a park opening, and a park ending.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// The agent asked, and the run parked on this call.
    Asked { call: String, question: String },
    /// These calls were answered, and the answers are on their way to the model.
    Answered { calls: Vec<String> },
    /// The park did not survive the turn that began without it.
    ///
    /// A record, not a decision: [`queued_turn`](crate::agent_loop::queued_turn)
    /// decided, and it recorded a result for every abandoned call before the
    /// turn started.
    Abandoned,
}

/// One `ask_user`, with the park it holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AskUserCapability {
    /// `Some` when this agent may not ask, and why. `None` is the ordinary
    /// conversation that can.
    pub mute: Option<Mute>,
    /// The questions this agent is parked on: `tool_call_id` -> question.
    ///
    /// Folded from this capability's own events, so it survives an offload and
    /// a restart — the answer may arrive days after the process that asked is
    /// gone. Ordered by call id rather than by arrival because the map *is* the
    /// order results are produced in, and an order that depended on insertion
    /// would not survive the round trip that recovers it.
    #[serde(default)]
    pub pending: BTreeMap<String, String>,
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
            pending: BTreeMap::new(),
        }
    }

    /// A workflow step that did not declare itself interactive.
    #[must_use]
    pub fn not_interactive() -> Self {
        Self {
            mute: Some(Mute::NotInteractive),
            pending: BTreeMap::new(),
        }
    }

    /// Why this answer set cannot resume the park, if it cannot.
    ///
    /// The rule and the diagnostic in one place. [`Capability::handle`] cannot
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

    /// The model called `ask_user`.
    fn asked(&self, call: &ToolCall) -> Decision {
        if let Some(mute) = self.mute {
            // Journals nothing: a refusal is not a fact about the agent.
            return Decision::reply(&call.id, mute.refusal());
        }
        let question = call
            .input
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Decision::record(vec![CapEvent::AskUser(Event::Asked {
            call: call.id.clone(),
            question: question.clone(),
        })])
        // The question has to reach the agent's log or it never reaches the
        // person: a client reads an ask as `AskRecorded`, and a capability's
        // own events append nothing readable.
        .then(Act::Record(Box::new(LifecycleEvent::AskRecorded(
            AskLifecycle {
                tool_call_id: Some(call.id.clone()),
                question: question.clone(),
            },
        ))))
        .then(Act::Park {
            call: call.id.clone(),
            note: question,
        })
    }

    /// Somebody answered.
    ///
    /// `None` — "not mine" — for anything this park cannot be resumed by, which
    /// covers both arms of [`AnswerError`]: an agent parked on nothing has no
    /// claim on an answer, and one whose questions these are not cannot start a
    /// turn from them.
    fn answered(&self, answers: &[AskAnswer]) -> Option<Decision> {
        if self.answer_error(answers).is_some() {
            return None;
        }
        let by_call: BTreeMap<&str, &str> = answers
            .iter()
            .map(|a| (a.tool_call_id.as_str(), a.text.as_str()))
            .collect();
        // Driven by `pending` rather than by the answers, so the results come
        // out in the order the park holds them and a call cannot be answered
        // twice.
        let results: Vec<ToolResultInput> = self
            .pending
            .keys()
            .map(|call| ToolResultInput {
                tool_call_id: call.clone(),
                output: by_call.get(call.as_str()).unwrap_or(&"").to_string(),
                is_error: false,
            })
            .collect();
        Some(
            Decision::record(vec![CapEvent::AskUser(Event::Answered {
                calls: self.pending.keys().cloned().collect(),
            })])
            .then(Act::Resume { results }),
        )
    }
}

impl AskUserCapability {
    /// Nothing when muted, which is the whole of "a muted agent has no
    /// `ask_user`".
    fn specs(&self) -> Vec<ToolSpec> {
        if self.mute.is_some() {
            return Vec::new();
        }
        vec![ToolSpec {
            name: TOOL.to_string(),
            description: "Pause and ask the user a clarifying question before continuing, when \
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
        }]
    }
}

#[async_trait::async_trait]
impl Capability for AskUserCapability {
    fn name(&self) -> &'static str {
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
    /// capability's own [`Capability::layer`], which is what routes the call to
    /// this actor's mailbox, where the park can be journaled. A layer pushed
    /// here in `setup` runs on the agent's task and could do neither.
    async fn setup(&self, _loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        match self.mute {
            Some(Mute::Unattended) => spec.say("unattended", UNATTENDED_PROMPT_SUFFIX),
            Some(Mute::NotInteractive) => {
                spec.say("not_interactive", NOT_INTERACTIVE_PROMPT_SUFFIX);
            }
            None => {}
        }
        Ok(())
    }

    fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        _facts: &AgentFacts,
        mailbox: &Arc<dyn Toolbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.specs(), mailbox)
    }

    fn handle(&self, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::Tool { call, .. } if call.name == TOOL => Some(self.asked(call)),
            Msg::Answer(answers) => self.answered(answers),
            // The park did not survive; see the module doc for why this is
            // `Began` and not `Ended`.
            Msg::Turn(TurnEvent::Began) if !self.pending.is_empty() => {
                Some(Decision::record(vec![CapEvent::AskUser(Event::Abandoned)]))
            }
            // Nothing to re-ask: this capability's park is answered by a person,
            // not by the session, so a load leaves it exactly where it was.
            Msg::Tool { .. }
            | Msg::Command(_)
            | Msg::Turn(_)
            | Msg::Child(_)
            | Msg::Reply(_)
            | Msg::Woke { .. }
            | Msg::Concluded
            | Msg::Loaded => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        // `let ... else` rather than a match with an arm per sibling: every
        // capability is offered every event, and listing the other nine here
        // would make adding a tenth a change to all of them.
        let CapEvent::AskUser(event) = event else {
            return;
        };
        match event {
            Event::Asked { call, question } => {
                self.pending.insert(call.clone(), question.clone());
            }
            Event::Answered { calls } => {
                for call in calls {
                    self.pending.remove(call);
                }
            }
            Event::Abandoned => self.pending.clear(),
        }
    }

    fn save(&self) -> CapSlice {
        CapSlice::AskUser(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{FakeCapability, facts, tool};
    use super::*;
    use crate::agent_loop::capabilities::Capabilities;
    use crate::agent_loop::capabilities::testing::{advertised_by, equipped, loading, spec};

    fn ask(id: &str, question: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: TOOL.into(),
            input: json!({ "question": question }),
        }
    }

    fn answer(id: &str, text: &str) -> AskAnswer {
        AskAnswer {
            tool_call_id: id.into(),
            text: text.into(),
        }
    }

    /// Fold a decision back into the capability that made it, exactly as the
    /// actor does — a capability that decided something has not yet changed.
    fn fold(c: &mut AskUserCapability, d: &Decision) {
        for event in &d.events {
            c.apply(event);
        }
    }

    /// Park on these questions, the only way there is to get there.
    fn parked(questions: &[(&str, &str)]) -> AskUserCapability {
        let mut c = AskUserCapability::new();
        for (id, q) in questions {
            let d = c.handle(&tool(&ask(id, q))).expect("mine");
            fold(&mut c, &d);
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
            advertised_by(&c, &facts()).is_empty(),
            "a muted agent advertises no ask_user"
        );

        // And a call that arrives anyway is refused in words rather than
        // declined — see the test below for what declining would cost.
        let said = refusal(
            &c.handle(&tool(&ask("t1", "which?")))
                .expect("mine even when muted"),
        );
        assert!(said.contains("routine"), "{said}");
        assert!(c.pending.is_empty(), "a refused ask parks nothing");
    }

    /// **A muted agent that asks anyway must be told no.** Declining the call
    /// hands it to the next capability, and the last one is the open-namespace
    /// sandbox that claims every name — so the model would be answered by the
    /// sandbox and never learn why its question went nowhere.
    #[test]
    fn a_muted_ask_is_claimed_rather_than_left_to_the_sandbox() {
        for c in [
            AskUserCapability::unattended(),
            AskUserCapability::not_interactive(),
        ] {
            // The fake stands in for the open-namespace capability behind it:
            // it claims the name too, and it is the one that answers if this
            // capability declines.
            let caps = Capabilities::new(vec![Box::new(c), Box::new(FakeCapability::new(TOOL))]);
            let taker = caps
                .iter()
                .find_map(|c| c.handle(&tool(&ask("t1", "which?"))).map(|d| (c.name(), d)));
            let Some(("ask_user", d)) = taker else {
                panic!("the sandbox layer swallowed the question: {taker:?}");
            };
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
        assert!(advertised_by(&c, &facts()).is_empty());
        let said = refusal(
            &c.handle(&tool(&ask("t1", "which?")))
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
        assert_eq!(advertised_by(&c, &facts()), vec![TOOL]);
    }

    /// The schema has to keep saying that `choices` are suggestions: a model
    /// that reads them as a closed set stops accepting the answer a person
    /// actually typed. `multiple` has to stay expressible for the same reason —
    /// a multi-select asked as a single choice loses every answer but one.
    #[test]
    fn the_advertised_schema_offers_multi_select_and_a_free_text_fallback() {
        let spec = AskUserCapability::new().specs().remove(0);
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
        let mut c = AskUserCapability::new();
        let d = c
            .handle(&tool(&ask("call-1", "which database?")))
            .expect("mine");

        let [CapEvent::AskUser(Event::Asked { call, question })] = d.events.as_slice() else {
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

        fold(&mut c, &d);
        assert_eq!(
            c.pending.get("call-1").map(String::as_str),
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

        fold(&mut c, &d);
        assert!(c.pending.is_empty(), "an answered park is over");
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
            c.answer_error(&[answer("call-1", "main")]),
            Some(AnswerError::Incomplete {
                missing: vec!["call-2".to_string()],
                unexpected: vec![],
            }),
            "the person who answered is owed the diagnostic"
        );
        assert_eq!(c.pending.len(), 2, "a refused answer changes nothing");
    }

    /// An answer for a call this agent is not parked on, and the park it does
    /// hold is left exactly where it was.
    #[test]
    fn an_answer_naming_a_call_that_is_not_pending_is_refused() {
        let c = parked(&[("call-1", "which?")]);
        let answers = [answer("call-1", "main"), answer("call-9", "who asked?")];
        assert!(c.handle(&Msg::Answer(&answers)).is_none());
        assert_eq!(
            c.answer_error(&answers),
            Some(AnswerError::Incomplete {
                missing: vec![],
                unexpected: vec!["call-9".to_string()],
            })
        );
        // And an agent parked on nothing has no claim on an answer at all.
        assert!(
            AskUserCapability::new()
                .handle(&Msg::Answer(&[answer("call-1", "main")]))
                .is_none()
        );
        assert_eq!(
            AskUserCapability::new().answer_error(&[answer("call-1", "main")]),
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
            [CapEvent::AskUser(Event::Abandoned)]
        ));
        assert!(d.acts.is_empty(), "abandoning is `queued_turn`'s to act on");
        fold(&mut c, &d);
        assert!(c.pending.is_empty());
        // And with nothing parked it has no opinion about a turn at all.
        assert!(c.handle(&Msg::Turn(TurnEvent::Began)).is_none());
    }

    /// The park is the state that made this capability move, so losing it in
    /// the journal loses the agent: the answer arrives days later, on a process
    /// that has since rehydrated the session.
    #[test]
    fn a_park_survives_a_slice_round_trip() {
        let c = parked(&[("call-1", "which?"), ("call-2", "which model?")]);
        let caps = Capabilities::new(vec![Box::new(c)]);

        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::AskUser(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(
            back.pending.into_iter().collect::<Vec<_>>(),
            vec![
                ("call-1".to_string(), "which?".to_string()),
                ("call-2".to_string(), "which model?".to_string()),
            ],
            "the reload was rebuilt from config and lost the park"
        );

        // And a muted one keeps its reason, which is what the model is told.
        let muted = Capabilities::new(vec![Box::new(AskUserCapability::unattended())]);
        let read: Capabilities =
            serde_json::from_str(&serde_json::to_string(&muted).expect("write")).expect("read");
        let CapSlice::AskUser(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.mute, Some(Mute::Unattended));
    }
}
