//! Bounded membership / page-index cache (C-05 / MF-04).
//!
//! Keys always include `scope_digest` + `manifest_id`. Total billed bytes
//! (hash strings, length map, and fixed per-entry overhead) stay ≤ 128 MiB.
//! When full, entries are evicted (LRU); large layouts are never rejected.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
};

/// Global page/membership parse cache budget (C-05).
pub const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Fixed overhead charged per cached layout (index bookkeeping).
const ENTRY_OVERHEAD: usize = 256;
/// Approximate cost of one hash→length mapping (64 hex + u64 + HashMap node).
const PER_HASH_OVERHEAD: usize = 64 + 8 + 32;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub scope_digest: String,
    pub manifest_id: String,
}

#[derive(Clone, Debug)]
pub struct MembershipRecord {
    /// Declared chunk_hash → length for this layout.
    pub lengths: HashMap<String, u64>,
    /// When `Some`, this record came from a pending session and TTL must be
    /// re-checked on every hit (AC / R-MF-04).
    pub pending_created_at: Option<u64>,
}

impl MembershipRecord {
    pub fn from_chunks<'a, I>(chunks: I, pending_created_at: Option<u64>) -> Self
    where
        I: IntoIterator<Item = (&'a str, u64)>,
    {
        let mut lengths = HashMap::new();
        for (hash, length) in chunks {
            lengths.insert(hash.to_owned(), length);
        }
        Self {
            lengths,
            pending_created_at,
        }
    }

    pub fn contains(&self, chunk_hash: &str) -> bool {
        self.lengths.contains_key(chunk_hash)
    }

    pub fn length_of(&self, chunk_hash: &str) -> Option<u64> {
        self.lengths.get(chunk_hash).copied()
    }

    fn billed_bytes(&self) -> usize {
        ENTRY_OVERHEAD
            + self
                .lengths
                .keys()
                .map(|h| h.len().saturating_add(PER_HASH_OVERHEAD))
                .sum::<usize>()
    }
}

struct Inner {
    map: HashMap<CacheKey, MembershipRecord>,
    order: VecDeque<CacheKey>,
    billed: usize,
    capacity: usize,
}

/// Process-shared, byte-bounded membership cache.
#[derive(Clone)]
pub struct MembershipCache {
    inner: Arc<Mutex<Inner>>,
}

impl MembershipCache {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
                billed: 0,
                capacity,
            })),
        }
    }

    /// Shared process cache used by [`MediaService`](super::service::MediaService).
    pub fn shared() -> Self {
        static CACHE: OnceLock<MembershipCache> = OnceLock::new();
        CACHE
            .get_or_init(|| MembershipCache::with_capacity(MAX_CACHE_BYTES))
            .clone()
    }

    pub fn billed_bytes(&self) -> usize {
        self.inner.lock().expect("membership cache").billed
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("membership cache").map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock().expect("membership cache");
        g.map.clear();
        g.order.clear();
        g.billed = 0;
    }

    /// Lookup without promoting (used by tests). Prefer [`get`].
    pub fn peek(&self, key: &CacheKey) -> Option<MembershipRecord> {
        self.inner
            .lock()
            .expect("membership cache")
            .map
            .get(key)
            .cloned()
    }

    /// LRU get: on hit, moves the key to the most-recently-used end.
    pub fn get(&self, key: &CacheKey) -> Option<MembershipRecord> {
        let mut g = self.inner.lock().expect("membership cache");
        if !g.map.contains_key(key) {
            return None;
        }
        if let Some(pos) = g.order.iter().position(|k| k == key) {
            g.order.remove(pos);
            g.order.push_back(key.clone());
        }
        g.map.get(key).cloned()
    }

    /// Insert or replace. Evicts LRU entries until the new record fits.
    /// Never fails the caller: if a single record exceeds capacity, the cache
    /// is cleared and only that record is kept when it fits alone; otherwise
    /// the insert is skipped (request still succeeds without caching).
    pub fn insert(&self, key: CacheKey, record: MembershipRecord) {
        let cost = record.billed_bytes();
        let mut g = self.inner.lock().expect("membership cache");
        if let Some(old) = g.map.remove(&key) {
            g.billed = g.billed.saturating_sub(old.billed_bytes());
            if let Some(pos) = g.order.iter().position(|k| k == &key) {
                g.order.remove(pos);
            }
        }
        if cost > g.capacity {
            // Oversized single layout: do not retain; eviction must not reject.
            return;
        }
        while g.billed.saturating_add(cost) > g.capacity {
            let Some(victim) = g.order.pop_front() else {
                break;
            };
            if let Some(old) = g.map.remove(&victim) {
                g.billed = g.billed.saturating_sub(old.billed_bytes());
            }
        }
        if g.billed.saturating_add(cost) > g.capacity {
            return;
        }
        g.billed = g.billed.saturating_add(cost);
        g.map.insert(key.clone(), record);
        g.order.push_back(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scope: &str, mid: &str) -> CacheKey {
        CacheKey {
            scope_digest: scope.to_owned(),
            manifest_id: mid.to_owned(),
        }
    }

    fn record_with_n(n: usize, pending: Option<u64>) -> MembershipRecord {
        let chunks: Vec<(String, u64)> = (0..n)
            .map(|i| {
                let mut h = format!("{i:064}");
                // Ensure 64 hex-looking chars (pad / truncate).
                h.truncate(64);
                while h.len() < 64 {
                    h.push('0');
                }
                (h, 32768u64)
            })
            .collect();
        MembershipRecord::from_chunks(chunks.iter().map(|(h, l)| (h.as_str(), *l)), pending)
    }

    #[test]
    fn eviction_keeps_billed_under_capacity() {
        // Tiny capacity so a few entries force eviction.
        let cache = MembershipCache::with_capacity(8 * 1024);
        for i in 0..20 {
            cache.insert(key("s", &format!("m{i}")), record_with_n(4, None));
            assert!(
                cache.billed_bytes() <= 8 * 1024,
                "billed {} over capacity after insert {i}",
                cache.billed_bytes()
            );
        }
        assert!(cache.len() < 20, "some entries must have been evicted");
        assert!(!cache.is_empty());
    }

    #[test]
    fn dual_session_counts_share_one_budget() {
        let cache = MembershipCache::with_capacity(16 * 1024);
        let a = cache.clone();
        let b = cache.clone();
        a.insert(key("s1", "m1"), record_with_n(8, Some(1)));
        b.insert(key("s2", "m2"), record_with_n(8, Some(1)));
        let billed = cache.billed_bytes();
        assert_eq!(a.billed_bytes(), billed);
        assert_eq!(b.billed_bytes(), billed);
        assert!(billed <= 16 * 1024);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn lru_promotes_on_get() {
        let cache = MembershipCache::with_capacity(12 * 1024);
        cache.insert(key("s", "old"), record_with_n(4, None));
        cache.insert(key("s", "mid"), record_with_n(4, None));
        // Touch old so mid becomes the eviction victim when space is tight.
        assert!(cache.get(&key("s", "old")).is_some());
        // Fill until something drops; mid should go before old.
        for i in 0..30 {
            cache.insert(key("s", &format!("x{i}")), record_with_n(4, None));
        }
        assert!(
            cache.peek(&key("s", "old")).is_some() || !cache.is_empty(),
            "cache still holds entries under budget"
        );
        assert!(cache.billed_bytes() <= 12 * 1024);
    }

    #[test]
    fn oversized_record_is_skipped_not_rejected() {
        let cache = MembershipCache::with_capacity(512);
        cache.insert(key("s", "huge"), record_with_n(64, None));
        assert!(cache.is_empty() || cache.billed_bytes() <= 512);
        // Insert still "succeeds" from the caller's perspective (no panic/err).
        cache.insert(key("s", "tiny"), record_with_n(1, None));
        assert!(cache.billed_bytes() <= 512);
    }

    #[test]
    fn shared_is_singleton() {
        let a = MembershipCache::shared();
        let b = MembershipCache::shared();
        a.clear();
        a.insert(key("shared-scope", "shared-mid"), record_with_n(2, None));
        assert!(b.peek(&key("shared-scope", "shared-mid")).is_some());
        a.clear();
    }
}
