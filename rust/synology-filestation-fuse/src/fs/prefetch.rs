//! What to fetch before the caller asks for it, and when not to bother.
//!
//! The mount speculates twice: once at `open`, to make a video file playable
//! at all, and once per `read`, to keep a streaming reader's pipe full. Both
//! used to be unconditional, which meant a 48 KiB header read of a 5.5 MiB
//! JPEG pulled ~5 MiB off the appliance and threw away all but the header.
//!
//! The two windows are not symmetric, and that asymmetry is the whole design:
//!
//! * The **head** is reachable by watching the access pattern. A player
//!   parsing a faststart MP4's `moov` reads contiguously from block 0, so a
//!   ramp that grows on sequential reads gets there on its own.
//! * The **tail** is reachable by nothing. A seek to EOF — which is how a
//!   player finds the `moov` of a file written without faststart, i.e. most
//!   camera and plain `ffmpeg` output — is by definition not sequential, so
//!   no access-pattern heuristic can ever predict it. It has to be fetched
//!   eagerly or not at all.
//!
//! So the tail window is decided at open time from what the file *is*, not
//! from what the caller has done so far, and block 0 is already in hand at
//! exactly that point (`open` downloads it synchronously) — which makes
//! sniffing the container magic free.

use std::sync::Arc;

use crate::cache::ReadCache;

/// Blocks of speculative read-ahead, at open and per sequential read. The
/// default lives in the crate root, because `MountOptions` is cross-platform
/// and this module is Linux-only.
#[cfg(test)]
use crate::DEFAULT_PREFETCH_BLOCKS;

/// How much of a media container's tail to fetch eagerly. Enough for the
/// `moov` of a typical recording; the index of a very long one will still
/// take on-demand reads.
pub(super) const TAIL_BLOCKS: u64 = 4;

/// Where a sequential run's window starts before it begins doubling.
const RAMP_START: u64 = 2;

/// Most blocks to ask for in a single ranged read.
///
/// Contiguous blocks are fetched as one request rather than one request each:
/// the window is the same bytes either way, but nineteen round trips become
/// two. Over SMB — where a read is a round trip on a handle rather than a
/// fresh HTTP request — that is most of the cost of the window.
pub(super) const MAX_PREFETCH_SPAN: usize = 16;

/// Does block 0 look like a container that keeps its index at the end?
///
/// Only containers that actually need the tail qualify. A JPEG or a raw file
/// keeps everything a reader wants up front, so fetching its last four blocks
/// buys nothing — and for the small files a photo corpus is made of, those
/// four blocks are a large fraction of the whole file.
pub(super) fn is_indexed_media(head: &[u8]) -> bool {
    // ISO base media (MP4/M4V/MOV) and bare QuickTime atoms: a four-byte box
    // length, then the box type.
    if head.len() >= 8 {
        let atom = &head[4..8];
        if matches!(atom, b"ftyp" | b"moov" | b"mdat" | b"wide" | b"skip") {
            return true;
        }
    }
    // Matroska / WebM: the EBML header magic.
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return true;
    }
    // AVI, which is a RIFF with an `idx1` at the end. Other RIFF payloads
    // (WAVE, for one) have no trailing index and must not qualify.
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"AVI " {
        return true;
    }
    // ASF / WMV: the header-object GUID. The index lives at the end.
    const ASF: [u8; 16] = [
        0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE,
        0x6C,
    ];
    head.starts_with(&ASF)
}

/// Blocks to fetch eagerly at open, **excluding** block 0 (the caller has
/// already fetched that one synchronously).
///
/// For media this is the window as it has always been: the head up to `depth`,
/// plus the last [`TAIL_BLOCKS`]. For everything else it is empty — block 0
/// plus [`ReadAhead`]'s ramp is enough, and skipping the rest is what takes a
/// header scan from twenty blocks to one.
pub(super) fn open_window(total_blocks: u64, depth: u64, media: bool) -> Vec<u64> {
    if !media || depth == 0 || total_blocks == 0 {
        return Vec::new();
    }
    let head_end = depth.min(total_blocks);
    let mut blocks: Vec<u64> = (1..head_end).collect();
    // Starting the tail at `head_end` at the earliest is what stops a file
    // only a little longer than the window from listing a block twice.
    if total_blocks > head_end {
        let tail_start = total_blocks.saturating_sub(TAIL_BLOCKS).max(head_end);
        blocks.extend(tail_start..total_blocks);
    }
    blocks
}

