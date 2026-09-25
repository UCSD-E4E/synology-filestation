//! [`SmbTransport`] — an in-process SMB read path for NAS files.
//!
//! Reads bytes over SMB3 (pure Rust, no OS mount, no privileges,
//! cross-platform) so bulk staging bypasses the FileStation HTTP Download API
//! and its shared `synoscgi` backend. This crate is the transport only; a
//! future selection layer (in core / the Python bindings) will prefer it and
//! fall back to the throttled HTTP path when SMB is unavailable.
//!
//! All ops on one transport share a single SMB connection and are serialized
//! behind a mutex — correct and simple. Concurrency across files would use a
//! connection pool (future work).

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use smb2::{ClientConfig, FileReader, RenameOptions, SmbClient, Tree};
use synology_filestation_core::{
    MetadataTransport, OpenWriteTransport, ReadTransport, StreamReadTransport,
    StreamWriteTransport, SynoAdditional, SynoFileInfo, SynoFsError, SynoTime, SynologyClient,
    WriteHandle, WriteOpen, WriteTransport,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::error::to_syno_error;
use crate::handles::{HandleCache, MAX_CACHED_HANDLES};
use crate::path::SmbPath;

/// Process-wide sequence making temp write names unique so concurrent writers
/// to the same target don't clobber each other's `.part` file.
static PART_SEQ: AtomicU64 = AtomicU64::new(0);

/// Name of the adjacent temp file an atomic write goes to before the rename.
fn part_name(path: &str, seq: u64) -> String {
    format!("{path}.part-{}-{}", std::process::id(), seq)
}

/// Whether an `smb2` error means the connection itself is dead (so the transport
/// should reconnect before the next operation), vs. a per-request error the live
/// connection can keep serving.
fn is_connection_lost(kind: smb2::ErrorKind) -> bool {
    matches!(
        kind,
        smb2::ErrorKind::ConnectionLost
            | smb2::ErrorKind::TimedOut
            | smb2::ErrorKind::SessionExpired
    )
}

/// Tracks whether the SMB link needs re-establishing. Isolated from `SmbClient`
/// so the reconnect decision is unit-testable without a live connection.
#[derive(Debug)]
struct ReconnectState {
    needs: AtomicBool,
    /// Whether reconnecting is something this transport can do at all.
    ///
    /// False for a transport built on a stream the caller supplied: only they
    /// know how the stream was made. Without this the flag below is a one-way
    /// door — a single transient `TimedOut` sets it, the reconnect that would
    /// clear it is refused because there is nothing to redial, and
    /// `reconnect_if_needed` re-sets it on that refusal. Every later operation
    /// then fails on a session that was never actually dead.
    possible: bool,
    /// Why the server turned the credentials down, once it has.
    ///
    /// A dropped link is worth retrying on the next operation; a rejected
    /// login is not. Every attempt is a real authentication, DSM blocks an
    /// address after three failures, and that block does not expire — so once
    /// this is set nothing here builds a session again. A remount is what
    /// clears it, because that is when the credentials can have changed.
    refused: std::sync::Mutex<Option<String>>,
    /// Whether a session has ever been built. Kept here rather than read off
    /// the session slot so that asking never waits on the lock an operation
    /// holds.
    has_session: AtomicBool,
}

impl Default for ReconnectState {
    fn default() -> Self {
        Self {
            needs: AtomicBool::new(false),
            possible: true,
            refused: std::sync::Mutex::new(None),
            has_session: AtomicBool::new(true),
        }
    }
}

impl ReconnectState {
    /// For a transport running on a stream the caller supplied: it can come
    /// back exactly when the caller left a way to reopen that stream.
    ///
    /// Without one it can never come back, which is what a tunnelled mount
    /// used to get unconditionally — and why a single stream EOF put it on the
    /// HTTP fallback for the rest of the session.
    fn for_supplied_stream(can_redial: bool) -> Self {
        Self {
            possible: can_redial,
            ..Self::default()
        }
    }

    /// Why the credentials were refused, if they have been.
    fn refused(&self) -> Option<String> {
        self.refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Remember that the credentials were refused, and say so once.
    fn refuse(&self, why: String) {
        tracing::warn!(
            "SMB: the server refused the login ({why}); not trying again until the \
             share is reconnected. An AD account needs its domain set."
        );
        *self.refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
    }

    /// For a transport that starts with no session at all.
    fn without_a_session() -> Self {
        Self {
            has_session: AtomicBool::new(false),
            ..Self::default()
        }
    }

    /// A fresh session is in place; nothing is owed.
    fn settled(&self) {
        self.has_session.store(true, Ordering::SeqCst);
        self.needs.store(false, Ordering::SeqCst);
    }

    /// A session exists and nothing has found it dead.
    fn is_live(&self) -> bool {
        self.has_session.load(Ordering::SeqCst) && !self.needs.load(Ordering::SeqCst)
    }

    /// Flag for reconnect if `kind` means the link is dead.
    fn flag_if_lost(&self, kind: smb2::ErrorKind) {
        if self.possible && is_connection_lost(kind) {
            self.needs.store(true, Ordering::SeqCst);
        }
    }

    /// If flagged, run `reconnect` (clearing the flag first). Returns `Ok(true)`
    /// when a reconnect ran and succeeded — the caller then drops its stale tree
    /// cache. On failure the flag is re-set so the next operation retries, and
    /// the error is returned. `Ok(false)` means no reconnect was needed.
    async fn reconnect_if_needed<F, Fut>(&self, reconnect: F) -> Result<bool, smb2::Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), smb2::Error>>,
    {
        if !self.possible || !self.needs.swap(false, Ordering::SeqCst) {
            return Ok(false);
        }
        if let Some(why) = self.refused() {
            self.needs.store(true, Ordering::SeqCst);
            return Err(smb2::Error::Auth { message: why });
        }
        match reconnect().await {
            Ok(()) => Ok(true),
            Err(e) => {
                if is_refusal(&e) {
                    self.refuse(e.to_string());
                }
                self.needs.store(true, Ordering::SeqCst);
                Err(e)
            }
        }
    }
}

/// Whether building a session failed because the server turned the
/// credentials down, rather than because the link did.
fn is_refusal(e: &smb2::Error) -> bool {
    to_syno_error(e).category() == synology_filestation_core::error::ErrorCategory::PermissionDenied
}

/// What an operation gets from a transport that has no session to serve it
/// on: not this backend, not now.
///
/// `NotSupported` rather than a transport failure, because nothing failed. A
/// transport attached before SMB was reachable — or after its login was
/// refused — is doing exactly what it should by standing aside, and a
/// failure would trip its breaker and log a warning on every call for an
/// outcome everyone expected. The caller goes to HTTP either way.
fn declined() -> SynoFsError {
    SynoFsError::NotSupported
}

/// Map a **local** filesystem error to a *definitive* [`SynoFsError`] (never
/// `Transport`).
///
/// A local failure — disk full, unwritable destination, missing source — can't
/// be fixed by falling back to another transport (HTTP would hit the same local
/// I/O, after buffering the whole file in memory first), so the selection layer
/// must not retry it elsewhere. The detail is logged since the definitive
/// variants carry no message.
fn local_fs_error(what: &str, e: &std::io::Error) -> SynoFsError {
    use std::io::ErrorKind;
    tracing::warn!(error = %e, "{what}");
    match e.kind() {
        ErrorKind::PermissionDenied => SynoFsError::PermissionDenied,
        ErrorKind::NotFound => SynoFsError::NotFound,
        ErrorKind::AlreadyExists => SynoFsError::AlreadyExists,
        // ENOSPC has no stable ErrorKind; detect via the raw errno (28 on unix).
        _ if e.raw_os_error() == Some(28) => SynoFsError::NoSpace,
        // Any other local FS failure is still definitive — don't fall back.
        _ => SynoFsError::InvalidArg,
    }
}

/// A byte stream SMB can run on, named so a reopened one can be handed back
/// as a value rather than as a type parameter.
pub trait SmbStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> SmbStream for T {}

/// A stream a caller reopened for us.
pub type BoxedStream = Box<dyn SmbStream>;

/// What [`SmbTransport::over_with_redial`]'s closure returns: the work of
/// reopening the stream, which for a tunnelled connection may mean bringing
/// the tunnel back up first.
pub type RedialFuture =
    Pin<Box<dyn Future<Output = Result<BoxedStream, SynoFsError>> + Send + 'static>>;

/// How to reopen the stream this transport runs on. What to authenticate on
/// the new one is the transport's own config, which follows the session: a
/// transport handed a session on another leg names that leg's server from
/// then on.
#[derive(Clone)]
struct Redialer {
    open: Arc<dyn Fn() -> RedialFuture + Send + Sync>,
}

/// Negotiate and authenticate an SMB session on `stream`.
///
/// Shared by the first connection and every reconnection after it, so a
/// rebuilt session is built the way the working one was rather than by a
/// second implementation that has never been run.
async fn client_over<S>(stream: S, cfg: &SmbConfig) -> Result<SmbClient, SynoFsError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let framed = Arc::new(crate::framing::StreamTransport::new(stream));
    // The host without its port: `Connection` names the server, and the
    // port is not part of a server's name.
    let server = cfg.host.split(':').next().unwrap_or(&cfg.host).to_string();

    let conn = smb2::client::connection::Connection::from_transport(
        Box::new(Arc::clone(&framed)),
        Box::new(framed),
        server,
    );

    SmbClient::from_connection(
        ClientConfig {
            addr: cfg.addr(),
            timeout: cfg.timeout,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            domain: cfg.domain.clone(),
            // Not ours to arm: `SmbClient` would dial `addr` behind the
            // caller's back, and for a tunnelled connection that address is
            // reachable only from inside the tunnel.
            auto_reconnect: false,
            compression: true,
            dfs_enabled: true,
            dfs_target_overrides: HashMap::new(),
        },
        conn,
    )
    .await
    .map_err(|e| to_syno_error(&e))
}

