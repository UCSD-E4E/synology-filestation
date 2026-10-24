//! Noticing a process that is walking the whole NAS.
//!
//! A search started from `/` or `~` (`find / -name x`, `du -sh ~`, an indexer)
//! descends into the mount and reads every directory on the appliance. On the
//! e4e NAS that is millions of directories: such a walk runs for days, keeps
//! the appliance listing directories for nobody, and goes unnoticed, because
//! the process that started it has usually long since stopped reading its
//! output. Seven of them were found and killed by hand in October 2026, the
//! oldest five days old, each holding over a thousand directories open.
//!
//! The mount sees every `opendir`, and the kernel says which thread asked, so
//! it can tell. A process is **walking** when either
//!
//! - it has kept opening directories for [`WALK_FOR`], [`WALK_DIRS`] or more,
//!   with no pause as long as [`QUIET`] (a pause starts the count again), or
//! - it holds [`HELD_DIRS`] directories open at once, which no program reading
//!   a directory or two does, and which `bfs` (the `find` on NixOS) does
//!   whenever it walks.
//!
//! A walker is reported once, as a warning naming the process and how to stop
//! it, and again when it ends. [`Walks::walkers`] lists the walking ones now,
//! which is what the GUI shows, and [`Walks::stop`] ends one, after checking
//! that the process id still names the same process.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// How long a process must keep opening directories to count as walking.
pub const WALK_FOR: Duration = Duration::from_secs(10 * 60);

/// How many directories it must have opened in that time.
pub const WALK_DIRS: u64 = 5_000;

/// Directories held open at once that make a process a walker at once.
pub const HELD_DIRS: u64 = 1_000;

/// A pause in opening directories this long ends a walk.
pub const QUIET: Duration = Duration::from_secs(5 * 60);

/// How long which process a thread belongs to is taken as known. Asking
/// `/proc` on every `opendir` was a few syscalls on the event loop per
/// directory of a walk that can be millions long; once a minute per thread is
/// nothing. A thread id reused by another process inside that minute would
/// have its opens counted against the old one, which costs a count, never a
/// signal: [`Procs::terminate`] checks again.
const RESOLVE_EVERY: Duration = Duration::from_secs(60);

/// How often finished processes are swept out. Every track is looked at, so
/// not on every open.
const PRUNE_EVERY: Duration = Duration::from_secs(1);

/// A process walking the mount, as [`Walks::walkers`] reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Walker {
    /// The process id (not the id of whichever of its threads asked).
    pub pid: u32,
    /// The executable's name, from its first argument.
    pub program: String,
    /// Its command line, shortened.
    pub command: String,
    /// How long it has been walking.
    pub walking_secs: u64,
    /// Directories opened since it started.
    pub dirs_opened: u64,
    /// Directories it holds open now.
    pub dirs_open: u64,
    /// The last directory it opened, as a path under the mountpoint.
    pub latest: String,
}

/// Why [`Walks::stop`] did not stop a process.
#[derive(Debug)]
pub enum StopError {
    /// The process is not walking the mount, or was never seen.
    NotWalking,
    /// The process has exited, or its id now names a different process.
    Gone,
    /// Signalling it failed.
    Io(std::io::Error),
}

impl std::fmt::Display for StopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopError::NotWalking => f.write_str("that process is not walking the mount"),
            StopError::Gone => f.write_str("that process has already exited"),
            StopError::Io(e) => write!(f, "could not stop it: {e}"),
        }
    }
}

impl std::error::Error for StopError {}

/// One process, told apart from any later process given the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// When it started, in whatever unit stays the same for its life.
    pub started: u64,
}

/// A process's command line, for showing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cmdline {
    /// The executable's name.
    pub program: String,
    /// Every argument, space-separated.
    pub line: String,
}

/// What `Walks` needs to know about processes, and the one thing it does to
/// them. [`LinuxProcs`] reads `/proc`; tests supply their own.
///
/// Never called with the `Walks` lock held: every directory open on the mount
/// waits on that lock, and a `/proc` read can stall on a process in D state.
pub trait Procs: Send + Sync {
    /// The process a thread belongs to. `None` for a thread that has exited,
    /// or an id from outside this pid namespace (the kernel sends 0).
    fn identify(&self, tid: u32) -> Option<Process>;
    /// A process's command line.
    fn command(&self, pid: u32) -> Cmdline;
    /// Ask `process` to exit, only if its id still names it. Fails with
    /// [`std::io::ErrorKind::NotFound`] when it has exited or the id names
    /// another process now.
    fn terminate(&self, process: Process) -> std::io::Result<()>;
}

/// The longest command line a [`Walker`] carries, in characters.
const MAX_COMMAND: usize = 200;

fn shorten(command: &str) -> String {
    if command.chars().count() <= MAX_COMMAND {
        return command.to_string();
    }
    let mut short: String = command.chars().take(MAX_COMMAND - 1).collect();
    short.push('…');
    short
}

/// A length of time as a person would say it; the same words as the GUI's
/// `WalkerPresenter.Duration`.
fn spoken(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86399 => format!("{} h {} min", secs / 3600, secs % 3600 / 60),
        _ => format!("{} d {} h", secs / 86400, secs % 86400 / 3600),
    }
}

/// [`Procs`] from `/proc`.
#[cfg(target_os = "linux")]
pub struct LinuxProcs;

