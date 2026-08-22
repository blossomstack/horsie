//! `SqlJournal` against the same contract every other journal is held to, on
//! whichever backend this run selected.
//!
//! All ten contract assertions are green here, including the five that were red
//! on the `FileJournal` this replaced (#61 item 9): snapshots and compaction
//! actually do something. The tests below the contract
//! are the ones that only mean something for a SQL backend — and because
//! `db::testing::db()` picks its dialect from the environment, they are a
//! PostgreSQL suite as well as a SQLite one without being written twice.

#![cfg(feature = "test-util")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
    // These tests read a `Journal` directly, which is the subject here rather
    // than a violation. Everywhere else the ban stands: only an actor reads its
    // own journal, and only to recover.
    clippy::disallowed_methods
)]

use async_trait::async_trait;
use futures_util::StreamExt;
use horsie_actor::testkit::conformance;
use horsie_actor::{Journal, PersistenceId};
use horsie_server::db::journal::SqlJournal;
use horsie_server::db::testing;

async fn journal() -> SqlJournal {
    SqlJournal::new(testing::db().await)
}

fn pid(id: &str) -> PersistenceId {
    PersistenceId::new("t", id)
}

/// Every event in a log, by `PersistenceId` rather than by bare id.
async fn read(j: &SqlJournal, pid: &PersistenceId) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut s = j.replay(pid, 0).await;
    while let Some(item) = s.next().await {
        out.push(item.unwrap().1);
    }
    out
}

async fn drain(j: &SqlJournal, id: &str, after: u64) -> Vec<(u64, Vec<u8>)> {
    let mut s = j.replay(&pid(id), after).await;
    let mut out = Vec::new();
    while let Some(item) = s.next().await {
        out.push(item.unwrap());
    }
    out
}

// ── the contract, shared with every other backend ────────────────────────────

macro_rules! conformance_tests {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test]
        async fn $name() {
            conformance::$name(&journal().await).await;
        }
    )*};
}

conformance_tests!(
    persist_then_replay_returns_events_in_order,
    replay_skips_events_at_or_before_after_seq,
    logs_are_namespaced_by_kind,
    clear_removes_all_state,
    persist_continues_numbering_after_compaction,
    snapshot_roundtrips_with_seq,
    delete_events_before_compacts,
    copy_snapshot_seeds_new_id,
    copy_snapshot_without_source_errors,
    snapshot_then_compact_leaves_only_later_events,
    last_seq_reports_where_the_log_ends,
    // The write fence. These are the assertions that matter most here: the
    // condition and the append have to be one statement, and only a real
    // database can show that.
    persist_rejects_a_stale_writer,
    persist_rejects_a_writer_ahead_of_the_log,
    save_snapshot_rejects_a_stale_writer,
    only_one_writer_can_start_a_log,
);

// ── backend specifics ────────────────────────────────────────────────────────

