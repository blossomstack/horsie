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
    AgentKey, SessionCommand,
    context::{SessionAgentKind, SessionContextProvider},
};
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_models::{
    hooks::{HookAction, HookRecord, StopOutcome, SubagentStopOutcome},
    runtime::{ServerHookEvent, StopInput, SubagentStopInput},
};
use horsie_workflow::{AgentOutcome, AgentOutcomeSink};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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
impl horsie_runtime_client::HookSink for SessionHookSink {
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
            .tell(SessionCommand::HooksRan {
                key: self.key,
                records: hooks,
            })
            .await;
        // After the records, so the transcript shows what halted the turn
        // above the turn's own failure.
        if let Some(reason) = halt {
            let _ = self
                .target
                .tell(SessionCommand::HaltAgent {
                    key: self.key,
                    reason,
                })
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
                    .tell(SessionCommand::ContinueAfterStop {
                        key: self.key,
                        reason,
                    })
                    .await;
            }
            // Blocked, but out of budget. The turn ends, and a second record
            // says why — otherwise this reads as a turn that stopped on its own.
            Some(_) => {
                self.continuations.store(0, Ordering::Relaxed);
                let _ = self
                    .session
                    .tell(SessionCommand::HooksRan {
                        key: self.key,
                        records: cap_reached(records),
                    })
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
            | HookAction::CwdChanged(_) => {}
        }
    }
    records
}
