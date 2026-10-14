//! The escalation, carried out.
//!
//! [`Chain`](crate::Chain) decides that SMB needs a tunnel; this is the thing
//! that raises one. It is the only place in the workspace that knows both
//! halves — the decision layer above and the OpenVPN client below — which is
//! why it lives here rather than in either.
//!
//! Nothing about it touches the operating system's network. The tunnel
//! terminates in this process, a userspace TCP stack sits on it, and what
//! comes back out is a byte stream like any other. No tun device, no
//! `CAP_NET_ADMIN`, no privileged helper, no installer component, and no
//! effect on anything else the machine is doing.
//!
//! ## One tunnel per connection
//!
//! Every call raises a fresh one. That sounds wasteful and is the honest
//! shape: the connection is what the caller asked for, an OpenVPN session
//! carries exactly one here, and a session that has died is usually why the
//! caller is asking again. Keeping a tunnel alive between calls would mean
//! deciding when to tear it down, which is a question nothing here can answer
//! and [`Chain`](crate::Chain) already answers in its own terms.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use synology_filestation_openvpn::{
    credentials, Error as VpnError, Mtu as VpnMtu, Profile, Tunnel as OpenVpn,
};
use tracing::{debug, info};
use zeroize::Zeroizing;

use crate::{Connection, Tunnel, TunnelUnavailable};

/// The tunnel described by a `.ovpn` profile on disk.
pub struct OpenVpnTunnel {
    /// Where the profile is. Read on each attempt rather than held: it is a
    /// shared secret (it embeds `ta.key`), and the copy on disk is the one
    /// [`ProfileSource`](crate::profile::ProfileSource) keeps current.
    profile: PathBuf,
    /// The name sent to the server, already carrying its domain if there is
    /// one. Kept finished rather than as parts: it is what goes on the wire,
    /// and it is what the log has to name.
    login: String,
    password: Zeroizing<String>,
    /// How long the whole thing may take, handshake included.
    patience: Duration,
    /// The largest IP packet either end puts in the tunnel. See
    /// [`OpenVpnTunnel::with_mtu`].
    mtu: VpnMtu,
}

/// Hand-written so the password never reaches a log.
impl std::fmt::Debug for OpenVpnTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenVpnTunnel")
            .field("profile", &self.profile)
            .field("login", &self.login)
            .field("password", &"<redacted>")
            .field("patience", &self.patience)
            .field("mtu", &self.mtu)
            .finish()
    }
}

impl OpenVpnTunnel {
    /// `domain` is the directory the account lives in — `KRG` for an AD user,
    /// `None` for a local one. It is not a separate credential; it decides
    /// what the username has to look like, which is why it is taken here
    /// rather than passed further down.
    pub fn new(
        profile: impl Into<PathBuf>,
        username: impl Into<String>,
        password: impl Into<String>,
        domain: Option<&str>,
        patience: Duration,
    ) -> Self {
        Self {
            profile: profile.into(),
            login: qualified(domain, &username.into()),
            password: Zeroizing::new(password.into()),
            patience,
            mtu: VpnMtu::default(),
        }
    }

    /// Use packets of at most `mtu` bytes inside the tunnel, rather than the
    /// largest that fits what the server pushes.
    ///
    /// The client-side `--mssfix`: the NAS sizes what it sends by the MSS our
    /// SYN offers, so a smaller MTU here keeps its wrapped packets under a path
    /// that would otherwise split each one in two.
    pub fn with_mtu(mut self, mtu: VpnMtu) -> Self {
        self.mtu = mtu;
        self
    }

