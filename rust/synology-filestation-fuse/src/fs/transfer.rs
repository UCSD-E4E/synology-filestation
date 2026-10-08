//! Where an open handle's writes go, and the half of the mount that runs on
//! the Tokio runtime rather than on a FUSE event-loop thread.
//!
//! `flush`, `setattr` and `rename` hand their transfer to [`Transfers`] and
//! return immediately, so no multi-gigabyte copy ever occupies a callback
//! thread. The kernel reply is sent from the task when the transfer lands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::{debug, warn};

use super::attr::split_nas_path;
use crate::cache::{DirCache, InodeCache, ReadCache};
use crate::spill::{payload_for, upload_payload, SpillBuffer};
use synology_filestation_core::client::SynologyClient;
use synology_filestation_core::error::SynoFsError;
use synology_filestation_core::transport::WriteHandle;
use synology_filestation_core::types::{SynoFileInfo, VIRTUAL_ROOT_PATH};

/// Where an open handle's writes go.
///
/// Two shapes, because the transports differ in what they can be told. SMB
/// addresses ranges, so a write goes to the server as it arrives: memory is
/// bounded by one chunk, `write(2)` back-pressures at network speed instead of
/// returning instantly into a growing temp file, and a failure surfaces where
/// it happened rather than minutes later at `close(2)`. The HTTP Upload API
/// takes a whole file and nothing smaller, so it keeps the old shape.
pub(super) enum WriteSink {
    /// Streamed to the server. `None` once closed, or once a write failed and
    /// the handle was abandoned.
    Streamed(Option<Box<dyn WriteHandle>>),
    /// Held locally, spilling to a temp file, and uploaded whole on close.
    Buffered(SpillBuffer),
}

pub(super) struct WriteBuffer {
    pub(super) nas_path: String,
    pub(super) ino: u64,
    pub(super) sink: WriteSink,
    pub(super) dirty: bool,
    /// True when the file was just created and has not yet been uploaded.
    /// Allows the first upload to use overwrite=false, skipping the
    /// delete-before-upload round trips. Cleared once an upload succeeds.
    pub(super) new_file: bool,
    /// Set once a streamed write has actually reached the server. Until then
    /// a failed stream can still be replaced by the buffered path; after it,
    /// the file on the server is a prefix only this handle could complete.
    pub(super) streamed: bool,

    /// Set when a write failed. Streamed, the file on the server is short and
    /// the handle is gone; buffered, the buffer no longer matches what the
    /// caller wrote. Either way `close` must report that rather than claim the
    /// write landed — or, worse, upload what is left over the destination.
    pub(super) broken: bool,
}

/// An open handle: its buffer, and the file it is on.
///
/// The inode is kept outside the buffer's lock, which an upload holds for
/// the whole transfer. Asking "is anything still being written to this file?"
/// must not mean waiting for an unrelated upload to finish to find out which
/// file it was.
#[derive(Clone)]
pub(super) struct OpenHandle {
    pub(super) ino: u64,
    pub(super) buffer: Arc<tokio::sync::Mutex<WriteBuffer>>,
}

impl OpenHandle {
    pub(super) fn new(buffer: WriteBuffer) -> Self {
        Self {
            ino: buffer.ino,
            buffer: Arc::new(tokio::sync::Mutex::new(buffer)),
        }
    }
}

/// Open write handles, keyed by file handle. The outer lock guards the map and
/// is never held across I/O; each buffer has its own lock so a transfer on one
/// handle never blocks work on another. Shared with spawned transfer tasks,
/// hence the `Arc`.
pub(super) type Buffers = Arc<Mutex<HashMap<u64, OpenHandle>>>;

/// How many file transfers may be on the wire at once.
///
/// The FUSE event loop used to be this limit by accident — one dispatch thread
/// meant one transfer — which is also precisely why a single upload wedged the
/// mount. Now that transfers run on the Tokio runtime, nothing else bounds
/// them, and unbounded parallel FileStation transfers are what saturated
/// `synoscgi` (the shared per-request CGI backend behind the Download/Upload
/// APIs) and took the appliance down. Keep the fan-out single-digit: DSM's own
/// web client uploads one slice at a time.
pub(super) const MAX_CONCURRENT_TRANSFERS: usize = 4;

/// Everything a file transfer touches, cloned out of the filesystem so a
/// spawned task owns it and can outlive the callback that started it.
///
/// This is what lets `flush`, `setattr` and `rename` hand a multi-gigabyte
/// transfer to the runtime and return immediately: the FUSE callback keeps
/// nothing borrowed, and the kernel reply is sent from the task when the
/// transfer actually lands.
#[derive(Clone)]
pub(super) struct Transfers {
    pub(super) client: Arc<SynologyClient>,
    pub(super) cache: Arc<InodeCache>,
    pub(super) dir_cache: Arc<DirCache>,
    pub(super) read_cache: Arc<ReadCache>,
    pub(super) buffers: Buffers,
    pub(super) limit: Arc<tokio::sync::Semaphore>,
}

/// Forget the listing of the directory `path` sits in.
///
/// Called wherever this mount changes what a directory contains. The TTL is a
/// contract about changes made *elsewhere*; a file this process just created
/// has to appear in the very next listing, or the mount contradicts itself.
///
/// Free rather than a method because both the filesystem and its transfer half
/// change directory contents, and the rule is the same for each.
pub(super) fn forget_parent_listing(dir_cache: &DirCache, path: &str) {
    let parent = match path.rfind('/') {
        // A top-level share: its parent is the virtual root.
        Some(0) | None => VIRTUAL_ROOT_PATH,
        Some(cut) => &path[..cut],
    };
    dir_cache.invalidate(parent);
}