/// Carry a redial failure back as an `smb2::Error` so it flows through the
/// same reconnect bookkeeping as any other.
///
/// `Io` rather than a connection-lost kind on purpose: the flag is re-set by
/// `reconnect_if_needed` itself, and classifying this as a link failure would
/// only re-flag what is already flagged. What it does buy is the reason —
/// "reopening the stream: …" says the tunnel would not come back, where a
/// bare `Disconnected` said only that the old one had gone.
///
/// Except for a refused login, which stays one. Folded into `Io` it looked
/// like every other failure to reopen, and was retried on the next operation
/// like one — against an appliance that blocks an address for good after
/// three.
fn redial_failed(e: SynoFsError) -> smb2::Error {
    let message = format!("reopening the stream: {e}");
    match e.category() {
        synology_filestation_core::error::ErrorCategory::PermissionDenied => {
            smb2::Error::Auth { message }
        }
        _ => smb2::Error::Io(std::io::Error::other(message)),
    }
}

/// Connection parameters for [`SmbTransport::connect`].
#[derive(Clone)]
pub struct SmbConfig {
    /// NAS host, either `host` or `host:port`. If no port is present, [`port`]
    /// is appended.
    ///
    /// [`port`]: Self::port
    pub host: String,
    /// Port used when `host` carries none. SMB is 445.
    pub port: u16,
    /// Account name (the sAMAccountName for AD users, e.g. `c.crutchfield.642`).
    pub username: String,
    /// Password (kept in memory by the SMB client for the session lifetime).
    pub password: String,
    /// NetBIOS domain for AD accounts (e.g. `KRG`); empty for local DSM users.
    pub domain: String,
    /// Connection/negotiate timeout.
    pub timeout: Duration,
}