#[cfg(target_os = "linux")]
impl LinuxProcs {
    /// Field 22 of `/proc/<pid>/stat`, in clock ticks since boot. Counted from
    /// after the command name, which is in parentheses and may hold spaces
    /// and parentheses of its own.
    fn started(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(19)?.parse().ok()
    }
}

#[cfg(target_os = "linux")]
impl Procs for LinuxProcs {
    /// From `Tgid:` in `/proc/<tid>/status` (a thread's directory is there
    /// even though `/proc` does not list it), then that process's start time.
    fn identify(&self, tid: u32) -> Option<Process> {
        if tid == 0 {
            return None;
        }
        let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
        let pid = status
            .lines()
            .find_map(|line| line.strip_prefix("Tgid:"))
            .and_then(|tgid| tgid.trim().parse().ok())?;
        Some(Process {
            pid,
            started: Self::started(pid)?,
        })
    }

    /// The arguments, and the file name of the first; `[comm]` for a process
    /// without any (a kernel thread, or one that has cleared them). The
    /// program comes from the first argument alone, since joined with spaces
    /// a path holding a space is no longer one word.
    fn command(&self, pid: u32) -> Cmdline {
        let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<_> = args
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(String::from_utf8_lossy)
            .collect();
        if let Some(first) = args.first() {
            let program = std::path::Path::new(first.as_ref())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| first.to_string());
            return Cmdline {
                program,
                line: args.join(" "),
            };
        }
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let name = format!("[{}]", comm.trim());
        Cmdline {
            program: name.clone(),
            line: name,
        }
    }

    /// Through a pidfd, so the check and the signal are about one process: the
    /// pidfd is opened first and names whatever holds the id then; the start
    /// time read after it says whether that is ours; and the signal goes to
    /// the pidfd, which never reaches a later process given the same id. A
    /// plain `kill` after the check could, if ours exited in between.
    fn terminate(&self, process: Process) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        use std::os::fd::{FromRawFd, OwnedFd};

        let gone = || Error::from(ErrorKind::NotFound);
        let pid = libc::pid_t::try_from(process.pid).map_err(|_| ErrorKind::InvalidInput)?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            let e = Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ESRCH) => gone(),
                _ => e,
            });
        }
        // Owned, so it is closed on every way out.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if Self::started(process.pid) != Some(process.started) {
            return Err(gone());
        }
        let sent = unsafe {
            use std::os::fd::AsRawFd;
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent == 0 {
            return Ok(());
        }
        let e = Error::last_os_error();
        Err(match e.raw_os_error() {
            Some(libc::ESRCH) => gone(),
            _ => e,
        })
    }
}

/// Every process that has opened a directory on the mount lately.
///
/// What `opendir` and `releasedir` report goes down a channel to a thread of
/// its own, which does all the work: reading `/proc` is a few syscalls, and
/// `/proc/<pid>/cmdline` waits on the target's mmap lock, which a process
/// faulting in a page of a file on this very mount can be holding. On the
/// FUSE event loop that is a way for the mount to wedge itself. The event
/// loop only sends.
pub struct Walks {
    core: Arc<Core>,
    events: Option<std::sync::mpsc::Sender<Event>>,
}

enum Event {
    Opened {
        fh: u64,
        tid: u32,
        path: String,
        at: Instant,
    },
    Released {
        fh: u64,
    },
}

impl Walks {
    /// Watching the mount at `mountpoint`, from this process.
    #[cfg(target_os = "linux")]
    pub fn new(mountpoint: PathBuf) -> Self {
        Self::spawn(Arc::new(LinuxProcs), std::process::id(), mountpoint)
    }

    /// With its own thread; it ends when this is dropped.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn spawn(procs: Arc<dyn Procs>, own: u32, mountpoint: PathBuf) -> Self {
        let core = Arc::new(Core::new(procs, own, mountpoint));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = Arc::clone(&core);
        let spawned = std::thread::Builder::new()
            .name("walks".into())
            .spawn(move || worker.run(rx));
        match spawned {
            Ok(_) => Self {
                core,
                events: Some(tx),
            },
            Err(e) => {
                tracing::warn!("could not start the thread that watches for walks: {e}");
                Self { core, events: None }
            }
        }
    }

    /// Without a thread: every call is answered on the caller's.
    #[cfg(test)]
    pub(crate) fn with(procs: Arc<dyn Procs>, own: u32, mountpoint: PathBuf) -> Self {
        Self {
            core: Arc::new(Core::new(procs, own, mountpoint)),
            events: None,
        }
    }

    /// Thread `tid` opened the directory at NAS path `path` as handle `fh`.
    pub fn opened(&self, fh: u64, tid: u32, path: &str) {
        let at = Instant::now();
        match &self.events {
            Some(tx) => {
                let _ = tx.send(Event::Opened {
                    fh,
                    tid,
                    path: path.to_string(),
                    at,
                });
            }
            None => {
                self.core.opened_at(fh, tid, path, at);
            }
        }
    }

    /// Handle `fh` was closed.
    pub fn released(&self, fh: u64) {
        match &self.events {
            Some(tx) => {
                let _ = tx.send(Event::Released { fh });
            }
            None => self.core.released(fh),
        }
    }

    /// The processes walking the mount now, by pid. Never reads `/proc`.
    pub fn walkers(&self) -> Vec<Walker> {
        self.core.walkers_at(Instant::now())
    }

    /// Ask walker `pid` to exit (`SIGTERM`).
    ///
    /// Only a process walking now, and only while its id still names the
    /// process that was seen walking: ids are reused, and the one a window
    /// showed a minute ago may belong to something else by the time the button
    /// is pressed. [`Procs::terminate`] makes that check and the signal one
    /// step.
    pub fn stop(&self, pid: u32) -> Result<(), StopError> {
        self.core.stop_at(pid, Instant::now())
    }
}

