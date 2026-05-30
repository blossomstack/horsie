use crate::error::JournalError;
use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use std::collections::HashMap;

/// Result alias for journal operations.
pub type JournalResult<T> = Result<T, JournalError>;

/// Append-only event log with snapshot support.
///
/// Events and snapshots are opaque byte blobs — serialization is the caller's
/// concern, keeping the journal free of any domain types. Sequence numbers are
/// 1-based and monotonic per `id`; an event's sequence number is stable for the
/// life of the log even after older events are compacted away.
#[async_trait]
pub trait Journal: Send + Sync + 'static {
    /// Append `events` to the log for `id`, assigning each the next sequence number.
    async fn persist(&self, id: &str, events: &[Vec<u8>]) -> JournalResult<()>;

    /// Stream every event for `id` whose sequence number is strictly greater than
    /// `after_seq`, in ascending sequence order.
    async fn replay(&self, id: &str, after_seq: u64) -> BoxStream<'_, JournalResult<Vec<u8>>>;

    /// Store `state` as the snapshot for `id`, taken at sequence `seq_nr` (the
    /// sequence number of the last event folded into it). Replaces any prior snapshot.
    async fn save_snapshot(&self, id: &str, state: Vec<u8>, seq_nr: u64) -> JournalResult<()>;

    /// Return the latest snapshot for `id` as `(state, seq_nr)`, if any.
    async fn latest_snapshot(&self, id: &str) -> JournalResult<Option<(Vec<u8>, u64)>>;

    /// Drop all events for `id` with sequence number less than or equal to `seq_nr`.
    async fn delete_events_before(&self, id: &str, seq_nr: u64) -> JournalResult<()>;

    /// Copy the snapshot from `from_id` onto `to_id`. `to_id` keeps the source's
    /// snapshot sequence number and starts with an empty event log, so a fresh
    /// actor recovers the copied state and continues numbering from there.
    async fn copy_snapshot(&self, from_id: &str, to_id: &str) -> JournalResult<()>;

    /// Remove all persisted state for `id`. Primarily a test helper.
    async fn clear(&self, id: &str) -> JournalResult<()>;
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
    inner: Mutex<HashMap<String, Entry>>,
}

impl InMemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Journal for InMemoryJournal {
    async fn persist(&self, id: &str, events: &[Vec<u8>]) -> JournalResult<()> {
        let mut map = self.inner.lock();
        let entry = map.entry(id.to_string()).or_default();
        for bytes in events {
            entry.last_seq += 1;
            entry.events.push((entry.last_seq, bytes.clone()));
        }
        Ok(())
    }

    async fn replay(&self, id: &str, after_seq: u64) -> BoxStream<'_, JournalResult<Vec<u8>>> {
        let items: Vec<JournalResult<Vec<u8>>> = {
            let map = self.inner.lock();
            map.get(id)
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

    async fn save_snapshot(&self, id: &str, state: Vec<u8>, seq_nr: u64) -> JournalResult<()> {
        let mut map = self.inner.lock();
        let entry = map.entry(id.to_string()).or_default();
        entry.last_seq = entry.last_seq.max(seq_nr);
        entry.snapshot = Some((state, seq_nr));
        Ok(())
    }

    async fn latest_snapshot(&self, id: &str) -> JournalResult<Option<(Vec<u8>, u64)>> {
        Ok(self.inner.lock().get(id).and_then(|e| e.snapshot.clone()))
    }

    async fn delete_events_before(&self, id: &str, seq_nr: u64) -> JournalResult<()> {
        let mut map = self.inner.lock();
        if let Some(entry) = map.get_mut(id) {
            entry.events.retain(|(seq, _)| *seq > seq_nr);
        }
        Ok(())
    }

    async fn copy_snapshot(&self, from_id: &str, to_id: &str) -> JournalResult<()> {
        let mut map = self.inner.lock();
        let snapshot = map
            .get(from_id)
            .and_then(|e| e.snapshot.clone())
            .ok_or_else(|| JournalError::Backend(format!("no snapshot for '{from_id}'")))?;
        let seq = snapshot.1;
        map.insert(
            to_id.to_string(),
            Entry {
                events: Vec::new(),
                last_seq: seq,
                snapshot: Some(snapshot),
            },
        );
        Ok(())
    }

    async fn clear(&self, id: &str) -> JournalResult<()> {
        self.inner.lock().remove(id);
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

    async fn drain(j: &InMemoryJournal, id: &str, after: u64) -> Vec<Vec<u8>> {
        let mut s = j.replay(id, after).await;
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        let j = InMemoryJournal::new();
        j.persist("a", &[vec![1], vec![2], vec![3]]).await.unwrap();
        let events = drain(&j, "a", 0).await;
        assert_eq!(events, vec![vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        let j = InMemoryJournal::new();
        j.persist("a", &[vec![1], vec![2], vec![3]]).await.unwrap();
        let events = drain(&j, "a", 1).await;
        assert_eq!(events, vec![vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn snapshot_roundtrips_with_seq() {
        let j = InMemoryJournal::new();
        j.save_snapshot("a", vec![9, 9], 5).await.unwrap();
        assert_eq!(j.latest_snapshot("a").await.unwrap(), Some((vec![9, 9], 5)));
    }

    #[tokio::test]
    async fn delete_events_before_compacts() {
        let j = InMemoryJournal::new();
        j.persist("a", &[vec![1], vec![2], vec![3]]).await.unwrap();
        j.delete_events_before("a", 2).await.unwrap();
        // Sequence numbers are preserved: only seq 3 remains.
        let events = drain(&j, "a", 0).await;
        assert_eq!(events, vec![vec![3]]);
    }

    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        let j = InMemoryJournal::new();
        j.persist("a", &[vec![1], vec![2]]).await.unwrap();
        j.delete_events_before("a", 2).await.unwrap();
        j.persist("a", &[vec![3]]).await.unwrap();
        // seq 3 is the only event and replay(after=2) yields it.
        let events = drain(&j, "a", 2).await;
        assert_eq!(events, vec![vec![3]]);
    }

    #[tokio::test]
    async fn copy_snapshot_seeds_new_id() {
        let j = InMemoryJournal::new();
        j.persist("src", &[vec![1], vec![2]]).await.unwrap();
        j.save_snapshot("src", vec![7], 2).await.unwrap();
        j.copy_snapshot("src", "dst").await.unwrap();
        assert_eq!(j.latest_snapshot("dst").await.unwrap(), Some((vec![7], 2)));
        // No events to replay on the copy.
        assert!(drain(&j, "dst", 2).await.is_empty());
        // New events on dst continue from seq 3.
        j.persist("dst", &[vec![8]]).await.unwrap();
        assert_eq!(drain(&j, "dst", 2).await, vec![vec![8]]);
    }

    #[tokio::test]
    async fn copy_snapshot_without_source_errors() {
        let j = InMemoryJournal::new();
        let err = j.copy_snapshot("missing", "dst").await.unwrap_err();
        assert!(matches!(err, JournalError::Backend(_)));
    }
}