    /// Raise a tunnel and open a connection through it, by `deadline`.
    async fn raise(
        &self,
        host: Ipv4Addr,
        port: u16,
        deadline: tokio::time::Instant,
    ) -> Result<Connection, TunnelUnavailable> {
        let text = Zeroizing::new(read_profile(&self.profile).await?);
        let profile = Profile::parse(&text).map_err(|e| {
            TunnelUnavailable::Transient(format!(
                "the vpn profile is not one this client can use: {e}"
            ))
        })?;

        let server = profile.remote.clone();
        let dial = (profile.remote.clone(), profile.port);
        let config = profile
            .into_config(Some(credentials(
                self.login.clone(),
                self.password.to_string(),
            )))
            .map_err(|e| {
                TunnelUnavailable::Transient(format!("the vpn profile is incomplete: {e}"))
            })?;

        // Resolved here rather than inside the tunnel, so a name that does not
        // resolve is reported as what it is rather than as a handshake nobody
        // answered.
        let resolved = tokio::net::lookup_host(dial).await.map_err(|e| {
            TunnelUnavailable::Transient(format!("cannot resolve the vpn server {server}: {e}"))
        })?;
        let remote = pick_address(resolved).ok_or_else(|| {
            TunnelUnavailable::Transient(format!("the vpn server {server} resolves to nothing"))
        })?;

        // The login name, because an unqualified one is refused by DSM before
        // the password is looked at and the server says only `AUTH_FAILED`. A
        // name is not a secret; the password is, and never appears.
        debug!(
            "tunnel: raising one to {server} for {host}:{port}, as {}",
            self.login
        );
        let tunnel = OpenVpn::connect(config, remote).await.map_err(|e| {
            let why = format!("the vpn tunnel to {server} did not come up: {e}");
            // A rejected password is not a blip. Every attempt is a real
            // authentication against the domain controller, so a chain that
            // re-probes on a timer works its way to a locked account.
            match e {
                VpnError::AuthFailed(_) => TunnelUnavailable::Refused(why),
                _ => TunnelUnavailable::Transient(why),
            }
        })?;

        // What is left of the budget, not the whole of it again. Handing the
        // full patience here means the outer bound always fires first, so
        // every failure is reported as the tunnel not coming up — erasing the
        // difference between a tunnel that never came up and a NAS not
        // listening behind one, which is the difference this layer exists to
        // draw.
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let stream = tunnel
            .open_stream((host, port), left, self.mtu)
            .await
            .map_err(|e| {
                TunnelUnavailable::Transient(format!(
                    "the tunnel is up, but nothing answered at {host}:{port} inside it: {e}"
                ))
            })?;

        info!(
            "tunnel: up, and {host}:{port} answered through it (MTU {}, MSS {})",
            self.mtu,
            self.mtu.mss()
        );
        Ok(Box::new(stream))
    }
}

#[async_trait]
impl Tunnel for OpenVpnTunnel {
    async fn open(&self, host: &str, port: u16) -> Result<Connection, TunnelUnavailable> {
        // An address, not a name. The tunnel pushes no DNS by design, so a
        // name here would be resolved against the network the tunnel exists to
        // get around — which either fails, or succeeds and points somewhere
        // else entirely.
        let address: Ipv4Addr = host.parse().map_err(|_| {
            TunnelUnavailable::Transient(format!(
                "{host} is not an address, and a tunnel that pushes no DNS has no way to \
                 make it one"
            ))
        })?;

        // The trait asks for a bounded wait, and this is where it is bounded:
        // the pieces below have their own deadlines, but nothing until here
        // knows how long the whole thing may take.
        // A backstop, not the mechanism: the steps inside share this deadline
        // and each says what it was doing, so this only fires if one of them
        // fails to bound itself.
        let deadline = tokio::time::Instant::now() + self.patience;
        match tokio::time::timeout_at(deadline, self.raise(address, port, deadline)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(TunnelUnavailable::Transient(format!(
                "the vpn tunnel did not come up within {:?}",
                self.patience
            ))),
        }
    }
}

/// The login name to send, for an account in `domain`.
///
/// DSM's `vpnauthd` runs a Synology pre-check — `SYNOWinsEnumAllDomains` and
/// the privilege file — *before* the FreeRADIUS site config it then defers to.
/// An unqualified name matches no local account in that pre-check, so it is
/// refused as "Incorrect user name" and never reaches the `Realm` regex that
/// would have accepted either form. The client therefore has to say which
/// directory the account is in, even though the server could work it out.
///
/// A name that already says so is left alone: `KRG\KRG\user` fails the same
/// pre-check, and the domain field and the username field are two places the
/// same person can put the same prefix.
fn qualified(domain: Option<&str>, username: &str) -> String {
    match domain.map(str::trim).filter(|d| !d.is_empty()) {
        Some(domain) if !username.contains('\\') && !username.contains('@') => {
            format!("{domain}\\{username}")
        }
        _ => username.to_string(),
    }
}

/// Which of a name's addresses to dial.
///
/// IPv4 first, then whatever there is. Not a preference about protocols: the
/// resolver's order is its own business, and a AAAA record in front of a
/// working A record would otherwise fail the whole escalation on a machine
/// with no IPv6 route — which describes most of the networks somebody is on
/// when they need this at all.
fn pick_address(resolved: impl Iterator<Item = SocketAddr>) -> Option<SocketAddr> {
    let mut first = None;
    for address in resolved {
        if address.is_ipv4() {
            return Some(address);
        }
        first.get_or_insert(address);
    }
    first
}

