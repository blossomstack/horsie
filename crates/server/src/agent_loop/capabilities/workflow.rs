//! `invoke_workflow` and `workflow_status`: an agent starting a run, and
//! reading back the ones still going.
//!
//! A run invoked this way is a runner inside the same session, parented on the
//! agent that asked, so a session may hold any number of them at once — the
//! shape this replaces had one `Option<WorkflowRunState>` on the session and
//! inferred a subagent's owning tree from "which step is in flight", an
//! inference with no answer once two runs are live.
//!
//! Shaped exactly like [`super::sub_agent`], and for the same reason: a run
//! that reports owes its output to the agent that invoked it, the outstanding
//! set on [`WorkflowState`] is the one fact saying whether it has been told,
//! and the record and the delivery arrive here as one [`Reported`] the actor's
//! arm cannot split — so a crash before the write replays as an output still
//! owed rather than as one silently dropped.
//!
//! Invariant 6 applies here too: a run in flight is an outstanding child, and
//! an agent may not conclude while it has one. [`holds`] is where that is
//! asked.
//!
//! The two capabilities do not overlap. A [`ChildOutcome::SubAgent`] is
//! declined here even for a child id this capability holds: the outcome's kind
//! and the owning capability have to agree, and `None` is how they say so.
//!
//! # What this file decides, and what it does not
//!
//! Every function below returns a narrow value — a request, a run, an output —
//! and never an event. The agent actor's arm is what journals, sends and
//! answers, so a decision here cannot half-happen: there is no way to write a
//! fact from this file without the arm that also acts on it.
//!
//! # Why it advertises a tool now
//!
//! Its session-side twin equipped nothing, because `invoke_workflow` had no
//! toolbox behind it and advertising a tool before there is one to execute is
//! how a model learns to call something that answers "no such tool". That is
//! no longer true: a name this capability's [`super::Capability::layer`] claims
//! becomes an [`AgentCommand`] on the agent's own mailbox, and the arm that
//! takes it calls straight into this file — which is the half that was missing.

use super::{Mailbox, SessionReply, SessionRequest};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::Incoming;
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::sessions::runners::action::{RunnerArgs, WorkflowSource};
use crate::sessions::runners::ids::{RunnerId, RunnerKind};
use crate::sessions::runners::loading::AgentFacts;
use crate::sessions::runners::message::{ChildMsg, ChildOutcome, WorkflowOutcome};
use horsie_agentcore::{ToolSpec, Toolbox};
use horsie_models::agent::{SubAgentResultPart, ToolResultInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The tool that starts a run.
pub const INVOKE_TOOL: &str = "invoke_workflow";
/// The tool that reads back what is still running.
pub const STATUS_TOOL: &str = "workflow_status";

/// One agent's workflow runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowCapability;

/// The workflow runs this agent has in flight.
///
/// Fields private to this file, for the reason
/// [`SubAgentState`](super::sub_agent::SubAgentState)'s are: whether a run is
/// still owed is a question, not a set to read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowState {
    /// Runs asked of the session and not yet answered, by the model's call.
    ///
    /// Journaled *before* the ask goes out, so a crash in the window replays as
    /// an intent [`reloaded`] asks about again, naming the same run.
    #[serde(default)]
    requested: BTreeMap<String, Pending>,
    /// Runs that exist and still owe their output.
    ///
    /// A set rather than the session-side map to an `AgentId`: this belongs to
    /// the agent that invoked, so the output goes into its own queue and there
    /// is no address to keep.
    #[serde(default)]
    outstanding: BTreeSet<RunnerId>,
}

impl WorkflowState {
    /// Invariant 6, named: an agent may not conclude while it has outstanding
    /// children, and a run in flight is one.
    #[must_use]
    pub(crate) fn holds_conclusion(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// A run was asked of the session.
    pub(crate) fn requested(&mut self, call: String, pending: Pending) {
        self.requested.insert(call, pending);
    }

    /// The session created it, so an output is now owed.
    pub(crate) fn started(&mut self, call: &str) {
        if let Some(pending) = self.requested.remove(call) {
            self.outstanding.insert(pending.child);
        }
    }

    /// The session would not create it.
    pub(crate) fn dropped(&mut self, call: &str) {
        self.requested.remove(call);
    }

    /// This run's output reached the queue.
    pub(crate) fn reported(&mut self, child: RunnerId) {
        self.outstanding.remove(&child);
    }
}

#[cfg(test)]
/// What this state holds, for the tests that assert on it.
///
/// `#[cfg(test)]` because nothing in production reads it: the decisions that
/// need it are in this file and take `&WorkflowState`. An accessor kept for a
/// caller that does not exist is how a private field stops being private.
impl WorkflowState {
    /// The invocations the session has not answered yet.
    #[must_use]
    pub(crate) fn pending(&self) -> &BTreeMap<String, Pending> {
        &self.requested
    }

