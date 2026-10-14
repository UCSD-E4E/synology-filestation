//! The FileStation HTTP client: construction, the transport table it
//! consults before falling back to HTTP, and the wiring the rest of the
//! module tree hangs off.

use bytes::Bytes;
use reqwest::{multipart, Client};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore, SemaphorePermit};
use tracing::{debug, error, warn};

use crate::error::{dsm_code_to_category, ErrorCategory, SynoFsError};
use crate::transport::{
    BreakerConfig, CircuitBreaker, MetadataTransport, OpenWriteTransport, ReadTransport,
    StreamReadTransport, StreamWriteTransport, WriteHandle, WriteOpen, WriteTransport,
};
/// Offer an operation to each metadata backend in turn before falling back to
/// HTTP, expanding at the head of the method it belongs to.
///
/// A macro rather than a function because the six operations differ in
/// signature and return type, and an `async` closure over `&dyn Trait` buys
/// lifetime grief for no gain. The `return` is the point: a backend that
/// answers *is* the answer.
macro_rules! via_metadata {
    ($self:ident, $method:ident ( $($arg:expr),* )) => {
        for entry in &$self.metadata_transports {
            if !entry.breaker.lock().unwrap().allows(Instant::now()) {
                continue;
            }
            match entry.transport.$method($($arg),*).await {
                Ok(v) => {
                    entry.breaker.lock().unwrap().on_success();
                    return Ok(v);
                }
                // Declining an operation it cannot promise is not a failure.
                Err(e) if e.category() == ErrorCategory::NotSupported => {
                    entry.answered();
                    continue;
                }
                Err(e) if e.category() == ErrorCategory::Transport => {
                    warn!("metadata backend failed (transient), falling back: {e}");
                    entry.breaker.lock().unwrap().on_failure(Instant::now());
                    continue;
                }
                // A reachable backend gave a definitive answer; trust it.
                Err(e) => {
                    entry.breaker.lock().unwrap().on_success();
                    return Err(e);
                }
            }
        }
    };
}

use crate::types::{
    AuthData, CreateFolderData, GetInfoData, ListData, ListShareData, Md5StartData, Md5StatusData,
    RenameData, SynoFileInfo, SynoResponse, UploadData, ADDITIONAL_FIELDS, SHARE_ADDITIONAL_FIELDS,
};

pub use metadata::LIST_PAGE_SIZE;
pub use throttle::ThrottleConfig;
pub use upload::DEFAULT_SLICE_SIZE;

mod download;
mod metadata;
mod moves;
mod session;
mod throttle;
mod upload;
mod verify;

use metadata::METADATA_READ_TIMEOUT;
use session::{StoredCreds, SESSION_AUTH_COOKIE};
use throttle::Throttle;
use upload::UploadDeadline;
#[cfg(test)]
mod tests;

/// One injected backend plus the [`CircuitBreaker`] tracking its health. The
/// breaker is a `std::sync::Mutex` (quick, non-async state mutation, never held
/// across an `.await`).
struct TransportEntry<T: ?Sized> {
    transport: Arc<T>,
    breaker: StdMutex<CircuitBreaker>,
}

impl<T: ?Sized> TransportEntry<T> {
    fn new(transport: Arc<T>) -> Self {
        Self::with_breaker(transport, BreakerConfig::default())
    }

    fn with_breaker(transport: Arc<T>, config: BreakerConfig) -> Self {
        Self {
            transport,
            breaker: StdMutex::new(CircuitBreaker::new(config)),
        }
    }

    /// Record that the backend *answered*, including when the answer was "I
    /// cannot do this".
    ///
    /// Load-bearing on the decline path. A breaker that has just admitted a
    /// half-open probe refuses everything until a verdict is recorded, so
    /// declining without one strands it half-open — and `allows` never returns
    /// true again, disabling the backend for the life of the process over an
    /// operation it merely does not implement.
    fn answered(&self) {
        self.breaker.lock().unwrap().on_success();
    }
}

/// Upload progress sink: `(bytes_done, bytes_total)`, called once per slice.
/// Borrowed rather than boxed so callers can pass a plain closure reference.
pub type ProgressSink<'a> = &'a (dyn Fn(u64, u64) + Send + Sync);

