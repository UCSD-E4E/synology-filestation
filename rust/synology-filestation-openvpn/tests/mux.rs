//! Several TCP connections over one tunnel.
//!
//! Loss on the path caps each TCP connection, not the link: one connection
//! from the NAS ran at about 1.7 MB/s over a lossy home uplink, and three at
//! once ran at about 5. A tunnel that carries one connection holds a mount to
//! the first number. So the tunnel is shared: each connection keeps its own
//! stack, and what arrives is handed to the connection it is addressed to.
//!
//! The far end here is an in-process interface that accepts several
//! connections on the NAS's port and echoes each one back, which is enough to
//! tell whether bytes reach the connection they were sent on.

use std::net::Ipv4Addr;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
use synology_filestation_openvpn::{Ifconfig, LinkFailure, Mtu, Mux, TunnelDevice};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const NAS: (Ipv4Addr, u16) = (Ipv4Addr::new(10, 90, 24, 1), 445);

const OURS: Ifconfig = Ifconfig {
    address: Ipv4Addr::new(10, 90, 24, 6),
    prefix: 24,
};

const PATIENCE: Duration = Duration::from_secs(5);

/// An interface that accepts up to `listeners` connections on the NAS's port
/// and sends back whatever each one says. Returns when the link goes away.
fn echo_peer(
    outbound: mpsc::Sender<Vec<u8>>,
    mut inbound: mpsc::Receiver<Vec<u8>>,
    listeners: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut device = TunnelDevice::new(outbound);
        let mut interface = Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut device,
            Instant::from_micros(0),
        );
        interface.update_ip_addrs(|addresses| {
            let _ = addresses.push(IpCidr::new(IpAddress::Ipv4(NAS.0), 24));
        });
        let mut sockets = SocketSet::new(Vec::new());
        let handles: Vec<SocketHandle> = (0..listeners)
            .map(|_| {
                let mut socket = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
                    tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
                );
                socket.listen(NAS.1).expect("a free port");
                sockets.add(socket)
            })
            .collect();

        let mut clock = 0i64;
        loop {
            loop {
                match inbound.try_recv() {
                    Ok(packet) => device.push(packet),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    // Everyone on our side has let go of the link.
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            clock += 1_000;
            interface.poll(Instant::from_micros(clock), &mut device, &mut sockets);
            for handle in &handles {
                let socket = sockets.get_mut::<tcp::Socket>(*handle);
                let mut buffer = [0u8; 4096];
                while socket.can_recv() && socket.can_send() {
                    let room = socket.send_capacity() - socket.send_queue();
                    let take = room.min(buffer.len());
                    if take == 0 {
                        break;
                    }
                    match socket.recv_slice(&mut buffer[..take]) {
                        Ok(0) | Err(_) => break,
                        Ok(len) => {
                            let _ = socket.send_slice(&buffer[..len]);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
}

/// A mux over a link whose far end echoes, and the far end's task.
fn linked(listeners: usize) -> (Mux, tokio::task::JoinHandle<()>) {
    let (to_peer, peer_inbound) = mpsc::channel(64);
    let (to_us, our_inbound) = mpsc::channel(64);
    let peer = echo_peer(to_us, peer_inbound, listeners);
    let mux = Mux::new(
        to_peer,
        our_inbound,
        OURS,
        Mtu::default(),
        LinkFailure::new(),
    );
    (mux, peer)
}

async fn round_trip(stream: &mut synology_filestation_openvpn::TunnelStream, said: &[u8]) {
    stream.write_all(said).await.expect("sent");
    let mut back = vec![0u8; said.len()];
    tokio::time::timeout(PATIENCE, stream.read_exact(&mut back))
        .await
        .expect("answered in time")
        .expect("readable");
    assert_eq!(back, said, "each connection hears its own echo");
}

#[tokio::test]
async fn two_connections_over_one_link_each_get_their_own_bytes() {
    let (mux, _peer) = linked(2);
    let mut a = mux.connect(NAS, PATIENCE).await.expect("first connection");
    let mut b = mux.connect(NAS, PATIENCE).await.expect("second connection");

    round_trip(&mut a, b"for the first").await;
    round_trip(&mut b, b"and for the second").await;
    round_trip(&mut a, b"first again").await;
}

#[tokio::test]
async fn connections_carry_bulk_at_the_same_time() {
    // The reason for all this: transfers on several connections at once, with
    // their segments interleaved on the one link, each arriving whole.
    let (mux, _peer) = linked(3);
    let mut tasks = Vec::new();
    for n in 0..3u8 {
        let mut stream = mux.connect(NAS, PATIENCE).await.expect("connected");
        tasks.push(tokio::spawn(async move {
            let sent: Vec<u8> = (0..256 * 1024u32)
                .map(|i| (i as u8).wrapping_mul(n + 1))
                .collect();
            let (mut reader, mut writer) = tokio::io::split(&mut stream);
            let writing = async {
                writer.write_all(&sent).await.expect("written");
            };
            let mut back = vec![0u8; sent.len()];
            let reading = async {
                reader.read_exact(&mut back).await.expect("read back");
            };
            tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(writing, reading)
            })
            .await
            .expect("in reasonable time");
            assert!(back == sent, "connection {n} got its own bytes back");
        }));
    }
    for task in tasks {
        task.await.expect("a transfer finished");
    }
}

#[tokio::test]
async fn closing_one_connection_leaves_the_others_working() {
    let (mux, _peer) = linked(2);
    let mut a = mux.connect(NAS, PATIENCE).await.expect("first");
    let mut b = mux.connect(NAS, PATIENCE).await.expect("second");
    round_trip(&mut a, b"before").await;

    drop(a);

    round_trip(&mut b, b"after the other went").await;
}

#[tokio::test]
async fn the_link_stays_while_a_connection_uses_it_and_goes_with_the_last() {
    // A tunnel is an authenticated session on the NAS. It is kept for as long
    // as something is carried on it, and not a moment longer.
    let (mux, peer) = linked(1);
    let mut stream = mux.connect(NAS, PATIENCE).await.expect("connected");

    drop(mux);
    round_trip(&mut stream, b"the mux handle is gone, the link is not").await;
    assert!(!peer.is_finished(), "the link is still up");

    drop(stream);
    tokio::time::timeout(PATIENCE, peer)
        .await
        .expect("the link closes once nothing uses it")
        .expect("the peer ended cleanly");
}

#[tokio::test]
async fn a_weak_handle_finds_the_link_only_while_something_holds_it() {
    let (mux, _peer) = linked(2);
    let weak = mux.downgrade();
    let stream = mux.connect(NAS, PATIENCE).await.expect("connected");
    drop(mux);

    let again = weak.upgrade().expect("a connection still holds the link");
    let mut second = again
        .connect(NAS, PATIENCE)
        .await
        .expect("and it opens another");
    round_trip(&mut second, b"through the same link").await;

    drop((again, second, stream));
    assert!(weak.upgrade().is_none(), "nothing holds it any more");
}

#[tokio::test]
async fn connections_get_different_local_ports() {
    // The only thing that tells their packets apart.
    let (to_peer, mut peer_inbound) = mpsc::channel(64);
    let (_to_us, our_inbound) = mpsc::channel(64);
    let mux = Mux::new(
        to_peer,
        our_inbound,
        OURS,
        Mtu::default(),
        LinkFailure::new(),
    );

    let short = Duration::from_millis(300);
    let first = tokio::spawn({
        let mux = mux.clone();
        async move { mux.connect(NAS, short).await }
    });
    let second = tokio::spawn({
        let mux = mux.clone();
        async move { mux.connect(NAS, short).await }
    });

    let mut ports = std::collections::HashSet::new();
    while ports.len() < 2 {
        let packet = tokio::time::timeout(PATIENCE, peer_inbound.recv())
            .await
            .expect("a SYN goes out")
            .expect("the link is open");
        let header = usize::from(packet[0] & 0x0f) * 4;
        ports.insert(u16::from_be_bytes([packet[header], packet[header + 1]]));
    }
    assert_eq!(ports.len(), 2, "two connections, two source ports");
    let _ = (first.await, second.await);
}
