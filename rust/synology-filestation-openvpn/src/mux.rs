//! Several TCP connections over one tunnel.
//!
//! A tunnel used to carry exactly one connection, and over a lossy path that
//! is a speed limit nobody chose: TCP's response to loss is per connection, so
//! one connection from the NAS held a mount to about 1.7 MB/s on a home uplink
//! that carried three times that over three connections. Raising a tunnel per
//! connection would get there too, at the price of a VPN login and a session
//! on the NAS for each.
//!
//! So connections share the tunnel instead. Each keeps the stack it always
//! had — its own `smoltcp` interface, its own task — and all of them send into
//! the one link. What comes back is handed to the connection it is addressed
//! to, by TCP destination port, which is the one thing that differs between
//! them: they share our address and dial the same host and port.
//!
//! The link is kept for as long as any connection uses it, and goes with the
//! last. It is an authenticated session on the NAS, so it is not something to
//! leave open for nobody.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;

use crate::ip::Ifconfig;
use crate::stack::{LinkFailure, TunnelStream};
use crate::Error;

/// How many packets may wait for one connection's stack to take them.
///
/// The same depth the link had when it fed a single stack: past it a packet is
/// dropped, as a link drops one, and TCP sends it again.
const QUEUE_DEPTH: usize = 64;

/// Where each connection's packets go, by its local port.
type Routes = Arc<Mutex<HashMap<u16, mpsc::Sender<Vec<u8>>>>>;

/// One tunnel, carrying as many TCP connections as are opened on it.
///
/// Cheap to clone; every clone is the same link.
#[derive(Clone)]
pub struct Mux {
    shared: Arc<Shared>,
}

/// A handle that finds the link only while something is still using it.
///
/// For a caller that wants to reuse a tunnel without being the reason it stays
/// up: holding a [`Mux`] would keep an idle tunnel, and its session on the NAS,
/// alive for nothing.
#[derive(Clone, Default)]
pub struct WeakMux(Weak<Shared>);

struct Shared {
    /// Into the tunnel. Every connection's stack sends here.
    outbound: mpsc::Sender<Vec<u8>>,
    routes: Routes,
    ifconfig: Ifconfig,
    /// Why the tunnel stopped, once it has.
    failure: LinkFailure,
    demux: JoinHandle<()>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.demux.abort();
    }
}

impl Mux {
    /// Share a link: packets for it go to `outbound`, and packets arriving on
    /// `inbound` are handed to whichever connection they are addressed to.
    ///
    /// `failure` is where the layer below records why it stopped, so that a
    /// connection that breaks can say why rather than only that it did.
    pub fn new(
        outbound: mpsc::Sender<Vec<u8>>,
        inbound: mpsc::Receiver<Vec<u8>>,
        ifconfig: Ifconfig,
        failure: LinkFailure,
    ) -> Self {
        let routes: Routes = Arc::default();
        let demux = tokio::spawn(demux(inbound, routes.clone()));
        Self {
            shared: Arc::new(Shared {
                outbound,
                routes,
                ifconfig,
                failure,
                demux,
            }),
        }
    }

    /// Open another connection over the link, and return once it is up.
    pub async fn connect(
        &self,
        remote: (Ipv4Addr, u16),
        patience: Duration,
    ) -> Result<TunnelStream, Error> {
        // A link that has already stopped knows why, and that is the reason
        // worth giving — not a connection attempt nobody answers.
        if let Some(stopped) = self.failure() {
            return Err(stopped);
        }
        let shared = &self.shared;
        let (to_connection, inbound) = mpsc::channel(QUEUE_DEPTH);
        // Claimed before the SYN goes out, or the answer to it has nowhere to
        // go.
        let port = claim_port(&shared.routes, to_connection);
        match TunnelStream::connect_from(
            shared.outbound.clone(),
            inbound,
            shared.ifconfig,
            remote,
            port,
            patience,
        )
        .await
        {
            Err(unanswered) => {
                lock(&shared.routes).remove(&port);
                // The tunnel may have stopped while the connection was being
                // made, in which case what happened is not that nobody
                // answered.
                Err(self.failure().unwrap_or(unanswered))
            }
            Ok(stream) => Ok(stream
                .explaining(shared.failure.clone())
                .holding(self.shared.clone())),
        }
    }

    /// Why the tunnel stopped, if it has.
    pub fn failure(&self) -> Option<Error> {
        self.shared.failure.reason()
    }

    /// Whether the link is still carrying: nothing has said it stopped, and
    /// packets from it are still being handed out.
    pub fn is_running(&self) -> bool {
        self.failure().is_none() && !self.shared.demux.is_finished()
    }

    /// A handle that does not keep the link up.
    pub fn downgrade(&self) -> WeakMux {
        WeakMux(Arc::downgrade(&self.shared))
    }
}

