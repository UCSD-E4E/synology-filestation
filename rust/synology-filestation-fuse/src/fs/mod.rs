use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use tracing::{debug, error, warn};

use crate::cache::{DirCache, InodeCache, ReadCache};
use crate::spill::SpillBuffer;
use synology_filestation_core::client::SynologyClient;
use synology_filestation_core::error::SynoFsError;
use synology_filestation_core::transport::WriteOpen;
use synology_filestation_core::types::{SynoFileInfo, VIRTUAL_ROOT_PATH};

mod attr;
mod callbacks;
mod prefetch;
#[cfg(test)]
mod tests;
mod transfer;

use attr::file_attr;
pub use attr::Ownership;
use prefetch::{
    is_indexed_media, open_window, InflightGuard, ReadAhead, MAX_INFLIGHT_PREFETCH_BLOCKS,
    MAX_PREFETCH_SPAN,
};
use transfer::{Buffers, Transfers, WriteBuffer, WriteSink, MAX_CONCURRENT_TRANSFERS};

const TTL: Duration = Duration::from_secs(1);

const ROOT_INO: u64 = 1;

pub struct SynologyFS {
    client: Arc<SynologyClient>,
    cache: Arc<InodeCache>,
    /// Directory listings, so a client that reads a directory repeatedly does
    /// not become repeated load on the appliance. See [`DirCache`].
    dir_cache: Arc<DirCache>,
    read_cache: Arc<ReadCache>,
    rt: tokio::runtime::Handle,
    write_buffers: Buffers,
    /// Bounds how many transfers are on the wire at once, now that they are no
    /// longer implicitly serialised by the event loop.
    transfer_limit: Arc<tokio::sync::Semaphore>,
    next_fh: AtomicU64,
    owner: Ownership,
    /// Bounds speculative block downloads. One permit is one block on the
    /// wire. See [`MAX_INFLIGHT_PREFETCH_BLOCKS`].
    prefetch_limit: Arc<tokio::sync::Semaphore>,
    /// Depth of the speculative window, in blocks. `0` switches it off.
    prefetch_blocks: u64,
    /// Per-handle sequential-read state, paired with the inode the handle is
    /// on so that closing one handle only cancels prefetch when it is the last
    /// handle on that file.
    read_ahead: Arc<Mutex<HashMap<u64, (u64, ReadAhead)>>>,
    /// Speculative downloads still running, so a close can abandon them.
    prefetch_tasks: Arc<Mutex<HashMap<u64, Vec<tokio::task::JoinHandle<()>>>>>,
}

impl SynologyFS {
    /// A directory's entries, from the cache when they are there.
    ///
    /// The single place a listing is obtained, so the caching applies to every
    /// caller rather than to whichever one remembered. `readdir` and `lookup`
    /// both go through it, and both used to hit the network unconditionally —
    /// which, for a directory the kernel reads three times per `ls`, is three
    /// round trips to say one thing.
    ///
    /// The virtual root is a listing like any other; that it comes from
    /// `list_share` rather than `list` is the only difference, and it is the
    /// one that gets polled.
    fn listing(&self, path: &str) -> Result<Arc<Vec<SynoFileInfo>>, SynoFsError> {
        if let Some(cached) = self.dir_cache.get(path) {
            return Ok(cached);
        }
        let entries = if path == VIRTUAL_ROOT_PATH {
            self.block(self.client.list_shares())?
        } else {
            self.block(self.client.list_dir(path))?
        };
        // `insert` hands back what it stored, so this still works when caching
        // is switched off (`--cache-ttl 0`) and nothing was stored at all.
        Ok(self.dir_cache.insert(path, entries))
    }

