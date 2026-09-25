//! Reusable mount library for the Synology FileStation driver.
//!
//! This crate is built as both a library and a binary. The binary
//! ([`main.rs`](../main.rs)) is the CLI; this library exposes the platform
//! mount backends (`fs`/`cache` on Linux, `webdav` on macOS, `winfs` on
//! Windows) behind a single **spawnable** API so the CLI *and* the FFI binding
//! ([`synology-filestation-ffi`]) can drive a mount without blocking the
//! caller's thread.
//!
//! The key difference from the old in-`main` mount code is that
//! [`spawn_mount`] returns a [`MountHandle`] immediately and the filesystem
//! runs on a background thread/task; [`MountHandle::stop`] unmounts and joins.
//! The CLI re-creates its previous blocking behaviour by simply parking on
//! `ctrl_c()` and then calling `stop()`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use synology_filestation_connect::{Chain, SmbRoute};
use synology_filestation_core::client::SynologyClient;
use synology_filestation_core::error::SynoFsError;
use synology_filestation_smb::{BoxedStream, RedialFuture};

/// Default umask for the synthetic mode the Linux backend reports, matching the
/// 0o755 directories / 0o644 files a process with the usual umask creates.
pub const DEFAULT_UMASK: u16 = 0o022;

/// How the SMB transport gets a new stream when the one it is running on dies.
///
/// The tunnel leg is the one that cannot heal itself. Its stream belongs to a
/// userspace TCP stack inside an in-process OpenVPN client, so `SmbClient` has
/// nothing it could redial — the address at the far end is reachable only from
/// inside the tunnel — and the transport was built unable to reconnect at all.
/// One stream EOF then ended SMB for the life of the mount: the
/// transport-selection breaker re-probed every 30 s, each probe met the same
/// dead session, and every operation paid a guaranteed failure before falling
/// back to the HTTP API.
///
/// [`Chain::reach_smb`] is what closes that hole. It opens a fresh connection
/// through the tunnel, and brings the tunnel itself back up when that is what
/// is missing — so the redial is one call, and it is the same call the mount
/// made at startup.
///
/// It also answers a question the first stream's answer does not settle:
/// **which leg** reaches the NAS now. A laptop carried back onto the network
/// gets `Direct`, and a direct address is a stream too — dialling it here is
/// what lets the mount that came up through a tunnel go on working over TCP,
/// on the same transport, without a remount. Where the bytes come from is
/// exactly what this layer exists to stop mattering.
///
/// Every reopening says how long the stream it is replacing lasted, and which
/// leg answered. Those two numbers are the diagnosis: a stream that dies after
/// the same round number every time is a server-side session or idle timeout,
/// and one that dies at ragged intervals is the link under it. Without them a
/// mount losing its stream hourly reported only "Disconnected from server",
/// once per operation, which says that something ended but nothing about what.
///
/// A direct dial goes to `dial.port` and gives up after `dial.patience`: see
/// [`Dial`].
///
/// Shared by the CLI and the FFI binding, which build the same transport.
pub fn reopen_through(
    chain: Arc<Chain>,
    dial: Dial,
) -> impl Fn() -> RedialFuture + Send + Sync + 'static + use<> {
    // When the stream being replaced was opened. Advanced only when a new one
    // is handed back, so a redial that fails and is retried still reports the
    // age of the stream that actually died rather than the retry interval.
    let opened_at = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    move || {
        let chain = chain.clone();
        let opened_at = opened_at.clone();
        Box::pin(async move {
            let lasted = opened_at
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .elapsed()
                .as_secs();
            let opened = |what: &str| {
                *opened_at.lock().unwrap_or_else(|e| e.into_inner()) = std::time::Instant::now();
                tracing::warn!("SMB: the stream lasted {lasted}s; reopened {what}");
            };
            match chain.reach_smb().await {
                Ok(SmbRoute::Tunnelled { connection, .. }) => {
                    opened("through the tunnel");
                    Ok(Box::new(connection) as BoxedStream)
                }
                Ok(SmbRoute::Direct { host }) => {
                    let stream = dial_direct(&host, dial).await.inspect_err(|e| {
                        tracing::warn!(
                            "SMB: the stream lasted {lasted}s; {host} answers directly \
                             but would not connect: {e}"
                        );
                    })?;
                    opened(&format!("directly to {host}"));
                    Ok(stream)
                }
                Ok(SmbRoute::Unavailable) => {
                    tracing::warn!(
                        "SMB: the stream lasted {lasted}s and cannot be reopened — \
                         no leg reaches the NAS; staying on the HTTP API"
                    );
                    Err(SynoFsError::Io("SMB cannot be reached at all".into()))
                }
                Err(e) => {
                    tracing::warn!(
                        "SMB: the stream lasted {lasted}s and cannot be reopened yet: {e}"
                    );
                    Err(SynoFsError::Io(format!("{e}")))
                }
            }
        })
    }
}