/// The state, and everything that changes it.
struct Core {
    procs: Arc<dyn Procs>,
    /// This process: the mount reading itself is never a walker.
    own: u32,
    mountpoint: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Which process each thread belongs to, and when that was found out.
    threads: HashMap<u32, (Process, Instant)>,
    /// Which process holds each open directory handle.
    handles: HashMap<u64, u32>,
    tracks: HashMap<u32, Track>,
    /// When finished tracks were last swept out.
    pruned: Option<Instant>,
}

/// One process's recent directory opens.
struct Track {
    /// What tells this process from a later one given the same id.
    started: u64,
    /// Read only once the walk is reported: most processes that open a
    /// directory never are, and reading it is the expensive part.
    command: Cmdline,
    /// When the current run of opens began, and when the latest was.
    first: Instant,
    last: Instant,
    /// Opens in the current run.
    opened: u64,
    /// Handles open now.
    open: u64,
    /// NAS path of the latest open.
    latest: String,
    /// Whether this run was reported as a walk. Only reported walks are
    /// listed, so the list and the log never disagree.
    reported: bool,
}

impl Track {
    fn walking(&self, now: Instant) -> bool {
        self.held()
            || (now.duration_since(self.last) < QUIET
                && self.opened >= WALK_DIRS
                && now.duration_since(self.first) >= WALK_FOR)
    }

    /// Walking by what it holds, however long since it last opened anything.
    fn held(&self) -> bool {
        self.open >= HELD_DIRS
    }

    /// Gone quiet and holding nothing: nothing left to watch.
    fn finished(&self, now: Instant) -> bool {
        self.open == 0 && now.duration_since(self.last) >= QUIET
    }
}

/// What a call found that is to be logged once the lock is let go.
#[derive(Default)]
struct News {
    /// Walks that have just started: their command is still to be read.
    started: Vec<Process>,
    stopped: Vec<Walker>,
}

