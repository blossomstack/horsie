//! The vocabulary a session is described in: what it is asked to do, and what
//! a reader is told about it.
//!
//! The other two thirds of the event-sourcing contract live in [`super::runner`]:
//! the events in `runner::event`, the folded state in `runner::state`. What
//! stays here is the command surface — the supervisor and the server-owned
//! tools compile against these — and the read projections a client consumes.
//!
//! Nothing here has behaviour beyond `Display`. The decisions live in the
//! runners, the fold in `runner::state`, and this file stays readable as a
//! description of the domain.

use crate::agent_loop::{AgentOutcome, AgentUsageSnapshot, UsageTotal};
/// Answering belongs to the agent that asked, so its vocabulary lives with the
/// agent. Re-exported because the session routes both and every caller reaches
/// them through it.
pub use crate::agent_loop::{AnswerError, AskAnswer};
use crate::sessions::UserMessageError;
use crate::sessions::forks::ForkMode;
use crate::sessions::spec::{AgentSettings, SessionStatus};
use horsie_actor::ReplyTo;
use horsie_models::hooks::HookRecord;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

use super::runner::ids::{AgentId, RunnerId};

/// Commands accepted by a [`SessionActor`](super::SessionActor).
#[derive(Serialize, Deserialize)]
pub enum SessionCommand {
    /// Getting and releasing this session's sandbox.
    Lifecycle(LifecycleCommand),
    /// The conversation: what a person sends and how a turn ends.
    Turn(TurnCommand),
    /// The workflow runs this session hosts.
    Run(RunCommand),
    /// The tree of delegated work.
    SubAgent(SubAgentCommand),
    /// Branching a conversation into a second one inside this session.
    Fork(ForkCommand),
    /// Questions answered without waking anything.
    Read(ReadCommand),
    /// What plugin hooks did, routed to the agent it happened to.
    Hooks(HookCommand),
    /// The session's own bookkeeping: its title, and preparation progress.
    Core(CoreCommand),
    /// Internal: an agent reported its terminal outcome. Top-level because it is
    /// the one command routed by *identity* rather than by variant — which agent
    /// sent it decides which runner answers.
    AgentOutcome(AgentOutcome),
}

/// Getting and releasing this session's sandbox.
#[derive(Serialize, Deserialize)]
pub enum LifecycleCommand {
    /// Build this session's runtime.
    ///
    /// Sent once, by the supervisor, as part of creating the session — and
    /// again by the session itself when it loads to find a create that the
    /// process died inside. It is idempotent against a runtime that already
    /// exists: a session that is past provisioning ignores it.
    Provision,
    /// Internal: the detached create has word of the runtime it asked for —
    /// "the machine is booting" — before it has an outcome. The vendor's own
    /// sentence, carried unedited, because it is what the user is shown.
    NarrateProvisioning { detail: String },
    /// Internal: the detached create finished. Carries the vendor's own error
    /// rather than a summary, because that string is what the user is shown.
    FinishProvisioning {
        error: Option<String>,
        terminal: bool,
    },
    /// The supervisor wants to unload this session. Answers `false` if a run
    /// started in the meantime, in which case nothing has changed and the idle
    /// clock simply restarts.
    PrepareOffload { reply: ReplyTo<bool> },
    /// Delete: cancel, tell the vendor, and stop.
    Delete { reply: ReplyTo<()> },
}

