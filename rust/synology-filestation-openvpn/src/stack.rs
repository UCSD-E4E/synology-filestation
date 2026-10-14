//! A TCP stack that lives in this process.
//!
//! The tunnel carries IP packets, because `dev tun` is a layer-3 device. The
//! operating system knows nothing about them — that is the whole point, and
//! the reason none of this needs a tun device, an installer component or a
//! privilege it does not have. So the packets need somewhere to go, and that
//! somewhere is here: `smoltcp`, an interface holding the address the server
//! assigned us, and one TCP connection to one port on one host.
//!
//! One connection per stack. Several can share a tunnel — see [`crate::mux`],
//! which hands each its own packets — but each is still its own interface and
//! its own task, because a general-purpose stack would be more code doing more
//! than anyone asked for.
//!
//! ## Why a task rather than a handle to poll
//!
//! `smoltcp` is sans-io like the rest of this crate: it does nothing until
//! polled, and it must be polled on a *timer* as well as on arrival, because
//! retransmission and delayed acknowledgement are things it does on its own
//! clock. Exposing that as a `poll()` for the caller to call put the whole
//! protocol's correctness in the hands of whoever used it — and the caller
//! this exists for is `smb2`, which reads and writes and quite reasonably
//! expects a stream to keep working while it is doing neither.
//!
//! So the stack gets a task of its own, exactly as the tunnel below it does,
//! and what comes out is an ordinary [`AsyncRead`] + [`AsyncWrite`].

use std::io;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant as StdInstant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp::{self, State};
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpCidr};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::PollSender;

use crate::ip::Ifconfig;
use crate::Error;

/// The largest datagram we will put on the wire, outer headers included.
///
/// The server pushes `mssfix 1450`, which is its statement of what the path
/// between here and it carries in one piece.
pub const LINK: usize = 1450;

/// The headers the kernel puts in front of our datagram, counted as IPv6 so
/// that an IPv4 path has twenty bytes spare rather than twenty too few.
pub const OUTER: usize = 40 + 8;

/// What wrapping an IP packet for the data channel costs.
///
/// A P_DATA_V2 header (opcode plus a three-byte peer id), an HMAC-SHA-512 tag,
/// the CBC IV, the four-byte packet id that is encrypted alongside the payload,
/// and PKCS#7 padding — which always adds at least one byte and at most a full
/// block, so a full block is what has to be reserved.
const WRAP: usize = 4 + 64 + 16 + 4 + 16;

/// What the tunnel's MTU leaves for an IP packet.
///
/// Derived rather than chosen, because choosing it is what went wrong: 1400
/// looked like a comfortable margin under `mssfix 1450` and was not, once
/// [`WRAP`] was added — an MTU-sized packet came to 1540 bytes on the wire, the
/// kernel answered `EMSGSIZE`, and the tunnel died a few milliseconds into the
/// first bulk write. Only full-size packets were ever affected, which is why
/// connecting and browsing looked fine and copying a file did not.
pub const MTU: usize = LINK - OUTER - WRAP;

/// How much each direction may buffer inside the stack.
///
/// This is the window, and the window is the speed limit: a sender may have
/// this much unacknowledged, so throughput cannot exceed it divided by the
/// round trip, however fast the link underneath is. At 64 KiB — where this
/// started — that is about 2 MB/s over a 30 ms path, and no amount of
/// bandwidth changes it.
///
/// So it is sized not to be the constraint rather than to be comfortable. Four
/// mebibytes carries roughly a gigabit at 30 ms, and about 300 Mbit even on a
/// 100 ms path — past anything a NAS at the end of a VPN is going to do. The
/// cost is eight mebibytes of buffer for the one connection this stack exists
/// to carry, which is not a trade worth thinking about twice.
///
/// It is not capped at 65535 by the protocol: `smoltcp` derives its window
/// scale (RFC 7323) from this capacity, so the advertised window grows with it.
const BUFFER: usize = 4 * 1024 * 1024;

/// The largest piece of a write handed to the stack at once.
///
/// A caller writing a megabyte should not turn into a megabyte-long
/// allocation in the queue; it turns into pieces the send buffer can take.
const WRITE_CHUNK: usize = 16 * 1024;

/// How many pieces may sit between the stack and the reader, or the writer and
/// the stack.
///
/// Small on purpose. This is not where buffering belongs — the socket's own
/// window is — and a deep queue here would let the far end believe bytes had
/// been read that are still in a channel.
const QUEUE_DEPTH: usize = 8;

