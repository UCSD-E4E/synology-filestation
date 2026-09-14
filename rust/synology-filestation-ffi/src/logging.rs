//! Where the crate's `tracing` events go: a C callback, a file, or both.
//!
//! The GUI installs a callback via `syno_set_log_callback`; we lazily install a
//! `tracing_subscriber::fmt` subscriber whose writer formats each event line
//! and hands it to whatever callback is currently registered. Installing the
//! subscriber is one-shot (a global default can only be set once), but the
//! callback target behind it can be swapped or cleared at any time.
//!
//! The callback alone is not enough to diagnose the case it most matters for.
//! It renders into the GUI's log pane, and that pane dies with the GUI —
//! including when a wedged mount takes the machine with it, which is the run
//! worth reading afterwards. So `syno_set_log_file` adds a second destination
//! that outlives the process, and the writer fans each record out to both. See
//! [`synology_filestation_fuse::logfile`] for what that file guarantees.

use std::ffi::{c_void, CString};
use std::io::{self, Write};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

use tracing_subscriber::{fmt, prelude::*, reload, EnvFilter, Registry};

use synology_filestation_fuse::logfile::{self, LogFile, LogFileError};

/// C callback signature: `(level, line, user_data)`. `level` mirrors the
/// `tracing::Level` ordering (1=ERROR … 5=TRACE); `line` is a NUL-terminated
/// UTF-8 string valid only for the duration of the call.
pub type LogCb = extern "C" fn(level: i32, line: *const c_char, user_data: *mut c_void);

/// A registered callback plus its opaque user-data pointer. The pointers are
/// only ever called/observed; we never deref `user_data` ourselves. The C#
/// side guarantees both stay valid until it clears the callback.
struct Sink {
    cb: LogCb,
    user_data: usize,
}

// SAFETY: the callback is a plain function pointer and `user_data` is treated
// as an opaque token; the foreign caller owns its lifetime and thread-safety.
unsafe impl Send for Sink {}

static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
static INIT: Once = Once::new();
/// Handle to the reloadable level filter, so `set_level` can change verbosity
/// after the subscriber is installed.
static RELOAD: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

fn sink() -> &'static Mutex<Option<Sink>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// The crates whose `debug!` a person asking for debug actually wants.
///
/// Everything else in the process — TLS, HTTP, the userspace TCP stack — is
/// somebody else's diagnostic, and at debug it arrives faster than a log pane
/// can render.
const OURS: [&str; 6] = [
    "synology_filestation_core",
    "synology_filestation_fuse",
    "synology_filestation_ffi",
    "synology_filestation_connect",
    "synology_filestation_openvpn",
    "synology_filestation_smb",
];

/// The filter expression a requested level means.
///
/// A bare level is global, which is the whole bug: "debug" used to mean debug
/// for every crate linked into the process. The verbose levels are therefore
/// scoped to this workspace's crates, and everything else is left at `info` —
/// left, not lowered, so a dependency's warning still arrives.
///
/// Anything that already looks like a filter expression is passed through
/// untouched, matching what the CLI's `--log-level` accepts. Scoping the
/// levels would otherwise remove the only way to ask a dependency a question.
fn directives(level: &str) -> String {
    let asked = level.trim();
    if asked.contains('=') || asked.contains(',') {
        return asked.to_string();
    }
    let verbose = match asked.to_ascii_lowercase().as_str() {
        "error" => return "error".to_string(),
        "warn" => return "warn".to_string(),
        "debug" => "debug",
        "trace" => "trace",
        // `info` and anything unrecognised. Unrecognised is deliberately the
        // same as the default rather than an error: this arrives from a
        // settings file, and a typo there should not silence the log.
        _ => return "info".to_string(),
    };
    let mut out = String::from("info");
    for target in OURS {
        out.push(',');
        out.push_str(target);
        out.push('=');
        out.push_str(verbose);
    }
    out
}

/// Change the active log verbosity. No-op until the subscriber is installed
/// (the GUI registers a callback at startup, before any connect).
pub fn set_level(level: &str) {
    if let Some(handle) = RELOAD.get() {
        if let Ok(filter) = EnvFilter::try_new(directives(level)) {
            let _ = handle.reload(filter);
        }
    }
}

