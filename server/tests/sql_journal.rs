//! `SqlJournal` against the same contract every other journal is held to, on
//! whichever backend this run selected.
//!
//! All ten contract assertions are green here, including the five that are red
//! on `FileJournal` (#61 item 9): this is the first journal in the tree where
//! snapshots and compaction actually do something. The tests below the contract
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
);

// ── backend specifics ────────────────────────────────────────────────────────

/// The property the whole design turns on: numbers come from `last_seq`, so
/// compaction cannot renumber what survives and a cursor stays meaningful.
#[tokio::test]
async fn compaction_never_renumbers_the_survivors() {
    let j = journal().await;
    j.persist(&pid("n"), &[vec![1], vec![2], vec![3], vec![4]])
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

    j.persist(&pid("n"), &[vec![5]]).await.unwrap();
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
        .persist(&pid("compacted"), &[vec![1], vec![2]])
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
    second.persist(&pid("compacted"), &[vec![3]]).await.unwrap();
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
    j.persist(&pid("long"), &events).await.unwrap();

    let tail = drain(&j, "long", 47).await;
    assert_eq!(
        tail,
        vec![(48, vec![47]), (49, vec![48]), (50, vec![49])],
        "only the events after the cursor, with their own numbers"
    );
}

/// Replay pages internally (1 000 rows at a time), so a log longer than one page
/// has to come back complete and in order — the page boundary is invisible to
/// the caller or it is a bug.
#[tokio::test]
async fn replay_pages_a_log_longer_than_one_page() {
    let j = journal().await;
    // 2 500 events: two full pages plus a partial one.
    let events: Vec<Vec<u8>> = (0..2_500u32).map(|i| i.to_le_bytes().to_vec()).collect();
    j.persist(&pid("paging"), &events).await.unwrap();

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
    j.persist(&pid("b"), &[vec![1]]).await.unwrap();
    j.persist(&pid("b"), &[vec![2], vec![3], vec![4]])
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
    j.persist(&pid("src"), &[vec![1], vec![2]]).await.unwrap();
    j.save_snapshot(&pid("src"), vec![9], 2).await.unwrap();
    j.copy_snapshot(&pid("src"), &pid("dst")).await.unwrap();

    j.persist(&pid("dst"), &[vec![3]]).await.unwrap();
    assert_eq!(
        drain(&j, "dst", 2).await,
        vec![(3, vec![3])],
        "the fork's first event follows the snapshot it was seeded with"
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
    j.persist(&pid("bin"), std::slice::from_ref(&payload))
        .await
        .unwrap();
    assert_eq!(drain(&j, "bin", 0).await, vec![(1, payload)]);
}

#[tokio::test]
async fn clear_removes_events_and_snapshot_together() {
    let db = testing::db().await;
    let j = SqlJournal::new(db.clone());
    j.persist(&pid("c"), &[vec![1]]).await.unwrap();
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
        let rows: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(rows, 0, "{table} still holds rows after clear()");
    }

    // Numbering restarts, because the log itself is gone.
    j.persist(&pid("c"), &[vec![2]]).await.unwrap();
    assert_eq!(drain(&j, "c", 0).await, vec![(1, vec![2])]);
}

/// The payoff: after a snapshot compacts the log, a fresh actor recovers the
/// same state while replaying only what came after it. This is what turns an
/// O(transcript) recovery into an O(events-since-snapshot) one.
#[tokio::test]
async fn recovery_reads_the_snapshot_and_only_the_events_after_it() {
    use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor, spawn_root};
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
            _ctx: &mut ActorContext<Self>,
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
    let a = spawn_root(Sum, j.clone() as std::sync::Arc<dyn Journal>);
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
    let b = spawn_root(Sum, j.clone() as std::sync::Arc<dyn Journal>);
    let (tx, rx) = tokio::sync::oneshot::channel();
    b.tell(Cmd::Get(tx)).await.unwrap();
    assert_eq!(
        rx.await.unwrap(),
        15,
        "snapshot (10) plus the one replayed event (5)"
    );
}
