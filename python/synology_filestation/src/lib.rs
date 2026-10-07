//! PyO3 bindings for the Synology FileStation HTTP client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use synology_filestation_core::error::{dsm_code_to_category, ErrorCategory, SynoFsError};
use synology_filestation_core::types::SynoFileInfo;
use synology_filestation_core::{SynologyClient, ThrottleConfig};
use synology_filestation_smb::{Fallback, Probe, SmbTransport};
use tokio::runtime::Runtime;

/// Which transport a client was given at login, and why.
///
/// Kept because the fallback to HTTP used to be silent: a username SMB would
/// not accept spent weeks failing its probe — each failure a strike towards
/// DSM's auto-block — while every transfer quietly went through FileStation.
struct TransportChoice {
    smb: Option<Arc<SmbTransport>>,
    fallback: Option<Fallback>,
}

impl TransportChoice {
    /// `"smb"` while the SMB session is up; `"http"` otherwise.
    fn name(&self) -> &'static str {
        match &self.smb {
            Some(smb) if smb.is_connected() => "smb",
            _ => "http",
        }
    }

    /// Why the client is on HTTP: a stable code, or `None` on SMB.
    fn reason(&self) -> Option<&'static str> {
        match (&self.smb, &self.fallback) {
            (Some(smb), _) if smb.is_connected() => None,
            // Attached, but the link has since gone; it is rebuilt on the
            // next operation that can use it.
            (Some(_), _) => Some("disconnected"),
            (None, Some(f)) => Some(f.kind.as_str()),
            (None, None) => None,
        }
    }

    /// The fallback in words, for a person; `None` on SMB.
    fn detail(&self) -> Option<String> {
        match (&self.smb, &self.fallback) {
            (Some(smb), _) if smb.is_connected() => None,
            (Some(_), _) => Some("the SMB session was lost; HTTP until it is rebuilt".into()),
            (None, f) => f.as_ref().map(|f| f.detail.clone()),
        }
    }
}

/// Probe SMB with the login's credentials and put it in front of HTTP when it
/// answers. Injecting a backend consumes the client, so this runs before the
/// client is frozen into an `Arc`.
async fn prefer_smb(
    client: SynologyClient,
    host: &str,
    username: &str,
    password: &str,
    domain: Option<&str>,
) -> (SynologyClient, TransportChoice) {
    match synology_filestation_smb::probe_as(host, username, password, domain).await {
        Probe::Smb(smb) => (
            synology_filestation_smb::attach(client, smb.clone()),
            TransportChoice {
                smb: Some(smb),
                fallback: None,
            },
        ),
        Probe::Http(fallback) => (
            client,
            TransportChoice {
                smb: None,
                fallback: Some(fallback),
            },
        ),
    }
}

/// Apply the request throttle (concurrency cap + rate belt + bounded jittered
/// backoff) to a freshly built client. Enabled by default for the Python
/// surface because it is the bulk-transfer consumer (e.g. a Temporal pipeline
/// staging `.ORF` files) that can saturate the NAS's shared `synoscgi` backend.
/// Pass `throttle=False` to opt out.
#[allow(clippy::too_many_arguments)]
fn apply_throttle(
    client: SynologyClient,
    throttle: bool,
    max_concurrency: usize,
    min_interval_ms: u64,
    max_attempts: u32,
    backoff_base_ms: u64,
    backoff_max_ms: u64,
) -> SynologyClient {
    if !throttle {
        return client;
    }
    client.with_throttle(ThrottleConfig {
        max_concurrency,
        min_interval: Duration::from_millis(min_interval_ms),
        max_attempts,
        backoff_base: Duration::from_millis(backoff_base_ms),
        backoff_max: Duration::from_millis(backoff_max_ms),
    })
}

// ─── Exception hierarchy ─────────────────────────────────────────────────────
//
// Each typed exception carries `.code` (the DSM numeric code, or None) and
// `.message` (the human-readable string) as instance attributes — see
// `synofs_to_pyerr` for how those are populated.

create_exception!(
    synology_filestation,
    FileStationError,
    PyException,
    "Base class for all Synology FileStation errors raised by this package."
);

create_exception!(
    synology_filestation,
    AuthError,
    FileStationError,
    "Authentication failed (wrong credentials, invalid OTP, etc.)."
);