    /// The runs that still owe an output.
    #[must_use]
    pub(crate) fn outstanding(&self) -> &BTreeSet<RunnerId> {
        &self.outstanding
    }
}

/// One run asked of the session and not yet answered.
///
/// The whole request rather than its id alone, because a re-ask on load has to
/// send the *same* request again — and the name the model wrote is the only
/// thing that says which graph the run is of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The runner the run will be, and the session's dedupe key.
    pub child: RunnerId,
    pub workflow: String,
    pub input: String,
}

/// The tool's arguments: exactly what the model writes, and nothing else.
///
/// The model names a workflow; it never writes a graph. Turning that name into
/// a definition is a database read, and a database read may not happen on a
/// mailbox — so this capability asks for [`WorkflowSource::Named`] and the
/// session resolves it while performing the create. That keeps the split every
/// other request makes rather than smuggling a resolved graph through the
/// tool's own arguments, where it would sit in a field the model must never
/// fill.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub workflow: String,
    pub input: String,
}

/// What a call to `invoke_workflow` came to.
pub(crate) enum Invoked {
    /// Told why, in words, and the run carries on. Journals nothing.
    Told(String),
    /// Journal the request, put it to the session, and park the call on it:
    /// the session's answer is what this call's result is made of.
    Ask { pending: Pending, note: String },
}

/// The model called `invoke_workflow`.
///
/// The run's id is minted here rather than in the fold: a decision may be
/// non-deterministic, a fold may not. The event and the request then name the
/// same run, and replay lands the id the log has.
#[must_use]
pub(crate) fn invoked(input: &Value) -> Invoked {
    let req: Request = match serde_json::from_value(input.clone()) {
        Ok(req) => req,
        // A capability that owns a tool name owns every call to it, including
        // the malformed ones. Declining would hand a mistyped `invoke_workflow`
        // to the open-namespace sandbox, which claims anything — so the model's
        // mistake would be silently absorbed instead of corrected.
        Err(e) => {
            return Invoked::Told(format!(
                "`{INVOKE_TOOL}` was called with arguments it cannot read: {e}"
            ));
        }
    };
    let pending = Pending {
        child: RunnerId::new_v4(),
        workflow: req.workflow,
        input: req.input,
    };
    let note = format!("starting workflow {}", pending.workflow);
    Invoked::Ask { pending, note }
}

/// The request a [`Pending`] names.
///
/// One function and two callers — the invocation, and the re-ask on load —
/// because the second has to send exactly what the first sent. A free function
/// rather than a method: nothing about the capability's own state belongs in a
/// request, and this is where that shows.
#[must_use]
pub(crate) fn request(call: &str, pending: &Pending) -> SessionRequest {
    SessionRequest::StartRunner {
        call: call.to_string(),
        id: pending.child,
        kind: RunnerKind::Workflow,
        args: Box::new(RunnerArgs::Workflow {
            source: WorkflowSource::Named(pending.workflow.clone()),
            input: pending.input.clone(),
        }),
    }
}

/// Everything asked for and never answered, asked again. Empty when there is
/// nothing outstanding.
///
/// A request still in the fold is one the dead process may never have sent, and
/// the model is parked on the call that made it. Re-asked with the id already
/// recorded, so the session can tell a repeat from a second invocation.
///
/// Nothing is journaled, and there is nothing here that could be: the
/// `WorkflowRequested` this reads is still the only fact, and a second copy
/// would say a second run was wanted.
#[must_use]
pub(crate) fn reloaded(state: &WorkflowState) -> Vec<SessionRequest> {
    state
        .requested
        .iter()
        .map(|(call, pending)| request(call, pending))
        .collect()
}

