//! A TCP conversation over the tunnel's own device.
//!
//! The tunnel carries IP packets and nothing on the machine knows what to do
//! with them, which is exactly the arrangement that needs no tun device and no
//! privileges — and exactly the arrangement where a mistake shows up as
//! silence. So the device is given a peer: a second `smoltcp` interface on the
//! other end of an in-memory link, listening where the NAS would.
//!
//! What this proves is the wiring — layer three with no ethernet header, the
//! address the server assigned us, packets going out and coming back, and the
//! stream semantics anything above is entitled to expect. What it cannot prove
//! is that a real DSM answers, which is the live pass.

use std::net::Ipv4Addr;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
use synology_filestation_openvpn::{Error, Ifconfig, LinkFailure, Mtu, TunnelDevice, TunnelStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Where the NAS sits inside the tunnel, and the port that matters.
const NAS: (Ipv4Addr, u16) = (Ipv4Addr::new(10, 90, 24, 1), 445);

const OURS: Ifconfig = Ifconfig {
    address: Ipv4Addr::new(10, 90, 24, 6),
    prefix: 24,
};

/// Long enough for a handshake over a link with no latency at all.
const PATIENCE: Duration = Duration::from_secs(5);

/// What the peer has been asked to do next.
enum Ask {
    Send(Vec<u8>),
    Close,
    /// Stop and start answering, without going away. A stalled peer
    /// acknowledges nothing, which is how a write is held in flight for as
    /// long as a test needs it there.
    Stall(bool),
    /// Go away without a word, as a link does when it fails.
    Vanish,
}

/// The other end of the link: an interface that listens where the NAS would,
/// running on its own task because the stream under test now drives itself.
struct Peer {
    device: TunnelDevice,
    interface: Interface,
    sockets: SocketSet<'static>,
    handle: smoltcp::iface::SocketHandle,
    inbound: mpsc::Receiver<Vec<u8>>,
    clock: i64,
}

impl Peer {
    fn listening(outbound: mpsc::Sender<Vec<u8>>, inbound: mpsc::Receiver<Vec<u8>>) -> Self {
        let mut device = TunnelDevice::new(outbound);
        let mut interface = Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut device,
            Instant::from_micros(0),
        );
        interface.update_ip_addrs(|addresses| {
            let _ = addresses.push(IpCidr::new(IpAddress::Ipv4(NAS.0), 24));
        });

        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 16 * 1024]),
            tcp::SocketBuffer::new(vec![0u8; 16 * 1024]),
        );
        socket.listen(NAS.1).expect("a free port");

        let mut sockets = SocketSet::new(Vec::new());
        let handle = sockets.add(socket);

        Self {
            device,
            interface,
            sockets,
            handle,
            inbound,
            clock: 0,
        }
    }

    fn poll(&mut self) {
        self.clock += 1_000;
        self.interface.poll(
            Instant::from_micros(self.clock),
            &mut self.device,
            &mut self.sockets,
        );
    }

    fn socket(&mut self) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(self.handle)
    }

    /// Run until told to stop, doing as asked and reporting what arrives.
    fn spawn(mut self) -> (mpsc::Sender<Ask>, mpsc::Receiver<Vec<u8>>) {
        let (ask, mut asked) = mpsc::channel::<Ask>(64);
        let (heard, hearing) = mpsc::channel::<Vec<u8>>(64);

        tokio::spawn(async move {
            let mut queued: Vec<u8> = Vec::new();
            let mut closing = false;
            let mut stalled = false;
            let mut established = false;
            loop {
                while let Ok(packet) = self.inbound.try_recv() {
                    self.device.push(packet);
                }
                if !stalled {
                    self.poll();
                }
                established |= self.socket().may_send();

                if !queued.is_empty() && self.socket().can_send() {
                    let sent = self.socket().send_slice(&queued).unwrap_or(0);
                    queued.drain(..sent);
                }
                if queued.is_empty() && closing && self.socket().may_send() {
                    self.socket().close();
                }

                let mut buffer = [0u8; 4096];
                while self.socket().can_recv() {
                    match self.socket().recv_slice(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(len) => {
                            if heard.send(buffer[..len].to_vec()).await.is_err() {
                                return;
                            }
                        }
                    }
                }

                // The other side has closed its half. That is an answer to
                // give in kind, not an instruction to stop: it says nothing
                // more is coming, and nothing at all about what is still owed
                // in this direction. So it joins the ordinary close path,
                // which waits until everything queued has gone.
                if established && !self.socket().may_recv() && !self.socket().can_recv() {
                    closing = true;
                }
                // And leaving is for when the exchange is over — letting go of
                // the channel is how a test learns the goodbye arrived.
                if established && !self.socket().is_active() {
                    return;
                }

                // Everything asked for so far, not one thing per turn: an
                // instruction still sitting in the channel is work owed, and
                // a peer that decides it has finished while its backlog is
                // unread stops in the middle of what it was told to do.
                loop {
                    match asked.try_recv() {
                        Ok(Ask::Send(bytes)) => queued.extend_from_slice(&bytes),
                        Ok(Ask::Close) => closing = true,
                        Ok(Ask::Stall(now)) => stalled = now,
                        Ok(Ask::Vanish) => return,
                        // Nothing more will be asked, which is not the same as
                        // being finished: what has been handed to `smoltcp` is
                        // not yet on the wire, and a peer that returns here
                        // drops the link out from under its own last segments.
                        Err(_) => break,
                    }
                }

                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        (ask, hearing)
    }
}

/// A stream connected to a peer that is already listening.
async fn connected() -> (TunnelStream, mpsc::Sender<Ask>, mpsc::Receiver<Vec<u8>>) {
    let (to_peer, peer_inbound) = mpsc::channel(64);
    let (to_us, our_inbound) = mpsc::channel(64);

    let (ask, heard) = Peer::listening(to_us, peer_inbound).spawn();
    let stream = TunnelStream::connect(to_peer, our_inbound, OURS, NAS, PATIENCE, Mtu::default())
        .await
        .expect("the connection comes up");

    (stream, ask, heard)
}

#[tokio::test]
async fn a_connection_is_made_and_carries_bytes_both_ways() {
    let (mut stream, ask, mut heard) = connected().await;

    stream.write_all(b"SMB2 would go here").await.expect("sent");
    assert_eq!(
        heard.recv().await.expect("the far end reads it"),
        b"SMB2 would go here"
    );

    ask.send(Ask::Send(b"and the answer".to_vec()))
        .await
        .expect("the far end speaks");
    let mut answer = [0u8; 14];
    stream.read_exact(&mut answer).await.expect("readable");
    assert_eq!(&answer, b"and the answer");
}

#[tokio::test]
async fn a_read_waits_for_bytes_instead_of_reporting_none() {
    // The whole reason this is an `AsyncRead`. A read that returned zero for
    // "nothing has arrived yet" is indistinguishable from end of stream, and
    // every framing layer above — including the one SMB needs — treats zero as
    // the connection being over. So a read with nothing to give must wait.
    let (mut stream, ask, _heard) = connected().await;

    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_millis(200), stream.read_exact(&mut byte)).await;
    assert!(read.is_err(), "nothing has been sent, so nothing is read");

    ask.send(Ask::Send(b"x".to_vec())).await.expect("queued");
    stream
        .read_exact(&mut byte)
        .await
        .expect("and now there is something");
    assert_eq!(&byte, b"x");
}

#[tokio::test]
async fn the_far_end_hanging_up_is_the_end_of_the_stream() {
    // The other half of the same distinction: when the connection really is
    // over, a read has to say so, or a reader waits for a peer that has gone.
    let (mut stream, ask, _heard) = connected().await;

    ask.send(Ask::Send(b"last words".to_vec()))
        .await
        .expect("queued");
    ask.send(Ask::Close).await.expect("and then goodbye");

    let mut rest = Vec::new();
    let read = tokio::time::timeout(PATIENCE, stream.read_to_end(&mut rest))
        .await
        .expect("the end arrives rather than hanging");

    assert_eq!(read.expect("a clean end"), rest.len());
    assert_eq!(rest, b"last words", "everything sent before the close");
}

#[tokio::test]
async fn bytes_sent_in_pieces_arrive_as_one_stream() {
    // TCP is a byte stream, and the framing layer above this reads a
    // four-byte header and then exactly as many bytes as it named. Both of
    // those cross segment boundaries whenever the far end feels like it.
    let (mut stream, ask, _heard) = connected().await;

    for piece in [&b"one"[..], b"two", b"three"] {
        ask.send(Ask::Send(piece.to_vec())).await.expect("queued");
        // Deliberately spaced, so they cannot arrive as one segment.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let mut all = [0u8; 11];
    tokio::time::timeout(PATIENCE, stream.read_exact(&mut all))
        .await
        .expect("in reasonable time")
        .expect("readable");
    assert_eq!(&all, b"onetwothree");
}

#[tokio::test]
async fn a_reader_that_takes_its_time_loses_nothing() {
    // More than fits in any one buffer between the stack and the reader. A
    // link drops what it cannot carry; a stream must not, and the difference
    // is the whole reason there is a window.
    let (mut stream, ask, _heard) = connected().await;

    // Bigger than the window, which is what makes this about the window: it
    // is now sized not to be the thing limiting a transfer, so a couple of
    // hundred kilobytes would simply sit in the buffer and prove nothing.
    let sent: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let sending = {
        let sent = sent.clone();
        tokio::spawn(async move {
            for chunk in sent.chunks(8 * 1024) {
                if ask.send(Ask::Send(chunk.to_vec())).await.is_err() {
                    return;
                }
            }
            let _ = ask.send(Ask::Close).await;
        })
    };

    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut received))
        .await
        .expect("in reasonable time")
        .expect("readable");
    sending.await.expect("the sender finished");

    assert_eq!(received.len(), sent.len(), "nothing was dropped");
    assert_eq!(received, sent, "and nothing was reordered");
}

