//! C ABI bindings for the Synology FileStation client and mount driver.
//!
//! Consumed by the desktop GUI via P/Invoke. The design mirrors the PyO3
//! binding ([`python/synology_filestation`]): an opaque [`SynoClient`] wraps an
//! `Arc<SynologyClient>` plus a per-client Tokio runtime, and every blocking
//! call funnels through `runtime.block_on`. Errors are reported through a
//! [`SynoError`] out-param carrying a typed `kind`, the raw DSM code, and a
//! human-readable message; the `kind`/`dsm_code` classification mirrors the
//! PyO3 exception mapping so both bindings agree.
//!
//! Every exported function wraps its body in `catch_unwind`: a panic in the
//! Rust core (or, for the mount, the WinFsp worker thread) is converted to a
//! `Panic` status rather than unwinding across the FFI boundary (UB).

use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use synology_filestation_connect::{
    profile::ProfileSource, Chain, Endpoints, NoTunnel, OpenVpnTunnel, TcpProber, Transport,
    TransportPolicy, Tunnel, DEFAULT_RECHECK,
};
use synology_filestation_core::client::SynologyClient;
use synology_filestation_core::error::SynoFsError;
use synology_filestation_core::types::SynoFileInfo;
use synology_filestation_core::ThrottleConfig;
use synology_filestation_fuse::legs::{Legs, DEFAULT_WATCH_INTERVAL};
use synology_filestation_fuse::{is_otp_required, spawn_mount, MountOptions};
use synology_filestation_smb::SmbConfig;
use tokio::runtime::Runtime;
use tracing::{info, warn};

mod logging;

const DOWNLOAD_CHUNK: u64 = 4 * 1024 * 1024;

/// How long to wait for a filesystem to finish after unmounting it.
///
/// Past this it is left to finish on its own. The volume is gone either way —
/// that happens first — and what remains is a worker waiting on a request the
/// NAS is not answering, which nothing here can hurry.
const UNMOUNT_PATIENCE: Duration = Duration::from_secs(10);

/// How long the whole tunnel attempt may take, handshake included.
const VPN_PATIENCE: Duration = Duration::from_secs(30);

/// How long to wait for SMB to answer before deciding it does not.
///
/// Short: this runs while somebody watches a spinner, and being wrong costs
/// them the HTTP path rather than the connection.
const SMB_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

// ─── Status / error reporting ────────────────────────────────────────────────

/// Status codes returned by every fallible export. `Ok` is 0; the rest mirror
/// the `SynoFsError` variants, plus a few FFI-specific cases.
#[repr(i32)]
#[derive(Clone, Copy)]
pub enum SynoStatus {
    Ok = 0,
    NotFound = 1,
    PermissionDenied = 2,
    AlreadyExists = 3,
    NotEmpty = 4,
    InvalidArg = 5,
    NoSpace = 6,
    NotSupported = 7,
    Io = 8,
    Api = 9,
    LoginFailed = 10,
    /// Login needs a 2FA / OTP code — the GUI should prompt and retry connect.
    OtpRequired = 11,
    /// A required pointer argument was null.
    NullArg = 12,
    /// A panic was caught at the FFI boundary.
    Panic = 13,
    /// DSM session id expired or unknown (DSM code 119).
    SidNotFound = 14,
    /// The NAS's TLS certificate could not be verified. Distinct from `Io` so
    /// the GUI can offer "accept self-signed certificate" rather than showing a
    /// generic transport error.
    TlsError = 15,
}

/// Out-param filled on error. `message` is a heap UTF-8 C string owned by the
/// caller; free it with [`syno_string_free`]. `dsm_code` is the raw Synology
/// API code when applicable, else 0.
#[repr(C)]
pub struct SynoError {
    pub kind: i32,
    pub dsm_code: u32,
    pub message: *mut c_char,
}

/// Map a core error to (typed status, raw DSM code). Both this and the PyO3
/// exception mapping derive from the core's shared [`ErrorCategory`], so the
/// GUI and the Python bindings classify every DSM code identically.
fn classify(err: &SynoFsError) -> (SynoStatus, u32) {
    use synology_filestation_core::error::ErrorCategory as C;
    use SynoStatus as S;

    // A rejected TLS certificate gets its own status ahead of everything else:
    // it is the one transport failure with a specific, actionable remedy (trust
    // the certificate, or tick "accept self-signed"), and the GUI can only
    // offer that if it can tell this case apart from a generic I/O error.
    if err.is_tls_error() {
        return (S::TlsError, 0);
    }

    // A failed (re)login always presents as LoginFailed, but we preserve the
    // underlying DSM code on `dsm_code` so callers can inspect what failed.
    if let SynoFsError::LoginFailed(inner) = err {
        let code = match inner.as_ref() {
            SynoFsError::ApiError(c) => *c,
            _ => 0,
        };
        return (S::LoginFailed, code);
    }

    let dsm = match err {
        SynoFsError::ApiError(c) => *c,
        _ => 0,
    };
    let status = match err.category() {
        C::NotFound => S::NotFound,
        C::PermissionDenied => S::PermissionDenied,
        C::AlreadyExists => S::AlreadyExists,
        C::NotEmpty => S::NotEmpty,
        C::InvalidArg => S::InvalidArg,
        C::NoSpace => S::NoSpace,
        C::NotSupported => S::NotSupported,
        C::SidExpired => S::SidNotFound,
        C::Transport => S::Io,
        // No dedicated FFI status for "busy"; surface as a generic API error
        // (the raw DSM code is still on `dsm_code`).
        C::Busy | C::Other => S::Api,
    };
    (status, dsm)
}

/// Write a status + message into `*err` (if non-null) and return the status as
/// an `i32`. Always returns the numeric status so callers can `return set_err(…)`.
unsafe fn set_err(err: *mut SynoError, status: SynoStatus, dsm: u32, message: &str) -> i32 {
    if !err.is_null() {
        let msg = CString::new(message).unwrap_or_default().into_raw();
        (*err).kind = status as i32;
        (*err).dsm_code = dsm;
        (*err).message = msg;
    }
    status as i32
}

unsafe fn set_core_err(err: *mut SynoError, e: &SynoFsError) -> i32 {
    let (status, dsm) = classify(e);
    set_err(err, status, dsm, &e.to_string())
}

/// Run `f`, converting any panic into a `Panic` status written to `err`.
fn guard<F: FnOnce() -> i32>(err: *mut SynoError, f: F) -> i32 {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => unsafe { set_err(err, SynoStatus::Panic, 0, "panic in native code") },
    }
}

