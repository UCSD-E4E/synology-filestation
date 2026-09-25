//! Which way to reach the NAS, and when to look again.
//!
//! Three legs, best first: SMB straight to the appliance, SMB through a
//! tunnel, and the HTTP FileStation API. They are not equivalent — SMB
//! addresses byte ranges, so a write that dies at 540 MiB resumes at 540 MiB,
//! while the HTTP upload has no resume and loses the file. The chain exists so
//! a user on a network that blocks port 445 still gets their file across,
//! without everyone else paying for that.
//!
//! ## The shape of the decision
//!
//! A tunnel is not a peer of the other two: it is a *precondition* that can
//! make SMB reachable. So `--disable-vpn` does not remove a transport, it
//! forbids the escalation, and a user on a locked-down network falls straight
//! from SMB to HTTP with no tunnel dialog.
//!
//! ## Why the choice is cached
//!
//! Every operation asks which transport to use, and probing per operation
//! would put a TCP connect in front of every `stat`. So the answer is decided
//! once and cached, and the cache expires **only while degraded**: sitting on
//! the best leg there is nothing better to find, so nothing is probed and
//! steady state costs nothing. Demotion does not wait for the cache — a
//! backend that starts failing trips its own circuit breaker in
//! `synology-filestation-core`, which is what makes a dead link cheap instead
//! of a ten-second timeout per call.

pub mod openvpn;
pub mod profile;

pub use openvpn::OpenVpnTunnel;

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// How long a degraded choice is trusted before the chain looks for a better
/// one. Steady state on the best leg never re-probes, so this is not a poll
/// interval so much as "how stale may a workaround get".
pub const DEFAULT_RECHECK: Duration = Duration::from_secs(60);

/// How long a tunnel that failed is left alone before it is raised again.
///
/// Separate from [`DEFAULT_RECHECK`] because the two legs cost different
/// things to ask about. The direct probe is one bounded TCP connect; raising
/// the tunnel is a full OpenVPN handshake and a real authentication against
/// the directory. Asking about the first every minute is cheap; asking about
/// the second that often, against a tunnel that comes up with a NAS silent
/// inside it, is a handshake a minute for the life of the mount.
///
/// A cost of *failing*: a tunnel whose stream merely ended is raised again at
/// once. A refusal is not governed by this at all — it is never retried on a
/// timer.
pub const DEFAULT_TUNNEL_RECHECK: Duration = Duration::from_secs(10 * 60);

/// What is carrying the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Transport {
    /// The HTTP FileStation API. Works from anywhere the DSM port is open,
    /// and cannot resume an interrupted transfer.
    Https,
    /// SMB through a tunnel we brought up.
    SmbOverVpn,
    /// SMB straight to the appliance.
    SmbDirect,
}

impl Transport {
    /// Whether this is the best the chain can do, and therefore whether
    /// looking again could find anything.
    fn is_best(self) -> bool {
        self == Transport::SmbDirect
    }
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Transport::SmbDirect => "SMB",
            Transport::SmbOverVpn => "SMB via VPN",
            Transport::Https => "HTTPS",
        };
        f.write_str(name)
    }
}

/// Which legs the user has allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportPolicy {
    smb: bool,
    vpn: bool,
    https: bool,
}

/// A policy that forbids everything, which is a configuration mistake rather
/// than a mount that quietly does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NothingEnabled;

impl std::fmt::Display for NothingEnabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("every transport is disabled, so there is no way to reach the NAS")
    }
}

impl std::error::Error for NothingEnabled {}

impl Default for TransportPolicy {
    fn default() -> Self {
        Self {
            smb: true,
            vpn: true,
            https: true,
        }
    }
}

impl TransportPolicy {
    /// Build a policy from the three `--disable-*` flags.
    ///
    /// Disabling means *do not probe*, not *probe and ignore*: on a network
    /// that black-holes port 445, `--disable-smb` also saves the connect
    /// timeout. It implies no escalation either, since there would be nothing
    /// to escalate to.
    pub fn from_flags(
        disable_smb: bool,
        disable_vpn: bool,
        disable_https: bool,
    ) -> Result<Self, NothingEnabled> {
        if disable_smb && disable_https {
            return Err(NothingEnabled);
        }
        Ok(Self {
            smb: !disable_smb,
            vpn: !disable_smb && !disable_vpn,
            https: !disable_https,
        })
    }

    /// Whether SMB may be probed at all.
    pub fn allows_smb(&self) -> bool {
        self.smb
    }

    /// Whether a tunnel may be raised to reach SMB.
    pub fn allows_vpn(&self) -> bool {
        self.vpn
    }

    /// Whether the HTTP API may carry the data.
    pub fn allows_https(&self) -> bool {
        self.https
    }
}

/// Where the NAS is, on each side of the tunnel.
///
/// Two addresses, not one, because the tunnel deliberately pushes no DNS: a
/// client inside it reaches the NAS at a private address that its public name
/// does not resolve to. Probing the wrong one is the difference between "SMB
/// is down" and "SMB is one escalation away".
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// The publicly resolvable name. Carries the HTTP API, and is what the
    /// direct SMB probe asks.
    pub public_host: String,
    /// The NAS's address inside the tunnel, when one is configured. `None`
    /// means there is nowhere for an escalation to land, so none is attempted
    /// however healthy the tunnel is.
    pub tunnel_host: Option<String>,
}

impl Endpoints {
    /// A NAS reachable only by its public name.
    pub fn public_only(host: impl Into<String>) -> Self {
        Self {
            public_host: host.into(),
            tunnel_host: None,
        }
    }

    /// A NAS that is also reachable at `tunnel_host` once the tunnel is up.
    pub fn with_tunnel(host: impl Into<String>, tunnel_host: impl Into<String>) -> Self {
        Self {
            public_host: host.into(),
            tunnel_host: Some(tunnel_host.into()),
        }
    }
}

/// A chosen transport and the address to use it against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Which leg of the chain this is.
    pub transport: Transport,
    /// The host SMB is reached at on this leg.
    ///
    /// **Not always dialable.** On [`Transport::SmbOverVpn`] this is an
    /// address inside a tunnel that terminates in this process, which no
    /// socket on this machine has a route to — dialling it reaches the
    /// operating system's idea of the network, which has never heard of it.
    /// Use [`Chain::reach_smb`], which hands back the connection it already
    /// had to open to know the leg works. `None` on the HTTP leg, which
    /// carries no SMB at all.
    pub smb_host: Option<String>,
}