pub struct SynologyClient {
    http: Client,
    /// Client for upload request bodies. Same connection policy as `http` minus
    /// the read timeout, which would otherwise cap how long a body may take to
    /// push — see [`build_http_transfer`].
    http_transfer: Client,
    /// How long a single upload request may take, derived from its size.
    upload_deadline: UploadDeadline,
    base_url: String,
    sid: RwLock<Option<String>>,
    /// Optional request throttle protecting the NAS from bulk-transfer
    /// saturation. `None` = unthrottled (the FUSE/CLI mount path); the Python
    /// and FFI bulk entry points attach one via [`SynologyClient::with_throttle`].
    throttle: Option<Throttle>,
    /// When true, operations that fail with `ApiError(119)` (SID not found)
    /// trigger a transparent re-login + single retry instead of surfacing the
    /// error. Off by default to preserve existing FUSE/WebDAV/WinFsp
    /// behavior; opt in via [`SynologyClient::with_auto_relogin`].
    auto_relogin: bool,
    creds: RwLock<Option<StoredCreds>>,
    /// True when TLS certificate verification has been turned off via
    /// [`SynologyClient::with_insecure_tls`]. Tracked separately from the
    /// `reqwest::Client` (which does not expose its own settings) so consumers
    /// can tell the user which mode they are connecting in.
    insecure_tls: bool,
    /// Whether the session id travels as a cookie or as a `_sid` query
    /// parameter. Settled by a probe immediately after login.
    session_auth: std::sync::atomic::AtomicU8,
    /// Injected read backends, tried in order before the HTTP Download API.
    /// Empty (the default) = HTTP only, i.e. exactly today's behavior.
    read_transports: Vec<TransportEntry<dyn ReadTransport>>,
    /// Injected write backends, tried in order before the HTTP Upload API.
    write_transports: Vec<TransportEntry<dyn WriteTransport>>,
    /// Injected streaming write backends, tried in order by `upload_from_path`
    /// before falling back to reading the file into memory + HTTP upload.
    stream_write_transports: Vec<TransportEntry<dyn StreamWriteTransport>>,
    /// Injected streaming read backends, tried in order by `download_to_path`
    /// before falling back to the buffering HTTP download.
    stream_read_transports: Vec<TransportEntry<dyn StreamReadTransport>>,
    /// Injected metadata backends, tried in order before the HTTP List/Delete/
    /// CreateFolder/Rename APIs.
    metadata_transports: Vec<TransportEntry<dyn MetadataTransport>>,
    /// Injected backends that can write into an open file at offsets. Empty is
    /// the HTTP case, where a write has to be buffered until the whole file is
    /// known.
    open_write_transports: Vec<TransportEntry<dyn OpenWriteTransport>>,
    /// Bytes per slice for the chunked upload path. A file larger than this is
    /// uploaded slice-by-slice so it is never held in memory whole; anything
    /// smaller takes the one-shot path. Default [`DEFAULT_SLICE_SIZE`].
    slice_size: usize,
}

impl std::fmt::Debug for SynologyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SynologyClient")
            .field("base_url", &self.base_url)
            .field("auto_relogin", &self.auto_relogin)
            .field("insecure_tls", &self.insecure_tls)
            .field("throttled", &self.throttle.is_some())
            .field("read_transports", &self.read_transports.len())
            .field("write_transports", &self.write_transports.len())
            .field(
                "stream_write_transports",
                &self.stream_write_transports.len(),
            )
            .field("stream_read_transports", &self.stream_read_transports.len())
            .finish_non_exhaustive()
    }
}

