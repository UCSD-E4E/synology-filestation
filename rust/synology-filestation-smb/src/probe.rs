//! Deciding whether a new client gets SMB, without letting the decision get
//! the caller's address blocked.
//!
//! Every login probes SMB with the same credentials and falls back to HTTP if
//! the probe fails. A probe that fails on *authentication* is not free: DSM
//! counts it towards its auto-block (on e4e-nas, 3 failures in 24 hours, and
//! the block is permanent), and the block covers every service on the
//! appliance, HTTPS included, for every user behind the same address. A
//! service that built a client per task with a username SMB did not accept
//! (a bare `svc_fishsense`, which FileStation takes and SMB reads as a local
//! account) got its cluster's shared NAT address blocked this way.
//!
//! So a refused login is remembered for a cool-down, process-wide, per
//! account, and nothing in this crate authenticates that account again in the
//! meantime — not a new client's probe, and not an existing transport
//! rebuilding a dropped session, which is the same strike. A network failure
//! is not remembered: it costs no strike, and the fallback for it stays
//! silent, as it always was.

use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use synology_filestation_core::error::SynoFsError;

use crate::error::is_refused_login;
use crate::transport::{SmbConfig, SmbTransport};

/// Seconds to stop probing SMB for an account after its login was refused.
pub const AUTH_COOLDOWN_ENV: &str = "SYNOLOGY_FS_SMB_AUTH_COOLDOWN_S";

/// The cool-down when [`AUTH_COOLDOWN_ENV`] does not set one: DSM's own
/// auto-block window. Anything shorter lets one long-lived process spend
/// more than one strike per window, and three is the limit.
pub const DEFAULT_AUTH_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

/// What the probe settled on for a new client.
pub enum Probe {
    /// An authenticated session; prefer it.
    Smb(Arc<SmbTransport>),
    /// HTTP only, and why.
    Http(Fallback),
}

impl Probe {
    /// The transport, if SMB was chosen.
    pub fn transport(&self) -> Option<&Arc<SmbTransport>> {
        match self {
            Probe::Smb(t) => Some(t),
            Probe::Http(_) => None,
        }
    }
}

/// Why a client is on HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback {
    pub kind: FallbackKind,
    /// For a person: what happened, and for a refusal, what to change.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackKind {
    /// `SYNOLOGY_FS_SMB_DISABLE` is set.
    Disabled,
    /// SMB could not be reached, or the session could not be set up for a
    /// reason other than the credentials. Costs no strike.
    Unreachable,
    /// The server refused the login just now.
    AuthRefused,
    /// The server refused this account's login earlier in the cool-down, so
    /// it was not asked again.
    AuthCooldown,
    /// SMB was attached, and its session has since gone; it is rebuilt on
    /// the next operation that can use it.
    Disconnected,
}

impl FallbackKind {
    /// A stable name, for bindings.
    pub fn as_str(self) -> &'static str {
        match self {
            FallbackKind::Disabled => "disabled",
            FallbackKind::Unreachable => "unreachable",
            FallbackKind::AuthRefused => "auth_refused",
            FallbackKind::AuthCooldown => "auth_cooldown",
            FallbackKind::Disconnected => "disconnected",
        }
    }
}

/// The cool-down [`AUTH_COOLDOWN_ENV`] asks for, or [`DEFAULT_AUTH_COOLDOWN`]
/// when it is unset or unreadable. Zero is honoured: it turns the guard off.
pub fn auth_cooldown() -> Duration {
    cooldown_from(std::env::var(AUTH_COOLDOWN_ENV).ok().as_deref())
}

fn cooldown_from(value: Option<&str>) -> Duration {
    match value.map(str::trim).map(str::parse::<u64>) {
        Some(Ok(secs)) => Duration::from_secs(secs),
        Some(Err(_)) => {
            tracing::warn!(
                "{AUTH_COOLDOWN_ENV} is not a whole number of seconds; using {}s",
                DEFAULT_AUTH_COOLDOWN.as_secs()
            );
            DEFAULT_AUTH_COOLDOWN
        }
        None => DEFAULT_AUTH_COOLDOWN,
    }
}