/// How often a transfer in progress says how it is going.
///
/// A write queue backing up above this stack looks the same whether the tunnel
/// is slow or stuck, and those want opposite responses. This is the difference:
/// bytes still moving is a link doing its best, bytes that stopped is something
/// wedged.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// A ceiling on how long the loop will sleep when `smoltcp` asks for nothing.
///
/// It should not happen, but a stack that sleeps forever on a wrong answer is
/// a stack that hangs.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// The device `smoltcp` drives, which is the tunnel wearing a different hat.
///
/// Everything above treats it as a network card; everything below is
/// `Tunnel::send` and `Tunnel::recv`. Arrived packets are [`push`]ed in by
/// whoever is driving, rather than pulled from a channel here, because the
/// driver has to *wait* on that channel among other things and a device is
/// only ever asked whether something has already arrived.
///
/// [`push`]: TunnelDevice::push
pub struct TunnelDevice {
    outbound: mpsc::Sender<Vec<u8>>,
    inbox: std::collections::VecDeque<Vec<u8>>,
    /// How many packets have gone each way, so a connection that never came up
    /// can say whether anything was ever sent and whether anything ever came
    /// back. `Cell` because a transmit token borrows the device immutably.
    sent: std::cell::Cell<usize>,
    received: usize,
}

impl TunnelDevice {
    pub fn new(outbound: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            outbound,
            inbox: std::collections::VecDeque::new(),
            sent: std::cell::Cell::new(0),
            received: 0,
        }
    }

    /// Hand the device a packet that arrived from the tunnel.
    pub fn push(&mut self, packet: Vec<u8>) {
        self.received += 1;
        self.inbox.push_back(packet);
    }

    /// Whether the tunnel has room for another packet right now.
    pub fn is_full(&self) -> bool {
        self.outbound.capacity() == 0
    }

    /// Packets handed to the tunnel, and packets taken from it.
    pub fn traffic(&self) -> (usize, usize) {
        (self.sent.get(), self.received)
    }
}

impl Device for TunnelDevice {
    type RxToken<'a>
        = TunnelRx
    where
        Self: 'a;
    type TxToken<'a>
        = TunnelTx<'a>
    where
        Self: 'a;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        // Layer three: no ethernet header, because `dev tun` has none.
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = MTU;
        capabilities
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.inbox.pop_front()?;
        Some((
            TunnelRx { packet },
            TunnelTx {
                outbound: &self.outbound,
                sent: &self.sent,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // `None` means "busy, ask me again", and `smoltcp` keeps the data.
        // Handing back a token we cannot honour is how the stack came to lose
        // packets it believed it had sent — see [`TunnelTx::consume`].
        if self.is_full() {
            return None;
        }
        Some(TunnelTx {
            outbound: &self.outbound,
            sent: &self.sent,
        })
    }
}

pub struct TunnelRx {
    packet: Vec<u8>,
}

impl RxToken for TunnelRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.packet)
    }
}

pub struct TunnelTx<'a> {
    outbound: &'a mpsc::Sender<Vec<u8>>,
    sent: &'a std::cell::Cell<usize>,
}

impl TxToken for TunnelTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        // A token is only handed out when there was room (see `transmit`), so
        // this is expected to succeed. It used to be the ordinary path, and
        // dropping here was excused as what a link does with a packet it cannot
        // carry — but this "link" is a channel to a task in the same process,
        // so the loss was ours to manufacture. TCP read it as congestion and
        // backed off, and a bulk write collapsed to retransmissions.
        // Counted only if it went: this number answers "was anything ever
        // sent", and a packet the tunnel would not take is precisely what it
        // must not claim.
        if self.outbound.try_send(buffer).is_ok() {
            self.sent.set(self.sent.get() + 1);
        }
        result
    }
}

