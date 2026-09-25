//! The `fuser::Filesystem` callbacks — the kernel's entry points into the
//! mount. Each one validates its arguments, then delegates to the helpers on
//! [`SynologyFS`] or hands the work to [`Transfers`]; the policy lives there,
//! and what is left here is the shape of the FUSE protocol.

use std::ffi::OsStr;
use std::io;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use fuser::{
    BsdFileFlags, Errno, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, LockOwner,
    OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, WriteFlags,
};
use tracing::{debug, error, info, warn};

use super::attr::{dir_open_flags, errno, file_attr};
use super::transfer::forget_parent_listing;
use super::{SynologyFS, ROOT_INO, TTL};
use synology_filestation_core::error::SynoFsError;
use synology_filestation_core::types::{SynoFileInfo, VIRTUAL_ROOT_PATH};

impl Filesystem for SynologyFS {
    fn init(&mut self, _req: &Request, _config: &mut fuser::KernelConfig) -> io::Result<()> {
        debug!("init: seeding virtual root");
        // Seed inode 1 with a virtual root that has no real NAS path.
        // readdir and lookup on this inode use list_shares() instead of list_dir().
        // We don't make any API call here — the session was already verified during login.
        let root_info = SynoFileInfo {
            name: String::new(),
            path: VIRTUAL_ROOT_PATH.to_string(),
            isdir: true,
            additional: None,
            code: None,
        };
        self.cache.seed_root(root_info);
        info!("NAS is mounted and ready for use");
        Ok(())
    }