/// What the session said about a run this agent asked for.
pub(crate) enum Run {
    Started { call: String, child: RunnerId },
    Dropped { call: String, reason: String },
}

impl Run {
    /// The call that is parked on this run.
    #[must_use]
    pub(crate) fn call(&self) -> &str {
        match self {
            Self::Started { call, .. } | Self::Dropped { call, .. } => call,
        }
    }

    /// The parked call's result. A refusal the model cannot see is a tool call
    /// that never returns, so it comes back as that call's result with
    /// `is_error` set rather than as a fresh answer.
    #[must_use]
    pub(crate) fn result(&self) -> ToolResultInput {
        let (output, is_error) = match self {
            Self::Started { child, .. } => (format!("Workflow run started: {child}"), false),
            Self::Dropped { reason, .. } => (reason.clone(), true),
        };
        ToolResultInput {
            tool_call_id: self.call().to_string(),
            output,
            is_error,
        }
    }
}

/// `None` when this reply answers something that is not a run of ours.
#[must_use]
pub(crate) fn replied(state: &WorkflowState, reply: &SessionReply) -> Option<Run> {
    let child = state.requested.get(reply.call())?.child;
    Some(match reply {
        SessionReply::Done { call } => Run::Started {
            call: call.clone(),
            child,
        },
        SessionReply::Refused { call, reason } => Run::Dropped {
            call: call.clone(),
            reason: reason.clone(),
        },
    })
}

/// A run reported, and this is what its output becomes in the queue.
pub struct Reported {
    pub child: RunnerId,
    pub item: Incoming,
}

/// `None` for anything this capability is not owed an output by.
#[must_use]
///
/// **No production sender reaches this yet.** The runners redesign routes a
/// child's movement through `sessions::runners::message`, which nothing
/// forwards to an agent so far, so only tests call this. Kept and kept public
/// rather than deleted: the behaviour is the settled answer for when that
/// forwarding lands, and deleting it would have to be re-derived.
pub fn child(state: &WorkflowState, m: &ChildMsg) -> Option<Reported> {
    match m {
        ChildMsg::Outcome {
            child,
            outcome: ChildOutcome::Workflow(o),
        } => {
            // Not one of mine: `None` rather than a delivery for an agent that
            // never invoked anything.
            state.outstanding.contains(child).then(|| {
                deliver(
                    *child,
                    match o {
                        WorkflowOutcome::Finished { output } => SubAgentResultPart {
                            subagent_id: child.to_string(),
                            label: "workflow".to_string(),
                            status: "completed".to_string(),
                            text: render(output),
                            spawned_at_ms: 0,
                            ended_at_ms: 0,
                        },
                        WorkflowOutcome::Failed { error } => failed_part(*child, error.clone()),
                    },
                )
            })
        }
        // A worker's report is the subagent capability's, even when this
        // capability holds that child id. Two capabilities must never both
        // plausibly claim one outcome.
        ChildMsg::Outcome {
            outcome: ChildOutcome::SubAgent(_),
            ..
        } => None,
        ChildMsg::Failed { child, error } => state
            .outstanding
            .contains(child)
            .then(|| deliver(*child, failed_part(*child, error.clone()))),
        // A run is runnable the moment it is created; only a fork has a seed
        // that can land later.
        ChildMsg::Ready { .. } => None,
    }
}

/// The output, in the shape this agent's own queue takes it.
fn deliver(child: RunnerId, part: SubAgentResultPart) -> Reported {
    Reported {
        child,
        item: Incoming::SubAgent {
            id: child.to_string(),
            part: Box::new(part),
        },
    }
}

/// What `workflow_status` shows the model.
///
/// Only what is still running: a run that reported has already been delivered
/// into this agent's transcript. A read, and it returns text rather than
/// anything to write — an event for it would grow the log every time the model
/// looked.
#[must_use]
pub(crate) fn render_status(state: &WorkflowState) -> String {
    if state.outstanding.is_empty() {
        return "No workflow runs are in flight.".to_string();
    }
    let mut text = format!("{} workflow run(s) in flight:", state.outstanding.len());
    for child in &state.outstanding {
        text.push_str(&format!("\n- {child}"));
    }
    text
}

