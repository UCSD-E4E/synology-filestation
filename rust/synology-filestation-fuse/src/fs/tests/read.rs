//! Reads: block assembly out of the read cache, and directory listings.

use super::*;
use crate::fs::prefetch::TAIL_BLOCKS;

// ── T1.4: block assembly ──────────────────────────────────────────────────

/// Regression: the assembly loop only stopped on a *fully empty* block, so a
/// block that came back short was appended and then the next block's bytes
/// were appended directly behind it. The caller received a contiguous-looking
/// buffer whose tail actually came from a different file offset — silent
/// corruption. A short block must end the read.
#[test]
fn a_short_block_ends_the_read_instead_of_fabricating_contiguity() {
    let f = fixture();
    let body = ramp(4096);
    // Block 0 is truncated to 300 bytes. This is *not* EOF: the file really
    // is 4096 bytes, and block 1 would happily serve its full 1024.
    mount_download(&f, body.clone(), Map::from([(0u64, 300usize)]));

    let out = f.fs.read_range(7, "/share/f.bin", 0, 2 * BLOCK).unwrap();

    assert_eq!(
        out.as_slice(),
        &body[..300],
        "a short block must end the read, not be back-filled with block 1's bytes"
    );
}

/// The stop-on-short rule must not break the ordinary case it resembles:
/// a final partial block at genuine EOF still contributes its bytes.
#[test]
fn a_genuinely_short_final_block_still_returns_the_whole_tail() {
    let f = fixture();
    let body = ramp(1500); // one full block + a 476-byte tail
    mount_download(&f, body.clone(), Map::new());

    let out = f.fs.read_range(8, "/share/f.bin", 0, 4 * BLOCK).unwrap();

    assert_eq!(out, body, "the short tail block is EOF, not corruption");
}

/// Reads that start part-way into a block must still line up.
#[test]
fn a_mid_block_offset_read_returns_the_right_slice() {
    let f = fixture();
    let body = ramp(4096);
    mount_download(&f, body.clone(), Map::new());

    let out = f.fs.read_range(11, "/share/f.bin", 1500, 1000).unwrap();

    assert_eq!(out.as_slice(), &body[1500..2500]);
}

// ── Listing cache ─────────────────────────────────────────────────────────

fn mount_share_listing(f: &Fixture) {
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(query_param("method", "list_share"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "data": {"total": 1, "offset": 0, "shares": [
                    {"name": "homes", "path": "/homes", "isdir": true}
                ]}
            })))
            .mount(&f.server),
    );
}

fn share_listings_asked_for(f: &Fixture) -> usize {
    f.rt.block_on(f.server.received_requests())
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r.url
                .query()
                .is_some_and(|q| q.contains("method=list_share"))
        })
        .count()
}

/// Regression: the mount asked the NAS for a listing every single time it
/// was asked for one, and the kernel asks more than once per directory
/// read — once for the entries, then again at the end offset to be told
/// there are no more. A desktop file manager polling a freshly-appeared
/// volume (GIO does exactly this) became a sustained stream of listings
/// against `synoscgi`, the shared CGI backend the whole appliance runs on:
/// roughly ten a second, indefinitely, for a directory nobody was looking
/// at any more.
#[test]
fn a_repeated_listing_is_served_without_asking_the_nas_again() {
    let f = fixture();
    mount_share_listing(&f);

    let first = f.fs.listing(VIRTUAL_ROOT_PATH).expect("a listing");
    let second = f.fs.listing(VIRTUAL_ROOT_PATH).expect("a listing");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1, "the same listing, served twice");
    assert_eq!(
        share_listings_asked_for(&f),
        1,
        "the second read has to come from the cache, or a polling client \
         is a denial of service aimed at the appliance"
    );
}