#[tokio::test]
async fn the_first_thing_out_is_a_syn_to_where_we_were_told() {
    // Before any peer answers: the packets a connection attempt puts on the
    // tunnel should be addressed from where the server said we are, to where
    // we were asked to go. Getting the source wrong is the sort of thing that
    // works locally and fails on the NAS, where the address was assigned for a
    // reason.
    let (to_peer, mut peer_inbound) = mpsc::channel(64);
    let (_to_us, our_inbound) = mpsc::channel(64);

    let attempt = tokio::spawn(TunnelStream::connect(
        to_peer,
        our_inbound,
        Ifconfig {
            address: Ipv4Addr::new(10, 90, 24, 6),
            prefix: 30,
        },
        NAS,
        Duration::from_millis(200),
        Mtu::default(),
    ));

    let packet = tokio::time::timeout(PATIENCE, peer_inbound.recv())
        .await
        .expect("something went out")
        .expect("a packet");

    // An IPv4 header with no options, then TCP.
    assert_eq!(packet[0] >> 4, 4, "IPv4");
    assert_eq!(packet[9], 6, "carrying TCP");
    assert_eq!(
        &packet[12..16],
        &[10, 90, 24, 6],
        "from the address we were given"
    );
    assert_eq!(&packet[16..20], &[10, 90, 24, 1], "to the NAS");

    let header_len = ((packet[0] & 0x0f) as usize) * 4;
    let tcp = &packet[header_len..];
    assert_eq!(u16::from_be_bytes([tcp[2], tcp[3]]), 445, "to the SMB port");
    // Flags live in the low six bits of byte 13; SYN is bit 1, and nothing
    // else should be set on a first attempt.
    assert_eq!(tcp[13] & 0x3f, 0x02, "a bare SYN");

    // And nobody answered it, which is a failure rather than a wait forever.
    assert!(attempt.await.expect("the task finished").is_err());
}

