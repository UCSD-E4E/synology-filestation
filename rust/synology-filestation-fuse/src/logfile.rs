//! A log file that is still there after the machine goes down.
//!
//! Everything this process says goes to stderr, and stderr lives in a terminal.
//! When a mount wedges the machine badly enough to need the power button, that
//! terminal is gone — and with it the only record of what the mount was doing
//! in the seconds that mattered. The lock-up is exactly the event nobody can
//! reproduce on demand, so it is exactly the one that has to be written down
//! while it happens.
//!
//! So `--log-file` adds a second sink beside stderr, with two properties the
//! terminal did not have:
//!
//! * **No buffering between the event and the OS.** Every record is one
//!   `write_all` followed by a flush, so a process that is killed — OOM, a
//!   panic, a `SIGKILL` from an impatient user — still leaves every line it
//!   ever emitted. A background writer thread (what `tracing-appender` gives
//!   you) loses precisely the tail that explains the death.
//! * **`fsync` on warn and error.** Page cache is not disk: a power cycle can
//!   take back the last seconds of writes. Forcing every record to disk is too
//!   expensive to do at `trace` on a busy mount — and slow enough to change the
//!   timing being observed — so the durability is spent where the evidence is.
//!
//! The file rotates at [`DEFAULT_MAX_BYTES`] and keeps [`DEFAULT_KEEP`] older
//! generations, so `--log-level trace` cannot fill the disk of a machine that
//! is already in trouble.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;

/// Rotate once the active file passes this size.
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How many rotated generations to keep beside the active file.
pub const DEFAULT_KEEP: usize = 3;

