//! A TCP conversation, end to end, through a tunnel that really encrypts.
//!
//! Every piece of this crate has been tested against something: the wire
//! format against captured bytes, the key derivation against OpenSSL, the
//! protocol against a real `openvpn`, the TCP stack against a peer on an
//! in-memory link. What none of them cover is all of it at once — and the
//! joins are where this kind of thing goes wrong, quietly, in a way that looks
//! like the network.
//!
//! So: a tunnel to a peer over a real socket, a TCP connection opened through
//! it, and bytes going both ways. The peer decrypts what arrives, hands it to
//! a `smoltcp` interface listening where the NAS would be, and encrypts
//! whatever that interface says back. Nothing on this machine routes
//! 10.90.24.1 and nothing is asked to.
//!
//! What it still cannot prove is that a real DSM answers. That is the live
//! pass, and it needs a person.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use common::{Answer, FakeServer, TA_KEY_HEX};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
use synology_filestation_openvpn::{Session, SessionConfig, StaticKey, Tunnel, TunnelDevice, PING};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Where the NAS sits inside the tunnel, and the port that matters.
const NAS: (Ipv4Addr, u16) = (Ipv4Addr::new(10, 90, 24, 1), 445);

/// What e4e-nas actually pushes, keepalives included.
///
/// The keepalives matter here: they share the data channel with the payload,
/// and a client that muddled the two would put a `PING` into the IP stack or
/// hand a TCP segment to the keepalive check. `ping 1` rather than the real
/// `10`, so a test can afford to watch it happen.
const PUSH: &str = "PUSH_REPLY,ifconfig 10.90.24.6 255.255.255.0,peer-id 4,\
                    cipher AES-256-CBC,ping 1,ping-restart 60";

/// The peer id that push hands out, which the peer then addresses its own data
/// packets with — `P_DATA_V2`, as a DSM does.
const PEER_ID: u32 = 4;

/// What the peer noticed, for a test to check afterwards.
#[derive(Clone, Default)]
struct Noticed {
    keepalives: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// A datagram that would not decrypt. Fatal to the exercise, and silence
    /// about it turns a key or replay regression into a timeout twenty seconds
    /// later with nothing to say.
    undecryptable: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// The far end: an OpenVPN peer with a TCP stack behind it.
///
/// Two layers in one task, because that is what a NAS is from here — the thing
/// that terminates the tunnel is also the thing listening on 445.
///
/// Handed its socket already bound, rather than a port to bind: a port that is
/// released and rebound is one something else can take in between, and the
/// collision would surface much later as a handshake nobody answered.
fn spawn_nas(mut server: FakeServer, socket: tokio::net::UdpSocket) -> Noticed {
    let noticed = Noticed::default();
    let noticing = noticed.clone();
    tokio::spawn(async move {
        let noticed = noticing;
        let started = std::time::Instant::now();

        // The tunnel's inside, from the NAS's point of view.
        let (outbound, mut from_stack) = mpsc::channel::<Vec<u8>>(64);
        let mut device = TunnelDevice::new(outbound);
        let mut interface = Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut device,
            Instant::from_micros(0),
        );
        interface.update_ip_addrs(|addresses| {
            let _ = addresses.push(IpCidr::new(IpAddress::Ipv4(NAS.0), 24));
        });

        // A few listeners, so more than one connection can share the tunnel.
        let mut sockets = SocketSet::new(Vec::new());
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let mut listener = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0u8; 16 * 1024]),
                    tcp::SocketBuffer::new(vec![0u8; 16 * 1024]),
                );
                listener.listen(NAS.1).expect("a free port");
                sockets.add(listener)
            })
            .collect();

        let mut client: Option<SocketAddr> = None;
        let mut datagram = vec![0u8; 4096];

        loop {
            tokio::select! {
                received = socket.recv_from(&mut datagram) => {
                    let Ok((len, from)) = received else { return };
                    client = Some(from);
                    let arrived = &datagram[..len];
                    if Session::is_data(arrived) {
                        match server.decrypt_payload(arrived) {
                            // A keepalive is addressed to the tunnel, not
                            // through it.
                            Ok(payload) if payload == PING => {
                                noticed.keepalives.fetch_add(1, Relaxed);
                            }
                            Ok(payload) => device.push(payload),
                            Err(_) => {
                                noticed.undecryptable.fetch_add(1, Relaxed);
                            }
                        }
                    } else {
                        for answer in server.handle(arrived) {
                            let _ = socket.send_to(&answer, from).await;
                        }
                    }
                }
                // TCP has its own clock: acknowledgements and retransmissions
                // happen on it, not only when something arrives.
                _ = tokio::time::sleep(Duration::from_millis(1)) => {}
            }

            // A real clock, not a loop counter. Retransmission and delayed
            // acknowledgement happen on it, and a counter that always runs
            // ahead of the world tests neither.
            let now = || Instant::from_micros(started.elapsed().as_micros() as i64);
            interface.poll(now(), &mut device, &mut sockets);

            // Whatever the client said, said back to it — proof the bytes
            // survived the whole round trip rather than merely left.
            //
            // Only as much as there is room to send back: ignoring what
            // `send_slice` accepted drops the remainder on the floor, which
            // quietly turns a conversation larger than one buffer into a
            // conversation the size of one buffer.
            for handle in &handles {
                let listening = sockets.get_mut::<tcp::Socket>(*handle);
                let mut buffer = [0u8; 4096];
                while listening.can_recv() && listening.can_send() {
                    let room = listening
                        .send_capacity()
                        .saturating_sub(listening.send_queue())
                        .min(buffer.len());
                    if room == 0 {
                        break;
                    }
                    match listening.recv_slice(&mut buffer[..room]) {
                        Ok(0) | Err(_) => break,
                        Ok(len) => {
                            let answer: Vec<u8> = buffer[..len]
                                .iter()
                                .map(|byte| byte.to_ascii_uppercase())
                                .collect();
                            let sent = listening.send_slice(&answer).unwrap_or(0);
                            assert_eq!(sent, answer.len(), "there was room, and it was taken");
                        }
                    }
                }
            }

            interface.poll(now(), &mut device, &mut sockets);

            // And out through the tunnel, encrypted like anything else.
            while let Ok(packet) = from_stack.try_recv() {
                if let Some(to) = client {
                    let sealed = server.encrypt_payload(&packet);
                    let _ = socket.send_to(&sealed, to).await;
                }
            }
        }
    });
    noticed
}