/// Can SMB be reached at `host` right now?
#[async_trait]
pub trait Prober: Send + Sync {
    /// True when the SMB port answers. The host is a parameter because the
    /// answer differs per leg: the public name for a direct connection, the
    /// in-tunnel address once a tunnel is up. Implementations must bound their
    /// own wait: this runs while a user watches a spinner.
    async fn smb_reachable(&self, host: &str) -> bool;
}

/// A byte stream to the NAS, however it was arrived at.
///
/// Boxed and named only by what it does, so this crate stays a decision layer:
/// what actually satisfies it is a TCP connection inside a userspace stack
/// inside an OpenVPN tunnel, and nothing here needs to know that.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// An open connection to the NAS through a tunnel.
pub type Connection = Box<dyn AsyncStream>;

/// Something that can make an unreachable NAS reachable.
#[async_trait]
pub trait Tunnel: Send + Sync {
    /// Bring the tunnel up and open a connection to `host:port` through it.
    ///
    /// Opening it **is** the probe. There is no separate "is it reachable"
    /// question to ask first: a tunnel that terminates inside this process has
    /// no address the operating system can be asked about, so the only way to
    /// find out whether the NAS answers is to talk to it — and having done
    /// that, throwing the connection away to open another one would be a
    /// second handshake to learn what the first already proved.
    ///
    /// Implementations must bound their own wait, as [`Prober`] must: this
    /// runs while a user watches a spinner, and the HTTP leg is one branch
    /// below. A tunnel that hangs holds up the answer for everybody, and the
    /// chain cannot bound it from outside without inventing a deadline that
    /// belongs to whoever knows what the tunnel is doing.
    ///
    /// Must not prompt: this can run at a cache expiry, and a dialog in the
    /// middle of someone's copy is worse than staying on the slower leg. A
    /// tunnel needing input reports failure and waits to be asked explicitly.
    async fn open(&self, host: &str, port: u16) -> Result<Connection, TunnelUnavailable>;
}

/// A tunnel that could not be established, with a reason worth logging.
///
/// Two kinds, because the chain re-decides on a timer and the two want
/// opposite things from it.
#[derive(Debug, Clone)]
pub enum TunnelUnavailable {
    /// Could be different next time: no route, a server not answering, a
    /// profile that has not been fetched yet.
    Transient(String),
    /// Will not be different next time, and asking again costs more than it
    /// can gain.
    ///
    /// A rejected password is the one that matters. Every attempt is a real
    /// authentication against the domain controller, so a chain that re-probes
    /// every minute with a password the user mistyped locks their account
    /// inside an hour — for a mount they have already given up on.
    Refused(String),
}

impl TunnelUnavailable {
    fn reason(&self) -> &str {
        match self {
            Self::Transient(why) | Self::Refused(why) => why,
        }
    }
}

impl std::fmt::Display for TunnelUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for TunnelUnavailable {}

/// The tunnel used when the caller has none: it never comes up, so the chain
/// falls from SMB straight to HTTP.
pub struct NoTunnel;

#[async_trait]
impl Tunnel for NoTunnel {
    async fn open(&self, _host: &str, _port: u16) -> Result<Connection, TunnelUnavailable> {
        Err(TunnelUnavailable::Transient(
            "no tunnel is configured".into(),
        ))
    }
}

/// A TCP connect to the SMB port, which is all "reachable" needs to mean here:
/// authentication is settled separately, against the HTTP API, before any of
/// this runs.
pub struct TcpProber {
    port: u16,
    timeout: Duration,
}

impl TcpProber {
    /// Probe the standard SMB port, giving up after `timeout`.
    ///
    /// A TCP connect and nothing more: reachability, not authorization. ICMP
    /// is not used — the tunnel drops it by design, so a ping would report a
    /// working path as dead.
    pub fn new(timeout: Duration) -> Self {
        Self { port: 445, timeout }
    }
}

#[async_trait]
impl Prober for TcpProber {
    async fn smb_reachable(&self, host: &str) -> bool {
        let addr = format!("{}:{}", host, self.port);
        match tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(&addr)).await {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                debug!("smb probe: {addr} refused the connection ({e})");
                false
            }
            Err(_) => {
                debug!("smb probe: {addr} did not answer within {:?}", self.timeout);
                false
            }
        }
    }
}

/// Nothing in the policy can carry data right now.
#[derive(Debug, Clone)]
pub struct NoTransport;

impl std::fmt::Display for NoTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SMB is unreachable and the HTTP fallback is disabled")
    }
}

impl std::error::Error for NoTransport {}

/// A cached choice and the moment it stops being trusted.
#[derive(Debug, Clone)]
struct Decision {
    route: Route,
    /// `None` for a choice at the top of the chain: there is nothing better to
    /// find, so it never expires on its own.
    expires_at: Option<Instant>,
}

/// The SMB port, which is not configurable: it is what SMB is.
const SMB_PORT: u16 = 445;

/// How SMB can be reached right now.
///
/// Two of these are addresses and one is an open connection, because that is
/// the honest shape of the answer. A NAS reachable directly is dialled by
/// whoever wants it; one reachable only through a tunnel was *already* dialled
/// to find that out, and handing back the connection is what stops the same
/// work being done twice.
pub enum SmbRoute {
    /// It answers directly. Dial this host.
    Direct { host: String },
    /// It answers through a tunnel, and this is the connection to it.
    Tunnelled {
        /// The address inside the tunnel, which still names the server on the
        /// wire even though it has already been dialled.
        host: String,
        connection: Connection,
    },
    /// SMB cannot be reached; the chain has fallen to the HTTP API.
    Unavailable,
}

/// Picks a transport, remembers it, and looks again when the answer could have
/// improved.
pub struct Chain {
    policy: TransportPolicy,
    endpoints: Endpoints,
    prober: Box<dyn Prober>,
    tunnel: Box<dyn Tunnel>,
    recheck: Duration,
    decision: Mutex<Option<Decision>>,
    /// Why the tunnel refused, once it has.
    ///
    /// Kept apart from the decision because it outlives one: a decision
    /// expires so the chain can look for something better, and this exists
    /// precisely so it stops looking. Cleared only by
    /// [`reconsider`](Chain::reconsider), which is what a caller says when the
    /// thing that was refused has changed.
    refused: Mutex<Option<String>>,
    /// How long a failed tunnel is left alone; see [`DEFAULT_TUNNEL_RECHECK`].
    tunnel_recheck: Duration,
    /// When the tunnel last failed to open, while that is still the most
    /// recent thing it did. Cleared by an open that works, and by
    /// [`reconsider`](Chain::reconsider).
    tunnel_failed_at: Mutex<Option<Instant>>,
}