/// Register (or, with `cb == None`, clear) the log callback, installing the
/// tracing subscriber on first use.
///
/// # Safety
/// `cb`/`user_data` must remain valid until a subsequent call clears them.
pub unsafe fn set_callback(cb: Option<LogCb>, user_data: *mut c_void) {
    {
        let mut guard = sink().lock().unwrap();
        *guard = cb.map(|cb| Sink {
            cb,
            user_data: user_data as usize,
        });
    }
    install();
}

/// Install the subscriber, once. Called from both entry points — registering a
/// callback and setting a log file — because either one alone is a complete
/// reason to start collecting records.
fn install() {
    INIT.call_once(|| {
        // INFO by default, reloadable via `set_level` so the GUI's log-level
        // control actually takes effect. Build the reload handle first so it is
        // available even if `try_init` later loses the global-default race.
        let (filter, handle) = reload::Layer::new(EnvFilter::new("info"));
        let _ = RELOAD.set(handle);
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_ansi(false).with_writer(Fanout))
            .try_init();
    });
}

/// Where records are also written, when the GUI has asked for a file.
static LOG_FILE: OnceLock<Mutex<Option<LogFile>>> = OnceLock::new();

fn log_file() -> &'static Mutex<Option<LogFile>> {
    LOG_FILE.get_or_init(|| Mutex::new(None))
}

/// A log file must never be what stops the process working, so every lock here
/// steps over a poisoning rather than propagating it.
fn file_guard() -> std::sync::MutexGuard<'static, Option<LogFile>> {
    log_file()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start also writing every record to `path`, and return where that turned out
/// to be. Installs the subscriber if nothing has yet: the GUI may ask for a
/// log file before it registers a callback, and a log nobody asked twice for
/// should still be written.
pub fn set_file(path: &Path) -> Result<PathBuf, LogFileError> {
    let resolved = logfile::resolve(path, None)?;
    let file = LogFile::open(resolved.clone())?;
    *file_guard() = Some(file);
    install();
    Ok(resolved)
}

/// Stop writing to the file, closing it.
pub fn clear_file() {
    *file_guard() = None;
}

/// Where records are being written, if anywhere.
pub fn file_path() -> Option<PathBuf> {
    file_guard().as_ref().map(|f| f.path().to_path_buf())
}

/// The active log path, if a mount at `mountpoint` would end up serving it.
///
/// A log written through the mount it describes deadlocks that mount: the
/// record emitted inside a filesystem callback becomes a write the same
/// callback has to serve. The CLI refuses this when it parses `--log-file`,
/// where it knows the mountpoint; the GUI sets the two independently, so the
/// check has to be made again when the pair is finally known.
pub fn conflicts_with_mount(mountpoint: &Path) -> Option<PathBuf> {
    let active = file_path()?;
    logfile::is_inside(&active, mountpoint).then_some(active)
}

/// How many `fsync`s the active file has performed. Test-only: fsync is not
/// observable from outside the process, so the durability policy is asserted
/// by watching the call happen.
#[cfg(test)]
fn file_syncs() -> Option<u64> {
    file_guard().as_ref().map(|f| f.sync_count())
}

/// The writer the `fmt` layer gets: one formatted record, sent both to the
/// registered C callback and to the log file.
struct Fanout;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Fanout {
    type Writer = FanoutWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FanoutWriter { durable: true }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        // The level has to be taken from the metadata here rather than scanned
        // back out of the formatted text: it decides whether the record is
        // forced to disk, and `level_of` below is a best-effort read of a
        // prefix, which is fine for tinting a log pane and not for this.
        FanoutWriter {
            durable: logfile::durable(meta.level()),
        }
    }
}

/// A `Write` sink that buffers a line and dispatches complete lines to the
/// registered callback. Lines are level-tagged best-effort by scanning the
/// formatted prefix.
struct FanoutWriter {
    durable: bool,
}

