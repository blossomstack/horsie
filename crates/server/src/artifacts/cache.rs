//! A byte-bounded LRU over artifact bytes.
//!
//! **There is no invalidation, and there never will be.** An artifact's id is
//! the sha256 of its bytes, so a cached entry cannot become wrong: either the
//! bytes hash to that id or they are not that artifact. Nothing in this file
//! watches for writes, and nothing that stores an artifact has to remember to
//! call an `invalidate`. The only reason an entry ever leaves is that the cache
//! is full.
//!
//! **Bounded by total bytes, not by entry count.** Artifacts are images and
//! PDFs, up to `MAX_ARTIFACT_BYTES` each; a thousand-entry cache is somewhere
//! between a few megabytes and ten gigabytes, which is not a bound at all. The
//! budget here is the number an operator actually cares about — resident
//! memory.
//!
//! Hand-rolled rather than a crate: the whole policy is "drop the entry with
//! the oldest use stamp until we are under budget", which is fifty lines and no
//! dependency.

use super::blobs::BlobKey;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// How many bytes of artifacts to keep resident by default.
///
/// 256 MB: large enough that a conversation's images stay hot for the whole
/// session, small enough to be an unremarkable share of a modest server.
pub const DEFAULT_BUDGET_BYTES: usize = 256 * 1024 * 1024;

struct Entry {
    bytes: Arc<Vec<u8>>,
    /// Monotonic use stamp. The LRU order *is* this number — there is no
    /// second structure holding an ordering that could drift out of step with
    /// the map, which is the classic hand-rolled-LRU bug.
    used: u64,
}

struct Inner {
    entries: HashMap<BlobKey, Entry>,
    resident: usize,
    clock: u64,
}

/// Artifact bytes held in memory, shared by every task that reads them.
///
/// Keyed by [`BlobKey`] — project *and* id — rather than by id alone. Content
/// addressing means the same bytes in two projects have the same id, so an
/// id-keyed cache would be a way to read another project's artifact by naming
/// its hash, without ever touching a row that says who owns it. The project in
/// the key is what makes "look in the cache first" safe.
pub struct ArtifactCache {
    inner: Mutex<Inner>,
    budget: usize,
}

