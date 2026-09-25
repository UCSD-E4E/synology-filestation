//! Writes: flush, truncate, cross-directory moves, and the streamed path.

use std::sync::atomic::Ordering;

use synology_filestation_core::transport::WriteHandle;

use super::*;

// ── T1.2: truncate ────────────────────────────────────────────────────────

/// Regression: a failed download was mapped to `Vec::new()`, so truncate
/// then uploaded `new_size` zero bytes over a perfectly good file. One read
/// timeout during `truncate -s N` destroyed the contents. It must abort.
#[test]
fn truncate_aborts_rather_than_zeroing_the_file_when_the_download_fails() {
    let f = fixture();
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(query_param("method", "download"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&f.server),
    );
    // If truncate ever reaches the upload after a failed read, this catches it.
    f.rt.block_on(
        Mock::given(http_method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"success": true, "data": {"blks": null}})),
            )
            .expect(0)
            .mount(&f.server),
    );

    let err =
        f.fs.truncate_file(9, "/share/big.bin", 100)
            .expect_err("an unreadable file must not be silently replaced with zeros");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
    // The `expect(0)` on the upload mock is asserted when the server drops.
}

/// Truncating to zero needs no prior content, and it is the common case
/// (`O_TRUNC`, `> file`). Downloading a file we are about to discard is pure
/// waste — and on a large file it is the whole file, in memory.
#[test]
fn truncate_to_zero_does_not_download_the_file_first() {
    let f = fixture();
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(query_param("method", "download"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&f.server),
    );
    mount_delete_ok(&f);
    mount_getinfo_gone(&f);
    mount_upload_ok(&f);

    // The trailing get_info re-read fails against these mocks (getinfo is
    // wired to "gone" for the overwrite poll), which is irrelevant here: the
    // assertion is the `expect(0)` on the download mock.
    let _ = f.fs.truncate_file(10, "/share/f.bin", 0);
}

/// Truncation still has to do its actual job.
#[test]
fn truncate_uploads_the_resized_content() {
    let f = fixture();
    let body = ramp(4096);
    mount_download(&f, body.clone(), Map::new());
    mount_delete_ok(&f);
    mount_getinfo_gone(&f);
    mount_upload_ok(&f);

    let _ = f.fs.truncate_file(12, "/share/f.bin", 100);

    let posted = posted_bodies(&f);
    assert_eq!(posted.len(), 1, "exactly one upload");
    assert!(
        posted[0].windows(100).any(|w| w == &body[..100]),
        "the upload must carry the first 100 bytes of the original file"
    );
}

// ── T1.3: cross-directory move ────────────────────────────────────────────

/// Regression: the move sized its download from the inode cache, then
/// deleted the source. A stale-low cached size therefore truncated the file
/// permanently. The download must ask for the whole file.
#[test]
fn cross_directory_move_copies_the_whole_file_not_the_cached_size() {
    let f = fixture();
    let body = ramp(4096);
    mount_download(&f, body.clone(), Map::new());
    mount_delete_ok(&f);
    mount_getinfo_gone(&f);
    mount_upload_ok(&f);

    // Stale metadata claiming the file is 10 bytes long.
    let ino = f.fs.cache.get_or_alloc_ino("/a/f.bin");
    f.fs.cache.insert(
        ino,
        SynoFileInfo {
            name: "f.bin".into(),
            path: "/a/f.bin".into(),
            isdir: false,
            additional: Some(SynoAdditional {
                size: Some(10),
                owner: None,
                time: None,
                perm: None,
            }),
            code: None,
        },
    );

    f.fs.move_across_dirs("/a/f.bin", "/b", "f.bin").unwrap();

    let posted = posted_bodies(&f);
    assert_eq!(posted.len(), 1, "exactly one upload");
    assert!(
        posted[0].windows(body.len()).any(|w| w == body.as_slice()),
        "the moved file must carry all {} bytes, not the {} the cache claimed",
        body.len(),
        10
    );
}

// ── T1.1: flush ───────────────────────────────────────────────────────────