impl Core {
    fn new(procs: Arc<dyn Procs>, own: u32, mountpoint: PathBuf) -> Self {
        Self {
            procs,
            own,
            mountpoint,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The thread behind [`Walks::spawn`]: events as they come, and a sweep
    /// whenever none has come for [`PRUNE_EVERY`], so a walk that qualifies
    /// by time alone, or ends, is said so without waiting for an open.
    fn run(&self, events: std::sync::mpsc::Receiver<Event>) {
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            match events.recv_timeout(PRUNE_EVERY) {
                Ok(Event::Opened { fh, tid, path, at }) => {
                    self.opened_at(fh, tid, &path, at);
                }
                Ok(Event::Released { fh }) => self.released(fh),
                Err(RecvTimeoutError::Timeout) => {
                    self.tick(Instant::now(), false);
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Sweep, and say what changed: the walks that started.
    fn tick(&self, now: Instant, force: bool) -> Vec<Walker> {
        let mut news = News::default();
        self.prune(&mut self.inner.lock().unwrap(), now, force, &mut news);
        self.announce(news, now)
    }

    /// An open at `now`: the walker, when this is the open that made it one.
    ///
    /// The usual open, by a thread seen in the last [`RESOLVE_EVERY`], is one
    /// lock and no syscalls. Otherwise `/proc` is read between two locks,
    /// never under one.
    fn opened_at(&self, fh: u64, tid: u32, path: &str, now: Instant) -> Option<Walker> {
        if tid == 0 {
            return None;
        }
        let mut news = News::default();

        {
            let mut inner = self.inner.lock().unwrap();
            self.prune(&mut inner, now, false, &mut news);
            let cached = inner
                .threads
                .get(&tid)
                .filter(|(_, at)| now.duration_since(*at) < RESOLVE_EVERY)
                .map(|(process, _)| *process);
            match cached {
                Some(process) if process.pid == self.own => {
                    drop(inner);
                    self.announce(news, now);
                    return None;
                }
                Some(process)
                    if inner
                        .tracks
                        .get(&process.pid)
                        .is_some_and(|t| t.started == process.started) =>
                {
                    self.record(&mut inner, process, fh, path, now, &mut news);
                    drop(inner);
                    return self.announce(news, now).into_iter().next();
                }
                // Not known, or known as a process its id no longer names:
                // ask again. A stale entry trusted here would hand the live
                // process's track to the one that has exited.
                _ => {}
            }
        }

        let Some(process) = self.procs.identify(tid) else {
            self.announce(news, now);
            return None;
        };
        let mut inner = self.inner.lock().unwrap();
        inner.threads.insert(tid, (process, now));
        if process.pid != self.own {
            self.record(&mut inner, process, fh, path, now, &mut news);
        }
        drop(inner);
        self.announce(news, now).into_iter().next()
    }

    /// Count an open by `process`, starting its track if it has none, or if
    /// the one under its id belongs to an earlier process.
    fn record(
        &self,
        inner: &mut Inner,
        process: Process,
        fh: u64,
        path: &str,
        now: Instant,
        news: &mut News,
    ) {
        let pid = process.pid;
        if inner
            .tracks
            .get(&pid)
            .is_some_and(|t| t.started != process.started)
        {
            // The id has been given to a new process. The old one has exited
            // (its handles went with it), so its walk is over.
            if let Some(old) = inner.tracks.remove(&pid) {
                if old.reported {
                    news.stopped.push(self.walker(pid, &old, old.last));
                }
            }
            inner.handles.retain(|_, p| *p != pid);
        }
        inner.handles.insert(fh, pid);
        let track = inner.tracks.entry(pid).or_insert_with(|| Track {
            started: process.started,
            command: Cmdline::default(),
            first: now,
            last: now,
            opened: 0,
            open: 0,
            latest: String::new(),
            reported: false,
        });
        // A pause ends a run of opens, but not a walk held open: a `find`
        // resumed after ^Z is the same walk, with the same start.
        if now.duration_since(track.last) >= QUIET && !track.held() {
            if track.reported {
                news.stopped.push(self.walker(pid, track, track.last));
            }
            track.first = now;
            track.opened = 0;
            track.reported = false;
        }
        track.last = now;
        track.opened += 1;
        track.open += 1;
        track.latest = path.to_string();
        if !track.reported && track.walking(now) {
            track.reported = true;
            news.started.push(process);
        }
    }

    fn released(&self, fh: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(pid) = inner.handles.remove(&fh) {
            if let Some(track) = inner.tracks.get_mut(&pid) {
                track.open = track.open.saturating_sub(1);
            }
        }
    }

    fn walkers_at(&self, now: Instant) -> Vec<Walker> {
        let inner = self.inner.lock().unwrap();
        let mut walkers: Vec<_> = inner
            .tracks
            .iter()
            .filter(|(_, track)| track.reported && track.walking(now))
            .map(|(pid, track)| self.walker(*pid, track, now))
            .collect();
        drop(inner);
        walkers.sort_by_key(|w| w.pid);
        walkers
    }

    /// Sweep now, whenever it last ran: the walks that have ended since.
    #[cfg(test)]
    fn prune_at(&self, now: Instant) -> Vec<Walker> {
        let mut news = News::default();
        self.prune(&mut self.inner.lock().unwrap(), now, true, &mut news);
        let stopped = news.stopped.clone();
        self.announce(news, now);
        stopped
    }

    /// Settle every track against `now`: report the walks that have started
    /// (one can, by time alone, between opens), end the ones that no longer
    /// are one, and forget processes gone quiet holding nothing and threads
    /// not seen for a while. At most every [`PRUNE_EVERY`] unless `force`d.
    ///
    /// A walk ends when its process stops qualifying, whether or not it still
    /// holds directories: a `find` suspended with ^Z keeps its handles, and
    /// its walk ending unlogged left the log with two starts and no end.
    fn prune(&self, inner: &mut Inner, now: Instant, force: bool, news: &mut News) {
        if !force
            && inner
                .pruned
                .is_some_and(|at| now.duration_since(at) < PRUNE_EVERY)
        {
            return;
        }
        inner.pruned = Some(now);
        for (pid, track) in inner.tracks.iter_mut() {
            let walking = track.walking(now);
            if track.reported && !walking {
                news.stopped.push(self.walker(*pid, track, track.last));
                track.reported = false;
            } else if !track.reported && walking {
                track.reported = true;
                news.started.push(Process {
                    pid: *pid,
                    started: track.started,
                });
            }
        }
        inner.tracks.retain(|_, track| !track.finished(now));
        inner
            .threads
            .retain(|_, (_, at)| now.duration_since(*at) < RESOLVE_EVERY);
    }

    /// Read the command of each walk that started, with no lock held, and
    /// log what a call found. The walkers that started.
    fn announce(&self, news: News, now: Instant) -> Vec<Walker> {
        for w in &news.stopped {
            tracing::info!(
                "pid {} ({}) has stopped walking the mount, after {} directories in {}",
                w.pid,
                w.program,
                w.dirs_opened,
                spoken(w.walking_secs)
            );
        }
        let mut started = Vec::new();
        for process in news.started {
            let command = self.procs.command(process.pid);
            let walker = {
                let mut inner = self.inner.lock().unwrap();
                match inner.tracks.get_mut(&process.pid) {
                    Some(track) if track.started == process.started => {
                        track.command = command;
                        self.walker(process.pid, track, now)
                    }
                    // Gone in the moment the lock was let go.
                    _ => continue,
                }
            };
            let mount = self.mountpoint.display();
            tracing::warn!(
                "pid {} ({}) is walking the whole mount: {} directories in {}, {} open now, \
                 the latest {}. Command: {}. A search from / or ~ that does not skip {mount} \
                 reads every directory on the NAS and runs for days. If that is not what was \
                 meant, stop it (`kill {}`); keep such searches off the mount with \
                 `find -xdev` or by pruning {mount}.",
                walker.pid,
                walker.program,
                walker.dirs_opened,
                spoken(walker.walking_secs),
                walker.dirs_open,
                walker.latest,
                walker.command,
                walker.pid,
            );
            started.push(walker);
        }
        started
    }

    fn walker(&self, pid: u32, track: &Track, now: Instant) -> Walker {
        Walker {
            pid,
            program: track.command.program.clone(),
            command: shorten(&track.command.line),
            walking_secs: now.duration_since(track.first).as_secs(),
            dirs_opened: track.opened,
            dirs_open: track.open,
            latest: self
                .mountpoint
                .join(track.latest.trim_start_matches('/'))
                .display()
                .to_string(),
        }
    }

    fn stop_at(&self, pid: u32, now: Instant) -> Result<(), StopError> {
        let (process, command) = {
            let inner = self.inner.lock().unwrap();
            let Some(track) = inner
                .tracks
                .get(&pid)
                .filter(|t| t.reported && t.walking(now))
            else {
                return Err(StopError::NotWalking);
            };
            let process = Process {
                pid,
                started: track.started,
            };
            (process, track.command.line.clone())
        };
        match self.procs.terminate(process) {
            Ok(()) => {
                tracing::info!("stopped pid {pid}, which was walking the mount: {command}");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // It exited on its own. Its walk is over, and the log says so.
                let mut news = News::default();
                {
                    let mut inner = self.inner.lock().unwrap();
                    if inner
                        .tracks
                        .get(&pid)
                        .is_some_and(|t| t.started == process.started)
                    {
                        if let Some(track) = inner.tracks.remove(&pid) {
                            if track.reported {
                                news.stopped.push(self.walker(pid, &track, track.last));
                            }
                        }
                        inner.handles.retain(|_, p| *p != pid);
                    }
                }
                self.announce(news, now);
                Err(StopError::Gone)
            }
            Err(e) => Err(StopError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{OnceLock, Weak};

    /// Processes as a table: thread → process, start times, and a record of
    /// what was asked and what was terminated.
    #[derive(Default)]
    struct Fake {
        threads: Mutex<HashMap<u32, u32>>,
        started: Mutex<HashMap<u32, u64>>,
        terminated: Mutex<Vec<u32>>,
        identified: AtomicUsize,
        /// The watch this answers for, to catch a call made under its lock.
        walks: OnceLock<Weak<Core>>,
        commands: AtomicUsize,
        /// The names of the threads every call was made on.
        callers: Mutex<Vec<Option<String>>>,
        under_lock: AtomicUsize,
    }

    impl Fake {
        /// A process `pid` started at `at`, with threads `pid` and `tids`.
        fn spawn(&self, pid: u32, at: u64, tids: &[u32]) {
            let mut threads = self.threads.lock().unwrap();
            threads.insert(pid, pid);
            for tid in tids {
                threads.insert(*tid, pid);
            }
            self.started.lock().unwrap().insert(pid, at);
        }

        fn exit(&self, pid: u32) {
            self.threads.lock().unwrap().retain(|_, p| *p != pid);
            self.started.lock().unwrap().remove(&pid);
        }

        fn check_lock(&self) {
            self.callers
                .lock()
                .unwrap()
                .push(std::thread::current().name().map(String::from));
            if let Some(walks) = self.walks.get().and_then(Weak::upgrade) {
                if walks.inner.try_lock().is_err() {
                    self.under_lock.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    impl Procs for Fake {
        fn identify(&self, tid: u32) -> Option<Process> {
            self.check_lock();
            self.identified.fetch_add(1, Ordering::SeqCst);
            let pid = *self.threads.lock().unwrap().get(&tid)?;
            let started = *self.started.lock().unwrap().get(&pid)?;
            Some(Process { pid, started })
        }
        fn command(&self, pid: u32) -> Cmdline {
            self.check_lock();
            self.commands.fetch_add(1, Ordering::SeqCst);
            let started = self.started.lock().unwrap().get(&pid).copied().unwrap_or(0);
            Cmdline {
                program: "find".into(),
                line: format!("find / -name x # {pid}@{started}"),
            }
        }
        fn terminate(&self, process: Process) -> std::io::Result<()> {
            self.check_lock();
            if self.started.lock().unwrap().get(&process.pid) != Some(&process.started) {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            self.terminated.lock().unwrap().push(process.pid);
            Ok(())
        }
    }

    const OWN: u32 = 1;

    fn watch() -> (Arc<Fake>, Arc<Core>) {
        let fake = Arc::new(Fake::default());
        let walks = Arc::new(Core::new(fake.clone(), OWN, PathBuf::from("/home/u/mnt")));
        fake.walks.set(Arc::downgrade(&walks)).unwrap();
        (fake, walks)
    }

    /// `n` opens and immediate closes by `tid`, evenly over `over`, starting
    /// at `t0` and numbering handles from `fh`. The last open's result.
    fn walk(
        walks: &Core,
        tid: u32,
        fh: &mut u64,
        t0: Instant,
        over: Duration,
        n: u64,
    ) -> Option<Walker> {
        let mut last = None;
        for i in 0..n {
            let at = t0 + over.mul_f64(i as f64 / (n - 1).max(1) as f64);
            last = walks.opened_at(*fh, tid, &format!("/share/d{i}"), at);
            walks.released(*fh);
            *fh += 1;
        }
        last
    }

    /// `HELD_DIRS` handles opened by `tid` at `at`, left open.
    fn hold(walks: &Core, tid: u32, fh: &mut u64, at: Instant) -> Option<Walker> {
        let mut reported = None;
        for _ in 0..HELD_DIRS {
            reported = reported.or(walks.opened_at(*fh, tid, "/share/d", at));
            *fh += 1;
        }
        reported
    }

    #[test]
    fn a_long_steady_walk_is_reported_once_with_who_and_where() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 1;

        // Just short of both thresholds: nothing.
        assert_eq!(
            walk(
                &walks,
                100,
                &mut fh,
                t0,
                WALK_FOR - Duration::from_secs(1),
                WALK_DIRS
            ),
            None
        );
        assert!(walks
            .walkers_at(t0 + WALK_FOR - Duration::from_secs(1))
            .is_empty());

        // The open that crosses them reports it.
        let reported = walks
            .opened_at(fh, 100, "/share/deep", t0 + WALK_FOR)
            .expect("the crossing open reports the walker");
        walks.released(fh);
        fh += 1;
        assert_eq!(
            reported,
            Walker {
                pid: 100,
                program: "find".into(),
                command: "find / -name x # 100@7".into(),
                walking_secs: WALK_FOR.as_secs(),
                dirs_opened: WALK_DIRS + 1,
                dirs_open: 1,
                latest: "/home/u/mnt/share/deep".into(),
            }
        );

        // Later opens do not report it again, but it stays listed.
        let later = t0 + WALK_FOR + Duration::from_secs(1);
        assert_eq!(walks.opened_at(fh, 100, "/share/x", later), None);
        let listed = walks.walkers_at(later);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].pid, 100);
        assert_eq!(listed[0].dirs_open, 1);
    }

    #[test]
    fn a_fast_walk_that_ends_soon_is_not_one() {
        // `ls -R` of a big project: many directories, but over in a minute.
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 1;
        walk(
            &walks,
            100,
            &mut fh,
            t0,
            Duration::from_secs(60),
            WALK_DIRS * 4,
        );
        assert!(walks.walkers_at(t0 + Duration::from_secs(61)).is_empty());
    }

    #[test]
    fn a_long_slow_browse_is_not_one() {
        // Someone clicking through folders for an hour.
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 1;
        walk(&walks, 100, &mut fh, t0, Duration::from_secs(3600), 600);
        assert!(walks.walkers_at(t0 + Duration::from_secs(3600)).is_empty());
    }

    #[test]
    fn threads_count_as_their_process() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[101, 102, 103]);
        let t0 = Instant::now();
        let mut fh = 1;
        for (i, tid) in [100, 101, 102, 103].into_iter().enumerate() {
            let start = t0 + (WALK_FOR / 4) * i as u32;
            walk(&walks, tid, &mut fh, start, WALK_FOR / 4, WALK_DIRS / 4 + 1);
        }
        let listed = walks.walkers_at(t0 + WALK_FOR);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].pid, 100);
    }

    #[test]
    fn holding_many_directories_open_is_walking_at_once() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 0;
        let reported = hold(&walks, 100, &mut fh, t0).expect("reported on reaching the limit");
        assert_eq!(reported.dirs_open, HELD_DIRS);
        assert_eq!(walks.walkers_at(t0).len(), 1);

        // Letting go of most of them, and not opening any more: no longer,
        // and said so.
        for fh in 0..HELD_DIRS - 10 {
            walks.released(fh);
        }
        let later = t0 + Duration::from_secs(1);
        let stopped = walks.prune_at(later);
        assert_eq!(stopped.iter().map(|w| w.pid).collect::<Vec<_>>(), [100]);
        assert!(walks.walkers_at(later).is_empty());
    }

    #[test]
    fn a_pause_starts_the_count_again() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 1;
        walk(&walks, 100, &mut fh, t0, WALK_FOR / 2, WALK_DIRS);
        // A long pause, then the other half: never ten steady minutes.
        let resumed = t0 + WALK_FOR / 2 + QUIET;
        walk(&walks, 100, &mut fh, resumed, WALK_FOR / 2, WALK_DIRS);
        assert!(walks.walkers_at(resumed + WALK_FOR / 2).is_empty());
    }

    #[test]
    fn a_walker_that_goes_quiet_is_reported_stopped_and_forgotten() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 1;
        walk(&walks, 100, &mut fh, t0, WALK_FOR, WALK_DIRS + 1);
        assert_eq!(walks.walkers_at(t0 + WALK_FOR).len(), 1);

        let quiet = t0 + WALK_FOR + QUIET;
        let stopped = walks.prune_at(quiet);
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].pid, 100);
        assert!(walks.walkers_at(quiet).is_empty());
        // Reported once.
        assert!(walks.prune_at(quiet + QUIET).is_empty());
    }

    #[test]
    fn a_walker_that_pauses_holding_directories_is_reported_stopped() {
        // `find` suspended with ^Z: quiet, but its handles stay open. Its walk
        // is over until it resumes, and the log should say so, not show a
        // second start with no end to the first.
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 1;
        walk(&walks, 100, &mut fh, t0, WALK_FOR, WALK_DIRS + 1);
        for _ in 0..200 {
            walks.opened_at(fh, 100, "/share/held", t0 + WALK_FOR);
            fh += 1;
        }
        assert_eq!(walks.walkers_at(t0 + WALK_FOR).len(), 1);

        let paused = t0 + WALK_FOR + QUIET;
        let stopped = walks.prune_at(paused);
        assert_eq!(stopped.iter().map(|w| w.pid).collect::<Vec<_>>(), [100]);
        assert!(walks.walkers_at(paused).is_empty());
        assert!(walks.prune_at(paused + QUIET).is_empty(), "said once");
    }

    #[test]
    fn a_quiet_process_that_never_walked_goes_without_a_word() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        walks.opened_at(1, 100, "/share", t0);
        walks.released(1);
        assert!(walks.prune_at(t0 + QUIET).is_empty());
        assert!(walks.inner.lock().unwrap().tracks.is_empty());
    }

