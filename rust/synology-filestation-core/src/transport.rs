//! Pluggable read/write **backends** and the health tracking that lets
//! [`SynologyClient`](crate::SynologyClient) prefer them over the HTTP
//! FileStation API — while transparently falling back to HTTP when a backend is
//! unavailable.
//!
//! The motivation is the NAS-saturation incident: the HTTP Download/Upload API
//! is proxied through the shared `synoscgi` backend, so a bulk transfer load can
//! take the appliance down. An alternative transport (SMB today; NFS, S3, or a
//! cache tomorrow) that talks to a *different* service relieves it. Backends
//! plug in by **dependency inversion**: they implement [`ReadTransport`] /
//! [`WriteTransport`] here, and consumers inject them at construction — the
//! read/write call sites (`download`/`upload`) never change, and this crate
//! never depends on any backend's protocol library.
//!
//! ## Error contract (the integration seam)
//!
//! An implementor maps its failures onto [`SynoFsError`] so the selection layer
//! can distinguish, via [`SynoFsError::category`]:
//!
//! * **transient** (connection lost / timeout / transport) → `Transport` — the
//!   selection layer trips this backend's [`CircuitBreaker`] and falls back;
//! * **definitive** (not-found / permission / …) → propagate unchanged; the
//!   backend is healthy and gave a real answer, so falling back would just ask
//!   the same NAS the same question.
//!
//! ## Write contract
//!
//! [`WriteTransport::write`] MUST NOT leave a partial/corrupt target: on success
//! the whole file is replaced; on failure the previous file is preserved
//! (**old-or-new**, never a partial, no data lost). That is what makes fallback
//! *safe* — a failed primary write can't leave a half-file for the HTTP path (or
//! another backend) to collide with. It need not be a single atomic operation.

use std::path::Path;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::SynoFsError;
use crate::types::SynoFileInfo;

/// A read backend serving file bytes as an alternative to the HTTP Download API.
#[async_trait]
pub trait ReadTransport: Send + Sync {
    /// Read `length` bytes at `offset`; `length == 0` reads the whole file
    /// (mirroring [`SynologyClient::download`](crate::SynologyClient::download)).
    async fn read(&self, path: &str, offset: u64, length: u64) -> Result<Bytes, SynoFsError>;
}

/// A read backend that **streams** a remote file to a local path without
/// buffering it in memory — the symmetric counterpart of [`StreamWriteTransport`],
/// for fetching large files (e.g. `.ORF`).
///
/// The destination is a **local path**: the implementor writes it atomically
/// (old-or-nothing — a failed fetch leaves no file, so the selection layer's
/// HTTP fallback can write cleanly). Same transient-vs-definitive error contract
/// as [`ReadTransport`].
#[async_trait]
pub trait StreamReadTransport: Send + Sync {
    /// Stream `remote_path` into the local file `local`, atomically.
    async fn read_to_path(&self, remote_path: &str, local: &Path) -> Result<(), SynoFsError>;
}

/// A write backend replacing a whole file, as an alternative to the HTTP Upload
/// API. Implementations MUST NOT leave a partial target on failure — see the
/// [module docs](self#write-contract).
#[async_trait]
pub trait WriteTransport: Send + Sync {
    /// Replace the whole file at `path` with `data`. On failure the previous
    /// file must be preserved (old-or-new, never a partial target).
    async fn write(&self, path: &str, data: &[u8]) -> Result<(), SynoFsError>;
}

/// A write backend that **streams** a local file to `remote_path` without
/// buffering it in memory — for staging large files (e.g. `.ORF`).
///
/// The source is a **local path**, not a reader, deliberately: a failed attempt
/// must be retryable by the selection layer's HTTP fallback, and a consumed
/// reader can't be rewound while a re-openable file can. Same old-or-new /
/// error contract as [`WriteTransport`].
#[async_trait]
pub trait StreamWriteTransport: Send + Sync {
    /// Stream the contents of local file `local` into `remote_path`, replacing
    /// it (old-or-new).
    async fn write_from_path(&self, remote_path: &str, local: &Path) -> Result<(), SynoFsError>;