impl Chain {
    /// Build a chain. `recheck` bounds how stale a *degraded* choice may get;
    /// see [`DEFAULT_RECHECK`].
    pub fn new(
        policy: TransportPolicy,
        endpoints: Endpoints,
        prober: Box<dyn Prober>,
        tunnel: Box<dyn Tunnel>,
        recheck: Duration,
    ) -> Self {
        Self {
            policy,
            endpoints,
            prober,
            tunnel,
            recheck,
            decision: Mutex::new(None),
            refused: Mutex::new(None),
            tunnel_recheck: DEFAULT_TUNNEL_RECHECK,
            tunnel_failed_at: Mutex::new(None),
        }
    }

    /// Leave a failed tunnel alone for `interval` rather than
    /// [`DEFAULT_TUNNEL_RECHECK`].
    pub fn with_tunnel_recheck(mut self, interval: Duration) -> Self {
        self.tunnel_recheck = interval;
        self
    }

    /// Whether SMB may be used at all. When it may not, there is nothing for
    /// a caller to attach and nothing to look for later.
    pub fn may_use_smb(&self) -> bool {
        self.policy.allows_smb()
    }

    /// A leg strictly better than `current` that answers now, if there is one.
    ///
    /// For a caller already carrying data on `current` and asking, on a timer,
    /// whether it could do better. So it differs from
    /// [`reach_smb`](Self::reach_smb) in what it will spend:
    ///
    /// * On the best leg it does nothing at all, which keeps the promise that
    ///   a mount on direct SMB causes no traffic of its own.
    /// * From the tunnel only the direct leg is better, so only the direct
    ///   probe is made — a second tunnel to ask would be a handshake for
    ///   nothing.
    /// * A tunnel is raised only if it has not refused, and not failed within
    ///   its own interval.
    ///
    /// It decides nothing. A leg that answers a probe is not yet a leg anyone
    /// is on — the session on it can still be refused — so the caller says
    /// which one it settled on with [`settle`](Self::settle).
    pub async fn better_than(&self, current: Transport) -> Option<SmbRoute> {
        if current.is_best() || !self.policy.allows_smb() {
            return None;
        }
        let public = &self.endpoints.public_host;
        if self.prober.smb_reachable(public).await {
            return Some(SmbRoute::Direct {
                host: public.clone(),
            });
        }
        if current >= Transport::SmbOverVpn {
            return None;
        }
        let inside = self.endpoints.tunnel_host.as_ref()?;
        let connection = self.raise_tunnel(inside).await?;
        Some(SmbRoute::Tunnelled {
            host: inside.clone(),
            connection,
        })
    }

    /// Record that `route` is the leg now in use.
    ///
    /// For a caller that found a better leg with
    /// [`better_than`](Self::better_than) and got a session on it: from here
    /// on the chain answers as though it had decided this itself, including
    /// never re-probing it if it is the best there is.
    pub fn settle(&self, route: Route) {
        self.remember(route, Instant::now());
    }

    /// Record that a connection through the tunnel opened, but nothing could
    /// be built on it.
    ///
    /// The tunnel opening proves the tunnel; it does not prove the session a
    /// caller builds on top. Without this a tunnel that came up and carried
    /// nothing looked like one that worked, so the interval a failure earns
    /// never applied — and a caller looking for a better leg every minute
    /// raised a fresh tunnel, handshake and directory login included, every
    /// minute. A refused login is not this: the caller latches that itself.
    pub fn tunnel_carried_nothing(&self, why: &str) {
        self.tunnel_failed(&TunnelUnavailable::Transient(format!(
            "it opened, but carried no session: {why}"
        )));
    }

    /// Open a connection through the tunnel to `inside`, unless asking is
    /// pointless or too soon.
    ///
    /// The one place the tunnel is asked, so its two rules — a refusal is
    /// final, and a failure buys it [`DEFAULT_TUNNEL_RECHECK`] of quiet — hold
    /// on every path that could raise one.
    async fn raise_tunnel(&self, inside: &str) -> Option<Connection> {
        if !self.policy.allows_vpn() {
            return None;
        }
        // A tunnel that refused is not asked again. The chain re-decides on a
        // timer, and a timer is exactly the wrong thing to point at a
        // credential: every attempt is a real authentication, and enough of
        // them lock the account.
        if let Some(why) = self.was_refused() {
            debug!("transport: not asking the tunnel again ({why})");
            return None;
        }
        if let Some(at) = *self.tunnel_failed_at.lock().unwrap() {
            if Instant::now() < at + self.tunnel_recheck {
                debug!("transport: the tunnel failed recently; not raising it again yet");
                return None;
            }
        }
        match self.tunnel.open(inside, SMB_PORT).await {
            Ok(connection) => {
                *self.tunnel_failed_at.lock().unwrap() = None;
                info!("transport: SMB reachable through the tunnel at {inside}");
                Some(connection)
            }
            Err(e) => {
                self.tunnel_failed(&e);
                None
            }
        }
    }

    /// Note a tunnel that would not open, in whichever of the two ways it
    /// failed.
    fn tunnel_failed(&self, e: &TunnelUnavailable) {
        match e {
            TunnelUnavailable::Refused(why) => {
                warn!("transport: the tunnel refused, and will not be asked again until something changes ({why})");
                *self.refused.lock().unwrap() = Some(why.clone());
            }
            // Not routine, and not `debug`: reaching here means a tunnel was
            // configured, SMB did not answer directly, and the escalation
            // that exists for exactly that case did not work. Left at `debug`
            // this was invisible at the level people run, so a mount that
            // tried and failed looked identical to one with no tunnel at all
            // — the fallback line, and nothing above it.
            TunnelUnavailable::Transient(_) => {
                warn!("transport: no SMB through the tunnel ({e})");
                *self.tunnel_failed_at.lock().unwrap() = Some(Instant::now());
            }
        }
    }

    /// The transport to use now, and the address to use it against.
    ///
    /// Cheap on the common path: a decision that has not expired is returned
    /// without touching the network.
    pub async fn route(&self) -> Result<Route, NoTransport> {
        if let Some(cached) = self.cached(Instant::now()) {
            return Ok(cached);
        }
        // The tunnel leg proves itself by opening a connection, and a caller
        // asking only which transport to use has nowhere to put one. Dropped
        // rather than avoided: there is no cheaper question to ask.
        let (chosen, _proof) = self.decide().await?;
        self.remember(chosen.clone(), Instant::now());
        Ok(chosen)
    }