/// Per-handle sequential-read tracking, and the ramp that rides on it.
#[derive(Debug, Default)]
pub(super) struct ReadAhead {
    /// Offset a read would have to start at to continue the last one.
    next_offset: Option<u64>,
    /// Current window, in blocks. Zero until a run is established.
    window: u64,
}

impl ReadAhead {
    /// Record a read and return the blocks worth fetching behind it.
    ///
    /// A first read never triggers read-ahead: one read at offset 0 followed
    /// by a close is the header-scan pattern, and it should cost one block.
    /// A reader that keeps going gets a window that doubles up to `depth`.
    ///
    /// `total_blocks` is `None` when the file's size is not known, in which
    /// case nothing is clamped — the empty-block EOF sentinel still stops it,
    /// at the price of the round trip that finds it.
    pub(super) fn advance(
        &mut self,
        offset: u64,
        size: u64,
        block_size: u64,
        total_blocks: Option<u64>,
        depth: u64,
    ) -> Vec<u64> {
        if size == 0 {
            return Vec::new();
        }
        let sequential = self.next_offset == Some(offset);
        self.next_offset = Some(offset + size);

        if !sequential || depth == 0 {
            self.window = 0;
            return Vec::new();
        }
        self.window = if self.window == 0 {
            RAMP_START.min(depth)
        } else {
            (self.window * 2).min(depth)
        };

        let last_block = (offset + size - 1) / block_size;
        let mut blocks = Vec::with_capacity(self.window as usize);
        for idx in (last_block + 1)..=(last_block + self.window) {
            if total_blocks.is_some_and(|t| idx >= t) {
                break;
            }
            blocks.push(idx);
        }
        blocks
    }
}

/// How many speculative **blocks** may be on the wire at once, mount-wide.
///
/// Deliberately its own limit rather than sharing `MAX_CONCURRENT_TRANSFERS`:
/// it has to be wide enough to hold one file's entire open window, or a video
/// open serialises into waves and the case the window exists for gets slower.
/// At the default depth that window is `depth - 1` head blocks plus
/// [`TAIL_BLOCKS`], so the budget is comfortably above it and below twice it.
/// Eight parallel opens used to mean ~160 simultaneous requests against
/// `synoscgi`, the shared CGI backend the whole appliance runs on.
///
/// Per connection reads go over: four SMB connections get four times this,
/// since on a lossy link each needs its share in flight to be worth having —
/// measured off campus, four connections ran at 2.8 MB/s on one connection's
/// budget and 4.4 on four. On HTTP it stays this.
///
/// Counted in blocks rather than in requests, which is what it used to be.
/// That was the same thing when a speculative task fetched one block, but
/// coalescing made a task a run of up to [`MAX_PREFETCH_SPAN`] blocks and
/// silently multiplied what the number permitted by sixteen. The caller's own
/// block then queued behind all of it, which over a link that reaches the NAS
/// through a VPN is enough to spend a minute waiting on one 256 KiB read.
///
/// A run never exceeds [`MAX_PREFETCH_SPAN`], so a request can always be
/// satisfied by an empty budget and no run can wait on permits it will never
/// get.
pub(super) const MAX_INFLIGHT_PREFETCH_BLOCKS: usize = 32;

/// The speculative blocks on the wire, mount-wide, claimed against a limit the
/// caller names each time: [`MAX_INFLIGHT_PREFETCH_BLOCKS`] for each
/// connection reads go over now.
///
/// A counter, not a semaphore, because the limit moves with the connections.
/// It used to be a semaphore sized for 64 connections plus a separate check
/// of how many permits were out, and a check and a claim in two steps let
/// every thread that checked before any claimed go past the limit together.
/// Here the claim *is* the check.
#[derive(Default)]
pub(super) struct Budget {
    in_flight: std::sync::atomic::AtomicUsize,
}

