//! Journal conformance suite.
//!
//! The contract assertions themselves live in `horsie_actor::testkit::conformance`
//! so every backend can run them — including `SqlJournal`, which lives in the
//! server crate and is held to the same suite there. This file binds them to the
//! one backend `horsie-actor` ships.
//!
//! Deliberately shaped differently from `tests/tests/provider_conformance.rs`:
//! that suite loops over backends *inside* each test, which cannot express "this
//! assertion is red for one backend only" — the shape this file needed while
//! `FileJournal` was still in the tree, and worth keeping for the next backend.

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