/// Regression: `flush` spawned the upload and replied OK immediately, and
/// the kernel discards whatever `release` later reports — so a failed upload
/// reached the application as a successful `close(2)`. The call `flush`
/// delegates to must surface the failure.
#[test]
fn flush_reports_an_upload_failure_instead_of_swallowing_it() {
    let f = fixture();
    f.rt.block_on(
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&f.server),
    );

    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");
    let err =
        f.fs.finish_upload(fh)
            .expect_err("a failed upload must not look like a successful close");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
}

/// `flush` must not return until the bytes are actually on the NAS — a
/// merely-queued upload is one whose failure nobody can report.
#[test]
fn flush_completes_the_upload_before_returning() {
    let f = fixture();
    mount_upload_ok(&f);

    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");
    f.fs.finish_upload(fh).unwrap();

    assert_eq!(
        posted_bodies(&f).len(),
        1,
        "the upload must have completed by the time flush returns, not merely been queued"
    );
}

/// A failed flush must leave the data buffered so `release` can retry it.
/// Previously `dirty` was cleared *before* the upload was even started, so a
/// failure silently discarded the write.
#[test]
fn a_failed_flush_keeps_the_data_pending_for_a_retry() {
    let f = fixture();
    f.rt.block_on(
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&f.server),
    );

    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");
    assert!(f.fs.finish_upload(fh).is_err());

    assert!(
        f.fs.buffer(fh).unwrap().blocking_lock().dirty,
        "a failed upload must leave the buffer dirty so release() retries it"
    );
}

/// A successful flush clears `new_file`, so a second flush of the same
/// handle overwrites rather than racing an `overwrite=false` create against
/// the file it just created.
#[test]
fn a_successful_flush_marks_the_file_as_existing() {
    let f = fixture();
    mount_upload_ok(&f);

    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");
    f.fs.finish_upload(fh).unwrap();

    let handle = f.fs.buffer(fh).unwrap();
    let buf = handle.blocking_lock();
    assert!(!buf.dirty, "a successful upload clears dirty");
    assert!(!buf.new_file, "the file now exists on the NAS");
}

#[test]
fn flush_of_a_clean_buffer_is_a_no_op() {
    let f = fixture();
    f.rt.block_on(
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&f.server),
    );

    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");
    f.fs.buffer(fh).unwrap().blocking_lock().dirty = false;

    f.fs.finish_upload(fh).unwrap();
}

// ── T1.5: concurrent dispatch ─────────────────────────────────────────────

/// A slow upload, so a second thread can be observed racing it.
fn mount_upload_slow(f: &Fixture, delay: Duration) {
    f.rt.block_on(
        Mock::given(http_method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"success": true, "data": {"blks": null}}))
                    .set_delay(delay),
            )
            .mount(&f.server),
    );
}

/// Regression: `finish_upload` snapshotted the buffer and then uploaded with
/// no lock held, which was safe only because fuser dispatched every callback
/// on one thread. Uploads now run on the runtime and the event loop is
/// multi-threaded, so a `write` can land while that handle's upload is in
/// flight — and for a spilled buffer
/// the "snapshot" is the temp file the upload is still streaming, so the
/// write would tear the body mid-transfer. A write must wait for the upload
/// already running on its handle.
#[test]
fn a_write_waits_for_an_upload_in_flight_on_the_same_handle() {
    let f = fixture();
    mount_upload_slow(&f, Duration::from_millis(600));

    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"original");

    std::thread::scope(|s| {
        s.spawn(|| f.fs.finish_upload(fh).unwrap());
        // Let the upload get as far as the wire before racing it.
        std::thread::sleep(Duration::from_millis(150));
        let started = std::time::Instant::now();
        f.fs.write_buffer_at(fh, 0, b"clobbered").unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "the write returned in {:?} — it did not wait for the in-flight upload",
            started.elapsed()
        );
    });

    let posted = posted_bodies(&f);
    assert_eq!(posted.len(), 1, "exactly one upload");
    assert!(
        posted[0].windows(8).any(|w| w == b"original"),
        "the upload must carry the bytes it started with, not the racing write's"
    );
}