/// Probe SMB for a new client: [`SmbTransport::connect`], behind the
/// process-wide refusal memory.
pub async fn probe_as(host: &str, username: &str, password: &str, domain: Option<&str>) -> Probe {
    if std::env::var_os("SYNOLOGY_FS_SMB_DISABLE").is_some() {
        return Probe::Http(Fallback {
            kind: FallbackKind::Disabled,
            detail: "SMB is disabled (SYNOLOGY_FS_SMB_DISABLE)".into(),
        });
    }
    let cfg = SmbConfig::for_account(host, username, password, domain);
    match AuthGate::global()
        .dial(
            &cfg,
            auth_cooldown(),
            Route::Address,
            is_refused_login,
            || SmbTransport::connect_ungated(&cfg),
        )
        .await
    {
        Ok(transport) => {
            tracing::info!(host, "SMB transport enabled; preferring it over HTTP");
            Probe::Smb(Arc::new(transport))
        }
        Err(Gated::Failed(e)) => Probe::Http(Fallback {
            kind: FallbackKind::Unreachable,
            detail: format!(
                "SMB unavailable at {} as {}: {e}",
                cfg.host,
                principal(&cfg)
            ),
        }),
        Err(Gated::Fallback(fallback)) => Probe::Http(fallback),
    }
}

/// Build a session for `cfg`'s account through the process-wide gate, for a
/// caller that speaks [`SynoFsError`].
///
/// A refusal, now or earlier in the cool-down, comes back as
/// `LoginFailed(PermissionDenied)` — the one shape [`is_refused_login`]
/// recognises — so the caller latches it as final rather than retrying.
pub(crate) async fn gated<T, F, Fut>(
    cfg: &SmbConfig,
    route: Route,
    dial: F,
) -> Result<T, SynoFsError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, SynoFsError>>,
{
    match AuthGate::global()
        .dial(cfg, auth_cooldown(), route, is_refused_login, dial)
        .await
    {
        Ok(t) => Ok(t),
        Err(Gated::Failed(e)) => Err(e),
        Err(Gated::Fallback(f)) => Err(match f.kind {
            FallbackKind::Unreachable => SynoFsError::Io(f.detail),
            _ => SynoFsError::LoginFailed(Box::new(SynoFsError::PermissionDenied)),
        }),
    }
}

/// [`gated`], for a session rebuilt by `smb2` itself: a refusal comes back
/// as `smb2::Error::Auth`, which the reconnect bookkeeping latches.
pub(crate) async fn gated_smb2<T, F, Fut>(
    cfg: &SmbConfig,
    route: Route,
    dial: F,
) -> Result<T, smb2::Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, smb2::Error>>,
{
    match AuthGate::global()
        .dial(
            cfg,
            auth_cooldown(),
            route,
            crate::error::is_login_refusal,
            dial,
        )
        .await
    {
        Ok(t) => Ok(t),
        Err(Gated::Failed(e)) => Err(e),
        Err(Gated::Fallback(f)) => Err(match f.kind {
            // Another transport's dial for this account just found the link
            // down; this one would find the same, so it stays flagged.
            FallbackKind::Unreachable => smb2::Error::Disconnected,
            _ => smb2::Error::Auth { message: f.detail },
        }),
    }
}

/// What a dial runs on, which decides whether a dial queued behind it may
/// take its answer that the link is down.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// The account's own address, dialled here: every such dial goes the same
    /// way, so one that found it down has answered for the rest.
    Address,
    /// A stream the caller opened. Another may come through a different
    /// tunnel, so its failure answers for nobody else.
    Stream,
}

/// How a gated dial failed.
pub(crate) enum Gated<E> {
    /// It was made, and failed for a reason other than the credentials.
    Failed(E),
    /// It was refused, or not made: SMB is not for this account right now.
    Fallback(Fallback),
}