/// How much of what the caller wrote is still ours to deliver.
///
/// Without this, `flush` and `shutdown` could only lie: they would return the
/// moment the bytes reached a channel, and `Drop` — which stops the task —
/// would then throw away everything the stack had not got round to sending. A
/// caller that writes, flushes and lets go is entitled to have the write
/// happen.
/// A count rather than a flag, because a flag cannot be asked the question.
/// The caller knows how many bytes it has handed over; what it needs to know
/// is whether *those* have arrived. A "nothing outstanding" flag is still true
/// for the moment between a write reaching the queue and the stack noticing
/// it, and a flush that returns in that moment returns too early.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Progress {
    /// Bytes from the caller the far end has acknowledged.
    acknowledged: u64,
    /// Our half is closed and the far end has acknowledged the goodbye.
    closed: bool,
    /// The stack has stopped. Nothing outstanding will ever be sent now, so
    /// anyone waiting is waiting for something that is not going to happen.
    stopped: bool,
    /// The peer closed its half in an orderly way and everything it sent has
    /// been handed over.
    ///
    /// The difference between this and any other ending is the difference
    /// between a message that finished and one that was cut off, and a reader
    /// cannot tell them apart from the bytes alone — which is exactly how a
    /// truncated SMB response would be read as a server with nothing more to
    /// say.
    ended_cleanly: bool,
}

/// Why the link under a stream stopped, when something below knows.
///
/// A stream can only report what it saw: bytes stopped. The layer underneath
/// often knows considerably more — that authentication was refused, that the
/// peer went silent, that the cipher was one we cannot speak — and a caller
/// told only "the connection ended" goes looking in the wrong place. Passed to
/// [`TunnelStream::explaining`], that reason is added to the errors this
/// stream reports.
#[derive(Clone, Default)]
pub struct LinkFailure(std::sync::Arc<std::sync::Mutex<Option<Error>>>);

impl LinkFailure {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record why the link stopped. The first reason stands: it is the one
    /// that explains everything after it.
    pub fn set(&self, error: Error) {
        if let Ok(mut held) = self.0.lock() {
            held.get_or_insert(error);
        }
    }

    /// The reason, if the link has stopped and said one.
    pub fn reason(&self) -> Option<Error> {
        self.0.lock().ok().and_then(|held| held.clone())
    }
}

/// One TCP connection over the tunnel, as a stream.
pub struct TunnelStream {
    writes: PollSender<Vec<u8>>,
    reads: mpsc::Receiver<Vec<u8>>,
    /// What was handed over but has not been copied out yet, and how much of
    /// it is gone. A reader is entitled to ask for one byte at a time.
    partial: Vec<u8>,
    taken: usize,
    /// How many bytes have been accepted from the caller, ever. What `flush`
    /// waits to see acknowledged.
    written: u64,
    progress: watch::Receiver<Progress>,
    /// What the layer below says about why it stopped, when it knows.
    cause: LinkFailure,
    /// Whatever has to outlive this connection for it to keep working: the
    /// shared link it runs on, which is let go of when the last connection
    /// using it is.
    _keep: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    /// Waits in progress, because these are polled repeatedly and the wait has
    /// to survive between calls.
    ///
    /// One slot each, rather than one between them: a `flush` that is
    /// abandoned — a timeout, a `select!` losing the race — leaves its wait
    /// behind, and a shared slot would hand it to the next `shutdown` to
    /// finish, which would then be waiting for the wrong thing entirely.
    flushing: Wait,
    shutting_down: Wait,
    task: JoinHandle<()>,
}

/// A wait on the stack's progress, kept across polls, and what it is waiting
/// for.
///
/// The threshold is kept beside the future because a wait that is abandoned —
/// a timeout, a `select!` losing the race — stays in its slot, and answering
/// the *next* caller's question with the previous one's is how a write still
/// sitting in the window gets reported as delivered.
type Wait = Option<(u64, Pin<Box<dyn std::future::Future<Output = bool> + Send>>)>;

/// Wait until the stack says what the caller is waiting for is true.
///
/// A stack that has stopped answers every question the same way: there is
/// nothing more it will do, so there is nothing more to wait for.
fn until(
    mut progress: watch::Receiver<Progress>,
    done: impl Fn(Progress) -> bool + Send + 'static,
) -> Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
    Box::pin(async move {
        loop {
            {
                let now = *progress.borrow_and_update();
                if done(now) {
                    return true;
                }
                if now.stopped {
                    return false;
                }
            }
            if progress.changed().await.is_err() {
                return false;
            }
        }
    })
}