/// The property the whole design turns on: numbers come from `last_seq`, so
/// compaction cannot renumber what survives and a cursor stays meaningful.
#[tokio::test]
async fn compaction_never_renumbers_the_survivors() {
    let j = journal().await;
    j.persist(&pid("n"), &[vec![1], vec![2], vec![3], vec![4]], 0)
        .await
        .unwrap();
    assert_eq!(
        drain(&j, "n", 0)
            .await
            .iter()
            .map(|(s, _)| *s)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    j.delete_events_before(&pid("n"), 2).await.unwrap();
    assert_eq!(
        drain(&j, "n", 0)
            .await
            .iter()
            .map(|(s, _)| *s)
            .collect::<Vec<_>>(),
        vec![3, 4],
        "survivors keep their original numbers"
    );

    j.persist(&pid("n"), &[vec![5]], 4).await.unwrap();
    assert_eq!(
        drain(&j, "n", 4).await,
        vec![(5, vec![5])],
        "the next event continues from last_seq, not from MAX(seq)"
    );
}

/// The same property across a restart, with the log fully compacted: a fresh
/// journal over the same database must not restart numbering at 1. An
/// implementation that cached the head in memory and seeded it from `MAX(seq)`
/// would silently corrupt the log here.
#[tokio::test]
async fn numbering_survives_a_restart_after_full_compaction() {
    let db = testing::db().await;
    let first = SqlJournal::new(db.clone());
    first
        .persist(&pid("compacted"), &[vec![1], vec![2]], 0)
        .await
        .unwrap();
    first
        .save_snapshot(&pid("compacted"), vec![99], 2)
        .await
        .unwrap();
    first
        .delete_events_before(&pid("compacted"), 2)
        .await
        .unwrap();

    // A new instance over the same database stands in for a restart.
    let second = SqlJournal::new(db);
    second
        .persist(&pid("compacted"), &[vec![3]], 2)
        .await
        .unwrap();
    assert_eq!(
        drain(&second, "compacted", 2).await,
        vec![(3, vec![3])],
        "the event after a compacted snapshot must be seq 3, not seq 1"
    );
}

/// A replay from a cursor must yield the tail, and nothing before it — the read
/// the file backend could only answer by decoding the whole log.
#[tokio::test]
async fn replay_from_a_cursor_yields_only_the_tail() {
    let j = journal().await;
    let events: Vec<Vec<u8>> = (0u8..50).map(|i| vec![i]).collect();
    j.persist(&pid("long"), &events, 0).await.unwrap();

    let tail = drain(&j, "long", 47).await;
    assert_eq!(
        tail,
        vec![(48, vec![47]), (49, vec![48]), (50, vec![49])],
        "only the events after the cursor, with their own numbers"
    );
}

/// Both sides of the 1 000-row boundary at once: `persist` chunks its `INSERT`
/// and `replay` pages its `SELECT`, at the same size, so a 2 500-event batch
/// crosses each of them twice. Either boundary is invisible to the caller or it
/// is a bug — and an off-by-one in the per-chunk sequence base would show up
/// here as duplicated or skipped numbers rather than as an error.
#[tokio::test]
async fn replay_pages_a_log_longer_than_one_page() {
    let j = journal().await;
    // 2 500 events: two full pages plus a partial one.
    let events: Vec<Vec<u8>> = (0..2_500u32).map(|i| i.to_le_bytes().to_vec()).collect();
    j.persist(&pid("paging"), &events, 0).await.unwrap();

    let seen = drain(&j, "paging", 0).await;
    assert_eq!(
        seen.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(),
        events,
        "a log spanning several pages must replay whole and in order"
    );
    assert_eq!(
        seen.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        (1..=2_500u64).collect::<Vec<_>>(),
        "and with contiguous sequence numbers across the page boundaries"
    );
}

#[tokio::test]
async fn a_batch_is_numbered_contiguously_from_the_log_head() {
    let j = journal().await;
    j.persist(&pid("b"), &[vec![1]], 0).await.unwrap();
    j.persist(&pid("b"), &[vec![2], vec![3], vec![4]], 1)
        .await
        .unwrap();
    assert_eq!(
        drain(&j, "b", 0).await,
        vec![(1, vec![1]), (2, vec![2]), (3, vec![3]), (4, vec![4])]
    );
}

#[tokio::test]
async fn a_fork_continues_numbering_from_the_copied_snapshot() {
    let j = journal().await;
    j.persist(&pid("src"), &[vec![1], vec![2]], 0)
        .await
        .unwrap();
    j.save_snapshot(&pid("src"), vec![9], 2).await.unwrap();
    j.copy_snapshot(&pid("src"), &pid("dst")).await.unwrap();

    j.persist(&pid("dst"), &[vec![3]], 2).await.unwrap();
    assert_eq!(
        drain(&j, "dst", 2).await,
        vec![(3, vec![3])],
        "the sub_session's first event follows the snapshot it was seeded with"
    );
    // And the source is untouched by the copy.
    assert_eq!(drain(&j, "src", 0).await.len(), 2);
}

#[tokio::test]
async fn reading_an_unknown_log_creates_nothing() {
    let db = testing::db().await;
    let j = SqlJournal::new(db.clone());
    assert!(drain(&j, "ghost", 0).await.is_empty());
    assert_eq!(j.latest_snapshot(&pid("ghost")).await.unwrap(), None);
    // A read must not have inserted a log row as a side effect.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journal_logs")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn payloads_survive_arbitrary_bytes() {
    let j = journal().await;
    let payload = vec![0u8, b'\n', 255, b'"', 0x1f];
    j.persist(&pid("bin"), std::slice::from_ref(&payload), 0)
        .await
        .unwrap();
    assert_eq!(drain(&j, "bin", 0).await, vec![(1, payload)]);
}

#[tokio::test]
async fn clear_removes_events_and_snapshot_together() {
    let db = testing::db().await;
    let j = SqlJournal::new(db.clone());
    j.persist(&pid("c"), &[vec![1]], 0).await.unwrap();
    j.save_snapshot(&pid("c"), vec![7], 1).await.unwrap();
    j.clear(&pid("c")).await.unwrap();
    assert!(drain(&j, "c", 0).await.is_empty());
    assert_eq!(j.latest_snapshot(&pid("c")).await.unwrap(), None);

    // Not just invisible — gone. `clear` must not lean on ON DELETE CASCADE,
    // which fires on PostgreSQL and does nothing on SQLite (foreign keys are
    // never enabled), leaving rows that no later read could ever reach: the
    // next `persist` allocates a fresh log_id, so the orphans would be
    // permanent and unobservable through the trait.
    for table in ["journal_events", "journal_snapshots", "journal_logs"] {
        let rows: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(rows, 0, "{table} still holds rows after clear()");
    }

    // Numbering restarts, because the log itself is gone.
    j.persist(&pid("c"), &[vec![2]], 0).await.unwrap();
    assert_eq!(drain(&j, "c", 0).await, vec![(1, vec![2])]);
}

/// The payoff: after a snapshot compacts the log, a fresh actor recovers the
/// same state while replaying only what came after it. This is what turns an
/// O(transcript) recovery into an O(events-since-snapshot) one.
#[tokio::test]
async fn recovery_reads_the_snapshot_and_only_the_events_after_it() {
    use horsie_actor::{ActorContext, ActorSystem, CommandEffect, EventSourcedActor};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Default, Clone)]
    #[serde(default)]
    struct SumState {
        total: i64,
    }
    #[derive(Serialize, Deserialize, Clone)]
    struct Added(i64);
    enum Cmd {
        Add(i64),
        Snapshot,
        Get(tokio::sync::oneshot::Sender<i64>),
    }
    struct Sum;

    #[async_trait]
    impl EventSourcedActor for Sum {
        type Command = Cmd;
        type Event = Added;
        type State = SumState;
        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("sum", "s1")
        }
        fn initial_state() -> SumState {
            SumState::default()
        }
        fn apply_event(mut s: SumState, e: Added) -> SumState {
            s.total += e.0;
            s
        }
        async fn handle_command(
            &mut self,
            state: &SumState,
            cmd: Cmd,
            _ctx: &mut ActorContext<Cmd>,
        ) -> CommandEffect<Added> {
            match cmd {
                Cmd::Add(n) => CommandEffect::persist(vec![Added(n)]),
                Cmd::Snapshot => CommandEffect::snapshot(),
                Cmd::Get(tx) => {
                    let _ = tx.send(state.total);
                    CommandEffect::none()
                }
            }
        }
    }

    let j = std::sync::Arc::new(journal().await);
    let pid = PersistenceId::new("sum", "s1");

    // Four events, a snapshot (which compacts), then one more.
    let a = horsie_server::testing::spawn_detached(
        &ActorSystem::new(j.clone() as std::sync::Arc<dyn Journal>),
        Sum,
    );
    for n in [1, 2, 3, 4] {
        a.tell(Cmd::Add(n)).await.unwrap();
    }
    a.tell(Cmd::Snapshot).await.unwrap();
    a.tell(Cmd::Add(5)).await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    a.tell(Cmd::Get(tx)).await.unwrap();
    assert_eq!(rx.await.unwrap(), 15);

    // The snapshot was taken at event 4 and compacted everything up to it.
    assert_eq!(
        j.latest_snapshot(&pid).await.unwrap().map(|(_, s)| s),
        Some(4)
    );
    let mut remaining = j.replay(&pid, 0).await;
    let mut left = Vec::new();
    while let Some(item) = remaining.next().await {
        left.push(item.unwrap().0);
    }
    assert_eq!(
        left,
        vec![5],
        "only the post-snapshot event survives, keeping its own number"
    );

    // A second incarnation recovers the same total from snapshot + tail.
    let b = horsie_server::testing::spawn_detached(
        &ActorSystem::new(j.clone() as std::sync::Arc<dyn Journal>),
        Sum,
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    b.tell(Cmd::Get(tx)).await.unwrap();
    assert_eq!(
        rx.await.unwrap(),
        15,
        "snapshot (10) plus the one replayed event (5)"
    );
}