/// The conversation.
#[derive(Serialize, Deserialize)]
pub enum TurnCommand {
    /// A message for one of this session's agents. Always accepted: the agent
    /// queues it durably and answers it at its next turn, so there is no
    /// rejection path and no `409`.
    UserMessage {
        agent_id: Option<String>,
        text: String,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
    },
    /// Cancel one agent's turn in flight. Queued messages are *not* discarded —
    /// stop means "not this turn", not "throw away what I asked for".
    ///
    /// `Err` is for an id that names no agent here. An agent that is simply not
    /// working is `Ok`: nothing to stop is not a failure.
    Stop {
        agent_id: String,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Answer every question one agent is parked on, at once. Routed, not
    /// decided: the agent owns what it asked and validates the set.
    Answer {
        agent_id: Option<String>,
        answers: Vec<AskAnswer>,
        reply: ReplyTo<Result<(), AnswerError>>,
    },
}

/// The workflow runs this session hosts.
#[derive(Serialize, Deserialize)]
pub enum RunCommand {
    /// Let the boundary start whatever it wants started. Sent to a session at
    /// load so a pending run begins, and after a retry.
    Advance,
    /// Re-run one execution from the root run's log.
    RetryStep {
        index: u32,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Read this session's root workflow run, if it is one.
    State {
        reply: ReplyTo<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
    /// Recovery found a step the process died inside. Suspends that run, which
    /// is the state a retry can move.
    ReconcileInterrupted { run: RunnerId },
}

/// The tree of delegated work.
#[derive(Serialize, Deserialize)]
pub enum SubAgentCommand {
    /// The `spawn_agent` tool: start a subagent under `caller`.
    Spawn {
        caller: AgentId,
        label: String,
        task: String,
        /// A plugin-declared agent type, already checked against the catalogue
        /// by the tool that advertised it. The session journals the name and
        /// never resolves it.
        agent_type: Option<String>,
        reply: ReplyTo<Result<Uuid, String>>,
    },
    /// Internal: the spawn's `Created` write came back — only now does the
    /// child actor exist (persist-then-spawn). A failed write spawns nothing
    /// and the tool gets the error.
    FinishSpawn {
        id: AgentId,
        task: String,
        reply: ReplyTo<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
    /// The `subagent_status` tool: one node, or the caller's whole subtree.
    Status {
        caller: AgentId,
        id: Option<Uuid>,
        reply: ReplyTo<Result<String, String>>,
    },
    /// Internal: post-recovery reconciliation of subagents the process died
    /// under. Their runs are over; the parents are owed the failure like any
    /// other terminal result.
    Reconcile,
}

/// What accepting a message produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAccepted {
    pub message_id: String,
    /// The fork this message created. Absent for every ordinary message, which
    /// is what makes the field additive.
    pub forked_agent: Option<String>,
}

impl MessageAccepted {
    /// An ordinary message, which created no fork.
    #[must_use]
    pub fn queued(message_id: String) -> Self {
        Self {
            message_id,
            forked_agent: None,
        }
    }
}

/// Branching a conversation into a second one inside this session.
///
/// A fork is a conversation, not delegated work: nothing here reports a result
/// to anybody, and the only reply any of it carries is the fork's own id.
#[derive(Serialize, Deserialize)]
pub enum ForkCommand {
    /// `/fork` or `/summary-n-fork`: branch the conversation of `parent`, and
    /// queue `message` in the new fork so it has something to do when its seed
    /// lands.
    Create {
        parent: AgentId,
        mode: ForkMode,
        message: String,
        reply: ReplyTo<Result<Uuid, String>>,
    },
    /// Internal: the `Created` write came back — only now does the fork's
    /// actor exist (persist-then-spawn, exactly as a subagent spawn does).
    FinishCreate {
        id: AgentId,
        reply: ReplyTo<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
    /// Internal: the detached seeding task wrote the fork's initial state, so
    /// the fork may run and the message waiting in its queue is released.
    Seeded { id: AgentId },
    /// Internal: the source agent's `/summary-n-fork` turn produced the summary
    /// these forks were waiting on.
    Summarised {
        forks: Vec<Uuid>,
        result: Result<String, String>,
    },
    /// Internal: the detached seeding task could not. Carries the reason
    /// verbatim, because that string is what the user is shown.
    SeedFailed { id: AgentId, error: String },
    /// A fork's own `set_session_title` call. Renames the fork, never the
    /// session.
    SetTitle {
        id: AgentId,
        title: String,
        reply: ReplyTo<Result<String, String>>,
    },
    /// Someone asked for this fork to go. Nothing ever removes one on its own.
    Delete {
        id: AgentId,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Internal: recovery found forks a dead process abandoned mid-seed.
    ReseedInterrupted,
}

/// Questions answered from the resident actor's memory. None of these touches
/// the journal, so opening a session to look at it costs no sandbox.
#[derive(Serialize, Deserialize)]
pub enum ReadCommand {
    /// Read forward from a cursor in one of the session's agents: `agent_id`
    /// absent or `"main"` for the primary agent, otherwise an agent's uuid.
    /// `None` answers "no such agent".
    ReadLog {
        agent_id: Option<String>,
        after: Option<crate::agent_loop::Cursor>,
        reply: ReplyTo<Option<crate::agent_loop::ReadOutcome>>,
    },
    /// Read a window *backwards* from a cursor — scroll-back.
    PageLog {
        agent_id: Option<String>,
        before: Option<u64>,
        max: usize,
        reply: ReplyTo<Option<crate::agent_loop::LogPage>>,
    },
    /// Read one agent's document: what it is, what became of it, and its live
    /// values.
    Agent {
        agent_id: Option<String>,
        reply: ReplyTo<Option<AgentDetail>>,
    },
    /// Read this session's recovered state: status, usage, and its agents.
    Snapshot { reply: ReplyTo<SessionSnapshot> },
    /// Read this session's aggregated usage.
    UsageStats { reply: ReplyTo<SessionUsageStats> },
}

/// What plugin hooks did. Pure routing: nothing here is persisted by the
/// session, and nothing here changes state.
#[derive(Serialize, Deserialize)]
pub enum HookCommand {
    /// Plugin hooks ran against one agent's tool call. The session forwards to
    /// the agent whose transcript the records belong in.
    Ran {
        agent: AgentId,
        records: Vec<HookRecord>,
    },
    /// A `Stop` hook blocked, so the turn continues with `reason` as its input.
    ContinueAfterStop { agent: AgentId, reason: String },
    /// A hook set `continue: false`, so this agent stops where it is.
    ///
    /// The session is the only thing that can act on it: the runtime that ran
    /// the hook has no way to end a turn, and the agent is mid-call. What
    /// stopping *means* is the owning runner's decision.
    Halt { agent: AgentId, reason: String },
}

/// The session's own bookkeeping.
#[derive(Serialize, Deserialize)]
pub enum CoreCommand {
    /// Set the session title from the built-in title tool.
    SetTitle {
        title: String,
        reply: ReplyTo<Result<String, String>>,
    },
    /// A rename that happened elsewhere — a person renaming from the list, or
    /// the supervisor telling a resident session what it just recorded.
    TitleSet { name: String },
    /// Internal: write this session's spec into its own log.
    ///
    /// Self-sent by recovery when the log has no spec, which is true exactly
    /// twice — for a session being created, and for one whose process died
    /// between the supervisor recording it and this write.
    RecordSpec {
        spec: Box<crate::sessions::spec::SessionSpec>,
    },
    /// Internal: journal the session's root runner, once. Self-sent by `adopt`
    /// when the log holds none, so a session created a moment ago and one
    /// recovered from history take the same path — and a root run and a
    /// nested one are born through the same event.
    CreateRoot,
    /// Record one turn-preparation stage in `agent`'s log. Sent by the context
    /// provider as it assembles a turn.
    Progress {
        agent: AgentId,
        stage: String,
        detail: Option<String>,
    },
}

/// How a turn ended.
///
/// [`AgentOutcome`] minus the variants that are not a way a turn ends at all.
/// `TurnEnd::split` is the only conversion, and its match is exhaustive, so a
/// variant added to `AgentOutcome` fails to compile there — which is the right
/// place to decide whether it is a way a turn ends or another thing to bank.
pub(crate) enum TurnEnd {
    /// The agent produced its output — structured, or its final text.
    Concluded { output: serde_json::Value },
    /// The agent parked on one or more questions for the user.
    Asked,
    /// `terminal` means the agent's sandbox is gone and no later message can
    /// bring it back; anything else is a turn the user can retry.
    Failed { error: String, terminal: bool },
    /// The agent parked awaiting its timers, which sessions do not support.
    Parked,
    /// The process died inside the turn, and the agent said so at recovery.
    Interrupted,
}

impl TurnEnd {
    /// Separate the two things an outcome can be: a turn that ended, or a
    /// report to answer before routing.
    pub(crate) fn split(outcome: AgentOutcome) -> Result<(Uuid, Self), (Uuid, NotAnEnd)> {
        match outcome {
            AgentOutcome::Concluded { agent, output } => Ok((agent, Self::Concluded { output })),
            AgentOutcome::Asked { agent, .. } => Ok((agent, Self::Asked)),
            AgentOutcome::Parked { agent } => Ok((agent, Self::Parked)),
            AgentOutcome::Interrupted { agent } => Ok((agent, Self::Interrupted)),
            AgentOutcome::Failed {
                agent,
                error,
                terminal,
                ..
            } => Ok((agent, Self::Failed { error, terminal })),
            AgentOutcome::UsageRecorded { agent, usage_total } => {
                Err((agent, NotAnEnd::Usage(usage_total)))
            }
            AgentOutcome::Started { agent } => Err((agent, NotAnEnd::Started)),
            AgentOutcome::ForkSummary {
                agent,
                forks,
                result,
            } => Err((agent, NotAnEnd::ForkSummary { forks, result })),
        }
    }
}

/// An outcome that is not a way a turn ended.
pub(crate) enum NotAnEnd {
    /// A turn began. The agent decided it, so the session is being told.
    Started,
    /// Tokens to bank. The turn they were spent on is a separate report.
    Usage(UsageTotal),
    /// The summary a `/summary-n-fork` turn was asked for.
    ForkSummary {
        forks: Vec<Uuid>,
        result: Result<String, String>,
    },
}

/// One agent's own usage/context-size snapshot, labeled with the model it ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUsageEntry {
    pub model: String,
    pub snapshot: AgentUsageSnapshot,
}

/// What a reader needs to know about a session, answered by the actor that owns
/// it. The whole live half of `GET /api/sessions/:id`, so that document is one
/// ask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub status: SessionStatus,
    /// Tokens summed across every agent this session hosts.
    pub usage_total: UsageTotal,
    /// Every agent this session hosts, in the vocabulary of
    /// `/sessions/:id/agents/:agent_id`.
    pub agents: Vec<AgentEntry>,
}

/// What became of one of a session's agents.
///
/// One vocabulary for different underlying facts — a conversation's main agent
/// takes its state from the session, a step from its run's log, a subagent
/// from its own record — because to a reader they are one question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// The session's runtime is still being built. Nothing has run yet.
    Provisioning,
    Running,
    /// Loaded and not working — where a conversation's agent rests between
    /// turns, and the one state that is not an ending.
    Idle,
    /// Parked on a question, waiting for an answer.
    AwaitingInput,
    /// Ran to a result. Only a subagent or a step reaches it: a conversation is
    /// never *done*.
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    /// The name a client reads this status by.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::AwaitingInput => "awaiting_input",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One agent a session hosts: which agent it is, what became of it, and when.
///
/// What it *said* is not here — a transcript is read from the agent's own log,
/// through `/history`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    /// `"main"`, or the agent's uuid. The vocabulary every agent-scoped route
    /// speaks.
    pub id: String,
    /// The agent that spawned this one. Absent for a main agent, for a step —
    /// which the definition chose, not an agent — and for a subagent rooted
    /// directly on a conversation or a step.
    pub parent: Option<Uuid>,
    /// A subagent's label, or the name of the step this agent is one execution
    /// of. Absent for a main agent, which is not one of several.
    pub label: Option<String>,
    pub depth: u32,
    /// The plugin-declared agent type a typed subagent runs as.
    pub agent_type: Option<String>,
    pub status: AgentStatus,
    pub error: Option<String>,
    /// When this agent started and when it reached its result. Zero for a main
    /// agent, and zero for `ended_at_ms` while an agent is still running.
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
}

/// Everything a session knows about one of its agents: its entry in the roster,
/// what it ran under, what it produced, and its live values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetail {
    pub entry: AgentEntry,
    /// The settings this agent runs under, resolved by the runner that owns
    /// it: a step's own preset, a subagent's snapshot, or the session's main
    /// settings.
    pub settings: AgentSettings,
    /// What a subagent was asked to do. A main agent is asked things one turn
    /// at a time, and a step's brief is its definition's.
    pub task: Option<String>,
    /// Its terminal result, once it has one. A step's structured output is
    /// rendered the same way a subagent's report is, because a reader wants the
    /// same thing from both.
    pub output: Option<String>,
    /// Read from the agent itself: its task list, its usage, and where in its
    /// log those were taken.
    pub state: crate::agent_loop::AgentStateView,
}

/// A session's aggregated usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUsageStats {
    pub session_total: UsageTotal,
    /// The main agent's usage, for the session kinds that have one. A run has
    /// no main agent, so it is `None` there.
    pub main_agent: Option<AgentUsageEntry>,
    /// Every agent's banked total, keyed as `agent_usage` keys it: `"main"`
    /// for the primary agent, the agent's uuid otherwise.
    pub agents: BTreeMap<String, UsageTotal>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Every state has a spelling, and one spelling: a `_ =>` arm is how the
    /// documents that carry a status came to disagree about what a failed
    /// provision looks like.
    #[test]
    fn every_agent_status_has_one_spelling() {
        for (status, expected) in [
            (AgentStatus::Provisioning, "provisioning"),
            (AgentStatus::Running, "running"),
            (AgentStatus::Idle, "idle"),
            (AgentStatus::AwaitingInput, "awaiting_input"),
            (AgentStatus::Completed, "completed"),
            (AgentStatus::Failed, "failed"),
            (AgentStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status.as_wire(), expected);
        }
    }
}