/// Borrow a required C string; returns `Err(status)` (already written to `err`)
/// when null or non-UTF-8.
unsafe fn req_str<'a>(p: *const c_char, name: &str, err: *mut SynoError) -> Result<&'a str, i32> {
    if p.is_null() {
        return Err(set_err(
            err,
            SynoStatus::NullArg,
            0,
            &format!("`{name}` must not be null"),
        ));
    }
    CStr::from_ptr(p).to_str().map_err(|_| {
        set_err(
            err,
            SynoStatus::InvalidArg,
            0,
            &format!("`{name}` is not UTF-8"),
        )
    })
}

/// Borrow an optional C string (null → `None`).
unsafe fn opt_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

// ─── JSON marshalling of file info ───────────────────────────────────────────

/// Flatten a `SynoFileInfo` into a stable, documented JSON shape. Keys are
/// always present (null when absent) so the C# side can deserialize into a
/// fixed record. Mirrors the PyO3 `fileinfo_to_pydict` contract.
fn fileinfo_json(info: &SynoFileInfo) -> serde_json::Value {
    let (size, mtime, atime, ctime, perm) = match &info.additional {
        Some(a) => (
            a.size,
            a.time.as_ref().map(|t| t.mtime),
            a.time.as_ref().map(|t| t.atime),
            a.time.as_ref().map(|t| t.ctime),
            a.perm.as_ref().map(|p| p.posix),
        ),
        None => (None, None, None, None, None),
    };
    serde_json::json!({
        "name": info.name,
        "path": info.path,
        "isdir": info.isdir,
        "size": size,
        "mtime": mtime,
        "atime": atime,
        "ctime": ctime,
        "perm": perm,
    })
}

unsafe fn emit_json(out: *mut *mut c_char, value: &serde_json::Value, err: *mut SynoError) -> i32 {
    if out.is_null() {
        return set_err(
            err,
            SynoStatus::NullArg,
            0,
            "output pointer must not be null",
        );
    }
    match CString::new(value.to_string()) {
        Ok(s) => {
            *out = s.into_raw();
            SynoStatus::Ok as i32
        }
        Err(_) => set_err(
            err,
            SynoStatus::Io,
            0,
            "result contained an interior NUL byte",
        ),
    }
}

// ─── Opaque handles ──────────────────────────────────────────────────────────

/// Opaque authenticated client handle.
pub struct SynoClient {
    inner: Arc<SynologyClient>,
    runtime: Arc<Runtime>,
    /// The SMB session and the chain that moves it between legs, for
    /// [`syno_transport`]. `None` only when SMB is not allowed at all.
    legs: Option<Arc<Legs>>,
    /// Looks for a better leg while the handle lives; aborted when it goes.
    watching: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SynoClient {
    fn drop(&mut self) {
        // Before the runtime goes, so a look in progress is not what the
        // runtime's shutdown waits on.
        if let Some(watching) = self.watching.take() {
            watching.abort();
        }
    }
}

/// The leg `legs` is on now, as the C ABI names it.
fn transport_of(legs: Option<&Legs>) -> SynoTransport {
    legs.map_or(SynoTransport::Https, |legs| legs.current().into())
}

/// Which leg a connection is using, as the GUI's badge shows it.
///
/// Ordered worst to best deliberately, so a consumer can compare them without
/// knowing what any of them mean.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SynoTransport {
    /// The HTTP FileStation API: works from anywhere the DSM port is open, and
    /// cannot resume an interrupted transfer.
    Https = 0,
    /// SMB through a tunnel this process raised.
    SmbOverVpn = 1,
    /// SMB straight to the appliance.
    SmbDirect = 2,
}

impl From<Transport> for SynoTransport {
    fn from(transport: Transport) -> Self {
        match transport {
            Transport::Https => Self::Https,
            Transport::SmbOverVpn => Self::SmbOverVpn,
            Transport::SmbDirect => Self::SmbDirect,
        }
    }
}

/// Which leg `client` is using: 0 HTTP, 1 SMB through a tunnel, 2 SMB direct.
///
/// `-1` for a null handle.
///
/// **Live**, and it changes: a connection that started on HTTP moves to SMB
/// when SMB becomes reachable, one through the tunnel moves to a direct
/// connection when that answers, and one whose SMB session died reports HTTP
/// until it is back. A consumer showing it should read it again rather than
/// keep the first answer. Cheap — it never touches the network.
///
/// # Safety
/// `client` must be a handle from `syno_connect`, or null.
#[no_mangle]
pub unsafe extern "C" fn syno_transport(client: *const SynoClient) -> i32 {
    match client.as_ref() {
        Some(client) => transport_of(client.legs.as_deref()) as i32,
        None => -1,
    }
}

/// Everything the chain needs that the caller supplied.
struct Reachable<'a> {
    host: &'a str,
    username: &'a str,
    password: &'a str,
    domain: Option<&'a str>,
    /// Where the OpenVPN profile is kept, if an escalation is allowed at all.
    vpn_profile: Option<&'a str>,
    /// The NAS's address inside that tunnel, which its public name does not
    /// resolve to — the tunnel pushes no DNS.
    vpn_host: Option<&'a str>,
    /// Where that profile lives on the NAS, if it should be fetched from
    /// there when it is not on disk yet.
    ///
    /// No default: where an appliance keeps such a file is a decision whoever
    /// set it up made, and guessing one would be this client asserting a
    /// layout it has no business knowing.
    vpn_profile_remote: Option<&'a str>,
}

