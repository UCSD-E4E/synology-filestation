//! Test support shared by the backend's test modules: a mock DSM behind a real
//! `SynologyClient`, and the seeding helpers the write tests build on.

mod attr;
mod dirs;
mod read;
mod write;
mod xattr;

use super::attr::*;
use super::transfer::*;
use super::*;
use std::collections::HashMap as Map;

use crate::DEFAULT_PREFETCH_BLOCKS;
use synology_filestation_core::types::{SynoAdditional, SynoOwner, SynoPerm};
use wiremock::matchers::{method as http_method, path as http_path, query_param};
use wiremock::{Mock, MockServer, Request as WmRequest, Respond, ResponseTemplate};

/// Small blocks keep the fixtures readable; the assembly rules under test
/// don't depend on the real 256 KiB size.
const BLOCK: u64 = 1024;

fn client_for(server: &MockServer) -> SynologyClient {
    let uri = server.uri();
    let (host, port) = uri
        .trim_start_matches("http://")
        .rsplit_once(':')
        .expect("mock server uri has a port");
    SynologyClient::new(host, port.parse().unwrap(), false)
}

/// Field order matters: `rt` is declared last so it is dropped last, after
/// the mock server has had a runtime to shut itself down on.
struct Fixture {
    fs: SynologyFS,
    server: MockServer,
    rt: tokio::runtime::Runtime,
}

fn fixture() -> Fixture {
    fixture_with_prefetch(DEFAULT_PREFETCH_BLOCKS)
}

fn fixture_with_prefetch(depth: u64) -> Fixture {
    fixture_with(depth, 30)
}

/// `dir_ttl` is `--cache-ttl` for listings; `0` switches their cache off.
fn fixture_with(depth: u64, dir_ttl: u64) -> Fixture {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(MockServer::start());
    let fs = SynologyFS::new(
        Arc::new(client_for(&server)),
        Arc::new(InodeCache::new(30)),
        Arc::new(DirCache::new(dir_ttl)),
        Arc::new(ReadCache::new(BLOCK, 64)),
        rt.handle().clone(),
        Ownership {
            uid: 1000,
            gid: 1000,
            umask: 0o022,
        },
        depth,
    );
    Fixture { fs, server, rt }
}

/// Serves file bytes the way DSM's Download API does: honours `Range`, and
/// answers 416 past EOF. `short` caps how many bytes a given range *start*
/// may return, which is how a test reproduces a truncated response — a
/// successful HTTP 200 carrying fewer bytes than the range asked for.
struct RangeFile {
    body: Vec<u8>,
    short: Map<u64, usize>,
    /// Held open this long, so a test can count what is in flight at once.
    delay: Option<Duration>,
}

impl RangeFile {
    fn delayed(&self, t: ResponseTemplate) -> ResponseTemplate {
        match self.delay {
            Some(d) => t.set_delay(d),
            None => t,
        }
    }
}

impl Respond for RangeFile {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        if self.body.is_empty() {
            return self.delayed(ResponseTemplate::new(200).set_body_bytes(Vec::new()));
        }
        let (start, end) = match req.headers.get("range").and_then(|v| v.to_str().ok()) {
            Some(r) => {
                let (s, e) = r
                    .trim_start_matches("bytes=")
                    .split_once('-')
                    .expect("range header is bytes=S-E");
                (
                    s.parse::<u64>().unwrap(),
                    e.parse::<u64>().unwrap_or(u64::MAX),
                )
            }
            None => (0, self.body.len() as u64 - 1),
        };
        if start as usize >= self.body.len() {
            return self.delayed(ResponseTemplate::new(416));
        }
        let end = (end as usize).min(self.body.len() - 1);
        let mut slice = self.body[start as usize..=end].to_vec();
        if let Some(&cap) = self.short.get(&start) {
            slice.truncate(cap);
        }
        self.delayed(ResponseTemplate::new(206).set_body_bytes(slice))
    }
}

fn mount_download(f: &Fixture, body: Vec<u8>, short: Map<u64, usize>) {
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(http_path("/webapi/entry.cgi"))
            .and(query_param("method", "download"))
            .respond_with(RangeFile {
                body,
                short,
                delay: None,
            })
            .mount(&f.server),
    );
}

fn mount_upload_ok(f: &Fixture) {
    f.rt.block_on(
        Mock::given(http_method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"success": true, "data": {"blks": null}})),
            )
            .mount(&f.server),
    );
}

fn mount_delete_ok(f: &Fixture) {
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(query_param("method", "delete"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true})),
            )
            .mount(&f.server),
    );
}

/// `clear_for_overwrite` polls getinfo until the file is gone; answering
/// "no such file" lets the upload proceed on the first poll.
fn mount_getinfo_gone(f: &Fixture) {
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(query_param("method", "getinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"success": false, "error": {"code": 414}})),
            )
            .mount(&f.server),
    );
}

fn ramp(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn posted_bodies(f: &Fixture) -> Vec<Vec<u8>> {
    f.rt.block_on(f.server.received_requests())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| r.body)
        .collect()
}

fn seed_dirty_buffer(f: &Fixture, nas_path: &str, data: &[u8]) -> u64 {
    seed_dirty_buffer_fh(f, 1, nas_path, data)
}

/// Same, but for the handle of the caller's choosing — the concurrency
/// tests need two live buffers at once.
fn seed_dirty_buffer_fh(f: &Fixture, fh: u64, nas_path: &str, data: &[u8]) -> u64 {
    let ino = f.fs.cache.get_or_alloc_ino(nas_path);
    let mut buf = SpillBuffer::new();
    buf.write_at(0, data).unwrap();
    f.fs.write_buffers.lock().unwrap().insert(
        fh,
        OpenHandle::new(WriteBuffer {
            nas_path: nas_path.to_string(),
            ino,
            sink: WriteSink::Buffered(buf),
            streamed: false,
            dirty: true,
            new_file: true,
            broken: false,
        }),
    );
    fh
}

/// A download that stays on the wire long enough for a test to count what is
/// in flight at one moment.
fn mount_download_slow(f: &Fixture, body: Vec<u8>, delay: Duration) {
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(http_path("/webapi/entry.cgi"))
            .and(query_param("method", "download"))
            .respond_with(RangeFile {
                body,
                short: Map::new(),
                delay: Some(delay),
            })
            .mount(&f.server),
    );
}

/// Put block 0 in the read cache, so `prime_open` can sniff it without the
/// synchronous download first. A test that measures what is in flight at one
/// moment must not serialise on the one fetch it does not care about.
fn seed_block0(f: &Fixture, ino: u64, magic: &[u8]) {
    let mut block = ramp(BLOCK as usize);
    block[..magic.len()].copy_from_slice(magic);
    f.fs.read_cache.insert(ino, 0, bytes::Bytes::from(block));
}

/// Wait for a condition, briefly. `JoinHandle::abort` is asynchronous: the
/// cancelled task's cleanup lands shortly after the call, not during it.
fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}
