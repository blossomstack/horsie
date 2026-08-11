//! Where a session meets its agents' plugin hooks.
//!
//! Two sinks and the predicates they read. [`SessionHookSink`] carries what the
//! runtime's inline tool hooks did back into the agent's transcript;
//! [`StopHookParent`] decorates the outcome sink so a turn's `Stop` hooks run
//! before the session hears the turn ended. Both are held by the agent rather
//! than by the session's command loop, which is what keeps a thirty-second hook
//! from blocking a cancel.
//!
//! The predicates below are pure functions over [`HookRecord`], deliberately
//! written as exhaustive matches: a newly wired event has to be classified here
//! on purpose, because a server event misfiled as a tool one is halted twice.

use super::{
    AgentKey, CANCEL_TIMEOUT, CommandEffect, HookCommand, SessionActor, SessionCommand,
    SessionDomainEvent, SessionState,
    context::{SessionAgentKind, SessionContextProvider},
};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::{AgentOutcome, AgentOutcomeSink, Incoming};
use crate::sessions::spec::SessionStatus;
use async_trait::async_trait;
use horsie_actor::ActorContext;
use horsie_actor::ActorRef;
use horsie_actor::ReplyTo;
use horsie_models::{
    hooks::{HookAction, HookRecord, StopOutcome, SubagentStopOutcome},
    runtime::{ServerHookEvent, StopInput, SubagentStopInput},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::oneshot;

/// Adapts the session's mailbox to the [`AgentOutcomeSink`] its agents report
/// to. No generation tag: the agent is resident and fences its own stale runs
/// by `run_id`, so every outcome that arrives here is one the session asked for.
pub(super) struct SessionParent {
    target: ActorRef<SessionCommand>,
}

impl SessionParent {
    pub(super) fn new(target: ActorRef<SessionCommand>) -> Self {
        Self { target }
    }
}

#[async_trait]
impl AgentOutcomeSink for SessionParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self
            .target
            .tell(SessionCommand::AgentOutcome(outcome))
            .await;
    }
}

/// Routes what plugin hooks did into the session's journal.
///
/// A `tell`, not an `ask`: nothing waits on a record, and a hook's audit trail
/// must never be able to slow the tool call it describes.
pub(super) struct SessionHookSink {
    target: ActorRef<SessionCommand>,
    /// Which agent's transcript these records belong in. A subagent's hooks are
    /// its own; without this they would all pile into one log with no way to
    /// tell whose call they guarded.
    key: AgentKey,
}

impl SessionHookSink {
    pub(super) fn new(target: ActorRef<SessionCommand>, key: AgentKey) -> Self {
        Self { target, key }
    }
}

#[async_trait]
impl horsie_runtime_host::HookSink for SessionHookSink {
    async fn record(&self, hooks: Vec<HookRecord>) {
        // A halt is read here rather than in the session's `HooksRan` handler
        // so that handler stays what it says it is: pure routing into an
        // agent's transcript.
        //
        // Tool records only. Every *server*-initiated event's records travel
        // this sink as well as being returned to the seam that fired them —
        // `RuntimeClient::run_hooks` does both — and each of those seams reads
        // the halt off its own return value: `start_blocked` at the pre-run
        // seam, `StopHookParent` at the stop seam. Acting on them here too
        // would halt the same agent twice, and on `Stop` it would fail a turn
        // the stop seam is deliberately ending cleanly.
        let halt = tool_halt_reason(&hooks);
        let _ = self
            .target
            .tell(SessionCommand::Hooks(HookCommand::Ran {
                key: self.key,
                records: hooks,
            }))
            .await;
        // After the records, so the transcript shows what halted the turn
        // above the turn's own failure.
        if let Some(reason) = halt {
            let _ = self
                .target
                .tell(SessionCommand::Hooks(HookCommand::Halt {
                    key: self.key,
                    reason,
                }))
                .await;
        }
    }
}