impl Write for FanoutWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The file first. `dispatch` crosses into foreign code, and a GUI that
        // hangs or dies in its callback must not take the on-disk record —
        // the one written precisely for the case where the GUI is gone — with
        // it.
        to_file(buf, self.durable);

        let text = String::from_utf8_lossy(buf);
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue;
            }
            dispatch(trimmed);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn to_file(buf: &[u8], durable: bool) {
    let mut guard = file_guard();
    let Some(file) = guard.as_mut() else {
        return;
    };
    if let Err(e) = file.writer(durable).write_all(buf) {
        // Never `warn!` from inside the writer — that is a straight recursion
        // back into here. Say it once, on stderr, and carry on: a log file
        // that has stopped working is not a reason to stop mounting.
        static COMPLAINED: Once = Once::new();
        COMPLAINED.call_once(|| {
            let _ = writeln!(io::stderr(), "log file is no longer writable: {e}");
        });
    }
}

fn level_of(line: &str) -> i32 {
    // The fmt layer prints "<timestamp> <LEVEL> <target>: <message>". The level
    // is the first whitespace-delimited token that exactly equals a level name;
    // returning on the first exact match means message text containing the word
    // "ERROR"/"DEBUG" (which comes after the level) can't misclassify the line.
    for tok in line.split_whitespace() {
        match tok {
            "ERROR" => return 1,
            "WARN" => return 2,
            "INFO" => return 3,
            "DEBUG" => return 4,
            "TRACE" => return 5,
            _ => {}
        }
    }
    3 // default to INFO if no level token is found
}

fn dispatch(line: &str) {
    // Copy the callback + user_data out while holding the lock, then release it
    // *before* calling into foreign code. Calling under the lock risks a
    // deadlock if the callback re-enters logging or tries to swap/clear itself.
    let target = {
        let guard = match sink().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        guard.as_ref().map(|s| (s.cb, s.user_data))
    };
    if let Some((cb, user_data)) = target {
        if let Ok(c) = CString::new(line) {
            cb(level_of(line), c.as_ptr(), user_data as *mut c_void);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the level was applied as a bare `LevelFilter`, which is
    /// global. Asking for "debug" therefore turned on debug for *every* crate
    /// in the process — rustls, hyper, reqwest, and, once a tunnel is up, the
    /// userspace TCP stack packet by packet. Our own crates emit almost no
    /// `debug!` at all, so nearly all of that volume was somebody else's, and
    /// it arrived through a C callback into a GUI that appends to a string per
    /// line. The log pane stopped being readable and the window stopped
    /// responding, which is a poor reward for asking a question.
    #[test]
    fn debug_does_not_turn_on_the_whole_dependency_tree() {
        let filter = directives("debug");

        assert!(
            filter.starts_with("info"),
            "everything else stays where it was, got {filter}"
        );
        assert!(
            filter.contains("synology_filestation_connect=debug"),
            "and ours go verbose, got {filter}"
        );
        assert!(
            filter.contains("synology_filestation_openvpn=debug"),
            "the tunnel is the thing being diagnosed, got {filter}"
        );
    }

    #[test]
    fn trace_scopes_the_same_way() {
        let filter = directives("trace");

        assert!(filter.starts_with("info"), "got {filter}");
        assert!(
            filter.contains("synology_filestation_openvpn=trace"),
            "got {filter}"
        );
    }

    /// The quiet levels were never the problem, and rewriting them would hide
    /// a dependency's warning — which is the one third-party message anybody
    /// wants.
    #[test]
    fn a_quiet_level_stays_global() {
        assert_eq!(directives("info"), "info");
        assert_eq!(directives("warn"), "warn");
        assert_eq!(directives("error"), "error");
    }

    #[test]
    fn an_unknown_level_is_info_rather_than_silence() {
        assert_eq!(directives("shout"), "info");
        assert_eq!(directives(""), "info");
    }

    /// An escape hatch, matching what the CLI already accepts: anything that
    /// looks like a filter expression is one, and is passed through untouched.
    /// Without it, scoping the levels would take away the only way to ask a
    /// dependency a question.
    #[test]
    fn an_explicit_filter_expression_is_left_alone() {
        let asked = "warn,synology_filestation_openvpn=trace,rustls=debug";

        assert_eq!(directives(asked), asked);
        assert_eq!(directives("hyper=debug"), "hyper=debug");
    }

    /// A directive string that does not parse would leave the subscriber on
    /// whatever it had, silently ignoring the setting.
    #[test]
    fn every_level_produces_something_that_parses() {
        for level in ["error", "warn", "info", "debug", "trace", "nonsense"] {
            assert!(
                tracing_subscriber::EnvFilter::try_new(directives(level)).is_ok(),
                "{level} produced an unparseable filter"
            );
        }
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// The log file is process-global, so these tests take turns with it.
    pub(super) static ONE_AT_A_TIME: StdMutex<()> = StdMutex::new(());

    /// Emit through a *local* subscriber: installing the global one is
    /// one-shot, and these tests must not spend it.
    fn emitting<T>(body: impl FnOnce() -> T) -> T {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(Fanout)
            .finish();
        tracing::subscriber::with_default(subscriber, body)
    }

    /// The GUI's log pane dies with the GUI, exactly as a terminal dies with
    /// its window. A mount started from the GUI has to leave the same record
    /// behind that a mount started from the CLI does.
    #[test]
    fn records_reach_the_log_file_once_one_is_set() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui.log");

        set_file(&path).expect("the file is writable");
        emitting(|| tracing::info!("mounting the share"));
        clear_file();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("mounting the share"), "held {written:?}");
    }

    #[test]
    fn clearing_the_log_file_stops_writing_to_it() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui.log");

        set_file(&path).unwrap();
        emitting(|| tracing::info!("while it was on"));
        let after_on = std::fs::read_to_string(&path).unwrap();

        clear_file();
        emitting(|| tracing::info!("after it was turned off"));

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            after_on,
            "nothing may be appended once the file is cleared"
        );
    }

    /// The metadata has to survive the trip through the fanout, or every
    /// record would be forced to disk (ruinous at `trace`) or none would be
    /// (which loses the warning that explains the power cycle).
    #[test]
    fn the_level_still_decides_durability_through_the_fanout() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui.log");

        set_file(&path).unwrap();
        let before = file_syncs().expect("a file is set");
        emitting(|| tracing::info!("routine"));
        let after_info = file_syncs().unwrap();
        emitting(|| tracing::warn!("the SMB stream died"));
        let after_warn = file_syncs().unwrap();
        clear_file();

        assert_eq!(after_info, before, "an info record is not worth a barrier");
        assert_eq!(after_warn, before + 1, "a warning is");
    }

    /// A path that cannot be opened is reported, not swallowed: the GUI has to
    /// be able to say the log is not being written.
    #[test]
    fn an_unusable_path_is_reported() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // A directory is not a file, and never becomes one.
        assert!(set_file(dir.path()).is_err());
        clear_file();
    }
}

