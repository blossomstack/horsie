//! The events a session journals, in the runner vocabulary.
//!
//! Three families. Session-scoped facts (the spec, the sandbox, a terminal
//! failure) name no runner. Agent-turn facts ([`SessionEvent::TurnBegan`],
//! [`SessionEvent::TurnEnded`]) name the agent; the *owning runner's* fold
//! decides what an end means, which is what replaced nine per-kind variants
//! (`TurnEnded`/`TurnFailed`/`TurnStopped`/`TurnInterrupted`,
//! `SubAgentCompleted`/`SubAgentFailed`, `StepConcluded`/`StepFailed`,
//! `ForkTurnEnded`) with one carrying a [`RecordedEnd`]. Runner-scoped facts
//! ride the [`SessionEvent::Runner`] envelope, so the fold routes them by id
//! instead of probing registries.
//!
//! Every variant that records a moment carries `at_ms`, the unix-epoch
//! millisecond it was recorded, stamped where the event is built — a fold may
//! never read a clock.

use crate::agent_loop::UsageTotal;
use crate::sessions::forks::ForkMode;
use crate::sessions::spec::{AgentSettings, SessionSpec};
use crate::sessions::workflow::WorkflowRunSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{AgentId, RunnerId};

/// Events recording a session's lifecycle. Persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    /// What this session is. The first thing a session journals, and the only
    /// thing a host needs besides the id to run it. Boxed because a spec is
    /// much larger than any other variant here.
    SpecRecorded { spec: Box<SessionSpec> },
    /// This session was given a name — by a person, by the title tool, or
    /// derived from the first message.
    Renamed { name: String },
    /// This session's runtime is being built. Journaled *before* the vendor is
    /// called: the status it produces starts nothing, so a message that
    /// arrives meanwhile queues instead of addressing a runtime that does not
    /// exist. Found unfinished at load, it is safe to re-attempt — no turn can
    /// have run under it.
    ProvisioningStarted { at_ms: u64 },
    /// What the vendor said about a create still in flight, in its own words.
    /// Narration: it decides nothing.
    ProvisioningProgress { at_ms: u64, detail: String },
    /// The vendor confirmed the runtime.
    ProvisioningSucceeded { at_ms: u64 },
    /// The create failed. `terminal` carries the one distinction that matters:
    /// a live vendor refusing to produce the runtime ends the session, while
    /// an offline vendor or a failed token mint leaves it retryable.
    ProvisioningFailed {
        at_ms: u64,
        error: String,
        terminal: bool,
    },
    /// Terminal: this session can never run again.
    SessionFailed { at_ms: u64, reason: String },
    /// One agent's cumulative usage after a completed run. Durable here so the
    /// session-level total never requires waking an idle agent. Keyed as the
    /// wire keys agents: `"main"`, or the agent's uuid.
    UsageRecorded {
        at_ms: u64,
        agent_id: String,
        usage_total: UsageTotal,
    },
    /// An agent started a turn. Recorded, not decided: the agent owns its own
    /// queue and chooses when that queue becomes a turn.
    TurnBegan { at_ms: u64, agent: AgentId },
    /// One of an agent's turns ended, however it ended. What the end *means* —
    /// a conversation resting, a delegated task owing its report, a step
    /// entry concluding — is the owning runner's fold's decision.
    TurnEnded {
        at_ms: u64,
        agent: AgentId,
        end: RecordedEnd,
    },
    /// A fact about one runner.
    Runner {
        id: RunnerId,
        at_ms: u64,
        event: RunnerEvent,
    },
}

