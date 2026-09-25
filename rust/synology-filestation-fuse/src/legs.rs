//! Which leg a mount is on, and moving it to a better one while it runs.
//!
//! The chain decides which leg reaches the NAS; this is what keeps asking.
//! Before it existed the answer given at connect was the answer for the life
//! of the mount: one that came up on HTTP because SMB was unreachable stayed
//! on HTTP after a NAS-side firewall fix or a walk back onto campus, and one
//! that came up through the tunnel kept paying for the tunnel with the NAS a
//! TCP connect away. The difference is not cosmetic — SMB addresses byte
//! ranges, so an interrupted transfer resumes; the HTTP API has no resume and
//! loses the file.
//!
//! The mount's SMB transport is attached from the start, with or without a
//! session, and a better leg reaches it by being handed a session on that leg
//! ([`SmbTransport::adopt`]). So the client's transport list never changes
//! after it is built, nothing in flight is disturbed, and the first session a
//! mount gets is just the first time this looked.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use synology_filestation_connect::{Chain, Route, SmbRoute, Transport};
use synology_filestation_core::error::SynoFsError;
use synology_filestation_smb::{SmbConfig, SmbTransport};
use tracing::{info, warn};

use crate::{dial_direct, reopen_through, Dial};

/// How often a mount on a degraded leg looks for a better one.
///
/// The direct probe it costs is one bounded TCP connect. The tunnel keeps to
/// its own, longer interval inside the chain, and a mount on the best leg
/// looks for nothing at all.
pub const DEFAULT_WATCH_INTERVAL: Duration = synology_filestation_connect::DEFAULT_RECHECK;

/// The part of an SMB transport this needs, so the decisions can be tested
/// without a server.
pub trait Session: Send + Sync + 'static {
    /// Whether a session is up and not known to be dead.
    fn is_connected(&self) -> bool;
    /// Whether the server has refused the login. Nothing asks again after.
    fn is_refused(&self) -> bool;
    /// Build a session on `route` and make it the one in use.
    fn take(
        &self,
        route: SmbRoute,
        dial: Dial,
    ) -> impl Future<Output = Result<(), SynoFsError>> + Send;
}

impl Session for SmbTransport {
    fn is_connected(&self) -> bool {
        SmbTransport::is_connected(self)
    }

    fn is_refused(&self) -> bool {
        self.refused().is_some()
    }

    async fn take(&self, route: SmbRoute, dial: Dial) -> Result<(), SynoFsError> {
        match route {
            SmbRoute::Direct { host } => {
                let stream = dial_direct(&host, dial).await?;
                self.adopt(stream, &host).await
            }
            SmbRoute::Tunnelled { host, connection } => self.adopt(connection, &host).await,
            SmbRoute::Unavailable => Err(SynoFsError::NotSupported),
        }
    }
}

/// A mount's SMB session and the chain that decides which leg it runs on.
pub struct Legs<S: Session = SmbTransport> {
    chain: Arc<Chain>,
    session: Arc<S>,
    /// Where and how long to dial a direct leg — the same as the redial uses.
    dial: Dial,
    /// The leg last reported, so a change is logged once rather than every
    /// time the watch looks.
    reported: Mutex<Option<Transport>>,
}

impl Legs<SmbTransport> {
    /// Attach a transport for SMB and put it on the best leg that answers now.
    ///
    /// `None` when the policy forbids SMB, since then there is nothing to
    /// attach and nothing to look for later. Otherwise the transport comes
    /// back whether or not a leg answered: attached without a session it
    /// stands aside for HTTP, and [`watch`](Self::watch) hands it one when
    /// SMB becomes reachable.
    pub async fn start(chain: Arc<Chain>, cfg: &SmbConfig) -> Option<Arc<Self>> {
        if !chain.may_use_smb() {
            info!("Transport: the HTTP API (SMB is disabled)");
            return None;
        }
        // The redial is what a session that dies between looks uses to come
        // straight back; the watch covers everything the redial cannot.
        let dial = Dial::for_config(cfg);
        let smb = Arc::new(SmbTransport::unconnected(
            cfg,
            reopen_through(chain.clone(), dial),
        ));
        let legs = Arc::new(Self::new(chain, smb, dial));
        legs.step().await;
        Some(legs)
    }
}

impl<S: Session> Legs<S> {
    pub fn new(chain: Arc<Chain>, session: Arc<S>, dial: Dial) -> Self {
        Self {
            chain,
            session,
            dial,
            reported: Mutex::new(None),
        }
    }

    /// The SMB transport, for attaching to the client.
    pub fn session(&self) -> &Arc<S> {
        &self.session
    }

