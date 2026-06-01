use crate::actor::{CommandEffect, EventSourcedActor};
use crate::error::{JournalError, TellError};
use crate::journal::Journal;
use crate::persistence_id::PersistenceId;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Mailbox capacity for every spawned actor.
const MAILBOX_CAPACITY: usize = 64;

/// A cheap, cloneable handle for sending commands to an actor.
pub struct ActorRef<C> {
    tx: mpsc::Sender<C>,
}

// Manual `Clone` — a `Sender<C>` clones regardless of whether `C: Clone`.
impl<C> Clone for ActorRef<C> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<C: Send + 'static> ActorRef<C> {
    /// Deliver `cmd` to the actor's mailbox, waiting if the mailbox is full.
    /// Fails only if the actor has stopped.
    pub async fn tell(&self, cmd: C) -> Result<(), TellError> {
        self.tx
            .send(cmd)
            .await
            .map_err(|_| TellError::MailboxClosed)
    }

    /// Send a request and await the actor's reply — the request/response pattern.
    ///
    /// `make` builds the command from a fresh reply channel; the actor answers by
    /// sending on it (e.g. via [`CommandEffect::PersistAndAck`], which replies only
    /// after the durable write, yielding genuine backpressure). Resolves once the
    /// actor processes the command and replies; errors if the actor stops first.
    pub async fn ask<F, R>(&self, make: F) -> Result<R, TellError>
    where
        F: FnOnce(tokio::sync::oneshot::Sender<R>) -> C,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tell(make(reply_tx)).await?;
        reply_rx.await.map_err(|_| TellError::MailboxClosed)
    }
}

/// Process-wide runtime state shared by every actor in a tree.
struct RuntimeInner {
    journal: Arc<dyn Journal>,
}

/// Handle to the runtime from inside an actor: spawn children, reference self,
/// and reach the journal directly when an actor needs it (e.g. fork).
pub struct ActorContext<A: EventSourcedActor> {
    inner: Arc<RuntimeInner>,
    self_tx: mpsc::Sender<A::Command>,
}

impl<A: EventSourcedActor> ActorContext<A> {
    /// A reference to this actor's own mailbox.
    pub fn self_ref(&self) -> ActorRef<A::Command> {
        ActorRef {
            tx: self.self_tx.clone(),
        }
    }

    /// Spawn a child actor sharing this runtime's journal. The child recovers
    /// from its own `persistence_id` before accepting commands.
    pub fn spawn<B: EventSourcedActor>(&self, actor: B) -> ActorRef<B::Command> {
        spawn_inner(actor, self.inner.clone())
    }

    /// Direct journal access for actors that manage persistence themselves
    /// (e.g. copying a snapshot to seed a forked session).
    pub fn journal(&self) -> &Arc<dyn Journal> {
        &self.inner.journal
    }
}

/// Spawn the root actor of a new runtime backed by `journal`.
pub fn spawn_root<A: EventSourcedActor>(
    actor: A,
    journal: Arc<dyn Journal>,
) -> ActorRef<A::Command> {
    let inner = Arc::new(RuntimeInner { journal });
    spawn_inner(actor, inner)
}

fn spawn_inner<A: EventSourcedActor>(actor: A, inner: Arc<RuntimeInner>) -> ActorRef<A::Command> {
    let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
    let ctx = ActorContext::<A> {
        inner,
        self_tx: tx.clone(),
    };
    tokio::spawn(run_actor(actor, rx, ctx));
    ActorRef { tx }
}

/// Rebuild an actor's state from its latest snapshot plus subsequent events.
/// Returns the recovered state and the sequence number of the last applied event.
async fn recover<A: EventSourcedActor>(
    pid: &PersistenceId,
    journal: &Arc<dyn Journal>,
) -> Result<(A::State, u64), JournalError> {
    let (mut state, mut seq_nr) = match journal.latest_snapshot(pid).await? {
        Some((bytes, seq)) => {
            let state = serde_json::from_slice::<A::State>(&bytes)
                .map_err(|e| JournalError::Serialization(e.to_string()))?;
            (state, seq)
        }
        None => (A::initial_state(), 0),
    };

    let mut stream = journal.replay(pid, seq_nr).await;
    while let Some(item) = stream.next().await {
        let bytes = item?;
        let event = serde_json::from_slice::<A::Event>(&bytes)
            .map_err(|e| JournalError::Serialization(e.to_string()))?;
        state = A::apply_event(state, event);
        seq_nr += 1;
    }
    Ok((state, seq_nr))
}