#[cfg(test)]
mod mount_conflict_tests {
    use super::*;

    /// The GUI picks its log path from a file dialog, and nothing stops that
    /// dialog from landing inside the very directory about to be mounted. The
    /// mount is refused rather than started, because the failure mode is not
    /// an error — it is a mount that wedges the first time it logs, which is
    /// the state this whole feature exists to explain.
    #[test]
    fn a_log_file_inside_the_requested_mountpoint_is_refused() {
        let _turn = super::file_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("nas");
        std::fs::create_dir(&mountpoint).unwrap();

        set_file(&mountpoint.join("mount.log")).unwrap();
        let conflict = conflicts_with_mount(&mountpoint);
        clear_file();

        assert!(
            conflict.is_some(),
            "a log under the mountpoint has to be caught"
        );
    }

    #[test]
    fn a_log_file_elsewhere_is_no_obstacle() {
        let _turn = super::file_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("nas");
        std::fs::create_dir(&mountpoint).unwrap();

        set_file(&dir.path().join("mount.log")).unwrap();
        let conflict = conflicts_with_mount(&mountpoint);
        clear_file();

        assert_eq!(conflict, None);
    }

    /// No log file at all cannot conflict with anything.
    #[test]
    fn no_log_file_never_blocks_a_mount() {
        let _turn = super::file_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        clear_file();

        assert_eq!(conflicts_with_mount(Path::new("/mnt/nas")), None);
    }
}