/// How to dial a host the chain says answers directly: which port, and how
/// long to wait.
///
/// Both come from the SMB config — [`SmbConfig::for_account`] reads
/// `SYNOLOGY_FS_SMB_PORT` and `SYNOLOGY_FS_SMB_TIMEOUT_MS` — so the probe
/// that chooses the leg, the dial that moves to it and the redial that brings
/// it back all agree on where SMB is. They used to disagree: the port override
/// reached only the old self-dialling path, and everything the chain drove
/// dialled 445.
///
/// The wait is bounded because a redial runs holding the transport's lock,
/// and a direct decision never expires: off campus, a bare connect to a port
/// that drops packets waits out the kernel's own timeout — about two minutes
/// on Linux — with every operation on the mount queued behind it rather than
/// falling back to HTTP.
///
/// [`SmbConfig::for_account`]: synology_filestation_smb::SmbConfig::for_account
#[derive(Debug, Clone, Copy)]
pub struct Dial {
    /// Used when the host carries no port of its own.
    pub port: u16,
    pub patience: Duration,
}

impl Dial {
    pub fn for_config(cfg: &synology_filestation_smb::SmbConfig) -> Self {
        Self {
            port: cfg.port,
            patience: cfg.timeout,
        }
    }
}

/// Dial a host the chain says answers directly, as `dial` says.
///
/// The port is appended only when the host does not already carry one, the
/// same rule `SmbConfig::addr` follows — a route naming `host:port` means that
/// port, and appending another to it produces an address that resolves to
/// nothing.
async fn dial_direct(host: &str, dial: Dial) -> Result<BoxedStream, SynoFsError> {
    let Dial { port, patience } = dial;
    let addr = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    match tokio::time::timeout(patience, tokio::net::TcpStream::connect(&addr)).await {
        Ok(Ok(stream)) => Ok(Box::new(stream) as BoxedStream),
        Ok(Err(e)) => Err(SynoFsError::Io(format!("dialling {addr}: {e}"))),
        Err(_) => Err(SynoFsError::Io(format!(
            "dialling {addr}: no answer within {patience:?}"
        ))),
    }
}

/// Default depth of the Linux backend's speculative read-ahead window, in
/// 256 KiB blocks. Lives here rather than beside the prefetch logic because
/// [`MountOptions`] is cross-platform and that module is Linux-only.
pub const DEFAULT_PREFETCH_BLOCKS: u64 = 16;

#[cfg(target_os = "linux")]
mod cache;
#[cfg(target_os = "linux")]
mod fs;
pub mod legs;
pub mod logfile;
mod spill;

#[cfg(target_os = "macos")]
mod webdav;

#[cfg(target_os = "windows")]
mod winfs;

/// Tunables that only the Linux/FUSE backend currently honours, but which are
/// accepted on every platform so callers can pass one uniform struct.
#[derive(Debug, Clone, Copy)]
pub struct MountOptions {
    /// Metadata cache TTL in seconds (Linux/FUSE only).
    pub cache_ttl: u64,
    /// Read cache size in MiB (Linux/FUSE only).
    pub read_cache_mb: u64,
    /// FUSE event-loop threads (Linux only). `0` means [`default_io_threads`].
    ///
    /// Every callback runs to completion on the thread that received it. File
    /// transfers are the exception — they run on the Tokio runtime and reply
    /// from there — so this bounds only the callbacks that still block: a read
    /// that misses the cache, a directory listing, a metadata call, and a write
    /// to a handle whose own upload is in flight. Each thread costs a ~16 MiB
    /// kernel receive buffer, which is why this is a small number rather than
    /// "as many as possible".
    pub io_threads: usize,
    /// Owner reported for every entry (Linux only). `None` means the mounting
    /// user. DSM's own uids are deliberately never used: they name accounts on
    /// the appliance, not on this machine.
    pub uid: Option<u32>,
    /// Group reported for every entry (Linux only). `None` means the mounting
    /// user's group.
    pub gid: Option<u32>,
    /// Masked out of the synthetic mode (Linux only), as a process umask would
    /// be: the default 0o022 yields 0o755 directories and 0o644 files.
    pub umask: u16,
    /// Depth of the speculative read-ahead window, in blocks (Linux/FUSE only).
    /// `0` switches speculation off entirely, which is what a bulk consumer
    /// walking a corpus wants: it reads each file once, in order, and every
    /// speculative block is waste that competes with the next file.
    pub prefetch_blocks: u64,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            cache_ttl: 30,
            read_cache_mb: 256,
            io_threads: 0,
            uid: None,
            gid: None,
            umask: DEFAULT_UMASK,
            prefetch_blocks: DEFAULT_PREFETCH_BLOCKS,
        }
    }
}

/// How many FUSE event-loop threads to run when the caller doesn't choose.
///
/// Never 1. Transfers no longer hold a thread, but a cache-missing read or a
/// slow listing still occupies one for a NAS round trip, and with a single
/// thread every other operation on the mount — `readdir` of the mount point
/// included — queues behind it.
pub fn default_io_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(4, 8)
}

/// Returns `true` when a *login* error means the account requires a 2FA / OTP
/// code (DSM reports this as API error 403 or 404 on the auth call). Shared by
/// the CLI's interactive prompt and the FFI binding's `OTP_REQUIRED` result so
/// both agree on what "needs an OTP" means.
pub fn is_otp_required(e: &SynoFsError) -> bool {
    matches!(e, SynoFsError::ApiError(403 | 404))
}

