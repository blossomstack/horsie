use crate::error::JournalError;
use crate::persistence_id::PersistenceId;
use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use std::collections::HashMap;

/// Result alias for journal operations.
pub type JournalResult<T> = Result<T, JournalError>;

/// Append-only event log with snapshot support.
///
/// Events and snapshots are opaque byte blobs — serialization is the caller's
/// concern, keeping the journal free of any domain types. Each log is identified by
/// a [`PersistenceId`] (actor kind + instance id). Sequence numbers are 1-based and
/// monotonic per `PersistenceId`; an event's sequence number is stable for the life
/// of the log even after older events are compacted away.
#[async_trait]
pub trait Journal: Send + Sync + 'static {
    /// Append `events` to `pid`'s log, assigning each the next sequence number.
    async fn persist(&self, pid: &PersistenceId, events: &[Vec<u8>]) -> JournalResult<()>;

    /// Stream every event for `pid` whose sequence number is strictly greater than
    /// `after_seq`, in ascending sequence order.
    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<Vec<u8>>>;

    /// Store `state` as the snapshot for `pid`, taken at sequence `seq_nr` (the
    /// sequence number of the last event folded into it). Replaces any prior snapshot.
    async fn save_snapshot(
        &self,
        pid: &PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> JournalResult<()>;

    /// Return the latest snapshot for `pid` as `(state, seq_nr)`, if any.
    async fn latest_snapshot(&self, pid: &PersistenceId) -> JournalResult<Option<(Vec<u8>, u64)>>;

    /// Drop all events for `pid` with sequence number less than or equal to `seq_nr`.
    async fn delete_events_before(&self, pid: &PersistenceId, seq_nr: u64) -> JournalResult<()>;

    /// Copy the snapshot from `from` onto `to`. `to` keeps the source's snapshot
    /// sequence number and starts with an empty event log, so a fresh actor recovers
    /// the copied state and continues numbering from there.
    async fn copy_snapshot(&self, from: &PersistenceId, to: &PersistenceId) -> JournalResult<()>;

    /// Remove all persisted state for `pid`. Primarily a test helper.
    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()>;
}

#[derive(Default)]
struct Entry {
    /// `(seq_nr, bytes)` in ascending sequence order.
    events: Vec<(u64, Vec<u8>)>,
    /// Sequence number of the most recently assigned event (0 = none yet).
    last_seq: u64,
    snapshot: Option<(Vec<u8>, u64)>,
}

/// In-memory [`Journal`] for tests and single-process runs.
#[derive(Default)]
pub struct InMemoryJournal {
    inner: Mutex<HashMap<PersistenceId, Entry>>,
}

