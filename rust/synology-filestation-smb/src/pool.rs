//! Reading over more than one connection.
//!
//! Loss on the path caps each TCP connection, not the link. Measured from a
//! home uplink that dropped about 3% of what the NAS sent: one connection ran
//! at 1.7 MB/s, three at once at 5.1, and the same loss showed on every route
//! tried, plain HTTPS included. The NAS's kernel (4.4, CUBIC, no loadable
//! modules) cannot be given a congestion control that shrugs loss off, so the
//! one lever left is more connections.
//!
//! So a transport can open extra sessions that serve only block reads. A read
//! is cut at [`STRIPE`] boundaries and the pieces are dealt round robin across
//! the main session and the extra ones, so consecutive blocks of a sequential
//! reader travel on different connections. Everything else — writes,
//! listings, renames — stays on the main session, which keeps what has to be
//! invalidated when a file changes down to each connection's read handles.
//!
//! Extra connections are opened in the background, never inside a read: a
//! read that waited on a login would be slower than one that never asked.
//! They are opened only while the main session is live, which is to say only
//! once its login has been accepted — each one is a login of its own, and a
//! login nobody has seen work is a possible strike towards DSM's auto-block.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smb2::{FileReader, SmbClient, Tree};
use synology_filestation_core::SynoFsError;

use crate::handles::{HandleCache, MAX_CACHED_HANDLES};
use crate::transport::BoxedStream;

/// How much of a read goes to one connection before the next takes over.
///
/// The mount's block size, so a reader asking for one block at a time — which
/// is what the kernel does without read-ahead — still spreads across them.
pub(crate) const STRIPE: u64 = 256 * 1024;

/// How long to wait before trying again after an extra connection could not
/// be opened. A network that will not carry one more connection now will
/// rarely carry it a moment later, and every attempt is a dial and a login.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// What an opener returns: a stream to the server, and the name the server
/// goes by on it — the public name directly, an address inside a tunnel.
pub type ConnectionFuture =
    Pin<Box<dyn Future<Output = Result<(BoxedStream, String), SynoFsError>> + Send + 'static>>;

/// How to open one more stream to the server.
pub(crate) type Opener = Arc<dyn Fn() -> ConnectionFuture + Send + Sync>;

/// One piece of a read, and which connection reads it: `0` is the main
/// session, `n` the n-th extra one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Piece {
    pub offset: u64,
    pub length: u64,
    pub connection: usize,
}

/// Cut a read into pieces at stripe boundaries and deal them across
/// `connections`, by stripe number, so the same block always goes to the same
/// connection however the read that asked for it was cut.
pub(crate) fn stripes(offset: u64, length: u64, connections: usize) -> Vec<Piece> {
    let connections = connections.max(1);
    if connections == 1 {
        return vec![Piece {
            offset,
            length,
            connection: 0,
        }];
    }
    let end = offset + length;
    let mut pieces = Vec::new();
    let mut at = offset;
    while at < end {
        let stripe = at / STRIPE;
        let next = ((stripe + 1) * STRIPE).min(end);
        pieces.push(Piece {
            offset: at,
            length: next - at,
            connection: (stripe % connections as u64) as usize,
        });
        at = next;
    }
    pieces
}

/// An extra session, serving block reads only.
pub(crate) struct Lane {
    /// The session, and one attached tree per share, connected on first use.
    /// Locked only to attach a share, never across a read.
    pub session: tokio::sync::Mutex<(SmbClient, HashMap<String, Tree>)>,
    /// Its own open read handles: a handle belongs to the session that opened
    /// it.
    pub handles: HandleCache<FileReader>,
}

impl Lane {
    pub fn new(client: SmbClient) -> Self {
        Self {
            session: tokio::sync::Mutex::new((client, HashMap::new())),
            handles: HandleCache::new(MAX_CACHED_HANDLES),
        }
    }
}

/// The extra connections a transport reads over, and how to open more.
pub(crate) struct Pool {
    /// How many extra connections are wanted; `0` is reading on the main
    /// session alone.
    wanted: usize,
    opener: Option<Opener>,
    live: Mutex<Vec<Arc<Lane>>>,
    /// Whether a task is opening connections now. One at a time: they are
    /// logins, and a burst of them is how a changed password costs several
    /// strikes instead of one.
    opening: AtomicBool,
    /// Bumped whenever the connections are let go of — the main session moved
    /// to another leg, or was rebuilt — so one opened for the old session is
    /// not kept for the new.
    generation: AtomicU64,
    /// Not before this, after an attempt that failed.
    retry_at: Mutex<Option<Instant>>,
    /// The count last logged, so a change is said once.
    reported: AtomicUsize,
}

impl Pool {
    pub fn off() -> Self {
        Self::with(0, None)
    }

    pub fn with(wanted: usize, opener: Option<Opener>) -> Self {
        Self {
            wanted,
            opener,
            live: Mutex::new(Vec::new()),
            opening: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            retry_at: Mutex::new(None),
            reported: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Lane>>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The extra connections up now.
    pub fn live(&self) -> Vec<Arc<Lane>> {
        self.lock().clone()
    }

    /// Whether more connections are wanted and it is time to try for them.
    /// Claims the right to: a `true` must be followed by [`opened`] when the
    /// attempt is over.
    ///
    /// [`opened`]: Self::opened
    pub fn should_open(&self) -> Option<(Opener, u64)> {
        let opener = self.opener.clone()?;
        if self.lock().len() >= self.wanted {
            return None;
        }
        if let Some(at) = *self.retry_at.lock().unwrap_or_else(|e| e.into_inner()) {
            if Instant::now() < at {
                return None;
            }
        }
        if self.opening.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some((opener, self.generation.load(Ordering::SeqCst)))
    }

    /// Whether a connection opened for `generation` is still wanted.
    pub fn wants(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation && self.lock().len() < self.wanted
    }

    /// Keep a connection opened for `generation`, unless the pool has moved
    /// on since. Returns whether it was kept.
    pub fn add(&self, lane: Lane, generation: u64) -> bool {
        let mut live = self.lock();
        if self.generation.load(Ordering::SeqCst) != generation || live.len() >= self.wanted {
            return false;
        }
        live.push(Arc::new(lane));
        let now = live.len();
        drop(live);
        self.report(now);
        true
    }

    /// The attempt to open connections is over; `failed` puts off the next.
    pub fn opened(&self, failed: bool) {
        if failed {
            *self.retry_at.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + RETRY_AFTER);
        }
        self.opening.store(false, Ordering::SeqCst);
    }

    /// Stop reading over `lane`: its connection has died.
    pub fn retire(&self, lane: &Arc<Lane>) {
        let mut live = self.lock();
        let before = live.len();
        live.retain(|l| !Arc::ptr_eq(l, lane));
        let now = live.len();
        drop(live);
        if now != before {
            self.report(now);
        }
    }

    /// Let go of every extra connection, returning them so their handles can
    /// be dealt with, and disown any being opened now.
    pub fn retire_all(&self) -> Vec<Arc<Lane>> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let gone = std::mem::take(&mut *self.lock());
        if !gone.is_empty() {
            self.report(0);
        }
        gone
    }

    fn report(&self, extra: usize) {
        let was = self.reported.swap(extra, Ordering::SeqCst);
        if was != extra {
            tracing::info!("SMB: reading over {} connections", extra + 1);
        }
    }
}