/// A live mount. Dropping the handle — or calling [`MountHandle::stop`], which
/// is just an explicit drop — unmounts the filesystem and joins the background
/// worker (the real work lives in `impl Drop for PlatformMount`, per platform).
/// The handle keeps the Tokio runtime handle and client alive for the lifetime
/// of the mount.
pub struct MountHandle {
    // The client is held so the session's strong reference outlives the caller
    // dropping theirs; the FUSE/WebDAV/WinFsp callbacks all clone from it.
    #[allow(dead_code)]
    client: Arc<SynologyClient>,
    #[allow(dead_code)]
    inner: PlatformMount,
}

impl MountHandle {
    /// Unmount the filesystem and wait for the background worker to finish.
    /// Equivalent to dropping the handle; provided as an explicit, readable
    /// call site. Errors during unmount are logged and swallowed — a
    /// best-effort unmount is the right contract.
    pub fn stop(self) {
        // Cleanup happens in `PlatformMount`'s `Drop` as `self` goes out of scope.
    }
}

// ─── Linux (FUSE) ────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
struct PlatformMount {
    session: Option<fuser::BackgroundSession>,
    /// Kept so a wedged mount can be detached by name. See [`detach`].
    mountpoint: PathBuf,
}

/// How long an ordinary unmount is given before the mount is treated as wedged.
///
/// Generous: this is only reached when `fusermount -u` has already failed,
/// which it does not do on a mount nobody is using.
#[cfg(target_os = "linux")]
const UNMOUNT_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(target_os = "linux")]
impl Drop for PlatformMount {
    /// Unmount, and do not come back wedged.
    ///
    /// `umount_and_join` unmounts and then *joins* the session's threads. Both
    /// halves can stick, and they stick together: a worker inside a callback
    /// the NAS is not answering keeps the kernel counting the mount as busy, so
    /// `fusermount -u` fails — and the session then never sees the end of
    /// `/dev/fuse`, so the join waits for a thread that is waiting for the
    /// unmount that just failed.
    ///
    /// The volume stayed mounted and the caller never returned, which is what
    /// made a disconnect hang until somebody ran `umount` by hand: doing that
    /// is what finally gave the session its EOF.
    ///
    /// So past a deadline the mount is detached lazily, which succeeds whether
    /// or not anyone is using it. **Then this waits for the join anyway.**
    /// Detaching frees the directory; it does not reach into the worker that is
    /// still inside an HTTP call, and that worker goes on using the Tokio
    /// runtime. Returning here is what lets the caller drop it — and a worker
    /// that outlives its runtime does not fail quietly, it panics. So the wait
    /// is what keeps the two in the right order; it is the *mountpoint* that
    /// needed to stop waiting, not this.
    ///
    /// Callers who cannot block are expected to do this off their own thread —
    /// which is what the FFI does, so the window is unblocked long before this
    /// returns.
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let mountpoint = self.mountpoint.clone();

        let (done, finished) = std::sync::mpsc::channel();
        let joining = std::thread::spawn(move || {
            let _ = done.send(session.umount_and_join());
        });

        if finished.recv_timeout(UNMOUNT_PATIENCE).is_err() {
            tracing::warn!(
                "unmounting {} is taking longer than {UNMOUNT_PATIENCE:?} — a request to the \
                 NAS is still outstanding, so the mount is busy. Detaching it; the filesystem \
                 will finish when that request does.",
                mountpoint.display()
            );
            detach(&mountpoint);
        }

        // However long that takes. Nothing is waiting on this that a person can
        // see, and letting go early is what turns a stuck request into a panic.
        if joining.join().is_err() {
            tracing::warn!("the filesystem's teardown panicked");
        }
    }
}

/// Detach a mount that will not come down the ordinary way.
///
/// `-z` is the whole point: a lazy unmount takes the filesystem out of the
/// directory tree immediately, whether or not something is still using it, and
/// the rest is cleaned up when the last user lets go. Without it a wedged mount
/// needs somebody with `sudo`, which is not a thing to ask of a person who
/// clicked Disconnect.
#[cfg(target_os = "linux")]
fn detach(mountpoint: &std::path::Path) {
    // fuse3 first: `fusermount` may not exist on a fuse3-only system, and vice
    // versa on an older one.
    for tool in ["fusermount3", "fusermount"] {
        match std::process::Command::new(tool)
            .arg("-u")
            .arg("-z")
            .arg(mountpoint)
            .status()
        {
            Ok(status) if status.success() => {
                tracing::info!("detached {}", mountpoint.display());
                return;
            }
            Ok(status) => tracing::debug!("{tool} exited {status} for {}", mountpoint.display()),
            Err(e) => tracing::debug!("{tool} is not usable here: {e}"),
        }
    }
    tracing::warn!(
        "could not detach {} — it may need `fusermount3 -uz {}` by hand",
        mountpoint.display(),
        mountpoint.display()
    );
}