/// …but only *its* handle. The per-handle wait must not become a global one:
/// serialising unrelated files is the very wedge this change removes.
#[test]
fn an_upload_does_not_block_work_on_another_handle() {
    let f = fixture();
    mount_upload_slow(&f, Duration::from_millis(600));

    let slow = seed_dirty_buffer_fh(&f, 1, "/share/slow.bin", b"payload");
    let other = seed_dirty_buffer_fh(&f, 2, "/share/other.bin", b"payload");

    std::thread::scope(|s| {
        s.spawn(|| f.fs.finish_upload(slow).unwrap());
        std::thread::sleep(Duration::from_millis(150));
        let started = std::time::Instant::now();
        f.fs.write_buffer_at(other, 0, b"unrelated").unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "a write to an unrelated handle waited {:?} on someone else's upload",
            started.elapsed()
        );
    });
}

/// The `write` callback's own error path: an unknown handle is EIO, not a
/// panic and not a silently accepted write.
#[test]
fn writing_to_an_unknown_handle_fails() {
    let f = fixture();
    assert!(f.fs.write_buffer_at(999, 0, b"x").is_err());
}

// ── T1.6: transfers run off the event loop ────────────────────────────────

/// The outcome of a `start_*` call, as seen from the test thread.
type Outcome<T> = std::sync::mpsc::Receiver<Result<T, SynoFsError>>;

/// Collects a `start_*` outcome from whichever runtime thread produced it.
fn outcome_channel<T: Send + 'static>() -> (
    impl FnOnce(Result<T, SynoFsError>) + Send + 'static,
    Outcome<T>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    (
        move |r| {
            let _ = tx.send(r);
        },
        rx,
    )
}

/// Regression: `flush` ran the upload with `block_on` on the FUSE
/// event-loop thread, so for the length of a transfer — minutes on a large
/// file — that thread served nothing else. The upload belongs on the
/// runtime; the callback should return as soon as it is queued.
#[test]
fn starting_an_upload_does_not_block_the_calling_thread() {
    let f = fixture();
    mount_upload_slow(&f, Duration::from_millis(600));
    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");

    let (done, outcome) = outcome_channel();
    let started = std::time::Instant::now();
    f.fs.start_upload(fh, done);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the calling thread waited {:?} on the transfer",
        started.elapsed()
    );

    outcome
        .recv_timeout(Duration::from_secs(10))
        .expect("the reply must still arrive once the transfer lands")
        .expect("the upload itself succeeds");
    assert_eq!(posted_bodies(&f).len(), 1, "the bytes really went out");
}

/// Going off-thread must not cost the error path: `close(2)` still reports
/// what the upload did, it just learns it later.
#[test]
fn a_backgrounded_upload_still_reports_its_failure() {
    let f = fixture();
    f.rt.block_on(
        Mock::given(http_method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&f.server),
    );
    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");

    let (done, outcome) = outcome_channel();
    f.fs.start_upload(fh, done);

    let result = outcome
        .recv_timeout(Duration::from_secs(10))
        .expect("a failed upload must still produce a reply");
    assert!(
        matches!(result, Err(SynoFsError::Io(_))),
        "got {result:?} — a failed upload must not look like a successful close"
    );
    assert!(
        f.fs.buffer(fh).unwrap().blocking_lock().dirty,
        "the data stays buffered for release() to retry"
    );
}

