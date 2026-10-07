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
//! account, and a probe for that account in the meantime goes to HTTP without
//! dialling. A network failure is not remembered: it costs no strike, and the
//! fallback for it stays silent, as it always was.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use synology_filestation_core::error::{ErrorCategory, SynoFsError};

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
}

impl FallbackKind {
    /// A stable name, for bindings.
    pub fn as_str(self) -> &'static str {
        match self {
            FallbackKind::Disabled => "disabled",
            FallbackKind::Unreachable => "unreachable",
            FallbackKind::AuthRefused => "auth_refused",
            FallbackKind::AuthCooldown => "auth_cooldown",
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
        .dial(&cfg, auth_cooldown(), || SmbTransport::connect(&cfg))
        .await
    {
        Ok(transport) => {
            tracing::info!(host, "SMB transport enabled; preferring it over HTTP");
            Probe::Smb(Arc::new(transport))
        }
        Err(fallback) => Probe::Http(fallback),
    }
}

/// The account a refusal is remembered for. Case-folded, because SMB names
/// are case-insensitive and `KRG\Svc` is the same strike as `krg\svc`.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Account {
    host: String,
    username: String,
    domain: String,
}

impl Account {
    fn of(cfg: &SmbConfig) -> Self {
        Self {
            host: cfg.host.to_lowercase(),
            username: cfg.username.to_lowercase(),
            domain: cfg.domain.to_lowercase(),
        }
    }
}

/// A refused login, and when.
struct Refusal {
    at: Instant,
    why: String,
}

/// Which accounts' logins were refused, and when.
///
/// Each account has its own async lock, held across the dial. Without it a
/// burst of clients created together would all dial before the first refusal
/// came back, and each would be a strike.
#[derive(Default)]
pub(crate) struct AuthGate {
    accounts: Mutex<HashMap<Account, Arc<tokio::sync::Mutex<Option<Refusal>>>>>,
}