/// Put SMB in front of the HTTP client, on the best leg that answers now.
///
/// The same three legs the CLI has, in the same order, deciding the same way:
/// this consumer had its own shortened version that only ever tried SMB at the
/// public host, so a NAS reachable only through a tunnel was invisible to it.
///
/// SMB is attached even when no leg answers, with no session, so the
/// connection can move to it later; the legs that come back are what watches
/// for that.
async fn reach(
    client: SynologyClient,
    settings: Reachable<'_>,
) -> (SynologyClient, Option<Arc<Legs>>) {
    // Fetched before anything asks for a tunnel, over the session just
    // authenticated — which is what lets somebody outside the NAS's network
    // get the file that gets them inside it.
    //
    // Only when told where it lives, though. Without that, whatever is already
    // on disk is what there is.
    if let (Some(local), Some(remote)) = (settings.vpn_profile, settings.vpn_profile_remote) {
        let source = ProfileSource {
            remote: remote.to_string(),
            local: PathBuf::from(local),
        };
        if let Err(e) = source.ensure(&client).await {
            warn!("VPN profile: {e}; the tunnel leg will not be available");
        }
    }

    let endpoints = match settings.vpn_host {
        Some(inside) => Endpoints::with_tunnel(settings.host, inside),
        None => {
            // A profile with nowhere to land is a setting half-filled in.
            // Raising the tunnel would still leave no address to reach the NAS
            // at, so the escalation is skipped — and said out loud, because
            // from the outside it looks exactly like a tunnel that failed.
            if settings.vpn_profile.is_some() {
                warn!(
                    "Transport: a VPN profile is set but no in-tunnel address, so the tunnel \
                     cannot be used. The NAS answers at a private address inside it, which its \
                     public name does not resolve to."
                );
            }
            Endpoints::public_only(settings.host)
        }
    };
    let tunnel: Box<dyn Tunnel> = match settings.vpn_profile {
        Some(profile) => Box::new(OpenVpnTunnel::new(
            profile,
            settings.username,
            settings.password,
            settings.domain,
            VPN_PATIENCE,
        )),
        None => Box::new(NoTunnel),
    };
    // The one leg this consumer lets be switched off, and only from the
    // environment — the escape hatch `auto_connect` has always honoured.
    let smb_disabled = std::env::var_os("SYNOLOGY_FS_SMB_DISABLE").is_some();
    let policy = TransportPolicy::from_flags(smb_disabled, false, false)
        .expect("HTTP is never disabled here, so something is always allowed");
    // Built before the chain: the probe has to ask the port this config will
    // dial (`SYNOLOGY_FS_SMB_PORT`), or it decides the leg on one nothing uses.
    let cfg = SmbConfig::for_account(
        settings.host,
        settings.username,
        settings.password,
        settings.domain,
    );
    // Shared rather than owned: the SMB transport keeps a handle to it so a
    // dead stream can be reopened long after this call returned, and the leg
    // watch asks it for better ones.
    let chain = Arc::new(
        Chain::new(
            policy,
            endpoints,
            Box::new(TcpProber::on_port(cfg.port, SMB_PROBE_TIMEOUT)),
            tunnel,
            DEFAULT_RECHECK,
        )
        .with_smb_port(cfg.port),
    );

    // Reachable is not the same as usable: the port can answer and the
    // session on top of it still be refused — which is exactly what an AD
    // account with no domain does. So what the badge reports is the session,
    // not the probe; `Legs` says which, and says so in the log when it
    // changes.
    match Legs::start(chain, &cfg).await {
        Some(legs) => (
            synology_filestation_smb::attach(client, legs.session().clone()),
            Some(legs),
        ),
        None => (client, None),
    }
}

/// Opaque live-mount handle.
pub struct SynoMount {
    // Never read: dropping it is what unmounts, and teardown drops the whole
    // mount rather than reaching in for this.
    _handle: synology_filestation_fuse::MountHandle,
    // Keeps the runtime alive for the duration of the mount (the FUSE/WinFsp
    // callbacks block on it). Dropped after the mount is torn down.
    _runtime: Arc<Runtime>,
}

// ─── Connect / session lifecycle ─────────────────────────────────────────────

/// Log in and return an opaque client handle in `*out`.
///
/// Returns [`SynoStatus::OtpRequired`] (no client created) when the account
/// needs a 2FA code; the caller should prompt and call again with `otp` set.
///
/// # Safety
/// All string args must be valid NUL-terminated UTF-8 or null where optional.
#[no_mangle]
pub unsafe extern "C" fn syno_connect(
    host: *const c_char,
    port: u16,
    https: bool,
    username: *const c_char,
    password: *const c_char,
    otp: *const c_char,
    auto_relogin: bool,
    // When false, accept any TLS certificate. A DSM appliance ships self-signed,
    // so the GUI exposes this as a per-connection checkbox — but it makes the
    // connection encrypted without being authenticated, so it is opt-in.
    verify_ssl: bool,
    // The NetBIOS domain the account lives in, for an appliance joined to a
    // directory; null for a local DSM user. Both legs that authenticate
    // against the directory need it, for different reasons. SMB: without it a
    // directory account is checked against the appliance's own accounts,
    // fails, and SMB is silently skipped — the reason this consumer has been
    // on the HTTP path even on a network where the NAS answers directly. VPN:
    // DSM refuses a name that does not carry it before the password is looked
    // at, and reports only `AUTH_FAILED`.
    domain: *const c_char,
    // Where the OpenVPN profile is kept, and where the NAS answers inside the
    // tunnel it describes. Both null means no escalation is possible, and a
    // NAS that does not answer directly is reached over HTTP as before.
    vpn_profile: *const c_char,
    vpn_host: *const c_char,
    // Where that profile lives on the NAS, if it should be fetched from there
    // when it is not on disk yet. Null to use only what is already on disk.
    vpn_profile_remote: *const c_char,
    out: *mut *mut SynoClient,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let host = match req_str(host, "host", err) {
            Ok(v) => v,
            Err(c) => return c,
        };
        let username = match req_str(username, "username", err) {
            Ok(v) => v,
            Err(c) => return c,
        };
        let password = match req_str(password, "password", err) {
            Ok(v) => v,
            Err(c) => return c,
        };
        let otp = opt_str(otp);
        let domain = opt_str(domain);
        let vpn_profile = opt_str(vpn_profile);
        let vpn_host = opt_str(vpn_host);
        let vpn_profile_remote = opt_str(vpn_profile_remote);
        if out.is_null() {
            return set_err(err, SynoStatus::NullArg, 0, "out pointer must not be null");
        }

        let client = if auto_relogin {
            SynologyClient::with_auto_relogin(host, port, https)
        } else {
            SynologyClient::new(host, port, https)
        };
        let client = if verify_ssl {
            client
        } else {
            client.with_insecure_tls()
        };

        // Default-on throttle for the FFI/GUI consumer: cap concurrency and
        // bound retries with jittered backoff so a bulk download from the GUI's
        // file browser can't saturate the appliance's shared synoscgi backend.
        // The rate-limit belt is intentionally left off (min_interval = 0):
        // this same client Arc backs a GUI-initiated mount, and spacing every
        // ranged block read would stall interactive streaming. The pure-Rust
        // FUSE/CLI mount path (main.rs) attaches no throttle at all.
        let client = client.with_throttle(ThrottleConfig {
            max_concurrency: 4,
            min_interval: Duration::from_millis(0),
            max_attempts: 5,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        });

        // Attempt login on a cheap current-thread runtime that spawns no worker
        // threads. Only on success do we build the persistent multi-thread
        // runtime the client/mount will use — so a wrong password or an
        // OTP-required first call doesn't churn OS threads per attempt.
        let probe_rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                return set_err(
                    err,
                    SynoStatus::Io,
                    0,
                    &format!("failed to start runtime: {e}"),
                )
            }
        };

        match probe_rt.block_on(client.login(username, password, otp)) {
            Ok(()) => {
                drop(probe_rt);
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => Arc::new(rt),
                    Err(e) => {
                        return set_err(
                            err,
                            SynoStatus::Io,
                            0,
                            &format!("failed to start runtime: {e}"),
                        )
                    }
                };
                // Which leg reaches the NAS, decided the same way the CLI
                // decides it: SMB directly, SMB through a tunnel this process
                // raises, or the HTTP API. The GUI's downloads and a
                // GUI-initiated mount both ride whatever this attaches, so the
                // choice is worth logging — the log pane is where a user finds
                // out they are on the slow path. And it is looked at again
                // while the handle lives, so the slow path is not for good.
                let (client, legs) = runtime.block_on(reach(
                    client,
                    Reachable {
                        host,
                        username,
                        password,
                        domain,
                        vpn_profile,
                        vpn_host,
                        vpn_profile_remote,
                    },
                ));
                let watching = legs
                    .as_ref()
                    .map(|legs| legs.watch(runtime.handle(), DEFAULT_WATCH_INTERVAL));
                let handle = Box::new(SynoClient {
                    inner: Arc::new(client),
                    runtime,
                    legs,
                    watching,
                });
                *out = Box::into_raw(handle);
                SynoStatus::Ok as i32
            }
            Err(e) if is_otp_required(&e) => set_err(
                err,
                SynoStatus::OtpRequired,
                0,
                "two-factor authentication code required",
            ),
            Err(e) => {
                // Present as a login failure regardless of the underlying code,
                // but preserve the DSM code in `dsm_code` for diagnostics.
                set_core_err(err, &SynoFsError::LoginFailed(Box::new(e)))
            }
        }
    })
}