/// The single event-loop thread used to be the only thing bounding how many
/// transfers we aimed at the NAS at once. Now that transfers run on the
/// runtime, that accidental limit is gone and an explicit one has to take
/// its place: parallel FileStation transfers are what saturated `synoscgi`
/// and took the appliance down.
#[test]
fn concurrent_transfers_are_capped_so_the_nas_is_not_swarmed() {
    let f = fixture();
    // Long enough that every upload started below is still on the wire when
    // the count is taken.
    mount_upload_slow(&f, Duration::from_secs(3));

    let started = MAX_CONCURRENT_TRANSFERS + 2;
    for fh in 0..started as u64 {
        seed_dirty_buffer_fh(&f, fh, &format!("/share/f{fh}.bin"), b"payload");
        f.fs.start_upload(fh, |_| {});
    }
    // Give every task that is *allowed* to run time to reach the server.
    std::thread::sleep(Duration::from_millis(500));

    let on_the_wire = posted_bodies(&f).len();
    assert!(
        on_the_wire <= MAX_CONCURRENT_TRANSFERS,
        "{started} uploads were queued and {on_the_wire} reached the NAS at once; \
         the cap is {MAX_CONCURRENT_TRANSFERS}"
    );
    assert_eq!(
        on_the_wire, MAX_CONCURRENT_TRANSFERS,
        "the cap must also be used in full — a smaller number means transfers \
         are queueing on something else"
    );
}

/// Unmounting must not abandon a transfer that is still in flight: the
/// runtime dies with the process, so anything still queued would be lost
/// data. `destroy` waits for it — and does not re-send it afterwards.
#[test]
fn unmounting_waits_for_a_transfer_still_in_flight() {
    let mut f = fixture();
    mount_upload_slow(&f, Duration::from_millis(400));
    let fh = seed_dirty_buffer(&f, "/share/new.txt", b"payload");

    let (done, _outcome) = outcome_channel::<()>();
    f.fs.start_upload(fh, done);
    fuser::Filesystem::destroy(&mut f.fs);

    assert!(
        !f.fs.buffer(fh).unwrap().blocking_lock().dirty,
        "unmount returned with the write still pending"
    );
    assert_eq!(
        posted_bodies(&f).len(),
        1,
        "the in-flight upload must be waited for, not re-sent"
    );
}

/// Truncate is read-modify-write over the whole file, so it blocked its
/// event-loop thread for just as long as an upload. Same treatment.
#[test]
fn starting_a_truncate_does_not_block_the_calling_thread() {
    let f = fixture();
    mount_download(&f, ramp(4096), Map::new());
    mount_delete_ok(&f);
    mount_getinfo_gone(&f);
    mount_upload_slow(&f, Duration::from_millis(600));

    let (done, outcome) = outcome_channel();
    let started = std::time::Instant::now();
    f.fs.start_truncate(12, "/share/f.bin".to_string(), 100, done);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the calling thread waited {:?} on the transfer",
        started.elapsed()
    );

    // The trailing get_info re-read fails against these mocks (getinfo is
    // wired to "gone"), which is irrelevant here: what matters is that a
    // reply came back at all, from the runtime rather than this thread.
    let _ = outcome
        .recv_timeout(Duration::from_secs(10))
        .expect("the reply must still arrive once the transfer lands");
    assert_eq!(
        posted_bodies(&f).len(),
        1,
        "the resized file really went out"
    );
}

/// So did a cross-directory move, which is a whole download plus a whole
/// upload.
#[test]
fn starting_a_cross_directory_move_does_not_block_the_calling_thread() {
    let f = fixture();
    mount_download(&f, ramp(4096), Map::new());
    mount_delete_ok(&f);
    mount_getinfo_gone(&f);
    mount_upload_slow(&f, Duration::from_millis(600));

    let (done, outcome) = outcome_channel();
    let started = std::time::Instant::now();
    f.fs.start_move_across_dirs(
        "/a/f.bin".to_string(),
        "/b".to_string(),
        "f.bin".to_string(),
        done,
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the calling thread waited {:?} on the transfer",
        started.elapsed()
    );

    outcome
        .recv_timeout(Duration::from_secs(10))
        .expect("the reply must still arrive once the transfer lands")
        .expect("the move itself succeeds");
    assert_eq!(posted_bodies(&f).len(), 1, "the copy really went out");
}

use std::sync::atomic::AtomicBool;
use std::sync::Mutex as StdMutex;