impl InMemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Journal for InMemoryJournal {
    async fn persist(&self, pid: &PersistenceId, events: &[Vec<u8>]) -> JournalResult<()> {
        let mut map = self.inner.lock();
        let entry = map.entry(pid.clone()).or_default();
        for bytes in events {
            entry.last_seq += 1;
            entry.events.push((entry.last_seq, bytes.clone()));
        }
        Ok(())
    }

    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<Vec<u8>>> {
        let items: Vec<JournalResult<Vec<u8>>> = {
            let map = self.inner.lock();
            map.get(pid)
                .map(|e| {
                    e.events
                        .iter()
                        .filter(|(seq, _)| *seq > after_seq)
                        .map(|(_, bytes)| Ok(bytes.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        stream::iter(items).boxed()
    }

    async fn save_snapshot(
        &self,
        pid: &PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> JournalResult<()> {
        let mut map = self.inner.lock();
        let entry = map.entry(pid.clone()).or_default();
        entry.last_seq = entry.last_seq.max(seq_nr);
        entry.snapshot = Some((state, seq_nr));
        Ok(())
    }

    async fn latest_snapshot(&self, pid: &PersistenceId) -> JournalResult<Option<(Vec<u8>, u64)>> {
        Ok(self.inner.lock().get(pid).and_then(|e| e.snapshot.clone()))
    }

    async fn delete_events_before(&self, pid: &PersistenceId, seq_nr: u64) -> JournalResult<()> {
        let mut map = self.inner.lock();
        if let Some(entry) = map.get_mut(pid) {
            entry.events.retain(|(seq, _)| *seq > seq_nr);
        }
        Ok(())
    }

    async fn copy_snapshot(&self, from: &PersistenceId, to: &PersistenceId) -> JournalResult<()> {
        let mut map = self.inner.lock();
        let snapshot = map
            .get(from)
            .and_then(|e| e.snapshot.clone())
            .ok_or_else(|| JournalError::Backend(format!("no snapshot for '{from}'")))?;
        let seq = snapshot.1;
        map.insert(
            to.clone(),
            Entry {
                events: Vec::new(),
                last_seq: seq,
                snapshot: Some(snapshot),
            },
        );
        Ok(())
    }

    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()> {
        self.inner.lock().remove(pid);
        Ok(())
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

    fn pid(id: &str) -> PersistenceId {
        PersistenceId::new("t", id)
    }

    async fn drain(j: &InMemoryJournal, id: &str, after: u64) -> Vec<Vec<u8>> {
        let mut s = j.replay(&pid(id), after).await;
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        let j = InMemoryJournal::new();
        j.persist(&pid("a"), &[vec![1], vec![2], vec![3]])
            .await
            .unwrap();
        assert_eq!(drain(&j, "a", 0).await, vec![vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn logs_are_namespaced_by_kind() {
        let j = InMemoryJournal::new();
        j.persist(&PersistenceId::new("workflow", "x"), &[vec![1]])
            .await
            .unwrap();
        j.persist(&PersistenceId::new("agent", "x"), &[vec![2]])
            .await
            .unwrap();
        // Same id, different kind → separate logs.
        let mut wf = j.replay(&PersistenceId::new("workflow", "x"), 0).await;
        let mut ag = j.replay(&PersistenceId::new("agent", "x"), 0).await;
        assert_eq!(wf.next().await.unwrap().unwrap(), vec![1]);
        assert_eq!(ag.next().await.unwrap().unwrap(), vec![2]);
    }

    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        let j = InMemoryJournal::new();
        j.persist(&pid("a"), &[vec![1], vec![2], vec![3]])
            .await
            .unwrap();
        assert_eq!(drain(&j, "a", 1).await, vec![vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn snapshot_roundtrips_with_seq() {
        let j = InMemoryJournal::new();
        j.save_snapshot(&pid("a"), vec![9, 9], 5).await.unwrap();
        assert_eq!(
            j.latest_snapshot(&pid("a")).await.unwrap(),
            Some((vec![9, 9], 5))
        );
    }

    #[tokio::test]
    async fn delete_events_before_compacts() {
        let j = InMemoryJournal::new();
        j.persist(&pid("a"), &[vec![1], vec![2], vec![3]])
            .await
            .unwrap();
        j.delete_events_before(&pid("a"), 2).await.unwrap();
        assert_eq!(drain(&j, "a", 0).await, vec![vec![3]]);
    }

    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        let j = InMemoryJournal::new();
        j.persist(&pid("a"), &[vec![1], vec![2]]).await.unwrap();
        j.delete_events_before(&pid("a"), 2).await.unwrap();
        j.persist(&pid("a"), &[vec![3]]).await.unwrap();
        assert_eq!(drain(&j, "a", 2).await, vec![vec![3]]);
    }

    #[tokio::test]
    async fn copy_snapshot_seeds_new_id() {
        let j = InMemoryJournal::new();
        j.persist(&pid("src"), &[vec![1], vec![2]]).await.unwrap();
        j.save_snapshot(&pid("src"), vec![7], 2).await.unwrap();
        j.copy_snapshot(&pid("src"), &pid("dst")).await.unwrap();
        assert_eq!(
            j.latest_snapshot(&pid("dst")).await.unwrap(),
            Some((vec![7], 2))
        );
        assert!(drain(&j, "dst", 2).await.is_empty());
        j.persist(&pid("dst"), &[vec![8]]).await.unwrap();
        assert_eq!(drain(&j, "dst", 2).await, vec![vec![8]]);
    }

    #[tokio::test]
    async fn copy_snapshot_without_source_errors() {
        let j = InMemoryJournal::new();
        let err = j
            .copy_snapshot(&pid("missing"), &pid("dst"))
            .await
            .unwrap_err();
        assert!(matches!(err, JournalError::Backend(_)));
    }
}