/// Why this turn's end is not the agent finishing, when a run is still in
/// flight.
///
/// Invariant 6, asked directly rather than merged out of a broadcast: an agent
/// whose run still owes it an output must not conclude, and must not be nudged
/// either, because a nudge is for a turn that ended with *nothing* coming.
#[must_use]
pub(crate) fn holds(state: &WorkflowState) -> Option<String> {
    state.holds_conclusion().then(|| {
        format!(
            "{} workflow run(s) still in flight",
            state.outstanding.len()
        )
    })
}

/// A failed run, in the shape the invoking agent's inbox takes — the same part
/// a subagent's report arrives as, because to the agent that asked they are the
/// same thing: work it delegated, come back.
///
/// The timestamps are zero because this capability holds neither; they live on
/// the run's `RunnerRecord`, and a client shows no duration rather than one
/// that never happened.
fn failed_part(child: RunnerId, error: String) -> SubAgentResultPart {
    SubAgentResultPart {
        subagent_id: child.to_string(),
        label: "workflow".to_string(),
        status: "failed".to_string(),
        text: error,
        spawned_at_ms: 0,
        ended_at_ms: 0,
    }
}

/// A run's output is a `Value`, and a step's output is usually already text.
/// Rendering a JSON string as `"…"` would hand the model its own quotes back.
fn render(output: &Value) -> String {
    if let Value::String(s) = output {
        return s.clone();
    }
    serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string())
}

impl WorkflowCapability {
    /// Both tools, advertised here rather than pushed as a toolbox layer — see
    /// the module doc for why that is what made advertising them honest.
    fn claims(&self) -> Vec<ClaimedTool> {
        vec![
            ClaimedTool::new(
                ToolSpec {
                    name: INVOKE_TOOL.to_string(),
                    description:
                        "Start a run of a workflow already defined on this server. Returns \
                    immediately; the run's final output, or its failure, is automatically \
                    delivered back to you as a message. Carry on with independent work meanwhile, \
                    and use workflow_status only when asked for progress or when a result seems \
                    lost — never as a poll."
                            .to_string(),
                    input_schema: json!({
                        "type": "object",
                        "required": ["workflow", "input"],
                        "properties": {
                            "workflow": {
                                "type": "string",
                                "description": "The name of the workflow to run."
                            },
                            "input": {
                                "type": "string",
                                "description": "The run's input, complete and self-contained: a \
                                    run does not see this conversation."
                            }
                        }
                    }),
                },
                |input, to| AgentCommand::WorkflowInvoke {
                    input,
                    answering: to,
                },
            ),
            ClaimedTool::new(
                ToolSpec {
                    name: STATUS_TOOL.to_string(),
                    description: "List the workflow runs you started that are still in flight. Do \
                        not poll or call this tool repeatedly: a run's output and its failures are \
                        automatically delivered to you as messages."
                        .to_string(),
                    input_schema: json!({ "type": "object", "properties": {} }),
                },
                |_input, to| AgentCommand::WorkflowStatus { answering: to },
            ),
        ]
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl WorkflowCapability {
    pub fn name(&self) -> &'static str {
        "workflow"
    }

    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        _facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.claims(), mailbox)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::AgentState;
    use crate::agent_loop::capabilities::Capability;
    use crate::agent_loop::capabilities::testing::{advertised_by, facts};
    use crate::agent_loop::state::AgentDomainEvent;
    use crate::sessions::runners::message::SubAgentOutcome;

    /// The arguments a well-formed invocation carries.
    fn invoke_input() -> Value {
        json!({"workflow": "release", "input": "cut 1.2.0"})
    }

    /// One event, folded the way the actor's journal folds it. The state a
    /// capability decides from is only ever what its own events left behind.
    fn fold(state: WorkflowState, event: AgentDomainEvent) -> WorkflowState {
        AgentState {
            workflow: state,
            ..AgentState::default()
        }
        .apply(event)
        .workflow
    }

    /// The runs that still owe an output.
    fn outstanding(state: &WorkflowState) -> Vec<RunnerId> {
        state.outstanding().iter().copied().collect()
    }

    /// Invoke, and let the session say yes — the only way there is to a run in
    /// flight.
    fn started(state: WorkflowState) -> (WorkflowState, RunnerId) {
        let Invoked::Ask { pending, .. } = invoked(&invoke_input()) else {
            panic!("a well-formed invocation asks the session for a run");
        };
        let child = pending.child;
        let state = fold(
            state,
            AgentDomainEvent::WorkflowRequested {
                call: "t1".to_string(),
                pending,
            },
        );
        let run = replied(&state, &SessionReply::Done { call: "t1".into() })
            .expect("the reply answers a call I made");
        let state = fold(
            state,
            AgentDomainEvent::WorkflowStarted {
                call: run.call().to_string(),
            },
        );
        (state, child)
    }