/// The cache may not outlive a change this mount itself made: a file
/// created and then listed has to be there, or the mount contradicts
/// itself within one process.
#[test]
fn a_listing_is_fetched_again_after_the_directory_changes() {
    let f = fixture();
    mount_share_listing(&f);

    f.fs.listing(VIRTUAL_ROOT_PATH).expect("a listing");
    f.fs.dir_cache.invalidate(VIRTUAL_ROOT_PATH);
    f.fs.listing(VIRTUAL_ROOT_PATH).expect("a listing");

    assert_eq!(share_listings_asked_for(&f), 2);
}

/// The virtual root is spelled `""`, so a top-level share's parent is not
/// what string arithmetic naively produces: `/homes` would otherwise
/// invalidate `""[..0]`, which is the same thing by luck rather than by
/// intent, and `homes` (no leading slash) would panic on the slice.
#[test]
fn the_parent_of_a_top_level_share_is_the_virtual_root() {
    let cache = DirCache::new(30);
    cache.insert(VIRTUAL_ROOT_PATH, vec![]);
    cache.insert("/homes", vec![]);

    forget_parent_listing(&cache, "/homes");

    assert!(cache.get(VIRTUAL_ROOT_PATH).is_none(), "the root is stale");
    assert!(
        cache.get("/homes").is_some(),
        "the share's own listing did not change; a file appeared beside it"
    );
}

#[test]
fn the_parent_of_a_nested_path_is_the_directory_holding_it() {
    let cache = DirCache::new(30);
    cache.insert("/homes/chris", vec![]);
    cache.insert("/homes", vec![]);

    forget_parent_listing(&cache, "/homes/chris/notes.txt");

    assert!(cache.get("/homes/chris").is_none());
    assert!(cache.get("/homes").is_some(), "only one level up");
}

/// A name with no separator at all reaches this from a caller that built
/// it itself. Slicing on a `None` index would panic in a FUSE callback,
/// which takes the mount down rather than failing one operation.
#[test]
fn a_bare_name_falls_back_to_the_root_rather_than_panicking() {
    let cache = DirCache::new(30);
    cache.insert(VIRTUAL_ROOT_PATH, vec![]);

    forget_parent_listing(&cache, "notes.txt");

    assert!(cache.get(VIRTUAL_ROOT_PATH).is_none());
}

// ── Speculative prefetch ──────────────────────────────────────────────────

/// Put a file of `size` bytes in the inode cache, so the prefetch planners can
/// see how long it is. `open` learns the size this way in production too — the
/// kernel must `lookup` before it can `open`, and `lookup` caches the info.
fn seed_size(f: &Fixture, path: &str, size: u64) -> u64 {
    let ino = f.fs.cache.get_or_alloc_ino(path);
    f.fs.cache.insert(
        ino,
        SynoFileInfo {
            name: path.rsplit('/').next().unwrap().to_string(),
            path: path.to_string(),
            isdir: false,
            additional: Some(SynoAdditional {
                size: Some(size),
                owner: None,
                time: None,
                perm: None,
            }),
            code: None,
        },
    );
    ino
}

/// Every block start the mount asked the NAS for, in request order.
fn downloaded_starts(f: &Fixture) -> Vec<u64> {
    f.rt.block_on(f.server.received_requests())
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.query().is_some_and(|q| q.contains("method=download")))
        .filter_map(|r| {
            let h = r.headers.get("range")?.to_str().ok()?;
            h.trim_start_matches("bytes=")
                .split_once('-')?
                .0
                .parse()
                .ok()
        })
        .collect()
}

/// The same requests as `downloaded_starts`, but keeping how much each one
/// asked for: `(start, blocks)`. What bounds the mount is the bytes a request
/// covers, not that it is one request.
fn downloaded_spans(f: &Fixture) -> Vec<(u64, u64)> {
    f.rt.block_on(f.server.received_requests())
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.query().is_some_and(|q| q.contains("method=download")))
        .filter_map(|r| {
            let h = r.headers.get("range")?.to_str().ok()?;
            let (start, end) = h.trim_start_matches("bytes=").split_once('-')?;
            let start: u64 = start.parse().ok()?;
            let end: u64 = end.parse().ok()?;
            Some((start, (end - start + 1).div_ceil(BLOCK)))
        })
        .collect()
}