impl TunnelStream {
    /// Open a connection through the tunnel, and return once it is up.
    ///
    /// `ifconfig` is where the server put us; `remote` is what we are dialling
    /// — for this driver's purpose, the NAS at its address inside the tunnel.
    /// `patience` bounds the wait: a peer that never answers is a failure, not
    /// a caller blocked forever.
    pub async fn connect(
        outbound: mpsc::Sender<Vec<u8>>,
        inbound: mpsc::Receiver<Vec<u8>>,
        ifconfig: Ifconfig,
        remote: (Ipv4Addr, u16),
        patience: Duration,
    ) -> Result<Self, Error> {
        // An ephemeral source port. Anything unused will do; the far end only
        // cares that it is consistent.
        let port = 49152 + (rand::random::<u16>() % 16000);
        Self::connect_from(outbound, inbound, ifconfig, remote, port, patience).await
    }

    /// [`connect`](Self::connect) from a source port the caller chose, which
    /// is how connections sharing one link are told apart.
    pub(crate) async fn connect_from(
        outbound: mpsc::Sender<Vec<u8>>,
        inbound: mpsc::Receiver<Vec<u8>>,
        ifconfig: Ifconfig,
        remote: (Ipv4Addr, u16),
        local_port: u16,
        patience: Duration,
    ) -> Result<Self, Error> {
        // Both ends of the question "is the NAS even on our subnet?". The push
        // reply says what we were given; this says what we then dialled, and
        // the pair is what turns "nothing answered" into an answer.
        tracing::debug!(
            "stack: connecting {}/{} → {}:{} inside the tunnel",
            ifconfig.address,
            ifconfig.prefix,
            remote.0,
            remote.1
        );

        let mut device = TunnelDevice::new(outbound);
        let started = StdInstant::now();

        let mut interface = Interface::new(
            Config::new(smoltcp::wire::HardwareAddress::Ip),
            &mut device,
            Instant::from_micros(0),
        );
        interface.update_ip_addrs(|addresses| {
            let _ = addresses.push(IpCidr::new(
                IpAddress::Ipv4(ifconfig.address),
                ifconfig.prefix,
            ));
        });

        let socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; BUFFER]),
        );
        let mut sockets = SocketSet::new(Vec::new());
        let handle = sockets.add(socket);

        sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(
                interface.context(),
                (IpAddress::Ipv4(remote.0), remote.1),
                local_port,
            )
            .map_err(|error| Error::Io(format!("connect: {error}")))?;

        let (to_stack, from_caller) = mpsc::channel(QUEUE_DEPTH);
        let (to_caller, from_stack) = mpsc::channel(QUEUE_DEPTH);
        let (up, is_up) = tokio::sync::oneshot::channel();
        // One slot, shared with the driver: a stream that breaks mid-write can
        // then say what happened rather than only that something did.
        let cause = LinkFailure::new();
        let (progress, watching) = watch::channel(Progress {
            acknowledged: 0,
            closed: false,
            stopped: false,
            ended_cleanly: false,
        });

        let task = tokio::spawn(drive(
            Driver {
                outbound: device.outbound.clone(),
                device,
                interface,
                sockets,
                handle,
                inbound,
                from_caller,
                to_caller: Some(to_caller),
                by: started + patience,
                progress,
                cause: cause.clone(),
                started,
            },
            up,
        ));

        match tokio::time::timeout(patience, is_up).await {
            Ok(Ok(Ok(()))) => Ok(Self {
                writes: PollSender::new(to_stack),
                reads: from_stack,
                partial: Vec::new(),
                taken: 0,
                written: 0,
                progress: watching,
                cause,
                _keep: None,
                flushing: None,
                shutting_down: None,
                task,
            }),
            Ok(Ok(Err(error))) => {
                task.abort();
                Err(error)
            }
            // The task ended without saying anything, which is a bug in it
            // rather than in the peer.
            Ok(Err(_)) => {
                task.abort();
                Err(Error::Io("the stack stopped".into()))
            }
            Err(_) => {
                task.abort();
                Err(Error::Io(format!(
                    "no answer from {}:{} inside the tunnel",
                    remote.0, remote.1
                )))
            }
        }
    }
}

impl TunnelStream {
    /// Have this stream's errors say what stopped underneath it.
    ///
    /// Without this a tunnel that failed to authenticate and a peer that hung
    /// up are the same "the connection ended", and the caller looks at the
    /// wrong end of the problem.
    pub fn explaining(mut self, cause: LinkFailure) -> Self {
        self.cause = cause;
        self
    }

    /// Keep `what` alive for as long as this connection is.
    pub(crate) fn holding(mut self, what: std::sync::Arc<dyn std::any::Any + Send + Sync>) -> Self {
        self._keep = Some(what);
        self
    }