/// The account a refusal is remembered for. Case-folded, because SMB names
/// are case-insensitive and `KRG\Svc` is the same strike as `krg\svc`.
///
/// The password is part of it, as a keyed hash that never leaves the
/// process: a corrected password is a different attempt and is worth one
/// try, where the same wrong one asked again is only another strike.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Account {
    host: String,
    username: String,
    domain: String,
    password: u64,
}

impl Account {
    fn of(cfg: &SmbConfig) -> Self {
        static KEY: OnceLock<RandomState> = OnceLock::new();
        Self {
            host: cfg.host.to_lowercase(),
            username: cfg.username.to_lowercase(),
            domain: cfg.domain.to_lowercase(),
            password: KEY.get_or_init(RandomState::new).hash_one(&cfg.password),
        }
    }
}

/// What the gate knows about one account.
#[derive(Default)]
struct Slot {
    refusal: Option<Refusal>,
    /// How the last dial that held the lock ended, for the dials that queued
    /// behind it.
    last: Option<Outcome>,
}

/// A refused login, and when.
struct Refusal {
    at: Instant,
    why: String,
}

struct Outcome {
    at: Instant,
    route: Route,
    /// `None` when the login was accepted; otherwise why the link could not
    /// be reached.
    unreachable: Option<String>,
}

/// Which accounts' logins were refused, and when.
///
/// Each account has its own async lock, held across a dial whose outcome
/// nobody knows yet. Without it a burst of clients created together would
/// all dial before the first refusal came back, and each would be a strike.
/// The dials that queued behind it take its answer rather than repeating it:
/// a link found down is down for them too, so they do not each wait out the
/// timeout in turn, and credentials just accepted are not a strike, so they
/// dial at once, side by side.
#[derive(Default)]
pub(crate) struct AuthGate {
    accounts: Mutex<HashMap<Account, Arc<tokio::sync::Mutex<Slot>>>>,
}

