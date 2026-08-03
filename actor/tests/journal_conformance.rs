//! Journal conformance suite.
//!
//! The contract assertions themselves live in `horsie_actor::testkit::conformance`
//! so every backend can run them — including `SqliteJournal`, which lives in the
//! server crate and is held to the same suite there. This file binds them to the
//! backends `horsie-actor` ships.
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

// ── backends ─────────────────────────────────────────────────────────────────

mod in_memory {
    use horsie_actor::InMemoryJournal;
    use horsie_actor::testkit::conformance;

    fn journal() -> InMemoryJournal {
        InMemoryJournal::new()
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        conformance::persist_then_replay_returns_events_in_order(&journal()).await;
    }
    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        conformance::replay_skips_events_at_or_before_after_seq(&journal()).await;
    }
    #[tokio::test]
    async fn logs_are_namespaced_by_kind() {
        conformance::logs_are_namespaced_by_kind(&journal()).await;
    }
    #[tokio::test]
    async fn clear_removes_all_state() {
        conformance::clear_removes_all_state(&journal()).await;
    }
    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        conformance::persist_continues_numbering_after_compaction(&journal()).await;
    }
    #[tokio::test]
    async fn snapshot_roundtrips_with_seq() {
        conformance::snapshot_roundtrips_with_seq(&journal()).await;
    }
    #[tokio::test]
    async fn delete_events_before_compacts() {
        conformance::delete_events_before_compacts(&journal()).await;
    }
    #[tokio::test]
    async fn copy_snapshot_seeds_new_id() {
        conformance::copy_snapshot_seeds_new_id(&journal()).await;
    }
    #[tokio::test]
    async fn copy_snapshot_without_source_errors() {
        conformance::copy_snapshot_without_source_errors(&journal()).await;
    }
    #[tokio::test]
    async fn snapshot_then_compact_leaves_only_later_events() {
        conformance::snapshot_then_compact_leaves_only_later_events(&journal()).await;
    }
}

#[cfg(feature = "file-journal")]
mod file {
    use horsie_actor::FileJournal;
    use horsie_actor::testkit::conformance;

    fn journal(dir: &tempfile::TempDir) -> FileJournal {
        FileJournal::new(dir.path())
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        let d = tempfile::tempdir().unwrap();
        conformance::persist_then_replay_returns_events_in_order(&journal(&d)).await;
    }
    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        let d = tempfile::tempdir().unwrap();
        conformance::replay_skips_events_at_or_before_after_seq(&journal(&d)).await;
    }
    #[tokio::test]
    async fn logs_are_namespaced_by_kind() {
        let d = tempfile::tempdir().unwrap();
        conformance::logs_are_namespaced_by_kind(&journal(&d)).await;
    }
    #[tokio::test]
    async fn clear_removes_all_state() {
        let d = tempfile::tempdir().unwrap();
        conformance::clear_removes_all_state(&journal(&d)).await;
    }
    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        let d = tempfile::tempdir().unwrap();
        conformance::persist_continues_numbering_after_compaction(&journal(&d)).await;
    }

    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::save_snapshot is a no-op returning Ok"]
    async fn snapshot_roundtrips_with_seq() {
        let d = tempfile::tempdir().unwrap();
        conformance::snapshot_roundtrips_with_seq(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::delete_events_before is a no-op returning Ok"]
    async fn delete_events_before_compacts() {
        let d = tempfile::tempdir().unwrap();
        conformance::delete_events_before_compacts(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::copy_snapshot returns Ok having copied nothing"]
    async fn copy_snapshot_seeds_new_id() {
        let d = tempfile::tempdir().unwrap();
        conformance::copy_snapshot_seeds_new_id(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::copy_snapshot succeeds with no source snapshot"]
    async fn copy_snapshot_without_source_errors() {
        let d = tempfile::tempdir().unwrap();
        conformance::copy_snapshot_without_source_errors(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal never compacts, so the whole log is replayed forever"]
    async fn snapshot_then_compact_leaves_only_later_events() {
        let d = tempfile::tempdir().unwrap();
        conformance::snapshot_then_compact_leaves_only_later_events(&journal(&d)).await;
    }
}