/// Why a `--log-file` could not be used.
#[derive(Debug)]
pub enum LogFileError {
    /// The log path is inside the directory this process is about to mount.
    /// Writing there would route every log record back through our own FUSE
    /// callbacks — and the first record written while a callback is logging
    /// deadlocks the mount against itself.
    InsideMount { path: PathBuf, mountpoint: PathBuf },
    /// The path could not be opened, or its parent could not be created.
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for LogFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFileError::InsideMount { path, mountpoint } => write!(
                f,
                "--log-file {} is inside the mountpoint {}. The log would be \
                 written through the mount it is describing, which deadlocks \
                 it. Choose a path on local disk.",
                path.display(),
                mountpoint.display()
            ),
            LogFileError::Io { path, source } => {
                write!(f, "cannot open log file {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LogFileError {}

/// Whether a record at this level is worth an `fsync`.
///
/// `tracing` orders levels by verbosity, so ERROR and WARN are the two that
/// compare at or below WARN.
pub fn durable(level: &Level) -> bool {
    *level <= Level::WARN
}

/// Whether `path` would be served by a mount at `mountpoint`.
///
/// Both are made absolute first, and the comparison is by path component — so
/// a mountpoint `/x/mnt` does not swallow a sibling log at `/x/mnt.log`.
/// Neither path need exist: the question is asked before anything is mounted.
pub fn is_inside(path: &Path, mountpoint: &Path) -> bool {
    match (std::path::absolute(path), std::path::absolute(mountpoint)) {
        (Ok(path), Ok(mountpoint)) => path.starts_with(mountpoint),
        // A path this process cannot even make absolute is not one we can
        // clear, so treat it as unsafe rather than waving it through.
        _ => true,
    }
}

/// Turn a requested `--log-file` into a path that can actually be written for
/// the life of the mount.
///
/// Absolute, because a mount outlives the shell that started it and a relative
/// path would follow a working directory that may not survive; parent created,
/// because asking a user to `mkdir` first is a way of losing the log; and never
/// inside `mountpoint`, for the reason on [`LogFileError::InsideMount`].
pub fn resolve(requested: &Path, mountpoint: Option<&Path>) -> Result<PathBuf, LogFileError> {
    let path = std::path::absolute(requested).map_err(|source| LogFileError::Io {
        path: requested.to_path_buf(),
        source,
    })?;

    if let Some(mountpoint) = mountpoint {
        // `absolute`, not `canonicalize`: this question is asked before
        // anything is mounted, and often before the mountpoint directory
        // itself exists. `starts_with` compares components, so a mountpoint
        // `/x/mnt` does not swallow a sibling log at `/x/mnt.log`.
        let mountpoint = std::path::absolute(mountpoint).map_err(|source| LogFileError::Io {
            path: mountpoint.to_path_buf(),
            source,
        })?;
        if is_inside(&path, &mountpoint) {
            return Err(LogFileError::InsideMount { path, mountpoint });
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LogFileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    Ok(path)
}

/// A rotating, immediately-flushed log file usable as a `tracing` writer.
pub struct LogFile {
    inner: Arc<Mutex<Rotating>>,
    path: PathBuf,
}

/// The path is the whole of what a caller can usefully be told; the open file
/// and its byte counter are ours.
impl std::fmt::Debug for LogFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogFile").field("path", &self.path).finish()
    }
}

/// Cloning shares the one open file. The `fmt` layer takes its writer by value,
/// and the caller still wants to name the path afterwards — and, in the CLI, to
/// hold the file open for the life of the mount.
impl Clone for LogFile {
    fn clone(&self) -> Self {
        LogFile {
            inner: Arc::clone(&self.inner),
            path: self.path.clone(),
        }
    }
}

impl LogFile {
    /// Open (creating, else appending to) the log at `path`.
    pub fn open(path: PathBuf) -> Result<Self, LogFileError> {
        Self::with_rotation(path, DEFAULT_MAX_BYTES, DEFAULT_KEEP)
    }

    /// As [`LogFile::open`], with the rotation policy spelled out. Separate so
    /// the rotation tests do not have to write eight megabytes to see a roll.
    pub fn with_rotation(path: PathBuf, max_bytes: u64, keep: usize) -> Result<Self, LogFileError> {
        let file = open_append(&path)?;
        // Append, so an existing file's size is already part of the budget: a
        // mount restarted in a loop must not be able to defeat the cap by
        // starting each run's accounting from zero.
        let written = file
            .metadata()
            .map_err(|source| LogFileError::Io {
                path: path.clone(),
                source,
            })?
            .len();

        Ok(LogFile {
            inner: Arc::new(Mutex::new(Rotating {
                path: path.clone(),
                file,
                written,
                max_bytes,
                keep,
                syncs: 0,
            })),
            path,
        })
    }

    /// Where records are being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A writer for one record whose durability the caller has already decided.
    ///
    /// The `MakeWriter` impl below takes that decision from the event's level.
    /// A consumer that has classified the record itself — the FFI fans one
    /// record out to both a C callback and this file — says so directly.
    pub fn writer(&self, durable: bool) -> LogFileWriter {
        LogFileWriter {
            inner: Arc::clone(&self.inner),
            sync: durable,
        }
    }

    /// How many `fsync`s this file has performed.
    ///
    /// There is no portable way to observe a sync from outside the process, so
    /// this is how the durability policy is asserted — including from the FFI
    /// crate, which is why it is not `#[cfg(test)]`.
    pub fn sync_count(&self) -> u64 {
        lock(&self.inner).syncs
    }
}

/// The shared, locked state behind every writer handed to the `fmt` layer.
struct Rotating {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    keep: usize,
    syncs: u64,
}

/// One record's worth of access to the file, carrying the durability decision
/// made from the event's metadata.
pub struct LogFileWriter {
    inner: Arc<Mutex<Rotating>>,
    sync: bool,
}

impl Write for LogFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = lock(&self.inner);
        file.record(buf, self.sync)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Every record has already been flushed by the time `write` returns;
        // this exists so the `fmt` layer's own flush is not an error.
        lock(&self.inner).file.flush()
    }
}

/// A log file is the last thing that should stop working because something
/// else panicked — a poisoned lock here would silence exactly the diagnostic
/// the panic is about, so the poison is stepped over.
fn lock(inner: &Arc<Mutex<Rotating>>) -> std::sync::MutexGuard<'_, Rotating> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_append(path: &Path) -> Result<File, LogFileError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| LogFileError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// `mount.log` rotated `n` generations back: `mount.log.1`, `mount.log.2`, …
fn generation(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

impl Rotating {
    /// Write one whole record, then put it as far down the stack as its level
    /// has earned.
    fn record(&mut self, buf: &[u8], sync: bool) -> io::Result<()> {
        // A record is never split across a rotation — half a line explains
        // nothing — so an empty file takes an over-sized record as it is.
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }

        self.file.write_all(buf)?;
        self.written += buf.len() as u64;
        // Reach the OS now. A process killed without unwinding — OOM, SIGKILL,
        // a panic in another thread — never runs a deferred flush, and the
        // records it never got to flush are the ones that explain the death.
        self.file.flush()?;

        if sync {
            self.file.sync_data()?;
            self.syncs += 1;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        // What is being rotated away is evidence: make it durable before the
        // rename rather than leaving it to writeback that a power cut may
        // never get to.
        self.file.sync_data()?;

        if self.keep == 0 {
            // Nowhere to roll to. Discard rather than grow — and without
            // inventing a `mount.log.0` nobody would think to look for.
            self.file = File::create(&self.path)?;
            self.written = 0;
            return Ok(());
        }

        let _ = fs::remove_file(generation(&self.path, self.keep));
        for n in (1..self.keep).rev() {
            let from = generation(&self.path, n);
            if from.exists() {
                fs::rename(&from, generation(&self.path, n + 1))?;
            }
        }
        fs::rename(&self.path, generation(&self.path, 1))?;

        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = LogFileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        // No event to classify — anything reaching the writer this way is
        // treated as worth keeping.
        self.writer(true)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        self.writer(durable(meta.level()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writing the log through the mount it describes deadlocks the mount: the
    /// record emitted inside a FUSE callback becomes a write that same callback
    /// has to serve. The path is refused up front rather than at the first
    /// interesting event.
    #[test]
    fn a_log_path_inside_the_mountpoint_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mnt");
        std::fs::create_dir(&mountpoint).unwrap();

        let err = resolve(&mountpoint.join("synofs.log"), Some(&mountpoint))
            .expect_err("a log inside the mount must not be accepted");

        assert!(
            matches!(err, LogFileError::InsideMount { .. }),
            "expected InsideMount, got {err:?}"
        );
    }

    /// `/x/mnt.log` is not inside `/x/mnt`, however much the strings look
    /// alike. A prefix test done on text rather than on path components would
    /// reject a perfectly good path.
    #[test]
    fn a_path_that_merely_shares_a_prefix_with_the_mountpoint_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mnt");
        std::fs::create_dir(&mountpoint).unwrap();

        let chosen = resolve(&dir.path().join("mnt.log"), Some(&mountpoint))
            .expect("a sibling of the mountpoint is not inside it");

        assert_eq!(chosen, dir.path().join("mnt.log"));
    }

    /// A mount outlives the shell that started it, so a path kept relative
    /// would be resolved against a working directory that may be gone — the
    /// same way `$TMPDIR` outlives a `nix develop` shell in `spill`.
    #[test]
    fn a_relative_path_is_made_absolute() {
        let chosen = resolve(Path::new("synofs.log"), None).expect("a relative path is usable");

        assert!(chosen.is_absolute(), "{} is not absolute", chosen.display());
        assert!(chosen.ends_with("synofs.log"));
    }

    /// Asking the user to create the directory first is a way of ending up with
    /// no log on the one run that needed one.
    #[test]
    fn the_parent_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("state/synology-filestation");

        let chosen = resolve(&nested.join("mount.log"), None).expect("parents are created");

        assert!(nested.is_dir(), "{} was not created", nested.display());
        assert_eq!(chosen, nested.join("mount.log"));
    }

    /// The whole point: a process that dies without unwinding still leaves
    /// every record it emitted. Nothing may sit in a userspace buffer waiting
    /// for a flush that a `SIGKILL` will never deliver.
    #[test]
    fn a_record_reaches_the_file_without_the_caller_flushing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mount.log");
        let log = LogFile::open(path.clone()).unwrap();

        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(log.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("the mount is still answering");
        });

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("the mount is still answering"),
            "log file held {written:?}"
        );
    }

    /// Page cache is not disk. Warnings and errors — the records that explain a
    /// machine that had to be power-cycled — are forced down; ordinary traffic
    /// is not, because an fsync per record at `trace` would itself perturb the
    /// timing under investigation.
    #[test]
    fn warnings_are_forced_to_disk_and_ordinary_records_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let log = LogFile::open(dir.path().join("mount.log")).unwrap();

        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(log.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("routine");
            assert_eq!(
                log.sync_count(),
                0,
                "an info record does not deserve a barrier"
            );

            tracing::warn!("the SMB stream died");
            assert_eq!(log.sync_count(), 1, "a warning must survive the power cut");

            tracing::error!("the mount is wedged");
            assert_eq!(log.sync_count(), 2, "so must an error");
        });
    }

    #[test]
    fn durable_covers_exactly_warn_and_error() {
        assert!(durable(&Level::ERROR));
        assert!(durable(&Level::WARN));
        assert!(!durable(&Level::INFO));
        assert!(!durable(&Level::DEBUG));
        assert!(!durable(&Level::TRACE));
    }

    /// `--log-level trace` on a busy mount writes fast. The machine being
    /// diagnosed is already in trouble; filling its disk is not an acceptable
    /// way to find out why.
    #[test]
    fn the_file_rotates_at_the_cap_and_keeps_a_bounded_number_of_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mount.log");
        let log = LogFile::with_rotation(path.clone(), 64, 2).unwrap();

        for _ in 0..5 {
            log.make_writer().write_all(&[b'x'; 30]).unwrap();
        }

        assert!(path.is_file(), "the active file is always present");
        assert!(rotated(&path, 1).is_file(), "one generation back");
        assert!(rotated(&path, 2).is_file(), "two generations back");
        assert!(
            !rotated(&path, 3).exists(),
            "keep = 2 means the third generation is dropped, not kept"
        );
    }

    /// A rotation that lost the records it rotated would defeat the purpose:
    /// the interesting minute is usually the one just before the end.
    #[test]
    fn rotation_keeps_what_was_already_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mount.log");
        let log = LogFile::with_rotation(path.clone(), 32, 2).unwrap();

        log.make_writer()
            .write_all(b"the first thing that happened\n")
            .unwrap();
        log.make_writer()
            .write_all(b"the thing that came after it\n")
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(rotated(&path, 1)).unwrap(),
            "the first thing that happened\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the thing that came after it\n"
        );
    }

    /// A single record is never split across a rotation, even one bigger than
    /// the cap: half a line explains nothing.
    #[test]
    fn a_record_larger_than_the_cap_is_still_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mount.log");
        let log = LogFile::with_rotation(path.clone(), 16, 2).unwrap();

        let long = "a".repeat(100);
        log.make_writer().write_all(long.as_bytes()).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), long);
    }

    /// Keeping no generations still has to mean a bounded file. Rolling with
    /// nowhere to roll to discards, rather than quietly growing forever or
    /// leaving a stray `mount.log.0` behind.
    #[test]
    fn keeping_no_generations_discards_rather_than_growing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mount.log");
        let log = LogFile::with_rotation(path.clone(), 16, 0).unwrap();

        log.make_writer().write_all(b"older\n").unwrap();
        log.make_writer().write_all(b"and the newest\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "and the newest\n");
        assert!(
            !rotated(&path, 0).exists(),
            "no generation 0 is left behind"
        );
        assert!(!rotated(&path, 1).exists(), "and none is kept");
    }

    fn rotated(path: &Path, generation: usize) -> PathBuf {
        PathBuf::from(format!("{}.{generation}", path.display()))
    }
}
