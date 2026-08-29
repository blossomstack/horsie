//! One agent: its session, the turn it is running, and the log both leave
//! behind.
//!
//! An agent is event-sourced, so a restarted process recovers an in-flight
//! session from the journal and continues. Everything durable about it is
//! [`AgentState`], everything it can be told is [`AgentCommand`], and every
//! change is an [`AgentDomainEvent`] that was journaled before it was believed.
//!
//! What is left in this file is the actor itself: its in-memory bookkeeping,
//! the dispatch that hands each command to the module that owns it, and the
//! fold that routes each event the same way. Everything else lives beside it —
//! [`queue`] the promises it has accepted, [`run`] the turn in flight,
//! [`conclude`] what ended it, [`compaction`] where the prompt starts,
//! [`timers`] and [`task_list`] the side registers, [`log`] what others write
//! into its transcript, [`seed`] branching, [`reads`] the questions
//! that wake nothing, [`repair`] what a crash left dangling, and [`sink`] the
//! path an event takes to the journal — over the vocabulary in [`types`] and
//! the state in [`state`], to the shape in [`component`].
//!
//! Two things deliberately do not happen on this mailbox. No provider call and
//! no toolbox build: those run on a spawned task, so a thirty-second MCP
//! connect cannot block a cancel. And no decision about whether this agent
//! exists: residency belongs to whoever spawned it.

mod compaction;
mod component;
mod conclude;
mod log;
mod queue;
mod reads;
mod repair;
mod run;
mod seed;
mod sink;
mod state;
mod task_list;
#[cfg(test)]
pub(super) mod testing;
mod timers;
mod types;

pub use reads::{ReadOutcome, ReplayWindow};
pub use state::{AgentState, UsageTotal, hook_entry, hook_entry_id};
pub use types::*;

use compaction::{COMPACT_AT_PERCENT, COMPACT_RETAIN_PERCENT, Compaction};
use component::Component;
use log::LogWrites;
use queue::Queue;
use reads::Reads;
use repair::{
    missing_tool_results, parked_call_ids, repair_unanswered_tool_calls,
    repair_unanswered_tool_calls_except,
};
use run::{Run, RunHandle, RunOutcome, RunReport, SeedSummary};
use seed::Seeding;
use sink::{CapturingSink, PersistSink, coarse_appends_an_entry, coarse_event};
use state::new_message_id;
use task_list::{TaskListToolbox, TaskLists};
use timers::{TimerToolbox, Timers};

use crate::agent_loop::context::AgentRuntimeContext;
use async_trait::async_trait;
use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor, PersistenceId, ReplyTo};
use std::sync::Arc;

/// Events an agent may journal between snapshots before the next turn boundary
/// takes one.
///
/// An agent's state *is* its transcript, so a snapshot costs O(transcript) to
/// write — snapshotting every turn would be quadratic over a session. This
/// trades a bounded replay on recovery for a bounded write amplification.
const SNAPSHOT_EVERY_EVENTS: u64 = 200;

/// Observer of an agent's durable history, notified once per event that is both
/// journaled and folded into state.
///
/// This is how a live stream learns what happened without reading the journal:
/// the actor is the only thing that touches its own log, and this is the seam
/// it publishes through. Implementations must not block — they run on the
/// actor's mailbox — and must treat delivery as best-effort.
pub trait AgentObserver: Send + Sync {
    /// `state` is the state *after* `event` was folded, so an observer that
    /// needs the resulting message can read `state.messages.last()` rather
    /// than re-deriving it from the event.
    fn publish(&self, event: &AgentDomainEvent, state: &AgentState);
}