    /// An error saying what the stream saw, and what the link below says
    /// about why.
    fn ended(&self, kind: io::ErrorKind, what: &str) -> io::Error {
        match self.cause.reason() {
            Some(reason) => io::Error::new(kind, format!("{what}: {reason}")),
            None => io::Error::new(kind, what.to_string()),
        }
    }
}

impl Drop for TunnelStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AsyncRead for TunnelStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();

        while me.taken == me.partial.len() {
            match me.reads.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    me.partial = chunk;
                    me.taken = 0;
                }
                // The connection is over. Zero bytes means exactly this and
                // nothing else, which is what lets a framing layer above tell
                // "the peer hung up" from "not yet" — and it is only an
                // ending if the peer actually said goodbye.
                Poll::Ready(None) => {
                    return Poll::Ready(if me.progress.borrow().ended_cleanly {
                        Ok(())
                    } else {
                        Err(me.ended(
                            io::ErrorKind::ConnectionReset,
                            "the connection ended before the peer closed it",
                        ))
                    })
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        let available = &me.partial[me.taken..];
        let wanted = available.len().min(buf.remaining());
        buf.put_slice(&available[..wanted]);
        me.taken += wanted;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for TunnelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();

        match me.writes.poll_reserve(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(_)) => return Poll::Ready(Err(me.gone())),
            Poll::Pending => return Poll::Pending,
        }

        let taking = buf.len().min(WRITE_CHUNK);
        let chunk = buf[..taking].to_vec();
        me.writes.send_item(chunk).map_err(|_| me.gone())?;
        me.written += taking as u64;
        Poll::Ready(Ok(taking))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let written = me.written;
        poll_wait(
            &mut me.flushing,
            &me.progress,
            cx,
            written,
            move |progress| progress.acknowledged >= written,
        )
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        // Closing our half of the channel is how the stack is told to close
        // its half of the connection — and then we wait for it to have done
        // so, because a shutdown that returns before the bytes went out is
        // the thing a shutdown exists to prevent.
        me.writes.close();
        poll_wait(&mut me.shutting_down, &me.progress, cx, 0, |progress| {
            progress.closed
        })
    }
}

/// Poll a wait, starting it if it has not started.
fn poll_wait(
    slot: &mut Wait,
    progress: &watch::Receiver<Progress>,
    cx: &mut Context<'_>,
    target: u64,
    done: impl Fn(Progress) -> bool + Send + 'static,
) -> Poll<io::Result<()>> {
    if slot
        .as_ref()
        .is_none_or(|(waiting_for, _)| *waiting_for != target)
    {
        *slot = Some((target, until(progress.clone(), done)));
    }
    let (_, waiting) = slot.as_mut().expect("just put there");
    match waiting.as_mut().poll(cx) {
        Poll::Ready(met) => {
            *slot = None;
            // The stack stopped before it could. Whatever was outstanding is
            // gone, and a caller told `Ok` would go on to the next thing
            // believing this one landed.
            Poll::Ready(if met {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the tunnel stack has stopped",
                ))
            })
        }
        Poll::Pending => Poll::Pending,
    }
}

impl TunnelStream {
    fn gone(&self) -> io::Error {
        self.ended(io::ErrorKind::BrokenPipe, "the tunnel stack has stopped")
    }
}

/// Why the loop stopped.
///
/// It has five exits and used to take all of them in silence, so a caller
/// mid-write got "the tunnel stack has stopped" and nothing more: no cause on
/// the error, nothing in the log, and no way to tell a peer that closed from a
/// link that went from a socket the stack itself gave up on.
#[derive(Debug, Clone, Copy)]
enum Stopped {
    /// Nobody was waiting for the connection any more.
    CallerGaveUp,
    /// It never came up inside the time it was given.
    NeverConnected,
    /// The socket refused what the caller wrote — it is not open for sending.
    CannotSend,
    /// The tunnel underneath stopped, and what it had delivered is handed over.
    TunnelGone,
    /// Both ends are finished and there is nothing left to read.
    Finished,
}

impl Stopped {
    fn why(self) -> &'static str {
        match self {
            Self::CallerGaveUp => "nobody was waiting for it any more",
            Self::NeverConnected => "it never came up",
            Self::CannotSend => "the connection would not accept what was written to it",
            Self::TunnelGone => "the tunnel underneath it stopped",
            Self::Finished => "the connection was closed at both ends",
        }
    }

    /// Whether this is the connection ending as it should, or something the
    /// caller is entitled to hear about.
    fn is_ordinary(self) -> bool {
        matches!(self, Self::Finished | Self::CallerGaveUp)
    }
}