/// Best-effort logout. Safe to call once; does not free the handle.
///
/// # Safety
/// `client` must be a live handle from [`syno_connect`] (or null).
#[no_mangle]
pub unsafe extern "C" fn syno_logout(client: *mut SynoClient) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if let Some(c) = client.as_ref() {
            // First, and not left to the free: the GUI logs out before it
            // frees, and on a slow teardown the gap between them runs past
            // twenty seconds. A look landing in it could raise a tunnel — a
            // real directory login — for a connection that is going away.
            if let Some(watching) = &c.watching {
                watching.abort();
            }
            info!("teardown: logging out");
            let _ = c.runtime.block_on(c.inner.logout());
            info!("teardown: logged out");
        }
    }));
}

/// Run `work` on a thread of its own, and wait `patience` for it to finish.
///
/// Returns whether it did. On a timeout the work is **left running**, not
/// abandoned: it owns the Tokio runtime that the filesystem's leftover workers
/// call back into, and releasing that while they are still running turns a hang
/// into a panic. What the deadline buys is the caller's thread, not the work's.
fn finish_within<F>(patience: Duration, work: F) -> bool
where
    F: FnOnce() + Send + 'static,
{
    let (finished, waiting) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        work();
        // Nobody may be listening any more, which is the point of this.
        let _ = finished.send(());
    });
    waiting.recv_timeout(patience).is_ok()
}

/// Free a client handle. After this the pointer is invalid.
///
/// # Safety
/// `client` must have come from [`syno_connect`] and not been freed already.
#[no_mangle]
pub unsafe extern "C" fn syno_client_free(client: *mut SynoClient) {
    if !client.is_null() {
        // Dropping the last `Arc<Runtime>` blocks this thread until the
        // runtime has shut down — which waits on anything still running in it.
        // The GUI blocks on this call, so if it never returns the window sits
        // on "Working…"; saying where we are is what makes that legible.
        info!("teardown: releasing the client");
        drop(Box::from_raw(client));
        info!("teardown: client released");
    }
}

/// Free a string returned by this library (e.g. `SynoError::message` or any
/// `out_json`).
///
/// # Safety
/// `s` must have come from this library and not been freed already.
#[no_mangle]
pub unsafe extern "C" fn syno_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

// ─── Helpers shared by the client ops ────────────────────────────────────────

/// Run an async op on the client's runtime with SID-expiry relogin retry.
unsafe fn run_op<F, Fut, T>(client: *mut SynoClient, op: F) -> Result<T, RunErr<SynoFsError>>
where
    F: Fn(Arc<SynologyClient>) -> Fut,
    Fut: std::future::Future<Output = Result<T, SynoFsError>>,
{
    let c = client.as_ref().ok_or(RunErr::Null)?;
    let inner = c.inner.clone();
    c.runtime
        .block_on(async { inner.with_relogin_retry(|| op(inner.clone())).await })
        .map_err(RunErr::Op)
}

enum RunErr<E> {
    Null,
    Op(E),
}

unsafe fn finish<T>(
    res: Result<T, RunErr<SynoFsError>>,
    err: *mut SynoError,
    on_ok: impl FnOnce(T) -> i32,
) -> i32 {
    match res {
        Ok(v) => on_ok(v),
        Err(RunErr::Null) => set_err(err, SynoStatus::NullArg, 0, "client must not be null"),
        Err(RunErr::Op(e)) => set_core_err(err, &e),
    }
}

// ─── Browse ──────────────────────────────────────────────────────────────────

/// List the top-level shares as a JSON array of file-info objects.
///
/// # Safety
/// `client` must be live; `out_json` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_list_shares(
    client: *mut SynoClient,
    out_json: *mut *mut c_char,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let res = run_op(client, |c| async move { c.list_shares().await });
        finish(res, err, |items| {
            let arr: Vec<_> = items.iter().map(fileinfo_json).collect();
            emit_json(out_json, &serde_json::Value::Array(arr), err)
        })
    })
}

/// List a directory as a JSON array of file-info objects.
///
/// # Safety
/// `client` must be live; `path`/`out_json` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_list_dir(
    client: *mut SynoClient,
    path: *const c_char,
    out_json: *mut *mut c_char,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let path = match req_str(path, "path", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let res = run_op(client, move |c| {
            let path = path.clone();
            async move { c.list_dir(&path).await }
        });
        finish(res, err, |items| {
            let arr: Vec<_> = items.iter().map(fileinfo_json).collect();
            emit_json(out_json, &serde_json::Value::Array(arr), err)
        })
    })
}