// ── streamed writes ──────────────────────────────────────────────────────
//
// With a backend that takes ranges, a write goes out when it is made. The
// difference from the buffered path is not speed but *when things happen*:
// memory stays bounded, `write(2)` waits for the server rather than for a
// local temp file, and a failure is reported by the call that caused it.

/// What a recording sink saw: each write as (offset, bytes).
type SeenWrites = Arc<StdMutex<Vec<(u64, Vec<u8>)>>>;

/// A write sink that records what it was given, and can be told to fail.
#[derive(Default)]
struct RecordingSink {
    writes: SeenWrites,
    closed: Arc<AtomicBool>,
    /// Writes from this one onward fail; `None` never fails. `Some(0)` is a
    /// link that was dead before the first byte — what a mount sees when the
    /// SMB session died while the handle sat open.
    fails_from: Option<usize>,
}

struct RecordingHandle {
    writes: SeenWrites,
    closed: Arc<AtomicBool>,
    fails_from: Option<usize>,
    seen: usize,
}

#[async_trait::async_trait]
impl WriteHandle for RecordingHandle {
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), SynoFsError> {
        let n = self.seen;
        self.seen += 1;
        if self.fails_from.is_some_and(|first| n >= first) {
            return Err(SynoFsError::Io("the link died mid-write".into()));
        }
        self.writes.lock().unwrap().push((offset, data.to_vec()));
        Ok(())
    }
    async fn close(&mut self) -> Result<(), SynoFsError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl synology_filestation_core::transport::OpenWriteTransport for RecordingSink {
    async fn open_write(
        &self,
        _path: &str,
        _mode: WriteOpen,
    ) -> Result<Box<dyn WriteHandle>, SynoFsError> {
        Ok(Box::new(RecordingHandle {
            writes: self.writes.clone(),
            closed: self.closed.clone(),
            fails_from: self.fails_from,
            seen: 0,
        }))
    }
}

/// A fixture whose client can stream, plus the sink it streams into.
fn streaming_fixture(fail_writes: bool) -> (Fixture, Arc<RecordingSink>) {
    streaming_fixture_failing_from(fail_writes.then_some(0))
}

fn streaming_fixture_failing_from(fails_from: Option<usize>) -> (Fixture, Arc<RecordingSink>) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(MockServer::start());
    let sink = Arc::new(RecordingSink {
        fails_from,
        ..Default::default()
    });
    let client = client_for(&server).with_open_write_transport(sink.clone());
    let fs = SynologyFS::new(
        Arc::new(client),
        Arc::new(InodeCache::new(30)),
        Arc::new(DirCache::new(30)),
        Arc::new(ReadCache::new(BLOCK, 64)),
        rt.handle().clone(),
        Ownership {
            uid: 1000,
            gid: 1000,
            umask: 0o022,
        },
        DEFAULT_PREFETCH_BLOCKS,
    );
    (Fixture { fs, server, rt }, sink)
}

/// Open a handle whose sink comes from the client, the way `create` does.
fn streamed_handle(f: &Fixture, nas_path: &str) -> u64 {
    streamed_handle_with(f, nas_path, true)
}

/// Same, for a handle onto a file that was already on the NAS: the buffered
/// path cannot stand in for one of those, since it would upload the bytes this
/// handle happens to write over the whole of what is there.
fn streamed_handle_onto_an_existing_file(f: &Fixture, nas_path: &str) -> u64 {
    streamed_handle_with(f, nas_path, false)
}

fn streamed_handle_with(f: &Fixture, nas_path: &str, new_file: bool) -> u64 {
    let fh = f.fs.next_fh.fetch_add(1, Ordering::Relaxed);
    let sink = f.fs.open_sink(nas_path);
    assert!(
        matches!(sink, WriteSink::Streamed(_)),
        "the backend should have taken this handle"
    );
    f.fs.write_buffers.lock().unwrap().insert(
        fh,
        Arc::new(tokio::sync::Mutex::new(WriteBuffer {
            nas_path: nas_path.to_string(),
            ino: 42,
            sink,
            streamed: false,
            dirty: false,
            new_file,
            broken: false,
        })),
    );
    fh
}