/// Blocks claimed from a [`Budget`], given back when this is dropped — so an
/// aborted download returns its share with its claims.
pub(super) struct Claim {
    budget: std::sync::Arc<Budget>,
    blocks: usize,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Claim `blocks`, if that keeps what is in flight within `limit`.
    pub(super) fn try_claim(
        self: &std::sync::Arc<Self>,
        blocks: usize,
        limit: usize,
    ) -> Option<Claim> {
        use std::sync::atomic::Ordering::SeqCst;
        self.in_flight
            .fetch_update(SeqCst, SeqCst, |now| {
                now.checked_add(blocks).filter(|&after| after <= limit)
            })
            .ok()?;
        Some(Claim {
            budget: self.clone(),
            blocks,
        })
    }

    #[cfg(test)]
    pub(super) fn in_flight(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.budget
            .in_flight
            .fetch_sub(self.blocks, std::sync::atomic::Ordering::SeqCst);
    }
}

/// How far a streaming reader may read ahead: `depth` blocks for each of the
/// `connections` reads go over, but never more than half of a read cache of
/// `cache_blocks` — a window wider than the cache evicts its own blocks before
/// the reader reaches them. The clamp never cuts below `depth` itself, which
/// is what the caller asked for.
pub(super) fn streaming_depth(depth: u64, connections: usize, cache_blocks: u64) -> u64 {
    depth
        .saturating_mul(connections as u64)
        .min(depth.max(cache_blocks / 2))
}

/// Releases a block's in-flight claim unless the download published it.
///
/// A prefetch task can be aborted mid-download now that closing a file
/// abandons its speculation. Aborting drops the future at an await point and
/// runs no error path, so without this the claim would stay set and the next
/// reader of that block would wait out `BLOCK_WAIT_TIMEOUT` before
/// re-downloading — a minute to recover from a cancellation whose whole point
/// was to make things faster.
pub(super) struct InflightGuard {
    read_cache: Arc<ReadCache>,
    ino: u64,
    block_idx: u64,
    armed: bool,
}

impl InflightGuard {
    pub(super) fn new(read_cache: Arc<ReadCache>, ino: u64, block_idx: u64) -> Self {
        Self {
            read_cache,
            ino,
            block_idx,
            armed: true,
        }
    }