    /// Stream `local` into `remote_path`, which **must not already exist**.
    ///
    /// Creating a file is where the volume is — a mount's large copies are new
    /// files, not replacements — so a backend that cannot serve this case
    /// doesn't get the traffic that matters. But "create" carries a promise
    /// [`write_from_path`](Self::write_from_path) does not: an existing name is
    /// [`SynoFsError::AlreadyExists`], never a silent clobber. Only a backend
    /// that can make that atomic (SMB's `FILE_CREATE` disposition, say) should
    /// implement it.
    ///
    /// The default declines with [`SynoFsError::NotSupported`], which the
    /// selection layer reads as "not for you" rather than "you failed" — the
    /// backend keeps its breaker shut and still serves replacing writes.
    async fn write_new_from_path(
        &self,
        remote_path: &str,
        local: &Path,
    ) -> Result<(), SynoFsError> {
        let _ = (remote_path, local);
        Err(SynoFsError::NotSupported)
    }
}

/// A backend that can answer questions about the *namespace* — what is here,
/// how big is it — and change it: create a directory, rename, delete.
///
/// Kept apart from the byte-moving traits because the capabilities are
/// genuinely separate. SMB can serve all of it; a bulk object store might only
/// carry contents. Every method defaults to [`SynoFsError::NotSupported`], so a
/// backend implements the operations it can actually promise and the selection
/// layer quietly uses HTTP for the rest — a decline is not a failure and does
/// not count against the backend's breaker.
///
/// Paths are FileStation logical paths (`/share/dir/file`), so a backend that
/// addresses storage differently translates on its own side. Return types match
/// the HTTP client's, because callers must not be able to tell which backend
/// answered.
#[async_trait]
pub trait MetadataTransport: Send + Sync {
    /// The shares available to this session, as directory entries.
    async fn list_shares(&self) -> Result<Vec<SynoFileInfo>, SynoFsError> {
        Err(SynoFsError::NotSupported)
    }

    /// Entries directly under `folder_path`.
    async fn list_dir(&self, folder_path: &str) -> Result<Vec<SynoFileInfo>, SynoFsError> {
        let _ = folder_path;
        Err(SynoFsError::NotSupported)
    }

    /// Metadata for a single entry.
    async fn get_info(&self, path: &str) -> Result<SynoFileInfo, SynoFsError> {
        let _ = path;
        Err(SynoFsError::NotSupported)
    }

    /// Create directory `name` under `parent`.
    async fn create_folder(&self, parent: &str, name: &str) -> Result<SynoFileInfo, SynoFsError> {
        let _ = (parent, name);
        Err(SynoFsError::NotSupported)
    }

    /// Rename within the same directory — `new_name` is a bare name, not a path.
    async fn rename(&self, old_path: &str, new_name: &str) -> Result<SynoFileInfo, SynoFsError> {
        let _ = (old_path, new_name);
        Err(SynoFsError::NotSupported)
    }

    /// Remove a file or an empty directory.
    async fn delete(&self, path: &str) -> Result<(), SynoFsError> {
        let _ = path;
        Err(SynoFsError::NotSupported)
    }

    /// Set the file's length, discarding anything past `size` or extending it
    /// with zeroes.
    ///
    /// A metadata operation rather than a transfer: on a protocol that can
    /// express it, the bytes being dropped are never read and never rewritten.
    /// The HTTP API cannot express it at all, which is why the fallback in
    /// [`SynologyClient::truncate`](crate::SynologyClient::truncate) has to
    /// move the file's contents to change one number.
    async fn truncate(&self, path: &str, size: u64) -> Result<(), SynoFsError> {
        let _ = (path, size);
        Err(SynoFsError::NotSupported)
    }

    /// Move `from` to the full path `to` — another directory, another name, or
    /// both — without the bytes leaving the server. A file already at `to` is
    /// replaced, as `rename(2)` replaces it.
    ///
    /// Declining is the default and the right answer for anything that would
    /// have to copy: [`SynologyClient::move_to`](crate::SynologyClient::move_to)
    /// then asks FileStation, which moves on the appliance too.
    async fn move_to(&self, from: &str, to: &str) -> Result<(), SynoFsError> {
        let _ = (from, to);
        Err(SynoFsError::NotSupported)
    }
}