/// Everything the loop owns.
struct Driver {
    device: TunnelDevice,
    interface: Interface,
    sockets: SocketSet<'static>,
    handle: SocketHandle,
    /// Packets arriving from the tunnel.
    inbound: mpsc::Receiver<Vec<u8>>,
    /// The same sender the device holds, for waiting on room in the tunnel.
    ///
    /// Now that a full tunnel means `transmit` refuses, `smoltcp` asks to be
    /// polled again immediately — so without something to wait for, the loop
    /// would spin at full tilt until the tunnel drained.
    outbound: mpsc::Sender<Vec<u8>>,
    /// Bytes the caller wants sent.
    from_caller: mpsc::Receiver<Vec<u8>>,
    /// How much of what the caller wrote is still ours to deliver.
    progress: watch::Sender<Progress>,
    /// Where a reason for stopping goes, so the stream reports why rather than
    /// only that it did.
    cause: LinkFailure,
    /// When to give up if the connection has still not come up.
    ///
    /// The loop's own deadline, not the caller's. `connect` aborts this task
    /// on every path it returns from — and can abort nothing on a path it
    /// never returns from, which is what a caller that gives up leaves behind:
    /// a stack still holding the sender into the tunnel below it, which keeps
    /// that tunnel's pump, its authenticated session and its socket alive with
    /// nobody left who wanted any of them.
    by: StdInstant,
    /// Bytes that arrived for the caller, until the peer stops sending.
    ///
    /// Dropped at that point and not before: letting go of it is how a reader
    /// above is told the stream has ended, and it is a different event from
    /// the connection being over. A peer that has said everything it intends
    /// to say still owes us an acknowledgement for what we are sending.
    to_caller: Option<mpsc::Sender<Vec<u8>>>,
    started: StdInstant,
}

impl Driver {
    fn now(&self) -> Instant {
        Instant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    fn socket(&mut self) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(self.handle)
    }
}