    /// The event and the request must name the same run, or the log records a
    /// run nothing created and the agent waits for ever. The args carry the
    /// name the model wrote; the session resolves it while performing the
    /// create, because a database read may not happen on a mailbox.
    #[test]
    fn an_invocation_journals_and_asks_for_the_same_run() {
        let Invoked::Ask { pending, note } = invoked(&invoke_input()) else {
            panic!("a well-formed invocation asks the session for a run");
        };
        assert!(
            note.contains("release"),
            "the park says what it is waiting for: {note}"
        );

        // What the actor journals and what it sends are built from this one
        // `Pending`, which is what makes them the same run.
        let child = pending.child;
        let SessionRequest::StartRunner {
            call,
            id,
            kind,
            args,
        } = request("t1", &pending)
        else {
            panic!("an invocation asks the session to start a runner");
        };
        assert_eq!(id, child, "the log records a run nothing was asked for");
        assert_eq!(call, "t1");
        assert_eq!(kind, RunnerKind::Workflow);
        let RunnerArgs::Workflow { source, input } = args.as_ref() else {
            panic!("expected workflow args, got {args:?}");
        };
        let WorkflowSource::Named(name) = source else {
            panic!("a tool call names a workflow; it never writes a graph");
        };
        assert_eq!(name, "release");
        assert_eq!(input, "cut 1.2.0");

        let state = fold(
            WorkflowState::default(),
            AgentDomainEvent::WorkflowRequested {
                call: "t1".to_string(),
                pending,
            },
        );
        assert!(
            outstanding(&state).is_empty(),
            "the session has not said yes yet"
        );
    }

    /// Arguments this capability cannot read are still *its* call to answer.
    /// Falling through would hand a mistyped `invoke_workflow` to the
    /// open-namespace sandbox, which claims anything — so the model's mistake
    /// would be silently absorbed instead of corrected.
    ///
    /// `Told` is the whole "journals nothing" half: it carries words and no
    /// request, so there is nothing for the actor's arm to write.
    #[test]
    fn a_malformed_invocation_is_refused_in_words_and_journals_nothing() {
        let Invoked::Told(text) = invoked(&json!({"workflow": "nope"})) else {
            panic!("the name is mine, so the mistake is mine to answer");
        };
        assert!(
            text.contains(INVOKE_TOOL),
            "the reply names the tool: {text}"
        );
    }

    /// The session said yes: the parked call gets the run's id, and only now is
    /// an output owed.
    #[test]
    fn a_started_run_answers_the_parked_call_and_becomes_outstanding() {
        let Invoked::Ask { pending, .. } = invoked(&invoke_input()) else {
            panic!("a well-formed invocation asks the session for a run");
        };
        let child = pending.child;
        let state = fold(
            WorkflowState::default(),
            AgentDomainEvent::WorkflowRequested {
                call: "t1".to_string(),
                pending,
            },
        );

        let run = replied(&state, &SessionReply::Done { call: "t1".into() })
            .expect("the reply answers a call I made");
        assert_eq!(run.call(), "t1");
        let result = run.result();
        assert_eq!(result.tool_call_id, "t1");
        assert!(
            result.output.contains(&child.to_string()),
            "the parked call is where the model learns the run's id: {}",
            result.output
        );
        assert!(!result.is_error);

        let state = fold(
            state,
            AgentDomainEvent::WorkflowStarted {
                call: "t1".to_string(),
            },
        );
        assert_eq!(outstanding(&state), vec![child]);
        assert!(state.pending().is_empty(), "an answered request is over");
    }