    #[test]
    fn the_mount_itself_and_unknown_threads_are_ignored() {
        let (fake, walks) = watch();
        fake.spawn(OWN, 7, &[2]);
        let t0 = Instant::now();
        for fh in 0..HELD_DIRS {
            walks.opened_at(fh, 2, "/share", t0); // a thread of this process
            walks.opened_at(HELD_DIRS + fh, 0, "/share", t0); // from another namespace
            walks.opened_at(2 * HELD_DIRS + fh, 999, "/share", t0); // exited
        }
        assert!(walks.walkers_at(t0).is_empty());
        assert!(walks.inner.lock().unwrap().tracks.is_empty());
    }

    #[test]
    fn a_reused_pid_is_a_new_process() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 0;
        hold(&walks, 100, &mut fh, t0);
        for fh in 0..fh {
            walks.released(fh);
        }
        fake.exit(100);

        // Inside QUIET, before the old track could be swept out, a new
        // process is given the same id and walks.
        let soon = t0 + RESOLVE_EVERY;
        fake.spawn(100, 8, &[]);
        let reported = hold(&walks, 100, &mut fh, soon).expect("a new walk");
        assert_eq!(reported.command, "find / -name x # 100@8");
        assert_eq!(reported.walking_secs, 0);
        assert_eq!(reported.dirs_opened, HELD_DIRS);

