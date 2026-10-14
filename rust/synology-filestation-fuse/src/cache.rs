use bytes::Bytes;
use moka::sync::Cache;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use synology_filestation_core::types::{InodeEntry, SynoFileInfo};

#[cfg(test)]
fn make_file_info(path: &str) -> SynoFileInfo {
    SynoFileInfo {
        name: path.rsplit('/').next().unwrap_or("").to_string(),
        path: path.to_string(),
        isdir: false,
        additional: None,
        code: None,
    }
}

pub struct InodeCache {
    next_ino: RwLock<u64>,
    /// ino → InodeEntry with TTL eviction
    by_ino: Cache<u64, Arc<InodeEntry>>,
    /// lowercase(path) → ino  (never TTL-evicted; invalidated manually on mutations)
    path_to_ino: RwLock<HashMap<String, u64>>,
    /// ino → original-case path  (never TTL-evicted; source of truth for path strings)
    ino_to_path: RwLock<HashMap<u64, String>>,
}

impl InodeCache {
    pub fn new(ttl_secs: u64) -> Self {
        let by_ino = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(ttl_secs))
            .build();
        Self {
            next_ino: RwLock::new(2), // 1 is reserved for root
            by_ino,
            path_to_ino: RwLock::new(HashMap::new()),
            ino_to_path: RwLock::new(HashMap::new()),
        }
    }

    /// Returns the existing inode for `path`, or allocates a new one.
    pub fn get_or_alloc_ino(&self, path: &str) -> u64 {
        let key = path.to_lowercase();
        {
            let map = self.path_to_ino.read().unwrap();
            if let Some(&ino) = map.get(&key) {
                return ino;
            }
        }
        let mut next = self.next_ino.write().unwrap();
        // Double-check after acquiring write lock
        let mut map = self.path_to_ino.write().unwrap();
        if let Some(&ino) = map.get(&key) {
            return ino;
        }
        let ino = *next;
        *next += 1;
        map.insert(key, ino);
        ino
    }

    /// Insert or refresh a cache entry.
    pub fn insert(&self, ino: u64, info: SynoFileInfo) {
        let path = info.path.clone();
        let entry = Arc::new(InodeEntry {
            ino,
            path: path.clone(),
            info,
        });
        self.by_ino.insert(ino, entry);
        let key = path.to_lowercase();
        self.path_to_ino.write().unwrap().insert(key, ino);
        self.ino_to_path.write().unwrap().insert(ino, path);
    }

    /// Seed inode 1 for the root directory.
    pub fn seed_root(&self, info: SynoFileInfo) {
        let path = info.path.clone();
        let entry = Arc::new(InodeEntry {
            ino: 1,
            path: path.clone(),
            info,
        });
        self.by_ino.insert(1, entry);
        let key = path.to_lowercase();
        self.path_to_ino.write().unwrap().insert(key, 1);
        self.ino_to_path.write().unwrap().insert(1, path);
    }

    /// Look up a cached entry by inode.
    pub fn get_by_ino(&self, ino: u64) -> Option<Arc<InodeEntry>> {
        self.by_ino.get(&ino)
    }

    /// Drop `ino`'s details as their TTL would, keeping its path: what a
    /// test needs to reproduce an entry that has aged out.
    #[cfg(test)]
    pub fn expire(&self, ino: u64) {
        self.by_ino.invalidate(&ino);
    }

    /// Look up the path for a given inode (even if metadata is evicted).
    pub fn get_path_for_ino(&self, ino: u64) -> Option<String> {
        // Prefer the live metadata cache (freshest data).
        if let Some(entry) = self.by_ino.get(&ino) {
            return Some(entry.path.clone());
        }
        // Fall back to ino_to_path which stores the original-case path permanently.
        self.ino_to_path.read().unwrap().get(&ino).cloned()
    }

    /// Remove all entries whose path starts with `prefix` (after mutations).
    pub fn invalidate_prefix(&self, prefix: &str) {
        let lower = prefix.to_lowercase();
        let mut map = self.path_to_ino.write().unwrap();
        let mut itp = self.ino_to_path.write().unwrap();
        let to_remove: Vec<String> = map
            .keys()
            .filter(|k| k.starts_with(&lower))
            .cloned()
            .collect();
        for path in &to_remove {
            if let Some(&ino) = map.get(path) {
                self.by_ino.invalidate(&ino);
                itp.remove(&ino);
            }
            map.remove(path);
        }
    }

    /// Remove a specific path and its metadata from the cache.
    pub fn invalidate_path(&self, path: &str) {
        let key = path.to_lowercase();
        let mut map = self.path_to_ino.write().unwrap();
        if let Some(&ino) = map.get(&key) {
            self.by_ino.invalidate(&ino);
            self.ino_to_path.write().unwrap().remove(&ino);
            map.remove(&key);
        }
    }

    /// Return the inode for `path` if it is already known, without allocating a new one.
    pub fn get_ino_for_path(&self, path: &str) -> Option<u64> {
        let key = path.to_lowercase();
        self.path_to_ino.read().unwrap().get(&key).copied()
    }

    /// Return the file size for `ino` from the metadata cache, if available.
    pub fn get_size_for_ino(&self, ino: u64) -> Option<u64> {
        self.by_ino
            .get(&ino)
            .and_then(|entry| entry.info.additional.as_ref()?.size)
    }
}

