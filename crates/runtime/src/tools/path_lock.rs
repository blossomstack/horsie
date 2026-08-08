//! One in-flight write per file.
//!
//! A turn's tool calls all run concurrently — the agent loop drives the whole
//! batch through a single `join_all` — and every write tool here is a
//! read-modify-write: `find_and_replace` reads the file, substitutes, and puts
//! the whole thing back. Two edits to one file in the same batch therefore both
//! read the original, and whichever writes second erases the other's work. Both
//! calls report `Replaced 1 occurrence(s)`, because neither can see the other,
//! so the loss surfaces much later as an edit that "didn't take".
//!
//! Serializing on the resolved path makes the second edit read what the first
//! wrote, which is what the batch already assumed was happening. It does not
//! order the two — a batch is unordered by construction — but an ordering that
//! makes the later `find` string unmatchable now fails loudly instead of
//! silently discarding a write.
//!
//! Only the writers take a lock. A read racing a write returns one version or
//! the other and loses nothing, so making readers wait would buy correctness
//! that is already there at the cost of latency that isn't.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// Live locks, keyed by resolved path.
///
/// Held by `Weak` so an entry evaporates once the last writer drops its `Arc`.
/// A long-lived runtime touches thousands of files, and a map that only ever
/// grew would keep a mutex for every one of them for the life of the process.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The lock guarding `path`, created on first use.
///
/// `path` is what the write tools themselves resolve — `working_dir.join(input
/// .path)` — normalized lexically so `src/x.rs` and `./src/x.rs` do not end up
/// with a lock each. Symlinks are deliberately not resolved: that requires the
/// file to exist, and `write_file` is allowed to create one.
pub fn for_path(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let key = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    // A poisoned map is still a coherent map — the guarded section is a lookup
    // and an insert, neither of which can leave it half-updated.
    let mut locks = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(live) = locks.get(&key).and_then(Weak::upgrade) {
        return live;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    // Dead entries are reachable only from here, so they are swept on insert
    // rather than by a timer that would exist for nothing else.
    locks.retain(|_, held| held.strong_count() > 0);
    lock
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_same_file_gets_the_same_lock() {
        let dir = std::env::temp_dir().join("horsie-path-lock-same");
        let a = for_path(&dir.join("f.rs"));
        let b = for_path(&dir.join("./f.rs"));
        assert!(Arc::ptr_eq(&a, &b), "one file, two locks");
    }

    #[test]
    fn different_files_get_different_locks() {
        let dir = std::env::temp_dir().join("horsie-path-lock-diff");
        let a = for_path(&dir.join("a.rs"));
        let b = for_path(&dir.join("b.rs"));
        assert!(!Arc::ptr_eq(&a, &b), "two files sharing a lock serializes");
    }

    /// The map must not grow by one entry per file edited, forever.
    #[test]
    fn a_dropped_lock_leaves_no_entry_behind() {
        let key = std::env::temp_dir().join("horsie-path-lock-reclaim/f.rs");
        drop(for_path(&key));
        // The sweep runs on insert, so provoke one.
        drop(for_path(
            &std::env::temp_dir().join("horsie-path-lock-reclaim/g.rs"),
        ));
        let locks = LOCKS.lock().unwrap();
        let absolute = std::path::absolute(&key).unwrap();
        assert!(!locks.contains_key(&absolute), "dead entry retained");
    }
}