    /// A refusal has to reach the model against the call it parked, or the
    /// invocation never returns.
    #[test]
    fn a_refusal_from_the_session_becomes_the_parked_calls_result() {
        let Invoked::Ask { pending, .. } = invoked(&invoke_input()) else {
            panic!("a well-formed invocation asks the session for a run");
        };
        let state = fold(
            WorkflowState::default(),
            AgentDomainEvent::WorkflowRequested {
                call: "t1".to_string(),
                pending,
            },
        );

        let run = replied(
            &state,
            &SessionReply::Refused {
                call: "t1".into(),
                reason: "no workflow named `release`".into(),
            },
        )
        .expect("the reply answers a call I made");
        let result = run.result();
        assert_eq!(result.tool_call_id, "t1");
        assert_eq!(result.output, "no workflow named `release`");
        assert!(result.is_error);

        let state = fold(
            state,
            AgentDomainEvent::WorkflowDropped {
                call: "t1".to_string(),
            },
        );
        assert!(state.pending().is_empty());
        assert!(outstanding(&state).is_empty());
    }

    /// **The crash window.** A journal that stops between `Requested` and the
    /// session's answer is a run the session may never have heard of, and the
    /// model is parked on the call that asked for it. The load asks again, with
    /// the id and the arguments the log already holds.
    #[test]
    fn a_run_the_session_never_answered_is_asked_again_on_load() {
        let Invoked::Ask { pending, .. } = invoked(&invoke_input()) else {
            panic!("a well-formed invocation asks the session for a run");
        };
        let child = pending.child;
        let first_call = "t1".to_string();
        let state = fold(
            WorkflowState::default(),
            AgentDomainEvent::WorkflowRequested {
                call: first_call.clone(),
                pending,
            },
        );

        // The cut: nothing past the request is folded, and what comes back is
        // read off the journal the way a new process reads it.
        let written = serde_json::to_string(&AgentState {
            workflow: state,
            ..AgentState::default()
        })
        .expect("write");
        let reloaded_state: AgentState = serde_json::from_str(&written).expect("read");

        // Requests and nothing else: a re-ask cannot be a second invocation,
        // because there is no event in what this returns.
        let asks = reloaded(&reloaded_state.workflow);
        let [SessionRequest::StartRunner { call, id, args, .. }] = asks.as_slice() else {
            panic!("expected exactly one re-ask, got {} of them", asks.len());
        };
        assert_eq!(
            *call, first_call,
            "a re-ask under a different call answers a park nobody is holding"
        );
        assert_eq!(*id, child, "the re-ask names a run the log never recorded");
        let RunnerArgs::Workflow { source, input } = args.as_ref() else {
            panic!("expected workflow args, got {args:?}");
        };
        let WorkflowSource::Named(name) = source else {
            panic!("a re-ask names a workflow; it never writes a graph");
        };
        // Without these the session is asked to start a run of nothing.
        assert_eq!(name, "release");
        assert_eq!(input, "cut 1.2.0");
    }

    /// And a run the session already answered is not asked for again, or every
    /// load starts the runs of the last one.
    #[test]
    fn a_run_the_session_answered_is_not_asked_again() {
        let (state, _child) = started(WorkflowState::default());
        assert!(
            reloaded(&state).is_empty(),
            "the session already started this run; asking again duplicates it"
        );
    }

    /// A reply for a call this capability never made belongs to whichever
    /// capability did make it.
    #[test]
    fn a_reply_for_a_call_i_never_made_is_not_mine() {
        assert!(
            replied(
                &WorkflowState::default(),
                &SessionReply::Done {
                    call: "someone-else".into()
                }
            )
            .is_none()
        );
    }

