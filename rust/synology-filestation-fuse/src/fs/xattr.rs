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
//!
//! A hash cannot be called off once started: fuser drops `FUSE_INTERRUPT`, so
//! a `getfattr` that is killed leaves its hash running until DSM finishes or
//! core's ceiling (15 min) passes. What keeps that cheap is that hashes have
//! their own small allowance ([`MAX_CONCURRENT_HASHES`]), apart from the
//! transfer slots, and that asking again joins the hash already running for
//! that file rather than starting another.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};

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

/// How many hashes may run at once. Separate from the transfer slots and
/// smaller: each one is the NAS reading a whole file, and sharing the
/// transfer slots let a few big ones stall every `close(2)` on the mount.
pub(super) const MAX_CONCURRENT_HASHES: usize = 2;

/// What a `getxattr` is answered with. A value of its own rather than a
/// `ReplyXattr` so the policy can be tested without a kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum XattrAnswer {
    /// How long the value is, for a caller that asked with no buffer.
    Size(u32),
    Value(Vec<u8>),
    Error(Errno),
}

/// Someone waiting on a hash.
type Waiter = Box<dyn FnOnce(XattrAnswer) + Send>;

/// Hashes in flight, by path, with everyone waiting on each.
pub(super) type Hashes = Arc<Mutex<HashMap<String, Vec<Waiter>>>>;

impl SynologyFS {
    /// Answer a `getxattr`, handing the answer to `done`.
    ///
    /// Everything that can be answered from memory is, on the calling thread:
    /// once `getxattr` is implemented at all, the kernel asks it for
    /// `security.capability` on writes and `ls` asks for SELinux labels and
    /// ACLs, so those have to cost nothing. Whatever needs the NAS — a
    /// directory check for a path whose details have aged out, and the hash —
    /// runs on the runtime, as an upload does.
    ///
    /// The length is promised only once everything a value depends on has
    /// been checked, so a tool that probes with no buffer is not told the
    /// attribute exists where the value would then fail.
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
        let xfer = self.transfers();
        if xfer.busy(&path) {
            // Hashed now, it would be the digest of a half-written file.
            debug!("getxattr {MD5_XATTR}: {path} has writes still to land");
            return done(XattrAnswer::Error(Errno::EBUSY));
        }
        if size != 0 && size < MD5_HEX_LEN {
            return done(XattrAnswer::Error(Errno::ERANGE));
        }
        if size == 0 && cached_dir == Some(false) {
            return done(XattrAnswer::Size(MD5_HEX_LEN));
        }

        let limit = self.hash_limit.clone();
        let hashes = self.hashes.clone();
        self.rt.spawn(async move {
            if cached_dir.is_none() {
                match xfer.client.get_info(&path).await {
                    Ok(info) if info.isdir => return done(XattrAnswer::Error(Errno::ENODATA)),
                    Ok(_) => {}
                    Err(e) => return done(XattrAnswer::Error(errno(e.to_errno()))),
                }
            }
            if size == 0 {
                return done(XattrAnswer::Size(MD5_HEX_LEN));
            }

            // Join a hash already running for this file, or lead one.
            {
                let mut running = hashes.lock().unwrap();
                if let Some(waiting) = running.get_mut(&path) {
                    waiting.push(Box::new(done));
                    return;
                }
                running.insert(path.clone(), vec![Box::new(done)]);
            }

            let answer = {
                let _slot = limit.acquire().await.expect("never closed");
                // The wait for a slot can be long, and a writer may have
                // started meanwhile.
                if xfer.busy(&path) {
                    XattrAnswer::Error(Errno::EBUSY)
                } else {
                    match xfer.client.md5(&path).await {
                        Ok(digest) if is_md5_hex(&digest) => {
                            XattrAnswer::Value(digest.to_ascii_lowercase().into_bytes())
                        }
                        Ok(digest) => {
                            warn!("getxattr {MD5_XATTR} {path}: the NAS answered {digest:?}");
                            XattrAnswer::Error(Errno::EIO)
                        }
                        Err(e) => {
                            warn!("getxattr {MD5_XATTR} {path}: {e}");
                            XattrAnswer::Error(errno(e.to_errno()))
                        }
                    }
                }
            };

            let waiting = hashes.lock().unwrap().remove(&path).unwrap_or_default();
            for waiter in waiting {
                waiter(answer.clone());
            }
        });
    }
}

/// The 32 hex digits the length was promised as.
fn is_md5_hex(digest: &str) -> bool {
    digest.len() == MD5_HEX_LEN as usize && digest.bytes().all(|b| b.is_ascii_hexdigit())
}