/// An agent run, modelled as an event-sourced actor. Each
/// `Run`/`InjectToolResult` drives a background `horsie_agentcore::Agent`
/// loop; coarse events are journaled incrementally so a crashed session
/// recovers its session and continues.
pub struct AgentActor {
    ctx: AgentRuntimeContext,
    params: AgentParams,
    running: Option<RunHandle>,
    /// Where durable history is published, when anyone is listening. `None` for
    /// workflow agents, which have no live stream.
    observer: Option<Arc<dyn AgentObserver>>,
    /// Events journaled since a snapshot was last *requested*. Counting
    /// requests rather than confirmed writes means a failed snapshot simply
    /// waits another interval, which is the right instinct for an
    /// optimization: retrying hard against a failing journal helps nobody.
    events_since_snapshot: u64,
    /// Id of the next run to start. Monotonic for this actor's loaded lifetime,
    /// which is all the fence needs — a report can only be stale within it.
    next_run_id: u64,
    /// Whether this agent's session has a runtime to run on.
    ///
    /// Seeded at spawn and moved by the `Runtime` lifecycle records the owner
    /// already sends — so nothing carries this fact but the log entry a reader
    /// sees anyway. In-memory on purpose: an agent that does not exist cannot
    /// be holding a turn, and one that is respawned is built with the answer
    /// that was true at the time.
    ready: bool,
    /// What the next run should tell the provider about tool use. Taken when a
    /// run starts, so it applies to exactly one turn. Set only when re-running
    /// a turn that ended without the result it owed — see the nudge in
    /// `handle_finished`. In-memory: a process that died mid-nudge starts the
    /// turn again from the queue, and a fresh attempt is the right default.
    pending_tool_choice: Option<horsie_agentcore::ToolChoice>,
    /// A prepare step is in flight. Gates a second `Resume` exactly as
    /// `running` does: between `Resume` and `StartPrepared` no run exists yet,
    /// so `running` alone would let two turns through and land two runs on one
    /// journal.
    preparing: bool,
    /// Whether this agent load has fired its start hook. Deliberately **not**
    /// journaled — a rehydrated agent fires again, which is precisely what
    /// `source: "resume"` means.
    start_hook_fired: bool,
    /// Callers waiting to hear that the in-flight run has terminated (see
    /// [`AgentCommand::Cancel`]). Drained the moment `RunFinished` is handled —
    /// the run task sends that as its very last act, so every journal write it
    /// could make has already happened by then.
    cancel_acks: Vec<ReplyTo<()>>,
    /// Chunks of the message currently being written, since the newest log
    /// entry. Cleared whenever an entry lands, because the entry supersedes
    /// them.
    ///
    /// Deliberately not journaled and not part of the fold. A delta's useful
    /// life ends when the finished message arrives — under a second — and
    /// persisting one would put a write transaction on the critical path of
    /// every token for data nothing will ever read again.
    deltas: Vec<String>,
    /// A counter, bumped whenever this agent moves, for readers to wait on.
    /// Only the fact that something happened travels through here; what
    /// happened is read from state, which is what leaves nothing to overflow.
    ///
    /// Held behind an `Arc` because the *owner* is whoever outlives this actor
    /// — for a session agent that is the supervisor, so an idle offload does
    /// not disconnect a reader and send it round the reconnect-reload loop. A
    /// standalone agent owns its own and the distinction costs nothing.
    revision: std::sync::Arc<tokio::sync::watch::Sender<crate::sessions::Revision>>,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        let revision = ctx.revision.clone();
        let ready = ctx.ready;
        Self {
            ctx,
            params,
            running: None,
            observer: None,
            events_since_snapshot: 0,
            next_run_id: 0,
            ready,
            pending_tool_choice: None,
            preparing: false,
            start_hook_fired: false,
            cancel_acks: Vec::new(),
            deltas: Vec::new(),
            revision,
        }
    }

    /// Announce that this agent has moved, waking every reader waiting on it.
    ///
    /// Called after anything a reader could want to see — a new entry, another
    /// delta, a cleared delta buffer. Announcing twice for one change is
    /// harmless: a reader that finds nothing new simply waits again.
    fn publish_revision(&self) {
        self.revision.send_modify(|r| *r += 1);
    }

    /// Same actor, publishing its durable history to `observer` — what a
    /// session agent needs and a workflow agent does not.
    pub fn with_observer(
        ctx: AgentRuntimeContext,
        params: AgentParams,
        observer: Arc<dyn AgentObserver>,
    ) -> Self {
        Self {
            observer: Some(observer),
            ..Self::new(ctx, params)
        }
    }

    /// Snapshot at a turn boundary, but only once enough events have accrued.
    ///
    /// Without this an agent that only ever converses — no ask, no park, no
    /// cancel — would never snapshot, and every recovery would stay a full
    /// replay of the whole transcript.
    /// Counting requests rather than confirmed writes means a failed snapshot
    /// simply waits another interval, which is the right instinct for an
    /// optimization: retrying hard against a failing journal helps nobody.
    fn snapshot_due(&mut self) -> bool {
        if self.events_since_snapshot < SNAPSHOT_EVERY_EVENTS {
            return false;
        }
        self.events_since_snapshot = 0;
        true
    }

    /// Persist `events`, taking a snapshot too if enough have accrued. The
    /// shape of every turn boundary that also ends a run.
    fn persist_maybe_snapshot(
        &mut self,
        events: Vec<AgentDomainEvent>,
    ) -> CommandEffect<AgentDomainEvent> {
        let effect = CommandEffect::persist(events);
        match self.snapshot_due() {
            true => effect.and_snapshot(),
            false => effect,
        }
    }

    /// The journal identity of an agent: kind `"agent"`, id = the agent's own
    /// [`AgentRuntimeContext::journal_id`]. Centralizes the kind so the
    /// workflow (e.g. sub session) and the actor agree.
    pub fn persistence_id_for(journal_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", journal_id.to_string())
    }

    /// Refuse to begin a turn while one is already in flight — running, or
    /// still in its prepare step.
    ///
    /// `start_run` overwrites `self.running` with a fresh token, so a second
    /// start orphans the first run's cancel token and leaves two background
    /// loops persisting interleaved events into one journal — including two
    /// `tool_result`s for the same `tool_call_id`, which makes the provider
    /// 400 on every later turn (#61 item 3). Callers gate on session status,
    /// but that is a different actor's state; this is the invariant enforced
    /// where it lives.
    ///
    /// `preparing` is part of it because a turn between the drain decision and
    /// `StartPrepared` has no run yet: gating on `running` alone would let a
    /// second drain straight through into the same collision.
    fn busy(&self) -> bool {
        self.running.is_some() || self.preparing
    }
}