/// Hand-written so the password never reaches a log. This is a public type
/// with a public `password` field, so a derived `Debug` would put a plaintext
/// credential one `{:?}` away for every downstream consumer, not just for us.
impl std::fmt::Debug for SmbConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmbConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl SmbConfig {
    /// A config with the SMB defaults (port 445, 10 s timeout, no domain).
    pub fn new(
        host: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port: 445,
            username: username.into(),
            password: password.into(),
            domain: String::new(),
            timeout: Duration::from_secs(10),
        }
    }

    /// Build an auto-SMB config from the same credentials the HTTP login uses,
    /// parsing the domain out of the username so AD accounts work with no extra
    /// input: `DOMAIN\user`, or `user@REALM` (NetBIOS domain = the first realm
    /// label, upper-cased — e.g. `KRG.LOCAL` → `KRG`), else a local account with
    /// no domain. Port defaults to 445.
    pub fn from_login(host: &str, username: &str, password: &str) -> Self {
        let (domain, user) = if let Some((d, u)) = username.split_once('\\') {
            (d.to_string(), u.to_string())
        } else if let Some((u, realm)) = username.split_once('@') {
            let netbios = realm.split('.').next().unwrap_or(realm).to_uppercase();
            (netbios, u.to_string())
        } else {
            (String::new(), username.to_string())
        };
        let mut cfg = SmbConfig::new(host, user, password);
        cfg.domain = domain;
        cfg
    }

    /// The config for an account, as [`auto_connect_as`] would build it:
    /// [`from_login`](Self::from_login), then the domain the caller chose,
    /// then the deploy overrides in the environment (see [`auto_connect`]).
    ///
    /// For a caller that builds its transport itself — one attached before
    /// SMB is reachable, say — and wants it authenticated exactly as the
    /// automatic path would authenticate it.
    ///
    /// An explicit domain wins over `SYNOLOGY_FS_SMB_DOMAIN`, which wins over
    /// the one parsed from the username. `Some("")` is an explicit answer, not
    /// an absent one: an empty domain is how a local DSM user is named, so it
    /// has to be able to override the environment back to none.
    pub fn for_account(host: &str, username: &str, password: &str, domain: Option<&str>) -> Self {
        let mut cfg = SmbConfig::from_login(host, username, password);
        match domain {
            Some(domain) => cfg.domain = domain.to_string(),
            None => {
                if let Ok(domain) = std::env::var("SYNOLOGY_FS_SMB_DOMAIN") {
                    cfg.domain = domain;
                }
            }
        }
        if let Some(port) = std::env::var("SYNOLOGY_FS_SMB_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            cfg.port = port;
        }
        cfg.timeout = Duration::from_millis(
            std::env::var("SYNOLOGY_FS_SMB_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2000),
        );
        cfg
    }

    /// The address this config names, port included.
    ///
    /// Public because a caller writing a redial closure for
    /// [`SmbTransport::over_with_redial`] needs the same address the transport
    /// was built for, and reassembling `host` and `port` by hand is how the
    /// two come to disagree.
    pub fn addr(&self) -> String {
        if self.host.contains(':') {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Transparently connect an SMB backend from the HTTP login credentials, for the
/// "always prefer SMB when reachable" policy — so bulk transfers avoid the HTTP
/// Download/Upload API (and the shared `synoscgi` backend it can saturate).
///
/// Returns `None` (→ HTTP only) whenever SMB is disabled, unreachable, or auth
/// fails — **never** an error, so a failed probe silently degrades to HTTP. A
/// short probe timeout bounds the cost off-network (where port 445 is dropped),
/// and the caller's circuit breaker takes over from there.
///
/// Deploy escape hatches (environment, invisible to any public API):
/// * `SYNOLOGY_FS_SMB_DISABLE` (any value) — never use SMB.
/// * `SYNOLOGY_FS_SMB_DOMAIN` — override the domain parsed from the username.
/// * `SYNOLOGY_FS_SMB_PORT` — override the SMB port (default 445).
/// * `SYNOLOGY_FS_SMB_TIMEOUT_MS` — probe timeout (default 2000).
pub async fn auto_connect(host: &str, username: &str, password: &str) -> Option<Arc<SmbTransport>> {
    auto_connect_as(host, username, password, None).await
}

/// [`auto_connect`] with the domain chosen by the caller.
///
/// An explicit domain wins over `SYNOLOGY_FS_SMB_DOMAIN`, which wins over
/// none. The environment variable predates the flag and stays supported: it is
/// how existing mounts are configured, and breaking them to tidy a precedence
/// list would be a poor trade.
pub async fn auto_connect_as(
    host: &str,
    username: &str,
    password: &str,
    domain: Option<&str>,
) -> Option<Arc<SmbTransport>> {
    if std::env::var_os("SYNOLOGY_FS_SMB_DISABLE").is_some() {
        return None;
    }
    let cfg = SmbConfig::for_account(host, username, password, domain);

    match SmbTransport::connect(&cfg).await {
        Ok(transport) => {
            tracing::info!(host, "SMB transport enabled; preferring it over HTTP");
            Some(Arc::new(transport))
        }
        Err(e) => {
            tracing::debug!(host, error = %e, "SMB unavailable; using HTTP only");
            None
        }
    }
}

/// [`auto_connect`] + attach: connect SMB from the login credentials and inject
/// it into `client` as the preferred backend for **reads, writes, and streaming
/// transfers**. Returns the client unchanged when SMB is unavailable.
///
/// This one call is the entire consumer integration — a consumer never lists the
/// individual backends, so adding a future backend here never touches consumers.
pub async fn auto_attach(
    client: SynologyClient,
    host: &str,
    username: &str,
    password: &str,
) -> SynologyClient {
    auto_attach_as(client, host, username, password, None).await
}

/// [`auto_attach`] with the SMB host and domain chosen by the caller.
///
/// The host is separate from the one used for HTTP because inside the tunnel
/// they differ: the NAS answers SMB at a private address its public name does
/// not resolve to.
pub async fn auto_attach_as(
    client: SynologyClient,
    host: &str,
    username: &str,
    password: &str,
    domain: Option<&str>,
) -> SynologyClient {
    match auto_connect_as(host, username, password, domain).await {
        Some(smb) => attach(client, smb),
        None => client,
    }
}

/// Put an SMB transport the caller built in front of the HTTP one.
///
/// Separate from [`auto_attach_as`] because how the SMB link was reached is
/// not this function's business: dialled directly, or opened through a tunnel
/// with [`SmbTransport::over`], it serves the same operations once it exists.
pub fn attach(client: SynologyClient, smb: Arc<SmbTransport>) -> SynologyClient {
    client
        .with_read_transport(smb.clone())
        .with_write_transport(smb.clone())
        .with_stream_write_transport(smb.clone())
        .with_stream_read_transport(smb.clone())
        .with_metadata_transport(smb.clone())
        .with_open_write_transport(smb)
}

/// Minimal file metadata — enough for the verify-once integrity check that
/// compares SMB against the FileStation API's `getinfo` before trusting SMB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub is_directory: bool,
}

struct Inner {
    /// `None` until a session has been handed over, for a transport attached
    /// before SMB could be reached. See [`SmbTransport::unconnected`].
    client: Option<SmbClient>,
    /// One attached `Tree` per share name, connected lazily on first use.
    trees: HashMap<String, Tree>,
}

impl Inner {
    fn with(client: SmbClient) -> Self {
        Self {
            client: Some(client),
            trees: HashMap::new(),
        }
    }
}

/// An authenticated SMB connection that reads NAS files.
pub struct SmbTransport {
    inner: Arc<Mutex<Inner>>,
    /// What the session is authenticated with and which server it names.
    /// Follows the session: [`adopt`](Self::adopt) on another leg updates the
    /// host, and a later redial rebuilds on the host the session is on.
    cfg: Arc<std::sync::Mutex<SmbConfig>>,
    /// Open read handles, so a block read is one round trip rather than a
    /// CREATE, a READ and a CLOSE. See [`crate::handles`].
    handles: Arc<HandleCache<FileReader>>,
    /// Tracks whether the SMB link needs re-establishing. `smb2` doesn't
    /// auto-reconnect, so without this a mount would degrade to HTTP permanently
    /// after one network flap.
    reconnect: Arc<ReconnectState>,
    /// How to reopen the stream underneath, for a transport that runs on one
    /// the caller supplied. `None` for a transport that dialled its own
    /// address — `SmbClient::reconnect` redials that itself.
    redial: Option<Redialer>,
}

impl SmbTransport {
    /// Connect + authenticate (SMB3 negotiate, NTLMv2, signing).
    pub async fn connect(cfg: &SmbConfig) -> Result<Self, SynoFsError> {
        let client = SmbClient::connect(ClientConfig {
            addr: cfg.addr(),
            timeout: cfg.timeout,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            domain: cfg.domain.clone(),
            auto_reconnect: false,
            compression: true,
            dfs_enabled: true,
            dfs_target_overrides: HashMap::new(),
        })
        .await
        .map_err(|e| to_syno_error(&e))?;
        // This one dialled its own address, so `SmbClient` can redial it.
        Ok(Self::built(
            Inner::with(client),
            cfg,
            ReconnectState::default(),
            None,
        ))
    }

    /// A transport with no session yet, for a mount that came up while SMB
    /// could not be reached.
    ///
    /// Attached anyway, so that there is somewhere for a session to go once
    /// SMB answers: the client's transport list is fixed when it is built, and
    /// a mount attached with HTTP alone stayed on HTTP until it was remounted.
    /// Until [`adopt`](Self::adopt) hands it one, every operation is declined
    /// at once — no dial, no wait — so the call goes on to HTTP exactly as if
    /// nothing were attached.
    ///
    /// It never dials on its own. Deciding *when* SMB is worth trying again
    /// belongs to whoever watches the legs, not to whichever file operation
    /// happened to arrive; one that did would hold the transport's lock
    /// through a probe, or through raising a tunnel.
    ///
    /// `reopen` is what [`over_with_redial`](Self::over_with_redial) takes,
    /// and serves the same purpose once a session exists.
    pub fn unconnected<F>(cfg: &SmbConfig, reopen: F) -> Self
    where
        F: Fn() -> RedialFuture + Send + Sync + 'static,
    {
        Self::built(
            Inner {
                client: None,
                trees: HashMap::new(),
            },
            cfg,
            ReconnectState::without_a_session(),
            Some(Redialer {
                open: Arc::new(reopen),
            }),
        )
    }

    fn built(
        inner: Inner,
        cfg: &SmbConfig,
        reconnect: ReconnectState,
        redial: Option<Redialer>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            cfg: Arc::new(std::sync::Mutex::new(cfg.clone())),
            handles: Arc::new(HandleCache::new(MAX_CACHED_HANDLES)),
            reconnect: Arc::new(reconnect),
            redial,
        }
    }

    /// Another handle on this same transport — the same session, lock and
    /// caches. For a write handle, which has to reach the session the
    /// transport is on *now*, not the one it was opened on.
    fn alias(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            cfg: Arc::clone(&self.cfg),
            handles: Arc::clone(&self.handles),
            reconnect: Arc::clone(&self.reconnect),
            redial: self.redial.clone(),
        }
    }

    fn config(&self) -> SmbConfig {
        self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether there is a session that operations can use: one has been
    /// built, and nothing has found it dead since.
    ///
    /// Cheap, and never touches the network — it is what a status badge
    /// reads.
    pub fn is_connected(&self) -> bool {
        self.reconnect.is_live()
    }

    /// Why the server refused the login, if it has. Once it has, this
    /// transport builds no session again: see [`adopt`](Self::adopt).
    pub fn refused(&self) -> Option<String> {
        self.reconnect.refused()
    }

    /// Build a session on `stream` and make it the one every later operation
    /// uses.
    ///
    /// How a transport attached without one gets its first, and how a mount
    /// moves to a better leg: `host` names the server on that leg, which from
    /// the tunnel is an address inside it and from campus is the public name.
    ///
    /// The new session is negotiated and authenticated before anything is
    /// locked, so operations carry on over the old one — or decline, if there
    /// is none — while the handshake runs. Then the swap:
    ///
    /// * Cached read handles are closed on the session they were opened on.
    /// * A write handle that is open keeps its writer, and finishes on the
    ///   session it started on: a transfer is not torn between two. Its next
    ///   reopen, if it needs one, lands on the new session.
    /// * The old session goes when the last thing using it lets go of it.
    ///
    /// A **refused login is final.** It is remembered, and every later call
    /// returns `PermissionDenied` without touching the stream. The appliance
    /// blocks an address after three failures and the block never expires,
    /// so a caller asking again on a timer — which is how this is called —
    /// would turn one typo into a lockout.
    pub async fn adopt<S>(&self, stream: S, host: &str) -> Result<(), SynoFsError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        if let Some(why) = self.reconnect.refused() {
            tracing::debug!("SMB: not building a session; the login was refused ({why})");
            return Err(SynoFsError::PermissionDenied);
        }
        let mut cfg = self.config();
        cfg.host = host.to_string();
        let fresh = match client_over(stream, &cfg).await {
            Ok(client) => client,
            Err(e) => {
                if e.category() == synology_filestation_core::error::ErrorCategory::PermissionDenied
                {
                    self.reconnect.refuse(format!("{e} at {host}"));
                }
                return Err(e);
            }
        };

        let (stale, _previous) = {
            let mut inner = self.inner.lock().await;
            let previous = inner.client.replace(fresh);
            // The trees belonged to the old session, and so did the handles.
            inner.trees.clear();
            *self.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
            self.reconnect.settled();
            (self.handles.clear(), previous)
        };
        // Outside the lock: closing is a round trip on the old session, and
        // the new one has operations to get on with.
        self.close_handles(stale).await;
        Ok(())
    }

    /// Connect + authenticate over a stream the caller opened.
    ///
    /// [`connect`](Self::connect) dials TCP, which is the right thing when the
    /// NAS is reachable. When it is not — off campus, where port 445 never
    /// leaves the building — the way to it is a tunnel, and what comes out of
    /// one is a stream rather than an address. Everything above this line is
    /// then ordinary SMB; the only difference is where the bytes go.
    ///
    /// `cfg.host` still names the server, and is still required. Nothing here
    /// dials it, but it is what tree connects are attributed to and what a DFS
    /// referral's path would be built from — an empty one becomes a malformed
    /// UNC at the first tree connect, a long way from here.
    ///
    /// `cfg.timeout` is **not** used on this path. It bounds the dial, and
    /// there is no dial; the deadline on each response afterwards belongs to
    /// the SMB client and is the same either way. A caller who wants the whole
    /// attempt bounded — a reachability probe, say — should wrap this call.
    ///
    /// # This one cannot bring itself back
    ///
    /// A transport built here will not reopen its own stream: only the caller
    /// knows how the stream was made. So a dead session is reported rather
    /// than recovered — `SmbClient` refuses to dial `host` behind the caller's
    /// back, which for a tunnelled connection would put the session on the
    /// open internet or on nothing at all. Recovery belongs to whoever owns
    /// the tunnel, and a caller that can reopen the stream should say so with
    /// [`over_with_redial`](Self::over_with_redial) — a mount built here
    /// serves the rest of its life off the fallback transport once its stream
    /// ends.
    pub async fn over<S>(stream: S, cfg: &SmbConfig) -> Result<Self, SynoFsError>
    where
        // `Send` and not `Sync`: the stream this exists for holds boxed
        // futures, so it is one and not the other, and the halves are behind
        // mutexes here anyway. Requiring `Sync` excluded the only caller.
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        Self::over_stream(stream, cfg, None).await
    }

    /// [`over`](Self::over), for a caller that can open the stream again.
    ///
    /// `reopen` is asked for a fresh stream to the same server when the
    /// current one is found dead, and the session is rebuilt on it by exactly
    /// the path that built the first one. That is the whole of what a
    /// tunnelled mount was missing: the transport-selection breaker already
    /// re-probes a failed backend every 30 s, but with nothing able to rebuild
    /// the session the probe only ever rediscovered the same dead stream.
    ///
    /// The closure owns the question of *how* — reopening the tunnel if that
    /// is what it takes — which is why it belongs to the caller and not here.
    /// A redial that fails leaves the link flagged, so the next operation
    /// tries again rather than giving up on SMB for good.
    pub async fn over_with_redial<S, F>(
        stream: S,
        cfg: &SmbConfig,
        reopen: F,
    ) -> Result<Self, SynoFsError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
        F: Fn() -> RedialFuture + Send + Sync + 'static,
    {
        let redialer = Redialer {
            open: Arc::new(reopen),
        };
        Self::over_stream(stream, cfg, Some(redialer)).await
    }

    async fn over_stream<S>(
        stream: S,
        cfg: &SmbConfig,
        redial: Option<Redialer>,
    ) -> Result<Self, SynoFsError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        if cfg.host.is_empty() {
            // Logged because `InvalidArg` carries no message, and a malformed
            // UNC at the first tree connect is a long way from the cause.
            tracing::warn!(
                "SMB over a supplied stream still needs a host: it names the server on the \
                 wire, whether or not anything dials it"
            );
            return Err(SynoFsError::InvalidArg);
        }

        let client = client_over(stream, cfg).await?;

        // Only a caller that can reopen the stream makes recovery possible;
        // without one, nothing here will pretend it might.
        let reconnect = ReconnectState::for_supplied_stream(redial.is_some());
        Ok(Self::built(Inner::with(client), cfg, reconnect, redial))
    }

    /// Map an `smb2::Error`, flagging the connection for reconnect when the
    /// error means the link is dead — so the next operation re-establishes it.
    fn mark_and_map(&self, e: &smb2::Error) -> SynoFsError {
        self.reconnect.flag_if_lost(e.kind());
        to_syno_error(e)
    }

    /// Run a best-effort cleanup op, ignoring its result **except** to flag the
    /// link for reconnect if it revealed a dead connection.
    fn note_cleanup(&self, r: Result<(), smb2::Error>) {
        if let Err(e) = r {
            self.reconnect.flag_if_lost(e.kind());
        }
    }

    /// The session to run an operation on, reconnecting first if a prior
    /// operation flagged the link dead.
    ///
    /// This is what lets a long-lived mount recover after a network flap: once
    /// the network is back, the next operation (e.g. a circuit-breaker
    /// half-open probe) rebuilds the session here and succeeds, closing the
    /// breaker — instead of degrading to HTTP for the rest of the session.
    ///
    /// Declines when there is nothing to run on: no session has been handed
    /// over yet, or the login was refused and none will be.
    async fn session<'a>(
        &self,
        client: &'a mut Option<SmbClient>,
        trees: &mut HashMap<String, Tree>,
    ) -> Result<&'a mut SmbClient, SynoFsError> {
        let Some(live) = client.as_mut() else {
            return Err(declined());
        };
        // Two ways back, depending on who owns the link. A transport that
        // dialled its own address lets `SmbClient` redial it; one running on a
        // supplied stream asks the caller for a new stream and rebuilds the
        // session on it, because the address at the far end of a tunnel is
        // reachable only from inside that tunnel.
        let reconnected = match &self.redial {
            Some(redialer) => {
                let open = Arc::clone(&redialer.open);
                let cfg = self.config();
                // Reborrowed, so the session slot is only lent to the redial
                // for as long as the redial runs.
                let slot = &mut *live;
                self.reconnect
                    .reconnect_if_needed(move || async move {
                        let stream = open().await.map_err(redial_failed)?;
                        *slot = client_over(stream, &cfg).await.map_err(redial_failed)?;
                        Ok(())
                    })
                    .await
            }
            None => {
                self.reconnect
                    .reconnect_if_needed(|| live.reconnect())
                    .await
            }
        };
        match reconnected {
            Ok(true) => {
                // The cached trees belonged to the dead session, and so did
                // every open file handle on it. Dropping rather than closing
                // them is right here: the session they belonged to is gone,
                // so there is nothing left to close them against.
                trees.clear();
                drop(self.handles.clear());
            }
            Ok(false) => {}
            // Refused: this operation, and every one after it, goes to HTTP.
            // A `PermissionDenied` here would reach the caller as "you may not
            // read this file", which is not what happened.
            Err(_) if self.reconnect.refused().is_some() => return Err(declined()),
            Err(e) => return Err(self.mark_and_map(&e)),
        }
        Ok(live)
    }

    /// The session and the attached share an operation on `share` runs on.
    async fn ready<'a>(
        &self,
        inner: &'a mut Inner,
        share: &str,
    ) -> Result<(&'a mut SmbClient, &'a mut Tree), SynoFsError> {
        let Inner { client, trees } = inner;
        let client = self.session(client, trees).await?;
        if !trees.contains_key(share) {
            let tree = client
                .connect_share(share)
                .await
                .map_err(|e| self.mark_and_map(&e))?;
            trees.insert(share.to_string(), tree);
        }
        let tree = trees.get_mut(share).expect("tree just ensured");
        Ok((client, tree))
    }

    /// Commit a fully-written temp file onto `target` with an **old-or-new**
    /// guarantee (fast rename, or move-aside on a name collision). See the
    /// module note; shared by `write_atomic` and `write_from_path`.
    async fn commit_temp(
        &self,
        client: &mut SmbClient,
        tree: &mut Tree,
        tmp: &str,
        target: &str,
        _seq: u64,
    ) -> Result<(), SynoFsError> {
        // One operation, performed by the server. The old-or-new guarantee is
        // now the server's to keep: there is no moment when the name resolves
        // to nothing, and nothing left half-done if this process dies here.
        //
        // What this replaces: rename, and on a name collision move the old
        // file aside, rename again, delete the backup, restoring the old file
        // if the second rename failed. Four operations, three of them
        // recovery, and a window in the middle where a reader looking for the
        // target found neither copy. That dance existed only because the SMB
        // library hardcoded ReplaceIfExists=false; our fork sends it.
        match client
            .rename_with_options(
                tree,
                tmp,
                target,
                RenameOptions {
                    replace_if_exists: true,
                },
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                // The temp file is ours and now has no name anyone wants.
                self.note_cleanup(client.delete_file(tree, tmp).await);
                Err(self.mark_and_map(&e))
            }
        }
    }

    /// Metadata for a logical path (`/share/sub/file`).
    pub async fn stat(&self, logical: &str) -> Result<FileMeta, SynoFsError> {
        let loc = SmbPath::from_logical(logical)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        let info = client
            .stat(tree, &loc.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;
        Ok(FileMeta {
            size: info.size,
            is_directory: info.is_directory,
        })
    }

    /// An open read handle for `logical`, from the cache when one is there.
    ///
    /// The lock is held only long enough to attach the share and take cheap
    /// `Arc` clones of the connection and the tree — never across a round trip.
    /// `smb2` is built for exactly this: `Connection` is an `Arc` over shared
    /// state with its own credit accounting and receiver task, and its own docs
    /// say concurrent users of one session need no external locking. Holding
    /// the transport mutex across the CREATE and the READ made every block read
    /// on the whole mount wait for every other one.
    async fn reader_for(
        &self,
        loc: &SmbPath,
        logical: &str,
    ) -> Result<Arc<FileReader>, SynoFsError> {
        self.close_free_handles().await;
        if let Some(reader) = self.handles.get(logical) {
            return Ok(reader);
        }

        let (conn, tree) = {
            let mut guard = self.inner.lock().await;
            let (client, tree) = self.ready(&mut guard, &loc.share).await?;
            let tree = tree.clone();
            (client.connection_mut().clone(), tree)
        };

        let reader = Arc::new(
            std::sync::Arc::new(tree)
                .open_file_reader(conn, &loc.path)
                .await
                .map_err(|e| self.mark_and_map(&e))?,
        );
        let displaced = self.handles.insert(logical, reader.clone());
        self.close_handles(displaced).await;
        Ok(reader)
    }

    /// Close handles the cache has finished with, parking any that a read is
    /// still using — dropping one instead would leak the open file on the
    /// appliance until the session ends.
    async fn close_handles(&self, handles: Vec<Arc<FileReader>>) {
        for handle in handles {
            match Arc::try_unwrap(handle) {
                Ok(reader) => {
                    if let Err(e) = reader.close().await {
                        tracing::debug!(error = %e, "smb: closing a read handle");
                    }
                }
                Err(still_in_use) => self.handles.defer(still_in_use),
            }
        }
    }

    /// Close whatever was parked earlier and has since come free.
    async fn close_free_handles(&self) {
        let free = self.handles.take_free();
        if !free.is_empty() {
            self.close_handles(free).await;
        }
    }

    /// Forget the handle for one path, closing it.
    async fn forget_handle(&self, logical: &str) {
        let dropped = self.handles.invalidate(logical);
        self.close_handles(dropped).await;
    }

    /// Forget every handle at or under `logical`.
    async fn forget_handles_under(&self, logical: &str) {
        let dropped = self.handles.invalidate_prefix(logical);
        self.close_handles(dropped).await;
    }

    /// Read `length` bytes at `offset`.
    ///
    /// One SMB2 READ on a handle that stays open between calls, with no lock
    /// held while it is in flight. This used to be a CREATE, a READ and a
    /// CLOSE, all three serialised behind the transport mutex — three round
    /// trips per 256 KiB block, one block at a time, for the whole mount.
    pub async fn read(
        &self,
        logical: &str,
        offset: u64,
        length: u64,
    ) -> Result<Bytes, SynoFsError> {
        if length == 0 {
            return self.read_full(logical).await;
        }
        let loc = SmbPath::from_logical(logical)?;
        let reader = self.reader_for(&loc, logical).await?;
        match reader.read_at(offset, length).await {
            Ok(data) => Ok(Bytes::from(data)),
            Err(e) => {
                // The handle may be why this failed — a session that went away
                // takes its file handles with it. Drop it so the next read
                // opens a fresh one rather than retrying a dead one forever.
                self.forget_handle(logical).await;
                Err(self.mark_and_map(&e))
            }
        }
    }

    /// Delete a file at `logical`.
    pub async fn delete(&self, logical: &str) -> Result<(), SynoFsError> {
        // A handle held open on a file being deleted is both a stale cache
        // entry and, on SMB, a reason the delete itself can fail.
        self.forget_handles_under(logical).await;
        let loc = SmbPath::from_logical(logical)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        client
            .delete_file(tree, &loc.path)
            .await
            .map_err(|e| self.mark_and_map(&e))
    }

    /// Read an entire file. Uses the pipelined, chunked read so multi-MB `.ORF`
    /// files (past the 8 MiB `MaxReadSize`) come back whole.
    pub async fn read_full(&self, logical: &str) -> Result<Bytes, SynoFsError> {
        let loc = SmbPath::from_logical(logical)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        let data = client
            .read_file_pipelined(tree, &loc.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;
        Ok(Bytes::from(data))
    }

    /// Stream a remote file to the local path `local` **without buffering it in
    /// memory** — for fetching large files. Chunks stream from SMB straight to a
    /// local temp file, which is fsynced and renamed onto `local` (atomic
    /// old-or-nothing; a failure leaves no destination).
    pub async fn read_to_path(&self, logical: &str, local: &Path) -> Result<(), SynoFsError> {
        let loc = SmbPath::from_logical(logical)?;
        let tmp = {
            let mut t = local.as_os_str().to_os_string();
            t.push(".part");
            std::path::PathBuf::from(t)
        };

        let result: Result<(), SynoFsError> = async {
            let mut guard = self.inner.lock().await;
            let (client, tree) = self.ready(&mut guard, &loc.share).await?;

            // Streaming download handle (owned; retains no borrow of client).
            let mut download = client
                .download(tree, &loc.path)
                .await
                .map_err(|e| self.mark_and_map(&e))?;

            // Pump SMB → local temp in bounded chunks (constant memory).
            let mut file = tokio::fs::File::create(&tmp)
                .await
                .map_err(|e| local_fs_error(&format!("create {}", tmp.display()), &e))?;
            while let Some(chunk) = download.next_chunk().await {
                let bytes = chunk.map_err(|e| self.mark_and_map(&e))?;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| local_fs_error(&format!("write {}", tmp.display()), &e))?;
            }
            file.sync_all()
                .await
                .map_err(|e| local_fs_error(&format!("fsync {}", tmp.display()), &e))?;
            drop(file);
            tokio::fs::rename(&tmp, local)
                .await
                .map_err(|e| local_fs_error(&format!("rename to {}", local.display()), &e))
        }
        .await;

        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await; // best-effort cleanup
        }
        result
    }

    /// Replace the file at `logical` with `data`, guaranteeing **old-or-new**:
    /// a reader always sees either the previous file or the fully-written new
    /// one — never a partial/corrupt file — and no data is lost on failure.
    /// That is what makes the selection layer's fallback to HTTP safe.
    ///
    /// New bytes go to an adjacent temp file first. If the target name is free,
    /// a single rename commits. If it already exists (DSM's rename is
    /// `ReplaceIfExists=false`, so it can't replace in one step) the old file is
    /// moved aside to a backup, the new file is renamed in, and the backup is
    /// dropped; a failure mid-swap restores the old file, and the new bytes stay
    /// in the temp — so no copy of the data is ever the only casualty of a
    /// failed step.
    ///
    /// Caveat: during the swap there is a brief window where the target *name*
    /// is momentarily absent (the bytes are safe in the backup/temp). A truly
    /// atomic replace needs `ReplaceIfExists=true` in the SMB layer — tracked as
    /// a follow-up.
    pub async fn write_atomic(&self, logical: &str, data: &[u8]) -> Result<(), SynoFsError> {
        self.forget_handle(logical).await;
        let loc = SmbPath::from_logical(logical)?;
        let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = part_name(&loc.path, seq);

        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;

        // New contents → temp file adjacent to the target (pipelined, so files
        // past the server's MaxWriteSize go in chunks).
        if let Err(e) = client.write_file_pipelined(tree, &tmp, data).await {
            self.note_cleanup(client.delete_file(tree, &tmp).await); // best-effort cleanup
            return Err(self.mark_and_map(&e));
        }

        self.commit_temp(client, tree, &tmp, &loc.path, seq).await
    }

    /// Stream a local file to `logical` (old-or-new) **without buffering it in
    /// memory** — for staging large files. Chunks are read from disk and written
    /// straight to an SMB temp file via a streaming writer, then committed onto
    /// the target with the same move-aside guarantee as [`write_atomic`].
    ///
    /// [`write_atomic`]: Self::write_atomic
    /// Stream a local file to `logical`, which must not already exist.
    ///
    /// Unlike [`Self::write_from_path`] this writes the destination directly
    /// instead of staging a `.part` and renaming. That is the whole point: the
    /// server's `FILE_CREATE` disposition is what makes "fail if it exists"
    /// atomic, and a rename would either clobber the winner of a race or need a
    /// check-then-act that cannot be made safe from here. The cost is that a
    /// failed transfer leaves a short file at the real name, so a failure
    /// removes it.
    pub async fn write_new_from_path(
        &self,
        logical: &str,
        local: &Path,
    ) -> Result<(), SynoFsError> {
        let loc = SmbPath::from_logical(logical)?;

        let mut file = tokio::fs::File::open(local)
            .await
            .map_err(|e| local_fs_error(&format!("open {}", local.display()), &e))?;

        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;

        // Exclusive create: an existing name comes back as
        // STATUS_OBJECT_NAME_COLLISION, which maps to AlreadyExists — the
        // caller's cue that this is an answer, not a transport failure.
        let mut writer = match client.create_file_writer_exclusive(tree, &loc.path).await {
            Ok(w) => w,
            Err(e) => return Err(self.mark_and_map(&e)),
        };

        let mut buf = vec![0u8; 1 << 20];
        let mut stream_err: Option<SynoFsError> = None;
        loop {
            match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = writer.write_chunk(&buf[..n]).await {
                        stream_err = Some(self.mark_and_map(&e));
                        break;
                    }
                }
                Err(e) => {
                    stream_err = Some(SynoFsError::Io(format!("read {}: {e}", local.display())));
                    break;
                }
            }
        }
        if let Err(e) = writer.finish().await {
            stream_err.get_or_insert_with(|| self.mark_and_map(&e));
        }

        if let Some(e) = stream_err {
            // We created this name, so removing it is ours to do — leaving a
            // truncated file where the caller asked for a whole one is worse
            // than leaving nothing.
            self.note_cleanup(client.delete_file(tree, &loc.path).await);
            return Err(e);
        }
        Ok(())
    }

    pub async fn write_from_path(&self, logical: &str, local: &Path) -> Result<(), SynoFsError> {
        self.forget_handle(logical).await;
        let loc = SmbPath::from_logical(logical)?;
        let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = part_name(&loc.path, seq);

        let mut file = tokio::fs::File::open(local)
            .await
            .map_err(|e| local_fs_error(&format!("open {}", local.display()), &e))?;

        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;

        // Streaming writer to the temp file (owned; retains no borrow of client).
        let mut writer = match client.create_file_writer(tree, &tmp).await {
            Ok(w) => w,
            Err(e) => return Err(self.mark_and_map(&e)),
        };

        // Pump disk → SMB in bounded chunks (constant memory, ~1 MiB).
        let mut buf = vec![0u8; 1 << 20];
        let mut stream_err: Option<SynoFsError> = None;
        loop {
            match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = writer.write_chunk(&buf[..n]).await {
                        stream_err = Some(self.mark_and_map(&e));
                        break;
                    }
                }
                Err(e) => {
                    stream_err = Some(SynoFsError::Io(format!("read {}: {e}", local.display())));
                    break;
                }
            }
        }
        // Always finalize to close the SMB handle; surface a finish error only if
        // the pump itself hadn't already failed.
        if let Err(e) = writer.finish().await {
            stream_err.get_or_insert_with(|| self.mark_and_map(&e));
        }

        if let Some(e) = stream_err {
            self.note_cleanup(client.delete_file(tree, &tmp).await); // best-effort cleanup
            return Err(e);
        }

        self.commit_temp(client, tree, &tmp, &loc.path, seq).await
    }
}