/// One persistence id is one log, however many handles are open on it.
///
/// `SqlJournal` holds nothing but a pool, so two of them over one database are
/// not two namespaces — they are two views of the same row, and what orders
/// their writes is that row's own `last_seq`. Conformance asks this of a
/// single handle; asking it of two is what shows the state lives in the
/// database rather than in the type, which is why one node can hand the same
/// journal to every actor it hosts.
#[tokio::test]
async fn two_handles_on_one_database_are_the_same_log() {
    let db = testing::db().await;
    let first = SqlJournal::new(db.clone());
    let second = SqlJournal::new(db);
    let pid = PersistenceId::new("session", "same-id");

    first.persist(&pid, &[b"first".to_vec()], 0).await.unwrap();

    // The second handle inherits where the log ends rather than starting a
    // count of its own, so its next write is admitted only from there.
    assert_eq!(second.last_seq(&pid).await.unwrap(), 1);
    assert!(
        second
            .persist(&pid, &[b"racing".to_vec()], 0)
            .await
            .is_err(),
        "a second handle was allowed to start a log that already exists"
    );

    second
        .persist(&pid, &[b"second".to_vec()], 1)
        .await
        .unwrap();
    assert_eq!(
        read(&first, &pid).await,
        vec![b"first".to_vec(), b"second".to_vec()],
        "both handles append to one history"
    );
}