/// Get info for a single path as a JSON file-info object.
///
/// # Safety
/// `client` must be live; `path`/`out_json` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_get_info(
    client: *mut SynoClient,
    path: *const c_char,
    out_json: *mut *mut c_char,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let path = match req_str(path, "path", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let res = run_op(client, move |c| {
            let path = path.clone();
            async move { c.get_info(&path).await }
        });
        finish(res, err, |info| {
            emit_json(out_json, &fileinfo_json(&info), err)
        })
    })
}

// ─── Transfers ───────────────────────────────────────────────────────────────

/// Progress callback: `(bytes_done, bytes_total, user_data)`. `total` is 0 when
/// the size is unknown.
pub type ProgressCb = extern "C" fn(done: u64, total: u64, user_data: *mut c_void);

fn report(cb: Option<ProgressCb>, user_data: *mut c_void, done: u64, total: u64) {
    if let Some(cb) = cb {
        cb(done, total, user_data);
    }
}

/// Download `remote` to `local`, reporting progress via the optional callback.
/// Streams the file in chunks so progress is fine-grained; writes to a
/// `.part` sidecar and renames on success for atomicity.
///
/// # Safety
/// `client` must be live; `remote`/`local` must be non-null. `progress` (if
/// non-null) and `user_data` must remain valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn syno_download_to(
    client: *mut SynoClient,
    remote: *const c_char,
    local: *const c_char,
    progress: Option<ProgressCb>,
    user_data: *mut c_void,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let remote = match req_str(remote, "remote", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let local = match req_str(local, "local", err) {
            Ok(v) => PathBuf::from(v),
            Err(c) => return c,
        };
        let c = match client.as_ref() {
            Some(c) => c,
            None => return set_err(err, SynoStatus::NullArg, 0, "client must not be null"),
        };
        let inner = c.inner.clone();

        // Stream into a `.part` sidecar, renamed on success for atomicity.
        // Append (rather than replace the extension) so `a.txt` and `a.csv`
        // get distinct sidecars and we never collide with an unrelated
        // `<stem>.part`. Computed out here so failure paths can clean it up.
        let part = {
            let mut p = local.clone().into_os_string();
            p.push(".syno-part");
            PathBuf::from(p)
        };

        let result: Result<(), SynoFsError> = c.runtime.block_on(async {
            use std::io::Write as _;

            // Determine total size up front for progress reporting.
            let total = inner
                .with_relogin_retry(|| inner.get_info(&remote))
                .await
                .ok()
                .and_then(|i| i.additional.and_then(|a| a.size))
                .unwrap_or(0);
            report(progress, user_data, 0, total);

            let mut file = std::fs::File::create(&part)
                .map_err(|e| SynoFsError::Io(format!("create {}: {e}", part.display())))?;

            let mut offset = 0u64;
            loop {
                let bytes = inner
                    .with_relogin_retry(|| inner.download(&remote, offset, DOWNLOAD_CHUNK))
                    .await?;
                if bytes.is_empty() {
                    break;
                }
                file.write_all(&bytes)
                    .map_err(|e| SynoFsError::Io(format!("write {}: {e}", part.display())))?;
                offset += bytes.len() as u64;
                // Report `total` as-is: 0 means "unknown" (the documented
                // contract). Using total.max(offset) would make an unknown-size
                // transfer report done == total every chunk, i.e. always 100%.
                report(progress, user_data, offset, total);
                // Stop at EOF: a short final chunk, or — when the size is known
                // and is an exact multiple of the chunk — having read it all
                // (avoids a spurious past-EOF range request).
                if (bytes.len() as u64) < DOWNLOAD_CHUNK || (total > 0 && offset >= total) {
                    break;
                }
            }
            file.flush()
                .map_err(|e| SynoFsError::Io(format!("flush {}: {e}", part.display())))?;
            drop(file);
            std::fs::rename(&part, &local)
                .map_err(|e| SynoFsError::Io(format!("rename into {}: {e}", local.display())))?;
            report(progress, user_data, offset, offset);
            Ok(())
        });

        match result {
            Ok(()) => SynoStatus::Ok as i32,
            Err(e) => {
                // Best-effort: don't leave a stale partial file behind on
                // failure (range error mid-transfer, rename failure, etc.).
                let _ = std::fs::remove_file(&part);
                set_core_err(err, &e)
            }
        }
    })
}

/// Upload `local` into the remote directory `remote_dir`. Progress is reported
/// per slice for files large enough to be sliced, else 0 → total.
///
/// # Safety
/// `client` must be live; `local`/`remote_dir` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_upload(
    client: *mut SynoClient,
    local: *const c_char,
    remote_dir: *const c_char,
    overwrite: bool,
    progress: Option<ProgressCb>,
    user_data: *mut c_void,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let local = match req_str(local, "local", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let remote_dir = match req_str(remote_dir, "remote_dir", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let c = match client.as_ref() {
            Some(c) => c,
            None => return set_err(err, SynoStatus::NullArg, 0, "client must not be null"),
        };

        // Streams from disk: large files go out in slices, so memory stays
        // bounded and progress can move per slice instead of jumping 0 → 100%.
        let filename = match std::path::Path::new(&local)
            .file_name()
            .and_then(|n| n.to_str())
        {
            Some(n) => n.to_string(),
            None => return set_err(err, SynoStatus::InvalidArg, 0, "local path has no filename"),
        };
        let total = match std::fs::metadata(&local) {
            Ok(m) => m.len(),
            Err(e) => return set_err(err, SynoStatus::Io, 0, &format!("stat {local}: {e}")),
        };
        report(progress, user_data, 0, total);

        // `user_data` travels as a usize for the same reason the log sink does:
        // it is an opaque token we never deref, and that keeps the closure Send.
        let ud = user_data as usize;
        let sink = move |done: u64, total: u64| report(progress, ud as *mut c_void, done, total);
        let local_path = std::path::PathBuf::from(&local);
        let inner = c.inner.clone();
        let result = c.runtime.block_on(async {
            inner
                .with_relogin_retry(|| {
                    inner.upload_from_path_with_progress(
                        &local_path,
                        &remote_dir,
                        &filename,
                        overwrite,
                        Some(&sink),
                    )
                })
                .await
        });
        match result {
            Ok(()) => {
                report(progress, user_data, total, total);
                SynoStatus::Ok as i32
            }
            Err(e) => set_core_err(err, &e),
        }
    })
}

