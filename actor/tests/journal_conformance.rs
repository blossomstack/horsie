// The journal itself is under test here, so it is called directly.
#![allow(clippy::disallowed_methods)]
//! Journal conformance suite.
//!
//! The same contract assertions run against every `Journal` implementation. The
//! assertions come from the trait's own doc comments (`actor/src/journal.rs:18-53`),
//! which are the real spec — they are behavioural, never about storage layout,
//! which is what makes them portable across backends.
//!
//! Deliberately shaped differently from `tests/tests/provider_conformance.rs`:
//! that suite loops over backends *inside* each test, which cannot express "this
//! assertion is red for one backend only". Five of these ten are red on
//! `FileJournal` (#61 item 9), so each backend gets its own test function.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::StreamExt;
use horsie_actor::{InMemoryJournal, Journal, PersistenceId};

fn pid(id: &str) -> PersistenceId {
    PersistenceId::new("conformance", id)
}

async fn drain(j: &dyn Journal, id: &str, after: u64) -> Vec<Vec<u8>> {
    let mut s = j.replay(&pid(id), after).await;
    let mut out = Vec::new();
    while let Some(item) = s.next().await {
        out.push(item.unwrap());
    }
    out
}

// ── the contract ─────────────────────────────────────────────────────────────

async fn persist_then_replay_returns_events_in_order(j: &dyn Journal) {
    j.persist(&pid("order"), &[vec![1], vec![2], vec![3]])
        .await
        .unwrap();
    assert_eq!(
        drain(j, "order", 0).await,
        vec![vec![1], vec![2], vec![3]],
        "replay must return events in ascending sequence order"
    );
}

async fn replay_skips_events_at_or_before_after_seq(j: &dyn Journal) {
    j.persist(&pid("skip"), &[vec![1], vec![2], vec![3]])
        .await
        .unwrap();
    assert_eq!(
        drain(j, "skip", 1).await,
        vec![vec![2], vec![3]],
        "replay(after_seq) must yield strictly-greater sequence numbers only"
    );
}

async fn logs_are_namespaced_by_kind(j: &dyn Journal) {
    j.persist(&PersistenceId::new("workflow", "shared"), &[vec![1]])
        .await
        .unwrap();
    j.persist(&PersistenceId::new("agent", "shared"), &[vec![2]])
        .await
        .unwrap();
    let mut wf = j.replay(&PersistenceId::new("workflow", "shared"), 0).await;
    let mut ag = j.replay(&PersistenceId::new("agent", "shared"), 0).await;
    assert_eq!(wf.next().await.unwrap().unwrap(), vec![1]);
    assert_eq!(ag.next().await.unwrap().unwrap(), vec![2]);
}

async fn clear_removes_all_state(j: &dyn Journal) {
    j.persist(&pid("cleared"), &[vec![1]]).await.unwrap();
    j.clear(&pid("cleared")).await.unwrap();
    assert!(drain(j, "cleared", 0).await.is_empty());
}

async fn persist_continues_numbering_after_compaction(j: &dyn Journal) {
    j.persist(&pid("numbering"), &[vec![1], vec![2]])
        .await
        .unwrap();
    j.delete_events_before(&pid("numbering"), 2).await.unwrap();
    j.persist(&pid("numbering"), &[vec![3]]).await.unwrap();
    assert_eq!(
        drain(j, "numbering", 2).await,
        vec![vec![3]],
        "an event's sequence number must be stable across compaction"
    );
}

async fn snapshot_roundtrips_with_seq(j: &dyn Journal) {
    j.save_snapshot(&pid("snap"), vec![9, 9], 5).await.unwrap();
    assert_eq!(
        j.latest_snapshot(&pid("snap")).await.unwrap(),
        Some((vec![9, 9], 5)),
        "a saved snapshot must be readable back with its sequence number"
    );
}

async fn delete_events_before_compacts(j: &dyn Journal) {
    j.persist(&pid("compact"), &[vec![1], vec![2], vec![3]])
        .await
        .unwrap();
    j.delete_events_before(&pid("compact"), 2).await.unwrap();
    assert_eq!(
        drain(j, "compact", 0).await,
        vec![vec![3]],
        "delete_events_before must drop events at or below seq_nr"
    );
}

async fn copy_snapshot_seeds_new_id(j: &dyn Journal) {
    j.persist(&pid("src"), &[vec![1], vec![2]]).await.unwrap();
    j.save_snapshot(&pid("src"), vec![7], 2).await.unwrap();
    j.copy_snapshot(&pid("src"), &pid("dst")).await.unwrap();
    assert_eq!(
        j.latest_snapshot(&pid("dst")).await.unwrap(),
        Some((vec![7], 2)),
        "copy_snapshot must seed the destination with the source snapshot"
    );
    assert!(
        drain(j, "dst", 2).await.is_empty(),
        "the destination must start with an empty event log"
    );
}

