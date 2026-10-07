//! Reading a directory: one listing per open handle, shared between readers,
//! and never fetched on a FUSE event-loop thread.

use super::*;
use std::sync::mpsc;
use std::time::Instant;

const DIR: &str = "/share/big";
const DIR_INO: u64 = 42;

/// A listing of `n` files under [`DIR`], answered after `delay`.
fn mount_dir_listing(f: &Fixture, n: usize, delay: Duration) {
    let files: Vec<_> = (0..n)
        .map(|i| {
            serde_json::json!({
                "name": format!("f{i}.wav"),
                "path": format!("{DIR}/f{i}.wav"),
                "isdir": false,
            })
        })
        .collect();
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(query_param("method", "list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "success": true,
                        "data": {"total": n, "offset": 0, "files": files}
                    }))
                    .set_delay(delay),
            )
            .mount(&f.server),
    );
}

fn listings_asked_for(f: &Fixture) -> usize {
    f.rt.block_on(f.server.received_requests())
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r.url
                .query()
                .is_some_and(|q| q.split('&').any(|kv| kv == "method=list"))
        })
        .count()
}

/// Start a `readdir` and hand back where its answer will arrive.
fn start(
    f: &Fixture,
    fh: u64,
    offset: u64,
) -> mpsc::Receiver<Result<Arc<Vec<DirEntry>>, SynoFsError>> {
    let (tx, rx) = mpsc::channel();
    f.fs.start_readdir(fh, DIR_INO, DIR.to_string(), offset, move |r| {
        let _ = tx.send(r);
    });
    rx
}

/// A `readdir`, waited for.
fn readdir(f: &Fixture, fh: u64, offset: u64) -> Arc<Vec<DirEntry>> {
    start(f, fh, offset)
        .recv_timeout(Duration::from_secs(10))
        .expect("readdir answered")
        .expect("readdir succeeded")
}

fn names(entries: &[DirEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

/// Regression: the wedge. The kernel reads a directory a few hundred entries
/// per call, and every call fetched the listing again whenever the cached
/// copy had expired. A directory of a million entries takes minutes to list,
/// so `find` went: one chunk, a full re-listing, one chunk, another full
/// re-listing — days to read one directory, with the mount frozen behind it.
/// Caching is off here, so any second fetch is the bug.
#[test]
fn a_directory_read_in_pieces_is_listed_once() {
    let f = fixture_with(DEFAULT_PREFETCH_BLOCKS, 0);
    mount_dir_listing(&f, 10, Duration::ZERO);
    let fh = f.fs.open_dir();

    let first = readdir(&f, fh, 0);
    let later = readdir(&f, fh, 5);
    let last = readdir(&f, fh, 12);

    assert_eq!(listings_asked_for(&f), 1, "one listing for one pass");
    assert_eq!(first.len(), 12, "., .. and ten files");
    assert_eq!(names(&later), names(&first), "every piece of one snapshot");
    assert_eq!(names(&last), names(&first));
}

/// `rewinddir` is a new pass, and a new pass is entitled to see what has
/// changed since the last one.
#[test]
fn reading_a_directory_from_the_start_again_lists_it_again() {
    let f = fixture_with(DEFAULT_PREFETCH_BLOCKS, 0);
    mount_dir_listing(&f, 3, Duration::ZERO);
    let fh = f.fs.open_dir();

    readdir(&f, fh, 0);
    readdir(&f, fh, 0);

    assert_eq!(listings_asked_for(&f), 2);
}

/// Two walkers reaching the same huge directory used to fetch it twice, side
/// by side, each for minutes.
#[test]
fn two_readers_of_one_directory_share_one_listing() {
    let f = fixture_with(DEFAULT_PREFETCH_BLOCKS, 0);
    mount_dir_listing(&f, 3, Duration::from_millis(500));
    let (a, b) = (f.fs.open_dir(), f.fs.open_dir());

    let first = start(&f, a, 0);
    let second = start(&f, b, 0);
    let first = first
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let second = second
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();

    assert_eq!(names(&first), names(&second));
    assert_eq!(
        listings_asked_for(&f),
        1,
        "the second reader waited for the first"
    );
}

/// Regression: `readdir` fetched its listing on the FUSE event-loop thread.
/// Eight walkers in eight slow directories were every thread the mount had,
/// and nothing else on it was answered until one finished.
#[test]
fn reading_a_directory_does_not_wait_for_the_nas() {
    let f = fixture_with(DEFAULT_PREFETCH_BLOCKS, 0);
    mount_dir_listing(&f, 3, Duration::from_secs(2));
    let fh = f.fs.open_dir();

    let started = Instant::now();
    let answer = start(&f, fh, 0);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "start_readdir returned only after {:?}",
        started.elapsed()
    );

    let entries = answer
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(entries.len(), 5);
}

/// A snapshot of a million entries is not something to keep after the
/// directory is closed.
#[test]
fn closing_a_directory_lets_go_of_its_listing() {
    let f = fixture_with(DEFAULT_PREFETCH_BLOCKS, 0);
    mount_dir_listing(&f, 3, Duration::ZERO);
    let fh = f.fs.open_dir();
    readdir(&f, fh, 0);

    f.fs.release_dir(fh);

    readdir(&f, fh, 3);
    assert_eq!(
        listings_asked_for(&f),
        2,
        "nothing was left behind for a closed handle to be served from"
    );
}