    /// The leg carrying data right now. Never touches the network.
    ///
    /// A session that is not live means HTTP is what answers, whatever the
    /// chain last decided — the badge has to say what carries the data, not
    /// what was planned. A live one is on whichever SMB leg the chain last
    /// settled on, which a redial keeps current as well as a move does.
    pub fn current(&self) -> Transport {
        if !self.session.is_connected() {
            return Transport::Https;
        }
        match self.chain.current() {
            Some(Route { transport, .. }) if transport != Transport::Https => transport,
            // Connected with no SMB leg on record is a moment between a
            // redial and its decision landing. Understating it is the safe
            // side: the next look settles it.
            _ => Transport::Https,
        }
    }

    /// Look once for a better leg, move to it if one answers, and return the
    /// leg the mount is on afterwards.
    ///
    /// On the best leg this is free — no probe, no tunnel. After a refused
    /// login it is free too, and stays that way: see [`Session::is_refused`].
    pub async fn step(&self) -> Transport {
        let before = self.current();
        if !self.session.is_refused() {
            if let Some(route) = self.chain.better_than(before).await {
                self.move_to(before, route).await;
            }
        }
        let after = self.current();
        self.report(after);
        after
    }

    async fn move_to(&self, from: Transport, route: SmbRoute) {
        let (leg, host) = match &route {
            SmbRoute::Direct { host } => (Transport::SmbDirect, host.clone()),
            SmbRoute::Tunnelled { host, .. } => (Transport::SmbOverVpn, host.clone()),
            SmbRoute::Unavailable => return,
        };
        match self.session.take(route, self.dial).await {
            Ok(()) => self.chain.settle(Route {
                transport: leg,
                smb_host: Some(host),
            }),
            // Said already, and in full, by the transport that was refused.
            Err(_) if self.session.is_refused() => {}
            // The tunnel opened, so the chain counted it as working; it has to
            // hear otherwise, or the next look raises another one.
            Err(e) if leg == Transport::SmbOverVpn => {
                self.chain.tunnel_carried_nothing(&e.to_string());
                warn!("Transport: no SMB session through the tunnel ({e}); staying on {from}");
            }
            Err(e) => warn!(
                "Transport: {leg} answers at {host}, but no session could be built on it \
                 ({e}); staying on {from}"
            ),
        }
    }

    /// Log the leg when it differs from the one last logged.
    fn report(&self, now: Transport) {
        let mut reported = self.reported.lock().unwrap_or_else(|e| e.into_inner());
        match *reported {
            None => info!("Transport: {now}"),
            Some(was) if was != now => info!("Transport: now {now}, was {was}"),
            Some(_) => {}
        }
        *reported = Some(now);
    }