/// How many times a `Stop` hook may hold a turn open before horsie ends it
/// regardless.
///
/// Not advisory. horsie runs unattended sessions, and `stop_hook_active` only
/// stops a hook that reads it — this exists for the ones that do not.
pub(super) const MAX_STOP_CONTINUATIONS: usize = 3;

/// Runs `Stop` hooks when a turn concludes, and honours what they say.
///
/// A decorator on the outcome sink rather than a branch in the session's
/// `AgentOutcome` handler, because `deliver` is called from the *agent's*
/// `RunFinished` handler. A slow hook therefore delays that agent's own mailbox
/// and never the session's command loop, which stays able to serve a cancel or
/// another agent while a 30-second `Stop` hook runs.
pub(super) struct StopHookParent {
    inner: Arc<dyn AgentOutcomeSink>,
    session: ActorRef<SessionCommand>,
    key: AgentKey,
    /// The provider whose `provide()` cached this agent's client. `Stop` never
    /// acquires a runtime of its own: a turn that already concluded must not be
    /// able to fail on provisioning, and there is nothing to guard if no runtime
    /// ever ran.
    provider: Arc<SessionContextProvider>,
    /// Consecutive continuations. Reset whenever a turn concludes without a
    /// block, so a long interactive session that legitimately continues a few
    /// times never accumulates toward the cap.
    continuations: Arc<AtomicUsize>,
}