    /// Reach SMB by the best route there is.
    ///
    /// This is what a mount calls. [`route`](Self::route) answers *which*
    /// transport; this answers *with* one — and on the tunnel leg that is a
    /// connection already open, because opening it was the only way to know
    /// the leg works.
    pub async fn reach_smb(&self) -> Result<SmbRoute, NoTransport> {
        if let Some(cached) = self.cached(Instant::now()) {
            match cached.transport {
                Transport::SmbDirect => {
                    if let Some(host) = cached.smb_host {
                        return Ok(SmbRoute::Direct { host });
                    }
                }
                Transport::Https => return Ok(SmbRoute::Unavailable),
                Transport::SmbOverVpn => {
                    if let Some(host) = cached.smb_host.clone() {
                        match self.tunnel.open(&host, SMB_PORT).await {
                            Ok(connection) => return Ok(SmbRoute::Tunnelled { host, connection }),
                            // What was remembered no longer holds. Falling
                            // through re-decides rather than reporting a
                            // failure the chain has other answers to.
                            // Falling through re-decides rather than
                            // reporting a failure the chain has other answers
                            // to — but without asking the tunnel again,
                            // because it has just been asked, and a second
                            // attempt in one call is a second wait for an
                            // answer already given.
                            Err(e) => {
                                debug!("transport: the remembered tunnel no longer opens ({e})");
                                self.tunnel_failed(&e);
                                self.forget(&cached);
                                let (chosen, _) = self.decide_without_the_tunnel().await?;
                                self.remember(chosen.clone(), Instant::now());
                                return Ok(match (chosen.transport, chosen.smb_host) {
                                    (Transport::SmbDirect, Some(host)) => SmbRoute::Direct { host },
                                    _ => SmbRoute::Unavailable,
                                });
                            }
                        }
                    }
                }
            }
        }

        let (chosen, opened) = self.decide().await?;
        self.remember(chosen.clone(), Instant::now());
        Ok(match (chosen.transport, chosen.smb_host, opened) {
            (Transport::SmbOverVpn, Some(host), Some(connection)) => {
                SmbRoute::Tunnelled { host, connection }
            }
            (Transport::SmbDirect, Some(host), _) => SmbRoute::Direct { host },
            _ => SmbRoute::Unavailable,
        })
    }

    /// Tell the chain that the route it handed out has stopped working.
    ///
    /// The best leg is remembered without an expiry, because there is nothing
    /// better to look for — which also means nothing ever asks again. A laptop
    /// carried off the campus network keeps being told SMB answers directly
    /// until something says otherwise, and the only thing that knows is
    /// whoever tried to use it.
    pub fn reconsider(&self) {
        *self.decision.lock().unwrap() = None;
        *self.refused.lock().unwrap() = None;
        *self.tunnel_failed_at.lock().unwrap() = None;
    }

    /// Drop a remembered decision, but only if it is still the one we acted
    /// on. A concurrent caller may have written a fresher answer between our
    /// reading it and finding it wanting, and that one is not ours to discard.
    /// Expire the remembered decision, leaving a refusal standing.
    ///
    /// What the recheck interval does when it lapses, without a test having to
    /// wait for it.
    #[cfg(test)]
    fn reconsider_the_decision_only(&self) {
        *self.decision.lock().unwrap() = None;
    }

    fn was_refused(&self) -> Option<String> {
        self.refused.lock().unwrap().clone()
    }

    fn forget(&self, acted_on: &Route) {
        let mut guard = self.decision.lock().unwrap();
        if guard.as_ref().is_some_and(|held| &held.route == acted_on) {
            *guard = None;
        }
    }

