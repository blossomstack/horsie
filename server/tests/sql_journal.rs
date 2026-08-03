//! `SqlJournal` against the same contract every other journal is held to, on
//! every backend this run can reach.
//!
//! All ten assertions are green here, including the five that are red on
//! `FileJournal` (#61 item 9): this is the first journal in the tree where
//! snapshots and compaction actually do something.

#![cfg(feature = "test-util")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use horsie_actor::Journal;
use horsie_actor::testkit::journal_conformance as contract;
use horsie_server::db::journal::SqlJournal;
use horsie_server::db::testing;
use uuid::Uuid;

/// Run one contract assertion against every available backend.
///
/// Each gets its own namespace so a database that outlives the process — unlike
/// a temp dir or a fresh map — cannot carry rows from an earlier run into this
/// one.
async fn on_this_backend<F, Fut>(assertion: F)
where
    F: Fn(Box<dyn Journal>, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let db = testing::db().await;
    let journal: Box<dyn Journal> = Box::new(SqlJournal::new(db));
    // A unique namespace per run: unlike a temp dir or a fresh map, a database
    // can outlive the process and carry rows into the next run.
    let ns = format!("conf-{}", Uuid::new_v4().simple());
    assertion(journal, ns).await;
}

macro_rules! contract_test {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            on_this_backend(|j, ns| async move { contract::$name(j.as_ref(), &ns).await }).await;
        }
    };
}

contract_test!(persist_then_replay_returns_events_in_order);
contract_test!(replay_skips_events_at_or_before_after_seq);
contract_test!(logs_are_namespaced_by_kind);
contract_test!(clear_removes_all_state);
contract_test!(persist_continues_numbering_after_compaction);
contract_test!(snapshot_roundtrips_with_seq);
contract_test!(delete_events_before_compacts);
contract_test!(copy_snapshot_seeds_new_id);
contract_test!(copy_snapshot_without_source_errors);
contract_test!(snapshot_then_compact_leaves_only_later_events);

/// Replay pages internally (1 000 rows at a time), so a log longer than one
/// page has to come back complete and in order — the boundary is invisible to
/// the caller or it is a bug.
#[tokio::test]
async fn replay_pages_a_log_longer_than_one_page() {
    use futures_util::StreamExt;
    use horsie_actor::PersistenceId;

    let db = testing::db().await;
    let dialect = db.dialect();
    {
        let journal = SqlJournal::new(db.clone());
        let pid = PersistenceId::new(format!("paging-{}", Uuid::new_v4().simple()), "a");

        // 2 500 events: two full pages plus a partial one.
        let events: Vec<Vec<u8>> = (0..2_500u32).map(|i| i.to_le_bytes().to_vec()).collect();
        journal.persist(&pid, &events).await.unwrap();

        #[expect(
            clippy::disallowed_methods,
            reason = "the journal implementation is what is under test here"
        )]
        let mut stream = journal.replay(&pid, 0).await;
        let mut seen = Vec::new();
        while let Some(item) = stream.next().await {
            seen.push(item.unwrap());
        }
        assert_eq!(
            seen,
            events,
            "a {} log spanning several pages must replay whole and in order",
            dialect.as_str()
        );
    }
}

/// A second `persist` after a process restart must continue numbering rather
/// than restart at 1 — the head is cached in memory, so a fresh journal over
/// the same database has to read it back.
#[tokio::test]
async fn a_fresh_journal_continues_numbering_from_the_database() {
    use horsie_actor::PersistenceId;

    let db = testing::db().await;
    let dialect = db.dialect();
    {
        let pid = PersistenceId::new(format!("restart-{}", Uuid::new_v4().simple()), "a");

        let first = SqlJournal::new(db.clone());
        first.persist(&pid, &[vec![1], vec![2]]).await.unwrap();

        // A new instance over the same database stands in for a restart.
        let second = SqlJournal::new(db.clone());
        second.persist(&pid, &[vec![3]]).await.unwrap();

        #[expect(
            clippy::disallowed_methods,
            reason = "the journal implementation is what is under test here"
        )]
        let events = {
            use futures_util::StreamExt;
            let mut s = second.replay(&pid, 0).await;
            let mut out = Vec::new();
            while let Some(item) = s.next().await {
                out.push(item.unwrap());
            }
            out
        };
        assert_eq!(
            events,
            vec![vec![1], vec![2], vec![3]],
            "on {}, a restarted journal must not reuse sequence numbers",
            dialect.as_str()
        );
    }
}

/// The head has to survive compaction: with every event deleted, numbering
/// continues from the snapshot rather than restarting at 1. Seeding the cache
/// from `MAX(seq)` alone would silently corrupt the log here.
#[tokio::test]
async fn numbering_survives_a_restart_after_full_compaction() {
    use futures_util::StreamExt;
    use horsie_actor::PersistenceId;

    let db = testing::db().await;
    let dialect = db.dialect();
    {
        let pid = PersistenceId::new(format!("compacted-{}", Uuid::new_v4().simple()), "a");

        let first = SqlJournal::new(db.clone());
        first.persist(&pid, &[vec![1], vec![2]]).await.unwrap();
        first.save_snapshot(&pid, vec![99], 2).await.unwrap();
        first.delete_events_before(&pid, 2).await.unwrap();

        let second = SqlJournal::new(db.clone());
        second.persist(&pid, &[vec![3]]).await.unwrap();

        #[expect(
            clippy::disallowed_methods,
            reason = "the journal implementation is what is under test here"
        )]
        let after_snapshot = {
            let mut s = second.replay(&pid, 2).await;
            let mut out = Vec::new();
            while let Some(item) = s.next().await {
                out.push(item.unwrap());
            }
            out
        };
        assert_eq!(
            after_snapshot,
            vec![vec![3]],
            "on {}, the event after a compacted snapshot must be seq 3, not seq 1",
            dialect.as_str()
        );
    }
}