// ── core transport traits (dependency inversion) ─────────────────────────────
//
// Implementing these is the entire integration surface: SynologyClient prefers
// an injected backend over HTTP and falls back on transport failures, with no
// change to any read/write call site.

#[async_trait]
impl ReadTransport for SmbTransport {
    async fn read(&self, path: &str, offset: u64, length: u64) -> Result<Bytes, SynoFsError> {
        // Delegate to the inherent method (inherent wins over the trait method
        // for this path syntax, so this is not recursive).
        SmbTransport::read(self, path, offset, length).await
    }
}

#[async_trait]
impl WriteTransport for SmbTransport {
    async fn write(&self, path: &str, data: &[u8]) -> Result<(), SynoFsError> {
        self.write_atomic(path, data).await
    }
}

#[async_trait]
impl StreamWriteTransport for SmbTransport {
    async fn write_from_path(&self, remote_path: &str, local: &Path) -> Result<(), SynoFsError> {
        SmbTransport::write_from_path(self, remote_path, local).await
    }

    async fn write_new_from_path(
        &self,
        remote_path: &str,
        local: &Path,
    ) -> Result<(), SynoFsError> {
        SmbTransport::write_new_from_path(self, remote_path, local).await
    }
}

#[async_trait]
impl StreamReadTransport for SmbTransport {
    async fn read_to_path(&self, remote_path: &str, local: &Path) -> Result<(), SynoFsError> {
        SmbTransport::read_to_path(self, remote_path, local).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_lost_kinds_trigger_reconnect() {
        // These mean the link is dead → reconnect before the next op.
        assert!(is_connection_lost(smb2::ErrorKind::ConnectionLost));
        assert!(is_connection_lost(smb2::ErrorKind::TimedOut));
        assert!(is_connection_lost(smb2::ErrorKind::SessionExpired));
        // These are per-request; the connection is still good.
        assert!(!is_connection_lost(smb2::ErrorKind::NotFound));
        assert!(!is_connection_lost(smb2::ErrorKind::AccessDenied));
        assert!(!is_connection_lost(smb2::ErrorKind::SharingViolation));
    }

    #[tokio::test]
    async fn a_transport_that_cannot_redial_is_not_wedged_by_a_blip() {
        // The flag is a one-way door when the reconnect that clears it cannot
        // run: `reconnect_if_needed` re-sets it on failure, so one transient
        // timeout on a perfectly live tunnel would fail every operation from
        // then on. There is nothing to reconnect, so there is nothing to owe.
        let state = ReconnectState::for_supplied_stream(false);
        state.flag_if_lost(smb2::ErrorKind::ConnectionLost);

        let outcome = state
            .reconnect_if_needed(|| async { panic!("there is nothing to redial") })
            .await;

        assert!(matches!(outcome, Ok(false)), "no attempt, and no error");
    }

    /// A mount that reaches the NAS through a tunnel runs on a stream this
    /// crate did not open, and every such transport used to be built unable to
    /// reconnect. The moment that stream ended, every later operation failed
    /// `Disconnected` against a session nothing would rebuild: the
    /// transport-selection breaker re-probed every 30 s, the probe hit the
    /// same dead session, and the mount served the rest of its life off the
    /// HTTP fallback. A caller that leaves a way to reopen the stream must get
    /// a transport that uses it.
    #[tokio::test]
    async fn a_supplied_stream_that_can_be_reopened_comes_back() {
        let state = ReconnectState::for_supplied_stream(true);
        state.flag_if_lost(smb2::ErrorKind::ConnectionLost);

        let ran = state
            .reconnect_if_needed(|| async { Ok(()) })
            .await
            .expect("the redial is allowed to run");

        assert!(
            ran,
            "the caller left a way to reopen the stream, so a dead link is reopened"
        );
    }

    #[tokio::test]
    async fn reconnect_runs_only_when_flagged_then_clears() {
        let state = ReconnectState::default();
        // Not flagged → no reconnect, and the closure is never run.
        let ran = state
            .reconnect_if_needed(|| async { panic!("should not reconnect") })
            .await
            .unwrap();
        assert!(!ran);

        // A connection-lost error flags it → reconnect runs once and succeeds.
        state.flag_if_lost(smb2::ErrorKind::ConnectionLost);
        let ran = state
            .reconnect_if_needed(|| async { Ok(()) })
            .await
            .unwrap();
        assert!(
            ran,
            "flagged → reconnect ran (caller clears its tree cache)"
        );

        // Flag cleared on success → the next op is a no-op again.
        let ran = state
            .reconnect_if_needed(|| async { panic!("should not reconnect") })
            .await
            .unwrap();
        assert!(!ran);
    }

    #[tokio::test]
    async fn failed_reconnect_keeps_the_flag_set_for_retry() {
        let state = ReconnectState::default();
        state.flag_if_lost(smb2::ErrorKind::TimedOut);
        // Reconnect fails → error surfaces, flag stays set.
        assert!(state
            .reconnect_if_needed(|| async { Err(smb2::Error::Disconnected) })
            .await
            .is_err());
        // Still flagged → the next operation retries the reconnect.
        let ran = state
            .reconnect_if_needed(|| async { Ok(()) })
            .await
            .unwrap();
        assert!(ran, "flag persisted so reconnect is retried");
    }

    #[tokio::test]
    async fn per_request_errors_do_not_flag_reconnect() {
        let state = ReconnectState::default();
        state.flag_if_lost(smb2::ErrorKind::NotFound); // not a link failure
        let ran = state
            .reconnect_if_needed(|| async { panic!("should not reconnect") })
            .await
            .unwrap();
        assert!(!ran);
    }

    // ── A login that was refused ──────────────────────────────────────────────

    /// A refused login is not a network blip. Every attempt is a real
    /// authentication, DSM blocks an address after three failures, and the
    /// block does not expire — so the reconnect that retries a dropped link on
    /// the next operation must not retry a rejected password on it. An AD
    /// account whose domain was never set is exactly this, every time.
    #[tokio::test]
    async fn a_refused_login_is_not_tried_again() {
        let state = ReconnectState::default();
        state.flag_if_lost(smb2::ErrorKind::ConnectionLost);

        let first = state
            .reconnect_if_needed(|| async {
                Err(smb2::Error::Auth {
                    message: "logon failure".into(),
                })
            })
            .await;
        assert!(first.is_err());
        assert!(state.refused().is_some(), "and it is remembered");

        let second = state
            .reconnect_if_needed(|| async { panic!("asked again after a refusal") })
            .await;
        assert!(second.is_err(), "still refused, without asking");
    }

    #[tokio::test]
    async fn a_dropped_link_is_still_retried() {
        // The other half: only a refusal is final.
        let state = ReconnectState::default();
        state.flag_if_lost(smb2::ErrorKind::ConnectionLost);
        let _ = state
            .reconnect_if_needed(|| async { Err(smb2::Error::Disconnected) })
            .await;

        assert!(state.refused().is_none());
        assert!(state
            .reconnect_if_needed(|| async { Ok(()) })
            .await
            .unwrap());
    }

    #[test]
    fn a_redial_that_was_refused_still_says_so() {
        // The redial carries its failure back as an `smb2::Error`. Folding a
        // rejected password into `Io` there hid the one failure that must not
        // be retried behind the kind that always is.
        assert!(is_refusal(&redial_failed(SynoFsError::PermissionDenied)));
        assert!(!is_refusal(&redial_failed(SynoFsError::Io(
            "no route".into()
        ))));
    }

    // ── A transport with no session yet ───────────────────────────────────────

    fn a_config() -> SmbConfig {
        SmbConfig::new("nas.example", "user", "hunter2")
    }

    fn never_redialled() -> impl Fn() -> RedialFuture + Send + Sync + 'static {
        || Box::pin(async { panic!("a transport with no session dialled on its own") })
    }