/// The MSS a SYN offers, read from its TCP options.
fn offered_mss(packet: &[u8]) -> Option<u16> {
    let header_len = ((packet[0] & 0x0f) as usize) * 4;
    let tcp = &packet[header_len..];
    let options = &tcp[20..((tcp[12] >> 4) as usize) * 4];
    let mut at = 0;
    while at < options.len() {
        match options[at] {
            0 => return None,
            1 => at += 1,
            2 => return Some(u16::from_be_bytes([options[at + 2], options[at + 3]])),
            _ => at += options[at + 1].max(2) as usize,
        }
    }
    None
}

#[tokio::test]
async fn a_smaller_mtu_is_what_the_syn_offers() {
    // The far end sizes every segment it sends us by the MSS in our SYN, and
    // there is no kernel here for `--mssfix` to rewrite it in: an MTU this
    // stack is given is the only way to keep the NAS's packets, once wrapped,
    // under a path that would otherwise split every one of them.
    let mtu = Mtu::new(1150).expect("in range");
    let (to_peer, mut peer_inbound) = mpsc::channel(64);
    let (_to_us, our_inbound) = mpsc::channel(64);

    let attempt = tokio::spawn(TunnelStream::connect(
        to_peer,
        our_inbound,
        OURS,
        NAS,
        Duration::from_millis(200),
        mtu,
    ));

    let syn = tokio::time::timeout(PATIENCE, peer_inbound.recv())
        .await
        .expect("something went out")
        .expect("a packet");

    assert_eq!(
        offered_mss(&syn),
        Some(1150 - 40),
        "MTU less IPv4 and TCP headers"
    );
    // And what the log reports as the MSS is the one actually offered.
    assert_eq!(
        offered_mss(&syn),
        Some(mtu.mss() as u16),
        "Mtu::mss agrees with the stack"
    );
    attempt.abort();
}

