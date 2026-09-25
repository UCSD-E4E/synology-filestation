//! Test support for the client: a mock DSM behind a real `SynologyClient`,
//! and the fixtures every group of tests builds on.

mod download;
mod metadata;
mod session;
mod setup;
mod throttle;
mod transport;
mod truncate;
mod upload;
mod verify;

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a client pointed at the given mock server.
fn client_for(server: &MockServer) -> SynologyClient {
    let uri = server.uri(); // "http://127.0.0.1:PORT"
    let without_scheme = uri.trim_start_matches("http://");
    let (host, port_str) = without_scheme.rsplit_once(':').unwrap();
    let port: u16 = port_str.parse().unwrap();
    SynologyClient::new(host, port, false)
}

/// Build an auto-relogin client pointed at the given mock server.
fn client_auto_for(server: &MockServer) -> SynologyClient {
    let uri = server.uri();
    let without_scheme = uri.trim_start_matches("http://");
    let (host, port_str) = without_scheme.rsplit_once(':').unwrap();
    let port: u16 = port_str.parse().unwrap();
    SynologyClient::with_auto_relogin(host, port, false)
}

// ── shared fixtures ───────────────────────────────────────────────────────

/// Write `len` bytes to a scratch file and return its path.
fn scratch_file(name: &str, len: usize) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("syno-slice-{}-{name}", std::process::id()));
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, data).unwrap();
    path
}

fn header_of(req: &wiremock::Request, name: &str) -> Option<String> {
    req.headers
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
}

fn write_scratch_file(bytes: &[u8]) -> std::path::PathBuf {
    let p = unique_tmp_path("stream-upload-src.bin");
    std::fs::write(&p, bytes).unwrap();
    p
}

enum Behave {
    Ok(&'static [u8]),
    Transient,       // category Transport → fall back
    NotFound,        // definitive → propagate, no fallback
    Exists,          // the create-new case: the name is taken
    CannotCreateNew, // capability gap, not a failure
    Declines,        // cannot serve anything right now: no session yet
}

struct FakeBackend {
    behave: Behave,
    calls: AtomicUsize,
    /// Calls that asked for create-new semantics specifically.
    new_calls: AtomicUsize,
}

impl FakeBackend {
    fn new(behave: Behave) -> Arc<Self> {
        Arc::new(Self {
            behave,
            calls: AtomicUsize::new(0),
            new_calls: AtomicUsize::new(0),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn new_call_count(&self) -> usize {
        self.new_calls.load(Ordering::SeqCst)
    }
    fn outcome_bytes(&self) -> Result<Bytes, SynoFsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behave {
            Behave::Ok(b) => Ok(Bytes::from_static(b)),
            Behave::Transient => Err(SynoFsError::Io("backend down".into())),
            Behave::NotFound => Err(SynoFsError::NotFound),
            Behave::Exists => Err(SynoFsError::AlreadyExists),
            Behave::CannotCreateNew | Behave::Declines => Err(SynoFsError::NotSupported),
        }
    }
    fn outcome_unit(&self) -> Result<(), SynoFsError> {
        self.outcome_bytes().map(|_| ())
    }
}

#[async_trait::async_trait]
impl ReadTransport for FakeBackend {
    async fn read(&self, _p: &str, _o: u64, _l: u64) -> Result<Bytes, SynoFsError> {
        self.outcome_bytes()
    }
}

#[async_trait::async_trait]
impl WriteTransport for FakeBackend {
    async fn write(&self, _p: &str, _d: &[u8]) -> Result<(), SynoFsError> {
        self.outcome_unit()
    }
}

#[async_trait::async_trait]
impl StreamWriteTransport for FakeBackend {
    async fn write_new_from_path(&self, _p: &str, _local: &Path) -> Result<(), SynoFsError> {
        self.new_calls.fetch_add(1, Ordering::SeqCst);
        self.outcome_unit()
    }
    async fn write_from_path(&self, _p: &str, _local: &Path) -> Result<(), SynoFsError> {
        self.outcome_unit()
    }
}

#[async_trait::async_trait]
impl StreamReadTransport for FakeBackend {
    async fn read_to_path(&self, _remote: &str, local: &Path) -> Result<(), SynoFsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behave {
            // A real backend writes the destination; do the same so tests
            // can assert on the file it produced.
            Behave::Ok(b) => {
                std::fs::write(local, b).map_err(|e| SynoFsError::Io(e.to_string()))?;
                Ok(())
            }
            Behave::Transient => Err(SynoFsError::Io("backend down".into())),
            Behave::NotFound => Err(SynoFsError::NotFound),
            // Write-side behaviours; a read never sees them.
            Behave::Exists | Behave::CannotCreateNew | Behave::Declines => {
                Err(SynoFsError::NotSupported)
            }
        }
    }
}

/// A client pointed at a dead address (no HTTP server) — proves a call is
/// served by the backend without ever touching HTTP.
fn offline_client() -> SynologyClient {
    SynologyClient::new("127.0.0.1", 1, false)
}

/// Upload POSTs only — the verification traffic is GETs.
async fn slice_posts(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect()
}

fn unique_tmp_path(name: &str) -> std::path::PathBuf {
    // A counter, not just the clock. `SystemTime` is only nanosecond-*typed*
    // — macOS and Windows tick it in microseconds — so two tests starting in
    // the same tick got the same "unique" path, and whichever finished first
    // deleted the other's file out from under it.
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let mut p = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    p.push(format!("synofs-test-{pid}-{nanos}-{seq}-{name}"));
    p
}

struct FakeMeta {
    behave: Behave,
    calls: AtomicUsize,
}

impl FakeMeta {
    fn new(behave: Behave) -> Arc<Self> {
        Arc::new(Self {
            behave,
            calls: AtomicUsize::new(0),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn entry(&self, name: &str) -> Result<SynoFileInfo, SynoFsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behave {
            Behave::Ok(_) => Ok(SynoFileInfo {
                name: name.to_string(),
                path: format!("/share/{name}"),
                isdir: false,
                additional: None,
                code: None,
            }),
            Behave::Transient => Err(SynoFsError::Io("backend down".into())),
            Behave::NotFound => Err(SynoFsError::NotFound),
            Behave::Exists => Err(SynoFsError::AlreadyExists),
            Behave::CannotCreateNew | Behave::Declines => Err(SynoFsError::NotSupported),
        }
    }
}

#[async_trait::async_trait]
impl MetadataTransport for FakeMeta {
    async fn list_dir(&self, _folder: &str) -> Result<Vec<SynoFileInfo>, SynoFsError> {
        self.entry("from-backend").map(|e| vec![e])
    }
    async fn get_info(&self, _path: &str) -> Result<SynoFileInfo, SynoFsError> {
        self.entry("from-backend")
    }
    async fn delete(&self, _path: &str) -> Result<(), SynoFsError> {
        self.entry("deleted").map(|_| ())
    }
    async fn truncate(&self, _path: &str, _size: u64) -> Result<(), SynoFsError> {
        self.entry("truncated").map(|_| ())
    }
}