    /// The run's terminal output reaches the agent that invoked it, rendered as
    /// text rather than as a quoted JSON string.
    #[test]
    fn a_finished_run_queues_its_output_for_the_invoker() {
        let (state, child) = started(WorkflowState::default());
        let reported = super::child(
            &state,
            &ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                    output: json!("shipped"),
                }),
            },
        )
        .expect("mine");
        assert_eq!(reported.child, child);
        let Incoming::SubAgent { id, part } = &reported.item else {
            panic!("expected a queued output, got {:?}", reported.item);
        };
        assert_eq!(*id, child.to_string());
        assert_eq!(part.status, "completed");
        assert_eq!(part.text, "shipped", "the model was handed its own quotes");

        let state = fold(
            state,
            AgentDomainEvent::WorkflowReported {
                child: reported.child,
            },
        );
        assert!(outstanding(&state).is_empty());
    }

    /// A run that failed is still an answer. An agent blocked on one that died
    /// and was never told would wait for ever.
    #[test]
    fn a_failed_run_is_queued_as_a_failed_part() {
        let (state, child) = started(WorkflowState::default());
        let reported = super::child(
            &state,
            &ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Failed {
                    error: "step 2 failed".into(),
                }),
            },
        )
        .expect("mine");
        let Incoming::SubAgent { part, .. } = &reported.item else {
            panic!("expected a queued output, got {:?}", reported.item);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "step 2 failed");
    }

    /// A run that never started takes the same path.
    #[test]
    fn a_run_that_never_started_is_reported_as_failed() {
        let (state, child) = started(WorkflowState::default());
        let reported = super::child(
            &state,
            &ChildMsg::Failed {
                child,
                error: "the create failed".into(),
            },
        )
        .expect("mine");
        let Incoming::SubAgent { part, .. } = &reported.item else {
            panic!("expected a queued output, got {:?}", reported.item);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "the create failed");
    }

    /// The outcome's kind and the owning capability must agree. A subagent's
    /// report is declined *even for a child id this capability holds* — which
    /// is the only way two capabilities on one agent cannot both claim it.
    #[test]
    fn a_subagent_outcome_is_never_mine_even_for_a_child_i_hold() {
        let (state, child) = started(WorkflowState::default());
        assert!(
            super::child(
                &state,
                &ChildMsg::Outcome {
                    child,
                    outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                        label: "l".into(),
                        report: "r".into(),
                    }),
                }
            )
            .is_none()
        );
        assert!(super::child(&state, &ChildMsg::Ready { child }).is_none());
    }

    /// A run this capability did not invoke is not its business.
    #[test]
    fn an_outcome_for_a_run_i_did_not_invoke_is_not_mine() {
        assert!(
            super::child(
                &WorkflowState::default(),
                &ChildMsg::Outcome {
                    child: RunnerId::new_v4(),
                    outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                        output: json!("done"),
                    }),
                }
            )
            .is_none()
        );
    }

    /// **Invariant 6.** A turn ending while a run is in flight does not finish
    /// this agent: the run's output is work it has not seen yet. Asked of this
    /// capability directly, so an agent that is holding something says so
    /// rather than being invisible in a merge.
    #[test]
    fn a_turn_ending_with_a_run_in_flight_holds_the_conclusion() {
        let (state, child) = started(WorkflowState::default());
        assert!(state.holds_conclusion());
        let note = holds(&state)
            .expect("a turn ending with an output still owed must not let the agent finish");
        assert_eq!(note, "1 workflow run(s) still in flight");

        let reported = super::child(
            &state,
            &ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Finished {
                    output: json!("done"),
                }),
            },
        )
        .expect("mine");
        let state = fold(
            state,
            AgentDomainEvent::WorkflowReported {
                child: reported.child,
            },
        );
        assert!(!state.holds_conclusion());
        assert!(
            holds(&state).is_none(),
            "an agent owed nothing has no opinion about its turn ending"
        );
    }

    /// A status read journals nothing — it returns text and nothing else — and
    /// lists only what is still going.
    #[test]
    fn status_lists_the_runs_in_flight_and_journals_nothing() {
        let (state, child) = started(WorkflowState::default());
        assert!(render_status(&state).contains(&child.to_string()));

        let state = fold(state, AgentDomainEvent::WorkflowReported { child });
        assert!(!render_status(&state).contains(&child.to_string()));
    }

    /// Both tools, claimed by this capability's own layer — which is what
    /// routes the call to the mailbox, where the intent can be journaled and the
    /// call parked.
    #[test]
    fn it_advertises_both_tools() {
        assert_eq!(
            advertised_by(&Capability::Workflow(WorkflowCapability), &facts()),
            vec![INVOKE_TOOL, STATUS_TOOL]
        );
    }

    /// `outstanding` is what invariant 6 and every delivery are decided from,
    /// so losing it in the journal loses the agent.
    #[test]
    fn the_runs_in_flight_survive_the_journal_round_trip() {
        let (state, child) = started(WorkflowState::default());

        let written = serde_json::to_string(&AgentState {
            workflow: state,
            ..AgentState::default()
        })
        .expect("write");
        let back: AgentState = serde_json::from_str(&written).expect("read");
        assert_eq!(
            outstanding(&back.workflow),
            vec![child],
            "a reload that lost the run in flight would let the agent conclude \
             with an output still coming"
        );
    }
}
