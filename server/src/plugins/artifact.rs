//! Content-addressed zip artifact store on the data dir: one `<hash>.zip` per
//! bundle version. The DB holds metadata + hash; these are the bytes the
//! runtime fetches. GC removes artifacts no `plugins` row references.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;

pub struct ArtifactStore {
    dir: PathBuf,
}

impl ArtifactStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The on-disk path for a hash's artifact (may not exist).
    pub fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.zip"))
    }

    pub fn exists(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    /// Write bytes atomically (temp file + rename), creating the dir as needed.
    pub fn write(&self, hash: &str, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{hash}.zip.tmp"));
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, self.path(hash))
    }

    /// Delete every `<hash>.zip` whose hash is not in `keep`.
    pub fn gc(&self, keep: &HashSet<String>) -> io::Result<()> {
        if !self.dir.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("zip")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !keep.contains(stem)
            {
                let _ = fs::remove_file(&path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn write_path_exists_and_gc_keeps_only_referenced() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(tmp.path());
        store.write("aaa", b"one").unwrap();
        store.write("bbb", b"two").unwrap();
        assert!(store.exists("aaa"));
        assert!(store.exists("bbb"));
        assert_eq!(fs::read(store.path("aaa")).unwrap(), b"one");

        let keep: HashSet<String> = ["aaa".to_string()].into_iter().collect();
        store.gc(&keep).unwrap();
        assert!(store.exists("aaa"));
        assert!(!store.exists("bbb"));
    }
}
