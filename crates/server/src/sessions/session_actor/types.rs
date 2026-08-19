//! The vocabulary a session is described in: what it is asked to do, what it
//! records having done, and what it knows as a result.
//!
//! Split out from the actor for one reason — every component names these, and
//! none of them names the actor. Keeping the commands, the events and the folded
//! state together also puts the three halves of the event-sourcing contract in
//! one place: a command decides, an event records, the state is their fold.
//!
//! Nothing here has behaviour beyond `Display`. The decisions live in the
//! components, the fold lives in [`SessionActor::apply_event`](super::SessionActor::apply_event),
//! and this file stays readable as a description of the domain.

use crate::agent_loop::{AgentOutcome, AgentUsageSnapshot, UsageTotal};
/// Answering belongs to the agent that asked, so its vocabulary lives with the
/// agent. Re-exported because the session routes both and every caller reaches
/// them through it.
pub use crate::agent_loop::{AnswerError, AskAnswer};
use crate::sessions::{
    UserMessageError,
    runners::ids::AgentId,
    spec::{AgentSettings, SessionStatus},
};
use horsie_actor::ReplyTo;
use horsie_models::hooks::HookRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Commands accepted by a [`SessionActor`].
///
/// Two kinds only: addressed to an agent, or the session's own. There is no
/// third — the six command groups this replaces were six ways of asking "which
/// of my four kinds of agent is this", and that question is now one lookup in
/// `SessionState::agents`.
#[derive(Serialize, Deserialize)]
pub enum SessionCommand {
    /// A capability asking for a child runner.
    ///
    /// The id is the capability's, not the session's, so the event it journaled
    /// and the command it sent name the same child — which is what lets the
    /// session dedupe a re-ask after a crash rather than spawning twice.
    StartRunner {
        id: crate::sessions::runners::ids::RunnerId,
        kind: crate::sessions::runners::ids::RunnerKind,
        args: Box<crate::sessions::runners::action::RunnerArgs>,
        /// The agent that asked. The session resolves its runner; the caller
        /// cannot see the tree and must not guess at it.
        parent: AgentId,
        reply: ReplyTo<Result<(), String>>,
    },
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
    /// An agent that is simply not working is `Ok`: nothing to stop is not a
    /// failure, and a client racing a turn's own end would otherwise see an
    /// error for winning the race.
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
    /// A person removed a runner — a fork, today. Nothing removes one on its
    /// own.
    ///
    /// Named by its **agent**, because that is the id a client holds: a fork's
    /// row in the session list carries the conversation's agent, and so does
    /// the acknowledgement that answered `/fork`. A runner id never leaves the
    /// session, so a client could not name one.
    DeleteRunner {
        agent: crate::sessions::runners::ids::AgentId,
        reply: ReplyTo<Result<(), String>>,
    },
    /// A person asked a workflow step to run again.
    ///
    /// Not part of the agent lifecycle: a retry comes from a person, not from
    /// an agent, which is why it is the session's command and not an outcome.
    RetryStep {
        index: usize,
        reply: ReplyTo<Result<(), String>>,
    },
    /// The supervisor wants to unload this session. Answers `false` if work
    /// started in the meantime, in which case the idle clock simply restarts.
    PrepareOffload { reply: ReplyTo<bool> },
    /// Delete: cancel, tell the vendor, and stop.
    Delete { reply: ReplyTo<()> },
    /// Questions answered from the resident actor's memory.
    Read(ReadCommand),
    /// What plugin hooks did, routed to the agent it happened to.
    Hooks(HookCommand),
    /// The session's own bookkeeping: its title, its spec, its boundary.
    Core(CoreCommand),
    /// Internal: an agent reported its terminal outcome. Top-level because it is
    /// the one command routed by *identity* — which agent sent it decides which
    /// runner answers.
    AgentOutcome(AgentOutcome),
}

/// What accepting a message produced.
///
/// More than the message's id because one message can do more than queue
/// itself: `/fork` creates a conversation, and the client has to be told which
/// one to open. A field rather than a second endpoint, so every client that can
/// send a message can fork without learning a new call.
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