/// What opening a file for writing may create, and what it promises about a
/// name that is already taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOpen {
    /// The file must not exist. An existing name is
    /// [`SynoFsError::AlreadyExists`], never a silent replacement.
    CreateNew,
    /// Open what is there and keep it, creating the file only if it is absent.
    /// Writes land at the offsets given; bytes outside them are untouched.
    Existing,
}

/// A backend that can write **into an open file at arbitrary offsets**.
///
/// This is the difference between a mount that spools a 6 GB copy to a local
/// spill file and ships it on `close(2)`, and one where each `write(2)` goes to
/// the server as it arrives — bounded memory, back-pressure at network speed,
/// and a write that fails when it fails rather than minutes later at close.
///
/// The HTTP FileStation API cannot do this at all: its upload takes a whole
/// file, so the buffering is not a design choice there but a consequence. A
/// backend that can address offsets implements this; the default declines and
/// the caller keeps buffering.
#[async_trait]
pub trait OpenWriteTransport: Send + Sync {
    async fn open_write(
        &self,
        path: &str,
        mode: WriteOpen,
    ) -> Result<Box<dyn WriteHandle>, SynoFsError> {
        let _ = (path, mode);
        Err(SynoFsError::NotSupported)
    }
}

/// An open file being written. Dropping one without [`close`](Self::close)
/// abandons whatever the server already has, so callers must close and check.
///
/// `Send` but not `Sync`: a handle is owned by whoever is writing to it, one
/// task at a time. Requiring `Sync` would rule out backends whose writer holds
/// an in-flight request — which is most of them.
#[async_trait]
pub trait WriteHandle: Send {
    /// Place `data` at `offset`. Offsets need not be contiguous; a backend that
    /// streams may pay a reopen when they jump.
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), SynoFsError>;

    /// Finish. The server holds every byte written when this returns `Ok`.
    async fn close(&mut self) -> Result<(), SynoFsError>;
}

