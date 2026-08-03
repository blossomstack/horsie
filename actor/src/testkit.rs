//! Fault-injecting [`Journal`] wrappers and on-disk fixtures.
//!
//! Gated behind `cfg(any(test, feature = "test-util"))`: available to the actor
//! crate's own tests unconditionally, and to `server` / `workflow` when they
//! enable `horsie-actor/test-util`.

use crate::error::JournalError;
use crate::journal::{Journal, JournalResult};
use crate::persistence_id::PersistenceId;
use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Wraps any [`Journal`], failing selected operations on demand.
pub struct FaultyJournal<J> {
    inner: J,
    /// Number of `persist` calls to allow before failing; `None` = never fail.
    persist_budget: Option<usize>,
    persists: AtomicUsize,
    fail_snapshot: bool,
    /// Sequence number at which `replay` yields an error instead of the event.
    replay_fails_at: Option<u64>,
}

impl<J> FaultyJournal<J> {
    /// A healthy wrapper — every call delegates until a fault is configured.
    pub fn wrapping(inner: J) -> Self {
        Self {
            inner,
            persist_budget: None,
            persists: AtomicUsize::new(0),
            fail_snapshot: false,
            replay_fails_at: None,
        }
    }

    /// Allow `n` successful persists, then fail every one after.
    #[must_use]
    pub fn fail_persist_after(mut self, n: usize) -> Self {
        self.persist_budget = Some(n);
        self
    }

    /// Fail every `save_snapshot`.
    #[must_use]
    pub fn fail_snapshot(mut self) -> Self {
        self.fail_snapshot = true;
        self
    }

    /// Yield an error in place of the event at `seq`, ending the replay there.
    #[must_use]
    pub fn fail_replay_at(mut self, seq: u64) -> Self {
        self.replay_fails_at = Some(seq);
        self
    }
}

#[async_trait]
impl<J: Journal> Journal for FaultyJournal<J> {
    async fn persist(&self, pid: &PersistenceId, events: &[Vec<u8>]) -> JournalResult<()> {
        if let Some(budget) = self.persist_budget
            && self.persists.fetch_add(1, Ordering::Relaxed) >= budget
        {
            return Err(JournalError::Backend("injected persist failure".into()));
        }
        self.inner.persist(pid, events).await
    }

    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<(u64, Vec<u8>)>> {
        let Some(fail_at) = self.replay_fails_at else {
            return self.inner.replay(pid, after_seq).await;
        };
        let mut out: Vec<JournalResult<(u64, Vec<u8>)>> = Vec::new();
        let mut inner = self.inner.replay(pid, after_seq).await;
        while let Some(item) = inner.next().await {
            // Fail at the journal's own numbering, so an injected failure lands
            // at the same event whether or not the log has been compacted.
            if item.as_ref().is_ok_and(|(seq, _)| *seq >= fail_at) {
                out.push(Err(JournalError::Backend("injected replay failure".into())));
                break;
            }
            out.push(item);
        }
        stream::iter(out).boxed()
    }

    async fn save_snapshot(
        &self,
        pid: &PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> JournalResult<()> {
        if self.fail_snapshot {
            return Err(JournalError::Backend("injected snapshot failure".into()));
        }
        self.inner.save_snapshot(pid, state, seq_nr).await
    }

    async fn latest_snapshot(&self, pid: &PersistenceId) -> JournalResult<Option<(Vec<u8>, u64)>> {
        self.inner.latest_snapshot(pid).await
    }

    async fn delete_events_before(&self, pid: &PersistenceId, seq_nr: u64) -> JournalResult<()> {
        self.inner.delete_events_before(pid, seq_nr).await
    }

    async fn copy_snapshot(&self, from: &PersistenceId, to: &PersistenceId) -> JournalResult<()> {
        self.inner.copy_snapshot(from, to).await
    }

    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()> {
        self.inner.clear(pid).await
    }
}

/// Write a `FileJournal`-format log for `pid` under `root`, replacing the line at
/// index `corrupt_at` with bytes that cannot be base64-decoded.
///
/// Mirrors `FileJournal::persist`'s framing exactly: one line per batch, each line
/// `base64(JSON([base64(event0), base64(event1), ...]))`.
#[cfg(any(test, feature = "file-journal"))]
pub fn write_corrupt_journal(
    root: &std::path::Path,
    pid: &PersistenceId,
    batches: &[Vec<Vec<u8>>],
    corrupt_at: usize,
) -> std::io::Result<()> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use std::io::Write;

    let path = root
        .join("actors")
        .join(&pid.kind)
        .join(&pid.id)
        .join("journal.jsonl");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(&path)?;
    for (index, batch) in batches.iter().enumerate() {
        let line = if index == corrupt_at {
            "!!!not-base64!!!".to_string()
        } else {
            let encoded: Vec<String> = batch.iter().map(|e| STANDARD.encode(e)).collect();
            let json =
                serde_json::to_vec(&encoded).map_err(|e| std::io::Error::other(e.to_string()))?;
            STANDARD.encode(&json)
        };
        writeln!(file, "{line}")?;
    }
    file.flush()
}

/// The `Journal` contract, as executable assertions.
///
/// Lives here rather than in a test file so every backend can be held to it —
/// including `SqliteJournal`, which lives in the server crate and so cannot be
/// reached from `actor/tests/`. The assertions come from the trait's own doc
/// comments, which are the real spec: they are behavioural, never about storage
/// layout, which is what makes them portable.
///
/// Each takes a fresh, empty journal.
///
/// Assertions panic on failure, which is the point — this module is test support
/// and only exists under the `test-util` feature, so the workspace's ban on
/// panic-prone constructs does not apply to it.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
pub mod conformance {
    use crate::{Journal, PersistenceId};
    use futures_util::StreamExt;