    /// Look again every `every`, for as long as the returned task runs.
    ///
    /// The first look is `every` from now: whoever built this has just looked,
    /// in [`start`](Legs::start).
    pub fn watch(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Handle,
        every: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let legs = Arc::clone(self);
        // Taken here rather than inside the task, which may not run for a
        // while after it is spawned.
        let first = tokio::time::Instant::now() + every;
        runtime.spawn(async move {
            let mut ticks = tokio::time::interval_at(first, every);
            // A look that overran — a tunnel raised with its full patience —
            // is followed by the next at the usual distance, not a burst.
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                legs.step().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use synology_filestation_connect::{
        Connection, Endpoints, Prober, TransportPolicy, Tunnel, TunnelUnavailable, DEFAULT_RECHECK,
    };

    use crate::log_capture::LogCapture;

    const TEST_DIAL: Dial = Dial {
        port: 445,
        patience: Duration::from_secs(2),
    };
    const PUBLIC: &str = "e4e-nas.ucsd.edu";
    const INSIDE: &str = "10.90.24.1";

    /// Cloned into the chain, so the test keeps a handle on the same state.
    #[derive(Clone, Default)]
    struct FakeProber {
        answers: Arc<AtomicBool>,
        probes: Arc<AtomicUsize>,
    }

    impl FakeProber {
        fn answering(answers: bool) -> Self {
            Self {
                answers: Arc::new(AtomicBool::new(answers)),
                probes: Arc::default(),
            }
        }
        fn starts_answering(&self) {
            self.answers.store(true, Ordering::SeqCst);
        }
        fn probes(&self) -> usize {
            self.probes.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Prober for FakeProber {
        async fn smb_reachable(&self, _host: &str) -> bool {
            self.probes.fetch_add(1, Ordering::SeqCst);
            self.answers.load(Ordering::SeqCst)
        }
    }

    #[derive(Clone)]
    struct FakeTunnel {
        works: bool,
        opens: Arc<AtomicUsize>,
    }

    impl FakeTunnel {
        fn new(works: bool) -> Self {
            Self {
                works,
                opens: Arc::default(),
            }
        }
        fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Tunnel for FakeTunnel {
        async fn open(&self, _host: &str, _port: u16) -> Result<Connection, TunnelUnavailable> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if !self.works {
                return Err(TunnelUnavailable::Transient("no route".into()));
            }
            let (ours, _theirs) = tokio::io::duplex(64);
            Ok(Box::new(ours))
        }
    }

    /// An SMB session that does what it is told.
    #[derive(Default)]
    struct FakeSession {
        connected: AtomicBool,
        refused: AtomicBool,
        /// The next `take` is refused by the server.
        refuses: AtomicBool,
        /// Every `take` fails the way a dead link does.
        fails: AtomicBool,
        took: Mutex<Vec<String>>,
    }

    impl FakeSession {
        fn took(&self) -> Vec<String> {
            self.took.lock().unwrap().clone()
        }
        fn dies(&self) {
            self.connected.store(false, Ordering::SeqCst);
        }
    }

    impl Session for FakeSession {
        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }
        fn is_refused(&self) -> bool {
            self.refused.load(Ordering::SeqCst)
        }
        async fn take(&self, route: SmbRoute, _dial: Dial) -> Result<(), SynoFsError> {
            let what = match &route {
                SmbRoute::Direct { host } => format!("direct {host}"),
                SmbRoute::Tunnelled { host, .. } => format!("tunnel {host}"),
                SmbRoute::Unavailable => "nothing".into(),
            };
            self.took.lock().unwrap().push(what);
            if self.refuses.load(Ordering::SeqCst) {
                self.refused.store(true, Ordering::SeqCst);
                return Err(SynoFsError::PermissionDenied);
            }
            if self.fails.load(Ordering::SeqCst) {
                return Err(SynoFsError::Io("connection reset".into()));
            }
            self.connected.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn legs(
        prober: &FakeProber,
        tunnel: &FakeTunnel,
    ) -> (Arc<Legs<FakeSession>>, Arc<FakeSession>, Arc<Chain>) {
        let chain = Arc::new(Chain::new(
            TransportPolicy::default(),
            Endpoints::with_tunnel(PUBLIC, INSIDE),
            Box::new(prober.clone()),
            Box::new(tunnel.clone()),
            DEFAULT_RECHECK,
        ));
        let session = Arc::new(FakeSession::default());
        let legs = Arc::new(Legs::new(chain.clone(), session.clone(), TEST_DIAL));
        (legs, session, chain)
    }

    #[tokio::test]
    async fn a_mount_that_started_on_http_moves_to_smb_once_it_answers() {
        // The case the issue was filed for: SMB unreachable at connect, a
        // NAS-side fix lands while the mount is up.
        let prober = FakeProber::answering(false);
        let tunnel = FakeTunnel::new(false);
        let (legs, session, chain) = legs(&prober, &tunnel);

        assert_eq!(legs.step().await, Transport::Https);
        assert!(session.took().is_empty(), "nothing answered to take");

        prober.starts_answering();
        assert_eq!(legs.step().await, Transport::SmbDirect);
        assert_eq!(session.took(), vec![format!("direct {PUBLIC}")]);
        assert_eq!(
            chain.current().map(|r| r.transport),
            Some(Transport::SmbDirect),
            "and the chain knows it, so a redial goes the same way"
        );
    }

    #[tokio::test]
    async fn on_the_best_leg_it_looks_for_nothing() {
        let prober = FakeProber::answering(true);
        let tunnel = FakeTunnel::new(true);
        let (legs, _session, _chain) = legs(&prober, &tunnel);

        assert_eq!(legs.step().await, Transport::SmbDirect);
        let probes = prober.probes();
        for _ in 0..5 {
            assert_eq!(legs.step().await, Transport::SmbDirect);
        }

        assert_eq!(prober.probes(), probes, "no traffic of its own");
        assert_eq!(tunnel.opens(), 0);
    }

    #[tokio::test]
    async fn a_tunnelled_mount_moves_to_direct_when_direct_answers() {
        // The laptop that came up off campus and walked back on.
        let prober = FakeProber::answering(false);
        let tunnel = FakeTunnel::new(true);
        let (legs, session, _chain) = legs(&prober, &tunnel);

        assert_eq!(legs.step().await, Transport::SmbOverVpn);
        assert_eq!(legs.step().await, Transport::SmbOverVpn);
        assert_eq!(
            tunnel.opens(),
            1,
            "a working tunnel is not raised again to ask whether it works"
        );

        prober.starts_answering();
        assert_eq!(legs.step().await, Transport::SmbDirect);
        assert_eq!(
            session.took(),
            vec![format!("tunnel {INSIDE}"), format!("direct {PUBLIC}")]
        );
    }

    #[tokio::test]
    async fn a_refused_login_is_never_asked_for_again() {
        // DSM blocks an address after three failed logins, for good. An AD
        // account with no domain set fails every time, so a watch that kept
        // offering sessions would lock the machine out within minutes.
        let prober = FakeProber::answering(true);
        let tunnel = FakeTunnel::new(true);
        let (legs, session, _chain) = legs(&prober, &tunnel);
        session.refuses.store(true, Ordering::SeqCst);

        assert_eq!(legs.step().await, Transport::Https);
        let probes = prober.probes();
        for _ in 0..5 {
            assert_eq!(legs.step().await, Transport::Https);
        }

        assert_eq!(session.took().len(), 1, "asked once, and believed");
        assert_eq!(prober.probes(), probes, "and not even probed for");
    }

    #[tokio::test]
    async fn a_leg_that_answers_but_carries_no_session_changes_nothing() {
        // A probe is a TCP connect; a session is a negotiation and a login on
        // top. The first can succeed where the second does not, and the
        // mount must stay on the leg that works rather than report one it is
        // not on.
        let prober = FakeProber::answering(false);
        let tunnel = FakeTunnel::new(true);
        let (legs, session, chain) = legs(&prober, &tunnel);
        assert_eq!(legs.step().await, Transport::SmbOverVpn);

        prober.starts_answering();
        session.fails.store(true, Ordering::SeqCst);
        assert_eq!(legs.step().await, Transport::SmbOverVpn);
        assert_eq!(
            chain.current().map(|r| r.transport),
            Some(Transport::SmbOverVpn),
            "not settled on a leg nothing is using"
        );
    }

    /// Regression: a tunnel that came up, and a 445 that answered inside it,
    /// counted as a tunnel that worked — even when no session could be built
    /// on what it carried. Nothing recorded the failure, so the tunnel's own
    /// interval never applied, and every minute's look raised a fresh tunnel:
    /// an OpenVPN handshake and a directory login each time, for the life of
    /// the mount.
    #[tokio::test(start_paused = true)]
    async fn a_tunnel_that_carries_no_session_waits_out_its_interval() {
        let prober = FakeProber::answering(false);
        let tunnel = FakeTunnel::new(true);
        let (legs, session, _chain) = legs(&prober, &tunnel);
        session.fails.store(true, Ordering::SeqCst);

        for _ in 0..3 {
            assert_eq!(legs.step().await, Transport::Https);
            tokio::time::advance(DEFAULT_WATCH_INTERVAL).await;
        }
        assert_eq!(tunnel.opens(), 1, "raised once, and not every minute");
        assert_eq!(prober.probes(), 3, "while the cheap probe carries on");

        tokio::time::advance(synology_filestation_connect::DEFAULT_TUNNEL_RECHECK).await;
        legs.step().await;
        assert_eq!(tunnel.opens(), 2, "and again once its interval is up");
    }

    #[tokio::test]
    async fn a_session_that_died_is_reported_and_brought_back() {
        let prober = FakeProber::answering(true);
        let tunnel = FakeTunnel::new(false);
        let (legs, session, _chain) = legs(&prober, &tunnel);
        assert_eq!(legs.step().await, Transport::SmbDirect);

        session.dies();
        assert_eq!(
            legs.current(),
            Transport::Https,
            "what carries the data now is the fallback, and the badge says so"
        );

        assert_eq!(legs.step().await, Transport::SmbDirect);
        assert_eq!(session.took().len(), 2);
    }

    #[tokio::test]
    async fn a_change_of_leg_is_logged_once() {
        let logs = LogCapture::at_the_default_level();
        let prober = FakeProber::answering(false);
        let tunnel = FakeTunnel::new(false);
        let (legs, _session, _chain) = legs(&prober, &tunnel);

        legs.step().await;
        prober.starts_answering();
        legs.step().await;
        legs.step().await;

        let text = logs.text();
        assert_eq!(
            text.matches("Transport: now SMB, was HTTPS").count(),
            1,
            "said when it happened, and not again: {text}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_looks_again_on_its_own() {
        let prober = FakeProber::answering(false);
        let tunnel = FakeTunnel::new(false);
        let (legs, _session, _chain) = legs(&prober, &tunnel);
        legs.step().await;

        let watching = legs.watch(&tokio::runtime::Handle::current(), DEFAULT_WATCH_INTERVAL);
        prober.starts_answering();
        tokio::time::advance(DEFAULT_WATCH_INTERVAL + Duration::from_secs(1)).await;
        for _ in 0..100 {
            if legs.current() == Transport::SmbDirect {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(legs.current(), Transport::SmbDirect);
        watching.abort();
    }
}