    /// The current choice without deciding one, for a status line that must
    /// not cause network traffic to render.
    pub fn current(&self) -> Option<Route> {
        self.decision
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.route.clone())
    }

    fn cached(&self, now: Instant) -> Option<Route> {
        let guard = self.decision.lock().unwrap();
        let decision = guard.as_ref()?;
        match decision.expires_at {
            // Best available: nothing to look for, so it never goes stale.
            None => Some(decision.route.clone()),
            Some(at) if now < at => Some(decision.route.clone()),
            Some(_) => None,
        }
    }

    fn remember(&self, route: Route, now: Instant) {
        let expires_at = if route.transport.is_best() {
            None
        } else {
            Some(now + self.recheck)
        };
        *self.decision.lock().unwrap() = Some(Decision { route, expires_at });
    }

    /// The best leg available, and — on the tunnel leg — the connection that
    /// proved it.
    async fn decide(&self) -> Result<(Route, Option<Connection>), NoTransport> {
        self.decide_with_tunnel(true).await
    }

    /// The same ladder with the tunnel rung missing, for a caller that has
    /// just tried it.
    async fn decide_without_the_tunnel(&self) -> Result<(Route, Option<Connection>), NoTransport> {
        self.decide_with_tunnel(false).await
    }

    async fn decide_with_tunnel(
        &self,
        may_open: bool,
    ) -> Result<(Route, Option<Connection>), NoTransport> {
        if self.policy.allows_smb() {
            let public = &self.endpoints.public_host;
            if self.prober.smb_reachable(public).await {
                debug!("transport: SMB answers directly at {public}");
                return Ok((
                    Route {
                        transport: Transport::SmbDirect,
                        smb_host: Some(public.clone()),
                    },
                    None,
                ));
            }
            match (self.policy.allows_vpn(), &self.endpoints.tunnel_host) {
                // The address *inside* the tunnel, not the public name: the
                // tunnel pushes no DNS, and the public name is exactly the one
                // that just failed. A tunnel that comes up without reaching
                // SMB and one that never comes up are now the same answer
                // here — the error says which, and it is the tunnel's to
                // explain rather than this layer's to guess.
                (true, Some(inside)) if may_open => {
                    if let Some(connection) = self.raise_tunnel(inside).await {
                        return Ok((
                            Route {
                                transport: Transport::SmbOverVpn,
                                smb_host: Some(inside.clone()),
                            },
                            Some(connection),
                        ));
                    }
                }
                // Just tried, and it did not open. Asking again in the same
                // call buys a second wait for an answer already given.
                (true, Some(_)) => {}
                // Nowhere for an escalation to land. Raising the tunnel would
                // still leave us with no address to mount, so don't.
                (true, None) => {
                    debug!("transport: no in-tunnel address configured, so no escalation")
                }
                (false, _) => {}
            }
        }

        if self.policy.allows_https() {
            info!("transport: falling back to the HTTP API");
            return Ok((
                Route {
                    transport: Transport::Https,
                    smb_host: None,
                },
                None,
            ));
        }
        Err(NoTransport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PUBLIC: &str = "e4e-nas.ucsd.edu";
    const INSIDE: &str = "10.90.24.1";

    #[derive(Default)]
    struct FakeProber {
        /// Hosts that answer on 445.
        answering: StdMutex<Vec<String>>,
        /// Every host asked, in order.
        asked: StdMutex<Vec<String>>,
    }

    impl FakeProber {
        fn answering(hosts: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                answering: StdMutex::new(hosts.iter().map(|h| h.to_string()).collect()),
                asked: StdMutex::new(Vec::new()),
            })
        }
        fn nothing_answers() -> Arc<Self> {
            Self::answering(&[])
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
        fn probes(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
        fn starts_answering(&self, host: &str) {
            self.answering.lock().unwrap().push(host.to_string());
        }
    }

    #[async_trait]
    impl Prober for Arc<FakeProber> {
        async fn smb_reachable(&self, host: &str) -> bool {
            self.asked.lock().unwrap().push(host.to_string());
            self.answering.lock().unwrap().iter().any(|h| h == host)
        }
    }

    struct FakeTunnel {
        /// Whether a connection through it can be opened at all.
        works: AtomicBool,
        /// Whether its refusal is one that asking again could change.
        refuses: AtomicBool,
        opens: AtomicUsize,
        /// Every host it was asked to open a connection to, in order.
        asked: StdMutex<Vec<String>>,
        /// The far end of whatever it last handed out, so a test can see what
        /// was written to it.
        far_end: StdMutex<Option<tokio::io::DuplexStream>>,
    }

    impl FakeTunnel {
        fn new(works: bool) -> Arc<Self> {
            Arc::new(Self {
                works: AtomicBool::new(works),
                refuses: AtomicBool::new(false),
                opens: AtomicUsize::new(0),
                asked: StdMutex::new(Vec::new()),
                far_end: StdMutex::new(None),
            })
        }
        /// A tunnel that cannot be raised.
        fn absent() -> Arc<Self> {
            Self::new(false)
        }
        /// A tunnel that opens a connection to the NAS.
        fn reaching() -> Arc<Self> {
            Self::new(true)
        }
        fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }
        fn stops_working(&self) {
            self.works.store(false, Ordering::SeqCst);
        }
        /// A tunnel that turns the credentials down, which is not a thing
        /// asking again can fix.
        fn refuses() -> Arc<Self> {
            let tunnel = Self::new(false);
            tunnel.refuses.store(true, Ordering::SeqCst);
            tunnel
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
        fn take_far_end(&self) -> tokio::io::DuplexStream {
            self.far_end.lock().unwrap().take().expect("one was opened")
        }
    }

    #[async_trait]
    impl Tunnel for Arc<FakeTunnel> {
        async fn open(&self, host: &str, port: u16) -> Result<Connection, TunnelUnavailable> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.asked.lock().unwrap().push(format!("{host}:{port}"));
            if !self.works.load(Ordering::SeqCst) {
                return Err(if self.refuses.load(Ordering::SeqCst) {
                    TunnelUnavailable::Refused("that password is not the one".into())
                } else {
                    TunnelUnavailable::Transient("no route".into())
                });
            }
            let (ours, theirs) = tokio::io::duplex(4096);
            *self.far_end.lock().unwrap() = Some(theirs);
            Ok(Box::new(ours))
        }
    }

    fn chain(policy: TransportPolicy, prober: Arc<FakeProber>, tunnel: Arc<FakeTunnel>) -> Chain {
        Chain::new(
            policy,
            Endpoints::with_tunnel(PUBLIC, INSIDE),
            Box::new(prober),
            Box::new(tunnel),
            DEFAULT_RECHECK,
        )
    }

    #[test]
    fn forbidding_everything_is_a_configuration_error() {
        // Better to refuse at startup than to mount something that cannot
        // answer a single read.
        assert!(TransportPolicy::from_flags(true, false, true).is_err());
    }

    #[test]
    fn disabling_smb_also_forbids_the_tunnel() {
        // A tunnel exists to make SMB reachable. Without SMB there is nothing
        // on the other side of it worth dialling.
        let policy = TransportPolicy::from_flags(true, false, false).unwrap();
        assert!(!policy.allows_smb());
        assert!(!policy.allows_vpn());
        assert!(policy.allows_https());
    }

    #[tokio::test]
    async fn a_reachable_smb_wins_and_no_tunnel_is_dialled() {
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        let route = chain.route().await.unwrap();
        assert_eq!(route.transport, Transport::SmbDirect);
        assert_eq!(route.smb_host.as_deref(), Some(PUBLIC));
        assert_eq!(prober.asked(), vec![PUBLIC], "the public name, once");
        assert_eq!(tunnel.opens(), 0, "nothing to escalate");
    }

    #[tokio::test]
    async fn the_tunnel_leg_is_probed_at_the_in_tunnel_address() {
        // The whole reason Endpoints carries two hosts: the tunnel pushes no
        // DNS, so the public name still fails inside it. Probing that name
        // again would report the escalation as useless and fall to HTTP.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        let route = chain.route().await.unwrap();
        assert_eq!(route.transport, Transport::SmbOverVpn);
        assert_eq!(
            route.smb_host.as_deref(),
            Some(INSIDE),
            "callers mount the in-tunnel address, not the public name"
        );
        assert_eq!(
            prober.asked(),
            vec![PUBLIC],
            "the public name is asked once, and not again inside the tunnel"
        );
        assert_eq!(
            tunnel.asked(),
            vec![format!("{INSIDE}:445")],
            "the connection is opened to the in-tunnel address, which is what \
             proves the leg — the public name is exactly the one that just failed"
        );
        assert_eq!(tunnel.opens(), 1);
    }

    #[tokio::test]
    async fn without_an_in_tunnel_address_no_tunnel_is_dialled() {
        // Raising a tunnel we have no address to mount through would cost the
        // user a connection and change nothing.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::absent();
        let chain = Chain::new(
            TransportPolicy::default(),
            Endpoints::public_only(PUBLIC),
            Box::new(prober.clone()),
            Box::new(tunnel.clone()),
            DEFAULT_RECHECK,
        );

        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);
        assert_eq!(tunnel.opens(), 0);
    }

    #[tokio::test]
    async fn a_tunnel_that_cannot_reach_smb_is_not_the_answer() {
        // Connected to *a* network is not connected to *this* NAS, and now
        // that opening the connection is the probe, those two failures are one
        // — a tunnel that does not come up and one that comes up somewhere
        // useless both fail to hand back a connection. Reporting SmbOverVpn
        // for either would blame SMB for a routing problem.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        let route = chain.route().await.unwrap();
        assert_eq!(route.transport, Transport::Https);
        assert_eq!(route.smb_host, None, "the HTTP leg carries no SMB");
        assert_eq!(tunnel.opens(), 1, "tried, and honestly reported");
    }

    #[tokio::test]
    async fn a_tunnel_that_refuses_is_not_asked_again() {
        // The chain re-decides on a timer, and a timer is exactly the wrong
        // thing to point at a credential. Every attempt is a real
        // authentication against the domain controller, so a mount left
        // running with a mistyped password would work its way to a locked
        // account — for something the user has already given up on.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::refuses();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        for _ in 0..5 {
            assert_eq!(chain.route().await.unwrap().transport, Transport::Https);
            chain.reconsider_the_decision_only();
        }

        assert_eq!(tunnel.opens(), 1, "asked once, and believed");
    }

    #[tokio::test]
    async fn a_refusal_is_reconsidered_when_something_has_changed() {
        // Not asking again is right until the thing that was refused changes.
        // Only the caller knows that it has — they are the one who asked for a
        // password again.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::refuses();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);
        chain.reconsider();
        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);

        assert_eq!(tunnel.opens(), 2, "asked again, having been told to");
    }

    #[tokio::test]
    async fn a_stale_tunnel_route_is_not_asked_twice_in_one_breath() {
        // Re-deciding after a remembered tunnel stops opening is right; asking
        // the tunnel again inside the same call is not. Every attempt is a
        // wait, and this one has just been made.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert!(matches!(
            chain.reach_smb().await.unwrap(),
            SmbRoute::Tunnelled { .. }
        ));
        tunnel.stops_working();

        assert!(matches!(
            chain.reach_smb().await.unwrap(),
            SmbRoute::Unavailable
        ));
        assert_eq!(
            tunnel.opens(),
            2,
            "the first call opened one, the second attempted one — not two"
        );
    }

    #[tokio::test]
    async fn a_caller_can_say_the_route_stopped_working() {
        // The best leg is remembered without an expiry, so nothing ever asks
        // again on its own. A laptop carried off the campus network would keep
        // being told SMB answers directly, and the only thing that knows
        // otherwise is whoever tried to use it.
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert_eq!(chain.route().await.unwrap().transport, Transport::SmbDirect);
        let probes = prober.probes();
        assert_eq!(
            chain.route().await.unwrap().transport,
            Transport::SmbDirect,
            "remembered, and not asked again"
        );
        assert_eq!(prober.probes(), probes);

        chain.reconsider();
        assert_eq!(chain.route().await.unwrap().transport, Transport::SmbDirect);
        assert!(prober.probes() > probes, "told otherwise, it looks again");
    }

    #[tokio::test]
    async fn a_disabled_tunnel_falls_straight_to_http() {
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let policy = TransportPolicy::from_flags(false, true, false).unwrap();
        let chain = chain(policy, prober.clone(), tunnel.clone());

        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);
        assert_eq!(tunnel.opens(), 0, "no dialog, no dialling");
    }

    #[tokio::test]
    async fn disabling_smb_costs_no_probe() {
        // The point of the flag on a network that black-holes 445: not one
        // connect timeout is paid.
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let policy = TransportPolicy::from_flags(true, false, false).unwrap();
        let chain = chain(policy, prober.clone(), tunnel.clone());

        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);
        assert_eq!(prober.probes(), 0);
    }

    #[tokio::test]
    async fn disabling_http_fails_loudly_rather_than_pretending() {
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::absent();
        let policy = TransportPolicy::from_flags(false, false, true).unwrap();
        let chain = chain(policy, prober.clone(), tunnel.clone());

        assert!(chain.route().await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_choice_is_not_re_probed_on_every_call() {
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        for _ in 0..5 {
            assert_eq!(chain.route().await.unwrap().transport, Transport::SmbDirect);
        }
        assert_eq!(prober.probes(), 1, "decided once, remembered after that");
    }

    #[tokio::test(start_paused = true)]
    async fn sitting_on_the_best_transport_never_probes_again() {
        // Nothing better exists, so a timer here would be pure noise on the
        // network for the entire life of the mount.
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        chain.route().await.unwrap();
        tokio::time::advance(DEFAULT_RECHECK * 10).await;
        chain.route().await.unwrap();

        assert_eq!(prober.probes(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_degraded_choice_is_looked_at_again_and_promoted() {
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);

        // The network comes back: on campus, off the hotel wifi, tunnel up by
        // other means.
        prober.starts_answering(PUBLIC);
        assert_eq!(
            chain.route().await.unwrap().transport,
            Transport::Https,
            "still trusted until it expires"
        );

        tokio::time::advance(DEFAULT_RECHECK + Duration::from_secs(1)).await;
        assert_eq!(chain.route().await.unwrap().transport, Transport::SmbDirect);
    }

    #[tokio::test]
    async fn the_status_line_never_causes_traffic() {
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert_eq!(chain.current(), None, "nothing decided yet");
        chain.route().await.unwrap();
        assert_eq!(
            chain.current().map(|r| r.transport),
            Some(Transport::SmbDirect)
        );
        assert_eq!(prober.probes(), 1, "asking what is live decided nothing");
    }

    #[tokio::test]
    async fn the_tunnel_leg_hands_back_the_connection_it_opened() {
        // The point of the whole shape. Opening it was the only way to learn
        // the leg works, so the caller gets that connection rather than an
        // address it would have to dial through a tunnel it cannot see.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        let reached = chain.reach_smb().await.unwrap();
        let SmbRoute::Tunnelled {
            host,
            mut connection,
        } = reached
        else {
            panic!("the tunnel leg should have been taken");
        };
        assert_eq!(host, INSIDE, "and it still names the server");

        // A real connection, not a token: what goes in comes out the far end.
        connection.write_all(b"smb2 would go here").await.unwrap();
        let mut far = tunnel.take_far_end();
        let mut heard = [0u8; 18];
        far.read_exact(&mut heard).await.unwrap();
        assert_eq!(&heard, b"smb2 would go here");

        assert_eq!(tunnel.opens(), 1, "and it was opened once, not twice");
    }

    #[tokio::test]
    async fn a_remembered_tunnel_route_opens_a_fresh_connection() {
        // The decision is cacheable; a connection is not. A second caller gets
        // its own, without the ladder being climbed again.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        let _first = chain.reach_smb().await.unwrap();
        let probes = prober.probes();
        let second = chain.reach_smb().await.unwrap();

        assert!(matches!(second, SmbRoute::Tunnelled { .. }));
        assert_eq!(tunnel.opens(), 2, "a connection each");
        assert_eq!(
            prober.probes(),
            probes,
            "and the public host was not asked again"
        );
    }

    #[tokio::test]
    async fn a_tunnel_that_stops_opening_makes_the_chain_look_again() {
        // A remembered answer that no longer holds is not a failure to report:
        // the chain has other legs, and the whole point of remembering is that
        // it can stop.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert!(matches!(
            chain.reach_smb().await.unwrap(),
            SmbRoute::Tunnelled { .. }
        ));
        tunnel.stops_working();

        assert!(
            matches!(chain.reach_smb().await.unwrap(), SmbRoute::Unavailable),
            "it falls to HTTP rather than insisting on what it remembered"
        );
    }

    #[tokio::test]
    async fn a_reachable_nas_is_dialled_by_whoever_wants_it() {
        // Nothing is opened on this leg: the caller can reach the address
        // itself, and opening a connection here would be one it did not ask
        // for.
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        match chain.reach_smb().await.unwrap() {
            SmbRoute::Direct { host } => assert_eq!(host, PUBLIC),
            _ => panic!("SMB answers directly"),
        }
        assert_eq!(tunnel.opens(), 0);
    }

    // ── The tunnel's own interval ─────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn a_tunnel_that_failed_waits_out_its_own_interval() {
        // The direct probe is one bounded TCP connect; raising the tunnel is a
        // full OpenVPN handshake and a real authentication. Re-deciding every
        // minute is right for the first and wrong for the second — the case
        // that found this is a tunnel that comes up and a NAS silent inside it,
        // which would have been a handshake a minute for as long as the mount
        // stayed up.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);
        assert_eq!(tunnel.opens(), 1);

        tokio::time::advance(DEFAULT_RECHECK + Duration::from_secs(1)).await;
        assert_eq!(chain.route().await.unwrap().transport, Transport::Https);
        assert_eq!(prober.probes(), 2, "the cheap leg is looked at again");
        assert_eq!(tunnel.opens(), 1, "the expensive one is not, yet");

        tokio::time::advance(DEFAULT_TUNNEL_RECHECK).await;
        chain.route().await.unwrap();
        assert_eq!(tunnel.opens(), 2, "and once its own interval is up, it is");
    }

    #[tokio::test]
    async fn reconsidering_forgets_that_the_tunnel_failed() {
        // `reconsider` is the caller saying something has changed; a failed
        // tunnel is as much a stale conclusion as a refused one.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        chain.route().await.unwrap();
        chain.reconsider();
        chain.route().await.unwrap();

        assert_eq!(tunnel.opens(), 2);
    }

    #[tokio::test]
    async fn a_tunnel_that_worked_is_not_held_to_the_interval() {
        // The interval is a cost of failing. A tunnel whose stream ended is
        // one the caller wants back now, and has every reason to expect.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        chain.reach_smb().await.unwrap();
        chain.reconsider_the_decision_only();
        assert!(matches!(
            chain.reach_smb().await.unwrap(),
            SmbRoute::Tunnelled { .. }
        ));
        assert_eq!(tunnel.opens(), 2);
    }

    // ── Looking for something better ──────────────────────────────────────────

    #[tokio::test]
    async fn nothing_is_better_than_the_best_leg() {
        // `is_best` is the contract that a mount on direct SMB causes no
        // traffic of its own. Looking for better must keep it.
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert!(chain.better_than(Transport::SmbDirect).await.is_none());
        assert_eq!(prober.probes(), 0);
        assert_eq!(tunnel.opens(), 0);
    }

    #[tokio::test]
    async fn from_http_a_direct_answer_is_better() {
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        match chain.better_than(Transport::Https).await {
            Some(SmbRoute::Direct { host }) => assert_eq!(host, PUBLIC),
            _ => panic!("SMB answers directly"),
        }
        assert_eq!(tunnel.opens(), 0, "nothing to escalate to");
    }

    #[tokio::test]
    async fn from_http_the_tunnel_is_better_than_nothing() {
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        match chain.better_than(Transport::Https).await {
            Some(SmbRoute::Tunnelled { host, .. }) => assert_eq!(host, INSIDE),
            _ => panic!("the tunnel reaches SMB"),
        }
    }

    #[tokio::test]
    async fn from_the_tunnel_only_direct_is_better() {
        // A laptop on the tunnel that walks back onto campus. The tunnel is
        // working; the question is only whether the direct leg now answers,
        // and raising a second tunnel to ask would be a handshake for nothing.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert!(chain.better_than(Transport::SmbOverVpn).await.is_none());
        assert_eq!(prober.asked(), vec![PUBLIC]);
        assert_eq!(tunnel.opens(), 0);

        prober.starts_answering(PUBLIC);
        assert!(matches!(
            chain.better_than(Transport::SmbOverVpn).await,
            Some(SmbRoute::Direct { .. })
        ));
        assert_eq!(tunnel.opens(), 0);
    }

    #[tokio::test]
    async fn looking_for_better_never_asks_a_tunnel_that_refused() {
        // The reason this is not a plain timer around `reach_smb`. DSM blocks
        // an address after three failed logins, and the block does not expire.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::refuses();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        chain.route().await.unwrap();
        for _ in 0..5 {
            assert!(chain.better_than(Transport::Https).await.is_none());
        }
        assert_eq!(tunnel.opens(), 1, "asked once, and believed");
    }

    #[tokio::test(start_paused = true)]
    async fn looking_for_better_keeps_to_the_tunnel_interval() {
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert!(chain.better_than(Transport::Https).await.is_none());
        assert!(chain.better_than(Transport::Https).await.is_none());
        assert_eq!(tunnel.opens(), 1);
        assert_eq!(prober.probes(), 2, "the direct probe is not rationed");

        tokio::time::advance(DEFAULT_TUNNEL_RECHECK + Duration::from_secs(1)).await;
        assert!(chain.better_than(Transport::Https).await.is_none());
        assert_eq!(tunnel.opens(), 2);
    }

    #[tokio::test]
    async fn looking_for_better_decides_nothing_until_told() {
        // A better leg that answers a probe is not yet a leg the mount is on:
        // the SMB session on it can still be refused. Remembering it early
        // would report a transport nothing is using.
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::absent();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        chain.better_than(Transport::Https).await.expect("better");
        assert_eq!(chain.current(), None);

        let direct = Route {
            transport: Transport::SmbDirect,
            smb_host: Some(PUBLIC.into()),
        };
        chain.settle(direct.clone());
        assert_eq!(chain.current(), Some(direct));
    }

    #[tokio::test]
    async fn a_tunnel_that_carried_nothing_counts_as_one_that_failed() {
        // Opening a connection through the tunnel proves the tunnel, not the
        // session on top of it. A caller that could not build one says so,
        // and the tunnel then keeps to the interval any failure would earn it.
        let prober = FakeProber::nothing_answers();
        let tunnel = FakeTunnel::reaching();
        let chain = chain(TransportPolicy::default(), prober.clone(), tunnel.clone());

        assert!(matches!(
            chain.better_than(Transport::Https).await,
            Some(SmbRoute::Tunnelled { .. })
        ));
        chain.tunnel_carried_nothing("the session setup timed out");

        assert!(chain.better_than(Transport::Https).await.is_none());
        assert_eq!(tunnel.opens(), 1);
    }

    #[tokio::test]
    async fn with_smb_disabled_nothing_is_better() {
        let prober = FakeProber::answering(&[PUBLIC]);
        let tunnel = FakeTunnel::reaching();
        let policy = TransportPolicy::from_flags(true, false, false).unwrap();
        let chain = chain(policy, prober.clone(), tunnel.clone());

        assert!(!chain.may_use_smb());
        assert!(chain.better_than(Transport::Https).await.is_none());
        assert_eq!(prober.probes(), 0);
    }

    // ── What reaches the user ─────────────────────────────────────────────────

    /// The buffer a capturing test reads its assertions out of.
    #[derive(Clone, Default)]
    struct Sink(Arc<StdMutex<Vec<u8>>>);

    /// Only one log-capturing test at a time, and where its output goes.
    static CAPTURING: StdMutex<()> = StdMutex::new(());
    static ACTIVE: StdMutex<Option<Sink>> = StdMutex::new(None);

    /// The writer the one global subscriber holds, forwarding to whichever
    /// capture is running.
    ///
    /// A thread-local subscriber looks like the right tool and is not:
    /// `tracing` caches each callsite's *interest* globally, so the first
    /// thread to reach one decides for every thread whether anybody wants it.
    /// With thirty-eight other tests running beside these two, a callsite
    /// registered by a thread holding no subscriber is cached as
    /// uninteresting, and the test that does want it then sees nothing —
    /// roughly one run in twenty-five, and never when the test runs alone.
    /// Serialising the captures does not help, because it is the *other*
    /// tests doing the registering.
    ///
    /// One subscriber, installed globally and never removed, keeps every
    /// callsite permanently interesting. Lines logged by tests running
    /// alongside land in the buffer too, which costs nothing: the assertions
    /// ask what the log *contains*.
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

    /// Captures what the process logs for as long as it is held.
    ///
    /// Which leg the chain chose, and why the better ones were not available,
    /// reaches a user as log output and nothing else — so the level an event
    /// is emitted at is behaviour, not decoration, and belongs under test like
    /// any other.
    struct LogCapture {
        sink: Sink,
        /// Held for the life of the capture, so two of these never overlap.
        _capturing: std::sync::MutexGuard<'static, ()>,
    }

    impl LogCapture {
        /// Capture at `info` — what somebody gets without changing a setting,
        /// which is the whole point of asserting on it.
        fn at_the_default_level() -> Self {
            // A panicking capture test must not stop the next one running.
            let capturing = CAPTURING.lock().unwrap_or_else(|e| e.into_inner());
            static INSTALLED: std::sync::Once = std::sync::Once::new();
            INSTALLED.call_once(|| {
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(RouteToActiveCapture)
                    .with_max_level(tracing::Level::INFO)
                    .with_ansi(false)
                    .finish();
                // Another test binary in this process may have got there
                // first; either way a subscriber is in place afterwards.
                let _ = tracing::subscriber::set_global_default(subscriber);
            });
            let sink = Sink::default();
            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink.clone());
            Self {
                sink,
                _capturing: capturing,
            }
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.sink.0.lock().unwrap_or_else(|e| e.into_inner()))
                .into_owned()
        }
    }

    impl Drop for LogCapture {
        fn drop(&mut self) {
            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// Regression: every way the tunnel leg could fail *except* a refusal was
    /// logged at `debug`. At the level people actually run, a mount that
    /// raised a tunnel, failed to reach SMB through it, and quietly used the
    /// slower HTTP API produced exactly the same output as one with no tunnel
    /// configured at all — the fallback line, and nothing above it.
    ///
    /// `docs/vpn-live-pass.md` even tabulates the messages this hid, which is
    /// how the gap was found: documented, and unreachable.
    #[tokio::test]
    async fn a_tunnel_that_did_not_reach_smb_says_so_at_the_default_level() {
        let logs = LogCapture::at_the_default_level();
        let chain = chain(
            TransportPolicy::default(),
            FakeProber::nothing_answers(),
            FakeTunnel::absent(),
        );

        let route = chain.reach_smb().await.expect("HTTP is still there");

        assert!(matches!(route, SmbRoute::Unavailable));
        let said = logs.text();
        assert!(
            said.contains("no route"),
            "the tunnel's own reason is the only thing that says which leg \
             broke, and it has to survive to the user. Got:\n{said}"
        );
        assert!(
            said.contains("WARN"),
            "a leg that was configured and did not work is not routine. \
             Got:\n{said}"
        );
    }

    /// The other half of the trade: this only fires when SMB was asked for,
    /// did not answer directly, and a tunnel was configured — so a mount that
    /// simply works has nothing to warn about, and the level stays meaningful.
    #[tokio::test]
    async fn a_leg_that_works_warns_about_nothing() {
        let logs = LogCapture::at_the_default_level();
        let chain = chain(
            TransportPolicy::default(),
            FakeProber::answering(&[PUBLIC]),
            FakeTunnel::reaching(),
        );

        chain.reach_smb().await.expect("SMB answers directly");

        let said = logs.text();
        assert!(!said.contains("WARN"), "nothing went wrong. Got:\n{said}");
    }
}
