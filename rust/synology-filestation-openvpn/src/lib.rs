//! An in-process OpenVPN client.
//!
//! e4e-nas terminates an OpenVPN tunnel itself so that off-campus researchers
//! can reach SMB on the appliance and nothing else. Using it should not require
//! a tun device, `CAP_NET_ADMIN`, a privileged helper or an installer
//! component, and it should not touch the rest of the machine's traffic — so
//! this crate speaks the protocol in the same process as the mount, and the
//! bytes it recovers are handed to a userspace TCP stack rather than to the
//! kernel.
//!
//! What is here so far is the control channel's envelope: the static key, the
//! packet framing, and the `tls-auth` HMAC that the server checks *before* it
//! will do any TLS work at all. That ordering is the reason this is the first
//! piece — a client that gets the HMAC wrong does not get an error back, it
//! gets silence.
//!
//! Everything is pinned against packets a real OpenVPN client emitted; see
//! `tests/wire_format.rs`.

mod channel;
mod data;
mod driver;
mod ip;
mod key_method;
mod mux;
mod packet;
mod prf;
mod profile;
mod push;
mod reliable;
mod replay;
mod session;
mod stack;
mod static_key;
mod tls_auth;

pub use channel::ControlChannel;
pub use data::{DataChannel, DataKeys, PeerId, PING};
pub use driver::Tunnel;
pub use ip::Ifconfig;
pub use key_method::{ClientKeyMethod2, ServerKeyMethod2, ServerMessage};
pub use mux::{Mux, WeakMux};
pub use packet::{Acks, ControlPacket, KeyId, Opcode, SessionId};
pub use prf::{key_expansion, tls1_prf, KeySource2};
pub use profile::{credentials, Profile};
pub use push::{PushReply, PUSH_REQUEST, SUPPORTED_CIPHER};
pub use reliable::{Delivery, Outgoing, RecvWindow, SendWindow};
pub use replay::ReplayWindow;
pub use session::{ClientAuth, Credentials, Session, SessionConfig, MAX_TLS_FRAGMENT};
pub use stack::{
    LinkFailure, Mtu, TunnelDevice, TunnelStream, LINK, MIN_MTU, MSS_OVERHEAD, MTU, OUTER,
};
pub use static_key::{KeyDirection, StaticKey, STATIC_KEY_LEN};
pub use tls_auth::{TlsAuth, TlsAuthHeader};

