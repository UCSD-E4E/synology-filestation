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

/// A directory's entries, `.` and `..` included, in the order offsets count.
pub(super) type Snapshot = Arc<Vec<DirEntry>>;

/// Open directory handles, each with the snapshot its pass is reading, once
/// it has one.
pub(super) type DirHandles = Arc<Mutex<HashMap<u64, Option<Snapshot>>>>;

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
        // cache's job, and the cache knows when to stop.
        if result.is_ok() {
            let mut flights = self.flights.lock().unwrap();
            if flights.get(&key).is_some_and(|f| Arc::ptr_eq(f, &flight)) {
                flights.remove(&key);
            }
        }
        result
    }
}

/// `path`'s entries in `readdir` order, registering an inode for each.
fn snapshot(cache: &InodeCache, ino: u64, path: &str, listing: &[SynoFileInfo]) -> Snapshot {
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

    let mut entries = Vec::with_capacity(listing.len() + 2);
    entries.push(DirEntry {
        ino,
        kind: FileType::Directory,
        name: ".".to_string(),
    });
    entries.push(DirEntry {
        ino: parent_ino,
        kind: FileType::Directory,
        name: "..".to_string(),
    });
    for info in listing {
        let child_ino = cache.get_or_alloc_ino(&info.path);
        let kind = if info.isdir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        cache.insert(child_ino, info.clone());
        entries.push(DirEntry {
            ino: child_ino,
            kind,
            name: info.name.clone(),
        });
    }
    Arc::new(entries)
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
        self.dir_handles.lock().unwrap().remove(&fh);
    }

    /// The entries `readdir` at `offset` on `fh` answers from, handed to
    /// `done`.
    ///
    /// Served at once from the handle's snapshot when the pass already has
    /// one. Otherwise the listing is fetched on the runtime and `done` runs
    /// there, so the calling event-loop thread is free the moment this
    /// returns.
    pub(super) fn start_readdir<F>(&self, fh: u64, ino: u64, path: String, offset: u64, done: F)
    where
        F: FnOnce(Result<Snapshot, SynoFsError>) + Send + 'static,
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
                .map(|listing| snapshot(&cache, ino, &path, &listing));
            if let Ok(entries) = &result {
                // Only a handle that is still open keeps it; one released
                // while this was fetching is gone, and must stay gone.
                if let Some(slot) = handles.lock().unwrap().get_mut(&fh) {
                    *slot = Some(Arc::clone(entries));
                }
            }
            done(result);
        });
    }
}