/// How a recorded turn ended.
///
/// The journal's shape of the ways a turn can end, distinct from the in-memory
/// `TurnEnd` an [`crate::agent_loop::AgentOutcome`] narrows to: a runner's
/// decision sits between them, so ends that are never journaled (a subagent's
/// interruption, a park) have no variant to mis-fold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RecordedEnd {
    /// The agent produced its output — structured, or its final text.
    Concluded { output: Value },
    /// The turn failed. Whether that fails the session too was decided before
    /// journaling: an unrecoverable failure is a separate [`SessionEvent::SessionFailed`].
    Failed { error: String },
    /// A person cancelled the turn. Distinct from `Concluded` only in intent.
    Stopped,
    /// The process died inside the turn, and the agent said so at recovery.
    Interrupted,
    /// The agent parked on questions for the user. Not a boundary — the turn
    /// is parked, not over — but it is the moment a status changes, and the
    /// one thing the session needs durable about it.
    Asked,
}

/// A fact about one runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerEvent {
    /// The runner exists. One creation event for every kind — persisted before
    /// any actor does, so a crash between the two replays as a runner that
    /// recovery repairs rather than an untracked agent.
    Created {
        /// The agent that asked for this runner. `None` only for the session's
        /// root — the one runner the session *is* rather than hosts.
        parent: Option<AgentId>,
        /// Everything the runner needs to be self-contained, snapshotted at
        /// creation. Boxed because a workflow graph dwarfs the other variants.
        args: Box<RunnerArgs>,
    },
    /// One execution of one workflow step began. Appended, never replacing: a
    /// loop back onto a step and a retry of one are both new entries, which is
    /// what keeps the log replayable and the graph projection lossless.
    StepStarted {
        index: u32,
        step: String,
        agent: AgentId,
        attempt: u32,
        /// The entry this came out of; `None` for the start step.
        from: Option<u32>,
        /// The transition condition that matched, if any.
        via: Option<String>,
        input: String,
    },
    /// A step was cancelled — by an interrupt, or by a retry taking its place.
    /// Suspends the run: a person decides between retrying and abandoning,
    /// because the step's effect on the shared workspace is unknown.
    StepCancelled { index: u32 },
    /// The run reached a terminal step with no error.
    RunFinished { output: Value },
    /// The run cannot continue — no transition matched, a step failed, or its
    /// budget ran out.
    RunFailed { error: String },
    /// This runner's latest terminal result was sent to the agent owed it.
    /// Persisted in the same effect as the send, so a reload neither re- nor
    /// never-sends. Meaningless for a runner with no parent.
    Reported,
    /// The fork's initial state is durable, so it may run and the message
    /// seeded alongside it is drained.
    ForkSeeded,
    /// The detached seeding task could not seed this fork. Carries the reason
    /// verbatim, because that string is what the user is shown.
    ForkSeedFailed { error: String },
    /// A fork named itself.
    ForkTitled { name: String },
    /// A fork was removed, because someone asked. Never automatic.
    ForkDeleted,
    /// The runner was cancelled from outside — its parent's chain stopped, or
    /// the session is going away.
    Cancelled,
}

/// What a runner is, snapshotted into its `Created` event.
///
/// Self-containment is the point: replay reconstructs every runner without a
/// store, a preset edited mid-flight cannot change work already under way, and
/// a cold runner's settings are in its own record instead of resolved through
/// a recursive walk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerArgs {
    /// The session's conversation. Its settings live on the spec, which the
    /// same journal already carries — snapshotting a copy here would be two
    /// sources for one answer.
    Main,
    /// A delegated task.
    Sub {
        label: String,
        task: String,
        /// The plugin-declared agent type this subagent runs as, if any.
        agent_type: Option<String>,
        /// The caller's effective settings, snapshotted at spawn.
        settings: AgentSettings,
    },
    /// A branch of a conversation.
    Fork {
        /// The source agent's log seq at the branch point.
        source_seq: u64,
        mode: ForkMode,
        /// What the fork was created to do. Durable so a fork abandoned
        /// mid-seed is re-seeded with it, rather than coming back idle with
        /// nothing to answer.
        message: String,
    },
    /// A workflow run, carrying its whole resolved graph.
    Workflow { graph: WorkflowRunSpec },
}