fn body_with_magic(magic: &[u8], len: usize) -> Vec<u8> {
    let mut v = ramp(len);
    v[..magic.len()].copy_from_slice(magic);
    v
}

const MP4_MAGIC: &[u8] = b"\x00\x00\x00\x18ftypisom";
const JPEG_MAGIC: &[u8] = b"\xFF\xD8\xFF\xE0\x00\x10JFIF";

/// The bug this whole change exists for. A 22-block file used to cost twenty
/// blocks at `open` — the fixed head-plus-tail window — no matter what the
/// caller went on to do. For a photo corpus being scanned for headers that is
/// the entire file, fetched to answer a 48 KiB read.
#[test]
fn opening_a_non_media_file_fetches_only_the_first_block() {
    let f = fixture();
    mount_download(
        &f,
        body_with_magic(JPEG_MAGIC, 22 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(&f, "/share/photo.jpg", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/photo.jpg");
    f.fs.await_prefetch(ino);

    assert_eq!(
        downloaded_starts(&f),
        vec![0],
        "a file with no trailing index needs nothing but the block the caller asked for"
    );
}

/// The case the prefetch was written for, and the one regression that would
/// make video unplayable again: a player finds a non-faststart `moov` by
/// seeking to EOF, which no access-pattern heuristic can ever predict. The
/// tail has to be fetched eagerly, so it still is.
#[test]
fn opening_a_media_file_still_fetches_the_head_and_the_tail() {
    let f = fixture();
    mount_download(
        &f,
        body_with_magic(MP4_MAGIC, 22 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(&f, "/share/clip.mp4", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.await_prefetch(ino);

    let cached: Vec<u64> = (0..22)
        .filter(|b| f.fs.read_cache.contains(ino, *b))
        .collect();
    assert_eq!(
        cached,
        vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 18, 19, 20, 21],
        "the media window is unchanged: block 0, the head, and the last four"
    );
}

/// Chris's measurement: reading 48 KiB cost the same as reading the whole
/// file. One read at offset 0 followed by a close must cost one block.
#[test]
fn a_header_read_does_not_drag_a_window_behind_it() {
    let f = fixture();
    mount_download(
        &f,
        body_with_magic(JPEG_MAGIC, 22 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(&f, "/share/photo.jpg", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/photo.jpg");
    f.fs.read_range(ino, "/share/photo.jpg", 0, 48).unwrap();
    f.fs.read_ahead(1, ino, "/share/photo.jpg", 0, 48);
    f.fs.await_prefetch(ino);

    assert_eq!(downloaded_starts(&f), vec![0]);
}

/// The other half of the deal: a reader that keeps going still gets read-ahead,
/// from its second read onward.
#[test]
fn a_sequential_reader_still_gets_read_ahead() {
    let f = fixture();
    mount_download(
        &f,
        body_with_magic(JPEG_MAGIC, 22 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(&f, "/share/photo.jpg", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/photo.jpg");
    f.fs.read_ahead(1, ino, "/share/photo.jpg", 0, BLOCK);
    f.fs.read_ahead(1, ino, "/share/photo.jpg", BLOCK, BLOCK);
    f.fs.await_prefetch(ino);

    let cached: Vec<u64> = (0..22)
        .filter(|b| f.fs.read_cache.contains(ino, *b))
        .collect();
    assert_eq!(
        cached,
        vec![0, 2, 3],
        "the ramp opens at two blocks — fetched as one run, cached as two"
    );
    assert_eq!(
        downloaded_starts(&f),
        vec![0, 2 * BLOCK],
        "and those two blocks cost one request, not two"
    );
}

/// Closing the file used to leave up to 4 MiB still downloading, competing
/// with whatever the caller did next. In a file-at-a-time walk every closed
/// file kept stealing bandwidth from its successors.
#[test]
fn closing_a_file_abandons_its_outstanding_prefetch() {
    let f = fixture();
    mount_download_slow(
        &f,
        body_with_magic(MP4_MAGIC, 40 * BLOCK as usize),
        Duration::from_secs(3),
    );
    let ino = seed_size(&f, "/share/clip.mp4", 40 * BLOCK);

    seed_block0(&f, ino, MP4_MAGIC);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.end_read(1, ino);

    assert_eq!(
        f.fs.outstanding_prefetch(ino),
        0,
        "release must drop the prefetch, not let it run on against the next file"
    );
    // And the claims must come back, or the next reader of those blocks waits
    // out the in-flight timeout instead of just downloading them.
    assert!(
        eventually(|| f.fs.read_cache.claim_inflight(ino, 15)),
        "an abandoned block's in-flight claim must be released"
    );
}

/// A second handle on the same file must not have its read-ahead cancelled by
/// the first one closing.
#[test]
fn closing_one_handle_leaves_another_handles_prefetch_alone() {
    let f = fixture();
    mount_download_slow(
        &f,
        body_with_magic(MP4_MAGIC, 40 * BLOCK as usize),
        Duration::from_secs(3),
    );
    let ino = seed_size(&f, "/share/clip.mp4", 40 * BLOCK);

    seed_block0(&f, ino, MP4_MAGIC);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.prime_open(2, ino, "/share/clip.mp4");
    f.fs.end_read(1, ino);

    assert!(
        f.fs.outstanding_prefetch(ino) > 0,
        "the file is still open on handle 2"
    );
}

/// Open eight media files at once against a mock that never answers, so that
/// what it has received is exactly what is on the wire.
fn eight_slow_opens(f: &Fixture) -> Vec<u64> {
    mount_download_slow(
        f,
        body_with_magic(MP4_MAGIC, 200 * BLOCK as usize),
        Duration::from_secs(3),
    );

    let mut inos = Vec::new();
    for i in 0..8u64 {
        let path = format!("/share/clip{i}.mp4");
        let ino = seed_size(f, &path, 200 * BLOCK);
        seed_block0(f, ino, MP4_MAGIC);
        f.fs.prime_open(i, ino, &path);
        inos.push(ino);
    }
    std::thread::sleep(Duration::from_millis(500));
    inos
}

/// The FUSE path is the highest-concurrency consumer in the codebase and had
/// nothing bounding it: eight parallel opens meant ~160 simultaneous requests
/// against `synoscgi`, the shared CGI backend the whole appliance runs on.
///
/// The budget has to be counted in **blocks**, not requests. It was written
/// when a speculative task was one block, so sixteen tasks meant sixteen
/// blocks on the wire. Coalescing then made a task a run of up to
/// `MAX_PREFETCH_SPAN` blocks without changing the constant, and the same
/// sixteen came to permit sixteen times as many bytes — with the caller's own
/// block queued behind all of them. Over a link that reaches the NAS through
/// a VPN, that is how a 256 KiB read waits out `BLOCK_WAIT_TIMEOUT`.
#[test]
fn prefetch_against_the_nas_is_capped_in_blocks_not_requests() {
    let f = fixture();
    eight_slow_opens(&f);

    let on_the_wire: u64 = downloaded_spans(&f).iter().map(|(_, blocks)| blocks).sum();
    assert!(
        on_the_wire <= MAX_INFLIGHT_PREFETCH_BLOCKS as u64,
        "eight opens put {on_the_wire} speculative blocks on the wire at once; \
         the budget is {MAX_INFLIGHT_PREFETCH_BLOCKS}"
    );
}

/// The budget bounds the mount, but it still has to fit one file's whole open
/// window. A window split into waves makes the case the window exists for — a
/// video that will not play until its trailing index arrives — slower than
/// having no window at all.
#[test]
fn one_files_whole_window_still_goes_out_at_once() {
    let f = fixture();
    eight_slow_opens(&f);

    let head_blocks = DEFAULT_PREFETCH_BLOCKS - 1;
    let tail_start = (200 - TAIL_BLOCKS) * BLOCK;
    let spans = downloaded_spans(&f);
    assert!(
        spans.contains(&(BLOCK, head_blocks)),
        "the first file's head window went out as one {head_blocks}-block run: {spans:?}"
    );
    assert!(
        spans.contains(&(tail_start, TAIL_BLOCKS)),
        "and its tail with it, as one more: {spans:?}"
    );
}

/// A claim has to mean a download that is on the wire *now*.
///
/// The permit was acquired inside the spawned task, but the blocks were
/// claimed before the task was ever spawned — so a run still queued for the
/// budget held a claim on every block in it. A reader that wanted one of
/// those blocks found the claim, went to sleep in `wait_for_block`, and
/// waited out the whole minute behind speculation that had not started.
/// That is why the mount's own log line insists the download "is still
/// running": nothing on that path could tell the difference. Speculation that
/// cannot start now claims nothing, and the reader just fetches the block.
#[test]
fn speculation_that_cannot_start_holds_no_claims() {
    let f = fixture();
    let inos = eight_slow_opens(&f);
    let last = *inos.last().unwrap();

    assert!(
        f.fs.read_cache.claim_inflight(last, 1),
        "the budget was long gone by the eighth open, so that file's window \
         must have been dropped rather than left parked on its blocks' claims"
    );
}

/// The knob a bulk consumer reaches for. Zero must mean zero — including the
/// open window, or a scan still pays for a window it set out to disable.
#[test]
fn depth_zero_turns_the_speculation_off_entirely() {
    let f = fixture_with_prefetch(0);
    mount_download(
        &f,
        body_with_magic(MP4_MAGIC, 22 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(&f, "/share/clip.mp4", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.read_ahead(1, ino, "/share/clip.mp4", 0, BLOCK);
    f.fs.read_ahead(1, ino, "/share/clip.mp4", BLOCK, BLOCK);
    f.fs.await_prefetch(ino);

    assert_eq!(downloaded_starts(&f), vec![0]);
}

/// The window is the right *blocks*, but it used to be fetched one block per
/// request — nineteen round trips to move a contiguous run. Over SMB that is
/// the difference between one READ and nineteen, and round trips are what this
/// mount is short of.
#[test]
fn a_contiguous_window_is_asked_for_in_one_request() {
    let f = fixture();
    mount_download(
        &f,
        body_with_magic(MP4_MAGIC, 22 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(&f, "/share/clip.mp4", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.await_prefetch(ino);

    let mut starts = downloaded_starts(&f);
    starts.sort_unstable();
    assert_eq!(
        starts,
        vec![0, BLOCK, 18 * BLOCK],
        "block 0 synchronously, then the head as one run and the tail as another"
    );
}

/// Coalescing must not change what ends up cached: every block of the run is
/// still individually addressable afterwards.
#[test]
fn a_coalesced_run_still_fills_every_block_it_covered() {
    let f = fixture();
    mount_download(
        &f,
        body_with_magic(MP4_MAGIC, 22 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(&f, "/share/clip.mp4", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.await_prefetch(ino);

    for b in 1..16u64 {
        let block = f.fs.read_cache.get(ino, b).expect("block {b} of the run");
        assert_eq!(block.len(), BLOCK as usize, "block {b} is whole");
    }
}

/// The bytes a coalesced run stores must land in the right blocks — splitting
/// one response into blocks is exactly where an off-by-one silently serves the
/// wrong file offsets.
#[test]
fn a_coalesced_run_puts_the_bytes_at_the_right_offsets() {
    let f = fixture();
    let body = body_with_magic(MP4_MAGIC, 22 * BLOCK as usize);
    mount_download(&f, body.clone(), Map::new());
    let ino = seed_size(&f, "/share/clip.mp4", 22 * BLOCK);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.await_prefetch(ino);

    for b in [1u64, 7, 15, 18, 21] {
        let got = f.fs.read_cache.get(ino, b).expect("a cached block");
        let start = (b * BLOCK) as usize;
        assert_eq!(
            got.as_ref(),
            &body[start..start + BLOCK as usize],
            "block {b} holds the bytes at its own offset"
        );
    }
}

/// A run that runs off the end of the file gets a short response. The block it
/// straddles keeps its real bytes and the rest are EOF — the same contract a
/// per-block fetch had.
#[test]
fn a_run_that_reaches_eof_stores_the_partial_block_it_got() {
    let f = fixture();
    // 3.5 blocks: the media window asks for blocks 1..3, and block 3 is half.
    let len = 3 * BLOCK as usize + BLOCK as usize / 2;
    let body = body_with_magic(MP4_MAGIC, len);
    mount_download(&f, body.clone(), Map::new());
    let ino = seed_size(&f, "/share/clip.mp4", len as u64);

    f.fs.prime_open(1, ino, "/share/clip.mp4");
    f.fs.await_prefetch(ino);

    let tail =
        f.fs.read_cache
            .get(ino, 3)
            .expect("the final partial block");
    assert_eq!(
        tail.len(),
        BLOCK as usize / 2,
        "half a block is what exists"
    );
    assert_eq!(tail.as_ref(), &body[3 * BLOCK as usize..]);
}

// ── Reading over several connections ─────────────────────────────────────────

/// Measured off campus: four SMB connections with the default read-ahead ran
/// at 2.8 MB/s, and with four times the read-ahead at 4.4 — each connection
/// had only one to four reads in flight, so the window, not the link, was the
/// limit. The budget is per connection, so it widens with them.
#[test]
fn with_four_connections_the_prefetch_budget_is_four_times_as_wide() {
    let f = fixture_reading_over(4);
    eight_slow_opens(&f);

    let on_the_wire: u64 = downloaded_spans(&f).iter().map(|(_, blocks)| blocks).sum();
    let budget = 4 * MAX_INFLIGHT_PREFETCH_BLOCKS as u64;
    assert!(
        on_the_wire > MAX_INFLIGHT_PREFETCH_BLOCKS as u64,
        "four connections, and still only {on_the_wire} blocks in flight"
    );
    assert!(
        on_the_wire <= budget,
        "{on_the_wire} blocks in flight; four connections allow {budget}"
    );
}

/// The reads a sequential reader ramps up to, as runs of blocks.
fn ramp_spans(f: &Fixture) -> Vec<u64> {
    mount_download(
        f,
        body_with_magic(JPEG_MAGIC, 400 * BLOCK as usize),
        Map::new(),
    );
    let ino = seed_size(f, "/share/scan.jpg", 400 * BLOCK);
    f.fs.prime_open(1, ino, "/share/scan.jpg");
    for b in 0..200 {
        f.fs.read_ahead(1, ino, "/share/scan.jpg", b * BLOCK, BLOCK);
        f.fs.await_prefetch(ino);
    }
    downloaded_spans(f)
        .into_iter()
        .map(|(_, blocks)| blocks)
        .collect()
}

#[test]
fn a_sequential_reader_ramps_further_over_more_connections() {
    let alone = ramp_spans(&fixture_with_prefetch_over(4, 1));
    let spread = ramp_spans(&fixture_with_prefetch_over(4, 4));

    assert!(
        alone.iter().all(|&blocks| blocks <= 4),
        "one connection keeps to its depth of 4: {alone:?}"
    );
    assert!(
        spread.iter().any(|&blocks| blocks > 4),
        "four connections read further ahead than one: {spread:?}"
    );
}

/// The window an open fetches is not scaled: it is fetched for every file
/// opened, which for a file browser making thumbnails is every video in a
/// folder, and the reads it saves are the first few.
#[test]
fn the_window_fetched_at_open_does_not_grow_with_connections() {
    let f = fixture_reading_over(4);
    eight_slow_opens(&f);

    let head_blocks = DEFAULT_PREFETCH_BLOCKS - 1;
    assert!(
        downloaded_spans(&f)
            .iter()
            .all(|&(_, blocks)| blocks <= head_blocks),
        "{:?}",
        downloaded_spans(&f)
    );
}