#[tokio::test]
async fn a_flushed_write_survives_letting_go_of_the_stream() {
    // Dropping the stream stops the task that owns the stack, so anything it
    // had not sent yet goes with it. That is what makes `flush` load-bearing
    // rather than decorative: a caller that writes, flushes and lets go is
    // entitled to have the write happen.
    let (mut stream, _ask, mut heard) = connected().await;

    stream
        .write_all(b"the last thing written")
        .await
        .expect("queued");
    stream.flush().await.expect("flushed");
    drop(stream);

    let mut collected = Vec::new();
    while collected.len() < 22 {
        let more = tokio::time::timeout(PATIENCE, heard.recv())
            .await
            .expect("the far end already has it")
            .expect("a chunk");
        collected.extend_from_slice(&more);
    }
    assert_eq!(collected, b"the last thing written");
}

#[tokio::test]
async fn a_shutdown_waits_for_what_it_is_shutting_down() {
    // The same promise, made harder: everything written must be out before
    // the connection is finished with, and `shutdown` must not return until
    // it is.
    let (mut stream, _ask, mut heard) = connected().await;

    stream.write_all(b"goodbye").await.expect("queued");
    tokio::time::timeout(PATIENCE, stream.shutdown())
        .await
        .expect("it does not wait forever")
        .expect("shut down");
    drop(stream);

    assert_eq!(
        tokio::time::timeout(PATIENCE, heard.recv())
            .await
            .expect("already delivered")
            .expect("a chunk"),
        b"goodbye"
    );
}

#[tokio::test]
async fn a_stream_cut_off_is_an_error_rather_than_an_ending() {
    // The distinction the reader above depends on. A peer that closed politely
    // and a link that died both stop the bytes; only one of them means the
    // message was complete. Reported as a clean end of stream, a truncated SMB
    // response reads as a server that had nothing more to say.
    let (mut stream, ask, _heard) = connected().await;

    ask.send(Ask::Vanish).await.expect("the link fails");

    let mut rest = Vec::new();
    let read = tokio::time::timeout(PATIENCE, stream.read_to_end(&mut rest))
        .await
        .expect("it does not wait forever");

    assert!(
        read.is_err(),
        "a link that died is not a peer that finished"
    );
}

#[tokio::test]
async fn a_flush_on_a_stream_that_died_is_not_a_success() {
    // `flush` returning `Ok` is a promise the bytes arrived. Once the stack
    // has stopped they never will, and saying so beats a caller that goes on
    // to the next thing believing the last one landed.
    let (mut stream, ask, _heard) = connected().await;

    ask.send(Ask::Stall(true)).await.expect("stop answering");
    stream.write_all(b"into the dark").await.expect("queued");
    ask.send(Ask::Vanish).await.expect("and then nothing");

    let flushed = tokio::time::timeout(PATIENCE, stream.flush())
        .await
        .expect("it does not wait forever");

    assert!(flushed.is_err(), "nothing was flushed anywhere");
}