/// The loop: poll, move bytes in both directions, and wait for whichever of
/// the four things that can happen next happens first.
async fn drive(mut driver: Driver, up: tokio::sync::oneshot::Sender<Result<(), Error>>) {
    let mut up = Some(up);
    // Whether the connection has ever been up, which is what separates "the
    // peer has stopped sending" from "it has not started".
    let mut established = false;
    // Bytes accepted from the caller that the send buffer had no room for.
    let mut queued: Vec<u8> = Vec::new();
    // How many bytes have been taken from the caller's queue, ever.
    let mut accepted: u64 = 0;
    // What the last report said, so the next one can be a rate.
    let mut reported_at = StdInstant::now();
    let mut reported_acknowledged: u64 = 0;
    // What was last said about all this, so that stopping can say it again
    // with the truth about having stopped attached.
    let mut last = *driver.progress.borrow();
    // Whether the tunnel underneath is still there.
    let mut link_gone = false;
    // Set on the way out of every exit, so a caller is never told only that
    // this stopped. No initial value worth having: every `break` names one,
    // which is the point.
    let stopped;
    // Whether the caller has finished writing, so the connection should be
    // closed once what it wrote has gone.
    let mut closing = false;

    loop {
        let now = driver.now();
        driver
            .interface
            .poll(now, &mut driver.device, &mut driver.sockets);

        // Nobody is waiting for this any more. A caller that gave up — a
        // `timeout`, a `select!` losing the race — drops the future that would
        // have told us, and dropping it closes the channel it was waiting on.
        // That is the only signal there is, and without it this loop runs on
        // holding a tunnel nobody wants.
        if up.as_ref().is_some_and(|caller| caller.is_closed()) {
            stopped = Stopped::CallerGaveUp;
            break;
        }

        // Or nobody ever answered. The caller has its own deadline and will
        // abort this when that passes, but a caller generous enough to wait a
        // long time should not mean a stack that waits that long to notice a
        // peer that was never there.
        if !established && StdInstant::now() >= driver.by {
            stopped = Stopped::NeverConnected;
            break;
        }

        // The moment it is carrying, whoever called `connect` is told.
        if driver.socket().may_send() {
            established = true;
            if let Some(up) = up.take() {
                let _ = up.send(Ok(()));
            }
        }

        // Out: as much of what the caller wrote as the window will take.
        if !queued.is_empty() && driver.socket().can_send() {
            match driver.socket().send_slice(&queued) {
                Ok(sent) => drop(queued.drain(..sent)),
                Err(e) => {
                    tracing::warn!("tunnel: the connection would not take a write: {e}");
                    stopped = Stopped::CannotSend;
                    break;
                }
            }
        }
        if closing && queued.is_empty() && driver.socket().may_send() {
            driver.socket().close();
        }

        // In: only as much as there is somewhere to put. Bytes left in the
        // socket are the window closing, which is how a reader that has
        // stopped reading slows the far end down instead of losing anything.
        let mut buffer = [0u8; 8 * 1024];
        {
            // Split apart so the socket and the caller's queue can be held at
            // once: they are different fields, which the borrow checker only
            // believes when it is shown them separately.
            let Driver {
                sockets,
                handle,
                to_caller,
                ..
            } = &mut driver;
            let socket = sockets.get_mut::<tcp::Socket>(*handle);
            while let (true, Some(sender)) = (socket.can_recv(), to_caller.as_ref()) {
                let Ok(permit) = sender.try_reserve() else {
                    break;
                };
                match socket.recv_slice(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(len) => permit.send(buffer[..len].to_vec()),
                }
            }
        }

        // How a transfer is going, while there is one. Both halves matter: a
        // rate says whether the link is moving, and what is waiting for an
        // acknowledgement says whether the window is the thing limiting it.
        let waiting_to_be_acknowledged = driver.socket().send_queue();
        if reported_at.elapsed() >= REPORT_EVERY {
            let acknowledged = last.acknowledged;
            let moved = acknowledged.saturating_sub(reported_acknowledged);
            if moved > 0 || waiting_to_be_acknowledged > 0 {
                let rate = moved as f64 / reported_at.elapsed().as_secs_f64() / 1024.0;
                tracing::info!(
                    "tunnel: {:.0} KiB/s out, {} KiB waiting to be acknowledged of a \
                     {} KiB window",
                    rate,
                    waiting_to_be_acknowledged / 1024,
                    BUFFER / 1024,
                );
            }
            reported_at = StdInstant::now();
            reported_acknowledged = acknowledged;
        }

        // The peer has closed its half and everything it sent has been
        // handed over. Letting go of the sender here is the end of stream a
        // reader above is waiting for — and doing it on this condition rather
        // than on the connection being finished is what stops a peer that
        // closes politely from leaving a reader waiting forever, which is
        // exactly how SMB servers end a session.
        let finished_reading =
            established && !driver.socket().may_recv() && !driver.socket().can_recv();

        // What anyone waiting on `flush` or `shutdown` is waiting for. What
        // has been taken from the caller, less what is still waiting for the
        // window and what the far end has not acknowledged — so this counts
        // bytes that *arrived*, rather than bytes that merely left.
        //
        // And `closed` waits for the goodbye to have been acknowledged, not
        // merely asked for: `close` only queues the FIN, and `Drop` stops this
        // task, so a shutdown that returned in between would leave the far end
        // holding a connection nobody is coming back to finish.
        let outstanding = (queued.len() + driver.socket().send_queue()) as u64;
        last = Progress {
            acknowledged: accepted.saturating_sub(outstanding),
            closed: closing
                && outstanding == 0
                && matches!(
                    driver.socket().state(),
                    State::FinWait2 | State::TimeWait | State::Closed
                ),
            stopped: false,
            ended_cleanly: last.ended_cleanly || finished_reading,
        };
        driver.progress.send_replace(last);

        if finished_reading {
            driver.to_caller = None;
        }

        // Everything having finished is not the same as everything having
        // been read: the stack holds far more than fits between it and a
        // reader, and what is still in there is owed to whoever is coming for
        // it.
        let unread = driver.socket().can_recv() && driver.to_caller.is_some();
        if (!driver.socket().is_active() || link_gone) && !unread {
            stopped = if link_gone {
                Stopped::TunnelGone
            } else {
                Stopped::Finished
            };
            break;
        }

        // Whether there are bytes waiting on a reader that has not caught up.
        let waiting_on_reader = driver.socket().can_recv() && driver.to_caller.is_some();
        // And whether the tunnel is too full to take what the stack wants to
        // send, in which case room appearing is a reason to wake.
        let waiting_on_tunnel = driver.device.is_full();
        let accepting_writes = queued.is_empty() && !closing;
        let delay = driver
            .interface
            .poll_delay(driver.now(), &driver.sockets)
            .map(|delay| Duration::from_micros(delay.total_micros()))
            .unwrap_or(IDLE_POLL)
            .min(IDLE_POLL);
        // Never past the point where the loop has decided to stop waiting.
        let delay = if established {
            delay
        } else {
            delay.min(driver.by.saturating_duration_since(StdInstant::now()))
        };

        tokio::select! {
            packet = driver.inbound.recv(), if !link_gone => match packet {
                Some(packet) => driver.device.push(packet),
                // The tunnel has stopped. Not a reason to throw away what
                // already arrived through it: a peer that said goodbye before
                // the link went is a peer that finished, and the difference
                // is decided above from what the socket saw, once the last of
                // it has been handed over.
                None => link_gone = true,
            },

            written = driver.from_caller.recv(), if accepting_writes => match written {
                Some(bytes) => {
                    accepted += bytes.len() as u64;
                    queued = bytes;
                }
                // The caller has finished writing, or gone.
                None => closing = true,
            },

            // Room for the reader appearing is a reason to wake: there are
            // bytes sitting in the socket waiting for it.
            _ = async {
                match driver.to_caller.as_ref() {
                    Some(sender) => { let _ = sender.reserve().await; }
                    None => std::future::pending().await,
                }
            }, if waiting_on_reader => {}

            // Room in the tunnel, for a stack that has something to send and
            // nowhere to put it.
            _ = driver.outbound.reserve(), if waiting_on_tunnel => {}

            _ = tokio::time::sleep(delay) => {}
        }
    }

    // Nothing more will be sent or received, whatever anyone is waiting for.
    // Said as it is rather than as anyone would like it: a `flush` waiting on
    // bytes this stack still had is a `flush` that failed, and a reader whose
    // peer never said goodbye was cut off rather than finished.
    // Which is worth saying when it is not the ordinary ending: a caller
    // mid-transfer sees a broken pipe, and this is the reason for it. Recorded
    // before the channels close, because that is what wakes the caller.
    if stopped.is_ordinary() {
        tracing::debug!("tunnel: the connection ended because {}", stopped.why());
    } else {
        tracing::warn!("tunnel: the connection ended because {}", stopped.why());
        driver.cause.set(Error::Io(format!(
            "the connection ended because {}",
            stopped.why()
        )));
    }

    driver.progress.send_replace(Progress {
        stopped: true,
        ..last
    });

    // Whoever is waiting on `connect` hears why, rather than waiting out the
    // whole timeout for an answer that is never coming.
    if let Some(up) = up.take() {
        let (sent, received) = driver.device.traffic();
        let _ = up.send(Err(Error::Io(why_it_never_came_up(sent, received))));
    }
}