    /// Attached when SMB is unreachable at connect, so there is somewhere for
    /// a session to go once it is. Until then it has to be free: every
    /// operation on the mount passes through it on the way to HTTP, so it
    /// declines at once rather than dialling, waiting or failing — a failure
    /// would trip its breaker and log a warning per call for an outcome
    /// everyone expected.
    #[tokio::test]
    async fn a_transport_with_no_session_declines_and_dials_nothing() {
        let smb = SmbTransport::unconnected(&a_config(), never_redialled());
        assert!(!smb.is_connected());

        let local = std::env::temp_dir().join("smb-unconnected-test");
        let outcomes: Vec<(&str, Result<(), SynoFsError>)> = vec![
            (
                "read",
                ReadTransport::read(&smb, "/share/f", 0, 10)
                    .await
                    .map(|_| ()),
            ),
            ("read whole", smb.read_full("/share/f").await.map(|_| ())),
            ("read to path", smb.read_to_path("/share/f", &local).await),
            ("write", smb.write_atomic("/share/f", b"x").await),
            (
                "list",
                MetadataTransport::list_dir(&smb, "/share")
                    .await
                    .map(|_| ()),
            ),
            (
                "shares",
                MetadataTransport::list_shares(&smb).await.map(|_| ()),
            ),
            (
                "info",
                MetadataTransport::get_info(&smb, "/share/f")
                    .await
                    .map(|_| ()),
            ),
            (
                "open for writing",
                smb.open_write("/share/f", WriteOpen::CreateNew)
                    .await
                    .map(|_| ()),
            ),
        ];

        for (what, outcome) in outcomes {
            let e = outcome.expect_err(what);
            assert_eq!(
                e.category(),
                synology_filestation_core::error::ErrorCategory::NotSupported,
                "{what}: {e:?}"
            );
        }
    }