/// Returns `true` when `/etc/fuse.conf` contains an uncommented
/// `user_allow_other` line, meaning a non-root user is permitted to pass the
/// `allow_other` option to `fusermount`/`fusermount3`.
#[cfg(target_os = "linux")]
fn fuse_conf_allows_other() -> bool {
    let contents = match std::fs::read_to_string("/etc/fuse.conf") {
        Ok(c) => c,
        Err(_) => return false,
    };
    contents.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.starts_with('#') && trimmed == "user_allow_other"
    })
}

/// Build the fuser session configuration for a mount.
///
/// Split out of [`spawn_mount`] so the threading and mount-option decisions can
/// be asserted without a live `/dev/fuse`.
#[cfg(target_os = "linux")]
fn session_config(allow_other: bool, opts: &MountOptions) -> fuser::Config {
    use fuser::{Config, MountOption, SessionACL};

    let mut mount_options = vec![
        MountOption::RW,
        MountOption::FSName("synology-fuse".to_string()),
    ];
    // AutoUnmount requires AllowOther or AllowRoot per libfuse / fuser docs.
    // Without that, the mount would fail. Skip AutoUnmount when allow_other is
    // unavailable so the mount still succeeds (the user just has to unmount
    // manually with `fusermount -u <mountpoint>`).
    if allow_other {
        mount_options.push(MountOption::AutoUnmount);
    }

    let mut config = Config::default();
    config.mount_options = mount_options;
    // `allow_other` (kernel-level access for other users) moved off MountOption
    // in fuser 0.17 — it's now expressed via Config::acl: SessionACL::All.
    config.acl = if allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    // fuser defaults to a single event-loop thread, and every callback runs to
    // completion on the thread that received it. Transfers are spawned onto the
    // runtime and reply from there, so none of them holds a thread; what still
    // does is a round trip per cache-missing read, listing or metadata call,
    // plus a write waiting on its own handle's upload. With one thread any of
    // those stalls the entire mount, `readdir` of the mount point included —
    // which is what made a copy into `~/mnt` look like "the home directory
    // won't load".
    //
    // `clone_fd` is left off: it needs Linux 4.5+ and its failure would surface
    // only after the session thread had already started, killing the mount. The
    // threads share one /dev/fuse fd instead, exactly as libfuse's own
    // multi-threaded loop does by default.
    config.n_threads = Some(if opts.io_threads == 0 {
        default_io_threads()
    } else {
        opts.io_threads
    });
    config
}

#[cfg(target_os = "linux")]
pub fn spawn_mount(
    client: Arc<SynologyClient>,
    rt: tokio::runtime::Handle,
    mountpoint: PathBuf,
    opts: MountOptions,
) -> Result<MountHandle, SynoFsError> {
    use cache::{DirCache, InodeCache, ReadCache, READ_BLOCK_SIZE};
    use fs::SynologyFS;
    use tracing::info;

    let cache = Arc::new(InodeCache::new(opts.cache_ttl));
    // The same TTL as the metadata cache: one flag, one staleness contract.
    let dir_cache = Arc::new(DirCache::new(opts.cache_ttl));
    let max_blocks = (opts.read_cache_mb * 1024 * 1024) / READ_BLOCK_SIZE;
    let read_cache = Arc::new(ReadCache::new(READ_BLOCK_SIZE, max_blocks.max(1)));
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    // DSM's uids and POSIX bits describe the appliance, not this machine, so
    // the mount presents everything under local identifiers instead.
    let owner = fs::Ownership {
        uid: opts.uid.unwrap_or(uid),
        gid: opts.gid.unwrap_or(gid),
        umask: opts.umask,
    };

    info!(
        "Read cache: {} MiB ({} blocks × {} MiB)",
        opts.read_cache_mb,
        max_blocks,
        READ_BLOCK_SIZE / (1024 * 1024)
    );

    let fs = SynologyFS::new(
        client.clone(),
        cache,
        dir_cache,
        read_cache,
        rt,
        owner,
        opts.prefetch_blocks,
    );

    // As a non-root user, allow_other requires `user_allow_other` in
    // /etc/fuse.conf; running as root always permits it.
    let allow_other = uid == 0 || fuse_conf_allows_other();
    if !allow_other {
        tracing::warn!(
            "allow_other is not enabled: only the mounting user can access the \
             filesystem. To allow other users, add or uncomment \
             `user_allow_other` in /etc/fuse.conf."
        );
    }

    let config = session_config(allow_other, &opts);
    info!("FUSE event loop: {} threads", config.n_threads.unwrap_or(1));

    info!("Mounting shares on {}", mountpoint.display());
    let session = fuser::spawn_mount(fs, &mountpoint, &config).map_err(|e| {
        SynoFsError::Io(format!("failed to mount on {}: {e}", mountpoint.display()))
    })?;
    info!("Mounted at {}", mountpoint.display());

    Ok(MountHandle {
        client,
        inner: PlatformMount {
            session: Some(session),
            mountpoint,
        },
    })
}

// ─── macOS (WebDAV via Finder) ─────────────────────────────────────────────────