#[test]
fn a_streamed_write_reaches_the_server_before_close() {
    // The buffered path cannot do this: nothing leaves the machine until
    // the file is complete. Here the bytes are gone by the time write(2)
    // returns, which is what bounds memory and paces the copy.
    let (f, sink) = streaming_fixture(false);
    let fh = streamed_handle(&f, "/share/streamed.bin");

    f.fs.write_buffer_at(fh, 0, b"first").unwrap();
    f.fs.write_buffer_at(fh, 5, b"second").unwrap();

    let writes = sink.writes.lock().unwrap().clone();
    assert_eq!(
        writes,
        vec![(0, b"first".to_vec()), (5, b"second".to_vec())],
        "each write went out where it was made, before any close"
    );
    assert!(!sink.closed.load(Ordering::SeqCst), "still open");
}

#[test]
fn closing_a_streamed_handle_uploads_nothing() {
    // No POST at close: the bytes are already there. A second copy over
    // HTTP would be the whole file's worth of traffic for nothing.
    let (f, sink) = streaming_fixture(false);
    let fh = streamed_handle(&f, "/share/streamed.bin");
    f.fs.write_buffer_at(fh, 0, b"payload").unwrap();

    f.fs.finish_upload(fh).expect("close");

    assert!(sink.closed.load(Ordering::SeqCst), "the handle was closed");
    assert_eq!(
        posted_bodies(&f).len(),
        0,
        "nothing was re-sent over the HTTP API"
    );
}

#[test]
fn a_failed_streamed_write_is_reported_at_the_write_and_again_at_close() {
    // Both halves matter. The write must fail where it happened — the
    // whole reason to stream — and close must not then report success over
    // a file the server never fully received.
    let (f, sink) = streaming_fixture(true);
    let fh = streamed_handle_onto_an_existing_file(&f, "/share/doomed.bin");

    let err =
        f.fs.write_buffer_at(fh, 0, b"payload")
            .expect_err("the write failed, so write(2) must say so");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");

    let err =
        f.fs.finish_upload(fh)
            .expect_err("close must not claim a file landed when a write failed");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
    assert!(
        !sink.closed.load(Ordering::SeqCst),
        "the handle was abandoned, not closed as if it were fine"
    );
}

/// Regression: the SMB backend opens a handle for an existing path without
/// touching the wire — deliberately, so it does not have to guess an offset —
/// so a dead session is invisible at open. The client's ladder therefore
/// records a success, its circuit breaker stays closed, and the doomed handle
/// reaches the mount. The first write then failed with
/// `smb: Disconnected from server`, the handle was abandoned, and `close`
/// reported the failure: **no file could be copied to the NAS at all**, even
/// though the HTTP path the metadata calls had already fallen back to was
/// working fine.
///
/// Nothing had left the machine at that point, so there is nothing to be
/// consistent with: the handle can still take the buffered path, which is what
/// this mount did for every write before streaming existed.
#[test]
fn a_streamed_write_that_fails_before_anything_lands_is_buffered_instead() {
    let (f, sink) = streaming_fixture(true);
    mount_upload_ok(&f);
    let fh = streamed_handle(&f, "/share/copied.bin");

    f.fs.write_buffer_at(fh, 0, b"payload")
        .expect("a stream that never sent anything must not fail the write");

    f.fs.finish_upload(fh).expect("close");

    assert!(
        sink.writes.lock().unwrap().is_empty(),
        "the stream took nothing"
    );
    let posted = posted_bodies(&f);
    assert_eq!(posted.len(), 1, "one upload, over HTTP");
    assert!(
        posted[0].windows(7).any(|w| w == b"payload"),
        "carrying the bytes the stream would not take"
    );
}