    /// The block reached the cache, and `insert` cleared the claim with it.
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.armed {
            self.read_cache.cancel_inflight(self.ino, self.block_idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The in-flight budget ─────────────────────────────────────────────────

    #[test]
    fn a_claim_within_the_limit_is_granted_and_given_back_when_dropped() {
        let budget = std::sync::Arc::new(Budget::new());
        let a = budget.try_claim(20, 32).expect("20 of 32");
        assert!(budget.try_claim(13, 32).is_none(), "33 would be over");
        let b = budget.try_claim(12, 32).expect("exactly 32");
        drop(a);
        assert!(budget.try_claim(20, 32).is_some(), "20 came back");
        drop(b);
        assert_eq!(budget.in_flight(), 0);
    }

    /// The limit is asked of the claim, not fixed at construction, so it can
    /// follow the number of connections reads go over.
    #[test]
    fn the_limit_is_the_one_asked_for_now() {
        let budget = std::sync::Arc::new(Budget::new());
        let _held = budget.try_claim(32, 128).expect("four connections' worth");
        assert!(
            budget.try_claim(1, 32).is_none(),
            "down to one connection: nothing more until it drains"
        );
    }

    /// Regression: the check and the claim were two steps, so every thread
    /// that read the count before any claimed went past the limit together.
    #[test]
    fn claims_racing_from_many_threads_never_exceed_the_limit() {
        let budget = std::sync::Arc::new(Budget::new());
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let budget = budget.clone();
                let peak = peak.clone();
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        if let Some(claim) = budget.try_claim(16, 32) {
                            peak.fetch_max(budget.in_flight(), std::sync::atomic::Ordering::SeqCst);
                            drop(claim);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("ran");
        }
        assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 32);
        assert_eq!(budget.in_flight(), 0);
    }

    // ── How far a streaming reader reads ahead ──────────────────────────────

    #[test]
    fn the_depth_is_per_connection() {
        assert_eq!(streaming_depth(16, 1, 1024), 16);
        assert_eq!(streaming_depth(16, 4, 1024), 64);
    }

    /// Regression: a window wider than the read cache evicts its own blocks
    /// before the reader gets to them. Half the cache is the most it takes.
    #[test]
    fn the_depth_never_outgrows_half_the_read_cache() {
        assert_eq!(streaming_depth(256, 8, 1024), 512);
        assert_eq!(streaming_depth(64, 8, 256), 128);
    }

    /// What the caller asked for, though, is not cut: a depth set above half
    /// a small cache on one connection is a choice already made.
    #[test]
    fn the_clamp_never_cuts_below_the_depth_asked_for() {
        assert_eq!(streaming_depth(64, 4, 64), 64);
    }

    #[test]
    fn an_absurd_depth_or_count_saturates_rather_than_overflowing() {
        assert_eq!(streaming_depth(u64::MAX, usize::MAX, 1024), u64::MAX);
        assert_eq!(streaming_depth(0, 8, 1024), 0, "off stays off");
    }

    // ── container sniffing ────────────────────────────────────────────────

    fn mp4() -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0x18];
        v.extend_from_slice(b"ftypisom");
        v
    }

    #[test]
    fn an_mp4_is_indexed_media() {
        assert!(is_indexed_media(&mp4()));
    }

    #[test]
    fn a_bare_quicktime_atom_is_indexed_media() {
        let mut v = vec![0, 0, 0, 0x08];
        v.extend_from_slice(b"mdat");
        assert!(is_indexed_media(&v));
    }

    #[test]
    fn a_matroska_file_is_indexed_media() {
        assert!(is_indexed_media(&[0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x02]));
    }

    #[test]
    fn an_avi_is_indexed_media() {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"AVI ");
        assert!(is_indexed_media(&v));
    }

    /// The case the whole change exists for: a photo corpus must not pay for
    /// a tail it will never read.
    #[test]
    fn a_jpeg_is_not_indexed_media() {
        assert!(!is_indexed_media(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]));
    }

    #[test]
    fn a_tiff_raw_is_not_indexed_media() {
        assert!(!is_indexed_media(b"II\x2a\x00\x08\x00\x00\x00"));
    }

    /// A RIFF container that is not AVI (a WAV, say) has no trailing index.
    #[test]
    fn a_non_avi_riff_is_not_indexed_media() {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"WAVE");
        assert!(!is_indexed_media(&v));
    }

    #[test]
    fn a_truncated_head_is_not_indexed_media() {
        assert!(!is_indexed_media(b"ft"));
        assert!(!is_indexed_media(&[]));
    }

    // ── the open window ───────────────────────────────────────────────────

    /// Video keeps exactly the window it has today: blocks 1..=15 of the head
    /// and the last four. This is the behaviour the prefetch was written for
    /// and the one regression that would make the mount unusable again.
    #[test]
    fn media_keeps_the_head_and_the_tail() {
        let w = open_window(22, DEFAULT_PREFETCH_BLOCKS, true);
        assert_eq!(
            w,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 18, 19, 20, 21]
        );
    }

    /// A 5.5 MiB JPEG is 22 blocks and used to cost 20 of them at open.
    #[test]
    fn a_non_media_file_gets_no_open_window() {
        assert!(open_window(22, DEFAULT_PREFETCH_BLOCKS, false).is_empty());
    }

    /// The head is clamped by the file, and a file that fits inside the head
    /// has no separate tail to fetch.
    #[test]
    fn a_short_media_file_has_no_separate_tail() {
        assert_eq!(open_window(3, DEFAULT_PREFETCH_BLOCKS, true), vec![1, 2]);
    }

    /// Blocks past EOF are never requested — they used to be, and each one
    /// was a round trip that could only ever come back empty.
    #[test]
    fn the_open_window_never_reaches_past_eof() {
        for total in 0..40u64 {
            for media in [true, false] {
                assert!(
                    open_window(total, DEFAULT_PREFETCH_BLOCKS, media)
                        .iter()
                        .all(|&b| b < total),
                    "total={total} media={media}"
                );
            }
        }
    }

    #[test]
    fn the_open_window_never_repeats_a_block() {
        let w = open_window(18, DEFAULT_PREFETCH_BLOCKS, true);
        let mut sorted = w.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), w.len(), "overlapping head and tail: {w:?}");
    }

    #[test]
    fn depth_zero_disables_the_open_window() {
        assert!(open_window(22, 0, true).is_empty());
    }

    // ── the read-ahead ramp ───────────────────────────────────────────────

    const BS: u64 = 1024;

    #[test]
    fn a_first_read_prefetches_nothing() {
        let mut ra = ReadAhead::default();
        assert!(ra
            .advance(0, 48, BS, Some(22), DEFAULT_PREFETCH_BLOCKS)
            .is_empty());
    }

    #[test]
    fn a_contiguous_read_starts_the_ramp() {
        let mut ra = ReadAhead::default();
        ra.advance(0, BS, BS, Some(64), DEFAULT_PREFETCH_BLOCKS);
        assert_eq!(
            ra.advance(BS, BS, BS, Some(64), DEFAULT_PREFETCH_BLOCKS),
            vec![2, 3]
        );
    }

    #[test]
    fn the_ramp_doubles_and_then_holds_at_the_depth() {
        let mut ra = ReadAhead::default();
        let mut widths = Vec::new();
        let mut off = 0;
        for _ in 0..8 {
            let blocks = ra.advance(off, BS, BS, Some(4096), DEFAULT_PREFETCH_BLOCKS);
            widths.push(blocks.len());
            off += BS;
        }
        assert_eq!(widths, vec![0, 2, 4, 8, 16, 16, 16, 16]);
    }

    /// The header-scan pattern in its other form: read, seek somewhere else,
    /// read again. Neither read should drag a window behind it.
    #[test]
    fn a_seek_resets_the_ramp() {
        let mut ra = ReadAhead::default();
        ra.advance(0, BS, BS, Some(64), DEFAULT_PREFETCH_BLOCKS);
        ra.advance(BS, BS, BS, Some(64), DEFAULT_PREFETCH_BLOCKS);
        assert!(
            ra.advance(40 * BS, BS, BS, Some(64), DEFAULT_PREFETCH_BLOCKS)
                .is_empty(),
            "a seek is not a continuation"
        );
    }

    /// After a seek, a run that resumes ramps from the bottom again rather
    /// than from wherever the previous run had grown to.
    #[test]
    fn a_resumed_run_ramps_from_the_bottom() {
        let mut ra = ReadAhead::default();
        for i in 0..5 {
            ra.advance(i * BS, BS, BS, Some(4096), DEFAULT_PREFETCH_BLOCKS);
        }
        ra.advance(900 * BS, BS, BS, Some(4096), DEFAULT_PREFETCH_BLOCKS);
        assert_eq!(
            ra.advance(901 * BS, BS, BS, Some(4096), DEFAULT_PREFETCH_BLOCKS)
                .len(),
            RAMP_START as usize
        );
    }

    #[test]
    fn the_ramp_never_reaches_past_eof() {
        let mut ra = ReadAhead::default();
        let mut off = 0;
        for _ in 0..10 {
            for b in ra.advance(off, BS, BS, Some(12), DEFAULT_PREFETCH_BLOCKS) {
                assert!(b < 12, "block {b} is past a 12-block file");
            }
            off += BS;
        }
    }

    /// Without a known size there is nothing to clamp against, so the ramp
    /// still runs — the EOF sentinel is what stops it.
    #[test]
    fn an_unknown_size_still_ramps() {
        let mut ra = ReadAhead::default();
        ra.advance(0, BS, BS, None, DEFAULT_PREFETCH_BLOCKS);
        assert_eq!(
            ra.advance(BS, BS, BS, None, DEFAULT_PREFETCH_BLOCKS),
            vec![2, 3]
        );
    }

    #[test]
    fn depth_zero_disables_the_ramp() {
        let mut ra = ReadAhead::default();
        ra.advance(0, BS, BS, Some(64), 0);
        assert!(ra.advance(BS, BS, BS, Some(64), 0).is_empty());
    }

    /// A partial final read still counts as a continuation.
    #[test]
    fn a_short_tail_read_still_continues_the_run() {
        let mut ra = ReadAhead::default();
        ra.advance(0, BS, BS, Some(64), DEFAULT_PREFETCH_BLOCKS);
        assert!(!ra
            .advance(BS, BS / 2, BS, Some(64), DEFAULT_PREFETCH_BLOCKS)
            .is_empty());
    }
}