#[cfg(target_os = "macos")]
struct PlatformMount {
    rt: tokio::runtime::Handle,
    mountpoint: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl Drop for PlatformMount {
    fn drop(&mut self) {
        use tracing::info;
        info!("Unmounting {}…", self.mountpoint.display());
        let mp = self.mountpoint.clone();
        // Ask macOS to unmount the WebDAV volume, then tear down the server.
        self.rt.block_on(async {
            let _ = tokio::process::Command::new("diskutil")
                .args(["unmount", &mp.to_string_lossy()])
                .status()
                .await;
        });
        // macOS does not always remove the /Volumes/<name> directory after
        // unmounting a WebDAV volume. Remove it ourselves if it is now empty.
        let _ = std::fs::remove_dir(&mp);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = self.rt.block_on(server);
        }
    }
}

/// On macOS we mount via Finder which always places volumes under `/Volumes/`.
/// Reject paths elsewhere so the user gets a predictable volume name.
#[cfg(target_os = "macos")]
fn validate_macos_mountpoint(mountpoint: &std::path::Path) -> Result<(), SynoFsError> {
    if !mountpoint.starts_with("/Volumes/") {
        return Err(SynoFsError::Io(format!(
            "On macOS the mountpoint must be under /Volumes/ (e.g. /Volumes/nas), got: {}",
            mountpoint.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn spawn_mount(
    client: Arc<SynologyClient>,
    rt: tokio::runtime::Handle,
    mountpoint: PathBuf,
    _opts: MountOptions,
) -> Result<MountHandle, SynoFsError> {
    use std::convert::Infallible;

    use dav_server::{fakels::FakeLs, DavHandler};
    use hyper::server::conn::http1;
    use hyper_util::rt::TokioIo;
    use tracing::info;
    use webdav::SynologyDavFs;

    validate_macos_mountpoint(&mountpoint)?;

    // Extract the desired volume name from the last component of the path
    // (e.g. "/Volumes/nas" → "nas") and use it as a URL path prefix so that
    // macOS names the mounted volume correctly.
    let volume_name = mountpoint
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path_prefix = format!("/{}", volume_name);

    let handler = Arc::new(
        DavHandler::builder()
            .filesystem(Box::new(SynologyDavFs::new(
                client.clone(),
                path_prefix.clone(),
            )))
            .locksystem(FakeLs::new())
            .build_handler(),
    );

    // All the async setup that must complete before we return runs on the
    // provided runtime. The accept loop is left running as a spawned task.
    let setup = rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| SynoFsError::Io(format!("failed to bind WebDAV listener: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| SynoFsError::Io(e.to_string()))?
            .port();
        let url = format!("http://127.0.0.1:{}{}/", port, path_prefix);
        info!("WebDAV server listening on {}", url);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let handler_srv = handler.clone();
        let server = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _addr) = match result {
                            Ok(r) => r,
                            Err(e) => { tracing::debug!("accept error: {}", e); break; }
                        };
                        let io = TokioIo::new(stream);
                        let h = handler_srv.clone();
                        tokio::spawn(async move {
                            let svc = hyper::service::service_fn(move |req| {
                                let h = h.clone();
                                async move { Ok::<_, Infallible>(h.handle(req).await) }
                            });
                            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                                tracing::debug!("WebDAV connection closed: {}", e);
                            }
                        });
                    }
                    _ = &mut shutdown_rx => {
                        info!("WebDAV server shutting down");
                        break;
                    }
                }
            }
        });

        Ok::<_, SynoFsError>((url, shutdown_tx, server))
    });

    let (url, shutdown_tx, server) = setup?;

    // Remove a stale mount-point directory left over from a previous run. If
    // the directory is actually still mounted this fails (EBUSY/EPERM), which
    // is fine — we leave it and let osascript fail with a clear message.
    if mountpoint.is_dir() && !mountpoint.is_symlink() {
        let _ = std::fs::remove_dir(&mountpoint);
    }

    // Ask Finder to mount the volume via AppleScript.
    let script = format!("mount volume \"{}\"", url);
    let out = rt
        .block_on(
            tokio::process::Command::new("osascript")
                .args(["-e", &script])
                .output(),
        )
        .map_err(|e| SynoFsError::Io(format!("failed to run osascript: {e}")))?;

    if !out.status.success() {
        let _ = shutdown_tx.send(());
        let _ = rt.block_on(server);
        return Err(SynoFsError::Io(format!(
            "osascript mount failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    info!("Mounted at {}", mountpoint.display());

    Ok(MountHandle {
        client,
        inner: PlatformMount {
            rt,
            mountpoint,
            shutdown: Some(shutdown_tx),
            server: Some(server),
        },
    })
}

// ─── Windows (WinFsp) ───────────────────────────────────────────────────────

/// Add WinFsp's bin directory to PATH so the Windows loader can find
/// winfsp-x64.dll when the delay-load triggers on the first WinFsp call.
/// WinFsp installs to a non-standard location and does not add itself to PATH.
#[cfg(target_os = "windows")]
fn ensure_winfsp_in_path() {
    let candidates = [
        r"C:\Program Files (x86)\WinFsp\bin",
        r"C:\Program Files\WinFsp\bin",
    ];
    for dir in &candidates {
        if std::path::Path::new(dir).join("winfsp-x64.dll").exists() {
            let current = std::env::var_os("PATH").unwrap_or_default();
            let current_str = current.to_string_lossy();
            if !current_str.to_lowercase().contains(&dir.to_lowercase()) {
                std::env::set_var("PATH", format!("{};{}", dir, current_str));
            }
            return;
        }
    }
}

/// WinFsp's `FileSystemHost` is not `Send`, so the mount lives on a dedicated
/// thread that owns it: the thread mounts, starts the dispatcher, parks until
/// signalled, then stops and unmounts. This mirrors the Linux background-thread
/// model and keeps the non-`Send` host confined to one thread.
#[cfg(target_os = "windows")]
struct PlatformMount {
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "windows")]
impl Drop for PlatformMount {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// On Windows WinFsp creates the mountpoint directory itself and deletes it on
/// unmount, so the directory must not exist (but its parent must) when mounting.
#[cfg(target_os = "windows")]
fn prepare_windows_mountpoint(mountpoint: &std::path::Path) -> Result<(), SynoFsError> {
    if let Some(parent) = mountpoint.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(SynoFsError::Io(format!(
                "On Windows the mountpoint's parent directory must exist, got: {}",
                mountpoint.display()
            )));
        }
    }
    if mountpoint.exists() {
        std::fs::remove_dir(mountpoint).map_err(|e| {
            SynoFsError::Io(format!(
                "Mountpoint {} already exists and could not be removed (is it still mounted?): {e}",
                mountpoint.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn spawn_mount(
    client: Arc<SynologyClient>,
    rt: tokio::runtime::Handle,
    mountpoint: PathBuf,
    _opts: MountOptions,
) -> Result<MountHandle, SynoFsError> {
    use std::sync::mpsc;
    use tracing::info;

    prepare_windows_mountpoint(&mountpoint)?;
    ensure_winfsp_in_path();

    // The worker reports the outcome of mount setup back over this channel so
    // spawn_mount can return a structured error if mounting fails.
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let worker_client = client.clone();
    let worker_mp = mountpoint.clone();
    let worker = std::thread::Builder::new()
        .name("synofs-winfsp".to_string())
        .spawn(move || {
            use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};

            let init = match winfsp::winfsp_init() {
                Ok(i) => i,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!(
                        "Failed to initialise WinFsp ({e:?}). Ensure WinFsp is installed from \
                         https://winfsp.dev/rel/ and the WinFsp.Launcher service is running."
                    )));
                    return;
                }
            };
            let _fsp = init;

            let mut volume_params = VolumeParams::new();
            volume_params.filesystem_name("SynoFS");
            volume_params.sector_size(512);
            volume_params.sectors_per_allocation_unit(1);
            volume_params.max_component_length(255);
            volume_params.file_info_timeout(1000);
            volume_params.case_sensitive_search(false);
            volume_params.unicode_on_disk(true);
            volume_params.persistent_acls(false);
            // Unique serial per mount so the Windows MountManager never reuses a
            // stale Volume GUID from a previous (now-dead) WinFsp mount, which
            // would cause STATUS_OBJECT_NAME_COLLISION on the next mount attempt.
            let serial = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0xdeadbeef);
            volume_params.volume_serial_number(serial);

            let fs_params = FileSystemParams::default_params(volume_params);
            let fs_ctx = winfs::SynologyWinFs::new(worker_client, rt.clone());
            // winfsp 0.13 made FileSystemHost generic over an operation-guard
            // strategy and split start() across the FineGuard/CoarseGuard impls,
            // so the guard must be named to disambiguate. FineGuard (the default)
            // dispatches callbacks concurrently — matching 0.12's behaviour — and
            // both SynologyWinFs and its FileContext are Sync, as it requires.
            let mut host: FileSystemHost<winfs::SynologyWinFs, FineGuard> =
                match FileSystemHost::new_with_options(fs_params, fs_ctx) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = ready_tx
                            .send(Err(format!("Failed to create WinFsp filesystem ({e:?})")));
                        return;
                    }
                };
            if let Err(e) = host.mount(&worker_mp) {
                let _ = ready_tx.send(Err(format!(
                    "Failed to mount on {} ({e:?}): ensure the drive letter is not already in use",
                    worker_mp.display()
                )));
                return;
            }
            if let Err(e) = host.start() {
                let _ = ready_tx.send(Err(format!("Failed to start WinFsp dispatcher ({e:?})")));
                return;
            }

            info!("Mounted at {}", worker_mp.display());
            let _ = ready_tx.send(Ok(()));

            // Park until stop() signals (or the sender is dropped), then tear down.
            let _ = shutdown_rx.recv();
            info!("Unmounting {}…", worker_mp.display());
            host.stop();
            host.unmount();
        })
        .map_err(|e| SynoFsError::Io(format!("failed to spawn WinFsp worker: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(MountHandle {
            client,
            inner: PlatformMount {
                shutdown: Some(shutdown_tx),
                worker: Some(worker),
            },
        }),
        Ok(Err(msg)) => {
            let _ = worker.join();
            Err(SynoFsError::Io(msg))
        }
        Err(_) => {
            let _ = worker.join();
            Err(SynoFsError::Io(
                "WinFsp worker exited before mounting".to_string(),
            ))
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Regression: the session ran fuser's default *single* event-loop thread,
    /// and `flush` used to upload on that thread. One multi-gigabyte upload
    /// therefore blocked every other FUSE callback on the mount — `readdir` of
    /// the mount point included — so the whole filesystem looked hung for as
    /// long as the copy took. Transfers have since moved to the runtime, but a
    /// cache-missing read or a listing still holds its thread for a round trip,
    /// so one thread is still one stall away from the same symptom.
    #[test]
    fn the_event_loop_runs_more_than_one_thread() {
        let cfg = session_config(false, &MountOptions::default());
        assert!(
            cfg.n_threads.unwrap_or(1) > 1,
            "a single event-loop thread lets any one slow callback wedge the \
             mount, got {:?}",
            cfg.n_threads
        );
    }

    #[test]
    fn the_thread_count_is_caller_tunable() {
        let opts = MountOptions {
            io_threads: 3,
            ..MountOptions::default()
        };
        assert_eq!(session_config(false, &opts).n_threads, Some(3));
    }

    /// `0` is the "you pick" value the CLI defaults to, so it must land on the
    /// automatic count rather than asking fuser for zero threads (which fails
    /// the mount outright).
    #[test]
    fn zero_threads_means_automatic() {
        let opts = MountOptions {
            io_threads: 0,
            ..MountOptions::default()
        };
        assert_eq!(
            session_config(false, &opts).n_threads,
            Some(default_io_threads())
        );
        assert!(default_io_threads() > 1);
    }

    /// AutoUnmount requires allow_other; asking for it without is a mount
    /// failure, so the existing pairing has to survive the config extraction.
    #[test]
    fn auto_unmount_is_only_requested_alongside_allow_other() {
        use fuser::{MountOption, SessionACL};

        let opts = MountOptions::default();
        let with = session_config(true, &opts);
        assert_eq!(with.acl, SessionACL::All);
        assert!(with.mount_options.contains(&MountOption::AutoUnmount));

        let without = session_config(false, &opts);
        assert_eq!(without.acl, SessionACL::Owner);
        assert!(!without.mount_options.contains(&MountOption::AutoUnmount));
    }
}

#[cfg(test)]
mod redial_tests {
    use super::*;
    use async_trait::async_trait;
    use synology_filestation_connect::{
        Endpoints, NoTunnel, Prober, TransportPolicy, DEFAULT_RECHECK,
    };

    use crate::log_capture::LogCapture;

    /// A NAS that answers, without dialling anything to find out — so the only
    /// connection the listener below sees is the one the redial makes.
    struct Answers;

    #[async_trait]
    impl Prober for Answers {
        async fn smb_reachable(&self, _host: &str) -> bool {
            true
        }
    }

    /// Regression: `SYNOLOGY_FS_SMB_PORT` moves SMB off 445, and the redial
    /// dialled 445 regardless — so a deployment on another port could reopen
    /// nothing directly. The host here carries no port, so only the configured
    /// one can reach the listener.
    #[tokio::test]
    async fn a_direct_redial_dials_the_configured_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port to answer on");
        let port = listener.local_addr().expect("its address").port();
        let accepted = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

        let chain = Arc::new(Chain::new(
            TransportPolicy::default(),
            Endpoints::public_only("127.0.0.1"),
            Box::new(Answers),
            Box::new(NoTunnel),
            DEFAULT_RECHECK,
        ));
        let dial = Dial {
            port,
            patience: Duration::from_secs(5),
        };

        let reopened = reopen_through(chain, dial)().await;

        assert!(
            reopened.is_ok(),
            "{:?}",
            reopened.err().map(|e| e.to_string())
        );
        assert!(accepted.await.expect("the accept task ran").is_ok());
    }

    /// Regression: a mount whose tunnel is gone, on a machine that can now
    /// reach the NAS directly, refused to reopen at all. The redial reported
    /// that the leg had changed and gave up, so SMB stayed dead for the life
    /// of the mount — with the NAS one TCP connection away, and every
    /// operation still paying a failed SMB attempt before falling back to
    /// HTTP. A stream is a stream: where the bytes come from is exactly what
    /// this layer exists to stop mattering.
    #[tokio::test]
    async fn a_tunnel_that_became_a_direct_route_still_reopens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port to answer on");
        let host = listener.local_addr().expect("its address").to_string();
        let accepted = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

        let chain = Arc::new(Chain::new(
            TransportPolicy::default(),
            Endpoints::public_only(host),
            Box::new(Answers),
            Box::new(NoTunnel),
            DEFAULT_RECHECK,
        ));

        let reopened = reopen_through(
            chain,
            Dial {
                port: 445,
                patience: Duration::from_secs(5),
            },
        )()
        .await;

        assert!(
            reopened.is_ok(),
            "the NAS answers directly, and that is a stream: {:?}",
            reopened.err().map(|e| e.to_string())
        );
        assert!(
            accepted.await.expect("the accept task ran").is_ok(),
            "and the redial is what connected"
        );
    }

    /// Regression: a mount whose SMB stream kept dying said only
    /// "Disconnected from server", once per operation, for an hour. Nothing
    /// said how long the stream had lasted before it went, or which leg
    /// brought it back — and those are the two facts that separate a periodic
    /// server-side timeout (a round number, every time) from a tunnel that
    /// keeps falling over (a ragged one). Which is the difference between
    /// knowing what to fix and watching it happen again.
    #[tokio::test]
    async fn reopening_a_stream_says_how_long_the_last_one_lasted() {
        let logs = LogCapture::at_the_default_level();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port to answer on");
        let host = listener.local_addr().expect("its address").to_string();
        let accepting = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

        let chain = Arc::new(Chain::new(
            TransportPolicy::default(),
            Endpoints::public_only(host),
            Box::new(Answers),
            Box::new(NoTunnel),
            DEFAULT_RECHECK,
        ));
        reopen_through(
            chain,
            Dial {
                port: 445,
                patience: Duration::from_secs(5),
            },
        )()
        .await
        .expect("a stream");
        let _ = accepting.await;

        let text = logs.text();
        assert!(
            text.contains("lasted"),
            "how long the dead stream lived is the whole diagnostic: {text}"
        );
        assert!(
            text.contains("directly"),
            "and so is which leg brought it back: {text}"
        );
    }

    /// Regression: a redial holds the transport's lock, and a direct decision
    /// never expires, so a laptop carried off campus redialled the campus
    /// address with a bare `TcpStream::connect` — about two minutes on Linux
    /// against a port that drops packets, during which every operation on the
    /// mount queued behind the lock instead of falling back to HTTP.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_direct_host_that_stopped_answering_is_given_up_on_in_time() {
        // A listener whose accept queue is full drops new SYNs, which is what
        // a firewall that silently drops 445 looks like from here.
        let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
        socket.bind("127.0.0.1:0".parse().unwrap()).expect("bound");
        let listener = socket.listen(1).expect("listening");
        let host = listener.local_addr().expect("its address").to_string();
        let mut queued = Vec::new();
        for _ in 0..4 {
            if let Ok(Ok(stream)) = tokio::time::timeout(
                Duration::from_millis(200),
                tokio::net::TcpStream::connect(&host),
            )
            .await
            {
                queued.push(stream);
            }
        }

        let chain = Arc::new(Chain::new(
            TransportPolicy::default(),
            Endpoints::public_only(host),
            Box::new(Answers),
            Box::new(NoTunnel),
            DEFAULT_RECHECK,
        ));

        let reopened = tokio::time::timeout(
            Duration::from_secs(5),
            reopen_through(
                chain,
                Dial {
                    port: 445,
                    patience: Duration::from_millis(500),
                },
            )(),
        )
        .await
        .expect("gave up within its patience, rather than the kernel's");
        assert!(reopened.is_err());
        drop((listener, queued));
    }
}