async fn copy_snapshot_without_source_errors(j: &dyn Journal) {
    assert!(
        j.copy_snapshot(&pid("missing"), &pid("dst2"))
            .await
            .is_err(),
        "copying a snapshot that does not exist must fail, not silently succeed"
    );
}

/// Asserts both that recovery starts from the snapshot and that the log was
/// compacted. The second half is what a `spawn_root`-based version of this test
/// would need: `FileJournal` recovers the correct *value* via a full replay from
/// event 0 even with snapshotting disabled, so asserting state alone would pass
/// and hide the bug.
async fn snapshot_then_compact_leaves_only_later_events(j: &dyn Journal) {
    j.persist(&pid("e2e"), &[vec![1], vec![2]]).await.unwrap();
    j.save_snapshot(&pid("e2e"), vec![42], 2).await.unwrap();
    j.delete_events_before(&pid("e2e"), 2).await.unwrap();
    j.persist(&pid("e2e"), &[vec![3]]).await.unwrap();

    assert_eq!(
        j.latest_snapshot(&pid("e2e")).await.unwrap(),
        Some((vec![42], 2)),
        "recovery must start from the snapshot"
    );
    assert_eq!(
        drain(j, "e2e", 0).await,
        vec![vec![3]],
        "only post-snapshot events should remain in the log"
    );
}

// ── backends ─────────────────────────────────────────────────────────────────

mod in_memory {
    use super::*;

    fn journal() -> InMemoryJournal {
        InMemoryJournal::new()
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        super::persist_then_replay_returns_events_in_order(&journal()).await;
    }
    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        super::replay_skips_events_at_or_before_after_seq(&journal()).await;
    }
    #[tokio::test]
    async fn logs_are_namespaced_by_kind() {
        super::logs_are_namespaced_by_kind(&journal()).await;
    }
    #[tokio::test]
    async fn clear_removes_all_state() {
        super::clear_removes_all_state(&journal()).await;
    }
    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        super::persist_continues_numbering_after_compaction(&journal()).await;
    }
    #[tokio::test]
    async fn snapshot_roundtrips_with_seq() {
        super::snapshot_roundtrips_with_seq(&journal()).await;
    }
    #[tokio::test]
    async fn delete_events_before_compacts() {
        super::delete_events_before_compacts(&journal()).await;
    }
    #[tokio::test]
    async fn copy_snapshot_seeds_new_id() {
        super::copy_snapshot_seeds_new_id(&journal()).await;
    }
    #[tokio::test]
    async fn copy_snapshot_without_source_errors() {
        super::copy_snapshot_without_source_errors(&journal()).await;
    }
    #[tokio::test]
    async fn snapshot_then_compact_leaves_only_later_events() {
        super::snapshot_then_compact_leaves_only_later_events(&journal()).await;
    }
}

#[cfg(feature = "file-journal")]
mod file {
    use horsie_actor::FileJournal;

    fn journal(dir: &tempfile::TempDir) -> FileJournal {
        FileJournal::new(dir.path())
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        let d = tempfile::tempdir().unwrap();
        super::persist_then_replay_returns_events_in_order(&journal(&d)).await;
    }
    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        let d = tempfile::tempdir().unwrap();
        super::replay_skips_events_at_or_before_after_seq(&journal(&d)).await;
    }
    #[tokio::test]
    async fn logs_are_namespaced_by_kind() {
        let d = tempfile::tempdir().unwrap();
        super::logs_are_namespaced_by_kind(&journal(&d)).await;
    }
    #[tokio::test]
    async fn clear_removes_all_state() {
        let d = tempfile::tempdir().unwrap();
        super::clear_removes_all_state(&journal(&d)).await;
    }
    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        let d = tempfile::tempdir().unwrap();
        super::persist_continues_numbering_after_compaction(&journal(&d)).await;
    }

    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::save_snapshot is a no-op returning Ok"]
    async fn snapshot_roundtrips_with_seq() {
        let d = tempfile::tempdir().unwrap();
        super::snapshot_roundtrips_with_seq(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::delete_events_before is a no-op returning Ok"]
    async fn delete_events_before_compacts() {
        let d = tempfile::tempdir().unwrap();
        super::delete_events_before_compacts(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::copy_snapshot returns Ok having copied nothing"]
    async fn copy_snapshot_seeds_new_id() {
        let d = tempfile::tempdir().unwrap();
        super::copy_snapshot_seeds_new_id(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::copy_snapshot succeeds with no source snapshot"]
    async fn copy_snapshot_without_source_errors() {
        let d = tempfile::tempdir().unwrap();
        super::copy_snapshot_without_source_errors(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal never compacts, so the whole log is replayed forever"]
    async fn snapshot_then_compact_leaves_only_later_events() {
        let d = tempfile::tempdir().unwrap();
        super::snapshot_then_compact_leaves_only_later_events(&journal(&d)).await;
    }
}