impl AuthGate {
    fn global() -> &'static AuthGate {
        static GATE: OnceLock<AuthGate> = OnceLock::new();
        GATE.get_or_init(AuthGate::default)
    }

    /// Run `dial` for `cfg`'s account unless a refusal within `cooldown`, or
    /// a dial that has just found the link down, says not to.
    pub(crate) async fn dial<T, E, F, Fut>(
        &self,
        cfg: &SmbConfig,
        cooldown: Duration,
        route: Route,
        refused: fn(&E) -> bool,
        dial: F,
    ) -> Result<T, Gated<E>>
    where
        E: Display,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let asked = Instant::now();
        let slot = self.slot(cfg, cooldown);
        let mut state = slot.lock().await;

        if let Some(refusal) = state.refusal.as_ref() {
            let since = refusal.at.elapsed();
            if since < cooldown {
                tracing::debug!(host = %cfg.host, "SMB: login refused earlier; not asking again");
                return Err(Gated::Fallback(Fallback {
                    kind: FallbackKind::AuthCooldown,
                    detail: format!(
                        "SMB login to {} as {} was refused {}s ago ({}); not trying again \
                         for {}s",
                        cfg.host,
                        principal(cfg),
                        since.as_secs(),
                        refusal.why,
                        (cooldown - since).as_secs()
                    ),
                }));
            }
            state.refusal = None;
        }

        // A dial finished while this one waited for the lock.
        if let Some(last) = state.last.as_ref().filter(|last| last.at >= asked) {
            match &last.unreachable {
                Some(why) if route == Route::Address && last.route == Route::Address => {
                    return Err(Gated::Fallback(Fallback {
                        kind: FallbackKind::Unreachable,
                        detail: why.clone(),
                    }));
                }
                _ => {
                    drop(state);
                    let result = dial().await;
                    if let Err(e) = &result {
                        if refused(e) {
                            // The credentials changed under a burst: rare,
                            // and the reason this is not a hard guarantee.
                            let mut state = slot.lock().await;
                            let fallback = self.refuse(cfg, cooldown, &mut state, e);
                            drop(state);
                            self.release(cfg, slot);
                            return Err(Gated::Fallback(fallback));
                        }
                    }
                    self.release(cfg, slot);
                    return result.map_err(Gated::Failed);
                }
            }
        }

        let result = dial().await;
        let out = match result {
            Ok(t) => {
                state.last = Some(Outcome {
                    at: Instant::now(),
                    route,
                    unreachable: None,
                });
                Ok(t)
            }
            Err(e) if refused(&e) => {
                state.last = None;
                Err(Gated::Fallback(self.refuse(cfg, cooldown, &mut state, &e)))
            }
            Err(e) => {
                tracing::debug!(host = %cfg.host, error = %e, "SMB unavailable; using HTTP only");
                state.last = Some(Outcome {
                    at: Instant::now(),
                    route,
                    unreachable: Some(format!(
                        "SMB unavailable at {} as {}: {e}",
                        cfg.host,
                        principal(cfg)
                    )),
                });
                Err(Gated::Failed(e))
            }
        };
        drop(state);
        self.release(cfg, slot);
        out
    }

    /// Remember a refusal, and say so — once per cool-down per account,
    /// because the lock is what stops the next dial getting this far.
    fn refuse(
        &self,
        cfg: &SmbConfig,
        cooldown: Duration,
        state: &mut Slot,
        e: &dyn Display,
    ) -> Fallback {
        let mut detail = format!(
            "SMB login to {} as {} was refused ({e}). Each refused login counts towards \
             the NAS's auto-block of this address, so SMB is not tried again for this \
             account for {}s; using HTTP",
            cfg.host,
            principal(cfg),
            cooldown.as_secs()
        );
        if cfg.domain.is_empty() {
            detail.push_str(
                ". If this is a domain account — bare username: SMB checks local accounts; \
                 set a domain (DOMAIN\\user or SYNOLOGY_FS_SMB_DOMAIN). If it is a local DSM \
                 account, its password was refused: check it before trying again",
            );
        }
        tracing::warn!("{detail}");
        state.refusal = Some(Refusal {
            at: Instant::now(),
            why: e.to_string(),
        });
        Fallback {
            kind: FallbackKind::AuthRefused,
            detail,
        }
    }

    /// The account's slot, sweeping out the ones that have nothing left to
    /// remember: a long-lived process that logs in as many users would
    /// otherwise keep one per account it ever saw.
    fn slot(&self, cfg: &SmbConfig, cooldown: Duration) -> Arc<tokio::sync::Mutex<Slot>> {
        let mut accounts = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
        accounts.retain(|_, slot| !forgettable(slot, 1, Some(cooldown)));
        accounts.entry(Account::of(cfg)).or_default().clone()
    }

    /// Let go of a slot, dropping it if nobody else holds it and it records
    /// no refusal.
    fn release(&self, cfg: &SmbConfig, slot: Arc<tokio::sync::Mutex<Slot>>) {
        let mut accounts = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
        let key = Account::of(cfg);
        // Two references: the map's and ours. New ones are only handed out
        // under the map lock, which is held here, so none can appear.
        let ours = accounts.get(&key).is_some_and(|s| Arc::ptr_eq(s, &slot));
        if ours && forgettable(&slot, 2, None) {
            accounts.remove(&key);
        }
    }

    #[cfg(test)]
    fn remembered(&self) -> usize {
        self.accounts.lock().unwrap().len()
    }
}

/// Whether a slot can go: nobody but its `holders` has it, and it has no
/// refusal to remember — any refusal, or with a `cooldown`, one younger than it.
fn forgettable(
    slot: &Arc<tokio::sync::Mutex<Slot>>,
    holders: usize,
    cooldown: Option<Duration>,
) -> bool {
    if Arc::strong_count(slot) > holders {
        return false;
    }
    match slot.try_lock() {
        Ok(state) => match &state.refusal {
            None => true,
            Some(r) => cooldown.is_some_and(|cd| r.at.elapsed() >= cd),
        },
        Err(_) => false,
    }
}