create_exception!(
    synology_filestation,
    SidNotFound,
    AuthError,
    "DSM session ID expired or unknown (DSM error 119). When auto-relogin is \
     enabled this is only raised if re-login itself fails."
);

create_exception!(
    synology_filestation,
    NoSuchFile,
    FileStationError,
    "File or folder does not exist (DSM codes 403, 414, 415)."
);

create_exception!(
    synology_filestation,
    PermissionDenied,
    FileStationError,
    "Permission denied (DSM codes 408, 1805)."
);

create_exception!(
    synology_filestation,
    AlreadyExists,
    FileStationError,
    "Path already exists (DSM codes 418, 1101)."
);

create_exception!(
    synology_filestation,
    NoSpace,
    FileStationError,
    "Quota or disk-space exhausted (DSM codes 419, 1804)."
);

create_exception!(
    synology_filestation,
    NotEmpty,
    FileStationError,
    "Directory not empty (DSM code 416)."
);

create_exception!(
    synology_filestation,
    InvalidArg,
    FileStationError,
    "Invalid parameter (DSM code 400)."
);

create_exception!(
    synology_filestation,
    NotSupported,
    FileStationError,
    "Operation is not supported by this DSM version or by this binding."
);

create_exception!(
    synology_filestation,
    TransportError,
    FileStationError,
    "Network or parse error talking to DSM (no DSM error code available)."
);

create_exception!(
    synology_filestation,
    TlsError,
    TransportError,
    "The NAS's TLS certificate could not be verified.\n\n\
     A DSM appliance ships with a self-signed certificate, so this is the \
     expected first-connect result against a stock NAS. Either install the \
     certificate in the system trust store, or pass `verify_ssl=False` to \
     accept any certificate — the connection is then encrypted but not \
     authenticated, so anything able to intercept it can read your password.\n\n\
     Subclasses TransportError, so existing `except TransportError` handlers \
     still catch it."
);

create_exception!(
    synology_filestation,
    DSMError,
    FileStationError,
    "Unmapped DSM error. The numeric code is available on `.code`."
);

/// Map a `SynoFsError` to the right typed Python exception, attaching `.code`
/// and `.message` so callers can inspect them. Mapping mirrors the errno
/// mapping in `synology_filestation_core::error` so the same DSM code lands
/// in the same place across both backends.
fn synofs_to_pyerr(py: Python<'_>, err: SynoFsError) -> PyErr {
    let message = err.to_string();

    // A rejected certificate is checked ahead of everything else, including the
    // LoginFailed wrapper. It happens during login, so it would otherwise
    // present as AuthError — but nothing was authenticated, or even asked: the
    // handshake failed. Reporting it as bad credentials sends the user to fix
    // the wrong thing. Mirrors the same ordering in the FFI's `classify`.
    if err.is_tls_error() {
        return PyErr::new::<TlsError, _>(message);
    }

    // Per-variant routing first; the ApiError branch maps DSM codes onto the
    // same typed exceptions so callers see consistent classes regardless of
    // whether the error came from a raw HTTP layer or a per-entry envelope.
    let (cls, code): (Bound<'_, PyAny>, Option<u32>) = match &err {
        SynoFsError::NotFound => (py.get_type::<NoSuchFile>().into_any(), None),
        SynoFsError::PermissionDenied => (py.get_type::<PermissionDenied>().into_any(), None),
        SynoFsError::AlreadyExists => (py.get_type::<AlreadyExists>().into_any(), None),
        SynoFsError::NotEmpty => (py.get_type::<NotEmpty>().into_any(), None),
        SynoFsError::InvalidArg => (py.get_type::<InvalidArg>().into_any(), None),
        SynoFsError::NoSpace => (py.get_type::<NoSpace>().into_any(), None),
        SynoFsError::NotSupported => (py.get_type::<NotSupported>().into_any(), None),
        SynoFsError::Io(_) => (py.get_type::<TransportError>().into_any(), None),
        SynoFsError::LoginFailed(inner) => {
            // Auth failures always present as AuthError, but we preserve the
            // underlying DSM code so callers can inspect what specifically
            // went wrong (account locked, OTP required, etc.).
            let inner_code = match inner.as_ref() {
                SynoFsError::ApiError(c) => Some(*c),
                _ => None,
            };
            (py.get_type::<AuthError>().into_any(), inner_code)
        }
        SynoFsError::ApiError(c) => {
            // Classify via the core's shared category table so the Python
            // exceptions and the FFI status codes agree on every DSM code.
            let cls = match dsm_code_to_category(*c) {
                ErrorCategory::SidExpired => py.get_type::<SidNotFound>().into_any(),
                ErrorCategory::InvalidArg => py.get_type::<InvalidArg>().into_any(),
                ErrorCategory::NotFound => py.get_type::<NoSuchFile>().into_any(),
                ErrorCategory::PermissionDenied => py.get_type::<PermissionDenied>().into_any(),
                ErrorCategory::NotEmpty => py.get_type::<NotEmpty>().into_any(),
                ErrorCategory::AlreadyExists => py.get_type::<AlreadyExists>().into_any(),
                ErrorCategory::NoSpace => py.get_type::<NoSpace>().into_any(),
                ErrorCategory::NotSupported => py.get_type::<NotSupported>().into_any(),
                // Busy (402) and any unmapped code stay a generic DSMError, as
                // before; Transport never originates from an ApiError code.
                ErrorCategory::Busy | ErrorCategory::Transport | ErrorCategory::Other => {
                    py.get_type::<DSMError>().into_any()
                }
            };
            (cls, Some(*c))
        }
    };

    // Construct the exception instance with the message as args[0], then set
    // `.code` and `.message` for caller convenience. If anything in here
    // fails, fall back to the underlying error from PyO3 so we never panic.
    let instance = match cls.call1((message.clone(),)) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let _ = instance.setattr("message", message);
    let _ = match code {
        Some(c) => instance.setattr("code", c),
        None => instance.setattr("code", py.None()),
    };
    PyErr::from_value(instance)
}

