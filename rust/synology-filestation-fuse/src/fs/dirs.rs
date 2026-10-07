//! Reading a directory.
//!
//! The kernel reads a directory a few hundred entries per `readdir`, and every
//! one of those calls used to fetch the listing again once the cached copy had
//! expired. For a directory of a million entries, which takes minutes to list
//! through the tunnel, that made `find` quadratic: one chunk, a full
//! re-listing, the next chunk, another — days to read one directory. And each
//! fetch ran on a FUSE event-loop thread, so a handful of walkers froze the
//! mount. Three things here stop that:
//!
//! - **One listing per pass.** `opendir` hands out a real handle, the first
//!   `readdir` on it keeps the listing it fetched, and the rest of the pass is
//!   served from that snapshot whatever the cache has expired since. A read
//!   from offset 0 is a new pass (`rewinddir`) and fetches again.
//! - **One fetch per directory at a time.** Readers that arrive while a
//!   directory is being listed wait for that listing rather than starting
//!   their own.
//! - **Off the event loop.** The fetch runs on the Tokio runtime and the reply
//!   is sent from there, the same shape the transfers use.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use fuser::FileType;
use synology_filestation_core::client::SynologyClient;
use synology_filestation_core::error::SynoFsError;
use synology_filestation_core::types::{SynoFileInfo, VIRTUAL_ROOT_PATH};
use tokio::sync::OnceCell;

use super::{SynologyFS, ROOT_INO};
use crate::cache::{DirCache, InodeCache};

/// One entry as `readdir` hands it to the kernel.
#[derive(Debug, Clone)]
pub(super) struct DirEntry {
    pub ino: u64,
    pub kind: FileType,
    pub name: String,
}

/// What one pass over a directory reads: the listing, shared, and nothing
/// else per handle.
///
/// Every reader of a listing holds the same `Arc` of it. Entries are turned
/// into [`DirEntry`]s, and their inodes registered, only as `readdir` hands
/// them out, a chunk at a time. Converting the whole listing up front cloned
/// every name into a vector per handle (memory that grew with the readers of
/// a million-entry directory) and did it in one pass that held a runtime
/// worker for as long as it took.
pub(super) struct Snapshot {
    listing: Arc<Vec<SynoFileInfo>>,
    ino: u64,
    parent_ino: u64,
    cache: Arc<InodeCache>,
}

impl Snapshot {
    fn new(cache: Arc<InodeCache>, ino: u64, path: &str, listing: Arc<Vec<SynoFileInfo>>) -> Self {
        let parent_ino = if path == VIRTUAL_ROOT_PATH {
            ROOT_INO
        } else {
            path.rfind('/')
                .map(|i| match &path[..i] {
                    // The parent of a top-level share is the virtual root.
                    "" => ROOT_INO,
                    parent => cache.get_or_alloc_ino(parent),
                })
                .unwrap_or(ROOT_INO)
        };
        Self {
            listing,
            ino,
            parent_ino,
            cache,
        }
    }

    /// Entries in the pass, `.` and `..` included.
    pub fn len(&self) -> usize {
        self.listing.len() + 2
    }

