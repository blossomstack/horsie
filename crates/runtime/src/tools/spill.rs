use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const MAX_FILES_PER_AGENT: usize = 20;
const MAX_BYTES_PER_AGENT: u64 = 50 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) struct SpillStore {
    pub(crate) root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
    lock: tokio::sync::Mutex<()>,
    max_files: usize,
    max_bytes: u64,
    max_age: Duration,
}

impl SpillStore {
    pub(crate) fn temporary() -> Option<Self> {
        let temporary = tempfile::Builder::new()
            .prefix("horsie-tool-output-")
            .tempdir()
            .ok()?;
        Some(Self {
            root: temporary.path().to_path_buf(),
            _temporary: Some(temporary),
            lock: tokio::sync::Mutex::new(()),
            max_files: MAX_FILES_PER_AGENT,
            max_bytes: MAX_BYTES_PER_AGENT,
            max_age: MAX_AGE,
        })
    }

    #[cfg(test)]
    fn with_policy(root: PathBuf, max_files: usize, max_bytes: u64, max_age: Duration) -> Self {
        Self {
            root,
            _temporary: None,
            lock: tokio::sync::Mutex::new(()),
            max_files,
            max_bytes,
            max_age,
        }
    }

    pub(super) async fn preserve(
        &self,
        agent: &str,
        call_id: &str,
        body: &[u8],
    ) -> Option<(String, u64)> {
        let _guard = self.lock.lock().await;
        let body_bytes = u64::try_from(body.len()).ok()?;
        if body_bytes > self.max_bytes {
            return None;
        }
        let directory = self.root.join(super::safe_name(agent));
        tokio::fs::create_dir_all(&directory).await.ok()?;
        let destination = directory.join(format!("{}.txt", super::safe_name(call_id)));
        self.prune(&directory, &destination, body_bytes).await;
        tokio::fs::write(&destination, body).await.ok()?;
        Some((destination.to_string_lossy().into_owned(), body_bytes))
    }

    async fn prune(&self, directory: &Path, destination: &Path, incoming_bytes: u64) {
        let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
            return;
        };
        let now = SystemTime::now();
        let mut retained = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path == destination {
                let _ = tokio::fs::remove_file(path).await;
                continue;
            }
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if now.duration_since(modified).unwrap_or(Duration::ZERO) > self.max_age {
                let _ = tokio::fs::remove_file(path).await;
            } else {
                retained.push((path, modified, metadata.len()));
            }
        }
        retained.sort_by_key(|(_, modified, _)| *modified);
        let mut bytes = retained.iter().map(|(_, _, len)| *len).sum::<u64>();
        let mut files = retained.len();
        for (path, _, len) in retained {
            if files < self.max_files && bytes.saturating_add(incoming_bytes) <= self.max_bytes {
                break;
            }
            if tokio::fs::remove_file(path).await.is_ok() {
                files = files.saturating_sub(1);
                bytes = bytes.saturating_sub(len);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn quota_removes_the_oldest_spill_for_that_agent() {
        let root = tempfile::tempdir().unwrap();
        let store = SpillStore::with_policy(root.path().to_path_buf(), 2, 12, MAX_AGE);
        let first = store.preserve("agent", "one", b"123456").await.unwrap();
        store.preserve("agent", "two", b"123456").await.unwrap();
        store.preserve("agent", "three", b"123456").await.unwrap();
        assert!(!Path::new(&first.0).exists());
        assert!(root.path().join("agent/two.txt").exists());
        assert!(root.path().join("agent/three.txt").exists());
    }

    #[tokio::test]
    async fn spills_are_isolated_and_oversized_files_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = SpillStore::with_policy(root.path().to_path_buf(), 2, 4, MAX_AGE);
        assert!(store.preserve("a", "one", b"1234").await.is_some());
        assert!(store.preserve("b", "one", b"1234").await.is_some());
        assert!(store.preserve("a", "huge", b"12345").await.is_none());
        assert!(root.path().join("a/one.txt").exists());
        assert!(root.path().join("b/one.txt").exists());
    }
}