/// Install the ring `CryptoProvider` as the process default exactly once.
///
/// reqwest 0.13 is built with `rustls-no-provider`, so rustls has no compiled-in
/// provider and needs one installed before the first TLS `ClientConfig` is built.
/// We use ring (not aws-lc-rs) to keep the build free of cmake/NASM. `install_default`
/// errors if a provider is already set — by another crate or a second client — which
/// is exactly the idempotent outcome we want, so the result is intentionally ignored.
fn install_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl SynologyClient {
    pub fn new(host: &str, port: u16, https: bool) -> Self {
        install_crypto_provider();
        let scheme = if https { "https" } else { "http" };
        let base_url = format!("{}://{}:{}/webapi", scheme, host, port);
        Self {
            http: build_http(false),
            http_transfer: build_http_transfer(false),
            upload_deadline: UploadDeadline::default(),
            base_url,
            sid: RwLock::new(None),
            auto_relogin: false,
            creds: RwLock::new(None),
            throttle: None,
            insecure_tls: false,
            session_auth: std::sync::atomic::AtomicU8::new(SESSION_AUTH_COOKIE),
            read_transports: Vec::new(),
            write_transports: Vec::new(),
            stream_write_transports: Vec::new(),
            stream_read_transports: Vec::new(),
            metadata_transports: Vec::new(),
            open_write_transports: Vec::new(),
            slice_size: DEFAULT_SLICE_SIZE,
        }
    }

    /// Accept any TLS certificate, including self-signed and expired ones, and
    /// ignore hostname mismatches.
    ///
    /// This turns HTTPS into encryption-without-authentication: anything able to
    /// intercept the connection can present its own certificate and read the
    /// password in the login exchange. It exists because a self-signed
    /// certificate is the out-of-the-box state for a DSM appliance, and is
    /// surfaced to users as an explicit opt-in (`--insecure` on the CLI, a
    /// checkbox in the GUI, `verify_ssl=False` in the Python bindings) rather
    /// than being the silent default it used to be.
    ///
    /// Prefer installing the NAS's certificate in the system trust store.
    pub fn with_insecure_tls(mut self) -> Self {
        self.http = build_http(true);
        self.http_transfer = build_http_transfer(true);
        self.insecure_tls = true;
        self
    }

    /// Shrink the metadata read timeout so a test can prove uploads are not
    /// governed by it without waiting out the real 30 s.
    #[cfg(test)]
    fn with_read_timeout_for_test(mut self, d: Duration) -> Self {
        self.http = build_client(self.insecure_tls, Some(d));
        self
    }

    /// Shrink the per-upload deadline so a test can watch it fire.
    #[cfg(test)]
    fn with_upload_deadline_for_test(mut self, grace: Duration, floor_bps: u64) -> Self {
        self.upload_deadline = UploadDeadline { grace, floor_bps };
        self
    }

    /// Whether TLS certificate verification has been disabled on this client.
    /// Consumers use it to tell the user which mode they are connecting in.
    pub fn insecure_tls(&self) -> bool {
        self.insecure_tls
    }
}

/// Build the metadata/download HTTP client — everything except upload request
/// bodies, which get [`build_http_transfer`] instead.
fn build_http(accept_invalid_certs: bool) -> Client {
    build_client(accept_invalid_certs, Some(METADATA_READ_TIMEOUT))
}

/// Build the client used for upload request bodies: the same connection policy,
/// but with **no** `read_timeout`.
///
/// reqwest's read timeout is not the idle timer the name suggests. It is a
/// `sleep` armed when the request is created and polled alongside the pending
/// request, so it caps the whole span from "request started" to "response
/// headers arrived" — the time spent writing the request body included. On a
/// 30 s cap, any upload whose body takes longer than 30 s to push fails with
/// `operation timed out`, which on a slow link is every large file: 10 MiB
/// needs more than 30 s below ~350 KB/s. Uploads are bounded per request by
/// [`UploadDeadline`] instead, which knows how many bytes are in flight.
fn build_http_transfer(accept_invalid_certs: bool) -> Client {
    build_client(accept_invalid_certs, None)
}

/// Build an HTTP client. `accept_invalid_certs` and `read_timeout` are the only
/// knobs that vary; everything else is the timeout/keepalive policy the whole
/// crate depends on, so it lives in one place rather than being repeated per
/// constructor.
fn build_client(accept_invalid_certs: bool, read_timeout: Option<Duration>) -> Client {
    let builder = Client::builder()
        .danger_accept_invalid_certs(accept_invalid_certs)
        // Drop idle connections after 4 s so we don't reuse connections the NAS
        // has already closed on its side (~7 s keep-alive on most DSM versions).
        .pool_idle_timeout(Duration::from_secs(4))
        // Fail fast if the NAS is unreachable rather than waiting for the OS-level
        // TCP timeout (~75 s on macOS, ETIMEDOUT / os error 60).
        .connect_timeout(Duration::from_secs(10))
        // Send TCP keepalive probes so stalled mid-transfer connections are
        // detected in seconds rather than waiting for the full OS TCP timeout
        // (~75 s on macOS, ETIMEDOUT / os error 60).
        .tcp_keepalive(Duration::from_secs(10));
    // Bound how long we'll wait for a response. Without this, a silently-dead
    // connection (e.g. routes changed when a VPN comes up mid-session) hangs
    // the FUSE callback indefinitely — the user sees their file manager freeze
    // with no error. Requests that carry a large body opt out (see
    // `build_http_transfer`) because this cap covers the body write too.
    let builder = match read_timeout {
        Some(d) => builder.read_timeout(d),
        None => builder,
    };
    builder.build().expect("failed to build HTTP client")
}

impl SynologyClient {
    /// Inject a [`ReadTransport`] backend (e.g. SMB). `download` will prefer it
    /// over the HTTP Download API when its circuit breaker is closed, falling
    /// back to HTTP (and any later backends) on transport failures. Call more
    /// than once to register several backends; they are tried in the order
    /// added, with HTTP last.
    ///
    /// Read/write call sites never change — this is the only place a consumer
    /// opts a backend in.
    pub fn with_read_transport(mut self, transport: Arc<dyn ReadTransport>) -> Self {
        self.read_transports.push(TransportEntry::new(transport));
        self
    }