/// The other half of the rule. Once the server has some of the file, the
/// buffered path cannot stand in: it holds only what was written after the
/// switch, and uploading that would publish a file with a hole where the
/// streamed bytes were. There is nothing to do but report the failure.
#[test]
fn a_streamed_write_that_fails_after_bytes_landed_still_breaks_the_handle() {
    let (f, sink) = streaming_fixture_failing_from(Some(1));
    mount_upload_ok(&f);
    let fh = streamed_handle(&f, "/share/half-sent.bin");

    f.fs.write_buffer_at(fh, 0, b"first")
        .expect("this one lands");
    let err =
        f.fs.write_buffer_at(fh, 5, b"second")
            .expect_err("the link died with bytes already on the server");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");

    let err =
        f.fs.finish_upload(fh)
            .expect_err("close must not claim a file landed when a write failed");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
    assert_eq!(
        sink.writes.lock().unwrap().len(),
        1,
        "only the first landed"
    );
    assert!(posted_bodies(&f).is_empty(), "and nothing was uploaded");
}

#[test]
fn without_a_streaming_backend_writes_are_buffered_as_before() {
    // The HTTP mount is unchanged: nothing can take a range, so the whole
    // file still goes at close.
    let f = fixture();
    let sink = f.fs.open_sink("/share/plain.bin");
    assert!(matches!(sink, WriteSink::Buffered(_)));
}

/// Regression: a 6 MB+ copy onto the mount ended as a **zero-byte file on
/// the NAS**. The buffer's spill to a temp file failed (`ENOENT` — the temp
/// directory was gone), which emptied it, and unlike the streamed path a
/// failed *buffered* write left the handle looking healthy: still dirty,
/// not broken. `close(2)` then happily uploaded what was left — nothing —
/// over the destination. The write reported EIO *and* the file was lost.
///
/// A write that failed must take the handle with it, so close reports the
/// failure instead of publishing a short file.
#[test]
fn a_failed_buffered_write_is_not_uploaded_as_a_truncated_file() {
    let f = fixture();
    mount_upload_ok(&f);

    let dir = std::env::temp_dir().join("synofs-fs-spill-dir-that-does-not-exist");
    assert!(!dir.exists(), "the test needs this path to be absent");

    let fh = f.fs.next_fh.fetch_add(1, Ordering::Relaxed);
    f.fs.write_buffers.lock().unwrap().insert(
        fh,
        Arc::new(tokio::sync::Mutex::new(WriteBuffer {
            nas_path: "/share/big.zip".to_string(),
            ino: 9,
            sink: WriteSink::Buffered(SpillBuffer::with_spill_at_in(8, dir)),
            streamed: false,
            dirty: false,
            new_file: true,
            broken: false,
        })),
    );

    f.fs.write_buffer_at(fh, 0, b"12345678")
        .expect("this much still fits in memory");
    let err =
        f.fs.write_buffer_at(fh, 8, b"overflow")
            .expect_err("the spill had nowhere to go, so write(2) must say so");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");

    let err =
        f.fs.finish_upload(fh)
            .expect_err("close must not claim a file landed when a write failed");
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
    assert_eq!(
        posted_bodies(&f).len(),
        0,
        "nothing was uploaded over the destination"
    );
}

#[test]
fn creating_a_file_and_writing_nothing_still_puts_it_on_the_nas() {
    // `touch`. The handle is opened, never written, and closed. Before
    // this, close found nothing dirty and did nothing, so the file existed
    // only in the inode cache and disappeared when the TTL lapsed.
    let f = fixture();
    mount_upload_ok(&f);

    let fh = f.fs.next_fh.fetch_add(1, Ordering::Relaxed);
    f.fs.write_buffers.lock().unwrap().insert(
        fh,
        Arc::new(tokio::sync::Mutex::new(WriteBuffer {
            nas_path: "/share/touched.txt".to_string(),
            ino: 7,
            sink: WriteSink::Buffered(SpillBuffer::new()),
            // What `create` now seeds.
            streamed: false,
            dirty: true,
            new_file: true,
            broken: false,
        })),
    );

    f.fs.finish_upload(fh).expect("close");

    let bodies = posted_bodies(&f);
    assert_eq!(bodies.len(), 1, "the empty file was uploaded");
}