// ---------------------------------------------------------------------------
// Directory listings
// ---------------------------------------------------------------------------

/// Directory listings, keyed by the directory's path.
///
/// `readdir` used to ask the NAS every time it was called, and the kernel calls
/// it more than once per directory read: once for the entries, then again at
/// the end offset to be told there are no more. A desktop file manager polling
/// a freshly-appeared volume — which is what GIO does — turned that into a
/// sustained stream of listings against `synoscgi`, the shared CGI backend the
/// whole appliance runs on. That is the same backend the transfer throttle in
/// the core client exists to protect.
///
/// The TTL is `--cache-ttl`, the one that already governs metadata, so this
/// inherits a staleness contract the mount has always had rather than
/// inventing a second one. Our own mutations invalidate immediately; only a
/// change made from another machine waits for the TTL.
pub struct DirCache {
    /// lowercase(path) → the listing. Lowercased because DSM is
    /// case-insensitive, and two spellings of one directory must not become
    /// two listings that can disagree.
    listings: Cache<String, Arc<Vec<SynoFileInfo>>>,
    /// `--cache-ttl 0` is how somebody asks for no caching. moka reads a zero
    /// TTL as "expire immediately", which is nearly the same but still lets an
    /// entry inserted and read in one instant hit. Honouring the flag exactly
    /// is cheaper to reason about than almost honouring it.
    enabled: bool,
}