/// Persist `events`, then fold them into `state`, advancing `seq_nr`. Returns the
/// (possibly unchanged) state and whether the durable write succeeded. On failure
/// the events are neither applied nor counted, keeping state consistent with what
/// was durably written; the error is also logged here. Callers that don't need the
/// outcome can ignore it (best-effort, as before); `PersistAndAck` forwards it.
async fn persist_events<A: EventSourcedActor>(
    pid: &PersistenceId,
    journal: &Arc<dyn Journal>,
    events: Vec<A::Event>,
    mut state: A::State,
    seq_nr: &mut u64,
) -> (A::State, Result<(), JournalError>) {
    let mut encoded = Vec::with_capacity(events.len());
    for event in &events {
        match serde_json::to_vec(event) {
            Ok(bytes) => encoded.push(bytes),
            Err(e) => {
                tracing::error!(%pid, error = %e, "failed to serialize event; skipping persist");
                return (state, Err(JournalError::Serialization(e.to_string())));
            }
        }
    }
    if let Err(e) = journal.persist(pid, &encoded).await {
        tracing::error!(%pid, error = %e, "failed to persist events; state left unchanged");
        return (state, Err(e));
    }
    for event in events {
        state = A::apply_event(state, event);
        *seq_nr += 1;
    }
    (state, Ok(()))
}

/// Snapshot `state` at `seq_nr` and compact the now-redundant event log.
async fn snapshot_state<A: EventSourcedActor>(
    pid: &PersistenceId,
    journal: &Arc<dyn Journal>,
    state: &A::State,
    seq_nr: u64,
) {
    let bytes = match serde_json::to_vec(state) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(%pid, error = %e, "failed to serialize snapshot; skipping");
            return;
        }
    };
    if let Err(e) = journal.save_snapshot(pid, bytes, seq_nr).await {
        tracing::error!(%pid, error = %e, "failed to save snapshot");
        return;
    }
    if let Err(e) = journal.delete_events_before(pid, seq_nr).await {
        tracing::error!(%pid, error = %e, "failed to compact event log after snapshot");
    }
}

