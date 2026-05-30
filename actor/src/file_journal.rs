use crate::error::JournalError;
use crate::journal::{Journal, JournalResult};
use crate::persistence_id::PersistenceId;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::stream::{self, BoxStream, StreamExt};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Filesystem-backed [`Journal`]: one base64-encoded **batch** record per line
/// under `<root>/actors/<kind>/<id>/journal.jsonl` (the [`PersistenceId`]'s kind
/// then instance id).
///
/// Each call to [`persist`](FileJournal::persist) appends exactly one line — a
/// base64-encoded JSON array of the batch's per-event base64 strings. Framing the
/// whole batch as a single line makes a torn final write (a process killed before
/// `persist` returned `Ok`) drop the *entire* partial batch on recovery, which is
/// what the actor runtime requires: it advances `seq_nr` only after `persist`
/// returns `Ok`, so a half-written batch was never counted and must not be
/// half-applied.
///
/// Snapshots are no-op (CLI runs are short, so recovery always full-replays the
/// log); the layout reserves `actors/<kind>/<id>/snapshot.jsonl` if snapshotting is
/// ever enabled. Base64 keeps the file strictly line-delimited regardless of the
/// opaque payload bytes.
pub struct FileJournal {
    root: PathBuf,
}

impl FileJournal {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn journal_path(&self, pid: &PersistenceId) -> PathBuf {
        self.root
            .join("actors")
            .join(&pid.kind)
            .join(&pid.id)
            .join("journal.jsonl")
    }
}

#[async_trait]
impl Journal for FileJournal {
    async fn persist(&self, pid: &PersistenceId, events: &[Vec<u8>]) -> JournalResult<()> {
        if events.is_empty() {
            return Ok(());
        }
        let path = self.journal_path(pid);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| JournalError::Backend(e.to_string()))?;
        }
        // One line per batch: base64( JSON([ base64(event0), base64(event1), ... ]) ).
        let encoded_events: Vec<String> = events.iter().map(|e| STANDARD.encode(e)).collect();
        let json = serde_json::to_vec(&encoded_events)
            .map_err(|e| JournalError::Serialization(e.to_string()))?;
        let mut line = STANDARD.encode(&json);
        line.push('\n');

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        file.write_all(line.as_bytes())
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        file.flush()
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        file.sync_all()
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<Vec<u8>>> {
        let items = decode_after(&self.journal_path(pid), after_seq);
        stream::iter(items).boxed()
    }

    async fn save_snapshot(
        &self,
        _pid: &PersistenceId,
        _state: Vec<u8>,
        _seq_nr: u64,
    ) -> JournalResult<()> {
        Ok(())
    }

    async fn latest_snapshot(&self, _pid: &PersistenceId) -> JournalResult<Option<(Vec<u8>, u64)>> {
        Ok(None)
    }

    async fn delete_events_before(&self, _pid: &PersistenceId, _seq_nr: u64) -> JournalResult<()> {
        Ok(())
    }

    async fn copy_snapshot(&self, _from: &PersistenceId, _to: &PersistenceId) -> JournalResult<()> {
        Ok(())
    }

    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()> {
        match std::fs::remove_file(self.journal_path(pid)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(JournalError::Backend(e.to_string())),
        }
    }
}