    /// The entry at `i` in the order offsets count, registering its inode as
    /// it is handed out.
    pub fn entry(&self, i: usize) -> Option<DirEntry> {
        match i {
            0 => Some(DirEntry {
                ino: self.ino,
                kind: FileType::Directory,
                name: ".".to_string(),
            }),
            1 => Some(DirEntry {
                ino: self.parent_ino,
                kind: FileType::Directory,
                name: "..".to_string(),
            }),
            _ => {
                let info = self.listing.get(i - 2)?;
                let ino = self.cache.get_or_alloc_ino(&info.path);
                self.cache.insert(ino, info.clone());
                Some(DirEntry {
                    ino,
                    kind: if info.isdir {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    name: info.name.clone(),
                })
            }
        }
    }

    /// Whether `other` reads the very same listing, not a copy of it.
    #[cfg(test)]
    pub fn shares_listing_with(&self, other: &Snapshot) -> bool {
        Arc::ptr_eq(&self.listing, &other.listing)
    }
}

/// Open directory handles, each with the snapshot its pass is reading, once
/// it has one.
pub(super) type DirHandles = Arc<Mutex<HashMap<u64, Option<Arc<Snapshot>>>>>;

/// A listing being fetched, which later readers of the same directory wait on.
type Flight = Arc<OnceCell<Arc<Vec<SynoFileInfo>>>>;

/// Listings in flight, by lowercased path (DSM is case-insensitive).
pub(super) type Flights = Arc<Mutex<HashMap<String, Flight>>>;

/// Everything fetching a listing needs, owned, so it can run on the runtime.
#[derive(Clone)]
pub(super) struct Lister {
    pub client: Arc<SynologyClient>,
    pub dir_cache: Arc<DirCache>,
    pub flights: Flights,
}

impl Lister {
    /// A directory's entries, from the cache when they are there, and from a
    /// fetch already under way when there is one.
    ///
    /// A fetch that fails leaves nothing behind, so the next reader waiting on
    /// it makes its own attempt and gets its own error, rather than all of
    /// them sharing one.
    pub async fn get(&self, path: &str) -> Result<Arc<Vec<SynoFileInfo>>, SynoFsError> {
        if let Some(cached) = self.dir_cache.get(path) {
            return Ok(cached);
        }
        let key = path.to_lowercase();
        let flight = Arc::clone(self.flights.lock().unwrap().entry(key.clone()).or_default());
        let result = flight
            .get_or_try_init(|| async {
                let entries = if path == VIRTUAL_ROOT_PATH {
                    self.client.list_shares().await?
                } else {
                    self.client.list_dir(path).await?
                };
                // `insert` hands back what it stored, so this still works when
                // caching is off and nothing was stored at all.
                Ok(self.dir_cache.insert(path, entries))
            })
            .await
            .cloned();
        // A finished flight must not answer the next reader: that is the
        // cache's job, and the cache knows when to stop. A failed one stays
        // while other readers still wait on it, so they retry on it one at a
        // time, and goes with the last of them; otherwise a directory that
        // stayed unreadable would hold its place for the life of the mount.
        // Our hold is let go under the lock, so two readers failing together
        // cannot each see the other's and each leave the removal to the other.
        let mut flights = self.flights.lock().unwrap();
        let ours = flights.get(&key).is_some_and(|f| Arc::ptr_eq(f, &flight));
        drop(flight);
        if ours && (result.is_ok() || flights.get(&key).is_some_and(|f| Arc::strong_count(f) == 1))
        {
            flights.remove(&key);
        }
        drop(flights);
        result
    }
}

impl SynologyFS {
    pub(super) fn lister(&self) -> Lister {
        Lister {
            client: self.client.clone(),
            dir_cache: self.dir_cache.clone(),
            flights: self.flights.clone(),
        }
    }

    /// A handle for one pass over a directory.
    pub(super) fn open_dir(&self) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.dir_handles.lock().unwrap().insert(fh, None);
        fh
    }

    /// Let go of a handle's snapshot.
    pub(super) fn release_dir(&self, fh: u64) {
        // Bound first, so that if this was the last hold on a listing of a
        // million entries, it is freed after the lock is let go, not while
        // every other open, read and close waits on it.
        let released = self.dir_handles.lock().unwrap().remove(&fh);
        drop(released);
    }

    /// The snapshot `readdir` at `offset` on `fh` answers from, handed to
    /// `done`.
    ///
    /// Served at once from the handle's snapshot when the pass already has
    /// one. Otherwise the listing is fetched on the runtime and `done` runs
    /// there, so the calling event-loop thread is free the moment this
    /// returns.
    pub(super) fn start_readdir<F>(&self, fh: u64, ino: u64, path: String, offset: u64, done: F)
    where
        F: FnOnce(Result<Arc<Snapshot>, SynoFsError>) + Send + 'static,
    {
        if offset > 0 {
            let kept = self.dir_handles.lock().unwrap().get(&fh).cloned().flatten();
            if let Some(kept) = kept {
                done(Ok(kept));
                return;
            }
        }

        let lister = self.lister();
        let cache = self.cache.clone();
        let handles = self.dir_handles.clone();
        self.rt.spawn(async move {
            let result = lister
                .get(&path)
                .await
                .map(|listing| Arc::new(Snapshot::new(cache, ino, &path, listing)));
            if let Ok(snapshot) = &result {
                // Only a handle that is still open keeps it; one released
                // while this was fetching is gone, and must stay gone. The
                // snapshot a rewind replaces is freed after the lock, for the
                // same reason as in `release_dir`.
                let replaced = handles
                    .lock()
                    .unwrap()
                    .get_mut(&fh)
                    .and_then(|slot| slot.replace(Arc::clone(snapshot)));
                drop(replaced);
            }
            done(result);
        });
    }
}