impl AuthGate {
    fn global() -> &'static AuthGate {
        static GATE: OnceLock<AuthGate> = OnceLock::new();
        GATE.get_or_init(AuthGate::default)
    }

    /// Run `dial` for `cfg`'s account unless a refusal within `cooldown`
    /// says not to.
    pub(crate) async fn dial<T, F, Fut>(
        &self,
        cfg: &SmbConfig,
        cooldown: Duration,
        dial: F,
    ) -> Result<T, Fallback>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, SynoFsError>>,
    {
        let slot = {
            let mut accounts = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
            accounts.entry(Account::of(cfg)).or_default().clone()
        };
        let mut refused = slot.lock().await;

        if let Some(refusal) = refused.as_ref() {
            let since = refusal.at.elapsed();
            if since < cooldown {
                let left = cooldown - since;
                tracing::debug!(host = %cfg.host, "SMB: login refused earlier; not asking again");
                return Err(Fallback {
                    kind: FallbackKind::AuthCooldown,
                    detail: format!(
                        "SMB login to {} as {} was refused {}s ago ({}); not trying again \
                         for {}s",
                        cfg.host,
                        principal(cfg),
                        since.as_secs(),
                        refusal.why,
                        left.as_secs()
                    ),
                });
            }
            *refused = None;
        }

        match dial().await {
            Ok(t) => Ok(t),
            Err(e) if is_refusal(&e) => {
                let mut detail = format!(
                    "SMB login to {} as {} was refused ({e}); using HTTP, and not trying SMB \
                     for this account again for {}s",
                    cfg.host,
                    principal(cfg),
                    cooldown.as_secs()
                );
                if cfg.domain.is_empty() {
                    detail.push_str(
                        ". Likely cause — bare username: SMB checks local accounts; set a \
                         domain (DOMAIN\\user or SYNOLOGY_FS_SMB_DOMAIN)",
                    );
                }
                // Once per cool-down per account: the lock above is what stops
                // the next client getting this far.
                tracing::warn!("{detail}");
                *refused = Some(Refusal {
                    at: Instant::now(),
                    why: e.to_string(),
                });
                Err(Fallback {
                    kind: FallbackKind::AuthRefused,
                    detail,
                })
            }
            Err(e) => {
                tracing::debug!(host = %cfg.host, error = %e, "SMB unavailable; using HTTP only");
                Err(Fallback {
                    kind: FallbackKind::Unreachable,
                    detail: format!("SMB unavailable at {} as {}: {e}", cfg.host, principal(cfg)),
                })
            }
        }
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

fn is_refusal(e: &SynoFsError) -> bool {
    e.category() == ErrorCategory::PermissionDenied
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

    /// A dial that counts itself and fails with `err`.
    fn failing(
        dials: &AtomicUsize,
        err: SynoFsError,
    ) -> impl Future<Output = Result<(), SynoFsError>> + '_ {
        dials.fetch_add(1, Ordering::SeqCst);
        async move { Err(err) }
    }

    #[tokio::test]
    async fn a_refused_login_is_not_tried_again_within_the_cooldown() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        let account = cfg("svc_fishsense", "");

        let first = gate
            .dial(&account, HOUR, || {
                failing(&dials, SynoFsError::PermissionDenied)
            })
            .await
            .unwrap_err();
        assert_eq!(first.kind, FallbackKind::AuthRefused);

        for _ in 0..5 {
            let again = gate
                .dial(&account, HOUR, || {
                    failing(&dials, SynoFsError::PermissionDenied)
                })
                .await
                .unwrap_err();
            assert_eq!(again.kind, FallbackKind::AuthCooldown);
        }
        assert_eq!(dials.load(Ordering::SeqCst), 1, "one strike, not six");
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
                    gate.dial(&cfg("svc", ""), HOUR, || async {
                        dials.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Err::<(), _>(SynoFsError::PermissionDenied)
                    })
                    .await
                })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap().is_err());
        }
        assert_eq!(dials.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_network_failure_is_not_remembered() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        for _ in 0..3 {
            let err = gate
                .dial(&cfg("u", "KRG"), HOUR, || {
                    failing(&dials, SynoFsError::Io("smb: timed out".into()))
                })
                .await
                .unwrap_err();
            assert_eq!(err.kind, FallbackKind::Unreachable);
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
        gate.dial(&cfg("svc", ""), HOUR, || {
            failing(&dials, SynoFsError::PermissionDenied)
        })
        .await
        .unwrap_err();

        // The fix — the same user, qualified — is a different account, and is
        // tried at once.
        let ok = gate
            .dial(&cfg("svc", "KRG"), HOUR, || async { Ok(()) })
            .await;
        assert!(ok.is_ok());
        // Case is not a different account.
        let again = gate
            .dial(&cfg("SVC", ""), HOUR, || {
                failing(&dials, SynoFsError::PermissionDenied)
            })
            .await
            .unwrap_err();
        assert_eq!(again.kind, FallbackKind::AuthCooldown);
        assert_eq!(dials.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_account_is_tried_again_once_the_cooldown_has_passed() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        let short = Duration::from_millis(30);
        gate.dial(&cfg("u", ""), short, || {
            failing(&dials, SynoFsError::PermissionDenied)
        })
        .await
        .unwrap_err();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let ok = gate.dial(&cfg("u", ""), short, || async { Ok(()) }).await;
        assert!(ok.is_ok());
    }

    #[tokio::test]
    async fn a_refused_bare_username_is_called_out() {
        let gate = AuthGate::default();
        let dials = AtomicUsize::new(0);
        let bare = gate
            .dial(&cfg("svc_fishsense", ""), HOUR, || {
                failing(&dials, SynoFsError::PermissionDenied)
            })
            .await
            .unwrap_err();
        assert!(
            bare.detail.contains(
                "bare username: SMB checks local accounts; set a domain \
                 (DOMAIN\\user or SYNOLOGY_FS_SMB_DOMAIN)"
            ),
            "{}",
            bare.detail
        );

        let qualified = gate
            .dial(&cfg("svc_fishsense", "KRG"), HOUR, || {
                failing(&dials, SynoFsError::PermissionDenied)
            })
            .await
            .unwrap_err();
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

    #[test]
    fn the_cooldown_defaults_to_hours_and_can_be_set() {
        assert_eq!(cooldown_from(None), DEFAULT_AUTH_COOLDOWN);
        assert!(DEFAULT_AUTH_COOLDOWN >= Duration::from_secs(3600));
        assert_eq!(cooldown_from(Some("7200")), Duration::from_secs(7200));
        assert_eq!(cooldown_from(Some("soon")), DEFAULT_AUTH_COOLDOWN);
    }
}