/// The lifecycle of a single actor: recover, then process commands until the
/// mailbox closes or the actor asks to stop.
async fn run_actor<A: EventSourcedActor>(
    mut actor: A,
    mut rx: mpsc::Receiver<A::Command>,
    mut ctx: ActorContext<A>,
) {
    let pid = actor.persistence_id();
    let journal = ctx.inner.journal.clone();

    let (mut state, mut seq_nr) = match recover::<A>(&pid, &journal).await {
        Ok(recovered) => recovered,
        Err(e) => {
            tracing::error!(%pid, error = %e, "actor recovery failed; shutting down");
            return;
        }
    };

    actor.on_recovery_complete(&state, &mut ctx).await;

    while let Some(cmd) = rx.recv().await {
        let effect = actor.handle_command(&state, cmd, &mut ctx).await;
        let CommandEffect {
            events,
            snapshot,
            ack,
            stop,
        } = effect;

        // One persist step, then the post-persist actions in a fixed order: write →
        // snapshot → ack → stop. The write outcome is folded only on success, so a
        // failed write leaves state consistent and the ack reports the failure.
        let result;
        (state, result) = persist_events::<A>(&pid, &journal, events, state, &mut seq_nr).await;

        // Snapshot only after a successful write (snapshotting state that diverged
        // from the journal would be unsound). Skipped when stopping — the state is
        // discarded next anyway.
        if snapshot && result.is_ok() && !stop {
            snapshot_state::<A>(&pid, &journal, &state, seq_nr).await;
        }
        // Reply only now — after the durable write is attempted — so an `ask` caller
        // returns the journaled guarantee (`Ok`) or the failure (`Err`) and can
        // decide whether to proceed.
        if let Some(ack) = ack {
            let _ = ack.send(result);
        }
        if stop {
            break;
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
    use super::*;
    use crate::journal::InMemoryJournal;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use std::time::Duration;
    use tokio::sync::oneshot;

    // A counter that persists every increment and snapshots on demand.
    struct Counter {
        id: String,
        // Lets a test observe the recovered value at startup.
        report: Option<oneshot::Sender<i64>>,
    }

    enum CounterCmd {
        Inc(i64),
        /// Increment, replying on the channel with the durable-write outcome.
        IncAck(i64, oneshot::Sender<Result<(), JournalError>>),
        Snapshot,
        Get(oneshot::Sender<i64>),
        Stop,
    }

    #[derive(Serialize, Deserialize)]
    enum CounterEvent {
        Incremented(i64),
    }

    #[derive(Serialize, Deserialize, Default, Clone)]
    struct CounterState {
        value: i64,
    }

    #[async_trait]
    impl EventSourcedActor for Counter {
        type Command = CounterCmd;
        type Event = CounterEvent;
        type State = CounterState;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("counter", self.id.clone())
        }

        fn initial_state() -> CounterState {
            CounterState::default()
        }

        fn apply_event(mut state: CounterState, event: CounterEvent) -> CounterState {
            match event {
                CounterEvent::Incremented(n) => state.value += n,
            }
            state
        }

        async fn handle_command(
            &mut self,
            state: &CounterState,
            cmd: CounterCmd,
            _ctx: &mut ActorContext<Self>,
        ) -> CommandEffect<CounterEvent> {
            match cmd {
                CounterCmd::Inc(n) => CommandEffect::persist(vec![CounterEvent::Incremented(n)]),
                CounterCmd::IncAck(n, ack) => {
                    CommandEffect::persist(vec![CounterEvent::Incremented(n)]).and_ack(ack)
                }
                CounterCmd::Snapshot => CommandEffect::snapshot(),
                CounterCmd::Get(reply) => {
                    let _ = reply.send(state.value);
                    CommandEffect::none()
                }
                CounterCmd::Stop => CommandEffect::stop(),
            }
        }

        async fn on_recovery_complete(
            &mut self,
            state: &CounterState,
            _ctx: &mut ActorContext<Self>,
        ) {
            if let Some(tx) = self.report.take() {
                let _ = tx.send(state.value);
            }
        }
    }

    async fn current_value(actor: &ActorRef<CounterCmd>) -> i64 {
        let (tx, rx) = oneshot::channel();
        actor.tell(CounterCmd::Get(tx)).await.unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn persists_and_applies_events() {
        let journal = Arc::new(InMemoryJournal::new());
        let actor = spawn_root(
            Counter {
                id: "c1".into(),
                report: None,
            },
            journal,
        );
        actor.tell(CounterCmd::Inc(3)).await.unwrap();
        actor.tell(CounterCmd::Inc(4)).await.unwrap();
        assert_eq!(current_value(&actor).await, 7);
    }

    #[tokio::test]
    async fn ask_with_persist_and_ack_returns_after_durable_write() {
        let journal = Arc::new(InMemoryJournal::new());
        let actor = spawn_root(
            Counter {
                id: "ack".into(),
                report: None,
            },
            journal,
        );
        // `ask` resolves only when the actor replies, and `PersistAndAck` replies
        // *after* the event is persisted and folded — so the new value is already
        // observable the instant `ask` returns, and the reply reports success. This
        // is the backpressure + durability guarantee the agent loop relies on.
        let durable = actor.ask(|ack| CounterCmd::IncAck(5, ack)).await.unwrap();
        assert!(durable.is_ok(), "in-memory journal write should succeed");
        assert_eq!(current_value(&actor).await, 5);
    }

    /// A journal whose `persist` always fails, to prove `PersistAndAck` surfaces the
    /// durable-write failure to the asker (rather than acking success on a write
    /// that never landed). Other operations delegate to an inner in-memory journal.
    struct FailingPersistJournal {
        inner: InMemoryJournal,
    }

    #[async_trait]
    impl Journal for FailingPersistJournal {
        async fn persist(
            &self,
            _pid: &PersistenceId,
            _events: &[Vec<u8>],
        ) -> crate::journal::JournalResult<()> {
            Err(JournalError::Backend("disk full".into()))
        }
        async fn replay(
            &self,
            pid: &PersistenceId,
            after_seq: u64,
        ) -> futures_util::stream::BoxStream<'_, crate::journal::JournalResult<Vec<u8>>> {
            self.inner.replay(pid, after_seq).await
        }
        async fn save_snapshot(
            &self,
            pid: &PersistenceId,
            state: Vec<u8>,
            seq_nr: u64,
        ) -> crate::journal::JournalResult<()> {
            self.inner.save_snapshot(pid, state, seq_nr).await
        }
        async fn latest_snapshot(
            &self,
            pid: &PersistenceId,
        ) -> crate::journal::JournalResult<Option<(Vec<u8>, u64)>> {
            self.inner.latest_snapshot(pid).await
        }
        async fn delete_events_before(
            &self,
            pid: &PersistenceId,
            seq_nr: u64,
        ) -> crate::journal::JournalResult<()> {
            self.inner.delete_events_before(pid, seq_nr).await
        }
        async fn copy_snapshot(
            &self,
            from: &PersistenceId,
            to: &PersistenceId,
        ) -> crate::journal::JournalResult<()> {
            self.inner.copy_snapshot(from, to).await
        }
        async fn clear(&self, pid: &PersistenceId) -> crate::journal::JournalResult<()> {
            self.inner.clear(pid).await
        }
    }

    #[tokio::test]
    async fn ask_with_persist_and_ack_reports_journal_failure() {
        let journal = Arc::new(FailingPersistJournal {
            inner: InMemoryJournal::new(),
        });
        let actor = spawn_root(
            Counter {
                id: "fail".into(),
                report: None,
            },
            journal,
        );
        // The write fails, so the ack carries Err — the asker learns the event was
        // NOT journaled and can abort instead of proceeding on a phantom history.
        let durable = actor.ask(|ack| CounterCmd::IncAck(5, ack)).await.unwrap();
        assert!(durable.is_err(), "failed journal write must report Err");
        // State was left unchanged because the events were never folded.
        assert_eq!(current_value(&actor).await, 0);
    }

    #[tokio::test]
    async fn recovers_state_from_event_log_after_restart() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());

        // First incarnation persists some events, then stops.
        let a1 = spawn_root(
            Counter {
                id: "c2".into(),
                report: None,
            },
            journal.clone(),
        );
        a1.tell(CounterCmd::Inc(5)).await.unwrap();
        a1.tell(CounterCmd::Inc(10)).await.unwrap();
        // Ensure the increments are processed before we drop and "crash".
        assert_eq!(current_value(&a1).await, 15);
        a1.tell(CounterCmd::Stop).await.unwrap();

        // Second incarnation reuses the same persistence_id and journal.
        let (report_tx, report_rx) = oneshot::channel();
        let _a2 = spawn_root(
            Counter {
                id: "c2".into(),
                report: Some(report_tx),
            },
            journal,
        );
        // Recovery folds the two events back to 15.
        assert_eq!(report_rx.await.unwrap(), 15);
    }

    #[tokio::test]
    async fn recovers_from_snapshot_after_compaction() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());

        let a1 = spawn_root(
            Counter {
                id: "c3".into(),
                report: None,
            },
            journal.clone(),
        );
        a1.tell(CounterCmd::Inc(2)).await.unwrap();
        a1.tell(CounterCmd::Inc(2)).await.unwrap();
        a1.tell(CounterCmd::Snapshot).await.unwrap();
        a1.tell(CounterCmd::Inc(1)).await.unwrap();
        assert_eq!(current_value(&a1).await, 5);
        a1.tell(CounterCmd::Stop).await.unwrap();

        // Confirm the snapshot compacted the pre-snapshot events.
        let count = {
            let mut remaining = journal
                .replay(&PersistenceId::new("counter", "c3"), 0)
                .await;
            let mut count = 0;
            while let Some(item) = remaining.next().await {
                item.unwrap();
                count += 1;
            }
            count
        };
        // Only the single post-snapshot increment should remain in the log.
        assert_eq!(count, 1);

        let (report_tx, report_rx) = oneshot::channel();
        let _a2 = spawn_root(
            Counter {
                id: "c3".into(),
                report: Some(report_tx),
            },
            journal,
        );
        // snapshot (4) + replayed post-snapshot event (1) == 5.
        assert_eq!(report_rx.await.unwrap(), 5);
    }

    #[tokio::test]
    async fn spawned_child_recovers_independently() {
        // A parent that spawns a child counter and forwards a value to it.
        struct Parent {
            child: Option<ActorRef<CounterCmd>>,
        }
        enum ParentCmd {
            Start,
            ChildValue(oneshot::Sender<i64>),
        }
        #[derive(Serialize, Deserialize, Default)]
        struct Empty {}

        #[async_trait]
        impl EventSourcedActor for Parent {
            type Command = ParentCmd;
            type Event = ();
            type State = Empty;
            fn persistence_id(&self) -> PersistenceId {
                PersistenceId::new("parent", "parent")
            }
            fn initial_state() -> Empty {
                Empty::default()
            }
            fn apply_event(state: Empty, _e: ()) -> Empty {
                state
            }
            async fn handle_command(
                &mut self,
                _state: &Empty,
                cmd: ParentCmd,
                ctx: &mut ActorContext<Self>,
            ) -> CommandEffect<()> {
                match cmd {
                    ParentCmd::Start => {
                        let child = ctx.spawn(Counter {
                            id: "child".into(),
                            report: None,
                        });
                        child.tell(CounterCmd::Inc(42)).await.unwrap();
                        self.child = Some(child);
                        CommandEffect::none()
                    }
                    ParentCmd::ChildValue(reply) => {
                        if let Some(child) = &self.child {
                            let v = current_value(child).await;
                            let _ = reply.send(v);
                        }
                        CommandEffect::none()
                    }
                }
            }
        }

        let journal = Arc::new(InMemoryJournal::new());
        let parent = spawn_root(Parent { child: None }, journal);
        parent.tell(ParentCmd::Start).await.unwrap();
        // Give the child a moment to process the increment.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (tx, rx) = oneshot::channel();
        parent.tell(ParentCmd::ChildValue(tx)).await.unwrap();
        assert_eq!(rx.await.unwrap(), 42);
    }
}