impl DirCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            listings: Cache::builder()
                .max_capacity(1_000)
                .time_to_live(Duration::from_secs(ttl_secs))
                .build(),
            enabled: ttl_secs > 0,
        }
    }

    /// Whether listings are cached at all, which `--cache-ttl 0` turns off.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn get(&self, path: &str) -> Option<Arc<Vec<SynoFileInfo>>> {
        if !self.enabled {
            return None;
        }
        self.listings.get(&path.to_lowercase())
    }

    /// Cache a listing, and hand back the shared copy.
    ///
    /// Returning it matters when caching is off: the caller still needs the
    /// entries it just fetched, and looking them back up would find nothing.
    pub fn insert(&self, path: &str, entries: Vec<SynoFileInfo>) -> Arc<Vec<SynoFileInfo>> {
        let entries = Arc::new(entries);
        if self.enabled {
            self.listings
                .insert(path.to_lowercase(), Arc::clone(&entries));
        }
        entries
    }

    /// Forget one directory's listing, after something in it changed.
    pub fn invalidate(&self, path: &str) {
        self.listings.invalidate(&path.to_lowercase());
    }

    /// Forget a directory and everything under it, for a removal or a rename.
    ///
    /// The trailing separator is what keeps `/photostudio` out of a sweep of
    /// `/photos` — the same distinction [`InodeCache::invalidate_prefix`]
    /// draws.
    pub fn invalidate_prefix(&self, prefix: &str) {
        let prefix = prefix.to_lowercase();
        let under = format!("{prefix}/");
        for (key, _) in self.listings.iter() {
            if *key == prefix || key.starts_with(&under) {
                self.listings.invalidate(key.as_str());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Block-level read cache for file data
// ---------------------------------------------------------------------------

/// Size of each cached block in bytes (256 KiB).
pub const READ_BLOCK_SIZE: u64 = 256 * 1024;

/// How long a reader waits on somebody else's in-flight block before giving
/// up on it — a backstop against a wedged FUSE worker, not a judgement about
/// the download.
///
/// Every path that claims a block releases the claim on the way out, a panic
/// or a cancelled task included, so a waiter is woken the instant the download
/// resolves either way. Reaching this deadline therefore means a download that
/// is still genuinely running, and giving up **does not** take the claim away
/// from it: freeing a live owner's claim is what turned a slow mount into a
/// stalled one, because the next reader started the same download again and
/// made the queue longer.
///
/// That reading holds only because no claim outlives the decision to run.
/// Speculation takes the budget it needs before it spawns anything and gives
/// its claims straight back when it cannot have it — see `spawn_run` — rather
/// than queueing for a permit while holding them, precisely so a waiter here
/// is never parked behind a download that has not started.
pub const BLOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// A block being downloaded, and the doorbell its waiters sleep on.
///
/// Readers used to poll for one every 5 ms, which cost a FUSE worker thread
/// its CPU and still added latency to the block it was waiting for. They now
/// sleep until the owner resolves it.
struct BlockSlot {
    /// `true` once the download finished, successfully or not. The bytes, if
    /// there are any, are in `blocks` by then.
    resolved: std::sync::Mutex<bool>,
    ready: std::sync::Condvar,
}

impl BlockSlot {
    fn new() -> Self {
        Self {
            resolved: std::sync::Mutex::new(false),
            ready: std::sync::Condvar::new(),
        }
    }
}

/// A cache of file data split into fixed-size blocks.
///
/// Key: `(ino, block_index)` — `block_index = byte_offset / READ_BLOCK_SIZE`.
/// Blocks are evicted after 5 minutes of idle time or when the capacity
/// limit is reached.  Explicit invalidation is available via [`invalidate_ino`].
pub struct ReadCache {
    blocks: Cache<(u64, u64), Bytes>,
    /// Tracks which block indices are cached for each inode so we can do
    /// targeted invalidation when a file is written or deleted.
    ino_blocks: RwLock<HashMap<u64, HashSet<u64>>>,
    /// Blocks currently being downloaded (prefetch or sync), each with the
    /// handle its waiters block on. A block here will appear in `blocks` once
    /// the download completes. Used to avoid issuing duplicate requests for
    /// the same block, and to wake whoever is waiting the moment it lands.
    in_flight: std::sync::Mutex<HashMap<(u64, u64), Arc<BlockSlot>>>,
    pub block_size: u64,
    /// How many blocks it holds at most.
    max_blocks: u64,
}

impl ReadCache {
    /// `max_blocks` = total capacity in blocks (= cache_bytes / block_size).
    pub fn new(block_size: u64, max_blocks: u64) -> Self {
        let blocks = Cache::builder()
            .max_capacity(max_blocks)
            .time_to_idle(Duration::from_secs(300))
            .build();
        Self {
            blocks,
            ino_blocks: RwLock::new(HashMap::new()),
            in_flight: std::sync::Mutex::new(HashMap::new()),
            block_size,
            max_blocks,
        }
    }

    /// How many blocks the cache holds at most.
    pub fn capacity_blocks(&self) -> u64 {
        self.max_blocks
    }

    pub fn get(&self, ino: u64, block_idx: u64) -> Option<Bytes> {
        self.blocks.get(&(ino, block_idx))
    }

    pub fn insert(&self, ino: u64, block_idx: u64, data: Bytes) {
        self.blocks.insert((ino, block_idx), data);
        self.ino_blocks
            .write()
            .unwrap()
            .entry(ino)
            .or_default()
            .insert(block_idx);
        self.resolve(ino, block_idx);
    }

    pub fn contains(&self, ino: u64, block_idx: u64) -> bool {
        self.blocks.contains_key(&(ino, block_idx))
    }

    /// Returns `true` if this call wins the race to download the block.
    /// Returns `false` if another task already claimed it — the caller should
    /// wait on [`wait_for_block`] instead.
    pub fn claim_inflight(&self, ino: u64, block_idx: u64) -> bool {
        let mut in_flight = self.in_flight.lock().unwrap();
        if in_flight.contains_key(&(ino, block_idx)) {
            return false;
        }
        in_flight.insert((ino, block_idx), Arc::new(BlockSlot::new()));
        true
    }

    /// Finish a claim and wake everyone waiting on it, whatever the outcome.
    fn resolve(&self, ino: u64, block_idx: u64) {
        let slot = self.in_flight.lock().unwrap().remove(&(ino, block_idx));
        if let Some(slot) = slot {
            *slot.resolved.lock().unwrap() = true;
            slot.ready.notify_all();
        }
    }

    /// Mark a failed download so other waiters don't spin forever.
    pub fn cancel_inflight(&self, ino: u64, block_idx: u64) {
        self.resolve(ino, block_idx);
    }

    /// Spin-wait (5 ms polls) until the block appears in cache or the
    /// in-flight marker is gone (meaning the download failed).
    /// Returns the cached bytes on success, `None` on failure.
    ///
    /// Bounded by [`BLOCK_WAIT_TIMEOUT`]: the owner of an in-flight claim is
    /// responsible for clearing it, but a task that dies without doing so (a
    /// panicking prefetch, a runtime shut down mid-download) would otherwise
    /// strand every waiter here forever — and these waiters are FUSE worker
    /// threads, so the whole mount stops answering. Giving up returns `None`,
    /// which the caller already handles as "that download failed".
    pub fn wait_for_block(&self, ino: u64, block_idx: u64) -> Option<Bytes> {
        self.wait_for_block_until(ino, block_idx, BLOCK_WAIT_TIMEOUT)
    }

    /// [`wait_for_block`](Self::wait_for_block) with an explicit deadline, so
    /// the timeout path is testable without stalling the suite.
    fn wait_for_block_until(&self, ino: u64, block_idx: u64, timeout: Duration) -> Option<Bytes> {
        // Take a reference to the doorbell and let go of the map: the owner
        // needs that lock to resolve, and holding it here would deadlock.
        let slot = self
            .in_flight
            .lock()
            .unwrap()
            .get(&(ino, block_idx))
            .cloned();
        // No claim means the download already finished — the block is either in
        // the cache or it failed.
        let Some(slot) = slot else {
            return self.blocks.get(&(ino, block_idx));
        };

        let deadline = Instant::now() + timeout;
        let mut resolved = slot.resolved.lock().unwrap();
        while !*resolved {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // The claim is deliberately left alone. It belongs to a
                // download that is still running — every path releases its
                // claim on the way out — and taking it away would only start a
                // second copy of the same work.
                tracing::warn!(
                    "read cache: gave up waiting on in-flight block ino={ino} \
                     idx={block_idx} after {timeout:?}; its download is still running"
                );
                drop(resolved);
                return self.blocks.get(&(ino, block_idx));
            }
            let (guard, _) = slot.ready.wait_timeout(resolved, remaining).unwrap();
            resolved = guard;
        }
        drop(resolved);
        self.blocks.get(&(ino, block_idx))
    }

    /// Evict a single cached block (e.g. a stale EOF sentinel).
    pub fn invalidate_block(&self, ino: u64, block_idx: u64) {
        self.blocks.invalidate(&(ino, block_idx));
        if let Some(set) = self.ino_blocks.write().unwrap().get_mut(&ino) {
            set.remove(&block_idx);
        }
    }

    /// Evict all cached blocks for `ino` (call after writing or deleting a file).
    pub fn invalidate_ino(&self, ino: u64) {
        let indices = self.ino_blocks.write().unwrap().remove(&ino);
        if let Some(set) = indices {
            for idx in set {
                self.blocks.invalidate(&(ino, idx));
                self.resolve(ino, idx);
            }
        }
        // `ino_blocks` only knows about blocks that arrived. A block still on
        // its way has a claim and waiters but no entry there, and dropping the
        // file underneath them must not leave them asleep for the deadline.
        let claimed: Vec<u64> = self
            .in_flight
            .lock()
            .unwrap()
            .keys()
            .filter(|(i, _)| *i == ino)
            .map(|(_, idx)| *idx)
            .collect();
        for idx in claimed {
            self.resolve(ino, idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // InodeCache tests
    // -----------------------------------------------------------------------

    #[test]
    fn inode_alloc_is_stable() {
        let cache = InodeCache::new(30);
        let a = cache.get_or_alloc_ino("/share/file.txt");
        let b = cache.get_or_alloc_ino("/share/file.txt");
        assert_eq!(a, b);
    }

    #[test]
    fn inode_alloc_is_case_insensitive() {
        let cache = InodeCache::new(30);
        let a = cache.get_or_alloc_ino("/Share/File.txt");
        let b = cache.get_or_alloc_ino("/share/file.txt");
        assert_eq!(a, b);
    }

    #[test]
    fn different_paths_get_different_inodes() {
        let cache = InodeCache::new(30);
        let a = cache.get_or_alloc_ino("/share/a.txt");
        let b = cache.get_or_alloc_ino("/share/b.txt");
        assert_ne!(a, b);
    }

    #[test]
    fn insert_and_get_by_ino() {
        let cache = InodeCache::new(30);
        let info = make_file_info("/share/hello.txt");
        let ino = cache.get_or_alloc_ino("/share/hello.txt");
        cache.insert(ino, info.clone());
        let entry = cache.get_by_ino(ino).unwrap();
        assert_eq!(entry.ino, ino);
        assert_eq!(entry.path, "/share/hello.txt");
    }

    #[test]
    fn seed_root_uses_inode_1() {
        let cache = InodeCache::new(30);
        let info = make_file_info("");
        cache.seed_root(info);
        let entry = cache.get_by_ino(1).unwrap();
        assert_eq!(entry.ino, 1);
    }

    #[test]
    fn get_path_for_ino_returns_correct_path() {
        let cache = InodeCache::new(30);
        let info = make_file_info("/share/doc.pdf");
        let ino = cache.get_or_alloc_ino("/share/doc.pdf");
        cache.insert(ino, info);
        assert_eq!(
            cache.get_path_for_ino(ino).as_deref(),
            Some("/share/doc.pdf")
        );
    }

    #[test]
    fn get_ino_for_path_is_case_insensitive() {
        let cache = InodeCache::new(30);
        let ino = cache.get_or_alloc_ino("/Share/File.TXT");
        assert_eq!(cache.get_ino_for_path("/share/file.txt"), Some(ino));
    }

    #[test]
    fn invalidate_path_removes_entry() {
        let cache = InodeCache::new(30);
        let info = make_file_info("/share/temp.txt");
        let ino = cache.get_or_alloc_ino("/share/temp.txt");
        cache.insert(ino, info);
        cache.invalidate_path("/share/temp.txt");
        assert!(cache.get_by_ino(ino).is_none());
        assert!(cache.get_ino_for_path("/share/temp.txt").is_none());
    }

    #[test]
    fn invalidate_prefix_removes_matching_entries() {
        let cache = InodeCache::new(30);
        for name in &["a.txt", "b.txt", "c.txt"] {
            let path = format!("/share/{}", name);
            let ino = cache.get_or_alloc_ino(&path);
            cache.insert(ino, make_file_info(&path));
        }
        let other_ino = cache.get_or_alloc_ino("/other/file.txt");
        cache.insert(other_ino, make_file_info("/other/file.txt"));

        cache.invalidate_prefix("/share/");

        assert!(cache.get_ino_for_path("/share/a.txt").is_none());
        assert!(cache.get_ino_for_path("/share/b.txt").is_none());
        assert!(cache.get_ino_for_path("/share/c.txt").is_none());
        // Entry outside the prefix is untouched.
        assert!(cache.get_ino_for_path("/other/file.txt").is_some());
    }

    #[test]
    fn get_size_for_ino_returns_size() {
        use synology_filestation_core::types::SynoAdditional;
        let cache = InodeCache::new(30);
        let mut info = make_file_info("/share/video.mp4");
        info.additional = Some(SynoAdditional {
            size: Some(1234567),
            owner: None,
            time: None,
            perm: None,
        });
        let ino = cache.get_or_alloc_ino("/share/video.mp4");
        cache.insert(ino, info);
        assert_eq!(cache.get_size_for_ino(ino), Some(1234567));
    }

    #[test]
    fn get_size_returns_none_when_not_cached() {
        let cache = InodeCache::new(30);
        assert!(cache.get_size_for_ino(999).is_none());
    }

    // -----------------------------------------------------------------------
    // ReadCache tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_cache_insert_and_get() {
        let rc = ReadCache::new(1024, 16);
        let data = Bytes::from_static(b"hello world");
        rc.insert(1, 0, data.clone());
        assert_eq!(rc.get(1, 0).unwrap(), data);
    }

    #[test]
    fn read_cache_contains() {
        let rc = ReadCache::new(1024, 16);
        assert!(!rc.contains(1, 0));
        rc.insert(1, 0, Bytes::from_static(b"x"));
        assert!(rc.contains(1, 0));
    }

    #[test]
    fn claim_inflight_grants_exclusive_access() {
        let rc = ReadCache::new(1024, 16);
        assert!(rc.claim_inflight(1, 0)); // first caller wins
        assert!(!rc.claim_inflight(1, 0)); // second caller loses
    }

    #[test]
    fn cancel_inflight_releases_slot() {
        let rc = ReadCache::new(1024, 16);
        assert!(rc.claim_inflight(1, 0));
        rc.cancel_inflight(1, 0);
        assert!(rc.claim_inflight(1, 0)); // slot is free again
    }

    #[test]
    fn wait_for_block_returns_data_after_insert() {
        let rc = Arc::new(ReadCache::new(1024, 16));
        assert!(rc.claim_inflight(1, 0));

        let rc2 = rc.clone();
        let handle = std::thread::spawn(move || rc2.wait_for_block(1, 0));

        std::thread::sleep(Duration::from_millis(20));
        rc.insert(1, 0, Bytes::from_static(b"payload"));

        let result = handle.join().unwrap();
        assert_eq!(result.unwrap(), Bytes::from_static(b"payload"));
    }

    #[test]
    fn wait_for_block_returns_none_after_cancel() {
        let rc = Arc::new(ReadCache::new(1024, 16));
        assert!(rc.claim_inflight(1, 0));

        let rc2 = rc.clone();
        let handle = std::thread::spawn(move || rc2.wait_for_block(1, 0));

        std::thread::sleep(Duration::from_millis(20));
        rc.cancel_inflight(1, 0);

        let result = handle.join().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn wait_for_block_returns_empty_bytes_as_eof_sentinel() {
        let rc = Arc::new(ReadCache::new(1024, 16));
        assert!(rc.claim_inflight(1, 5));

        let rc2 = rc.clone();
        let handle = std::thread::spawn(move || rc2.wait_for_block(1, 5));

        std::thread::sleep(Duration::from_millis(20));
        rc.insert(1, 5, Bytes::new()); // EOF sentinel

        let result = handle.join().unwrap();
        assert_eq!(result, Some(Bytes::new()));
    }

    /// Regression: the claim owner can die without ever publishing the block or
    /// releasing the claim (a panicking prefetch task, a runtime torn down
    /// mid-download). Before this bound, every waiter looped on a 5 ms sleep
    /// forever — and the waiters are FUSE worker threads, so the mount stopped
    /// answering entirely. The wait must give up and report failure.
    #[test]
    fn wait_for_block_gives_up_on_an_abandoned_claim() {
        let rc = ReadCache::new(1024, 16);
        assert!(rc.claim_inflight(1, 0));
        // Nobody will ever insert() or cancel_inflight() — the owner is gone.
        let start = Instant::now();
        let result = rc.wait_for_block_until(1, 0, Duration::from_millis(50));
        assert!(result.is_none(), "an abandoned claim must not yield data");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "wait must be bounded, took {:?}",
            start.elapsed()
        );
    }

    /// Regression: giving up used to *free* the claim, on the theory that the
    /// owner must be dead. A timer cannot tell a dead owner from a slow one,
    /// and under load every owner is slow — so the mount freed claims whose
    /// downloads were merely queued, a second reader started the same download
    /// again, and the duplicated work made the queue longer still. Eight video
    /// thumbnails were enough to turn that into a mount that served nothing.
    ///
    /// The claim now stays. Every path that takes one releases it on the way
    /// out — including a panic or an abort, via `InflightGuard` — so an
    /// unresolved claim means a download still running, and the right thing to
    /// do about a download still running is nothing.
    #[test]
    fn giving_up_does_not_free_a_claim_whose_owner_is_still_working() {
        let rc = ReadCache::new(1024, 16);
        assert!(rc.claim_inflight(2, 7));

        assert!(rc
            .wait_for_block_until(2, 7, Duration::from_millis(50))
            .is_none());

        assert!(
            !rc.claim_inflight(2, 7),
            "the owner still holds this block; a second download must not start"
        );
    }

    /// The owner releasing its claim must wake the waiters, not leave them to
    /// notice on the next poll — there are no polls any more.
    #[test]
    fn a_failed_download_releases_its_waiters_at_once() {
        let rc = Arc::new(ReadCache::new(1024, 16));
        assert!(rc.claim_inflight(4, 1));

        let rc2 = rc.clone();
        let waiter = std::thread::spawn(move || {
            let start = Instant::now();
            let got = rc2.wait_for_block_until(4, 1, Duration::from_secs(30));
            (got, start.elapsed())
        });

        std::thread::sleep(Duration::from_millis(30));
        rc.cancel_inflight(4, 1);

        let (got, waited) = waiter.join().unwrap();
        assert!(got.is_none(), "a failed download yields no data");
        assert!(
            waited < Duration::from_secs(5),
            "the waiter must wake on the failure, not sit out the deadline: {waited:?}"
        );
    }

    /// Dropping an inode's blocks must release anyone waiting on one, or a
    /// file being written underneath a reader strands that reader.
    #[test]
    fn invalidating_an_inode_releases_its_waiters() {
        let rc = Arc::new(ReadCache::new(1024, 16));
        assert!(rc.claim_inflight(5, 2));

        let rc2 = rc.clone();
        let waiter = std::thread::spawn(move || {
            let start = Instant::now();
            let got = rc2.wait_for_block_until(5, 2, Duration::from_secs(30));
            (got, start.elapsed())
        });

        std::thread::sleep(Duration::from_millis(30));
        rc.invalidate_ino(5);

        let (got, waited) = waiter.join().unwrap();
        assert!(got.is_none());
        assert!(
            waited < Duration::from_secs(5),
            "invalidation must wake the waiter: {waited:?}"
        );
    }

    /// A reader arriving after the download already finished has no claim to
    /// wait on, and must simply be handed the block.
    #[test]
    fn a_waiter_that_arrives_after_the_block_landed_still_gets_it() {
        let rc = ReadCache::new(1024, 16);
        assert!(rc.claim_inflight(6, 0));
        rc.insert(6, 0, Bytes::from_static(b"already here"));

        assert_eq!(
            rc.wait_for_block_until(6, 0, Duration::from_millis(50)),
            Some(Bytes::from_static(b"already here"))
        );
    }

    /// The bound must not cut short a download that is merely slow: a block
    /// published before the deadline is still returned.
    #[test]
    fn wait_for_block_still_returns_a_slow_but_successful_download() {
        let rc = Arc::new(ReadCache::new(1024, 16));
        assert!(rc.claim_inflight(3, 0));

        let rc2 = rc.clone();
        let handle =
            std::thread::spawn(move || rc2.wait_for_block_until(3, 0, Duration::from_secs(30)));

        std::thread::sleep(Duration::from_millis(30));
        rc.insert(3, 0, Bytes::from_static(b"slow payload"));

        assert_eq!(
            handle.join().unwrap(),
            Some(Bytes::from_static(b"slow payload"))
        );
    }

    #[test]
    fn invalidate_ino_removes_all_blocks() {
        let rc = ReadCache::new(1024, 16);
        rc.insert(1, 0, Bytes::from_static(b"block0"));
        rc.insert(1, 1, Bytes::from_static(b"block1"));
        rc.insert(2, 0, Bytes::from_static(b"other_ino"));

        rc.invalidate_ino(1);

        assert!(!rc.contains(1, 0));
        assert!(!rc.contains(1, 1));
        assert!(rc.contains(2, 0)); // other ino untouched
    }

    #[test]
    fn invalidate_ino_clears_inflight_markers() {
        let rc = ReadCache::new(1024, 16);
        // Insert a block so ino_blocks tracks it, then re-claim it as in-flight
        // (simulates a concurrent re-download starting just before invalidation).
        rc.insert(1, 0, Bytes::from_static(b"data"));
        rc.claim_inflight(1, 0);
        rc.invalidate_ino(1);
        // The in-flight marker should have been cleared along with the cached block.
        assert!(rc.claim_inflight(1, 0));
    }

    // ── DirCache ──────────────────────────────────────────────────────────────

    /// Regression: `readdir` called the NAS unconditionally, and nothing cached
    /// the answer. A desktop file manager polling the mount — which is what
    /// GIO does the moment a volume appears — turned into a sustained ~10
    /// `list_shares` per second against `synoscgi`, the shared CGI backend the
    /// whole appliance runs on. One directory read cost three round trips: the
    /// listing, the kernel's follow-up at the end offset, and its repeat.
    #[test]
    fn a_listing_is_served_from_the_cache_the_second_time() {
        let cache = DirCache::new(30);

        assert!(cache.get("/photos").is_none(), "nothing cached yet");
        cache.insert("/photos", vec![make_file_info("/photos/a.jpg")]);

        let hit = cache.get("/photos").expect("the listing was just cached");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].path, "/photos/a.jpg");
    }

    /// The same case-insensitivity the inode map uses: DSM treats paths
    /// case-insensitively, so two spellings must not become two listings that
    /// can disagree with each other.
    #[test]
    fn a_listing_is_found_whatever_case_it_is_asked_for() {
        let cache = DirCache::new(30);
        cache.insert("/Photos", vec![make_file_info("/Photos/a.jpg")]);

        assert!(cache.get("/photos").is_some());
        assert!(cache.get("/PHOTOS").is_some());
    }

    /// Our own mutations cannot wait for the TTL: a file created and then
    /// listed has to be there, or the mount contradicts itself.
    #[test]
    fn a_changed_directory_is_listed_again_rather_than_from_the_cache() {
        let cache = DirCache::new(30);
        cache.insert("/photos", vec![make_file_info("/photos/a.jpg")]);

        cache.invalidate("/photos");

        assert!(cache.get("/photos").is_none());
    }

    /// A directory that was removed or renamed takes its descendants with it,
    /// exactly as `InodeCache::invalidate_prefix` already does for inodes.
    #[test]
    fn removing_a_directory_forgets_the_listings_underneath_it() {
        let cache = DirCache::new(30);
        cache.insert("/photos", vec![make_file_info("/photos/a.jpg")]);
        cache.insert("/photos/2026", vec![make_file_info("/photos/2026/b.jpg")]);
        cache.insert("/photostudio", vec![make_file_info("/photostudio/c.jpg")]);

        cache.invalidate_prefix("/photos");

        assert!(cache.get("/photos").is_none());
        assert!(cache.get("/photos/2026").is_none());
        assert!(
            cache.get("/photostudio").is_some(),
            "a sibling whose name merely starts the same way is not underneath it"
        );
    }

    /// `--cache-ttl 0` is how somebody asks for no caching at all. Serving a
    /// stale listing to them would be ignoring the flag.
    #[test]
    fn a_zero_ttl_caches_nothing() {
        let cache = DirCache::new(0);
        cache.insert("/photos", vec![make_file_info("/photos/a.jpg")]);

        assert!(cache.get("/photos").is_none());
    }

    /// ...but the caller still needs the listing it just fetched. Caching off
    /// means "do not remember this", not "lose it".
    #[test]
    fn a_disabled_cache_still_hands_back_what_was_put_in_it() {
        let cache = DirCache::new(0);

        let stored = cache.insert("/photos", vec![make_file_info("/photos/a.jpg")]);

        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].path, "/photos/a.jpg");
    }
}