/// Split a full remote path like ``/share/sub/file.txt`` into
/// ``(/share/sub, file.txt)``. Empty filename or trailing-slash inputs are
/// treated as `InvalidArg` — DSM rejects uploads with no filename, and we'd
/// rather fail fast than have it look like a valid request that 400s
/// server-side. Trailing slashes are checked *before* the split so a path
/// like ``/share/`` is rejected (caller meant a directory, not a file)
/// instead of being misinterpreted as filename "share" under root.
fn split_remote_path(remote_path: &str) -> PyResult<(String, String)> {
    if remote_path.ends_with('/') || remote_path.is_empty() {
        return Err(PyErr::new::<InvalidArg, _>(
            "remote_path must end in a filename, not '/'",
        ));
    }
    let (parent, name) = remote_path
        .rsplit_once('/')
        .ok_or_else(|| PyErr::new::<InvalidArg, _>("remote_path must contain a '/' separator"))?;
    if name.is_empty() {
        return Err(PyErr::new::<InvalidArg, _>(
            "remote_path must end in a filename, not '/'",
        ));
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok((parent.to_string(), name.to_string()))
}

// ─── File-info helper ────────────────────────────────────────────────────────

/// Convert a `SynoFileInfo` to a Python dict with a stable, documented shape.
/// Keys are always present even when the underlying `additional` block is
/// missing, so downstream code can do `info["size"] is None` instead of a
/// missing-key check.
fn fileinfo_to_pydict<'py>(py: Python<'py>, info: &SynoFileInfo) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("name", &info.name)?;
    dict.set_item("path", &info.path)?;
    dict.set_item("isdir", info.isdir)?;

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
    dict.set_item("size", size)?;
    dict.set_item("mtime", mtime)?;
    dict.set_item("atime", atime)?;
    dict.set_item("ctime", ctime)?;
    dict.set_item("perm", perm)?;
    Ok(dict)
}

// ─── Client (sync) ───────────────────────────────────────────────────────────

#[pyclass]
struct Client {
    inner: Arc<SynologyClient>,
    runtime: Arc<Runtime>,
    transport: TransportChoice,
}