impl WeakMux {
    /// The link, if anything is still using it.
    pub fn upgrade(&self) -> Option<Mux> {
        self.0.upgrade().map(|shared| Mux { shared })
    }
}

fn lock(routes: &Routes) -> std::sync::MutexGuard<'_, HashMap<u16, mpsc::Sender<Vec<u8>>>> {
    routes.lock().unwrap_or_else(|e| e.into_inner())
}

/// A local port no live connection on this link is using, now routed to `to`.
fn claim_port(routes: &Routes, to: mpsc::Sender<Vec<u8>>) -> u16 {
    let mut routes = lock(routes);
    // Connections that have gone leave their route behind until something
    // notices; this is a good moment to.
    routes.retain(|_, sender| !sender.is_closed());
    loop {
        let port = 49152 + (rand::random::<u16>() % 16000);
        if let std::collections::hash_map::Entry::Vacant(free) = routes.entry(port) {
            free.insert(to);
            return port;
        }
    }
}

/// Hand each packet from the link to the connection it is addressed to.
///
/// A packet for no connection is dropped: it belongs to one that has gone, and
/// handing it to another would put foreign bytes into that connection's
/// stack. When the link ends every route goes with it, which is how each
/// connection learns the tunnel underneath it stopped.
async fn demux(mut inbound: mpsc::Receiver<Vec<u8>>, routes: Routes) {
    while let Some(packet) = inbound.recv().await {
        let Some(port) = destination_port(&packet) else {
            continue;
        };
        let mut routes = lock(&routes);
        if let Some(to) = routes.get(&port) {
            match to.try_send(packet) {
                // Full is dropped rather than waited on: this is a link, and a
                // link that blocks for one connection stops carrying all of
                // them. TCP asks again.
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => {
                    routes.remove(&port);
                }
            }
        }
    }
    lock(&routes).clear();
}

/// The TCP destination port of an IPv4 packet, if it is one we can route.
///
/// `None` for anything that is not TCP over IPv4, and for a fragment after the
/// first, which carries no TCP header. The stack has no reassembly, so such a
/// fragment was never going to be read by anyone.
fn destination_port(packet: &[u8]) -> Option<u16> {
    const TCP: u8 = 6;
    let first = *packet.first()?;
    if first >> 4 != 4 {
        return None;
    }
    let header = usize::from(first & 0x0f) * 4;
    if header < 20 || packet.len() < header + 4 || packet[9] != TCP {
        return None;
    }
    let fragment_offset = u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff;
    if fragment_offset != 0 {
        return None;
    }
    Some(u16::from_be_bytes([packet[header + 2], packet[header + 3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv4 header of `header_len` bytes carrying `protocol`, followed by
    /// the start of a TCP header from port 445 to `to`.
    fn packet(header_len: usize, protocol: u8, fragment: u16, to: u16) -> Vec<u8> {
        let mut p = vec![0u8; header_len + 20];
        p[0] = 0x40 | (header_len / 4) as u8;
        p[6..8].copy_from_slice(&fragment.to_be_bytes());
        p[9] = protocol;
        p[header_len..header_len + 2].copy_from_slice(&445u16.to_be_bytes());
        p[header_len + 2..header_len + 4].copy_from_slice(&to.to_be_bytes());
        p
    }

    #[test]
    fn a_tcp_packet_is_routed_by_its_destination_port() {
        assert_eq!(destination_port(&packet(20, 6, 0, 50000)), Some(50000));
    }

    #[test]
    fn ip_options_move_the_tcp_header_and_are_allowed_for() {
        assert_eq!(destination_port(&packet(24, 6, 0, 50001)), Some(50001));
    }

    #[test]
    fn what_is_not_tcp_over_ipv4_is_not_routed() {
        assert_eq!(destination_port(&packet(20, 1, 0, 50000)), None, "ICMP");
        assert_eq!(destination_port(&packet(20, 17, 0, 50000)), None, "UDP");
        let mut v6 = packet(20, 6, 0, 50000);
        v6[0] = 0x60;
        assert_eq!(destination_port(&v6), None, "IPv6");
    }

    #[test]
    fn a_later_fragment_has_no_port_to_read() {
        assert_eq!(destination_port(&packet(20, 6, 185, 50000)), None);
        // "More fragments" on the first one still has the header.
        assert_eq!(destination_port(&packet(20, 6, 0x2000, 50000)), Some(50000));
    }

    #[test]
    fn a_packet_too_short_to_hold_the_ports_is_not_read_past_its_end() {
        assert_eq!(destination_port(&[]), None);
        assert_eq!(destination_port(&packet(20, 6, 0, 1)[..22]), None);
        let mut bad = packet(20, 6, 0, 50000);
        bad[0] = 0x44; // a header length below the minimum
        assert_eq!(destination_port(&bad), None);
    }
}