/// Tuning for a backend's [`CircuitBreaker`].
#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Consecutive transport failures that trip the breaker open.
    pub failure_threshold: u32,
    /// How long the breaker stays open before allowing a single re-probe.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    /// Open after 2 consecutive transport failures; re-probe after 30 s. Sized
    /// so that walking off the SMB network trips the backend quickly (so we
    /// don't pay its connect timeout on every file) yet recovers on its own
    /// once we're back.
    fn default() -> Self {
        Self {
            failure_threshold: 2,
            cooldown: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum State {
    /// Backend is trusted — attempts are allowed.
    Closed,
    /// Backend is failing — attempts are skipped until `opened_at + cooldown`.
    Open { opened_at: Instant },
    /// Cooldown elapsed — one trial attempt is out; further attempts wait for
    /// its verdict.
    HalfOpen,
}

/// Per-backend health tracker. Skips a failing backend (so we don't pay its
/// connect timeout on every operation) and periodically re-probes to recover.
///
/// Time is passed in explicitly (`now: Instant`) rather than read from the clock
/// so the state machine is deterministically testable.
#[derive(Debug)]
pub struct CircuitBreaker {
    config: BreakerConfig,
    state: State,
    consecutive_failures: u32,
}

impl CircuitBreaker {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            state: State::Closed,
            consecutive_failures: 0,
        }
    }

    /// Whether a backend attempt is allowed at `now`. When the cooldown has
    /// elapsed this transitions `Open → HalfOpen` and allows exactly one trial.
    pub fn allows(&mut self, now: Instant) -> bool {
        match self.state {
            State::Closed => true,
            State::Open { opened_at } => {
                if now.duration_since(opened_at) >= self.config.cooldown {
                    self.state = State::HalfOpen;
                    true
                } else {
                    false
                }
            }
            State::HalfOpen => false,
        }
    }

    /// Record a healthy response (bytes, or a definitive error the backend
    /// answered): the backend is trusted again.
    pub fn on_success(&mut self) {
        self.state = State::Closed;
        self.consecutive_failures = 0;
    }

    /// Record a transport failure. A half-open trial that fails re-opens the
    /// breaker immediately; otherwise failures accumulate toward the threshold.
    pub fn on_failure(&mut self, now: Instant) {
        match self.state {
            State::HalfOpen => {
                self.state = State::Open { opened_at: now };
            }
            _ => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.state = State::Open { opened_at: now };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new(BreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(30),
        })
    }

    #[test]
    fn starts_closed_and_allows() {
        let mut b = breaker();
        assert!(b.allows(Instant::now()));
    }

    #[test]
    fn opens_only_after_threshold_consecutive_failures() {
        let t = Instant::now();
        let mut b = breaker();
        b.on_failure(t);
        assert!(b.allows(t), "1 failure < threshold(2): still closed");
        b.on_failure(t);
        assert!(!b.allows(t), "2 failures: open, attempts skipped");
    }

    #[test]
    fn success_resets_the_failure_count() {
        let t = Instant::now();
        let mut b = breaker();
        b.on_failure(t);
        b.on_success(); // reachable again
        b.on_failure(t);
        assert!(
            b.allows(t),
            "count reset, so a lone later failure stays closed"
        );
    }

    #[test]
    fn stays_open_during_cooldown_then_half_opens_for_one_probe() {
        let t = Instant::now();
        let mut b = breaker();
        b.on_failure(t);
        b.on_failure(t); // open at t
        assert!(!b.allows(t + Duration::from_secs(10)), "still cooling");
        assert!(!b.allows(t + Duration::from_secs(29)), "still cooling");
        assert!(
            b.allows(t + Duration::from_secs(30)),
            "cooldown elapsed → half-open, one probe allowed"
        );
        assert!(
            !b.allows(t + Duration::from_secs(30)),
            "half-open: only a single trial is in flight"
        );
    }

    #[test]
    fn half_open_success_closes() {
        let t = Instant::now();
        let mut b = breaker();
        b.on_failure(t);
        b.on_failure(t);
        assert!(b.allows(t + Duration::from_secs(30))); // half-open probe
        b.on_success();
        assert!(b.allows(t + Duration::from_secs(31)), "closed again");
    }

    #[test]
    fn half_open_failure_reopens_for_another_cooldown() {
        let t = Instant::now();
        let mut b = breaker();
        b.on_failure(t);
        b.on_failure(t);
        let probe = t + Duration::from_secs(30);
        assert!(b.allows(probe)); // half-open probe
        b.on_failure(probe); // probe failed → reopen at `probe`
        assert!(!b.allows(probe + Duration::from_secs(10)), "cooling again");
        assert!(
            b.allows(probe + Duration::from_secs(30)),
            "half-opens again after another full cooldown"
        );
    }
}

/// What is at a move's destination, as far as the move needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Absent,
    File,
    Dir { empty: bool },
}

/// What a move may replace at its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovePlan {
    /// Nothing is there; a name that turns up meanwhile must make the move
    /// fail rather than be overwritten.
    Fresh,
    /// A file replaces a file.
    ReplaceFile,
    /// A folder replaces an empty folder, which goes first.
    ReplaceEmptyDir,
}

/// `rename(2)`'s rule for what a move may replace, shared by every way of
/// moving so none of them can disagree.
///
/// FileStation's CopyMove with `overwrite` would merge a folder into one
/// already there and overwrite what both held; SMB's replacing rename would
/// refuse a folder outright. Neither is what a mount promises. A folder only
/// replaces an empty folder, a non-empty one is [`SynoFsError::NotEmpty`],
/// and a file and a folder never replace each other.
pub fn plan_move(source_is_dir: bool, dest: Destination) -> Result<MovePlan, SynoFsError> {
    match (source_is_dir, dest) {
        (_, Destination::Absent) => Ok(MovePlan::Fresh),
        (false, Destination::File) => Ok(MovePlan::ReplaceFile),
        (true, Destination::Dir { empty: true }) => Ok(MovePlan::ReplaceEmptyDir),
        (true, Destination::Dir { empty: false }) => Err(SynoFsError::NotEmpty),
        // EISDIR and ENOTDIR, which the kernel answers itself before a FUSE
        // mount is asked; anyone else calling is told the name is taken.
        (false, Destination::Dir { .. }) | (true, Destination::File) => {
            Err(SynoFsError::AlreadyExists)
        }
    }
}