    fn destroy(&mut self) {
        debug!("destroy: flushing write buffers and logging out");
        // Transfers now run on the runtime, which dies with the process — so
        // unmount has to wait for anything still in flight rather than leave it
        // to be aborted. Taking each buffer's lock is that wait: an upload holds
        // it for the whole transfer, and whatever is still dirty afterwards is
        // work that never started (or failed) and is uploaded here.
        let fhs: Vec<u64> = self.write_buffers.lock().unwrap().keys().cloned().collect();
        for fh in fhs {
            if let Err(e) = self.finish_upload(fh) {
                warn!("destroy: failed to flush fh {}: {}", fh, e);
            }
        }
        let _ = self.block(self.client.logout());
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parent_path = match self.get_path_for_ino(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Looking up a share name directly under the virtual root.
        if parent_path == VIRTUAL_ROOT_PATH {
            debug!("lookup share: {}", name_str);
            let shares = match self.listing(VIRTUAL_ROOT_PATH) {
                Ok(s) => s,
                Err(e) => {
                    reply.error(errno(e.to_errno()));
                    return;
                }
            };
            match shares.iter().find(|s| s.name == name_str).cloned() {
                Some(info) => {
                    let ino = self.cache.get_or_alloc_ino(&info.path);
                    let attr = self.syno_to_attr(ino, &info);
                    self.cache.insert(ino, info);
                    reply.entry(&TTL, &attr, fuser::Generation(0));
                }
                None => reply.error(Errno::ENOENT),
            }
            return;
        }

        let child_path = format!("{}/{}", parent_path.trim_end_matches('/'), name_str);
        debug!("lookup: {}", child_path);

        // Check cache first
        if let Some(entry) = self
            .cache
            .get_by_ino(self.cache.get_or_alloc_ino(&child_path))
        {
            let attr = self.syno_to_attr(entry.ino, &entry.info);
            reply.entry(&TTL, &attr, fuser::Generation(0));
            return;
        }

        match self.block(self.client.get_info(&child_path)) {
            Ok(info) => {
                let ino = self.cache.get_or_alloc_ino(&child_path);
                let attr = self.syno_to_attr(ino, &info);
                self.cache.insert(ino, info);
                reply.entry(&TTL, &attr, fuser::Generation(0));
            }
            Err(SynoFsError::NotFound) | Err(SynoFsError::ApiError(408 | 414 | 415)) => {
                reply.error(Errno::ENOENT);
            }
            Err(e) => {
                error!("lookup {}: {}", child_path, e);
                reply.error(errno(e.to_errno()));
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = ino.0;
        debug!("getattr: ino={}", ino);

        // The virtual root has no real NAS path — always return synthetic attrs directly.
        if ino == ROOT_INO {
            reply.attr(&TTL, &self.virtual_root_attr());
            return;
        }

        // Serve from cache if available
        if let Some(entry) = self.cache.get_by_ino(ino) {
            let attr = self.syno_to_attr(ino, &entry.info);
            reply.attr(&TTL, &attr);
            return;
        }

        // Cache miss: look up the path and fetch from API
        let path = match self.get_path_for_ino(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.block(self.client.get_info(&path)) {
            Ok(info) => {
                let attr = self.syno_to_attr(ino, &info);
                self.cache.insert(ino, info);
                reply.attr(&TTL, &attr);
            }
            Err(e) => {
                error!("getattr ino={} path={}: {}", ino, path, e);
                reply.error(errno(e.to_errno()));
            }
        }
    }

    /// Nothing to open — a directory here is a listing, not a handle — but the
    /// reply is the only place to tell the kernel it may cache one, and
    /// fuser's default says nothing. See [`dir_open_flags`].
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.get_path_for_ino(ino.0).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        reply.opened(FileHandle(0), dir_open_flags(self.dir_cache.is_enabled()));
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = ino.0;
        let path = match self.get_path_for_ino(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        debug!("readdir: ino={} path={} offset={}", ino, path, offset);

        // Virtual root: list FileStation shares instead of a real directory.
        let (entries, parent_ino) = if path == VIRTUAL_ROOT_PATH {
            let shares = match self.listing(VIRTUAL_ROOT_PATH) {
                Ok(s) => s,
                Err(e) => {
                    error!("readdir shares: {}", e);
                    reply.error(errno(e.to_errno()));
                    return;
                }
            };
            debug!("readdir root: got {} shares", shares.len());
            (shares, ROOT_INO)
        } else {
            let entries = match self.listing(&path) {
                Ok(e) => e,
                Err(e) => {
                    error!("readdir {}: {}", path, e);
                    reply.error(errno(e.to_errno()));
                    return;
                }
            };
            let parent_ino = path
                .rfind('/')
                .map(|i| {
                    let parent_path = &path[..i];
                    // parent of a top-level share (e.g. "/homes") is the virtual root
                    if parent_path.is_empty() {
                        ROOT_INO
                    } else {
                        self.cache.get_or_alloc_ino(parent_path)
                    }
                })
                .unwrap_or(ROOT_INO);
            (entries, parent_ino)
        };

        // Build full entry list: ".", "..", then actual entries
        let mut all_entries: Vec<(u64, FileType, String)> = Vec::new();
        all_entries.push((ino, FileType::Directory, ".".to_string()));
        all_entries.push((parent_ino, FileType::Directory, "..".to_string()));

        for file_info in entries.iter() {
            let child_ino = self.cache.get_or_alloc_ino(&file_info.path);
            let kind = if file_info.isdir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let name = file_info.name.clone();
            self.cache.insert(child_ino, file_info.clone());
            all_entries.push((child_ino, kind, name));
        }

        for (i, (child_ino, kind, name)) in all_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*child_ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino = ino.0;
        if size == 0 {
            reply.data(&[]);
            return;
        }

        let path = match self.get_path_for_ino(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let size = size as u64;
        let block_size = self.read_cache.block_size;
        let last_block = (offset + size - 1) / block_size;

        debug!(
            "read: ino={} path={} offset={} size={} blocks=[{}..={}]",
            ino,
            path,
            offset,
            size,
            offset / block_size,
            last_block
        );

        let result = match self.read_range(ino, &path, offset, size) {
            Ok(r) => r,
            Err(e) => {
                reply.error(errno(e.to_errno()));
                return;
            }
        };

        // Read-ahead, but only for a reader that has shown it is streaming.
        // This used to fire on every read, unconditionally and unclamped: 16
        // blocks of speculative HTTP behind a caller who may have wanted 48
        // bytes and may be about to close the file.
        self.read_ahead(fh.0, ino, &path, offset, size);

        reply.data(&result);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = ino.0;
        let path = match self.get_path_for_ino(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        debug!("open: ino={} path={} flags={:#o}", ino, path, flags.0);

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        // Block 0 eagerly, and the rest of the window only if block 0 turns out
        // to be a container that keeps its index at the end. A JPEG used to pay
        // for a 20-block media window it could never use.
        self.prime_open(fh, ino, &path);
        let writable = flags.acc_mode() != fuser::OpenAccMode::O_RDONLY;
        self.add_write_buffer(fh, path, ino, false, writable);
        // FOPEN_KEEP_CACHE: don't invalidate the kernel page cache between opens.
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino = ino.0;
        let fh = fh.0;
        debug!(
            "write: ino={} fh={} offset={} len={}",
            ino,
            fh,
            offset,
            data.len()
        );

        // The buffer spills to a temp file once it outgrows SpillBuffer's
        // threshold, so a large copy no longer pins the whole file in RAM. A
        // flush already running on this handle holds the buffer's lock, so this
        // waits for that transfer rather than mutating the file it is streaming.
        match self.write_buffer_at(fh, offset, data) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => {
                error!("write: buffering failed: {}", e);
                reply.error(Errno::EIO);
            }
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let fh = fh.0;
        debug!("flush: fh={}", fh);
        // The kernel returns *this* reply from close(2), and discards whatever
        // release() reports. So the reply has to carry the upload's real
        // outcome — but it does not have to be produced here. The transfer runs
        // on the runtime and answers the kernel when it lands, leaving this
        // event-loop thread free to serve the rest of the mount meanwhile.
        self.start_upload(fh, move |r| match r {
            Ok(()) => reply.ok(),
            Err(e) => {
                error!("flush fh={}: {}", fh, e);
                reply.error(errno(e.to_errno()));
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let fh = fh.0;
        debug!("release: fh={}", fh);
        // Whatever this handle was still speculatively downloading is now work
        // for a file nobody is reading. Dropping it here is what stops a
        // file-at-a-time walk from having every closed file compete with its
        // successors for the same appliance.
        self.end_read(fh, ino.0);
        // Same treatment as flush, plus the handle teardown — which has to wait
        // for the upload, since the buffer owns the spill file being streamed.
        let buffers = self.write_buffers.clone();
        self.start_upload(fh, move |r| {
            buffers.lock().unwrap().remove(&fh);
            match r {
                Ok(()) => reply.ok(),
                Err(e) => {
                    error!("release fh={}: {}", fh, e);
                    reply.error(errno(e.to_errno()));
                }
            }
        });
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parent_path = match self.get_path_for_ino(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let new_path = format!("{}/{}", parent_path.trim_end_matches('/'), name_str);
        debug!("create: {}", new_path);

        // The directory now has a file in it that its cached listing does not
        // mention, and the upload does not happen until flush.
        self.dir_cache.invalidate(&parent_path);

        // Defer the actual NAS upload to flush/release.  Allocate an inode and
        // seed the cache with a synthetic entry so getattr works before the first
        // flush.  The write buffer is marked new_file=true so flush uses
        // overwrite=false, skipping the delete-before-upload round trips.
        let ino = self.cache.get_or_alloc_ino(&new_path);
        let synthetic_info = synology_filestation_core::types::SynoFileInfo {
            name: name_str.to_string(),
            path: new_path.clone(),
            isdir: false,
            additional: None,
            code: None,
        };
        let attr = self.syno_to_attr(ino, &synthetic_info);
        self.cache.insert(ino, synthetic_info);

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.add_write_buffer(fh, new_path, ino, true, true);

        reply.created(
            &TTL,
            &attr,
            fuser::Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let parent_path = match self.get_path_for_ino(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = format!("{}/{}", parent_path.trim_end_matches('/'), name_str);
        debug!("unlink: {}", path);

        match self.block(self.client.delete(&path)) {
            Ok(()) => {
                if let Some(ino) = self.cache.get_ino_for_path(&path) {
                    self.read_cache.invalidate_ino(ino);
                }
                self.cache.invalidate_path(&path);
                forget_parent_listing(&self.dir_cache, &path);
                reply.ok();
            }
            Err(e) => {
                error!("unlink {}: {}", path, e);
                reply.error(errno(e.to_errno()));
            }
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let parent_path = match self.get_path_for_ino(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = format!("{}/{}", parent_path.trim_end_matches('/'), name_str);
        debug!("rmdir: {}", path);

        match self.block(self.client.delete(&path)) {
            Ok(()) => {
                self.cache.invalidate_prefix(&path);
                self.dir_cache.invalidate_prefix(&path);
                forget_parent_listing(&self.dir_cache, &path);
                reply.ok();
            }
            Err(e) => {
                error!("rmdir {}: {}", path, e);
                reply.error(errno(e.to_errno()));
            }
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let parent_path = match self.get_path_for_ino(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        debug!("mkdir: {}/{}", parent_path, name_str);

        match self.block(self.client.create_folder(&parent_path, name_str)) {
            Ok(info) => {
                let ino = self.cache.get_or_alloc_ino(&info.path);
                let attr = self.syno_to_attr(ino, &info);
                self.cache.insert(ino, info);
                self.dir_cache.invalidate(&parent_path);
                reply.entry(&TTL, &attr, fuser::Generation(0));
            }
            Err(e) => {
                error!("mkdir {}/{}: {}", parent_path, name_str, e);
                reply.error(errno(e.to_errno()));
            }
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = parent.0;
        let new_parent = new_parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let new_name_str = match new_name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let parent_path = match self.get_path_for_ino(parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let new_parent_path = match self.get_path_for_ino(new_parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let old_path = format!("{}/{}", parent_path.trim_end_matches('/'), name_str);
        let new_path = format!("{}/{}", new_parent_path.trim_end_matches('/'), new_name_str);
        debug!("rename: {} -> {}", old_path, new_path);

        // Same directory: use the efficient Rename API.
        // POSIX rename(2) must atomically replace the destination if it exists.
        // The Synology Rename API does not overwrite — delete the destination first.
        if parent == new_parent {
            if old_path != new_path {
                if let Some(ino) = self.cache.get_ino_for_path(&new_path) {
                    self.read_cache.invalidate_ino(ino);
                }
                self.cache.invalidate_path(&new_path);
                forget_parent_listing(&self.dir_cache, &new_path);
                let _ = self.block(self.client.delete(&new_path)); // ignore error — may not exist
            }
            match self.block(self.client.rename(&old_path, new_name_str)) {
                Ok(info) => {
                    if let Some(ino) = self.cache.get_ino_for_path(&old_path) {
                        self.read_cache.invalidate_ino(ino);
                    }
                    self.cache.invalidate_path(&old_path);
                    forget_parent_listing(&self.dir_cache, &old_path);
                    let ino = self.cache.get_or_alloc_ino(&new_path);
                    self.cache.insert(ino, info);
                    reply.ok();
                }
                Err(e) => {
                    error!("rename {} -> {}: {}", old_path, new_path, e);
                    reply.error(errno(e.to_errno()));
                }
            }
            return;
        }

        // Cross-directory: check if it's a directory (not supported atomically)
        let is_dir = self
            .cache
            .get_by_ino(self.cache.get_or_alloc_ino(&old_path))
            .map(|e| e.info.isdir)
            .unwrap_or(false);

        if is_dir {
            // Cross-directory move of directories is not supported
            warn!("rename: cross-directory move of directories not supported");
            reply.error(Errno::ENOSYS);
            return;
        }

        // Cross-directory file move: download, upload, delete. A whole-file
        // transfer, so it goes to the runtime rather than sitting on this
        // event-loop thread for the length of the copy.
        let cache = self.cache.clone();
        let dir_cache = self.dir_cache.clone();
        let old = old_path.clone();
        let new = new_path.clone();
        self.start_move_across_dirs(
            old_path,
            new_parent_path,
            new_name_str.to_string(),
            move |r| match r {
                Ok(()) => {
                    cache.invalidate_path(&old);
                    // Both ends changed, and the move only lands when the
                    // transfer does — so this cannot be done up front with the
                    // same-directory case above.
                    forget_parent_listing(&dir_cache, &old);
                    forget_parent_listing(&dir_cache, &new);
                    reply.ok();
                }
                Err(e) => {
                    error!("rename {} -> {}: {}", old, new, e);
                    reply.error(errno(e.to_errno()));
                }
            },
        );
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = ino.0;
        // Only handle size truncation (used by truncate())
        if let Some(new_size) = size {
            let path = match self.get_path_for_ino(ino) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            debug!(
                "setattr truncate: ino={} path={} new_size={}",
                ino, path, new_size
            );

            // Read-modify-write over the whole file, so it too runs on the
            // runtime and replies from there.
            let cache = self.cache.clone();
            let owner = self.owner;
            let failed_path = path.clone();
            self.start_truncate(ino, path, new_size, move |r| match r {
                Ok(info) => {
                    let attr = file_attr(owner, ino, &info);
                    cache.insert(ino, info);
                    reply.attr(&TTL, &attr);
                }
                Err(e) => {
                    error!("setattr truncate {}: {}", failed_path, e);
                    reply.error(errno(e.to_errno()));
                }
            });
        } else {
            // For other attribute changes (permissions, timestamps), return current attrs
            // The Synology API doesn't support changing these
            if let Some(entry) = self.cache.get_by_ino(ino) {
                let attr = self.syno_to_attr(ino, &entry.info);
                reply.attr(&TTL, &attr);
            } else {
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // Return reasonable placeholder stats
        // A future improvement could call SYNO.FileStation.Info for real quota info
        reply.statfs(
            u64::MAX / 4096, // blocks
            u64::MAX / 4096, // bfree
            u64::MAX / 4096, // bavail
            u64::MAX,        // files
            u64::MAX,        // ffree
            4096,            // bsize
            255,             // namelen
            4096,            // frsize
        );
    }
}