    pub fn new(
        client: Arc<SynologyClient>,
        cache: Arc<InodeCache>,
        dir_cache: Arc<DirCache>,
        read_cache: Arc<ReadCache>,
        rt: tokio::runtime::Handle,
        owner: Ownership,
        prefetch_blocks: u64,
    ) -> Self {
        Self {
            client,
            cache,
            dir_cache,
            read_cache,
            rt,
            write_buffers: Arc::new(Mutex::new(HashMap::new())),
            transfer_limit: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRANSFERS)),
            next_fh: AtomicU64::new(1),
            owner,
            prefetch_limit: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_PREFETCH_BLOCKS)),
            prefetch_blocks,
            read_ahead: Arc::new(Mutex::new(HashMap::new())),
            prefetch_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A transfer bundle owning clones of everything a background task needs.
    fn transfers(&self) -> Transfers {
        Transfers {
            client: self.client.clone(),
            cache: self.cache.clone(),
            dir_cache: self.dir_cache.clone(),
            read_cache: self.read_cache.clone(),
            buffers: self.write_buffers.clone(),
            limit: self.transfer_limit.clone(),
        }
    }

    fn block<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.rt.block_on(fut)
    }

    fn syno_to_attr(&self, ino: u64, info: &SynoFileInfo) -> FileAttr {
        file_attr(self.owner, ino, info)
    }

    fn get_path_for_ino(&self, ino: u64) -> Option<String> {
        self.cache.get_path_for_ino(ino)
    }

    /// Start `fh`'s upload on the Tokio runtime and hand the outcome to `done`.
    ///
    /// This is the shape that keeps a transfer off the FUSE event loop.
    /// `close(2)` still returns the upload's real outcome — `done` sends the
    /// kernel reply — but it is produced on a runtime thread, so the event-loop
    /// thread returns to serving the mount the moment the work is queued.
    /// Uploading inline was what made a large copy look like a hung filesystem.
    fn start_upload<F>(&self, fh: u64, done: F)
    where
        F: FnOnce(Result<(), SynoFsError>) + Send + 'static,
    {
        let xfer = self.transfers();
        self.rt.spawn(async move { done(xfer.upload(fh).await) });
    }

    /// Same, for the read-modify-write behind `truncate`.
    fn start_truncate<F>(&self, ino: u64, path: String, new_size: u64, done: F)
    where
        F: FnOnce(Result<SynoFileInfo, SynoFsError>) + Send + 'static,
    {
        let xfer = self.transfers();
        self.rt
            .spawn(async move { done(xfer.truncate(ino, &path, new_size).await) });
    }

    /// Same, for the download→upload→delete behind a cross-directory rename.
    fn start_move_across_dirs<F>(
        &self,
        old_path: String,
        new_parent: String,
        new_name: String,
        done: F,
    ) where
        F: FnOnce(Result<(), SynoFsError>) + Send + 'static,
    {
        let xfer = self.transfers();
        self.rt.spawn(async move {
            done(
                xfer.move_across_dirs(&old_path, &new_parent, &new_name)
                    .await,
            )
        });
    }

    /// Upload `fh`'s buffered data, blocking until it lands.
    ///
    /// Only `destroy` uses this: at unmount there is no event loop left to keep
    /// responsive, and the runtime is about to go away, so the wait is the
    /// point. Everything else goes through [`SynologyFS::start_upload`].
    fn finish_upload(&self, fh: u64) -> Result<(), SynoFsError> {
        let xfer = self.transfers();
        self.block(xfer.upload(fh))
    }

    /// Buffer `data` at `offset` for the handle `fh`.
    ///
    /// Split out of the `write` callback so the locking contract — a write to a
    /// handle whose upload is in flight waits for it — is testable without a
    /// live mount. Waiting is correct: the upload may be streaming the very
    /// spill file this would overwrite. It is also rare, since an application
    /// has stopped writing by the time `close(2)` triggers the flush.
    fn write_buffer_at(&self, fh: u64, offset: u64, data: &[u8]) -> Result<(), SynoFsError> {
        let handle = self
            .buffer(fh)
            .ok_or_else(|| SynoFsError::Io(format!("write to unknown file handle {fh}")))?;
        // A FUSE callback thread is not a runtime thread, so blocking here is
        // allowed — and on the streamed path that block IS the back-pressure:
        // `write(2)` returns when the server has the bytes, so a copy runs at
        // the speed of the link rather than the speed of the local disk.
        self.rt.block_on(async move {
            let mut buf = handle.lock().await;
            // Set when a streamed write failed with nothing of this file on the
            // server yet, so the handle can still take the buffered path.
            let mut fall_back = false;
            match &mut buf.sink {
                WriteSink::Streamed(slot) => {
                    let stream = slot
                        .as_mut()
                        .ok_or_else(|| SynoFsError::Io("write to a closed handle".into()))?;
                    match stream.write_at(offset, data).await {
                        Ok(()) => buf.streamed = true,
                        Err(e) => {
                            // The stream is finished either way: whatever is on
                            // the server is short of what the caller asked for.
                            *slot = None;
                            // But if nothing of this file ever left the machine,
                            // there is nothing to be consistent with. The
                            // buffered path can still carry the whole file — it
                            // is what this mount did for every write before
                            // streaming existed — so use it rather than failing
                            // a copy the HTTP API would have completed. Only for
                            // a file this handle is writing whole: over an
                            // existing one, the buffer holds what was written
                            // after the switch and uploading it would publish a
                            // file with a hole where the rest used to be.
                            if buf.streamed || !buf.new_file {
                                buf.broken = true;
                                return Err(e);
                            }
                            warn!(
                                "streamed write to {} failed ({e}); \
                                 buffering this handle instead",
                                buf.nas_path
                            );
                            fall_back = true;
                        }
                    }
                }
                WriteSink::Buffered(spill) => {
                    if let Err(e) = spill.write_at(offset, data) {
                        // The buffer no longer holds what the caller wrote, and
                        // the upload path cannot tell a short buffer from a
                        // short file — it would publish the truncated version
                        // over the destination. Abandon the handle the same way
                        // a failed streamed write does, so `close` reports the
                        // failure instead of a file that never landed.
                        buf.broken = true;
                        return Err(SynoFsError::Io(e.to_string()));
                    }
                }
            }
            if fall_back {
                buf.sink = WriteSink::Buffered(SpillBuffer::new());
                let WriteSink::Buffered(spill) = &mut buf.sink else {
                    unreachable!("just assigned")
                };
                if let Err(e) = spill.write_at(offset, data) {
                    buf.broken = true;
                    return Err(SynoFsError::Io(e.to_string()));
                }
            }
            buf.dirty = true;
            Ok(())
        })
    }

    /// Open `path` for writing, streaming when a backend can take ranges.
    ///
    /// `WriteOpen::Existing` rather than `CreateNew`: `create(2)` without
    /// `O_EXCL` may legitimately land on a file that already exists, and by
    /// the time this runs the kernel has already decided that is allowed.
    fn open_sink(&self, path: &str) -> WriteSink {
        match self.block(self.client.open_write(path, WriteOpen::Existing)) {
            Ok(Some(stream)) => WriteSink::Streamed(Some(stream)),
            Ok(None) => WriteSink::Buffered(SpillBuffer::new()),
            // A backend that refused is not a reason to fail the open: the
            // buffered path may still succeed, and it is what this mount did
            // for every write before streaming existed.
            Err(e) => {
                warn!("open_write {path}: {e}; buffering this handle instead");
                WriteSink::Buffered(SpillBuffer::new())
            }
        }
    }

    /// Give `fh` a write buffer for `nas_path`, as `open` and `create` do.
    ///
    /// A `new_file` is dirty from birth. `create(2)` is a request for a file
    /// to exist, and `touch` makes exactly this handle: opened, written to
    /// never, closed. Starting clean meant close saw nothing to do and the
    /// file never reached the NAS at all — it lived in the inode cache until
    /// the TTL expired and then vanished.
    ///
    /// The sink is opened *before* the map is locked, never inside the
    /// `insert` call: a method's receiver is evaluated before its arguments,
    /// so `lock().insert(.., open_sink(..))` held the lock across a network
    /// round trip. Every close's upload task takes this lock on a runtime
    /// worker, so both workers blocked on it while the round trip waited for a
    /// worker — and the mount stopped, tunnel and all.
    ///
    /// Only a `writable` handle gets a sink from the NAS. The kernel refuses a
    /// write on a read-only descriptor before it reaches us, so asking the NAS
    /// for a write handle on every `O_RDONLY` open was a round trip — 110 ms
    /// through the tunnel — that could never be used.
    pub(super) fn add_write_buffer(
        &self,
        fh: u64,
        nas_path: String,
        ino: u64,
        new_file: bool,
        writable: bool,
    ) {
        let sink = if writable {
            self.open_sink(&nas_path)
        } else {
            WriteSink::Buffered(SpillBuffer::new())
        };
        self.write_buffers.lock().unwrap().insert(
            fh,
            Arc::new(tokio::sync::Mutex::new(WriteBuffer {
                sink,
                nas_path,
                ino,
                streamed: false,
                dirty: new_file,
                new_file,
                broken: false,
            })),
        );
    }

    /// The buffer for `fh`, if the handle is still open. The map lock is only
    /// ever held long enough to clone the `Arc` — never across a transfer —
    /// so one handle's upload cannot stall lookups of another's.
    fn buffer(&self, fh: u64) -> Option<Arc<tokio::sync::Mutex<WriteBuffer>>> {
        self.write_buffers.lock().unwrap().get(&fh).cloned()
    }

    // ── Speculative prefetch ──────────────────────────────────────────────

    /// Fetch block 0, then decide from what block 0 turns out to be whether
    /// this file gets the media window.
    ///
    /// The sniff is free here and nowhere else: block 0 is downloaded
    /// synchronously anyway, so that the caller's first `read` is a guaranteed
    /// cache hit, and it is in hand at exactly the point the decision has to
    /// be made. Getting it wrong in the safe direction costs a few on-demand
    /// reads; getting it wrong the other way is what made every 5 MiB JPEG
    /// cost 5 MiB to look at.
    pub(super) fn prime_open(&self, fh: u64, ino: u64, path: &str) {
        self.read_ahead
            .lock()
            .unwrap()
            .insert(fh, (ino, ReadAhead::default()));

        let block_size = self.read_cache.block_size;
        let Some(total_blocks) = self.total_blocks(ino) else {
            return;
        };

        // If block 0 is cached as an empty EOF sentinel but the file is known
        // to be non-empty, evict the stale sentinel so we re-download real
        // data below. This can happen when a previous read attempt received an
        // empty body or HTTP 416 for a block that should have content.
        if let Some(b) = self.read_cache.get(ino, 0) {
            if b.is_empty() {
                debug!("open: evicting stale EOF sentinel for ino={} block=0", ino);
                self.read_cache.invalidate_block(ino, 0);
            }
        }

        // Block 0 synchronously. If another task already claimed it, skip — it
        // will be in cache when read() runs.
        if !self.read_cache.contains(ino, 0) && self.read_cache.claim_inflight(ino, 0) {
            match self.block(self.client.download(path, 0, block_size)) {
                Ok(data) if !data.is_empty() => self.read_cache.insert(ino, 0, data),
                _ => self.read_cache.cancel_inflight(ino, 0),
            }
        }

        // Without block 0 there is nothing to sniff, so claim nothing further:
        // the ramp in `read_ahead` still covers a reader that goes on reading.
        let Some(head) = self.read_cache.get(ino, 0) else {
            return;
        };
        let window = open_window(total_blocks, self.prefetch_blocks, is_indexed_media(&head));
        self.spawn_prefetch(ino, path, window);
    }

    /// Start whatever read-ahead this read has earned.
    ///
    /// Nothing, for a first read or a seek — the header-scan pattern any
    /// thumbnailer, indexer or `file(1)` uses must cost the block it asked
    /// for and no more. A reader that keeps going gets a window that doubles
    /// up to `--prefetch-blocks`.
    pub(super) fn read_ahead(&self, fh: u64, ino: u64, path: &str, offset: u64, size: u64) {
        let block_size = self.read_cache.block_size;
        let total_blocks = self.total_blocks(ino);
        let blocks = {
            let mut handles = self.read_ahead.lock().unwrap();
            let (_, state) = handles.entry(fh).or_insert((ino, ReadAhead::default()));
            state.advance(offset, size, block_size, total_blocks, self.prefetch_blocks)
        };
        self.spawn_prefetch(ino, path, blocks);
    }

    /// Forget a handle, and abandon the file's speculation once the last
    /// handle on it is gone.
    ///
    /// Closing used to leave the whole open window still downloading. In a
    /// file-at-a-time walk that meant every closed file went on stealing
    /// bandwidth from its successors — which is what turned a nine-file scan
    /// into a spread of 0.6 s to 13.4 s for the same work.
    pub(super) fn end_read(&self, fh: u64, ino: u64) {
        let last_handle = {
            let mut handles = self.read_ahead.lock().unwrap();
            handles.remove(&fh);
            !handles.values().any(|(other, _)| *other == ino)
        };
        if last_handle {
            if let Some(tasks) = self.prefetch_tasks.lock().unwrap().remove(&ino) {
                for task in tasks {
                    task.abort();
                }
            }
        }
    }

    /// The file's length in blocks, or `None` when the size is not cached.
    fn total_blocks(&self, ino: u64) -> Option<u64> {
        self.cache
            .get_size_for_ino(ino)
            .filter(|size| *size > 0)
            .map(|size| size.div_ceil(self.read_cache.block_size))
    }

    /// Queue speculative blocks, contiguous ones together.
    ///
    /// Blocks that follow each other are fetched as a single ranged read and
    /// split back into blocks on arrival. The window covers the same bytes it
    /// always did; what changes is that a fifteen-block head is one request
    /// rather than fifteen. On a transport where a read costs a round trip on
    /// an open handle, that ratio *is* the cost of the window.
    ///
    /// A block already cached or already claimed by somebody else ends the run
    /// rather than joining it, so a run only ever covers blocks this call owns.
    fn spawn_prefetch(&self, ino: u64, path: &str, blocks: Vec<u64>) {
        let mut run: Vec<u64> = Vec::new();
        for block_idx in blocks {
            let ours = !self.read_cache.contains(ino, block_idx)
                && self.read_cache.claim_inflight(ino, block_idx);
            let continues = run.last().is_some_and(|last| last + 1 == block_idx)
                && run.len() < MAX_PREFETCH_SPAN;
            if !ours || !continues {
                self.spawn_run(ino, path, std::mem::take(&mut run));
            }
            if ours {
                run.push(block_idx);
            }
        }
        self.spawn_run(ino, path, run);
    }

    /// Fetch one contiguous run of already-claimed blocks in a single request.
    ///
    /// The budget is taken here, before the task exists, and a run that cannot
    /// have it is **dropped rather than queued**. Speculation is optional by
    /// definition, and a queued run is worse than no run at all: the claims
    /// were taken in `spawn_prefetch` before this point, so a run parked on
    /// the budget holds a claim on every block in it, and a reader that wants
    /// one of those blocks sleeps in `wait_for_block` behind a download that
    /// has not started. Sixty seconds later it gives up, having been told the
    /// download is still running — the wait path cannot tell a slow owner from
    /// an idle one, so the owner must never be idle. Dropping the run leaves
    /// the blocks unclaimed and the reader simply fetches what it needs; the
    /// ramp in [`ReadAhead`] re-earns the window as the reader goes on.
    fn spawn_run(&self, ino: u64, path: &str, blocks: Vec<u64>) {
        let Some(&first) = blocks.first() else {
            return;
        };
        let block_size = self.read_cache.block_size;
        let span = blocks.len() as u64 * block_size;
        // Built before the budget is asked for, so that giving up on the run
        // releases every claim it took on the way in.
        let guards: Vec<InflightGuard> = blocks
            .iter()
            .map(|&idx| InflightGuard::new(self.read_cache.clone(), ino, idx))
            .collect();
        let permit = self
            .prefetch_limit
            .clone()
            .try_acquire_many_owned(blocks.len() as u32);
        let Ok(permit) = permit else {
            debug!(
                "prefetch: budget full, dropping {} block(s) from {} at block {}",
                blocks.len(),
                path,
                first
            );
            return; // the guards release every claim in the run
        };
        let client = self.client.clone();
        let read_cache = self.read_cache.clone();
        let path = path.to_string();

        let task = self.rt.spawn(async move {
            // Held for the life of the download and released by dropping it,
            // so an abort gives the budget back with the claims.
            let _permit = permit;
            let mut guards = guards;
            let Ok(data) = client.download(&path, first * block_size, span).await else {
                return; // the guards release every claim in the run
            };
            // Split the response back into the blocks it covers. A response
            // shorter than the span is EOF: the block it straddles keeps what
            // came back, and the blocks past it get the empty sentinel — the
            // same contract a block-at-a-time fetch had.
            for (n, guard) in guards.iter_mut().enumerate() {
                let from = n * block_size as usize;
                let piece = if from >= data.len() {
                    bytes::Bytes::new()
                } else {
                    data.slice(from..(from + block_size as usize).min(data.len()))
                };
                read_cache.insert(ino, first + n as u64, piece);
                guard.disarm();
            }
        });

        let mut tasks = self.prefetch_tasks.lock().unwrap();
        let for_ino = tasks.entry(ino).or_default();
        for_ino.retain(|task| !task.is_finished());
        for_ino.push(task);
    }

    /// Test seam: block until this inode's queued prefetch has settled.
    #[cfg(test)]
    pub(super) fn await_prefetch(&self, ino: u64) {
        let tasks = self
            .prefetch_tasks
            .lock()
            .unwrap()
            .remove(&ino)
            .unwrap_or_default();
        for task in tasks {
            let _ = self.rt.block_on(task);
        }
    }

    /// Test seam: how many of this inode's prefetch tasks are still running.
    #[cfg(test)]
    pub(super) fn outstanding_prefetch(&self, ino: u64) -> usize {
        self.prefetch_tasks
            .lock()
            .unwrap()
            .get(&ino)
            .map(|tasks| tasks.iter().filter(|task| !task.is_finished()).count())
            .unwrap_or(0)
    }

    /// Assemble a byte range for `read`, block by block, out of the read cache.
    ///
    /// Split out of the `read` callback so the assembly rules are testable
    /// without a live mount; `read` keeps the read-ahead and the reply.
    fn read_range(
        &self,
        ino: u64,
        path: &str,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, SynoFsError> {
        let block_size = self.read_cache.block_size;
        let first_block = offset / block_size;
        let last_block = (offset + size - 1) / block_size;

        let mut result: Vec<u8> = Vec::with_capacity(size as usize);

        for block_idx in first_block..=last_block {
            let block_start = block_idx * block_size;

            let block = if let Some(cached) = self.read_cache.get(ino, block_idx) {
                if cached.is_empty() {
                    debug!("read: EOF sentinel hit ino={} block={}", ino, block_idx);
                    break;
                }
                cached
            } else if self.read_cache.claim_inflight(ino, block_idx) {
                // We won the race — download synchronously.
                debug!("read cache miss: ino={} block={}", ino, block_idx);
                // The guard releases the claim on every way out of here, a
                // panic included. That is what lets a waiter treat an
                // unresolved claim as a download still running, rather than
                // guessing with a timer whether its owner is dead.
                let mut claim = InflightGuard::new(self.read_cache.clone(), ino, block_idx);
                match self.block(self.client.download(path, block_start, block_size)) {
                    Ok(b) if !b.is_empty() => {
                        self.read_cache.insert(ino, block_idx, b.clone());
                        claim.disarm();
                        b
                    }
                    Ok(empty) => {
                        // EOF — cache empty sentinel so any waiters know too.
                        self.read_cache.insert(ino, block_idx, empty);
                        claim.disarm();
                        break;
                    }
                    Err(e) => {
                        error!("read {}: {}", path, e);
                        return Err(e);
                    }
                }
            } else {
                // Another task is downloading this block — wait for it.
                debug!("read waiting for inflight: ino={} block={}", ino, block_idx);
                match self.read_cache.wait_for_block(ino, block_idx) {
                    Some(b) if b.is_empty() => break, // EOF sentinel
                    Some(b) => b,
                    // Other task had a real network error.
                    None => return Err(SynoFsError::Io("block download failed".into())),
                }
            };

            // Slice the portion of this block that overlaps [offset, offset+size)
            let local_start = offset.saturating_sub(block_start) as usize;
            let local_end = (offset + size)
                .saturating_sub(block_start)
                .min(block_size)
                .min(block.len() as u64) as usize;

            if local_start < local_end {
                result.extend_from_slice(&block[local_start..local_end]);
            }

            // A block shorter than the block size is the last one with data:
            // the server had nothing beyond it to give. Stop here. Continuing
            // would append the *next* block's bytes directly behind this one,
            // handing the caller a buffer whose tail comes from a different file
            // offset than it claims — silent corruption rather than a short read.
            if (block.len() as u64) < block_size {
                debug!(
                    "read: short block ino={} idx={} len={} < {}; stopping",
                    ino,
                    block_idx,
                    block.len(),
                    block_size
                );
                break;
            }
        }

        Ok(result)
    }

    /// Blocking wrappers over the transfer bundle. The callbacks all go through
    /// `start_*`; these exist so the tests can assert on what a transfer *does*
    /// (download sizing, failure handling) without a runtime dance.
    #[cfg(test)]
    fn truncate_file(
        &self,
        ino: u64,
        path: &str,
        new_size: u64,
    ) -> Result<SynoFileInfo, SynoFsError> {
        let xfer = self.transfers();
        self.block(xfer.truncate(ino, path, new_size))
    }

    #[cfg(test)]
    fn move_across_dirs(
        &self,
        old_path: &str,
        new_parent: &str,
        new_name: &str,
    ) -> Result<(), SynoFsError> {
        let xfer = self.transfers();
        self.block(xfer.move_across_dirs(old_path, new_parent, new_name))
    }

    /// Synthetic FileAttr for the virtual root (inode 1).
    fn virtual_root_attr(&self) -> FileAttr {
        FileAttr {
            ino: INodeNo(ROOT_INO),
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: self.owner.perm(true),
            nlink: 2,
            uid: self.owner.uid,
            gid: self.owner.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}