/// An open-write backend whose `open_write` stays on the wire until the test
/// lets it go: an SMB CREATE through a tunnel with a 110 ms round trip, held
/// open for as long as the test needs to look at what else is stuck.
struct GatedSink {
    started: StdMutex<Option<std::sync::mpsc::Sender<()>>>,
    gate: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl synology_filestation_core::transport::OpenWriteTransport for GatedSink {
    async fn open_write(
        &self,
        _path: &str,
        _mode: WriteOpen,
    ) -> Result<Box<dyn WriteHandle>, SynoFsError> {
        if let Some(started) = self.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        let _ = self.gate.acquire().await.expect("the gate is never closed");
        Ok(Box::new(RecordingHandle {
            writes: SeenWrites::default(),
            closed: Arc::default(),
            fails_from: None,
            seen: 0,
        }))
    }
}

fn gated_fixture() -> (Fixture, Arc<GatedSink>) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(MockServer::start());
    let sink = Arc::new(GatedSink {
        started: StdMutex::new(None),
        gate: tokio::sync::Semaphore::new(0),
    });
    let client = client_for(&server).with_open_write_transport(sink.clone());
    let fs = SynologyFS::new(
        Arc::new(client),
        Arc::new(InodeCache::new(30)),
        Arc::new(DirCache::new(30)),
        Arc::new(ReadCache::new(BLOCK, 64)),
        rt.handle().clone(),
        Ownership {
            uid: 1000,
            gid: 1000,
            umask: 0o022,
        },
        DEFAULT_PREFETCH_BLOCKS,
    );
    (Fixture { fs, server, rt }, sink)
}

#[test]
fn an_open_waiting_on_the_nas_does_not_hold_every_other_handle() {
    // The wedge of 2026-09-25. `open` held the write-buffer map's lock across
    // its `open_write` round trip, and every close's upload task takes that
    // lock on a runtime worker. The round trip needed a worker, the workers
    // waited for the lock, and the whole mount stopped — the tunnel's packet
    // loop included, since it runs on the same two workers.
    let (f, sink) = gated_fixture();
    let (started, open_is_on_the_wire) = std::sync::mpsc::channel();
    *sink.started.lock().unwrap() = Some(started);

    std::thread::scope(|s| {
        s.spawn(|| {
            f.fs.add_write_buffer(1, "/share/slow.bin".into(), 42, false, true)
        });
        open_is_on_the_wire
            .recv_timeout(Duration::from_secs(5))
            .expect("the open reached the backend");

        let (looked_up, lookup_done) = std::sync::mpsc::channel();
        let fs = &f.fs;
        s.spawn(move || {
            let _ = fs.transfers().buffer(2);
            let _ = looked_up.send(());
        });
        let answered = lookup_done.recv_timeout(Duration::from_secs(2)).is_ok();

        // Let the open finish whatever happened, so the scope can join.
        sink.gate.add_permits(1);
        assert!(
            answered,
            "another handle's buffer lookup waited on an open's network round trip"
        );
    });
}

#[test]
fn a_read_only_open_does_not_open_the_file_for_writing_on_the_nas() {
    // Every `open` used to call `open_write`, `O_RDONLY` included: an SMB
    // CREATE for write access on the NAS — 110 ms through the tunnel — paid by
    // a reader that can never write, since the kernel refuses a write on a
    // read-only descriptor before it reaches us.
    let (f, sink) = gated_fixture();
    let (started, opened_for_write) = std::sync::mpsc::channel();
    *sink.started.lock().unwrap() = Some(started);
    sink.gate.add_permits(1);

    f.fs.add_write_buffer(1, "/share/read-me.bin".into(), 42, false, false);

    assert!(
        opened_for_write.try_recv().is_err(),
        "a read-only open asked the NAS for a write handle"
    );
    let handle =
        f.fs.transfers()
            .buffer(1)
            .expect("the handle is registered");
    let buffer = handle.try_lock().expect("nothing else holds the buffer");
    assert!(matches!(buffer.sink, WriteSink::Buffered(_)));
    assert!(!buffer.dirty, "a read-only handle has nothing to upload");
}