    #[test]
    fn part_name_is_adjacent_and_unique_per_seq() {
        let pid = std::process::id();
        assert_eq!(
            part_name("REEF/img.orf", 7),
            format!("REEF/img.orf.part-{pid}-7")
        );
        // Different sequence numbers yield different temp names (so concurrent
        // writers to the same target don't collide).
        assert_ne!(part_name("x", 1), part_name("x", 2));
    }

    #[test]
    fn from_login_parses_domain_from_username() {
        // DOMAIN\user — unambiguous.
        let c = SmbConfig::from_login("nas", "KRG\\c.crutchfield.642", "pw");
        assert_eq!(c.domain, "KRG");
        assert_eq!(c.username, "c.crutchfield.642");

        // user@REALM (UPN) — NetBIOS domain is the first realm label, upper-cased.
        let c = SmbConfig::from_login("nas", "c.crutchfield.642@krg.local", "pw");
        assert_eq!(c.domain, "KRG");
        assert_eq!(c.username, "c.crutchfield.642");

        // Local account — no domain.
        let c = SmbConfig::from_login("nas", "admin", "pw");
        assert_eq!(c.domain, "");
        assert_eq!(c.username, "admin");
        assert_eq!(c.port, 445);
    }

    #[test]
    fn smbconfig_new_defaults_and_addr() {
        let cfg = SmbConfig::new("nas.example.com", "user", "pass");
        assert_eq!(cfg.port, 445);
        assert_eq!(cfg.addr(), "nas.example.com:445");
        let cfg2 = SmbConfig::new("nas.example.com:1445", "user", "pass");
        assert_eq!(
            cfg2.addr(),
            "nas.example.com:1445",
            "explicit port preserved"
        );
    }
}