#[tokio::test]
async fn a_flush_that_was_given_up_on_does_not_satisfy_the_next_one() {
    // A `flush` abandoned by a timeout or a `select!` leaves its wait behind.
    // Reused, it answers the *next* flush's question with the previous one's —
    // a smaller threshold, already met — so a later write is reported as
    // delivered while it is still sitting in the window.
    let (mut stream, ask, mut heard) = connected().await;

    ask.send(Ask::Stall(true)).await.expect("stop answering");
    stream.write_all(b"first").await.expect("queued");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), stream.flush())
            .await
            .is_err(),
        "nothing is acknowledged while the peer is stalled"
    );

    // The first write lands — but nobody flushes again, so the abandoned wait
    // is still sitting there with the first write's threshold in it.
    ask.send(Ask::Stall(false)).await.expect("answer again");
    assert_eq!(
        tokio::time::timeout(PATIENCE, heard.recv())
            .await
            .expect("delivered")
            .expect("a chunk"),
        b"first"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    ask.send(Ask::Stall(true)).await.expect("and stop again");

    stream.write_all(b"second").await.expect("queued");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), stream.flush())
            .await
            .is_err(),
        "the second write has not been acknowledged by anybody"
    );
}

#[tokio::test]
async fn a_shutdown_is_over_only_once_the_goodbye_has_gone() {
    // Dropping the stream stops the task, so a `shutdown` that returns while
    // the FIN is still queued leaves the far end holding a connection nobody
    // is going to finish. The peer reports the goodbye by letting go of its
    // own channel.
    let (mut stream, _ask, mut heard) = connected().await;

    stream.write_all(b"goodbye").await.expect("queued");
    tokio::time::timeout(PATIENCE, stream.shutdown())
        .await
        .expect("it does not wait forever")
        .expect("shut down");
    drop(stream);

    let mut collected = Vec::new();
    while let Some(chunk) = tokio::time::timeout(PATIENCE, heard.recv())
        .await
        .expect("the peer is not left waiting")
    {
        collected.extend_from_slice(&chunk);
    }
    assert_eq!(collected, b"goodbye");
}

#[tokio::test]
async fn what_is_still_in_the_window_when_the_connection_ends_is_still_read() {
    // Both sides having closed is not the same as everything having been read.
    // The stack holds far more than fits between it and a reader, and a reader
    // that arrives late is entitled to all of it — otherwise a response is
    // truncated and reported as a successful read.
    let (mut stream, ask, _heard) = connected().await;

    let sent: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
    for chunk in sent.chunks(8 * 1024) {
        ask.send(Ask::Send(chunk.to_vec())).await.expect("queued");
    }
    ask.send(Ask::Close).await.expect("and goodbye");

    // Our half closes too, so the connection finishes rather than sitting
    // half-open — and it finishes while nobody has read a byte.
    stream.shutdown().await.expect("our half");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut received = Vec::new();
    tokio::time::timeout(PATIENCE, stream.read_to_end(&mut received))
        .await
        .expect("in reasonable time")
        .expect("readable");

    assert_eq!(received.len(), sent.len(), "nothing was discarded");
    assert_eq!(received, sent);
}

#[tokio::test]
async fn a_stream_says_what_stopped_underneath_it() {
    // A stream can only report what it saw: bytes stopped. The layer below
    // often knows why — authentication refused, the peer gone, a cipher we
    // cannot speak — and a caller told only "the connection ended" goes
    // looking at the wrong end of the problem.
    let (stream, ask, _heard) = connected().await;
    let cause = LinkFailure::new();
    let mut stream = stream.explaining(cause.clone());

    // The link goes, and the layer that owns it knows what happened.
    cause.set(Error::AuthFailed("wrong password".into()));
    ask.send(Ask::Vanish).await.expect("the link fails");

    let mut rest = Vec::new();
    let failed = tokio::time::timeout(PATIENCE, stream.read_to_end(&mut rest))
        .await
        .expect("it does not wait forever")
        .expect_err("a link that died is not a peer that finished");

    assert!(
        failed.to_string().contains("wrong password"),
        "the reason from below reaches the caller: {failed}"
    );
}