/// The profile's text, with a failure that names the file.
async fn read_profile(path: &Path) -> Result<String, TunnelUnavailable> {
    tokio::fs::read_to_string(path).await.map_err(|e| {
        TunnelUnavailable::Transient(format!(
            "cannot read the vpn profile at {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("an address")
    }

    #[test]
    fn an_ipv4_address_is_preferred_wherever_it_comes_in_the_list() {
        // A resolver answering with the AAAA record first is ordinary. On a
        // machine with no IPv6 route, taking whatever came first fails the
        // whole escalation with a working A record sitting right behind it.
        let resolved = [addr("[2001:db8::1]:1194"), addr("10.0.0.1:1194")];

        assert_eq!(
            pick_address(resolved.into_iter()),
            Some(addr("10.0.0.1:1194"))
        );
    }

    #[test]
    fn an_ipv6_only_name_is_still_dialled() {
        // Preferring one is not refusing the other: a server that really is
        // v6-only should be tried, not skipped.
        let resolved = [addr("[2001:db8::1]:1194")];

        assert_eq!(
            pick_address(resolved.into_iter()),
            Some(addr("[2001:db8::1]:1194"))
        );
    }

    #[test]
    fn a_name_that_resolves_to_nothing_has_nothing_to_dial() {
        assert_eq!(pick_address(std::iter::empty()), None);
    }

    /// Regression: e4e-nas rejected `c.crutchfield.642` with a bare
    /// `AUTH_FAILED` before the password was ever looked at. DSM's `vpnauthd`
    /// runs a Synology pre-check — `SYNOWinsEnumAllDomains` plus the privilege
    /// file — ahead of the FreeRADIUS site config, and an unqualified name
    /// matches no local account there, so it is refused as "Incorrect user
    /// name" and never reaches the `Realm` regex that would have handled it.
    /// The domain the SMB leg already carries is the one this needs.
    #[test]
    fn an_ad_login_is_qualified_with_the_domain() {
        assert_eq!(
            qualified(Some("KRG"), "c.crutchfield.642"),
            "KRG\\c.crutchfield.642"
        );
    }

    /// Somebody who typed the prefix themselves has said it once. Saying it
    /// again sends `KRG\KRG\…`, which fails the same pre-check for a new
    /// reason and reads, in a log, exactly like the bug being fixed here.
    #[test]
    fn a_name_that_already_carries_its_domain_is_left_alone() {
        assert_eq!(
            qualified(Some("KRG"), "KRG\\c.crutchfield.642"),
            "KRG\\c.crutchfield.642"
        );
    }

    /// The other form the `Realm` regex handles, and one the pre-check
    /// accepts, so it is qualified already.
    #[test]
    fn a_upn_is_qualified_already() {
        assert_eq!(qualified(Some("KRG"), "user@krg.local"), "user@krg.local");
    }

    /// No domain is a local account on an appliance joined to nothing.
    /// Prefixing an absent one would send `\admin`, which is not a name.
    #[test]
    fn a_login_with_no_domain_is_sent_as_typed() {
        assert_eq!(qualified(None, "admin"), "admin");
        assert_eq!(qualified(Some(""), "admin"), "admin");
        assert_eq!(qualified(Some("  "), "admin"), "admin");
    }

    /// The qualifying has to happen where the tunnel is built, not at one of
    /// the two call sites, or the CLI and the GUI get to disagree about it.
    #[test]
    fn the_tunnel_authenticates_as_the_qualified_name() {
        let tunnel = OpenVpnTunnel::new(
            "/nowhere/e4e-nas-vpn.ovpn",
            "c.crutchfield.642",
            "hunter2",
            Some("KRG"),
            Duration::from_secs(1),
        );

        assert_eq!(tunnel.login, "KRG\\c.crutchfield.642");
    }

    /// The profile embeds `ta.key` and the password is a domain credential.
    /// `Debug` is hand-written precisely so neither reaches a log; the login
    /// name does, because not knowing what was sent is what made the live
    /// failure cost a NAS-side investigation.
    #[test]
    fn debug_shows_the_login_and_never_the_password() {
        let shown = format!(
            "{:?}",
            OpenVpnTunnel::new(
                "/nowhere/e4e-nas-vpn.ovpn",
                "c.crutchfield.642",
                "hunter2",
                Some("KRG"),
                Duration::from_secs(1),
            )
        );

        // `Debug` for a string escapes the separator, so this asserts on the
        // two halves rather than on the form they are printed in.
        assert!(shown.contains("KRG") && shown.contains("c.crutchfield.642"));
        assert!(!shown.contains("hunter2"), "got {shown}");
    }
}