/// A stale writer's append is rejected by the database, not merged.
///
/// The in-memory journal has the same test, but this is the implementation that
/// matters: the condition and the append have to be one statement, and only SQL
/// can show that here.
#[tokio::test]
async fn a_stale_writers_append_is_rejected() {
    let db = testing::db().await;
    let j = SqlJournal::new(db);
    let pid = PersistenceId::new("session", "conflict");

    // Two writers recovered at the same point; one of them writes.
    j.persist(&pid, &[b"first".to_vec()], 0).await.unwrap();

    // The other still believes the log is empty, and its next write is where it
    // finds out. Nothing told it, and nothing could have.
    let err = j.persist(&pid, &[b"stale".to_vec()], 0).await.unwrap_err();
    assert!(
        matches!(
            err,
            horsie_actor::JournalError::Conflict {
                expected: 0,
                actual: 1,
                ..
            }
        ),
        "a stale write was accepted: {err:?}"
    );

    // And it left nothing behind — the rejection rolled the append back with
    // it.
    let mut events: Vec<Vec<u8>> = Vec::new();
    let mut s = j.replay(&pid, 0).await;
    while let Some(item) = s.next().await {
        events.push(item.unwrap().1);
    }
    assert_eq!(events, vec![b"first".to_vec()]);

    // The writer that is up to date still works.
    j.persist(&pid, &[b"second".to_vec()], 1).await.unwrap();
}

/// A whole batch is rejected together, not partly applied.
///
/// The fold is what makes this true: the numbers are allocated by the same
/// statement that checks the condition, so a rejected batch never gets any.
#[tokio::test]
async fn a_rejected_batch_leaves_no_partial_write() {
    let db = testing::db().await;
    let j = SqlJournal::new(db);
    let pid = PersistenceId::new("session", "batch");

    j.persist(&pid, &[b"a".to_vec()], 0).await.unwrap();
    assert!(
        j.persist(&pid, &[b"b".to_vec(), b"c".to_vec(), b"d".to_vec()], 0)
            .await
            .is_err()
    );

    assert_eq!(
        j.last_seq(&pid).await.unwrap(),
        1,
        "a rejected batch consumed sequence numbers"
    );
    assert_eq!(read(&j, &pid).await, vec![b"a".to_vec()]);
}

/// Snapshots carry the same condition: a writer whose log has moved on would
/// otherwise overwrite the state the next recovery starts from.
#[tokio::test]
async fn a_stale_writers_snapshot_is_rejected() {
    let db = testing::db().await;
    let j = SqlJournal::new(db);
    let pid = PersistenceId::new("session", "stale-snapshot");

    j.persist(&pid, &[b"a".to_vec(), b"b".to_vec()], 0)
        .await
        .unwrap();

    let err = j
        .save_snapshot(&pid, b"stale".to_vec(), 1)
        .await
        .unwrap_err();
    assert!(matches!(err, horsie_actor::JournalError::Conflict { .. }));
    assert!(j.latest_snapshot(&pid).await.unwrap().is_none());

    j.save_snapshot(&pid, b"current".to_vec(), 2).await.unwrap();
}

/// The allocator is one row per log, so a quiet id starts at zero however busy
/// its neighbours in the table are.
///
/// Now that one journal serves the whole node this is the separation that is
/// left — every session in the database shares these tables, and the only thing
/// keeping one writer's fence off another's is which `log_id` the `(kind, id)`
/// lookup resolved to.
#[tokio::test]
async fn numbering_belongs_to_a_log_and_not_to_the_table() {
    let db = testing::db().await;
    let j = SqlJournal::new(db);
    let busy = PersistenceId::new("session", "busy");
    let quiet = PersistenceId::new("session", "quiet");

    for expected in 0..3 {
        j.persist(&busy, &[b"x".to_vec()], expected).await.unwrap();
    }

    assert_eq!(
        j.last_seq(&quiet).await.unwrap(),
        0,
        "a neighbour's writes advanced this log"
    );
    // So its first writer still expects an empty log, and is admitted.
    j.persist(&quiet, &[b"first".to_vec()], 0).await.unwrap();
    assert_eq!(read(&j, &quiet).await, vec![b"first".to_vec()]);
    assert_eq!(read(&j, &busy).await.len(), 3);
}
