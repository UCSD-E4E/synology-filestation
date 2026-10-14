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

/// How long to wait after the first failure to open an extra connection.
/// A network that will not carry one more connection now will rarely carry it
/// a moment later, and every attempt is a dial and a login.
const FIRST_BACKOFF: Duration = Duration::from_secs(30);

/// The longest wait between attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Failures in a row after which no more attempts are made until the session
/// changes. A NAS that caps sessions per account, or a path that times every
/// extra connection out, would otherwise cost a login every few minutes for
/// the life of the mount, for nothing.
const MAX_FAILURES: u32 = 6;

/// How long to wait after `failures` failed attempts in a row, or `None` once
/// it is time to stop asking.
fn backoff(failures: u32) -> Option<Duration> {
    match failures {
        0 => Some(Duration::ZERO),
        n if n >= MAX_FAILURES => None,
        n => Some(FIRST_BACKOFF.saturating_mul(1 << (n - 1)).min(MAX_BACKOFF)),
    }
}

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
    /// Failed attempts in a row, and when the last one was.
    failures: Mutex<(u32, Option<Instant>)>,
    /// Connections let go of whose handles are still being closed. Kept
    /// where invalidation can find them: a read in flight on one still holds
    /// a file open, and a rename meeting it fails.
    draining: Mutex<Vec<Arc<Lane>>>,
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
            failures: Mutex::new((0, None)),
            draining: Mutex::new(Vec::new()),
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

    /// How many extra connections are up now, without copying the list.
    pub fn live_count(&self) -> usize {
        self.lock().len()
    }

    /// Whether more connections are wanted and it is time to try for them.
    /// Claims the right to: a `true` must be followed by [`opened`] when the
    /// attempt is over.
    ///
    /// [`opened`]: Self::opened
    pub fn should_open(&self) -> Option<(Opener, u64)> {
        self.should_open_at(Instant::now())
    }

    /// [`should_open`](Self::should_open), as if it were `now`.
    pub fn should_open_at(&self, now: Instant) -> Option<(Opener, u64)> {
        let opener = self.opener.clone()?;
        if self.lock().len() >= self.wanted {
            return None;
        }
        let (failures, last) = *self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let wait = backoff(failures)?;
        if last.is_some_and(|at| now < at + wait) {
            return None;
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

    /// The attempt to open connections is over. A failure puts off the next,
    /// for longer each time, and enough in a row stop them; a success
    /// forgets the failures before it.
    pub fn opened(&self, failed: bool) {
        {
            let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
            *failures = if failed {
                (failures.0.saturating_add(1), Some(Instant::now()))
            } else {
                (0, None)
            };
        }
        self.opening.store(false, Ordering::SeqCst);
    }

    /// Stop reading over `lane`. Returns whether it was live, in which case
    /// it is now draining and the caller closes its handles.
    pub fn retire(&self, lane: &Arc<Lane>) -> bool {
        let mut live = self.lock();
        let before = live.len();
        live.retain(|l| !Arc::ptr_eq(l, lane));
        let now = live.len();
        drop(live);
        if now == before {
            return false;
        }
        self.drain(lane.clone());
        self.report(now);
        true
    }

    fn draining(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Lane>>> {
        self.draining.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn drain(&self, lane: Arc<Lane>) {
        self.draining().push(lane);
    }

    /// Every connection whose handles invalidation has to reach: the live
    /// ones and the ones still draining.
    pub fn all(&self) -> Vec<Arc<Lane>> {
        let mut all = self.live();
        all.extend(self.draining().iter().cloned());
        all
    }

    /// A draining connection has closed everything it had open.
    pub fn drained(&self, lane: &Arc<Lane>) {
        self.draining().retain(|l| !Arc::ptr_eq(l, lane));
    }

    /// Let go of every extra connection, returning them — now draining — so
    /// their handles can be closed, and disown any being opened now. A new
    /// session is a new chance, so past failures are forgotten.
    pub fn retire_all(&self) -> Vec<Arc<Lane>> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.failures.lock().unwrap_or_else(|e| e.into_inner()) = (0, None);
        let gone = std::mem::take(&mut *self.lock());
        for lane in &gone {
            self.drain(lane.clone());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn opener() -> Opener {
        Arc::new(|| Box::pin(async { Err(SynoFsError::Io("not in this test".into())) }))
    }

    /// Each failure doubles the wait, up to a ceiling, and after enough of
    /// them there is no next attempt: a NAS that caps sessions per account,
    /// or a path that times every extra connection out, would otherwise cost
    /// a dial and a login every half minute for the life of the mount.
    #[test]
    fn failing_to_open_backs_off_and_then_stops() {
        assert_eq!(backoff(0), Some(Duration::ZERO));
        assert_eq!(backoff(1), Some(Duration::from_secs(30)));
        assert_eq!(backoff(2), Some(Duration::from_secs(60)));
        assert_eq!(backoff(3), Some(Duration::from_secs(120)));
        assert!(backoff(MAX_FAILURES - 1).is_some());
        assert_eq!(backoff(MAX_FAILURES), None, "given up");
        for n in 1..MAX_FAILURES {
            assert!(backoff(n).unwrap() <= MAX_BACKOFF);
        }
    }

    #[test]
    fn a_pool_that_has_given_up_asks_again_once_its_session_changes() {
        let pool = Pool::with(3, Some(opener()));
        for _ in 0..MAX_FAILURES {
            let claimed = pool.should_open_at(Instant::now() + Duration::from_secs(86_400));
            assert!(claimed.is_some(), "still trying");
            pool.opened(true);
        }
        assert!(
            pool.should_open_at(Instant::now() + Duration::from_secs(86_400))
                .is_none(),
            "given up, however long it waits"
        );

        drop(pool.retire_all());
        assert!(
            pool.should_open().is_some(),
            "a new session is a new chance"
        );
    }

    #[test]
    fn an_attempt_that_works_forgets_the_failures_before_it() {
        let pool = Pool::with(3, Some(opener()));
        let far = Instant::now() + Duration::from_secs(86_400);
        for _ in 0..MAX_FAILURES - 1 {
            pool.should_open_at(far).expect("trying");
            pool.opened(true);
        }
        pool.should_open_at(far).expect("trying");
        pool.opened(false);
        for _ in 0..MAX_FAILURES - 1 {
            pool.should_open_at(far).expect("the count started again");
            pool.opened(true);
        }
    }
}
