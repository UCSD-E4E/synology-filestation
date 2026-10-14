//! The driver against a peer in this process.
//!
//! `tests/interop.rs` drives a real `openvpn` and is the stronger check, but
//! it needs the binary and is skipped without it. This one needs nothing but a
//! loopback socket, so it runs everywhere the crate is built — and it is the
//! only place the driver's own error handling is exercised, as opposed to the
//! state machine's.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use common::{Answer, FakeServer, TA_KEY_HEX};
use synology_filestation_openvpn::{Error, Session, SessionConfig, StaticKey, Tunnel};

const PUSH: &str = "PUSH_REPLY,ifconfig 10.90.24.6 255.255.255.0,peer-id 4,cipher AES-256-CBC";

/// A port nothing is listening on, having just been let go of.
fn a_free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("a local socket")
        .local_addr()
        .expect("bound")
        .port()
}

/// Answer whatever arrives until told to stop, then say nothing at all.
///
/// A peer that goes quiet rather than closing is what a tunnel actually meets:
/// UDP has no hang-up, so a NAS that reboots or a route that disappears simply
/// stops answering.
fn serve_until_quiet(
    mut server: FakeServer,
    port: u16,
) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let quiet = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watching = std::sync::Arc::clone(&quiet);
    tokio::spawn(async move {
        let socket = tokio::net::UdpSocket::bind(("127.0.0.1", port))
            .await
            .expect("the port is free");
        let mut buffer = vec![0u8; 4096];
        loop {
            let Ok((len, from)) = socket.recv_from(&mut buffer).await else {
                return;
            };
            if watching.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            for answer in server.handle(&buffer[..len]) {
                let _ = socket.send_to(&answer, from).await;
            }
        }
    });
    quiet
}

/// A tunnel whose peer stops answering shortly after it comes up.
async fn tunnel_that_will_be_abandoned() -> (Tunnel, std::sync::Arc<std::sync::atomic::AtomicBool>)
{
    let server = FakeServer::new(Answer::KeyMaterialThen(PUSH.to_string()));
    let mut config = config_for(&server);
    config.peer_timeout = Some(Duration::from_millis(300));
    let port = a_free_port();
    let quiet = serve_until_quiet(server, port);

    let remote: SocketAddr = ([127, 0, 0, 1], port).into();
    let tunnel = Tunnel::connect(config, remote).await.expect("a tunnel");
    (tunnel, quiet)
}

#[tokio::test]
async fn a_tunnel_that_already_died_is_not_blamed_on_the_far_end() {
    // The failure this exists to prevent. Asked for a connection, a dead
    // tunnel used to answer with a stream that timed out — reported as nobody
    // answering at 10.90.24.1:445, which blames the NAS's SMB port for a VPN
    // that was not there. The reason is known; it should be the one given.
    let (tunnel, quiet) = tunnel_that_will_be_abandoned().await;
    quiet.store(true, std::sync::atomic::Ordering::SeqCst);

    // Wait for the tunnel to notice.
    for _ in 0..100 {
        if tunnel.failure().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(matches!(tunnel.failure(), Some(Error::PeerGone(_))));

    let refused = tunnel
        .open_stream(
            (Ipv4Addr::new(10, 90, 24, 1), 445),
            Duration::from_secs(5),
            synology_filestation_openvpn::Mtu::default(),
        )
        .await;

    assert!(
        matches!(refused, Err(Error::PeerGone(_))),
        "the tunnel's own reason, not the stream's guess: {refused:?}",
        refused = refused.map(|_| ())
    );
}

#[tokio::test]
async fn a_tunnel_that_dies_while_connecting_says_what_died() {
    // The same distinction, arrived at from the other direction: the tunnel is
    // alive when asked and gone before the connection can be made. A patience
    // timeout would name the address inside the tunnel; what actually happened
    // is that the tunnel stopped.
    let (tunnel, quiet) = tunnel_that_will_be_abandoned().await;
    quiet.store(true, std::sync::atomic::Ordering::SeqCst);

    let outcome = tunnel
        .open_stream(
            (Ipv4Addr::new(10, 90, 24, 1), 445),
            Duration::from_secs(3),
            synology_filestation_openvpn::Mtu::default(),
        )
        .await;

    assert!(
        matches!(outcome, Err(Error::PeerGone(_))),
        "what stopped is what is reported: {outcome:?}",
        outcome = outcome.map(|_| ())
    );
}

/// Answer whatever arrives, from `port`, starting `after`.
fn serve_from(mut server: FakeServer, port: u16, after: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let socket = tokio::net::UdpSocket::bind(("127.0.0.1", port))
            .await
            .expect("the port is still free");
        let mut buffer = vec![0u8; 4096];
        loop {
            let Ok((len, from)) = socket.recv_from(&mut buffer).await else {
                return;
            };
            for answer in server.handle(&buffer[..len]) {
                let _ = socket.send_to(&answer, from).await;
            }
        }
    });
}

fn config_for(server: &FakeServer) -> SessionConfig {
    let mut config = SessionConfig::new(
        server.ca_pem.clone(),
        "localhost",
        StaticKey::from_hex(TA_KEY_HEX).expect("test vector"),
    );
    // Retransmit briskly: this test is about what happens between attempts,
    // and the default two seconds is a long time to hold a test open.
    config.tls_timeout = Duration::from_millis(100);
    config
}

#[tokio::test]
async fn a_tunnel_comes_up_over_a_real_socket() {
    let server = FakeServer::new(Answer::KeyMaterialThen(PUSH.to_string()));
    let config = config_for(&server);
    let port = a_free_port();
    serve_from(server, port, Duration::ZERO);

    let remote: SocketAddr = ([127, 0, 0, 1], port).into();
    let tunnel = Tunnel::connect(config, remote)
        .await
        .expect("a handshake over loopback");

    assert!(tunnel.failure().is_none(), "and nothing went wrong after");
}