impl Client {
    /// Run a future on the per-Client Tokio runtime and convert the error.
    /// All public methods funnel through this so the Tokio runtime is the
    /// single sync↔async seam.
    fn run<F, T>(&self, py: Python<'_>, fut: F) -> PyResult<T>
    where
        F: std::future::Future<Output = Result<T, SynoFsError>> + Send,
        T: Send,
    {
        // We must release the GIL while blocking on async work — otherwise
        // every Python thread waiting on a network call would serialize and
        // deadlock-prone code in user-land would pay through the nose.
        py.detach(|| self.runtime.block_on(fut))
            .map_err(|e| synofs_to_pyerr(py, e))
    }
}

#[pymethods]
impl Client {
    #[staticmethod]
    #[pyo3(signature = (host, port, username, password, *, https=true, verify_ssl=true, otp=None, auto_relogin=true, throttle=true, max_concurrency=4, min_interval_ms=150, max_attempts=5, backoff_base_ms=1000, backoff_max_ms=60000, domain=None))]
    #[allow(clippy::too_many_arguments)] // mirrors the Python kwargs surface
    fn login(
        py: Python<'_>,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        https: bool,
        verify_ssl: bool,
        otp: Option<&str>,
        auto_relogin: bool,
        throttle: bool,
        max_concurrency: usize,
        min_interval_ms: u64,
        max_attempts: u32,
        backoff_base_ms: u64,
        backoff_max_ms: u64,
        domain: Option<&str>,
    ) -> PyResult<Self> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| {
                    PyErr::new::<TransportError, _>(format!("failed to start runtime: {e}"))
                })?,
        );
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
        let client = apply_throttle(
            client,
            throttle,
            max_concurrency,
            min_interval_ms,
            max_attempts,
            backoff_base_ms,
            backoff_max_ms,
        );

        // Log in, then transparently prefer SMB when it's reachable — bulk
        // transfers then avoid the HTTP Download/Upload API (and the synoscgi
        // backend it can saturate). `auto_connect` returns None on any failure,
        // so this silently degrades to HTTP; it runs before the client is frozen
        // into an Arc because injecting a backend consumes the client.
        //
        // Login failures are always AuthError regardless of the underlying DSM
        // code (wrong password / OTP required / …); wrap in LoginFailed so
        // synofs_to_pyerr routes to the AuthError class.
        let rt = runtime.clone();
        let (client, transport) = py
            .detach(move || {
                rt.block_on(async move {
                    client
                        .login(username, password, otp)
                        .await
                        .map_err(|e| SynoFsError::LoginFailed(Box::new(e)))?;
                    Ok::<_, SynoFsError>(prefer_smb(client, host, username, password, domain).await)
                })
            })
            .map_err(|e| synofs_to_pyerr(py, e))?;

        Ok(Self {
            inner: Arc::new(client),
            runtime,
            transport,
        })
    }

    /// `"smb"` or `"http"`: which transport file operations prefer. SMB keeps
    /// bulk transfers off FileStation, so `"http"` on a host that can reach SMB
    /// is worth reading `transport_reason` for.
    #[getter]
    fn transport(&self) -> &'static str {
        self.transport.name()
    }

    /// Why the client is on HTTP, or `None` on SMB: `"disabled"`,
    /// `"unreachable"`, `"auth_refused"`, `"auth_cooldown"` or `"disconnected"`.
    #[getter]
    fn transport_reason(&self) -> Option<&'static str> {
        self.transport.reason()
    }

    /// The fallback in words — for a refusal, what to change. `None` on SMB.
    #[getter]
    fn transport_detail(&self) -> Option<String> {
        self.transport.detail()
    }

    fn logout(&self, py: Python<'_>) -> PyResult<()> {
        let inner = self.inner.clone();
        self.run(py, async move { inner.logout().await })
    }

    fn exists(&self, py: Python<'_>, path: &str) -> PyResult<bool> {
        let inner = self.inner.clone();
        let path = path.to_string();
        let result = py.detach(|| {
            self.runtime
                .block_on(async { inner.with_relogin_retry(|| inner.get_info(&path)).await })
        });
        match result {
            Ok(_) => Ok(true),
            Err(SynoFsError::NotFound) => Ok(false),
            // 408 from getinfo on a missing path is the standard "no entry"
            // shape (per-entry code in files[0]); also treat as not-existing.
            Err(SynoFsError::ApiError(414 | 415 | 403 | 408)) => Ok(false),
            Err(e) => Err(synofs_to_pyerr(py, e)),
        }
    }

    fn getinfo<'py>(&self, py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyDict>> {
        let inner = self.inner.clone();
        let path_owned = path.to_string();
        let info = py
            .detach(|| {
                self.runtime.block_on(async {
                    inner
                        .with_relogin_retry(|| inner.get_info(&path_owned))
                        .await
                })
            })
            // DSM uses code 408 in per-entry context to mean "no permission
            // OR no such file" — the two are indistinguishable from the API
            // surface. For getinfo specifically, a missing-or-inaccessible
            // path should look like NoSuchFile to Python callers, mirroring
            // os.stat's FileNotFoundError on a path you can't reach.
            .map_err(|e| match e {
                SynoFsError::ApiError(408) => synofs_to_pyerr(py, SynoFsError::NotFound),
                other => synofs_to_pyerr(py, other),
            })?;
        fileinfo_to_pydict(py, &info)
    }

    #[pyo3(signature = (path, *, offset=0, length=0))]
    fn download<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        offset: u64,
        length: u64,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let inner = self.inner.clone();
        let path_owned = path.to_string();
        let bytes = py
            .detach(|| {
                self.runtime.block_on(async {
                    inner
                        .with_relogin_retry(|| inner.download(&path_owned, offset, length))
                        .await
                })
            })
            .map_err(|e| synofs_to_pyerr(py, e))?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// MD5 of ``path`` as lowercase hex, computed by the NAS
    /// (``SYNO.FileStation.MD5``) — nothing is downloaded. DSM reads the whole
    /// file to produce it, so this takes time proportional to the file's size
    /// (up to a 15-minute ceiling), and on a throttled client it holds a
    /// transfer slot for that whole time, exactly like a download.
    fn md5(&self, py: Python<'_>, path: &str) -> PyResult<String> {
        let inner = self.inner.clone();
        let path_owned = path.to_string();
        self.run(py, async move {
            inner.with_relogin_retry(|| inner.md5(&path_owned)).await
        })
    }

    fn list_dir<'py>(&self, py: Python<'py>, path: &str) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let inner = self.inner.clone();
        let path_owned = path.to_string();
        let entries = py
            .detach(|| {
                self.runtime.block_on(async {
                    inner
                        .with_relogin_retry(|| inner.list_dir(&path_owned))
                        .await
                })
            })
            .map_err(|e| synofs_to_pyerr(py, e))?;
        entries.iter().map(|f| fileinfo_to_pydict(py, f)).collect()
    }

    fn list_shares<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let inner = self.inner.clone();
        let shares = py
            .detach(|| {
                self.runtime
                    .block_on(async { inner.with_relogin_retry(|| inner.list_shares()).await })
            })
            .map_err(|e| synofs_to_pyerr(py, e))?;
        shares.iter().map(|s| fileinfo_to_pydict(py, s)).collect()
    }

    /// Upload raw bytes to ``remote_path``. Splits the path into a parent
    /// directory + filename internally so callers don't have to. Mirrors
    /// fsspec's ``pipe_file`` shape.
    #[pyo3(signature = (remote_path, data, *, overwrite=true))]
    fn upload_bytes(
        &self,
        py: Python<'_>,
        remote_path: &str,
        data: &[u8],
        overwrite: bool,
    ) -> PyResult<()> {
        let (folder, filename) = split_remote_path(remote_path)?;
        let buf = data.to_vec();
        let inner = self.inner.clone();
        py.detach(|| {
            self.runtime.block_on(async {
                inner
                    .with_relogin_retry(|| inner.upload(&folder, &filename, buf.clone(), overwrite))
                    .await
            })
        })
        .map_err(|e| synofs_to_pyerr(py, e))
    }

    fn download_to(&self, py: Python<'_>, remote_path: &str, local_path: &str) -> PyResult<()> {
        let inner = self.inner.clone();
        let remote = remote_path.to_string();
        let local: PathBuf = local_path.into();
        py.detach(|| {
            self.runtime
                .block_on(async { inner.download_to_path(&remote, &local).await })
        })
        .map_err(|e| synofs_to_pyerr(py, e))
    }

    #[pyo3(signature = (local_path, remote_dir, *, overwrite=true, _create_parents=true))]
    fn upload(
        &self,
        py: Python<'_>,
        local_path: &str,
        remote_dir: &str,
        overwrite: bool,
        _create_parents: bool,
    ) -> PyResult<()> {
        // Filename is the last path component.
        let filename = std::path::Path::new(local_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| PyErr::new::<InvalidArg, _>("local_path has no filename"))?
            .to_string();

        let inner = self.inner.clone();
        let local = std::path::PathBuf::from(local_path);
        let remote_dir = remote_dir.to_string();
        // Streams the file straight to SMB when available (no in-memory copy);
        // falls back to reading it in + HTTP upload otherwise. API unchanged.
        py.detach(|| {
            self.runtime.block_on(async {
                inner
                    .with_relogin_retry(|| {
                        inner.upload_from_path(&local, &remote_dir, &filename, overwrite)
                    })
                    .await
            })
        })
        .map_err(|e| synofs_to_pyerr(py, e))
    }

    #[pyo3(signature = (parent_dir, name, *, _force_parent=true))]
    fn create_folder(
        &self,
        py: Python<'_>,
        parent_dir: &str,
        name: &str,
        _force_parent: bool,
    ) -> PyResult<()> {
        let inner = self.inner.clone();
        let parent = parent_dir.to_string();
        let name = name.to_string();
        py.detach(|| {
            self.runtime.block_on(async {
                inner
                    .with_relogin_retry(|| inner.create_folder(&parent, &name))
                    .await
            })
        })
        .map_err(|e| synofs_to_pyerr(py, e))?;
        Ok(())
    }

    fn delete(&self, py: Python<'_>, remote_path: &str) -> PyResult<()> {
        let inner = self.inner.clone();
        let path = remote_path.to_string();
        py.detach(|| {
            self.runtime
                .block_on(async { inner.with_relogin_retry(|| inner.delete(&path)).await })
        })
        .map_err(|e| synofs_to_pyerr(py, e))
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[allow(unused_variables)]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Py<PyAny>,
        exc_value: Py<PyAny>,
        traceback: Py<PyAny>,
    ) -> PyResult<bool> {
        // Best-effort logout: a failure during logout shouldn't shadow an
        // exception in the `with` body, so swallow logout errors entirely.
        let inner = self.inner.clone();
        let _ = py.detach(|| self.runtime.block_on(async move { inner.logout().await }));
        Ok(false) // do not suppress exceptions raised inside the `with` block
    }
}

