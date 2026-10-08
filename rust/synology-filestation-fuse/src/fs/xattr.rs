//! Extended attributes: one, `user.synology.md5`, the NAS's own MD5 of a file.
//!
//! `md5sum` on the mount reads every byte back over the network. There is no
//! filesystem operation for "this file's checksum" it could ask instead, so
//! checking an upload landed meant downloading it again. DSM can hash a file
//! on its own disk (`SYNO.FileStation.MD5`), and an extended attribute is the
//! nearest thing to a checksum operation that a stock tool can ask for by
//! name:
//!
//! ```text
//! getfattr -n user.synology.md5 --only-values <file>
//! ```
//!
//! It is deliberately **not listed**. `cp -a`, `rsync -X` and file managers
//! copy every attribute a file lists by reading it, and each read here is the
//! NAS reading the whole file: a listed digest would have copying a folder
//! off the mount hash every file in it.

use std::ffi::OsStr;

use fuser::Errno;
use tracing::{debug, warn};

use super::attr::errno;
use super::{SynologyFS, ROOT_INO};

/// The one attribute this mount has.
pub(super) const MD5_XATTR: &str = "user.synology.md5";

/// An MD5 in lowercase hex is always this long, so the kernel's "how big is
/// it?" is answered without a hash. It asks that before every value, and
/// answering it by hashing would have the NAS read the file twice.
const MD5_HEX_LEN: u32 = 32;

/// What a `getxattr` is answered with. A value of its own rather than a
/// `ReplyXattr` so the policy can be tested without a kernel.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum XattrAnswer {
    /// How long the value is, for a caller that asked with no buffer.
    Size(u32),
    Value(Vec<u8>),
    Error(Errno),
}

impl SynologyFS {
    /// Answer a `getxattr`, handing the answer to `done`.
    ///
    /// Everything but the hash is answered at once, on the calling thread:
    /// once `getxattr` is implemented at all, the kernel asks it for
    /// `security.capability` on writes and `ls` asks for SELinux labels and
    /// ACLs, so those have to cost nothing. The hash runs on the runtime, as
    /// an upload does — the NAS reads the whole file, minutes for a big one,
    /// and the FUSE thread that asked has the rest of the mount to serve. It
    /// holds a transfer slot meanwhile, since to the appliance it is a
    /// download's worth of reading.
    pub(super) fn start_getxattr<F>(&self, ino: u64, name: &OsStr, size: u32, done: F)
    where
        F: FnOnce(XattrAnswer) + Send + 'static,
    {
        if name != OsStr::new(MD5_XATTR) || ino == ROOT_INO {
            return done(XattrAnswer::Error(Errno::ENODATA));
        }
        let Some(path) = self.get_path_for_ino(ino) else {
            return done(XattrAnswer::Error(Errno::ENOENT));
        };
        let cached_dir = self.cache.get_by_ino(ino).map(|e| e.info.isdir);
        if cached_dir == Some(true) {
            return done(XattrAnswer::Error(Errno::ENODATA));
        }
        if size == 0 {
            return done(XattrAnswer::Size(MD5_HEX_LEN));
        }
        if size < MD5_HEX_LEN {
            return done(XattrAnswer::Error(Errno::ERANGE));
        }
        if self.writing(ino) {
            // Hashed now, it would be the digest of a half-uploaded file.
            debug!("getxattr {MD5_XATTR}: {path} has writes still to land");
            return done(XattrAnswer::Error(Errno::EBUSY));
        }

        let xfer = self.transfers();
        self.rt.spawn(async move {
            if cached_dir.is_none() {
                match xfer.client.get_info(&path).await {
                    Ok(info) if info.isdir => return done(XattrAnswer::Error(Errno::ENODATA)),
                    Ok(_) => {}
                    Err(e) => return done(XattrAnswer::Error(errno(e.to_errno()))),
                }
            }
            let _slot = xfer.permit().await;
            match xfer.client.md5(&path).await {
                Ok(digest) => done(XattrAnswer::Value(digest.into_bytes())),
                Err(e) => {
                    warn!("getxattr {MD5_XATTR} {path}: {e}");
                    done(XattrAnswer::Error(errno(e.to_errno())))
                }
            }
        });
    }

    /// The attribute names `listxattr` reports for `ino`: none. See the
    /// module documentation for why the digest is not among them.
    pub(super) fn listed_xattrs(&self, _ino: u64) -> Vec<u8> {
        Vec::new()
    }

    /// Whether an open handle on `ino` holds writes the NAS does not have
    /// yet: unflushed, mid-upload, or from a write that failed.
    fn writing(&self, ino: u64) -> bool {
        let handles: Vec<_> = self
            .write_buffers
            .lock()
            .unwrap()
            .values()
            .filter(|h| h.ino == ino)
            .cloned()
            .collect();
        handles.iter().any(|h| match h.buffer.try_lock() {
            // An upload holds the lock for the whole transfer.
            Err(_) => true,
            Ok(buffer) => buffer.dirty || buffer.broken,
        })
    }
}