#[tokio::test]
async fn a_conversation_completes_through_the_tunnel() {
    let server = FakeServer::with_peer_id(Answer::KeyMaterialThen(PUSH.to_string()), PEER_ID);
    let mut config = SessionConfig::new(
        server.ca_pem.clone(),
        "localhost",
        StaticKey::from_hex(TA_KEY_HEX).expect("test vector"),
    );
    config.tls_timeout = Duration::from_millis(100);

    // Bound before it is handed over, so nothing can take the port in between.
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a local socket");
    let remote: SocketAddr = socket.local_addr().expect("bound");
    let noticed = spawn_nas(server, socket);

    let tunnel = Tunnel::connect(config, remote).await.expect("a tunnel");
    let mut stream = tunnel
        .open_stream(NAS, Duration::from_secs(20))
        .await
        .expect("a connection to the NAS inside the tunnel");

    // More than one segment, and more than the far end can hold at once: a
    // conversation that fits in a single buffer proves only that a single
    // buffer works, and SMB does not send 18 bytes at a time.
    let said: Vec<u8> = (0..64 * 1024)
        .map(|i| b"smb2 would go here "[i % 19])
        .collect();
    let expected: Vec<u8> = said.iter().map(|b| b.to_ascii_uppercase()).collect();

    let writing = {
        let said = said.clone();
        tokio::spawn(async move {
            stream.write_all(&said).await.expect("written");
            let mut heard = vec![0u8; said.len()];
            stream.read_exact(&mut heard).await.expect("read");
            heard
        })
    };

    let heard = tokio::time::timeout(Duration::from_secs(60), writing)
        .await
        .expect("in reasonable time")
        .expect("the task finished");

    assert_eq!(
        heard.len(),
        expected.len(),
        "all of it came back, not one buffer's worth"
    );
    assert_eq!(
        heard, expected,
        "the bytes came back changed, which means they arrived"
    );
    assert_eq!(
        noticed.undecryptable.load(Relaxed),
        0,
        "and every datagram the peer was sent was one it could open"
    );
}

#[tokio::test]
async fn keepalives_and_payload_share_the_channel() {
    // They travel the same way — same keys, same replay window, same opcode —
    // and are for entirely different layers. A client that muddled them would
    // put a `PING` into the IP stack, or hand a TCP segment to the keepalive
    // check and conclude the peer had gone.
    let server = FakeServer::with_peer_id(Answer::KeyMaterialThen(PUSH.to_string()), PEER_ID);
    let mut config = SessionConfig::new(
        server.ca_pem.clone(),
        "localhost",
        StaticKey::from_hex(TA_KEY_HEX).expect("test vector"),
    );
    config.tls_timeout = Duration::from_millis(100);

    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a local socket");
    let remote: SocketAddr = socket.local_addr().expect("bound");
    let noticed = spawn_nas(server, socket);

    let tunnel = Tunnel::connect(config, remote).await.expect("a tunnel");
    let mut stream = tunnel
        .open_stream(NAS, Duration::from_secs(20))
        .await
        .expect("a connection");

    // Long enough for the pushed `ping 1` to fire between the two exchanges.
    for round in 0..2 {
        stream.write_all(b"still here").await.expect("written");
        let mut heard = [0u8; 10];
        tokio::time::timeout(Duration::from_secs(20), stream.read_exact(&mut heard))
            .await
            .expect("in reasonable time")
            .expect("read");
        assert_eq!(&heard, b"STILL HERE", "round {round}");
        if round == 0 {
            tokio::time::sleep(Duration::from_millis(1_400)).await;
        }
    }

    assert!(
        noticed.keepalives.load(Relaxed) > 0,
        "a keepalive went through the same channel as the payload"
    );
    assert_eq!(noticed.undecryptable.load(Relaxed), 0);
}