        walks
            .stop_at(100, soon)
            .expect("the new process can be stopped");
        assert_eq!(*fake.terminated.lock().unwrap(), vec![100]);
    }

    #[test]
    fn a_thread_is_looked_up_once_a_minute_not_on_every_open() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[101]);
        let t0 = Instant::now();
        let mut fh = 1;
        walk(&walks, 100, &mut fh, t0, Duration::from_secs(30), 1000);
        walk(&walks, 101, &mut fh, t0, Duration::from_secs(30), 1000);
        assert_eq!(fake.identified.load(Ordering::SeqCst), 2);

        walk(
            &walks,
            100,
            &mut fh,
            t0 + RESOLVE_EVERY,
            Duration::from_secs(1),
            10,
        );
        assert_eq!(fake.identified.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn proc_is_never_read_under_the_lock() {
        // Every directory open on the mount waits on that lock, and a read of
        // /proc can stall behind a process in D state.
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[101]);
        let t0 = Instant::now();
        let mut fh = 0;
        hold(&walks, 100, &mut fh, t0);
        walks.opened_at(fh, 101, "/share", t0 + RESOLVE_EVERY);
        walks.walkers_at(t0 + RESOLVE_EVERY);
        walks.stop_at(100, t0 + RESOLVE_EVERY).expect("stopped");
        assert!(fake.identified.load(Ordering::SeqCst) > 0);
        assert_eq!(fake.under_lock.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn stop_signals_a_walker() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        hold(&walks, 100, &mut 0, t0);
        walks.stop_at(100, t0).expect("stopped");
        assert_eq!(*fake.terminated.lock().unwrap(), vec![100]);
    }

    #[test]
    fn stop_refuses_a_process_that_is_not_walking() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        walks.opened_at(1, 100, "/share", t0);
        assert!(matches!(walks.stop_at(100, t0), Err(StopError::NotWalking)));
        assert!(matches!(walks.stop_at(555, t0), Err(StopError::NotWalking)));
        assert!(fake.terminated.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_refuses_an_id_that_now_names_another_process() {
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        hold(&walks, 100, &mut 0, t0);
        // The walker exits without its handles being seen to close, and its id
        // goes to something else.
        fake.exit(100);
        fake.spawn(100, 8, &[]);
        assert!(matches!(walks.stop_at(100, t0), Err(StopError::Gone)));
        assert!(fake.terminated.lock().unwrap().is_empty());
        assert!(
            walks.walkers_at(t0).is_empty(),
            "a gone walker is not listed"
        );
    }

    #[test]
    fn the_caller_of_opened_never_does_the_work() {
        // `opened` is called on the FUSE event loop, and reading
        // /proc/<pid>/cmdline waits on that process's mmap lock, which a
        // process faulting in a page of a file on this mount can be holding.
        let fake = Arc::new(Fake::default());
        fake.spawn(100, 7, &[]);
        let walks = Walks::spawn(fake.clone(), OWN, PathBuf::from("/m"));
        for fh in 0..HELD_DIRS {
            walks.opened(fh, 100, "/share");
        }
        let listed = (0..200)
            .find_map(|_| {
                let w = walks.walkers();
                if w.is_empty() {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                } else {
                    Some(w)
                }
            })
            .expect("the walker is listed once the thread catches up");
        assert_eq!(listed[0].command, "find / -name x # 100@7");

        let callers = fake.callers.lock().unwrap();
        assert!(!callers.is_empty());
        assert!(
            callers.iter().all(|name| name.as_deref() == Some("walks")),
            "{callers:?}"
        );
    }

    #[test]
    fn a_command_is_read_only_for_a_walk() {
        // Every `ls` opens a directory; none of them is worth a read of its
        // command line.
        let (fake, walks) = watch();
        let t0 = Instant::now();
        for pid in 200..300 {
            fake.spawn(pid, 7, &[]);
            walks.opened_at(pid as u64, pid, "/share", t0);
            walks.released(pid as u64);
        }
        assert_eq!(fake.commands.load(Ordering::SeqCst), 0);

        fake.spawn(100, 7, &[]);
        hold(&walks, 100, &mut 1000, t0);
        assert_eq!(fake.commands.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_held_walk_resumed_after_a_pause_is_the_same_walk() {
        // `bfs` holding its directories, suspended with ^Z, then `fg`: still
        // walking all along, so no end and no second start.
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 0;
        hold(&walks, 100, &mut fh, t0);

        let resumed = t0 + QUIET + Duration::from_secs(60);
        assert!(walks.prune_at(resumed).is_empty(), "never stopped");
        assert_eq!(walks.opened_at(fh, 100, "/share/next", resumed), None);
        let listed = walks.walkers_at(resumed);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].walking_secs,
            (QUIET + Duration::from_secs(60)).as_secs()
        );
        assert_eq!(listed[0].dirs_opened, HELD_DIRS + 1);
    }

    #[test]
    fn a_stale_thread_does_not_take_the_live_process_s_track() {
        // Process A (pid 100) exits and B is given its id within a minute.
        // B's thread 101 is looked up fresh; B's main thread, 100, is still
        // remembered as A's.
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        walks.opened_at(1, 100, "/share", t0);
        walks.released(1);
        fake.exit(100);
        fake.spawn(100, 8, &[101]);

        let soon = t0 + Duration::from_secs(10);
        walks.opened_at(2, 101, "/share", soon);
        walks.opened_at(3, 100, "/share", soon);

        let inner = walks.inner.lock().unwrap();
        let track = &inner.tracks[&100];
        assert_eq!(track.started, 8);
        assert_eq!(track.opened, 2);
        assert_eq!(track.open, 2);
    }

    #[test]
    fn a_walk_that_qualifies_by_time_alone_is_reported_before_it_is_listed() {
        // 5,001 directories in 9.5 minutes, then nothing for a while: at ten
        // minutes it is walking, and the log must say so before the window
        // does, or the two disagree.
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        let mut fh = 1;
        walk(
            &walks,
            100,
            &mut fh,
            t0,
            WALK_FOR - Duration::from_secs(30),
            WALK_DIRS + 1,
        );
        let at = t0 + WALK_FOR;
        assert!(
            walks.walkers_at(at).is_empty(),
            "not listed before reported"
        );

        let started = walks.tick(at, true);
        assert_eq!(started.iter().map(|w| w.pid).collect::<Vec<_>>(), [100]);
        assert_eq!(walks.walkers_at(at).len(), 1);

        let quiet = t0 + WALK_FOR - Duration::from_secs(30) + QUIET;
        assert_eq!(walks.prune_at(quiet).len(), 1, "and its end is said too");
    }

    #[test]
    fn a_walker_that_exits_before_stop_is_logged_as_stopped() {
        let capture = crate::log_capture::LogCapture::at_the_default_level();
        let (fake, walks) = watch();
        fake.spawn(100, 7, &[]);
        let t0 = Instant::now();
        hold(&walks, 100, &mut 0, t0);
        fake.exit(100);

        assert!(matches!(walks.stop_at(100, t0), Err(StopError::Gone)));
        let text = capture.text();
        assert!(text.contains("pid 100 (find) is walking"), "got: {text}");
        assert!(
            text.contains("pid 100 (find) has stopped walking"),
            "got: {text}"
        );
    }

    #[test]
    fn a_long_command_line_is_shortened() {
        assert!(shorten(&"x".repeat(1000)).chars().count() <= MAX_COMMAND);
        assert_eq!(shorten("find / -name x"), "find / -name x");
    }

    #[test]
    fn a_duration_reads_as_the_gui_says_it() {
        assert_eq!(spoken(45), "45 s");
        assert_eq!(spoken(600), "10 min");
        assert_eq!(spoken(7500), "2 h 5 min");
        assert_eq!(spoken(5 * 86400 + 4 * 3600 + 59), "5 d 4 h");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_procs_reads_this_process() {
        let me = std::process::id();
        let procs = LinuxProcs;
        let found = procs.identify(me).expect("this process");
        assert_eq!(found.pid, me);
        let command = procs.command(me);
        let argv0 = std::env::args().next().unwrap();
        let name = std::path::Path::new(&argv0).file_name().unwrap();
        assert_eq!(command.program, name.to_string_lossy());
        assert!(command.line.starts_with(&argv0));

        // A thread of this process answers with this process.
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let live = std::thread::spawn(move || {
            tx.send(unsafe { libc::gettid() } as u32).unwrap();
            let _ = done_rx.recv();
        });
        let tid = rx.recv().unwrap();
        assert_eq!(procs.identify(tid), Some(found));
        done_tx.send(()).unwrap();
        live.join().unwrap();

        assert_eq!(procs.identify(0), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_procs_signals_only_the_process_it_was_given() {
        use std::os::unix::process::ExitStatusExt;

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep");
        let procs = LinuxProcs;
        let found = procs.identify(child.id()).expect("the child");

        // Another process under the same id, as far as the start time says.
        let other = Process {
            started: found.started + 1,
            ..found
        };
        let refused = procs.terminate(other).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::NotFound);
        assert!(child.try_wait().unwrap().is_none(), "left alone");

        procs.terminate(found).expect("signalled");
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));

        // Reaped: the id names nothing now.
        assert_eq!(
            procs.terminate(found).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