#[tokio::test]
async fn giving_up_on_a_connection_takes_the_stack_with_it() {
    // `connect` aborts its task on every path it returns from. It cannot abort
    // anything on a path it never returns from: a caller that gives up — a
    // `timeout`, a `select!` losing the race — drops the future, and whatever
    // it had already spawned is left running with nobody holding it.
    //
    // What is left is not idle. It holds the sender into the tunnel below,
    // which keeps that tunnel's pump alive, which keeps an authenticated
    // OpenVPN session and its socket alive. Once per retry, forever.
    let (to_peer, mut peer_inbound) = mpsc::channel(64);
    let (_to_us, our_inbound) = mpsc::channel(64);

    let attempt = tokio::spawn(TunnelStream::connect(
        to_peer,
        our_inbound,
        OURS,
        NAS,
        // Long enough that the caller below is certainly the one giving up.
        Duration::from_secs(600),
        Mtu::default(),
    ));

    // It got as far as putting a SYN on the link, so the stack is running.
    tokio::time::timeout(PATIENCE, peer_inbound.recv())
        .await
        .expect("something went out")
        .expect("a packet");

    attempt.abort();

    // The stack holds the only sender on this channel. Its closing is the
    // stack having gone.
    let ended = tokio::time::timeout(Duration::from_secs(30), async {
        while peer_inbound.recv().await.is_some() {}
    })
    .await;

    assert!(
        ended.is_ok(),
        "the stack outlived the caller that asked for it"
    );
}

#[tokio::test]
async fn a_bulk_write_gets_through_at_a_usable_rate() {
    // The one the earlier tests could not see. They moved 200 KB and only asked
    // whether it arrived — and it did, eventually, because TCP recovers from
    // losing packets. What it does not do is recover *quickly*: the device used
    // to drop whatever the tunnel's queue had no room for, `smoltcp` believed
    // it had sent those packets, and the retransmissions read as congestion.
    //
    // What that looked like from above was an SMB write queue that would not
    // drain — frames sitting half a minute, never on the wire — because the
    // writer was blocked in `send` on a stack that was mostly retransmitting.
    let (mut stream, _ask, mut heard) = connected().await;

    // Enough to fill the window many times over, which is what a file copy is.
    const BULK: usize = 4 * 1024 * 1024;

    // The far end has to keep reading, or this measures the test harness.
    let counting = tokio::spawn(async move {
        let mut total = 0usize;
        while total < BULK {
            match heard.recv().await {
                Some(chunk) => total += chunk.len(),
                None => break,
            }
        }
        total
    });

    let sent: Vec<u8> = (0..BULK).map(|i| (i % 251) as u8).collect();
    let started = std::time::Instant::now();
    stream.write_all(&sent).await.expect("written");
    stream.flush().await.expect("acknowledged");
    let took = started.elapsed();

    let arrived = tokio::time::timeout(Duration::from_secs(30), counting)
        .await
        .expect("the far end kept up")
        .expect("the task finished");
    assert_eq!(arrived, BULK, "all of it, not most of it");

    // Loopback through an in-process peer: not a network measurement, but the
    // difference between a stack that sends and one that spends its time
    // sending things twice.
    eprintln!(
        "4 MiB in {took:?} ({:.1} MB/s)",
        BULK as f64 / took.as_secs_f64() / 1e6
    );
    // No threshold asserted. An in-process peer with no latency measures the
    // harness, not the tunnel: the window that limits a real transfer is
    // irrelevant when the round trip is microseconds. What this pins is that
    // four megabytes go through intact and promptly enough to notice a stall.
    assert!(
        took < Duration::from_secs(20),
        "4 MiB took {took:?} even in-process"
    );
}

#[tokio::test]
async fn a_stream_that_breaks_says_which_way_it_broke() {
    // What a caller mid-transfer actually got: "the tunnel stack has stopped",
    // and nothing else. The loop has five exits and took all of them in
    // silence, so a peer that closed, a link that went and a socket the stack
    // itself gave up on were one sentence — and the one thing the sentence did
    // not say was which.
    let (mut stream, ask, _heard) = connected().await;
    ask.send(Ask::Vanish).await.expect("the link fails");

    let mut rest = Vec::new();
    let failed = tokio::time::timeout(PATIENCE, stream.read_to_end(&mut rest))
        .await
        .expect("it does not wait forever")
        .expect_err("a link that died is not a peer that finished");

    assert!(
        failed.to_string().contains("because"),
        "the error says why it ended, not only that it did: {failed}"
    );
    assert!(
        failed.to_string().contains("tunnel"),
        "and names the tunnel as the thing that went: {failed}"
    );
}