/// `DOMAIN\user`, or the bare name when there is no domain.
fn principal(cfg: &SmbConfig) -> String {
    if cfg.domain.is_empty() {
        cfg.username.clone()
    } else {
        format!("{}\\{}", cfg.domain, cfg.username)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cfg(user: &str, domain: &str) -> SmbConfig {
        let mut cfg = SmbConfig::new("nas.example", user, "pw");
        cfg.domain = domain.into();
        cfg
    }

    const HOUR: Duration = Duration::from_secs(3600);

    fn refusal() -> SynoFsError {
        SynoFsError::LoginFailed(Box::new(SynoFsError::PermissionDenied))
    }

    /// A dial that counts itself and fails with `err`.
    fn failing(
        dials: &AtomicUsize,
        err: SynoFsError,
    ) -> impl Future<Output = Result<(), SynoFsError>> + '_ {
        dials.fetch_add(1, Ordering::SeqCst);
        async move { Err(err) }
    }

    async fn try_dial(
        gate: &AuthGate,
        cfg: &SmbConfig,
        cooldown: Duration,
        dials: &AtomicUsize,
        err: SynoFsError,
    ) -> Fallback {
        match gate
            .dial(cfg, cooldown, Route::Address, is_refused_login, || {
                failing(dials, err)
            })
            .await
        {
            Ok(()) => panic!("the dial fails"),
            Err(Gated::Fallback(f)) => f,
            Err(Gated::Failed(e)) => Fallback {
                kind: FallbackKind::Unreachable,
                detail: e.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn a_refused_login_is_not_tried_again_within_the_cooldown() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        let account = cfg("svc_fishsense", "");

        let first = try_dial(&gate, &account, HOUR, &dials, refusal()).await;
        assert_eq!(first.kind, FallbackKind::AuthRefused);
        for _ in 0..5 {
            let again = try_dial(&gate, &account, HOUR, &dials, refusal()).await;
            assert_eq!(again.kind, FallbackKind::AuthCooldown);
        }
        assert_eq!(dials.load(Ordering::SeqCst), 1, "one strike, not six");
    }

    /// The signing and access-denied family maps to `PermissionDenied` too,
    /// but it is the server's configuration, not the credentials: no strike,
    /// nothing to remember, and no advice about domains.
    #[tokio::test]
    async fn a_permission_error_that_is_not_a_refused_login_is_not_remembered() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        for _ in 0..2 {
            let f = try_dial(
                &gate,
                &cfg("u", ""),
                HOUR,
                &dials,
                SynoFsError::PermissionDenied,
            )
            .await;
            assert_eq!(f.kind, FallbackKind::Unreachable);
        }
        assert_eq!(dials.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_burst_of_clients_spends_one_strike() {
        // A service that creates a client per task starts many at once; the
        // first refusal has not come back when the rest would dial.
        let gate = Arc::new(AuthGate::default());
        let dials = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let (gate, dials) = (gate.clone(), dials.clone());
                tokio::spawn(async move {
                    gate.dial(
                        &cfg("svc", ""),
                        HOUR,
                        Route::Address,
                        is_refused_login,
                        || async {
                            dials.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            Err::<(), _>(refusal())
                        },
                    )
                    .await
                    .is_err()
                })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap());
        }
        assert_eq!(dials.load(Ordering::SeqCst), 1);
    }

    /// Regression: the lock held across every dial made a burst wait out the
    /// probe timeout once per client, off campus where port 445 is dropped.
    #[tokio::test]
    async fn a_burst_shares_the_answer_that_the_link_is_down() {
        let gate = Arc::new(AuthGate::default());
        let dials = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let (gate, dials) = (gate.clone(), dials.clone());
                tokio::spawn(async move {
                    gate.dial(
                        &cfg("u", "KRG"),
                        HOUR,
                        Route::Address,
                        is_refused_login,
                        || async {
                            dials.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Err::<(), _>(SynoFsError::Io("smb: timed out".into()))
                        },
                    )
                    .await
                })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap().is_err());
        }
        assert_eq!(dials.load(Ordering::SeqCst), 1, "one timeout, shared");
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "{:?}",
            started.elapsed()
        );
    }

    /// But two streams handed over are two routes: one that failed says
    /// nothing about the next, which may come through a different tunnel.
    #[tokio::test]
    async fn a_failed_stream_is_not_taken_as_the_answer_for_another() {
        let gate = Arc::new(AuthGate::default());
        let dials = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (gate, dials) = (gate.clone(), dials.clone());
                tokio::spawn(async move {
                    gate.dial(
                        &cfg("u", "KRG"),
                        HOUR,
                        Route::Stream,
                        is_refused_login,
                        || async {
                            dials.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            Err::<(), _>(SynoFsError::Io("smb: stream closed".into()))
                        },
                    )
                    .await
                })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap().is_err());
        }
        assert_eq!(dials.load(Ordering::SeqCst), 4);
    }

    /// And once the leader's login is accepted, the rest dial side by side
    /// rather than one handshake after another.
    #[tokio::test]
    async fn a_burst_after_an_accepted_login_dials_in_parallel() {
        let gate = Arc::new(AuthGate::default());
        let dials = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let (gate, dials) = (gate.clone(), dials.clone());
                tokio::spawn(async move {
                    gate.dial(
                        &cfg("u", "KRG"),
                        HOUR,
                        Route::Address,
                        is_refused_login,
                        || async {
                            dials.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Ok::<(), SynoFsError>(())
                        },
                    )
                    .await
                })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap().is_ok());
        }
        assert_eq!(
            dials.load(Ordering::SeqCst),
            16,
            "every client gets a session"
        );
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "two handshakes deep, not sixteen: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_network_failure_is_not_remembered() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        for _ in 0..3 {
            let f = try_dial(
                &gate,
                &cfg("u", "KRG"),
                HOUR,
                &dials,
                SynoFsError::Io("smb: timed out".into()),
            )
            .await;
            assert_eq!(f.kind, FallbackKind::Unreachable);
        }
        assert_eq!(
            dials.load(Ordering::SeqCst),
            3,
            "no strike, so no reason to wait"
        );
    }

    #[tokio::test]
    async fn the_refusal_belongs_to_one_account() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        try_dial(&gate, &cfg("svc", ""), HOUR, &dials, refusal()).await;

        // The fix — the same user, qualified — is a different account, and is
        // tried at once.
        let ok = gate
            .dial(
                &cfg("svc", "KRG"),
                HOUR,
                Route::Address,
                is_refused_login,
                || async { Ok::<(), SynoFsError>(()) },
            )
            .await;
        assert!(ok.is_ok());
        // Case is not a different account.
        let again = try_dial(&gate, &cfg("SVC", ""), HOUR, &dials, refusal()).await;
        assert_eq!(again.kind, FallbackKind::AuthCooldown);
        assert_eq!(dials.load(Ordering::SeqCst), 1);
    }

    /// Regression: keyed without the password, correcting it in a running
    /// process still kept SMB off for the rest of the day.
    #[tokio::test]
    async fn a_corrected_password_is_tried() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        try_dial(&gate, &cfg("svc", "KRG"), HOUR, &dials, refusal()).await;

        let mut fixed = cfg("svc", "KRG");
        fixed.password = "the right one".into();
        let ok = gate
            .dial(&fixed, HOUR, Route::Address, is_refused_login, || async {
                Ok::<(), SynoFsError>(())
            })
            .await;
        assert!(ok.is_ok());
    }

    #[tokio::test]
    async fn the_account_is_tried_again_once_the_cooldown_has_passed() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        let short = Duration::from_millis(30);
        try_dial(&gate, &cfg("u", ""), short, &dials, refusal()).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let ok = gate
            .dial(
                &cfg("u", ""),
                short,
                Route::Address,
                is_refused_login,
                || async { Ok::<(), SynoFsError>(()) },
            )
            .await;
        assert!(ok.is_ok());
    }

    /// Regression: an entry per account ever probed, kept for the life of
    /// the process.
    #[tokio::test]
    async fn only_live_refusals_are_kept() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        for user in ["a", "b", "c"] {
            gate.dial(
                &cfg(user, "KRG"),
                HOUR,
                Route::Address,
                is_refused_login,
                || async { Ok::<(), SynoFsError>(()) },
            )
            .await
            .ok();
            try_dial(
                &gate,
                &cfg(user, "X"),
                HOUR,
                &dials,
                SynoFsError::Io("down".into()),
            )
            .await;
        }
        assert_eq!(gate.remembered(), 0, "nothing to remember about these");

        let short = Duration::from_millis(30);
        try_dial(&gate, &cfg("refused", ""), short, &dials, refusal()).await;
        assert_eq!(gate.remembered(), 1, "a refusal is kept");
        tokio::time::sleep(Duration::from_millis(60)).await;
        try_dial(
            &gate,
            &cfg("other", "KRG"),
            short,
            &dials,
            SynoFsError::Io("down".into()),
        )
        .await;
        assert_eq!(gate.remembered(), 0, "and swept once it has expired");
    }

    #[tokio::test]
    async fn a_refused_bare_username_is_called_out_without_misleading_a_local_account() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        let bare = try_dial(&gate, &cfg("svc_fishsense", ""), HOUR, &dials, refusal()).await;
        assert!(
            bare.detail.contains(
                "bare username: SMB checks local accounts; set a domain \
                 (DOMAIN\\user or SYNOLOGY_FS_SMB_DOMAIN)"
            ),
            "{}",
            bare.detail
        );
        // An empty domain is also how a local DSM account is named, and for
        // one the domain advice is wrong — following it spends another strike.
        assert!(
            bare.detail.contains("If this is a domain account"),
            "{}",
            bare.detail
        );
        assert!(bare.detail.contains("local DSM account"), "{}", bare.detail);

        let qualified =
            try_dial(&gate, &cfg("svc_fishsense", "KRG"), HOUR, &dials, refusal()).await;
        assert!(
            !qualified.detail.contains("bare username"),
            "{}",
            qualified.detail
        );
        assert!(
            qualified.detail.contains("KRG\\svc_fishsense"),
            "{}",
            qualified.detail
        );
    }

    /// Every way this crate authenticates goes through the same gate: a
    /// refusal seen by a rebuilding transport stops a new client's probe, and
    /// the other way round.
    #[tokio::test]
    async fn the_probe_and_a_rebuilt_session_share_the_refusal() {
        let mut account = cfg("shared", "");
        account.host = "shared-gate.example".into();

        let refused = gated_smb2(&account, Route::Address, || async {
            Err::<(), _>(smb2::Error::Auth {
                message: "STATUS_LOGON_FAILURE".into(),
            })
        })
        .await
        .unwrap_err();
        assert!(crate::error::is_login_refusal(&refused));

        let skipped = gated(&account, Route::Stream, || async {
            panic!("dialled an account refused moments ago");
            #[allow(unreachable_code)]
            Ok::<(), SynoFsError>(())
        })
        .await
        .unwrap_err();
        assert!(is_refused_login(&skipped), "{skipped:?}");
    }

    #[test]
    fn the_cooldown_defaults_to_hours_and_can_be_set() {
        assert_eq!(cooldown_from(None), DEFAULT_AUTH_COOLDOWN);
        assert!(DEFAULT_AUTH_COOLDOWN >= Duration::from_secs(3600));
        assert_eq!(cooldown_from(Some("7200")), Duration::from_secs(7200));
        assert_eq!(cooldown_from(Some("soon")), DEFAULT_AUTH_COOLDOWN);
    }
}