    /// Inject a [`WriteTransport`] backend. `upload` will prefer it over the
    /// HTTP Upload API when healthy, falling back to HTTP on transport failures.
    /// The backend's `write` must be atomic so fallback is safe.
    pub fn with_write_transport(mut self, transport: Arc<dyn WriteTransport>) -> Self {
        self.write_transports.push(TransportEntry::new(transport));
        self
    }

    /// Inject a [`StreamWriteTransport`] backend. `upload_from_path` will stream
    /// the local file straight to it when healthy, falling back to reading the
    /// file into memory + HTTP upload on transport failures.
    pub fn with_stream_write_transport(mut self, transport: Arc<dyn StreamWriteTransport>) -> Self {
        self.stream_write_transports
            .push(TransportEntry::new(transport));
        self
    }

    /// Inject a [`StreamReadTransport`] backend. `download_to_path` will stream
    /// straight to disk through it when healthy, falling back to the buffering
    /// HTTP download on transport failures.
    pub fn with_stream_read_transport(mut self, transport: Arc<dyn StreamReadTransport>) -> Self {
        self.stream_read_transports
            .push(TransportEntry::new(transport));
        self
    }

    /// Inject a [`MetadataTransport`] backend. Listings, `get_info`, and the
    /// namespace mutations prefer it over the HTTP FileStation APIs, falling
    /// back to HTTP when it is unhealthy or declines an operation.
    pub fn with_metadata_transport(mut self, transport: Arc<dyn MetadataTransport>) -> Self {
        self.metadata_transports
            .push(TransportEntry::new(transport));
        self
    }

    /// Register a metadata backend with a breaker tuned for a test, so
    /// half-open states are reachable without waiting out a real cooldown.
    #[cfg(test)]
    fn with_metadata_transport_breaker(
        mut self,
        transport: Arc<dyn MetadataTransport>,
        config: BreakerConfig,
    ) -> Self {
        self.metadata_transports
            .push(TransportEntry::with_breaker(transport, config));
        self
    }

    /// Register a streaming write backend with a test-tuned breaker.
    #[cfg(test)]
    fn with_stream_write_transport_breaker(
        mut self,
        transport: Arc<dyn StreamWriteTransport>,
        config: BreakerConfig,
    ) -> Self {
        self.stream_write_transports
            .push(TransportEntry::with_breaker(transport, config));
        self
    }

    /// Register an open-write backend with a test-tuned breaker.
    #[cfg(test)]
    fn with_open_write_transport_breaker(
        mut self,
        transport: Arc<dyn OpenWriteTransport>,
        config: BreakerConfig,
    ) -> Self {
        self.open_write_transports
            .push(TransportEntry::with_breaker(transport, config));
        self
    }

    /// Inject an [`OpenWriteTransport`] backend, letting `open_write` hand out
    /// streaming write handles instead of buffering whole files.
    pub fn with_open_write_transport(mut self, transport: Arc<dyn OpenWriteTransport>) -> Self {
        self.open_write_transports
            .push(TransportEntry::new(transport));
        self
    }

    /// Open `path` for writing through a backend that can address offsets.
    ///
    /// `Ok(None)` is the ordinary answer when nothing can: the caller buffers
    /// the writes and uploads the whole file, which is all the HTTP API
    /// supports. An `Err` means a backend had a real answer — an existing name
    /// under [`WriteOpen::CreateNew`], say — and that answer stands rather than
    /// being retried down a path that would reach the same conclusion.
    pub async fn open_write(
        &self,
        path: &str,
        mode: WriteOpen,
    ) -> Result<Option<Box<dyn WriteHandle>>, SynoFsError> {
        for entry in &self.open_write_transports {
            if !entry.breaker.lock().unwrap().allows(Instant::now()) {
                continue;
            }
            match entry.transport.open_write(path, mode).await {
                Ok(handle) => {
                    entry.breaker.lock().unwrap().on_success();
                    return Ok(Some(handle));
                }
                // Cannot stream. Not a failure, and not this backend's fault.
                Err(e) if e.category() == ErrorCategory::NotSupported => {
                    entry.answered();
                    continue;
                }
                Err(e) if e.category() == ErrorCategory::Transport => {
                    warn!("open-write backend failed (transient), buffering instead: {e}");
                    entry.breaker.lock().unwrap().on_failure(Instant::now());
                    continue;
                }
                Err(e) => {
                    entry.breaker.lock().unwrap().on_success();
                    return Err(e);
                }
            }
        }
        Ok(None)
    }
}