/// Decode complete batch lines (those terminated by `\n`) in order, assigning each
/// contained event a 1-based sequence number; yield those whose seq > `after_seq`.
/// A trailing partial line (a torn write that never returned `Ok`) is dropped; an
/// undecodable complete line is treated as a corruption boundary and stops replay.
fn decode_after(path: &Path, after_seq: u64) -> Vec<JournalResult<Vec<u8>>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let parts: Vec<&str> = content.split('\n').collect();
    // All parts except the last were terminated by `\n` (complete lines). The last
    // part is whatever followed the final `\n` (a torn remainder, or empty) — drop it.
    let complete = if parts.is_empty() {
        &[][..]
    } else {
        &parts[..parts.len() - 1]
    };

    let mut out = Vec::new();
    let mut seq: u64 = 0;
    for line in complete {
        if line.is_empty() {
            break; // corruption boundary — stop rather than misnumber later events
        }
        let json = match STANDARD.decode(line) {
            Ok(bytes) => bytes,
            Err(_) => break,
        };
        let batch: Vec<String> = match serde_json::from_slice(&json) {
            Ok(b) => b,
            Err(_) => break,
        };
        for event_b64 in &batch {
            match STANDARD.decode(event_b64) {
                Ok(bytes) => {
                    seq += 1;
                    if seq > after_seq {
                        out.push(Ok(bytes));
                    }
                }
                Err(_) => return out,
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn pid(id: &str) -> PersistenceId {
        PersistenceId::new("test", id)
    }

    async fn drain(j: &FileJournal, id: &str, after: u64) -> Vec<Vec<u8>> {
        let mut s = j.replay(&pid(id), after).await;
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn persist_then_replay_roundtrips_in_order() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[vec![1, 2], vec![3], vec![4, 5, 6]])
            .await
            .unwrap();
        assert_eq!(
            drain(&j, "r1", 0).await,
            vec![vec![1, 2], vec![3], vec![4, 5, 6]]
        );
    }

    #[tokio::test]
    async fn lays_out_under_actors_kind_id() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&PersistenceId::new("workflow", "run1"), &[vec![1]])
            .await
            .unwrap();
        j.persist(&PersistenceId::new("agent", "sess1"), &[vec![2]])
            .await
            .unwrap();
        assert!(
            dir.path()
                .join("actors/workflow/run1/journal.jsonl")
                .exists()
        );
        assert!(dir.path().join("actors/agent/sess1/journal.jsonl").exists());
    }

    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[vec![1]]).await.unwrap();
        j.persist(&pid("r1"), &[vec![2], vec![3]]).await.unwrap();
        assert_eq!(drain(&j, "r1", 1).await, vec![vec![2], vec![3]]);
        assert_eq!(drain(&j, "r1", 2).await, vec![vec![3]]);
    }

    #[tokio::test]
    async fn append_across_calls_keeps_sequence() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[vec![1]]).await.unwrap();
        j.persist(&pid("r1"), &[vec![2], vec![3]]).await.unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn snapshots_are_noop_replay_is_full() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[vec![1], vec![2]]).await.unwrap();
        j.save_snapshot(&pid("r1"), vec![9, 9], 2).await.unwrap();
        assert_eq!(j.latest_snapshot(&pid("r1")).await.unwrap(), None);
        j.delete_events_before(&pid("r1"), 2).await.unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![1], vec![2]]);
    }

    #[tokio::test]
    async fn empty_batch_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[]).await.unwrap();
        assert!(drain(&j, "r1", 0).await.is_empty());
    }

    #[tokio::test]
    async fn torn_trailing_batch_is_dropped_whole() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[vec![1], vec![2]]).await.unwrap();
        let path = j.journal_path(&pid("r1"));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"c29tZS1wYXJ0aWFs").unwrap(); // valid base64 prefix, no '\n'
        f.flush().unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![1], vec![2]]);
    }

    #[tokio::test]
    async fn garbage_trailing_line_is_ignored() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[vec![7]]).await.unwrap();
        let path = j.journal_path(&pid("r1"));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"!!!not base64!!!").unwrap();
        f.flush().unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![7]]);
    }

    #[tokio::test]
    async fn replay_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        assert!(drain(&j, "ghost", 0).await.is_empty());
    }

    #[tokio::test]
    async fn clear_removes_log() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist(&pid("r1"), &[vec![1]]).await.unwrap();
        j.clear(&pid("r1")).await.unwrap();
        assert!(drain(&j, "r1", 0).await.is_empty());
        j.clear(&pid("r1")).await.unwrap(); // missing log is a no-op
    }

    #[tokio::test]
    async fn handles_binary_payload_with_newline_bytes() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        let payload = vec![b'a', b'\n', b'b', 0, 255, b'\n'];
        j.persist(&pid("r1"), std::slice::from_ref(&payload))
            .await
            .unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![payload]);
    }
}