/// Delete a file or directory.
///
/// # Safety
/// `client` must be live; `path` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_delete(
    client: *mut SynoClient,
    path: *const c_char,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let path = match req_str(path, "path", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let res = run_op(client, move |c| {
            let path = path.clone();
            async move { c.delete(&path).await }
        });
        finish(res, err, |()| SynoStatus::Ok as i32)
    })
}

/// Create a folder named `name` under `parent`.
///
/// # Safety
/// `client` must be live; `parent`/`name` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_create_folder(
    client: *mut SynoClient,
    parent: *const c_char,
    name: *const c_char,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let parent = match req_str(parent, "parent", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let name = match req_str(name, "name", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let res = run_op(client, move |c| {
            let parent = parent.clone();
            let name = name.clone();
            async move { c.create_folder(&parent, &name).await.map(|_| ()) }
        });
        finish(res, err, |()| SynoStatus::Ok as i32)
    })
}

/// Rename a file or directory (same-directory rename only).
///
/// # Safety
/// `client` must be live; `path`/`new_name` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_rename(
    client: *mut SynoClient,
    path: *const c_char,
    new_name: *const c_char,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let path = match req_str(path, "path", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let new_name = match req_str(new_name, "new_name", err) {
            Ok(v) => v.to_string(),
            Err(c) => return c,
        };
        let res = run_op(client, move |c| {
            let path = path.clone();
            let new_name = new_name.clone();
            async move { c.rename(&path, &new_name).await.map(|_| ()) }
        });
        finish(res, err, |()| SynoStatus::Ok as i32)
    })
}

// ─── Mount lifecycle ─────────────────────────────────────────────────────────

/// Mount the FileStation share at `mountpoint`, in-process, on a background
/// thread. Returns immediately with a [`SynoMount`] handle in `*out` once the
/// mount is established (or an error if it fails to come up).
///
/// # Safety
/// `client` must be live; `mountpoint`/`out` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn syno_mount(
    client: *mut SynoClient,
    mountpoint: *const c_char,
    cache_ttl: u64,
    read_cache_mb: u64,
    // Speculative read-ahead depth in blocks; `0` switches it off. A bulk
    // consumer walking a corpus reads each file once and every speculative
    // block is waste, so this has to be reachable from the GUI too.
    prefetch_blocks: u64,
    out: *mut *mut SynoMount,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        let mountpoint = match req_str(mountpoint, "mountpoint", err) {
            Ok(v) => PathBuf::from(v),
            Err(c) => return c,
        };
        if out.is_null() {
            return set_err(err, SynoStatus::NullArg, 0, "out pointer must not be null");
        }
        let c = match client.as_ref() {
            Some(c) => c,
            None => return set_err(err, SynoStatus::NullArg, 0, "client must not be null"),
        };

        // The log file and the mountpoint are chosen independently here, so
        // this is the first moment the pair is known. Mounting anyway would
        // wedge on the first record written from inside a callback.
        if let Some(log) = logging::conflicts_with_mount(&mountpoint) {
            return set_err(
                err,
                SynoStatus::InvalidArg,
                0,
                &format!(
                    "the log file {} is inside the mountpoint {}. The log would be \
                     written through the mount it is describing, which deadlocks it. \
                     Choose a log path on local disk.",
                    log.display(),
                    mountpoint.display()
                ),
            );
        }

        let opts = MountOptions {
            cache_ttl,
            read_cache_mb,
            prefetch_blocks,
            ..MountOptions::default()
        };
        match spawn_mount(
            c.inner.clone(),
            c.runtime.handle().clone(),
            mountpoint,
            opts,
        ) {
            Ok(handle) => {
                let mount = Box::new(SynoMount {
                    _handle: handle,
                    _runtime: c.runtime.clone(),
                });
                *out = Box::into_raw(mount);
                SynoStatus::Ok as i32
            }
            Err(e) => set_core_err(err, &e),
        }
    })
}

/// Unmount and free a mount handle. Blocks until the background worker exits.
/// After this the pointer is invalid.
///
/// # Safety
/// `mount` must have come from [`syno_mount`] and not been freed already.
#[no_mangle]
pub unsafe extern "C" fn syno_unmount(mount: *mut SynoMount) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if mount.is_null() {
            return;
        }
        let mount = Box::from_raw(mount);

        // Unmount, then wait for the filesystem to finish — but not forever.
        //
        // `stop()` unmounts and then *joins* the worker threads. The unmount
        // is the part that matters to whoever asked: the volume is gone from
        // the filesystem the moment it returns. The join is bookkeeping, and a
        // worker still inside a callback — waiting on a NAS that has stopped
        // answering — will not reach the point where it notices the unmount
        // until that request gives up, which can be minutes.
        //
        // Waiting for that is what hung the GUI: it blocks on this call, so
        // the window sat on a progress bar with the volume already gone.
        //
        // The whole mount moves onto the thread, its handle on the runtime
        // included. That is not tidiness: the leftover workers call back into
        // the runtime, and releasing it while they are still running turns
        // this hang into a panic.
        info!("teardown: unmounting and waiting for the filesystem to finish");
        if finish_within(UNMOUNT_PATIENCE, move || drop(mount)) {
            info!("teardown: filesystem finished");
        } else {
            warn!(
                "teardown: the volume is unmounted, but the filesystem has not finished after {UNMOUNT_PATIENCE:?} — a request to the NAS is still outstanding. Closing it in the background rather than waiting."
            );
        }
    }));
}

// ─── Logging ─────────────────────────────────────────────────────────────────

/// Register a callback that receives the library's log lines. Pass a null
/// `cb` to clear it. Installs the tracing subscriber on first call.
///
/// # Safety
/// `cb`/`user_data` must remain valid until cleared with a null `cb`.
#[no_mangle]
pub unsafe extern "C" fn syno_set_log_callback(cb: Option<logging::LogCb>, user_data: *mut c_void) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        logging::set_callback(cb, user_data);
    }));
}

/// Set the log verbosity: one of "error", "warn", "info", "debug", "trace"
/// (anything else falls back to "info"). No-op until a callback is registered.
///
/// # Safety
/// `level` must be a valid NUL-terminated UTF-8 string, or null (treated as "info").
#[no_mangle]
pub unsafe extern "C" fn syno_set_log_level(level: *const c_char) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        logging::set_level(opt_str(level).unwrap_or("info"));
    }));
}