#[tokio::test]
async fn a_refusal_before_the_peer_is_listening_does_not_end_the_tunnel() {
    // The socket is `connect`ed, so the kernel reports ICMP port-unreachable
    // as `ECONNREFUSED` on the next operation — which is exactly what a client
    // dialling a server that has not finished starting gets back, and what a
    // NAT that has forgotten the binding produces mid-session.
    //
    // Treated as fatal it ends the tunnel permanently on a condition the
    // retransmission layer exists to ride out: the peer here answers a few
    // hundred milliseconds later and everything works.
    let server = FakeServer::new(Answer::KeyMaterialThen(PUSH.to_string()));
    let config = config_for(&server);
    let port = a_free_port();
    serve_from(server, port, Duration::from_millis(400));

    let remote: SocketAddr = ([127, 0, 0, 1], port).into();
    let tunnel = Tunnel::connect(config, remote)
        .await
        .expect("the refusals are not the end of it");

    assert!(tunnel.failure().is_none());
}

#[tokio::test]
async fn a_connection_through_the_tunnel_reaches_the_far_side_encrypted() {
    // The join everything else was for: a tunnel carrying IP, a TCP stack on
    // top of it, and a connection out of that stack whose first packet arrives
    // at the peer as an ordinary encrypted data packet. Nothing on this
    // machine has a route to 10.90.24.1 and nothing needs one.
    let server = FakeServer::new(Answer::KeyMaterialThen(PUSH.to_string()));
    let config = config_for(&server);
    let port = a_free_port();

    // The peer, answering control packets and reporting what it decrypts.
    let (arrived, mut arriving) = tokio::sync::mpsc::channel(16);
    let mut server = server;
    tokio::spawn(async move {
        let socket = tokio::net::UdpSocket::bind(("127.0.0.1", port))
            .await
            .expect("the port is free");
        let mut buffer = vec![0u8; 4096];
        loop {
            let Ok((len, from)) = socket.recv_from(&mut buffer).await else {
                return;
            };
            let datagram = &buffer[..len];
            if Session::is_data(datagram) {
                if let Ok(payload) = server.decrypt_payload(datagram) {
                    if arrived.send(payload).await.is_err() {
                        return;
                    }
                }
                continue;
            }
            for answer in server.handle(datagram) {
                let _ = socket.send_to(&answer, from).await;
            }
        }
    });

    let remote: SocketAddr = ([127, 0, 0, 1], port).into();
    let tunnel = Tunnel::connect(config, remote).await.expect("a tunnel");

    assert_eq!(
        tunnel
            .ifconfig()
            .expect("the server said where we are")
            .address,
        Ipv4Addr::new(10, 90, 24, 6),
        "the address from the push reply, not one we chose"
    );

    // Nothing is listening inside, so this cannot complete — what matters is
    // what leaves.
    let _ = tokio::time::timeout(
        Duration::from_millis(600),
        tunnel.open_stream(
            (Ipv4Addr::new(10, 90, 24, 1), 445),
            Duration::from_millis(500),
            synology_filestation_openvpn::Mtu::default(),
        ),
    )
    .await;

    // Keepalives are not it; the first thing carrying IP is.
    let packet = loop {
        let payload = tokio::time::timeout(Duration::from_secs(10), arriving.recv())
            .await
            .expect("something was sent through it")
            .expect("a payload");
        if payload.first().map(|first| first >> 4) == Some(4) {
            break payload;
        }
    };

    assert_eq!(packet[9], 6, "carrying TCP");
    assert_eq!(&packet[12..16], &[10, 90, 24, 6], "from where we were put");
    assert_eq!(&packet[16..20], &[10, 90, 24, 1], "to the NAS");

    let header_len = ((packet[0] & 0x0f) as usize) * 4;
    let tcp = &packet[header_len..];
    assert_eq!(u16::from_be_bytes([tcp[2], tcp[3]]), 445, "to the SMB port");
    assert_eq!(tcp[13] & 0x3f, 0x02, "a bare SYN");
}

#[tokio::test]
async fn a_datagram_that_will_not_send_does_not_end_the_tunnel() {
    // What killed a copy. A megabyte of SMB write is a burst of packets, and a
    // burst is what fills an interface queue — at which point the kernel
    // refuses a datagram with `ENOBUFS`, which has no `ErrorKind` of its own
    // and arrives as `Uncategorized`. Anything not on a list of five
    // recognised kinds used to end the session, so the tunnel died a few
    // milliseconds into the write and every layer above reported something
    // true and useless.
    //
    // Provoked here by sending a datagram larger than the socket will take:
    // `EMSGSIZE`, also uncategorised, also not the tunnel's fault.
    let server = FakeServer::new(Answer::KeyMaterialThen(PUSH.to_string()));
    let config = config_for(&server);
    let port = a_free_port();
    serve_from(server, port, Duration::ZERO);

    let remote: SocketAddr = ([127, 0, 0, 1], port).into();
    let tunnel = Tunnel::connect(config, remote).await.expect("a tunnel");

    // Far larger than any datagram a socket will carry.
    let enormous = vec![0u8; 200 * 1024];
    let _ = tunnel.send(enormous).await;

    // The refusal is about that datagram, not about the tunnel: an ordinary
    // payload still goes, and nothing has stopped.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        tunnel.failure().is_none(),
        "one datagram the kernel would not take is not the end of a session: {:?}",
        tunnel.failure()
    );
    tunnel
        .send(b"and this still goes".to_vec())
        .await
        .expect("still carrying");
}