    fn pid(id: &str) -> PersistenceId {
        PersistenceId::new("conformance", id)
    }

    async fn drain(j: &dyn Journal, id: &str, after: u64) -> Vec<Vec<u8>> {
        let mut s = j.replay(&pid(id), after).await;
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap().1);
        }
        out
    }

    // ── the contract ─────────────────────────────────────────────────────────────

    pub async fn persist_then_replay_returns_events_in_order(j: &dyn Journal) {
        j.persist(&pid("order"), &[vec![1], vec![2], vec![3]])
            .await
            .unwrap();
        assert_eq!(
            drain(j, "order", 0).await,
            vec![vec![1], vec![2], vec![3]],
            "replay must return events in ascending sequence order"
        );
    }

    pub async fn replay_skips_events_at_or_before_after_seq(j: &dyn Journal) {
        j.persist(&pid("skip"), &[vec![1], vec![2], vec![3]])
            .await
            .unwrap();
        assert_eq!(
            drain(j, "skip", 1).await,
            vec![vec![2], vec![3]],
            "replay(after_seq) must yield strictly-greater sequence numbers only"
        );
    }

    pub async fn logs_are_namespaced_by_kind(j: &dyn Journal) {
        j.persist(&PersistenceId::new("workflow", "shared"), &[vec![1]])
            .await
            .unwrap();
        j.persist(&PersistenceId::new("agent", "shared"), &[vec![2]])
            .await
            .unwrap();
        let mut wf = j.replay(&PersistenceId::new("workflow", "shared"), 0).await;
        let mut ag = j.replay(&PersistenceId::new("agent", "shared"), 0).await;
        assert_eq!(wf.next().await.unwrap().unwrap(), (1, vec![1]));
        assert_eq!(ag.next().await.unwrap().unwrap(), (1, vec![2]));
    }

    pub async fn clear_removes_all_state(j: &dyn Journal) {
        j.persist(&pid("cleared"), &[vec![1]]).await.unwrap();
        j.clear(&pid("cleared")).await.unwrap();
        assert!(drain(j, "cleared", 0).await.is_empty());
    }

    pub async fn persist_continues_numbering_after_compaction(j: &dyn Journal) {
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

    pub async fn snapshot_roundtrips_with_seq(j: &dyn Journal) {
        j.save_snapshot(&pid("snap"), vec![9, 9], 5).await.unwrap();
        assert_eq!(
            j.latest_snapshot(&pid("snap")).await.unwrap(),
            Some((vec![9, 9], 5)),
            "a saved snapshot must be readable back with its sequence number"
        );
    }

    pub async fn delete_events_before_compacts(j: &dyn Journal) {
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

    pub async fn copy_snapshot_seeds_new_id(j: &dyn Journal) {
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

    pub async fn copy_snapshot_without_source_errors(j: &dyn Journal) {
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
    pub async fn snapshot_then_compact_leaves_only_later_events(j: &dyn Journal) {
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

    fn pid() -> PersistenceId {
        PersistenceId::new("t", "a")
    }

    #[tokio::test]
    async fn fail_persist_after_lets_the_first_n_through() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_persist_after(1);
        assert!(j.persist(&pid(), &[vec![1]]).await.is_ok());
        assert!(j.persist(&pid(), &[vec![2]]).await.is_err());
        assert!(j.persist(&pid(), &[vec![3]]).await.is_err());
    }

    #[tokio::test]
    async fn fail_persist_after_zero_fails_immediately() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_persist_after(0);
        assert!(j.persist(&pid(), &[vec![1]]).await.is_err());
    }

    #[tokio::test]
    async fn healthy_by_default_delegates_to_inner() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new());
        j.persist(&pid(), &[vec![7]]).await.unwrap();
        let mut s = j.replay(&pid(), 0).await;
        assert_eq!(s.next().await.unwrap().unwrap(), (1, vec![7]));
    }

    #[tokio::test]
    async fn fail_snapshot_rejects_saves_but_not_persists() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_snapshot();
        assert!(j.persist(&pid(), &[vec![1]]).await.is_ok());
        assert!(j.save_snapshot(&pid(), vec![9], 1).await.is_err());
    }

    #[tokio::test]
    async fn fail_replay_at_truncates_the_stream_with_an_error() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_replay_at(2);
        j.persist(&pid(), &[vec![1], vec![2], vec![3]])
            .await
            .unwrap();
        let mut s = j.replay(&pid(), 0).await;
        assert!(s.next().await.unwrap().is_ok()); // seq 1
        assert!(s.next().await.unwrap().is_err()); // seq 2 → injected failure
    }

    #[cfg(feature = "file-journal")]
    #[tokio::test]
    async fn corrupt_fixture_produces_a_file_that_stops_decoding_midway() {
        let dir = tempfile::tempdir().unwrap();
        let pid = PersistenceId::new("t", "corrupt");
        write_corrupt_journal(
            dir.path(),
            &pid,
            &[vec![vec![1]], vec![vec![2]], vec![vec![3]]],
            1,
        )
        .unwrap();

        let j = crate::file_journal::FileJournal::new(dir.path());
        let mut s = j.replay(&pid, 0).await;
        let first = s.next().await.unwrap().unwrap();
        assert_eq!(first, (1, vec![1]));
        // Everything past the corrupt line is unreachable — that is the fixture
        // working, and separately it is the bug (#61 item 13), asserted in
        // actor/tests/journal_corruption.rs.
        assert!(s.next().await.is_none());
    }
}