/// Everything that can go wrong reading a key or a packet.
///
/// Deliberately one enum: the caller's question is always "can I use this?",
/// and splitting it by module would only make them write two `From` impls.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("packet is too short to hold {context}")]
    Truncated {
        /// What we were trying to read when the bytes ran out.
        context: &'static str,
    },

    #[error("unknown packet opcode {0}")]
    UnknownOpcode(u8),

    #[error("a packet carries at most {max} acknowledgements; this one claims {count}", max = crate::Acks::MAX)]
    TooManyAcks {
        /// How many the packet claimed to carry.
        count: usize,
    },

    #[error("packet is not authentic: the tls-auth HMAC does not match")]
    BadHmac,

    #[error("packet belongs to a different session")]
    WrongSession,

    #[error("packet belongs to key {0:?}, and this session is running {running:?}", running = .1)]
    OtherKeyId(KeyId, KeyId),

    #[error("packet acknowledges messages of a different session")]
    AckForAnotherSession,

    #[error("packet has already been seen, or is too old to prove otherwise")]
    Replayed,

    #[error("the first packet of a session must be a server reset acknowledging ours")]
    UnexpectedFirstPacket,

    #[error("peer offered key method {0}; only 2 exists")]
    UnsupportedKeyMethod(u8),

    #[error("the server rejected these credentials{0}")]
    AuthFailed(
        /// Whatever the server said after `AUTH_FAILED`, which is sometimes a
        /// reason and sometimes nothing.
        String,
    ),

    #[error("the server sent \"{0}\", which is not what this stage of the session expects")]
    UnexpectedControlMessage(String),

    #[error("the peer closed the TLS session")]
    PeerClosed,

    #[error("a data packet cannot have opcode {0:?}")]
    UnexpectedDataOpcode(Opcode),

    #[error("the packet does not decrypt to a whole number of blocks")]
    BadPadding,

    #[error("this key has sent every packet id it has")]
    PacketIdExhausted,

    #[error("the server pushed \"{0}\", which is not something we can act on")]
    BadPushDirective(String),

    #[error("the server chose cipher {0}; this client speaks only {expected}", expected = crate::SUPPORTED_CIPHER)]
    UnsupportedCipher(String),

    #[error("the server pushed compression ({0}); this client implements none")]
    UnsupportedCompression(String),

    #[error("the tunnel is not ready to carry anything yet")]
    NotReady,

    #[error("the server never answered the request for its configuration")]
    NoPushReply,

    #[error("the tunnel did not come up in time")]
    HandshakeTimeout,

    #[error("the peer has not been heard from in {0:?}")]
    PeerGone(std::time::Duration),

    #[error("socket: {0}")]
    Io(String),

    #[error("the profile is not usable: {0}")]
    BadProfile(String),

    #[error("the profile asks for something this client does not do: {0}")]
    UnsupportedProfileOption(String),

    #[error("{context} is longer than the protocol can describe")]
    FieldTooLong {
        /// Which field, so the caller knows what to shorten.
        context: &'static str,
    },

    #[error("TLS: {0}")]
    Tls(String),

    #[error("a static key is {STATIC_KEY_LEN} bytes; this one is {actual}")]
    KeyLength {
        /// How many bytes were actually decoded.
        actual: usize,
    },

    #[error("static key is not hexadecimal")]
    KeyNotHex,

    #[error("no OpenVPN static key found")]
    KeyMissing,
}

impl Error {
    /// Whether this ends the session, or only this datagram.
    ///
    /// A caller reading a socket needs the difference. Most of what can go
    /// wrong here is about one packet — a duplicate, a reordered flight, a
    /// stray from another session, something a scanner sent — and a client
    /// that tore the tunnel down over any of them would be unusable on a link
    /// that loses anything at all. What is fatal is the session itself being
    /// over or impossible: a refused password, a cipher we cannot speak, a
    /// peer that hung up.
    pub fn is_fatal(&self) -> bool {
        match self {
            // One packet, and the next one may well be fine.
            Error::Truncated { .. }
            | Error::UnknownOpcode(_)
            | Error::BadHmac
            | Error::WrongSession
            | Error::AckForAnotherSession
            | Error::UnexpectedFirstPacket
            | Error::Replayed
            | Error::OtherKeyId(..)
            | Error::TooManyAcks { .. }
            | Error::UnexpectedDataOpcode(_)
            | Error::BadPadding
            | Error::UnexpectedControlMessage(_)
            // A data packet that arrives before the tunnel is open. The
            // server generates its key a moment before we finish with ours,
            // so its first keepalive can land in that gap — which the interop
            // tests document, having been caught by it. Tearing a healthy
            // session down over one early packet would be absurd.
            | Error::NotReady => false,

            // The session cannot continue, or never could.
            Error::KeyLength { .. }
            | Error::KeyNotHex
            | Error::KeyMissing
            | Error::UnsupportedKeyMethod(_)
            | Error::FieldTooLong { .. }
            | Error::Tls(_)
            | Error::AuthFailed(_)
            | Error::PeerClosed
            | Error::PacketIdExhausted
            | Error::BadPushDirective(_)
            | Error::UnsupportedCipher(_)
            | Error::UnsupportedCompression(_)
            | Error::NoPushReply
            | Error::BadProfile(_)
            | Error::UnsupportedProfileOption(_)
            | Error::HandshakeTimeout
            | Error::PeerGone(_)
            | Error::Io(_) => true,
        }
    }
}