/// Also write every log record to `path`, so the log outlives the process.
///
/// The GUI's log pane goes away with the GUI, exactly as a terminal goes away
/// with its window — including when a wedged mount forces a power cycle, which
/// is the run worth reading afterwards. Records reach the OS as they are
/// emitted, and warnings and errors are forced to disk, so a killed or
/// powered-off process still leaves them behind. The file rotates at 8 MiB,
/// keeping three older generations beside it.
///
/// Pass null to stop writing to a file. Returns `Ok` with the resolved path
/// written to `out` (free it with [`syno_string_free`]) — the caller asked for
/// a path, but parent directories are created and a relative path is made
/// absolute, so where it actually landed is worth reporting.
///
/// # Safety
/// `path` must be a valid NUL-terminated UTF-8 string, or null. `out` may be
/// null if the caller does not want the resolved path.
#[no_mangle]
pub unsafe extern "C" fn syno_set_log_file(
    path: *const c_char,
    out: *mut *mut c_char,
    err: *mut SynoError,
) -> i32 {
    guard(err, || {
        if !out.is_null() {
            *out = ptr::null_mut();
        }
        let Some(path) = opt_str(path) else {
            logging::clear_file();
            return SynoStatus::Ok as i32;
        };
        match logging::set_file(Path::new(path)) {
            Ok(resolved) => {
                if !out.is_null() {
                    // A path that cannot survive the trip is not worth failing
                    // the call over: the file is already open and collecting.
                    if let Ok(s) = CString::new(resolved.to_string_lossy().into_owned()) {
                        *out = s.into_raw();
                    }
                }
                SynoStatus::Ok as i32
            }
            Err(e) => set_err(err, SynoStatus::Io, 0, &e.to_string()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn empty_err() -> SynoError {
        SynoError {
            kind: 0,
            dsm_code: 0,
            message: ptr::null_mut(),
        }
    }

    unsafe fn err_message(err: &SynoError) -> String {
        if err.message.is_null() {
            String::new()
        } else {
            CStr::from_ptr(err.message).to_string_lossy().into_owned()
        }
    }

    /// Host/port for the FFI from a mock server URI ("http://127.0.0.1:PORT").
    fn host_port(server: &MockServer) -> (CString, u16) {
        let uri = server.uri();
        let without = uri.trim_start_matches("http://");
        let (host, port) = without.rsplit_once(':').unwrap();
        (cstr(host), port.parse().unwrap())
    }

    /// A multi-thread runtime that outlives the test so the mock server's
    /// background task keeps running while the (separate) FFI runtime drives
    /// blocking calls on the test's main thread.
    fn server_with<F>(setup: F) -> (Runtime, MockServer)
    where
        F: std::future::Future<Output = MockServer>,
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(setup);
        (rt, server)
    }

    fn login_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"sid": "sid-123"}
        }))
    }

    #[test]
    fn work_that_finishes_in_time_is_waited_for() {
        assert!(finish_within(Duration::from_secs(5), || {}));
    }

    #[test]
    fn work_that_does_not_finish_gives_the_caller_its_thread_back() {
        // The disconnect that hung. Unmounting a volume whose NAS has stopped
        // answering joins worker threads that are inside a callback, and the
        // GUI blocks on that call — with the volume already gone, because the
        // unmount is what happens first. So waiting buys nothing.
        let (release, held) = std::sync::mpsc::channel::<()>();

        let started = std::time::Instant::now();
        let finished = finish_within(Duration::from_millis(200), move || {
            let _ = held.recv();
        });

        assert!(!finished);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it stopped waiting rather than waiting anyway"
        );
        // And left the work running rather than abandoning it, because it owns
        // what the stragglers are still using.
        release
            .send(())
            .expect("the work is still there to be released");
    }

    #[test]
    fn a_client_is_freed_promptly_rather_than_parking_a_thread() {
        // What a disconnect does, and what it must not do: `syno_client_free`
        // drops the last `Arc<Runtime>`, and dropping a Tokio runtime blocks
        // the calling thread until the runtime has shut down. The GUI calls
        // this from a thread pool worker and waits for it, so anything that
        // stops the runtime shutting down stops the disconnect — and the whole
        // window sits on "Working…" with nothing to say.
        let (_rt, server) = server_with(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/webapi/auth.cgi"))
                .respond_with(login_ok())
                .mount(&server)
                .await;
            server
        });

        let (host, port) = host_port(&server);
        let freed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watching = std::sync::Arc::clone(&freed);

        // On its own thread, so a hang is a failed assertion rather than a
        // test run that never ends.
        std::thread::spawn(move || unsafe {
            let mut client: *mut SynoClient = ptr::null_mut();
            let mut err = empty_err();
            let rc = syno_connect(
                host.as_ptr(),
                port,
                false,
                cstr("alice").as_ptr(),
                cstr("secret").as_ptr(),
                ptr::null(),
                false,
                true,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut client,
                &mut err,
            );
            assert_eq!(rc, SynoStatus::Ok as i32);
            syno_logout(client);
            syno_client_free(client);
            watching.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        for _ in 0..100 {
            if freed.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("syno_client_free did not return: the runtime never shut down");
    }

    #[test]
    fn connect_then_list_shares() {
        let (_rt, server) = server_with(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/webapi/auth.cgi"))
                .respond_with(login_ok())
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/webapi/entry.cgi"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": true,
                    "data": {"shares": [{"name": "home", "path": "/home", "isdir": true}]}
                })))
                .mount(&server)
                .await;
            server
        });

        unsafe {
            let (host, port) = host_port(&server);
            let mut client: *mut SynoClient = ptr::null_mut();
            let mut err = empty_err();
            let rc = syno_connect(
                host.as_ptr(),
                port,
                false,
                cstr("alice").as_ptr(),
                cstr("secret").as_ptr(),
                ptr::null(),
                false,
                true,
                // No SMB domain and no tunnel: these exercise the login path,
                // and SMB is not reachable from a wiremock server anyway.
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut client,
                &mut err,
            );
            assert_eq!(rc, SynoStatus::Ok as i32, "connect: {}", err_message(&err));
            assert!(!client.is_null());

            let mut out_json: *mut c_char = ptr::null_mut();
            let rc = syno_list_shares(client, &mut out_json, &mut err);
            assert_eq!(rc, SynoStatus::Ok as i32, "list: {}", err_message(&err));
            let json = CStr::from_ptr(out_json).to_string_lossy().into_owned();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value[0]["name"], "home");
            assert_eq!(value[0]["isdir"], true);

            syno_string_free(out_json);
            syno_logout(client);
            syno_client_free(client);
        }
    }

    #[test]
    fn connect_reports_otp_required() {
        let (_rt, server) = server_with(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/webapi/auth.cgi"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": false, "error": {"code": 403}
                })))
                .mount(&server)
                .await;
            server
        });

        unsafe {
            let (host, port) = host_port(&server);
            let mut client: *mut SynoClient = ptr::null_mut();
            let mut err = empty_err();
            let rc = syno_connect(
                host.as_ptr(),
                port,
                false,
                cstr("alice").as_ptr(),
                cstr("secret").as_ptr(),
                ptr::null(),
                false,
                true,
                // No SMB domain and no tunnel: these exercise the login path,
                // and SMB is not reachable from a wiremock server anyway.
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut client,
                &mut err,
            );
            assert_eq!(rc, SynoStatus::OtpRequired as i32);
            assert!(client.is_null());
            syno_string_free(err.message);
        }
    }

    #[test]
    fn connect_bad_credentials_is_login_failed() {
        let (_rt, server) = server_with(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/webapi/auth.cgi"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": false, "error": {"code": 400}
                })))
                .mount(&server)
                .await;
            server
        });

        unsafe {
            let (host, port) = host_port(&server);
            let mut client: *mut SynoClient = ptr::null_mut();
            let mut err = empty_err();
            let rc = syno_connect(
                host.as_ptr(),
                port,
                false,
                cstr("alice").as_ptr(),
                cstr("wrong").as_ptr(),
                ptr::null(),
                false,
                true,
                // No SMB domain and no tunnel: these exercise the login path,
                // and SMB is not reachable from a wiremock server anyway.
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut client,
                &mut err,
            );
            assert_eq!(rc, SynoStatus::LoginFailed as i32);
            assert_eq!(err.dsm_code, 400, "DSM code preserved on `.dsm_code`");
            syno_string_free(err.message);
        }
    }

    #[test]
    fn dsm_119_classifies_as_sid_not_found() {
        let (_rt, server) = server_with(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/webapi/auth.cgi"))
                .respond_with(login_ok())
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/webapi/entry.cgi"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": false, "error": {"code": 119}
                })))
                .mount(&server)
                .await;
            server
        });

        unsafe {
            let (host, port) = host_port(&server);
            let mut client: *mut SynoClient = ptr::null_mut();
            let mut err = empty_err();
            // auto_relogin = false so the 119 surfaces instead of triggering a retry.
            let rc = syno_connect(
                host.as_ptr(),
                port,
                false,
                cstr("alice").as_ptr(),
                cstr("secret").as_ptr(),
                ptr::null(),
                false,
                true,
                // No SMB domain and no tunnel: these exercise the login path,
                // and SMB is not reachable from a wiremock server anyway.
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut client,
                &mut err,
            );
            assert_eq!(rc, SynoStatus::Ok as i32, "connect: {}", err_message(&err));

            let mut out_json: *mut c_char = ptr::null_mut();
            let rc = syno_list_dir(client, cstr("/home").as_ptr(), &mut out_json, &mut err);
            assert_eq!(rc, SynoStatus::SidNotFound as i32);
            assert_eq!(err.dsm_code, 119);

            syno_string_free(err.message);
            syno_logout(client);
            syno_client_free(client);
        }
    }

    #[test]
    fn null_client_is_null_arg_not_panic() {
        unsafe {
            let mut out_json: *mut c_char = ptr::null_mut();
            let mut err = empty_err();
            let rc = syno_list_shares(ptr::null_mut(), &mut out_json, &mut err);
            assert_eq!(rc, SynoStatus::NullArg as i32);
            syno_string_free(err.message);
        }
    }

    /// An address in TEST-NET-1 (RFC 5737), which is guaranteed to answer
    /// nothing — so the probe below fails for the reason the test is about
    /// rather than because of whatever the machine running it happens to have
    /// listening on 445.
    const NOWHERE: &str = "192.0.2.1";

    #[tokio::test]
    async fn a_nas_that_answers_nothing_falls_to_http_and_says_so() {
        // The consumer never disables a leg, so this is the only way it reaches
        // the HTTP path: SMB unreachable, and no profile with which to escalate.
        let client = SynologyClient::new(NOWHERE, 5001, true);

        let (_client, legs) = reach(
            client,
            Reachable {
                host: NOWHERE,
                // Empty, because nothing here authenticates: the probe fails
                // before any credential is used. A literal pair in a test is
                // also exactly what a secret scanner is built to notice, and it
                // is right to — so the fix is not to write one.
                username: "",
                password: "",
                domain: Some("EXAMPLE"),
                vpn_profile: None,
                vpn_host: None,
                vpn_profile_remote: None,
            },
        )
        .await;

        assert_eq!(transport_of(legs.as_deref()), SynoTransport::Https);
        // And SMB is attached anyway, with no session, so that the connection
        // can move to it when it answers. This used to be decided once: a GUI
        // that connected while SMB was unreachable stayed on HTTP, with its
        // badge saying so, until the user disconnected and connected again.
        let legs = legs.expect("a place for SMB to go");
        assert!(!legs.session().is_connected());
    }

    /// Regression: the leg watch was stopped only when the handle was freed,
    /// and the GUI logs out first — a gap that runs past twenty seconds when
    /// teardown is slow. A look landing in it could raise a tunnel, which is a
    /// real directory login, for a connection that was already going away.
    #[test]
    fn logging_out_stops_the_leg_watch() {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("a runtime"),
        );
        let watching = runtime.spawn(std::future::pending::<()>());
        let mut client = SynoClient {
            // Refused at once, so the logout itself is quick.
            inner: Arc::new(SynologyClient::new("127.0.0.1", 1, false)),
            runtime: runtime.clone(),
            legs: None,
            watching: Some(watching),
        };

        unsafe { syno_logout(&mut client) };

        let stopped = runtime.block_on(async {
            for _ in 0..100 {
                if client.watching.as_ref().is_none_or(|w| w.is_finished()) {
                    return true;
                }
                tokio::task::yield_now().await;
            }
            false
        });
        assert!(stopped, "the watch is stopped by the logout, not the free");
    }

    #[test]
    fn a_handle_that_is_not_there_reports_no_transport() {
        // The GUI asks this of whatever it holds; a null is the answer to "you
        // have not connected", not a reason to read through it.
        assert_eq!(unsafe { syno_transport(std::ptr::null()) }, -1);
    }

    #[test]
    fn the_legs_are_ordered_worst_to_best() {
        // The C ABI promises this so a badge can compare them without knowing
        // what any of them mean.
        assert!(SynoTransport::Https < SynoTransport::SmbOverVpn);
        assert!(SynoTransport::SmbOverVpn < SynoTransport::SmbDirect);
    }
}