/// What e4e-nas *actually* pushes, as opposed to what this file assumed.
///
/// The second value is the peer, not a mask: OpenVPN 2.5 defaults to
/// `topology net30`, and DSM runs 2.5. Every test here used a mask, which put
/// the client on a `/24` containing the NAS and so could never exercise the
/// case the live tunnel hits.
const PUSH_NET30: &str = "PUSH_REPLY,ifconfig 10.90.24.6 10.90.24.5,peer-id 4,\
                          cipher AES-256-CBC,ping 1,ping-restart 60";

/// Regression: under `net30` the client is given a four-address block —
/// 10.90.24.4 to .7 — and the NAS at 10.90.24.1 is **not in it**. The stack
/// installed the address and no route at all, so an off-link destination had
/// nowhere to go: the SYN was never emitted, nothing answered, and thirty
/// seconds later the tunnel reported "the connection was refused or reset".
/// It was never refused. Nothing was ever sent.
///
/// This is the whole live failure, and the suite could not see it because
/// every push in it was a `topology subnet` one.
#[tokio::test]
async fn a_net30_tunnel_can_still_reach_the_nas() {
    let server = FakeServer::with_peer_id(Answer::KeyMaterialThen(PUSH_NET30.to_string()), PEER_ID);
    let mut config = SessionConfig::new(
        server.ca_pem.clone(),
        "localhost",
        StaticKey::from_hex(TA_KEY_HEX).expect("test vector"),
    );
    config.tls_timeout = Duration::from_millis(100);

    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a local socket");
    let remote: SocketAddr = socket.local_addr().expect("bound");
    let noticed = spawn_nas(server, socket);

    let tunnel = Tunnel::connect(config, remote).await.expect("a tunnel");
    let mut stream = tunnel
        .open_stream(NAS, Duration::from_secs(10))
        .await
        .expect("the NAS is off-link under net30, and reached through the peer");

    let said = b"smb2 would go here".to_vec();
    let expected: Vec<u8> = said.iter().map(|b| b.to_ascii_uppercase()).collect();

    stream.write_all(&said).await.expect("written");
    let mut heard = vec![0u8; said.len()];
    tokio::time::timeout(Duration::from_secs(20), stream.read_exact(&mut heard))
        .await
        .expect("in reasonable time")
        .expect("read");

    assert_eq!(heard, expected, "the bytes came back, so they arrived");
    assert_eq!(noticed.undecryptable.load(Relaxed), 0);
}

#[tokio::test]
async fn several_connections_share_one_tunnel() {
    // One VPN login, one session on the NAS, and as many TCP connections over
    // it as a mount wants: loss caps each connection, not the link.
    let server = FakeServer::with_peer_id(Answer::KeyMaterialThen(PUSH.to_string()), PEER_ID);
    let mut config = SessionConfig::new(
        server.ca_pem.clone(),
        "localhost",
        StaticKey::from_hex(TA_KEY_HEX).expect("test vector"),
    );
    config.tls_timeout = Duration::from_millis(100);
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a local socket");
    let remote: SocketAddr = socket.local_addr().expect("bound");
    let noticed = spawn_nas(server, socket);

    let tunnel = Tunnel::connect(config, remote).await.expect("a tunnel");
    let shared = tunnel.share().expect("a tunnel that pushed an address");
    let mut first = shared
        .connect(NAS, Duration::from_secs(20))
        .await
        .expect("the first connection");
    let mut second = shared
        .connect(NAS, Duration::from_secs(20))
        .await
        .expect("a second, over the same tunnel");

    for (stream, said) in [(&mut first, &b"first"[..]), (&mut second, b"second")] {
        stream.write_all(said).await.expect("written");
        let mut heard = vec![0u8; said.len()];
        tokio::time::timeout(Duration::from_secs(20), stream.read_exact(&mut heard))
            .await
            .expect("in reasonable time")
            .expect("read");
        assert_eq!(
            heard,
            said.to_ascii_uppercase(),
            "each heard its own answer"
        );
    }
    assert_eq!(noticed.undecryptable.load(Relaxed), 0);
}