#[async_trait]
impl EventSourcedActor for AgentActor {
    type Command = AgentCommand;
    type Event = AgentDomainEvent;
    type State = AgentState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.ctx.journal_id)
    }

    fn initial_state() -> AgentState {
        AgentState::default()
    }

    /// Fold one event into state by routing it to the module that owns it.
    ///
    /// Exhaustive on purpose, and the only place that is: an event added later
    /// fails to compile *here*, where it has to be classified, rather than
    /// silently reaching the wrong fold.
    fn apply_event(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
        match event {
            e @ AgentDomainEvent::Seeded { .. } => Seeding::apply(&mut state, e),
            e @ (AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::Received { .. }
            | AgentDomainEvent::TurnBegan { .. }
            | AgentDomainEvent::AskRecorded { .. }
            | AgentDomainEvent::Parked { .. }) => Queue::apply(&mut state, e),
            e @ (AgentDomainEvent::MessageComplete { .. }
            | AgentDomainEvent::MessageAborted { .. }
            | AgentDomainEvent::ToolComplete { .. }
            | AgentDomainEvent::RunComplete { .. }
            | AgentDomainEvent::RunAborted { .. }
            | AgentDomainEvent::RunCancelled { .. }
            | AgentDomainEvent::Nudged { .. }) => Run::apply(&mut state, e),
            e @ (AgentDomainEvent::HookRan { .. } | AgentDomainEvent::LifecycleRecorded { .. }) => {
                LogWrites::apply(&mut state, e)
            }
            e @ AgentDomainEvent::Compacted { .. } => Compaction::apply(&mut state, e),
            e @ (AgentDomainEvent::TimerArmed { .. }
            | AgentDomainEvent::TimerCancelled { .. }
            | AgentDomainEvent::TimerFired { .. }) => Timers::apply(&mut state, e),
            e @ AgentDomainEvent::TaskListChanged { .. } => TaskLists::apply(&mut state, e),
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &AgentState,
        cmd: AgentCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            AgentCommand::Queue(c) => Queue::handle(self, state, c, ctx).await,
            AgentCommand::Run(c) => Run::handle(self, state, c, ctx).await,
            AgentCommand::Timer(c) => Timers::handle(self, state, c, ctx).await,
            AgentCommand::TaskList(c) => TaskLists::handle(self, state, c, ctx).await,
            AgentCommand::Read(c) => Reads::handle(self, state, c, ctx).await,
            AgentCommand::Log(c) => LogWrites::handle(self, state, c, ctx).await,
            AgentCommand::Seed(c) => Seeding::handle(self, state, c, ctx).await,
            // Inlined rather than given a module of its own: stopping is the
            // actor's whole answer, and there is no state, no event and no
            // second command to keep it company.
            AgentCommand::Core(CoreCommand::Shutdown) => CommandEffect::stop(),
        }
    }

    /// Publish what just became durable. This is the whole reason a live
    /// stream no longer reads the journal: by the time this runs the events
    /// are written and folded, so `state` already contains the messages they
    /// appended.
    async fn on_events_persisted(&mut self, events: &[AgentDomainEvent], state: &AgentState) {
        self.events_since_snapshot = self
            .events_since_snapshot
            .saturating_add(events.len() as u64);
        // An entry supersedes every chunk that preceded it — the finished
        // message says everything they were building towards — so the deltas
        // are dropped the moment one lands. This is also what keeps the delta
        // sub-sequence short and restartable: it counts within one entry, never
        // across the session.
        if events.iter().any(coarse_appends_an_entry) {
            self.deltas.clear();
        }
        self.publish_revision();
        let Some(observer) = &self.observer else {
            return;
        };
        for event in events {
            observer.publish(event, state);
        }
    }

    /// Repair whatever the crash left half-done, before the first live command.
    ///
    /// Each module is asked in turn and does its own repair, because they are
    /// not the same kind of work: a timer re-arms itself, while an interrupted
    /// turn has to be reported to the *parent* — which must happen from this
    /// hook so the report is ordered ahead of anything queued while the actor
    /// was loading. Nothing here persists; anything that needs to journal
    /// arrives as an ordinary command.
    async fn on_recovery_complete(
        &mut self,
        state: &AgentState,
        ctx: &mut ActorContext<AgentCommand>,
    ) {
        // Announce where this incarnation starts. The channel outlives the
        // actor, so after an idle offload it still holds the position from
        // before — republishing costs nothing and keeps a reader that has been
        // waiting through the offload from having to guess.
        self.publish_revision();
        Timers::on_load(self, state, ctx).await;
        Run::on_load(self, state, ctx).await;
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
    use super::*;
    use crate::agent_loop::agent_actor::testing::*;
    use crate::agent_loop::context::{AgentOutcome, AgentOutcomeSink};
    /// Without a turn-boundary snapshot an agent that only converses — no ask,
    /// no park, no cancel — never snapshots, and every recovery stays a full
    /// replay of the whole transcript.
    #[test]
    fn a_turn_boundary_snapshots_only_once_enough_events_have_accrued() {
        let session_id = uuid::Uuid::new_v4();
        let ctx = AgentRuntimeContext {
            artifacts: None,
            context_provider: Arc::new(StubContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(StubParent),
            journal_id: session_id,
            ready: true,
        };
        let mut agent = AgentActor::new(ctx, AgentParams::from_def(&def_fixture()));

        assert!(
            !agent.snapshot_due(),
            "a fresh agent has nothing worth snapshotting"
        );

        agent.events_since_snapshot = SNAPSHOT_EVERY_EVENTS - 1;
        assert!(
            !agent.snapshot_due(),
            "one event short of the interval must not snapshot"
        );

        agent.events_since_snapshot = SNAPSHOT_EVERY_EVENTS;
        assert!(
            agent.snapshot_due(),
            "reaching the interval snapshots at the turn boundary"
        );
        assert_eq!(
            agent.events_since_snapshot, 0,
            "the counter resets on request, so a failed write waits one interval"
        );
        assert!(
            !agent.snapshot_due(),
            "and the very next turn does not snapshot again"
        );
    }

    /// The observer replaces journal replay: it must see every durable event,
    /// after the fold, with the resulting message already in state.
    #[tokio::test]
    async fn an_observer_sees_durable_appends_with_folded_state() {
        use crate::agent_loop::{ContextError, ContextProvider, Contexts};
        use horsie_actor::{ActorSystem, InMemoryJournal, Journal};

        struct NoContext;
        #[async_trait]
        impl ContextProvider for NoContext {
            async fn provide(&self) -> Result<Contexts, ContextError> {
                Err(ContextError::retryable("no context"))
            }
        }
        struct DeafParent;
        #[async_trait]
        impl AgentOutcomeSink for DeafParent {
            async fn deliver(&self, _: AgentOutcome) {}
        }

        /// Records `(event, message-count-at-publish)` so the test can prove
        /// the fold already happened when the observer ran.
        #[derive(Default)]
        struct Recorder {
            seen: std::sync::Mutex<Vec<(String, usize)>>,
        }
        impl AgentObserver for Recorder {
            fn publish(&self, event: &AgentDomainEvent, state: &AgentState) {
                let label = match event {
                    AgentDomainEvent::InputMessage { message } => {
                        format!("input:{}", message.id)
                    }
                    AgentDomainEvent::MessageComplete { message } => {
                        format!("complete:{}", message.id)
                    }
                    other => format!("other:{other:?}"),
                };
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((label, state.log.len()));
            }
        }

        let session_id = uuid::Uuid::new_v4();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let recorder = Arc::new(Recorder::default());
        let ctx = AgentRuntimeContext {
            artifacts: None,
            context_provider: Arc::new(NoContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(DeafParent),
            journal_id: session_id,
            ready: true,
        };
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::with_observer(ctx, AgentParams::from_def(&def_fixture()), recorder.clone()),
        );

        let one = user_msg("one");
        let two = user_msg("two");
        let (ack, ack_rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Run(RunCommand::PersistProgress {
                events: vec![
                    AgentDomainEvent::InputMessage {
                        message: one.clone(),
                    },
                    AgentDomainEvent::MessageComplete {
                        message: two.clone(),
                    },
                ],
                ack: ReplyTo::from_sender(ack),
            }))
            .await
            .unwrap();
        ack_rx.await.unwrap().unwrap();

        let seen = recorder.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (format!("input:{}", one.id), 2),
                (format!("complete:{}", two.id), 2),
            ],
            "both events publish once, and state is already folded when they do"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod interruption_tests {
    //! What an agent says about the turn its process died inside.
    //!
    //! The fact lives here and nowhere else. An owner holds a *status*, which
    //! cannot say which turn produced it — so recovery used to ask "is the
    //! session running?" and got yes about a turn that had begun since. These
    //! are about the agent answering for itself instead.
    use super::*;
    use crate::agent_loop::agent_actor::testing::*;
    use crate::agent_loop::context::AgentOutcome;
    use horsie_actor::{ActorSystem, InMemoryJournal, Journal};
    use horsie_models::agent::Usage;

    /// Spawn an agent over a journal that already holds `events`, and hand back
    /// whatever it reports while recovering.
    async fn recover_with(
        events: &[AgentDomainEvent],
    ) -> tokio::sync::mpsc::UnboundedReceiver<AgentOutcome> {
        let id = uuid::Uuid::new_v4();
        let journal = Arc::new(InMemoryJournal::new());
        let encoded: Vec<Vec<u8>> = events
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap())
            .collect();
        journal
            .persist(&AgentActor::persistence_id_for(id), &encoded, 0)
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            artifacts: None,
            context_provider: Arc::new(HangingContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: id,
            ready: true,
        };
        let mut params = AgentParams::from_def(&def_fixture());
        // Every agent a session spawns is interactive, so this is the only
        // configuration that matters — and it is the one that returns from
        // `on_recovery_complete` early, so the report has to precede that.
        params.interactive = true;
        let _agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );
        // Recovery runs before the first command, so anything reported is
        // already on its way by the time the spawn returns.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        rx
    }

    fn began() -> AgentDomainEvent {
        AgentDomainEvent::TurnBegan {
            consumed: Vec::new(),
            answered: Vec::new(),
            at_ms: 0,
        }
    }

    /// A journal ending at `TurnBegan` is what a process killed mid-run leaves
    /// behind, and the agent is the only thing that can say so: its owner sees
    /// a status, and a status cannot name a turn.
    #[tokio::test]
    async fn a_turn_the_process_died_in_is_reported_at_recovery() {
        let mut outcomes = recover_with(&[began()]).await;
        assert!(
            matches!(outcomes.try_recv(), Ok(AgentOutcome::Interrupted { .. })),
            "an agent recovering mid-turn must tell its owner the turn is over"
        );
    }

    /// The other half. A turn that reached a boundary under its own power is
    /// not an interruption, and reporting one would end a turn that had already
    /// ended properly.
    #[tokio::test]
    async fn a_turn_that_reached_a_boundary_is_not_reported() {
        let mut outcomes = recover_with(&[
            began(),
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(1, 1),
                iterations: 1,
                context_tokens: 0,
                at_ms: 1,
            },
        ])
        .await;
        assert!(
            outcomes.try_recv().is_err(),
            "a completed turn is not an interruption"
        );
    }

    /// A park is a boundary too: the agent is waiting for an answer, not
    /// stranded mid-run. Reporting it would move the session off
    /// `AwaitingInput` and lose the question.
    #[tokio::test]
    async fn a_parked_turn_is_not_reported() {
        let mut outcomes = recover_with(&[
            began(),
            AgentDomainEvent::AskRecorded {
                asks: vec![crate::agent_loop::AskedQuestion {
                    tool_call_id: Some("call-1".into()),
                    question: "which one?".into(),
                }],
                at_ms: 1,
            },
        ])
        .await;
        assert!(
            outcomes.try_recv().is_err(),
            "an agent parked on a question has no interrupted turn to report"
        );
    }
}