// ─── AsyncClient (pyo3-async-runtimes) ───────────────────────────────────────
//
// Mirrors `Client` but every method returns an awaitable. Internally the
// futures run on the pyo3-async-runtimes Tokio runtime; we don't spin our own
// per-instance runtime here because await points are explicit and there's no
// sync→async bridge to manage.

use pyo3_async_runtimes::tokio::future_into_py;

/// Convert `SynoFsError` to PyErr while holding the GIL. Used inside async
/// blocks where we don't have a Python token in scope but need to construct
/// a typed Python exception before returning.
fn synofs_to_pyerr_gil(e: SynoFsError) -> PyErr {
    Python::attach(|py| synofs_to_pyerr(py, e))
}

/// Same as `synofs_to_pyerr_gil` but applies the getinfo-specific 408 →
/// NoSuchFile remap. Centralized so sync and async getinfo agree.
fn getinfo_err_gil(e: SynoFsError) -> PyErr {
    Python::attach(|py| match e {
        SynoFsError::ApiError(408) => synofs_to_pyerr(py, SynoFsError::NotFound),
        other => synofs_to_pyerr(py, other),
    })
}

#[pyclass]
struct AsyncClient {
    inner: Arc<SynologyClient>,
    transport: TransportChoice,
}

#[pymethods]
impl AsyncClient {
    #[staticmethod]
    #[pyo3(signature = (host, port, username, password, *, https=true, verify_ssl=true, otp=None, auto_relogin=true, throttle=true, max_concurrency=4, min_interval_ms=150, max_attempts=5, backoff_base_ms=1000, backoff_max_ms=60000, domain=None))]
    #[allow(clippy::too_many_arguments)] // mirrors the Python kwargs surface
    fn login<'py>(
        py: Python<'py>,
        host: String,
        port: u16,
        username: String,
        password: String,
        https: bool,
        verify_ssl: bool,
        otp: Option<String>,
        auto_relogin: bool,
        throttle: bool,
        max_concurrency: usize,
        min_interval_ms: u64,
        max_attempts: u32,
        backoff_base_ms: u64,
        backoff_max_ms: u64,
        domain: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            let client = if auto_relogin {
                SynologyClient::with_auto_relogin(&host, port, https)
            } else {
                SynologyClient::new(&host, port, https)
            };
            let client = if verify_ssl {
                client
            } else {
                client.with_insecure_tls()
            };
            let client = apply_throttle(
                client,
                throttle,
                max_concurrency,
                min_interval_ms,
                max_attempts,
                backoff_base_ms,
                backoff_max_ms,
            );
            client
                .login(&username, &password, otp.as_deref())
                .await
                .map_err(|e| synofs_to_pyerr_gil(SynoFsError::LoginFailed(Box::new(e))))?;
            // Transparently prefer SMB when reachable; HTTP otherwise.
            let (client, transport) =
                prefer_smb(client, &host, &username, &password, domain.as_deref()).await;
            Ok(AsyncClient {
                inner: Arc::new(client),
                transport,
            })
        })
    }

    /// `"smb"` or `"http"`: which transport file operations prefer. SMB keeps
    /// bulk transfers off FileStation, so `"http"` on a host that can reach SMB
    /// is worth reading `transport_reason` for.
    #[getter]
    fn transport(&self) -> &'static str {
        self.transport.name()
    }

    /// Why the client is on HTTP, or `None` on SMB: `"disabled"`,
    /// `"unreachable"`, `"auth_refused"`, `"auth_cooldown"` or `"disconnected"`.
    #[getter]
    fn transport_reason(&self) -> Option<&'static str> {
        self.transport.reason()
    }

    /// The fallback in words — for a refusal, what to change. `None` on SMB.
    #[getter]
    fn transport_detail(&self) -> Option<String> {
        self.transport.detail()
    }

    fn logout<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner.logout().await.map_err(synofs_to_pyerr_gil)
        })
    }

    fn exists<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let result = inner.with_relogin_retry(|| inner.get_info(&path)).await;
            match result {
                Ok(_) => Ok(true),
                Err(SynoFsError::NotFound) => Ok(false),
                Err(SynoFsError::ApiError(414 | 415 | 403 | 408)) => Ok(false),
                Err(e) => Err(synofs_to_pyerr_gil(e)),
            }
        })
    }

    fn getinfo<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let info = inner
                .with_relogin_retry(|| inner.get_info(&path))
                .await
                .map_err(getinfo_err_gil)?;
            Python::attach(|py| -> PyResult<Py<PyDict>> {
                fileinfo_to_pydict(py, &info).map(|d| d.unbind())
            })
        })
    }

    #[pyo3(signature = (path, *, offset=0, length=0))]
    fn download<'py>(
        &self,
        py: Python<'py>,
        path: String,
        offset: u64,
        length: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let bytes = inner
                .with_relogin_retry(|| inner.download(&path, offset, length))
                .await
                .map_err(synofs_to_pyerr_gil)?;
            Python::attach(|py| -> PyResult<Py<PyBytes>> { Ok(PyBytes::new(py, &bytes).unbind()) })
        })
    }

    /// MD5 of ``path`` as lowercase hex, computed by the NAS
    /// (``SYNO.FileStation.MD5``) — nothing is downloaded. DSM reads the whole
    /// file to produce it, so this takes time proportional to the file's size
    /// (up to a 15-minute ceiling), and on a throttled client it holds a
    /// transfer slot for that whole time, exactly like a download.
    fn md5<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .with_relogin_retry(|| inner.md5(&path))
                .await
                .map_err(synofs_to_pyerr_gil)
        })
    }

    fn list_dir<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let entries = inner
                .with_relogin_retry(|| inner.list_dir(&path))
                .await
                .map_err(synofs_to_pyerr_gil)?;
            Python::attach(|py| -> PyResult<Py<pyo3::types::PyList>> {
                let dicts: Vec<Bound<'_, PyDict>> = entries
                    .iter()
                    .map(|f| fileinfo_to_pydict(py, f))
                    .collect::<PyResult<_>>()?;
                Ok(pyo3::types::PyList::new(py, dicts)?.unbind())
            })
        })
    }

    fn list_shares<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let shares = inner
                .with_relogin_retry(|| inner.list_shares())
                .await
                .map_err(synofs_to_pyerr_gil)?;
            Python::attach(|py| -> PyResult<Py<pyo3::types::PyList>> {
                let dicts: Vec<Bound<'_, PyDict>> = shares
                    .iter()
                    .map(|s| fileinfo_to_pydict(py, s))
                    .collect::<PyResult<_>>()?;
                Ok(pyo3::types::PyList::new(py, dicts)?.unbind())
            })
        })
    }

    #[pyo3(signature = (remote_path, data, *, overwrite=true))]
    fn upload_bytes<'py>(
        &self,
        py: Python<'py>,
        remote_path: String,
        data: Vec<u8>,
        overwrite: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (folder, filename) = split_remote_path(&remote_path)?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .with_relogin_retry(|| inner.upload(&folder, &filename, data.clone(), overwrite))
                .await
                .map_err(synofs_to_pyerr_gil)
        })
    }

    fn download_to<'py>(
        &self,
        py: Python<'py>,
        remote_path: String,
        local_path: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let local: PathBuf = local_path.into();
            inner
                .download_to_path(&remote_path, &local)
                .await
                .map_err(synofs_to_pyerr_gil)
        })
    }

    #[pyo3(signature = (local_path, remote_dir, *, overwrite=true, _create_parents=true))]
    fn upload<'py>(
        &self,
        py: Python<'py>,
        local_path: String,
        remote_dir: String,
        overwrite: bool,
        _create_parents: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let filename = std::path::Path::new(&local_path)
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| {
                    Python::attach(|_py| PyErr::new::<InvalidArg, _>("local_path has no filename"))
                })?
                .to_string();
            let local = std::path::PathBuf::from(&local_path);
            // Streams to SMB when available (no in-memory copy); HTTP fallback
            // otherwise. API unchanged.
            inner
                .with_relogin_retry(|| {
                    inner.upload_from_path(&local, &remote_dir, &filename, overwrite)
                })
                .await
                .map_err(synofs_to_pyerr_gil)
        })
    }

    /// Stream a local file to a full remote path (`/share/dir/name`). Like
    /// `upload` but the remote filename may differ from the local one. Streams
    /// to SMB when available, HTTP fallback otherwise.
    #[pyo3(signature = (local_path, remote_path, *, overwrite=true))]
    fn upload_file<'py>(
        &self,
        py: Python<'py>,
        local_path: String,
        remote_path: String,
        overwrite: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let (folder, filename) = split_remote_path(&remote_path)?;
            let local = std::path::PathBuf::from(&local_path);
            inner
                .with_relogin_retry(|| {
                    inner.upload_from_path(&local, &folder, &filename, overwrite)
                })
                .await
                .map_err(synofs_to_pyerr_gil)
        })
    }

    #[pyo3(signature = (parent_dir, name, *, _force_parent=true))]
    fn create_folder<'py>(
        &self,
        py: Python<'py>,
        parent_dir: String,
        name: String,
        _force_parent: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .with_relogin_retry(|| inner.create_folder(&parent_dir, &name))
                .await
                .map_err(synofs_to_pyerr_gil)?;
            Ok(())
        })
    }

    fn delete<'py>(&self, py: Python<'py>, remote_path: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .with_relogin_retry(|| inner.delete(&remote_path))
                .await
                .map_err(synofs_to_pyerr_gil)
        })
    }
}