impl ArtifactCache {
    /// A cache holding at most `budget` bytes of artifacts.
    pub fn new(budget: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                resident: 0,
                clock: 0,
            }),
            budget,
        }
    }

    /// The bytes for `key`, if they are resident. Counts as a use.
    pub fn get(&self, key: &BlobKey) -> Option<Arc<Vec<u8>>> {
        let mut inner = self.lock();
        let clock = inner.clock.wrapping_add(1);
        inner.clock = clock;
        let entry = inner.entries.get_mut(key)?;
        entry.used = clock;
        Some(Arc::clone(&entry.bytes))
    }

    /// Make `bytes` resident, evicting least-recently-used entries until the
    /// cache is back under budget.
    ///
    /// An entry larger than the whole budget is simply not cached: admitting it
    /// would evict everything else and still leave the cache over budget.
    pub fn insert(&self, key: BlobKey, bytes: Arc<Vec<u8>>) {
        if bytes.len() > self.budget {
            return;
        }
        let mut inner = self.lock();
        let clock = inner.clock.wrapping_add(1);
        inner.clock = clock;
        let size = bytes.len();
        if let Some(previous) = inner.entries.insert(key, Entry { bytes, used: clock }) {
            inner.resident -= previous.bytes.len();
        }
        inner.resident += size;
        while inner.resident > self.budget {
            let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(evicted) = inner.entries.remove(&oldest) {
                inner.resident -= evicted.bytes.len();
            }
        }
    }

    /// Forget these keys — the artifacts behind them have been deleted.
    ///
    /// Not invalidation: the bytes were still correct. This is only so a
    /// released session's images stop occupying the budget.
    pub fn forget(&self, keys: &[BlobKey]) {
        let mut inner = self.lock();
        for key in keys {
            if let Some(evicted) = inner.entries.remove(key) {
                inner.resident -= evicted.bytes.len();
            }
        }
    }

    /// Bytes currently resident.
    pub fn resident_bytes(&self) -> usize {
        self.lock().resident
    }

    /// A poisoned lock is recovered from rather than propagated: every mutation
    /// above is a few infallible map operations, so there is no panic between
    /// two writes that could leave `resident` disagreeing with `entries`. A
    /// cache that refuses to serve because an unrelated task panicked would
    /// turn a cosmetic failure into an outage.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for ArtifactCache {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::projects::ProjectId;

    fn key(project: &str, id: &str) -> BlobKey {
        BlobKey::new(ProjectId::new(project), id)
    }

    fn bytes(n: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![0u8; n])
    }

    #[test]
    fn a_stored_entry_reads_back() {
        let cache = ArtifactCache::new(1024);
        cache.insert(key("p1", "a"), Arc::new(b"hello".to_vec()));
        assert_eq!(
            cache.get(&key("p1", "a")).as_deref(),
            Some(&b"hello".to_vec())
        );
        assert!(cache.get(&key("p1", "b")).is_none());
    }

    /// The same id in another project is a different entry, not a hit.
    #[test]
    fn the_project_is_part_of_the_key() {
        let cache = ArtifactCache::new(1024);
        cache.insert(key("p1", "shared"), Arc::new(b"secret".to_vec()));
        assert!(cache.get(&key("p2", "shared")).is_none());
    }

    #[test]
    fn exceeding_the_budget_evicts_the_least_recently_used() {
        let cache = ArtifactCache::new(300);
        cache.insert(key("p1", "a"), bytes(100));
        cache.insert(key("p1", "b"), bytes(100));
        cache.insert(key("p1", "c"), bytes(100));
        assert_eq!(cache.resident_bytes(), 300);

        // Touch `a`, making `b` the oldest use.
        assert!(cache.get(&key("p1", "a")).is_some());
        cache.insert(key("p1", "d"), bytes(100));

        assert!(
            cache.get(&key("p1", "b")).is_none(),
            "b was least recently used"
        );
        assert!(cache.get(&key("p1", "a")).is_some());
        assert!(cache.get(&key("p1", "c")).is_some());
        assert!(cache.get(&key("p1", "d")).is_some());
        assert_eq!(cache.resident_bytes(), 300);
    }

    /// One big entry evicts as many small ones as it takes — the bound is
    /// bytes, not entries.
    #[test]
    fn a_large_entry_evicts_several_small_ones() {
        let cache = ArtifactCache::new(300);
        cache.insert(key("p1", "a"), bytes(50));
        cache.insert(key("p1", "b"), bytes(50));
        cache.insert(key("p1", "c"), bytes(50));
        cache.insert(key("p1", "big"), bytes(250));

        assert!(cache.get(&key("p1", "big")).is_some());
        assert!(cache.resident_bytes() <= 300);
        let survivors = [key("p1", "a"), key("p1", "b"), key("p1", "c")]
            .iter()
            .filter(|k| cache.get(k).is_some())
            .count();
        assert_eq!(survivors, 1, "the two oldest go, the newest stays");
    }

    #[test]
    fn an_entry_bigger_than_the_budget_is_not_admitted() {
        let cache = ArtifactCache::new(100);
        cache.insert(key("p1", "a"), bytes(50));
        cache.insert(key("p1", "huge"), bytes(500));
        assert!(cache.get(&key("p1", "huge")).is_none());
        assert!(
            cache.get(&key("p1", "a")).is_some(),
            "and it evicted nothing"
        );
        assert_eq!(cache.resident_bytes(), 50);
    }

    #[test]
    fn re_inserting_a_key_does_not_double_count_its_bytes() {
        let cache = ArtifactCache::new(1000);
        cache.insert(key("p1", "a"), bytes(100));
        cache.insert(key("p1", "a"), bytes(100));
        assert_eq!(cache.resident_bytes(), 100);
    }

    #[test]
    fn forgetting_frees_the_budget() {
        let cache = ArtifactCache::new(1000);
        cache.insert(key("p1", "a"), bytes(100));
        cache.insert(key("p1", "b"), bytes(100));
        cache.forget(&[key("p1", "a"), key("p1", "never-there")]);
        assert!(cache.get(&key("p1", "a")).is_none());
        assert_eq!(cache.resident_bytes(), 100);
    }

    #[test]
    fn the_cache_is_shareable_across_tasks() {
        let cache = Arc::new(ArtifactCache::new(1024));
        let clone = Arc::clone(&cache);
        std::thread::spawn(move || clone.insert(key("p1", "a"), Arc::new(b"x".to_vec())))
            .join()
            .unwrap();
        assert!(cache.get(&key("p1", "a")).is_some());
    }
}