/// Questions answered from the resident actor's memory. None of these touches
/// the journal, so opening a session to look at it costs no sandbox.
#[derive(Serialize, Deserialize)]
pub enum ReadCommand {
    /// Read forward from a cursor in one of the session's agents: `agent_id`
    /// absent or `"main"` for the primary agent, otherwise a subagent id.
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
    /// values. `agent_id` absent or `"main"` for the primary agent — which, on
    /// a run, is the step in flight. `None` answers "no such agent".
    Agent {
        agent_id: Option<String>,
        reply: ReplyTo<Option<AgentDetail>>,
    },
    /// Read this session's recovered state: status, usage, and its agents.
    Snapshot { reply: ReplyTo<SessionSnapshot> },
    /// Read this session's aggregated usage.
    UsageStats { reply: ReplyTo<SessionUsageStats> },
    /// Read this session's workflow run, if it is one.
    ///
    /// A read rather than a `Run` command: a run's log is journal data on the
    /// runner, and asking for it wakes nothing.
    RunState {
        reply: ReplyTo<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
}

/// What plugin hooks did. Pure routing: nothing here is persisted by the
/// session, and nothing here changes state.
#[derive(Serialize, Deserialize)]
pub enum HookCommand {
    /// Plugin hooks ran against one agent's tool call. The session forwards to
    /// the agent whose transcript the records belong in. Carries no reply
    /// because nothing waits on it.
    Ran {
        key: AgentId,
        records: Vec<HookRecord>,
    },
    /// A `Stop` hook blocked, so the turn continues with `reason` as its input.
    ///
    /// Routed through the session for the same reason `Ran` is: the sink is
    /// built before its `AgentActor` is spawned, so it holds a key rather than
    /// an `ActorRef`.
    ContinueAfterStop { key: AgentId, reason: String },
    /// A hook set `continue: false`, so this agent stops where it is.
    ///
    /// The session is the only thing that can act on it: the runtime that ran
    /// the hook has no way to end a turn, and the agent is mid-call. What
    /// stopping *means* is per key — a turn boundary for the main agent, a
    /// failed node for a subagent, a failed step for a step.
    Halt { key: AgentId, reason: String },
}

/// The session's own bookkeeping.
#[derive(Serialize, Deserialize)]
pub enum CoreCommand {
    /// Set the session title from the built-in title tool.
    SetTitle {
        /// The agent that called the tool, in the runners' flat id space.
        /// The old handler renames the session regardless of who asked; the
        /// runner routes on it.
        agent: AgentId,
        title: String,
        reply: ReplyTo<Result<String, String>>,
    },
    /// A rename that happened elsewhere — a person renaming from the list, or
    /// the supervisor telling a resident session what it just recorded.
    ///
    /// Journals it here too. The supervisor's copy is what the session list
    /// shows; this one is what the running session reads, and a session's own
    /// journal is the truth about that session.
    TitleSet { name: String },
    /// Internal: the detached create has news about the sandbox.
    ///
    /// Addressed to the runtime runner that asked, because a session could hold
    /// more than one — the runner shape is what makes that expressible, even
    /// though one is still the rule.
    RuntimeEvent {
        runner: crate::sessions::runners::ids::RunnerId,
        event: crate::sessions::runners::runtime::Event,
    },
    /// Internal: drive the boundary, persisting whatever it starts.
    ///
    /// Self-sent once at load, because that is the one boundary nothing else
    /// reaches. `Runner::actions` is idempotent, so this is the same call every
    /// other boundary makes — but recovery may not persist, and `RecordSpec` is
    /// already returning an effect of its own, so the drive arrives as an
    /// ordinary command down the path a live one would take.
    ///
    /// Without it a recovered session starts nothing: every runner asks for its
    /// first agent and no one is listening.
    Advance,
    /// Internal: write this session's spec into its own log.
    ///
    /// Self-sent by recovery when the log has no spec, which is true exactly
    /// twice — for a session being created, and for one whose process died
    /// between the supervisor recording it and this write. Both take the same
    /// path, so the crash case is not a special case.
    RecordSpec {
        spec: Box<crate::sessions::spec::SessionSpec>,
    },
    /// Record one turn-preparation stage in `key`'s log. Sent by the context
    /// provider as it assembles a turn.
    Progress {
        key: AgentId,
        stage: String,
        detail: Option<String>,
    },
}

/// How a turn ended.
///
/// [`AgentOutcome`] minus the two variants that are not a way a turn ends at
/// all: `UsageRecorded`, banked identically for every agent a session hosts,
/// and `Started`, which reports a turn *beginning*. `on_agent_outcome` answers
/// both once before routing. Narrowing them away here is what lets the three
/// components that handle an outcome match exhaustively on the five real cases,
/// instead of each carrying an `unreachable!` for a variant it can never be
/// handed.
///
/// It is a second vocabulary for something `crate::agent_loop` already names, and
/// that is the deliberate cost. `AgentOutcome` is the *protocol* between an
/// agent and whatever owns it, and horsie has owners that are not sessions; a
/// session's components want the smaller thing. [`TurnEnd::split`] is the only
/// conversion, and its match is exhaustive, so a variant added to `AgentOutcome`
/// fails to compile there — which is the right place to decide whether it is a
/// way a turn ends or another thing to bank.
/// The one conversion from the agent protocol into the vocabulary a runner
/// speaks. Its match is exhaustive, so a variant added to `AgentOutcome` fails
/// to compile here — the right place to decide whether it is a way a turn ends
/// or another thing to bank.
pub(super) struct TurnEnd;

impl TurnEnd {
    /// Separate the two things an outcome can be: a turn that ended, or usage to
    /// bank. Both carry the agent that reported them.
    ///
    /// A `Result` rather than an `Option` so the caller cannot reach the routing
    /// path with a non-ending outcome still in hand — the narrowing is total,
    /// and nothing below it needs a case for a variant that never arrives.
    pub(super) fn split(
        outcome: AgentOutcome,
    ) -> Result<(Uuid, crate::sessions::runners::TurnEnd), (Uuid, NotAnEnd)> {
        use crate::sessions::runners::TurnEnd as End;
        match outcome {
            AgentOutcome::Concluded { agent, output } => Ok((agent, End::Concluded { output })),
            AgentOutcome::Asked { agent, .. } => Ok((agent, End::Asked)),
            AgentOutcome::Parked { agent } => Ok((agent, End::Parked)),
            AgentOutcome::Interrupted { agent } => Ok((agent, End::Interrupted)),
            AgentOutcome::Failed {
                agent,
                error,
                terminal,
                ..
            } => Ok((agent, End::Failed { error, terminal })),
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
pub(super) enum NotAnEnd {
    /// A turn began. The agent decided it, so the session is being told.
    Started,
    /// Tokens to bank. The turn they were spent on is a separate report.
    Usage(UsageTotal),
    /// The summary a `/summary-n-fork` turn was asked for. Nothing about how
    /// that turn ended — it is still running, or it ended some other way.
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
/// it. Every field is recovered from the journal, so an unloaded session gives
/// the same answers as a loaded one — it just has to be loaded to give them.
///
/// The whole live half of `GET /api/sessions/:id`, so that document is one ask.
/// It used to be four — status here, usage, the subagent tree and the run log
/// each separately — all four served by this same actor, and reassembled above
/// it by an HTTP handler that had to know what kind of session this was to do
/// it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub status: SessionStatus,
    /// Tokens summed across every agent this session hosts. The per-agent
    /// breakdown is [`SessionUsageStats`], which only the run graph needs.
    pub usage_total: UsageTotal,
    /// Every agent this session hosts, in the vocabulary of
    /// `/sessions/:id/agents/:agent_id`.
    pub agents: Vec<AgentEntry>,
}

/// What became of one of a session's agents.
///
/// One vocabulary for three different underlying facts — a conversation's main
/// agent takes its state from the session, a run's step agent from the run log,
/// a subagent from the tree — because to a reader they are one question. Asked
/// three times above the actor, they became three projections that disagreed: a
/// concluded step answered `running` for ever, and a session whose runtime
/// never built answered `idle` beside a status that said `failed`.
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
    ///
    /// Here rather than beside a wire type because two places now project an
    /// `AgentStatus` outward — the HTTP layer and the supervisor's global feed —
    /// and a second copy of this mapping is a second chance for them to
    /// disagree about what `awaiting_input` is called.
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
    /// directly on either.
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
    /// agent — nothing spawned it, and it is as old as the session, whose
    /// `created_at` is on the same document — and zero for `ended_at_ms` while
    /// an agent is still running.
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
}

/// Everything a session knows about one of its agents: its entry in the roster,
/// what it ran under, what it produced, and its live values.
///
/// One answer rather than a tree read, a run read and a state read stitched
/// together by the caller — which is what left a step's document reporting the
/// session's model and a permanent `running`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetail {
    pub entry: AgentEntry,
    /// The settings this agent runs under, resolved from what this session is
    /// and where the agent sits in it: a step's own preset, a subagent's
    /// inherited tree root, or the session's main settings.
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
    /// Every agent's banked total, keyed as `agent_usage` keys it: `"main"` for
    /// the primary agent, the agent's uuid for a subagent or a workflow step.
    ///
    /// Here so a run can report per-step tokens: a step's key *is* its
    /// `StepRun.agent`, so the run graph only needs the map, not a read per
    /// step. Usage banks at turn end, so a step in flight reads zero — the same
    /// as `session_total`.
    pub agents: HashMap<String, UsageTotal>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Every state has a spelling, and one spelling: a `_ =>` arm is how the
    /// documents that carry a status came to disagree about what a failed
    /// provision looks like. Three projections read this now — the session
    /// list, an agent document, and the global feed's fork rows — so the
    /// mapping is tested where it lives rather than at one of them.
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