// ─── Module registration ─────────────────────────────────────────────────────

#[pymodule]
fn _native(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // `tracing` (built with its `log` feature, below) emits `log` records when
    // no subscriber is installed, and pyo3-log hands those to `logging`.
    // `tracing_log::LogTracer` used to be installed here first; it is the
    // bridge the other way, `log` into `tracing`, and by taking the one global
    // `log` slot it left pyo3-log uninstalled — no Rust warning ever reached
    // Python.
    let _ = pyo3_log::try_init();

    m.add_class::<Client>()?;
    m.add_class::<AsyncClient>()?;

    m.add("FileStationError", py.get_type::<FileStationError>())?;
    m.add("AuthError", py.get_type::<AuthError>())?;
    m.add("SidNotFound", py.get_type::<SidNotFound>())?;
    m.add("NoSuchFile", py.get_type::<NoSuchFile>())?;
    m.add("PermissionDenied", py.get_type::<PermissionDenied>())?;
    m.add("AlreadyExists", py.get_type::<AlreadyExists>())?;
    m.add("NoSpace", py.get_type::<NoSpace>())?;
    m.add("NotEmpty", py.get_type::<NotEmpty>())?;
    m.add("InvalidArg", py.get_type::<InvalidArg>())?;
    m.add("NotSupported", py.get_type::<NotSupported>())?;
    m.add("TransportError", py.get_type::<TransportError>())?;
    m.add("TlsError", py.get_type::<TlsError>())?;
    m.add("DSMError", py.get_type::<DSMError>())?;

    Ok(())
}