impl StopHookParent {
    /// The outcome sink one of a session's agents reports to: the session's own,
    /// wrapped so this agent's `Stop` hooks run first.
    pub(super) fn wrap(
        session: ActorRef<SessionCommand>,
        key: AgentKey,
        provider: Arc<SessionContextProvider>,
    ) -> Arc<dyn AgentOutcomeSink> {
        Arc::new(Self {
            inner: Arc::new(SessionParent::new(session.clone())),
            session,
            key,
            provider,
            continuations: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl AgentOutcomeSink for StopHookParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        // `Stop` fires when a turn *ends*. An ask or a park is a turn still in
        // progress, and a failure is not a stop the hook could act on.
        let AgentOutcome::Concluded { output, .. } = &outcome else {
            return self.inner.deliver(outcome).await;
        };
        // No plugins, or no runtime this turn: nothing declared a hook, so the
        // round-trip would be pure latency on every single turn.
        let Some(client) = self
            .provider
            .use_plugins()
            .then(|| self.provider.cached_client())
            .flatten()
        else {
            return self.inner.deliver(outcome).await;
        };

        let used = self.continuations.load(Ordering::Relaxed);
        let last_assistant_message = output.as_str().map(str::to_string);
        // The spec's own definition: true when horsie would normally stop but is
        // being held in the loop by a blocking hook. A cooperative hook returns
        // early on it.
        let stop_hook_active = used > 0;
        // A subagent's turn ending is a `SubagentStop`, not a `Stop`. This sink
        // decorates every agent a session hosts, and until it was gated on the
        // kind a session with four subagents fired `Stop` five times.
        let event = match self.provider.kind {
            SessionAgentKind::Sub(id) => ServerHookEvent::SubagentStop(SubagentStopInput {
                agent_id: id.to_string(),
                agent_type: self.provider.agent_type(),
                last_assistant_message,
                stop_hook_active,
            }),
            // A step keeps `Stop`: it fires `SessionStart` and roots its own
            // subagent tree, so answering `SubagentStop` would contradict its
            // own start.
            SessionAgentKind::Main | SessionAgentKind::Step(_) => {
                ServerHookEvent::Stop(StopInput {
                    last_assistant_message,
                    stop_hook_active,
                })
            }
        };
        let records = client.run_hooks(event).await.unwrap_or_default();

        // A halt outranks a block, which is the spec's own precedence: a hook
        // that says both is asking to stop, and the turn is already stopping.
        // So this ends the turn the way an unblocked one ends — the records are
        // already on their way to the transcript, `run_hooks` having put them on
        // the sink, and there is nothing to add to them the way `CapReached`
        // adds to a block.
        if let Some(reason) = halt_reason(&records) {
            self.continuations.store(0, Ordering::Relaxed);
            tracing::info!(reason, "a stop hook set continue: false; the turn ends");
            return self.inner.deliver(outcome).await;
        }

        match stop_verdict(&records) {
            // Blocked *from stopping*, with budget left: the turn does not
            // conclude. The parent never hears about it, so the session never
            // marks the turn done and never drains its queue early.
            Some(reason) if used < MAX_STOP_CONTINUATIONS => {
                self.continuations.fetch_add(1, Ordering::Relaxed);
                let _ = self
                    .session
                    .tell(SessionCommand::Hooks(HookCommand::ContinueAfterStop {
                        key: self.key,
                        reason,
                    }))
                    .await;
            }
            // Blocked, but out of budget. The turn ends, and a second record
            // says why — otherwise this reads as a turn that stopped on its own.
            Some(_) => {
                self.continuations.store(0, Ordering::Relaxed);
                let _ = self
                    .session
                    .tell(SessionCommand::Hooks(HookCommand::Ran {
                        key: self.key,
                        records: cap_reached(records),
                    }))
                    .await;
                self.inner.deliver(outcome).await;
            }
            None => {
                self.continuations.store(0, Ordering::Relaxed);
                self.inner.deliver(outcome).await;
            }
        }
    }
}

/// Why a hook in this batch set `continue: false`, if one did.
///
/// Reads the envelope rather than any outcome: `continue` is a common field, so
/// every seam that can act on a halt reads it the same way.
fn halt_reason(records: &[HookRecord]) -> Option<String> {
    records.iter().find_map(halt_of)
}

/// The same, restricted to the records the sink is the only route for.
fn tool_halt_reason(records: &[HookRecord]) -> Option<String> {
    records
        .iter()
        .filter(|r| is_tool_seam(&r.action))
        .find_map(halt_of)
}

/// One record's halt, with the fallback every seam shows when the hook set
/// `continue: false` without a `stopReason`.
fn halt_of(record: &HookRecord) -> Option<String> {
    record.halt.as_ref().map(|h| {
        h.reason
            .clone()
            .unwrap_or_else(|| "a hook set continue: false".to_string())
    })
}

/// Whether this record was made by a hook the runtime ran inline with a tool
/// call, rather than by one the server initiated.
///
/// Listed rather than `_`: a newly wired event must be classified here
/// deliberately, because a server event misfiled as a tool one is halted twice.
fn is_tool_seam(action: &HookAction) -> bool {
    match action {
        HookAction::PreToolUse(_)
        | HookAction::PostToolUse(_)
        | HookAction::PostToolUseFailure(_)
        | HookAction::PostToolBatch(_) => true,
        HookAction::SessionStart(_)
        | HookAction::SessionEnd(_)
        | HookAction::UserPromptSubmit(_)
        // Fired from the pre-run seam like the other server events, so its
        // records reach this sink and the expansion path both.
        | HookAction::UserPromptExpansion(_)
        | HookAction::Stop(_)
        | HookAction::StopFailure(_)
        | HookAction::SubagentStart(_)
        | HookAction::SubagentStop(_)
        | HookAction::TaskCreated(_)
        | HookAction::TaskCompleted(_)
        | HookAction::Notification(_)
        // Fired by the compaction, which the server owns: a run asks its
        // policy, and the policy is the server's.
        | HookAction::PreCompact(_)
        | HookAction::PostCompact(_)
        | HookAction::CwdChanged(_) => false,
    }
}

/// Why a stop hook is holding this turn open, if one is.
///
/// Both stop events, because both mean the same thing: blocked *from stopping*,
/// so the agent that fired it continues under the same budget.
fn stop_verdict(records: &[HookRecord]) -> Option<String> {
    // An empty input is not a turn, so a hook that blocked without saying why
    // still has to say something.
    let said = |reason: &Option<String>, fallback: &str| {
        Some(reason.clone().unwrap_or_else(|| fallback.to_string()))
    };
    records.iter().find_map(|r| match &r.action {
        HookAction::Stop(s) => match &s.outcome {
            StopOutcome::Blocked(b) => said(&b.reason, "a Stop hook asked for another iteration"),
            // A failure is never fatal here: a stop hook runs after the fact, so
            // a guard that could not run cannot deny anything. Only `PreToolUse`
            // fails closed.
            StopOutcome::Ran(_) | StopOutcome::Failed(_) | StopOutcome::CapReached(_) => None,
        },
        HookAction::SubagentStop(s) => match &s.outcome {
            SubagentStopOutcome::Blocked(b) => {
                said(&b.reason, "a SubagentStop hook asked for another iteration")
            }
            SubagentStopOutcome::Ran(_)
            | SubagentStopOutcome::Failed(_)
            | SubagentStopOutcome::CapReached(_) => None,
        },
        // Listed rather than `_`: a future event that can hold a turn open must
        // fail to compile here rather than be silently ignored.
        HookAction::PreToolUse(_)
        | HookAction::PostToolUse(_)
        | HookAction::PostToolUseFailure(_)
        | HookAction::PostToolBatch(_)
        // A `PreCompact` block stops the compaction, not the turn; the
        // compaction path reads its own verdict.
        | HookAction::PreCompact(_)
        | HookAction::PostCompact(_)
        | HookAction::SessionStart(_)
        | HookAction::SessionEnd(_)
        | HookAction::UserPromptSubmit(_)
        | HookAction::UserPromptExpansion(_)
        | HookAction::StopFailure(_)
        | HookAction::SubagentStart(_)
        | HookAction::TaskCreated(_)
        | HookAction::TaskCompleted(_)
        | HookAction::Notification(_)
        | HookAction::CwdChanged(_) => None,
    })
}

/// Narrow a blocking record's outcome to name the cap.
///
/// The only place `CapReached` is produced: `HookInvocation::record` sees one
/// hook's reply and cannot know the budget, so the outcome is narrowed here
/// rather than invented in the library.
fn cap_reached(mut records: Vec<HookRecord>) -> Vec<HookRecord> {
    for r in &mut records {
        match &mut r.action {
            HookAction::Stop(s) => {
                if let StopOutcome::Blocked(b) = &s.outcome {
                    s.outcome = StopOutcome::CapReached(b.clone());
                }
            }
            HookAction::SubagentStop(s) => {
                if let SubagentStopOutcome::Blocked(b) = &s.outcome {
                    s.outcome = SubagentStopOutcome::CapReached(b.clone());
                }
            }
            HookAction::PreToolUse(_)
            | HookAction::PostToolUse(_)
            | HookAction::PostToolUseFailure(_)
            | HookAction::PostToolBatch(_)
            | HookAction::SessionStart(_)
            | HookAction::SessionEnd(_)
            | HookAction::UserPromptSubmit(_)
            | HookAction::UserPromptExpansion(_)
            | HookAction::StopFailure(_)
            | HookAction::SubagentStart(_)
            | HookAction::TaskCreated(_)
            | HookAction::TaskCompleted(_)
            | HookAction::Notification(_)
            // A compaction has no continuation budget to run out of: a
            // `PreCompact` block abandons it once and nothing loops.
            | HookAction::PreCompact(_)
            | HookAction::PostCompact(_)
            | HookAction::CwdChanged(_) => {}
        }
    }
    records
}

/// Routing what plugin hooks did into the session.
///
/// No events and no state: a hook record belongs in the transcript of the agent
/// whose call it guarded, so this component only ever forwards. The one thing it
/// decides is what a halt *means*, and it decides it by not deciding — a halt
/// re-enters through the ordinary outcome path, so "what a failure means" stays
/// answered in one place per agent kind rather than branching here.
pub(super) struct HookRouting;

impl HookRouting {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: HookCommand,
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            HookCommand::Ran { key, records } => {
                // The agent owns its own transcript, so the records go to it
                // rather than into the session's log. An agent that has already
                // gone is not an error: the records describe a call it made
                // before it left, and there is nothing left to tell.
                if let Some(agent) = actor.agents.as_ref().and_then(|a| a.get(key)) {
                    let _ = agent.actor.tell(AgentCommand::HooksRan { records }).await;
                }
                CommandEffect::none()
            }
            HookCommand::Halt { key, reason } => {
                // A halt races the turn it is halting: the records reach the
                // session on the sink while the tool call that produced them is
                // still returning, so the turn can finish first. Failing it then
                // would rewrite a turn that already ended — which is why
                // `ContinueAfterStop` below no-ops on the same condition.
                let live = actor
                    .agents
                    .as_ref()
                    .and_then(|a| a.get(key))
                    .filter(|_| state.status == SessionStatus::Running)
                    .cloned();
                let Some(agent) = live else {
                    tracing::warn!(
                        session = %actor.id,
                        "a hook halted an agent whose turn had already ended; ignored"
                    );
                    return CommandEffect::none();
                };
                // Cancel first, so the agent is not still appending to its own
                // journal when the outcome below is folded.
                let (tx, rx) = oneshot::channel();
                let _ = agent
                    .actor
                    .tell(AgentCommand::Cancel {
                        ack: Some(ReplyTo::from_sender(tx)),
                    })
                    .await;
                if tokio::time::timeout(CANCEL_TIMEOUT, rx).await.is_err() {
                    tracing::warn!(session = %actor.id, "halted agent did not finish in time");
                }
                // Routed through the ordinary outcome path rather than given its
                // own per-key branching: a halt is a failure with a reason, and
                // what a failure means for a main agent, a subagent and a step is
                // already decided in one place.
                actor
                    .on_agent_outcome(
                        state,
                        AgentOutcome::Failed {
                            agent: match key {
                                AgentKey::Main => actor.id,
                                AgentKey::Sub(id) | AgentKey::Step(id) => id,
                            },
                            error: reason,
                            // Not recoverable and not terminal: re-running the same
                            // turn would meet the same hook, but the session is
                            // perfectly able to run the next thing the user sends.
                            recoverable: false,
                            terminal: false,
                        },
                        ctx,
                    )
                    .await
            }
            HookCommand::ContinueAfterStop { key, reason } => {
                // One more thing addressed to the agent, queued like the rest:
                // the turn it continues is over by the time this lands, so the
                // agent's own boundary drain is what starts the next one.
                if let Some(agent) = actor.agents.as_ref().and_then(|a| a.get(key)) {
                    let _ = agent
                        .actor
                        .tell(AgentCommand::Enqueue {
                            item: Incoming::Continue {
                                id: uuid::Uuid::new_v4().to_string(),
                                reason,
                            },
                            ack: None,
                        })
                        .await;
                }
                CommandEffect::none()
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    //! What a `Stop` hook can do to a turn, and what a halt means.
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    /// A blocking `Stop` means *blocked from stopping*: the turn does not
    /// conclude, and the reason becomes the input to another run. The opposite
    /// of a `PreToolUse` refusal.
    #[tokio::test]
    async fn a_blocking_stop_hook_starts_another_run_with_its_reason() {
        let (_f, session) = stop_harness(vec![
            stop_blocked("tests still failing"),
            vec![stop_record(StopOutcome::Ran(
                horsie_models::hooks::ContextInjected {
                    additional_context: None,
                },
            ))],
        ])
        .await;
        send(&session, "do the thing").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(inputs.len(), 2, "the turn continued once: {inputs:?}");
        assert!(inputs[0].contains("do the thing"), "{inputs:?}");
        assert!(inputs[1].contains("tests still failing"), "{inputs:?}");
    }

    /// The loop guard that must not be optional: horsie runs unattended
    /// sessions, so a hook ignoring `stop_hook_active` would spin forever with
    /// nobody watching.
    #[tokio::test]
    async fn an_unconditionally_blocking_stop_hook_is_stopped_by_the_cap() {
        let (_f, session) = stop_harness(vec![stop_blocked("again")]).await;
        send(&session, "go").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(
            inputs.len(),
            1 + MAX_STOP_CONTINUATIONS,
            "the original turn plus exactly the cap: {inputs:?}"
        );
    }

    /// And the record says the cap ended it, rather than looking like a turn
    /// that ended on its own.
    #[tokio::test]
    async fn the_capped_continuation_is_recorded_as_cap_reached() {
        let (_f, session) = stop_harness(vec![stop_blocked("again")]).await;
        send(&session, "go").await;
        settled_inputs(&session).await;
        let outcomes = stop_outcomes(&session).await;
        assert!(
            matches!(outcomes.last(), Some(StopOutcome::CapReached(_))),
            "the last record must name the cap, got {outcomes:?}"
        );
    }

    /// Non-blocking feedback informs the model; it does not force a turn.
    /// Starting a run on it would make every advisory `Stop` hook an infinite
    /// session.
    #[tokio::test]
    async fn non_blocking_additional_context_does_not_start_a_run() {
        let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Ran(
            horsie_models::hooks::ContextInjected {
                additional_context: Some("consider the tests".into()),
            },
        ))]])
        .await;
        send(&session, "go").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(inputs.len(), 1, "informed, not forced: {inputs:?}");
    }

    /// `continue: false` outranks `decision: "block"`, which is the spec's own
    /// precedence — and the one seam where that precedence is observable. The
    /// same record blocks *and* halts; the turn ends rather than continuing.
    #[tokio::test]
    async fn a_halt_beats_a_blocking_stop_hook() {
        let mut blocking = stop_blocked("tests still failing");
        blocking[0].halt = Some(horsie_models::hooks::HookHalt {
            reason: Some("out of budget".into()),
        });
        let (_f, session, id, journal) = stop_harness_with_journal(vec![blocking]).await;
        send(&session, "go").await;
        let inputs = settled_inputs(&session).await;
        assert_eq!(
            inputs.len(),
            1,
            "the halt must stop the block continuing the turn: {inputs:?}"
        );
        // And ends it *cleanly*. `run_hooks` puts its records on the same sink
        // tool records take, so before `tool_halt_reason` narrowed what the sink
        // acts on, this halt also arrived as a `HaltAgent` and failed the turn
        // the stop seam had already concluded. Read off the journal rather than
        // the status: `TurnEnded` lands after the spurious failure and hides it.
        let events = journaled_events(&journal, id).await;
        assert!(
            !events.iter().any(|e| e.contains("TurnFailed")),
            "a halted stop ends the turn, it does not fail it: {events:?}"
        );
    }

    /// A halt from a tool hook reaches the session as its own command, because
    /// the runtime that ran the hook cannot end a turn and the agent is mid-call
    /// when it arrives. The reason is what the user is shown.
    #[tokio::test]
    async fn halting_the_main_agent_fails_the_turn_with_the_hooks_reason() {
        let gate = BlockingProvider::new();
        let (_f, session, _id, _journal) = spawn_session_with_provider(gate.clone()).await;
        let status = |s: ActorRef<SessionCommand>| async move {
            s.ask(|reply| SessionCommand::Read(ReadCommand::Snapshot { reply }))
                .await
                .unwrap()
                .status
        };
        send(&session, "go").await;
        // The turn is parked in the provider, which is where a tool hook's halt
        // would arrive from.
        for _ in 0..200 {
            if status(session.clone()).await == SessionStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        session
            .tell(SessionCommand::Hooks(HookCommand::Halt {
                key: AgentKey::Main,
                reason: "the repo is locked".into(),
            }))
            .await
            .unwrap();
        gate.release();

        for _ in 0..200 {
            if let SessionStatus::Failed { reason } = status(session.clone()).await {
                assert_eq!(reason, "the repo is locked");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the halted turn never failed");
    }

    /// `Stop` runs after the fact, so a guard that could not run cannot deny
    /// anything. Only `PreToolUse` fails closed.
    #[tokio::test]
    async fn a_failing_stop_hook_concludes_the_turn_anyway() {
        let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Failed(
            horsie_models::hooks::HookFailed {
                reason: "spawn failed".into(),
            },
        ))]])
        .await;
        send(&session, "go").await;
        assert_eq!(settled_inputs(&session).await.len(), 1);
    }

    /// Every `Stop` hook that ran reaches the transcript, which is the point of
    /// running them at all.
    #[tokio::test]
    async fn every_stop_hook_run_reaches_the_transcript() {
        let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Ran(
            horsie_models::hooks::ContextInjected {
                additional_context: None,
            },
        ))]])
        .await;
        send(&session, "go").await;
        settled_inputs(&session).await;
        assert_eq!(stop_outcomes(&session).await.len(), 1);
    }

    /// The bug this change exists to close, end to end.
    ///
    /// `injected_context` knew how to pull `additionalContext` off a `Stop`
    /// record and had exactly one caller — the `SessionStart` bootstrap — so a
    /// `Stop` hook's context was recorded, rendered in the web UI, and never
    /// shown to the model. It reaches the next turn's prompt now because
    /// `prompt_messages` translates the record where it sits.
    #[tokio::test]
    async fn a_stop_hooks_context_reaches_the_next_prompt() {
        let (_f, session, prompts) = stop_harness_with_prompts(vec![vec![stop_record(
            StopOutcome::Ran(horsie_models::hooks::ContextInjected {
                additional_context: Some("run the linter before you finish".into()),
            }),
        )]])
        .await;
        send(&session, "first").await;
        settled_inputs(&session).await;
        assert_eq!(stop_outcomes(&session).await.len(), 1);

        // The hook ran when the first turn ended, so the second turn is the
        // first prompt that can carry it.
        send(&session, "second").await;
        settled_inputs(&session).await;

        let seen = prompts.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|t| t.contains("run the linter before you finish")),
            "the Stop hook's context must reach the model, got {seen:?}"
        );
    }

    /// A hook guards one agent's call, so its record belongs in that agent's
    /// transcript. Routed to the session instead, every agent's hooks would pile
    /// into one log with no way to tell whose call they guarded — which is what
    /// the session-scoped journal did before.
    #[tokio::test]
    async fn a_subagents_hooks_land_on_its_own_transcript() {
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let sub = spawn_sub(&session, "research", "dig into it").await;
        wait_for_tree(&journal, id, |t| {
            t.node(sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;

        session
            .tell(SessionCommand::Hooks(HookCommand::Ran {
                key: AgentKey::Sub(sub),
                records: vec![hook_record("guard", "tc1")],
            }))
            .await
            .unwrap();

        // `tell` is fire-and-forget through two mailboxes; poll rather than race.
        let mut waited = 0;
        loop {
            let page = agent_history(&session, Some(sub.to_string())).await;
            if !hook_ids(&page).is_empty() {
                assert_eq!(hook_ids(&page), vec!["hook:0".to_string()]);
                break;
            }
            assert!(waited < 100, "the subagent never recorded the hook");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 1;
        }

        let main = agent_history(&session, None).await;
        assert!(
            hook_ids(&main).is_empty(),
            "the main agent made no such call: {:?}",
            main.entries
        );
    }
}