impl Transfers {
    pub(super) fn buffer(&self, fh: u64) -> Option<Arc<tokio::sync::Mutex<WriteBuffer>>> {
        self.buffers
            .lock()
            .unwrap()
            .get(&fh)
            .map(|h| h.buffer.clone())
    }

    /// Wait for a slot on the wire. Held only for the network call itself, so a
    /// queued transfer costs a future, not a thread.
    pub(super) async fn permit(&self) -> tokio::sync::SemaphorePermit<'_> {
        self.limit
            .acquire()
            .await
            .expect("the transfer semaphore is never closed")
    }

    /// Upload `fh`'s buffered data and resolve once the NAS has it.
    ///
    /// `dirty` is cleared only *after* the upload succeeds, so a failed flush
    /// leaves the data buffered for `release` to retry rather than silently
    /// discarding the write.
    pub(super) async fn upload(&self, fh: u64) -> Result<(), SynoFsError> {
        let handle = match self.buffer(fh) {
            Some(h) => h,
            // Unknown handle: there is genuinely nothing to do.
            None => return Ok(()),
        };
        // Lock first, permit second — never the other way round. A task holding
        // a permit while waiting for a buffer lock could be waiting on a task
        // that is itself queued for a permit; with every permit held by such a
        // waiter, nothing would ever progress.
        //
        // The lock is held for the whole transfer, which is the point: for a
        // spilled buffer the payload *is* the temp file being streamed, so a
        // concurrent write would tear the body mid-flight.
        let mut buf = handle.lock().await;
        if buf.broken {
            // A write already failed and took the handle with it. Reporting
            // success here would tell the application its file is safe.
            return Err(SynoFsError::Io(format!(
                "close {}: an earlier write to this file failed",
                buf.nas_path
            )));
        }
        if !buf.dirty {
            // Nothing written since the last successful upload.
            return Ok(());
        }

        // Decide what this handle needs before touching any other field: the
        // match borrows the sink, and the rest of the work wants the buffer.
        enum Finish {
            Close(Option<Box<dyn WriteHandle>>),
            Send(crate::spill::Payload),
        }
        let finish = match &mut buf.sink {
            WriteSink::Streamed(slot) => Finish::Close(slot.take()),
            WriteSink::Buffered(spill) => {
                Finish::Send(payload_for(spill).map_err(|e| SynoFsError::Io(e.to_string()))?)
            }
        };
        let (nas_path, ino, overwrite) = (buf.nas_path.clone(), buf.ino, !buf.new_file);

        match finish {
            // Streamed: the bytes went out as they were written, so closing is
            // the whole of "make it durable".
            Finish::Close(stream) => {
                if let Some(mut stream) = stream {
                    let _permit = self.permit().await;
                    stream.close().await?;
                }
                debug!("close: fh={} path={}", fh, nas_path);
            }
            Finish::Send(payload) => {
                let (parent, filename) = match split_nas_path(&nas_path) {
                    Some(v) => v,
                    None => return Err(SynoFsError::InvalidArg),
                };
                debug!(
                    "upload: fh={} parent={:?} filename={:?} size={}",
                    fh,
                    parent,
                    filename,
                    payload.len()
                );
                let _permit = self.permit().await;
                upload_payload(&self.client, parent, filename, payload, overwrite).await?;
            }
        }

        // Durable now: stop advertising the buffer as dirty, and record that the
        // file exists on the NAS so a later flush overwrites instead of racing
        // an `overwrite=false` create against itself.
        buf.dirty = false;
        buf.new_file = false;
        drop(buf);

        self.cache.invalidate_path(&nas_path);
        forget_parent_listing(&self.dir_cache, &nas_path);
        self.read_cache.invalidate_ino(ino);
        Ok(())
    }

    /// Resize `path` to `new_size`, zero-extending or truncating.
    ///
    /// FileStation has no truncate call, so this is read-modify-write. A
    /// download that fails must abort the whole operation, because the
    /// alternative — treating an unreadable file as an empty one — uploads
    /// `new_size` zero bytes over perfectly good data.
    pub(super) async fn truncate(
        &self,
        ino: u64,
        path: &str,
        new_size: u64,
    ) -> Result<SynoFileInfo, SynoFsError> {
        // The client picks how: a backend that can set a length does it in one
        // round trip, and only the HTTP fallback still has to move the file's
        // contents to change one number.
        {
            let _permit = self.permit().await;
            self.client.truncate(path, new_size).await?;
        }

        self.read_cache.invalidate_ino(ino);
        self.cache.invalidate_path(path);
        self.client.get_info(path).await
    }

    /// Move a file between directories: download, upload to the new location,
    /// then delete the source. FileStation's Rename API cannot move across
    /// directories, so this is the only route.
    pub(super) async fn move_across_dirs(
        &self,
        old_path: &str,
        new_parent: &str,
        new_name: &str,
    ) -> Result<(), SynoFsError> {
        // `length = 0` means "the whole file". Sizing this request from the
        // inode cache instead would silently truncate the file whenever that
        // cached size is stale-low — and this path deletes the source
        // afterwards, so the missing bytes are simply gone.
        let permit = self.permit().await;
        let data = self.client.download(old_path, 0, 0).await?;
        drop(permit);

        {
            let _permit = self.permit().await;
            self.client
                .upload(new_parent, new_name, data.to_vec(), true)
                .await?;
        }

        // Only the source deletion is best-effort: the copy is already safe on
        // the NAS, so a failure here leaves a duplicate rather than losing data.
        if let Err(e) = self.client.delete(old_path).await {
            warn!(
                "rename: uploaded {}/{} but failed to delete {}: {}",
                new_parent, new_name, old_path, e
            );
        }
        Ok(())
    }
}