/// Whether a share the server advertised is one a user would browse.
///
/// `NetShareEnumAll` also reports the administrative plumbing — `IPC$`, the
/// per-volume admin shares — which are not places to put files. The special
/// bit (`0x8000_0000`) marks them, and the `$` suffix catches servers that
/// don't set it; anything that isn't a disk tree (low bits non-zero) is a
/// printer or a pipe.
fn is_user_share(share_type: u32, name: &str) -> bool {
    const SPECIAL: u32 = 0x8000_0000;
    const TYPE_MASK: u32 = 0x0000_000F;
    share_type & SPECIAL == 0 && share_type & TYPE_MASK == 0 && !name.ends_with('$')
}

/// The logical path a rename produces: `new_name` replaces the last component
/// of `old_path`, which is what the FileStation `rename` contract means by a
/// name rather than a path.
fn sibling_path(old_path: &str, new_name: &str) -> String {
    let trimmed = old_path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{parent}/{new_name}"),
        _ => format!("/{new_name}"),
    }
}

/// Windows FILETIME → unix seconds, for the timestamps FileStation reports.
fn unix_secs(t: smb2::pack::FileTime) -> i64 {
    t.to_system_time()
        .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build the `SynoFileInfo` a caller expects, so nothing downstream can tell
/// whether SMB or the HTTP API answered.
fn file_info(
    name: &str,
    path: &str,
    isdir: bool,
    size: u64,
    mtime: i64,
    crtime: i64,
) -> SynoFileInfo {
    SynoFileInfo {
        name: name.to_string(),
        path: path.to_string(),
        isdir,
        additional: Some(SynoAdditional {
            size: Some(size),
            owner: None,
            time: Some(SynoTime {
                atime: mtime,
                mtime,
                ctime: mtime,
                crtime,
            }),
            perm: None,
        }),
        code: None,
    }
}

#[async_trait]
impl MetadataTransport for SmbTransport {
    async fn list_shares(&self) -> Result<Vec<SynoFileInfo>, SynoFsError> {
        let mut guard = self.inner.lock().await;
        let Inner { client, trees } = &mut *guard;
        // No share to attach for an enumeration, so only the session. This
        // used to call `SmbClient::reconnect` directly, which a transport on a
        // supplied stream cannot do: the enumeration alone bypassed the redial.
        let client = self.session(client, trees).await?;
        let shares = client
            .list_shares()
            .await
            .map_err(|e| self.mark_and_map(&e))?;
        Ok(shares
            .into_iter()
            .filter(|s| is_user_share(s.share_type, &s.name))
            .map(|s| {
                let path = format!("/{}", s.name);
                file_info(&s.name, &path, true, 0, 0, 0)
            })
            .collect())
    }

    async fn list_dir(&self, folder_path: &str) -> Result<Vec<SynoFileInfo>, SynoFsError> {
        let loc = SmbPath::from_logical(folder_path)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        let entries = client
            .list_directory(tree, &loc.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;

        let base = folder_path.trim_end_matches('/');
        Ok(entries
            .into_iter()
            // The dot entries are an SMB directory's own bookkeeping; a
            // FileStation listing has never had them and readdir synthesises
            // its own.
            .filter(|e| e.name != "." && e.name != "..")
            .map(|e| {
                let path = format!("{base}/{}", e.name);
                file_info(
                    &e.name,
                    &path,
                    e.is_directory,
                    e.size,
                    unix_secs(e.modified),
                    unix_secs(e.created),
                )
            })
            .collect())
    }

    async fn get_info(&self, path: &str) -> Result<SynoFileInfo, SynoFsError> {
        let loc = SmbPath::from_logical(path)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        let info = client
            .stat(tree, &loc.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;
        let name = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path);
        Ok(file_info(
            name,
            path,
            info.is_directory,
            info.size,
            unix_secs(info.modified),
            unix_secs(info.created),
        ))
    }

    async fn create_folder(&self, parent: &str, name: &str) -> Result<SynoFileInfo, SynoFsError> {
        let full = format!("{}/{}", parent.trim_end_matches('/'), name);
        let loc = SmbPath::from_logical(&full)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        client
            .create_directory(tree, &loc.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;
        Ok(file_info(name, &full, true, 0, 0, 0))
    }

    async fn rename(&self, old_path: &str, new_name: &str) -> Result<SynoFileInfo, SynoFsError> {
        // The name a handle was opened under is gone; anything underneath a
        // renamed directory goes with it.
        self.forget_handles_under(old_path).await;
        let from = SmbPath::from_logical(old_path)?;
        let new_logical = sibling_path(old_path, new_name);
        let to = SmbPath::from_logical(&new_logical)?;
        // A rename is a tree-local operation; crossing shares would be a copy,
        // which this is not.
        if from.share != to.share {
            return Err(SynoFsError::InvalidArg);
        }

        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &from.share).await?;
        client
            .rename(tree, &from.path, &to.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;

        let info = client
            .stat(tree, &to.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;
        Ok(file_info(
            new_name,
            &new_logical,
            info.is_directory,
            info.size,
            unix_secs(info.modified),
            unix_secs(info.created),
        ))
    }

    async fn delete(&self, path: &str) -> Result<(), SynoFsError> {
        let loc = SmbPath::from_logical(path)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        // SMB deletes files and directories through different calls, so ask
        // first. FileStation's delete takes either.
        let info = client
            .stat(tree, &loc.path)
            .await
            .map_err(|e| self.mark_and_map(&e))?;
        let done = if info.is_directory {
            client.delete_directory(tree, &loc.path).await
        } else {
            client.delete_file(tree, &loc.path).await
        };
        done.map_err(|e| self.mark_and_map(&e))
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<(), SynoFsError> {
        self.forget_handle(path).await;
        let loc = SmbPath::from_logical(path)?;
        let mut guard = self.inner.lock().await;
        let (client, tree) = self.ready(&mut guard, &loc.share).await?;
        // SET_INFO with FileEndOfFileInformation: the file's length is a
        // number the server sets, so the bytes past it are never read and
        // never rewritten — regardless of how many there are.
        client
            .set_end_of_file(tree, &loc.path, size)
            .await
            .map_err(|e| self.mark_and_map(&e))
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn administrative_shares_are_not_places_to_put_files() {
        const DISK: u32 = 0;
        const IPC: u32 = 3;
        const SPECIAL: u32 = 0x8000_0000;

        assert!(is_user_share(DISK, "fishsense_data"));
        // IPC$ is the named-pipe endpoint the share enumeration itself rides on.
        assert!(!is_user_share(IPC | SPECIAL, "IPC$"));
        // Per-volume admin shares: flagged special, and named with a $.
        assert!(!is_user_share(DISK | SPECIAL, "C$"));
        // A server that forgets the special bit is still caught by the suffix.
        assert!(!is_user_share(DISK, "ADMIN$"));
        // Printers and pipes are not directories.
        assert!(!is_user_share(1, "LaserJet"));
    }

    #[test]
    fn rename_replaces_the_last_component_only() {
        assert_eq!(
            sibling_path("/share/dir/old.orf", "new.orf"),
            "/share/dir/new.orf"
        );
        // A trailing slash is a directory being renamed, not a different path.
        assert_eq!(sibling_path("/share/dir/", "renamed"), "/share/renamed");
        // Renaming a share-level entry keeps it share-level.
        assert_eq!(sibling_path("/share/file", "other"), "/share/other");
    }

    #[test]
    fn a_windows_epoch_timestamp_becomes_unix_seconds() {
        use std::time::{Duration, UNIX_EPOCH};
        let when = UNIX_EPOCH + Duration::from_secs(1_786_315_083);
        assert_eq!(
            unix_secs(smb2::pack::FileTime::from_system_time(when)),
            1_786_315_083
        );
    }
}

/// One open file being written over SMB.
///
/// The server has no notion of a "file position we hold": every write carries
/// its own offset. What the handle carries instead is a writer *positioned* at
/// the next offset it expects, because sequential appends — what a copy is —
/// then cost one round trip each. A write that lands somewhere else closes that
/// writer and opens another at the offset asked for, which is a reopen a
/// sequential copy never pays.
pub struct SmbWriteHandle {
    /// The transport it was opened on, for the session it is on *now*: a
    /// writer is reopened on whatever session the transport has, which after
    /// a move to a better leg is not the one this handle started on.
    transport: SmbTransport,
    loc: SmbPath,
    writer: Option<smb2::FileWriter>,
    /// Where the open writer will put the next chunk.
    next: u64,
}

impl SmbWriteHandle {
    /// Open a writer at `offset`, replacing whatever this handle had.
    ///
    /// `FileOpenIf` is what makes this safe for an existing file: it opens what
    /// is there and creates only when absent, so bytes outside the ones written
    /// survive. An overwrite disposition here would quietly truncate a file the
    /// caller only meant to patch.
    async fn reopen_at(&mut self, offset: u64) -> Result<(), SynoFsError> {
        self.finish_writer().await?;
        let transport = &self.transport;
        let mut guard = transport.inner.lock().await;
        let (client, tree) = transport.ready(&mut guard, &self.loc.share).await?;
        let writer = client
            .create_file_writer_at(tree, &self.loc.path, offset)
            .await
            .map_err(|e| transport.mark_and_map(&e))?;
        self.writer = Some(writer);
        self.next = offset;
        Ok(())
    }

    /// Close the open writer, if any, surfacing what the server said.
    async fn finish_writer(&mut self) -> Result<(), SynoFsError> {
        match self.writer.take() {
            // `finish` reports the bytes it wrote; the caller tracks its own
            // offsets, so only the failure matters here.
            Some(w) => w
                .finish()
                .await
                .map(|_| ())
                .map_err(|e| self.transport.mark_and_map(&e)),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl WriteHandle for SmbWriteHandle {
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), SynoFsError> {
        if needs_reopen(self.writer.is_some(), self.next, offset) {
            self.reopen_at(offset).await?;
        }
        let writer = self.writer.as_mut().expect("writer just opened");
        writer
            .write_chunk(data)
            .await
            .map_err(|e| self.transport.mark_and_map(&e))?;
        self.next = offset + data.len() as u64;
        Ok(())
    }

    async fn close(&mut self) -> Result<(), SynoFsError> {
        // A handle opened and never written still names a file the caller
        // expects to exist — `touch` is precisely that sequence. Opening at 0
        // here is what creates it; without this, closing an untouched handle
        // would report success over a file that was never made.
        if self.writer.is_none() && self.next == 0 {
            self.reopen_at(0).await?;
        }
        self.finish_writer().await
    }
}

#[async_trait]
impl OpenWriteTransport for SmbTransport {
    async fn open_write(
        &self,
        path: &str,
        mode: WriteOpen,
    ) -> Result<Box<dyn WriteHandle>, SynoFsError> {
        self.forget_handle(path).await;
        let loc = SmbPath::from_logical(path)?;
        let mut handle = SmbWriteHandle {
            transport: self.alias(),
            loc: loc.clone(),
            writer: None,
            next: 0,
        };

        match mode {
            // Create eagerly, so a name already taken is an error the caller
            // gets from `open` — where `create(2)` can still report it — rather
            // than from the first write.
            WriteOpen::CreateNew => {
                let mut guard = self.inner.lock().await;
                let (client, tree) = self.ready(&mut guard, &loc.share).await?;
                let writer = client
                    .create_file_writer_exclusive(tree, &loc.path)
                    .await
                    .map_err(|e| self.mark_and_map(&e))?;
                handle.writer = Some(writer);
            }
            // Nothing to do until the first write says where it goes: opening
            // now would guess an offset and pay a reopen for guessing wrong.
            WriteOpen::Existing => {}
        }
        Ok(Box::new(handle))
    }
}

/// Whether the next write has to open a new writer.
///
/// Sequential appends — what copying a file into the mount is — keep the writer
/// they have and cost one round trip per chunk. Anything that lands away from
/// where the open writer sits has to reopen there, because SMB writes carry
/// their own offset and the writer's is set when it opens.
fn needs_reopen(has_writer: bool, next_offset: u64, write_offset: u64) -> bool {
    !has_writer || write_offset != next_offset
}

#[cfg(test)]
mod write_handle_tests {
    use super::*;

    #[test]
    fn a_sequential_copy_never_reopens() {
        // The case that matters: chunk after chunk, each starting where the
        // last ended. One open writer serves the whole file.
        let mut next = 0u64;
        for _ in 0..4 {
            assert!(!needs_reopen(true, next, next), "offset {next} continued");
            next += 1 << 20;
        }
    }

    #[test]
    fn the_first_write_opens_a_writer() {
        assert!(needs_reopen(false, 0, 0));
    }

    #[test]
    fn a_write_that_lands_elsewhere_reopens_there() {
        // Seeking back to patch a header, and skipping forward past a hole.
        assert!(needs_reopen(true, 4096, 0));
        assert!(needs_reopen(true, 4096, 8192));
    }
}