/// Capturing what the process logs, so the lines a mount depends on can be
/// asserted on.
///
/// Ported from the connect crate's own capture, and for its reason: which leg
/// reaches the NAS, and why a stream had to be reopened, reach a user as log
/// output and nothing else. The level an event is emitted at is behaviour.
///
/// A thread-local subscriber looks like the right tool and is not: `tracing`
/// caches each callsite's *interest* globally, so a callsite first reached by
/// a thread holding no subscriber is cached as uninteresting and the test that
/// wants it then sees nothing. One subscriber, installed once and never
/// removed, keeps every callsite permanently interesting; lines from tests
/// running alongside land in the buffer too, which costs nothing, because the
/// assertions ask what the log *contains*.
#[cfg(test)]
mod log_capture {
    use std::sync::{Arc, Mutex, MutexGuard};

    #[derive(Clone, Default)]
    pub(crate) struct Sink(Arc<Mutex<Vec<u8>>>);

    /// Only one capturing test at a time, and where its output goes.
    static CAPTURING: Mutex<()> = Mutex::new(());
    static ACTIVE: Mutex<Option<Sink>> = Mutex::new(None);

    #[derive(Clone, Default)]
    struct RouteToActiveCapture;

    impl std::io::Write for RouteToActiveCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(sink) = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                sink.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RouteToActiveCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            Self
        }
    }

    pub(crate) struct LogCapture {
        sink: Sink,
        /// Held for the life of the capture, so two of these never overlap.
        _capturing: MutexGuard<'static, ()>,
    }

    impl LogCapture {
        /// Capture at `info` — what somebody gets without changing a setting,
        /// which is the whole point of asserting on it.
        pub(crate) fn at_the_default_level() -> Self {
            // A panicking capture test must not stop the next one running.
            let capturing = CAPTURING.lock().unwrap_or_else(|e| e.into_inner());
            static INSTALLED: std::sync::Once = std::sync::Once::new();
            INSTALLED.call_once(|| {
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(RouteToActiveCapture)
                    .with_max_level(tracing::Level::INFO)
                    .with_ansi(false)
                    .finish();
                // Another test in this binary may have got there first; either
                // way a subscriber is in place afterwards.
                let _ = tracing::subscriber::set_global_default(subscriber);
            });
            let sink = Sink::default();
            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink.clone());
            Self {
                sink,
                _capturing: capturing,
            }
        }

        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.sink.0.lock().unwrap_or_else(|e| e.into_inner()))
                .into_owned()
        }
    }

    impl Drop for LogCapture {
        fn drop(&mut self) {
            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}