/// Why a connection that never came up did not.
///
/// "Refused or reset" was said whatever happened, and for the case that
/// actually occurs in the field it was simply untrue: a tunnel came up, the
/// SYN went out, the NAS said nothing at all, and thirty seconds later the
/// driver reported a refusal that never happened. That sent an afternoon after
/// the routing in this crate, which turned out to be fine.
///
/// The counts are the whole point. Nothing received means the packets left and
/// the far end was silent — which is somebody else's half of the problem, and
/// saying so is what stops it being investigated here again.
fn why_it_never_came_up(sent: usize, received: usize) -> String {
    if received == 0 {
        format!(
            "nothing answered inside the tunnel: {sent} packets went out and none came back, \
             so the address is silent rather than refusing"
        )
    } else {
        format!("the connection was refused or reset after {sent} sent and {received} received")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message that sent an afternoon after a routing bug that did not
    /// exist. Silence and refusal are different problems belonging to
    /// different people, and only one of them is worth reading this crate for.
    #[test]
    fn a_silent_far_end_is_not_reported_as_a_refusal() {
        let said = why_it_never_came_up(6, 0);

        assert!(said.contains("silent"), "got {said}");
        assert!(!said.contains("refused or reset"), "got {said}");
        assert!(
            said.contains('6'),
            "how much we sent is the evidence: {said}"
        );
    }

    /// A real refusal still reads as one — the counts are context, not a new
    /// diagnosis.
    #[test]
    fn something_that_answered_and_refused_still_says_so() {
        let said = why_it_never_came_up(3, 1);

        assert!(said.contains("refused or reset"), "got {said}");
        assert!(said.contains('1'), "got {said}");
    }
}
