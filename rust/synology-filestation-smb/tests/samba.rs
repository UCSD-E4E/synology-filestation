//! The SMB write paths, against a real SMB server.
//!
//! Everything else in this crate is mocked or pure, because SMB has no useful
//! in-process fake. These two operations are the ones that most need a server:
//! both are single round trips whose whole value is what the *server* does with
//! them, so a mock asserting we sent the right bytes proves the less
//! interesting half.
//!
//! `#[ignore]`d because they need Docker, which CI does not have. Run them with:
//!
//! ```text
//! cargo test -p synology-filestation-smb --test samba -- --ignored --test-threads=1
//! ```
//!
//! One at a time: every test in the process names its compose project after
//! the process id, so two starting together collide on container names.
//!
//! The containers are Samba, not DSM, so they prove the operations are correct
//! SMB — not that this particular NAS accepts them. That still wants the live
//! pass against e4e-nas.

use std::time::Duration;

use smb2::testing::TestServers;
use synology_filestation_core::MetadataTransport;
use synology_filestation_smb::{SmbConfig, SmbTransport};

/// Connect our transport to the auth container, which is the credentialed
/// NTLMv2 path the NAS uses — not the guest one.
async fn connect(servers: &TestServers) -> SmbTransport {
    let cfg = SmbConfig {
        host: "127.0.0.1".to_string(),
        port: smb2::testing::auth_port(),
        username: "testuser".to_string(),
        password: "testpass".to_string(),
        domain: String::new(),
        timeout: Duration::from_secs(10),
    };
    let _ = servers;
    SmbTransport::connect(&cfg)
        .await
        .expect("the auth container should accept these credentials")
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_second_write_replaces_the_first_under_the_same_name() {
    // The replacing rename. Before the fork this needed four operations with a
    // window where the name resolved to nothing; the point of the test is that
    // the *server* accepts one operation and the name never stops resolving.
    let servers = TestServers::start().await.expect("docker compose up");
    let smb = connect(&servers).await;

    let path = "/private/replace-me.txt";
    smb.write_atomic(path, b"first").await.expect("first write");
    assert_eq!(&smb.read_full(path).await.unwrap()[..], b"first");

    smb.write_atomic(path, b"second")
        .await
        .expect("second write");
    assert_eq!(
        &smb.read_full(path).await.unwrap()[..],
        b"second",
        "the second write replaced the first"
    );

    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn truncate_shortens_a_file_without_rewriting_it() {
    let servers = TestServers::start().await.expect("docker compose up");
    let smb = connect(&servers).await;

    let path = "/private/truncate-me.bin";
    let original: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    smb.write_atomic(path, &original).await.expect("write");

    MetadataTransport::truncate(&smb, path, 100)
        .await
        .expect("set end of file");

    let after = smb.read_full(path).await.unwrap();
    assert_eq!(after.len(), 100, "the length is what we set");
    assert_eq!(
        &after[..],
        &original[..100],
        "and the bytes it kept are the ones that were already there"
    );

    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn truncate_can_also_extend_a_file() {
    // The same call grows: the server materialises the gap, and on a sparse
    // filesystem that costs nothing until something writes there.
    let servers = TestServers::start().await.expect("docker compose up");
    let smb = connect(&servers).await;

    let path = "/private/extend-me.bin";
    smb.write_atomic(path, b"abcd").await.expect("write");

    MetadataTransport::truncate(&smb, path, 8)
        .await
        .expect("set end of file");

    let after = smb.read_full(path).await.unwrap();
    assert_eq!(&after[..], b"abcd\0\0\0\0", "old bytes, then zeroes");

    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn closing_a_handle_nothing_was_written_to_still_creates_the_file() {
    // `touch` through the mount: opened, never written, closed. The file has
    // to exist afterwards, and the only thing that can make it exist is the
    // close — there was no write to do it.
    use synology_filestation_core::transport::{OpenWriteTransport, WriteOpen};

    let servers = TestServers::start().await.expect("docker compose up");
    let smb = connect(&servers).await;

    let path = "/private/touched.txt";
    smb.delete(path).await.ok();

    let mut handle = smb
        .open_write(path, WriteOpen::Existing)
        .await
        .expect("open for writing");
    handle.close().await.expect("close");

    let meta = smb.stat(path).await.expect("the file should exist now");
    assert_eq!(meta.size, 0, "created, and empty");

    smb.delete(path).await.ok();
}

// ── A session handed over later ──────────────────────────────────────────────

/// The auth container's config, with the password of our choosing.
fn config_with(password: &str) -> SmbConfig {
    SmbConfig {
        host: "127.0.0.1".to_string(),
        port: smb2::testing::auth_port(),
        username: "testuser".to_string(),
        password: password.to_string(),
        domain: String::new(),
        timeout: Duration::from_secs(10),
    }
}

async fn a_stream_to_the_server() -> tokio::net::TcpStream {
    tokio::net::TcpStream::connect(("127.0.0.1", smb2::testing::auth_port()))
        .await
        .expect("the auth container listens")
}

fn no_redial() -> impl Fn() -> synology_filestation_smb::RedialFuture + Send + Sync + 'static {
    || {
        Box::pin(async {
            Err(synology_filestation_core::SynoFsError::Io(
                "not in this test".into(),
            ))
        })
    }
}

/// A transport that has been handed one session.
async fn adopted() -> SmbTransport {
    let smb = SmbTransport::unconnected(&config_with("testpass"), no_redial());
    smb.adopt(Box::new(a_stream_to_the_server().await), "127.0.0.1")
        .await
        .expect("the session is built on the stream it was given");
    smb
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_transport_attached_without_a_session_serves_once_it_is_given_one() {
    // The mount that came up while SMB was unreachable. It carries on over
    // HTTP, and when SMB answers it is handed a session — it must then serve
    // exactly as a transport that had one from the start.
    let _servers = TestServers::start().await.expect("docker compose up");
    let smb = SmbTransport::unconnected(&config_with("testpass"), no_redial());
    assert!(!smb.is_connected());

    smb.adopt(Box::new(a_stream_to_the_server().await), "127.0.0.1")
        .await
        .expect("adopted");

    assert!(smb.is_connected());
    let path = "/private/adopted.txt";
    smb.write_atomic(path, b"over smb").await.expect("write");
    assert_eq!(&smb.read_full(path).await.unwrap()[..], b"over smb");
    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_better_session_replaces_a_working_one() {
    // The laptop walking from the tunnel back onto campus: the old session
    // works, and the new one takes over without anything being remounted.
    let _servers = TestServers::start().await.expect("docker compose up");
    let smb = adopted().await;
    let path = "/private/moved.txt";
    smb.write_atomic(path, b"written on the first")
        .await
        .unwrap();
    // A read handle cached on the first session, which has to be let go of.
    assert_eq!(smb.read(path, 0, 5).await.unwrap().as_ref(), b"writt");

    smb.adopt(Box::new(a_stream_to_the_server().await), "127.0.0.1")
        .await
        .expect("the second session");

    assert_eq!(
        &smb.read_full(path).await.unwrap()[..],
        b"written on the first"
    );
    assert_eq!(smb.read(path, 8, 2).await.unwrap().as_ref(), b"on");
    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_write_open_across_the_handover_finishes_where_it_started() {
    // A transfer running when the better leg arrives finishes on the leg it
    // began on, rather than being torn between two sessions.
    use synology_filestation_core::transport::{OpenWriteTransport, WriteOpen};

    let _servers = TestServers::start().await.expect("docker compose up");
    let smb = adopted().await;
    let path = "/private/across.bin";
    smb.delete(path).await.ok();

    let mut handle = smb
        .open_write(path, WriteOpen::CreateNew)
        .await
        .expect("open");
    handle.write_at(0, b"first half, ").await.expect("before");

    smb.adopt(Box::new(a_stream_to_the_server().await), "127.0.0.1")
        .await
        .expect("the second session");

    handle.write_at(12, b"second half").await.expect("after");
    handle.close().await.expect("close");

    assert_eq!(
        &smb.read_full(path).await.unwrap()[..],
        b"first half, second half"
    );
    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_refused_session_is_not_asked_for_again() {
    // A wrong password — or an AD account with no domain set — handed a
    // stream every minute would lock the address out of the NAS for good.
    let _servers = TestServers::start().await.expect("docker compose up");
    let smb = SmbTransport::unconnected(&config_with("not the password"), no_redial());

    let refused = smb
        .adopt(Box::new(a_stream_to_the_server().await), "127.0.0.1")
        .await
        .expect_err("the server turns it down");
    assert_eq!(
        refused.category(),
        synology_filestation_core::error::ErrorCategory::PermissionDenied,
        "{refused:?}"
    );
    assert!(smb.refused().is_some());

    // Handed a stream nobody is on the other end of: if it tried, it would
    // hang or fail differently. It must not try.
    let (ours, _theirs) = tokio::io::duplex(1024);
    let again = tokio::time::timeout(
        Duration::from_secs(2),
        smb.adopt(Box::new(ours), "127.0.0.1"),
    )
    .await
    .expect("answered at once")
    .expect_err("still refused");
    assert_eq!(
        again.category(),
        synology_filestation_core::error::ErrorCategory::PermissionDenied
    );
}

// ── One slow listing does not hold up the rest of the mount ──────────────────

/// SMB2 command code for QUERY_DIRECTORY.
const QUERY_DIRECTORY: u16 = 0x000E;

/// A session to the server whose directory listings come back `delay` late.
///
/// Sits between the transport and Samba and holds back every response whose
/// command is QUERY_DIRECTORY, passing everything else straight through. SMB
/// responses are routed by message id, so a late one overtaken by others is
/// what a slow listing on a real NAS looks like: one huge directory paging
/// through a tunnel while the rest of the mount asks small questions.
async fn listings_delayed_by(delay: Duration) -> SmbTransport {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let server = a_stream_to_the_server().await;
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (mut from_server, mut to_server) = server.into_split();
    let (mut from_client, mut to_client) = tokio::io::split(theirs);

    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
    });

    // Whole frames only, so a held-back response is never interleaved with
    // another one on the way out.
    let (frames, mut out) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(frame) = out.recv().await {
            if to_client.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        loop {
            let mut netbios = [0u8; 4];
            if from_server.read_exact(&mut netbios).await.is_err() {
                break;
            }
            let len = u32::from_be_bytes([0, netbios[1], netbios[2], netbios[3]]) as usize;
            let mut frame = netbios.to_vec();
            frame.resize(4 + len, 0);
            if from_server.read_exact(&mut frame[4..]).await.is_err() {
                break;
            }
            // The SMB2 header is in the clear even on a signed session; the
            // command sits 12 bytes in.
            let command = u16::from_le_bytes([frame[4 + 12], frame[4 + 13]]);
            if command == QUERY_DIRECTORY {
                let frames = frames.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = frames.send(frame);
                });
            } else if frames.send(frame).is_err() {
                break;
            }
        }
    });

    let smb = SmbTransport::unconnected(&config_with("testpass"), no_redial());
    smb.adopt(Box::new(ours), "127.0.0.1")
        .await
        .expect("the session is built through the proxy");
    smb
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_slow_listing_does_not_hold_up_a_lookup() {
    // The wedge: a `find` reached a directory of a million entries, and the
    // listing took the transport's lock for the whole of its paging. Every
    // lookup on every share then waited behind it, and so did every FUSE
    // thread asking one. A lookup has nothing to do with somebody else's
    // listing and must not wait for it.
    let _servers = TestServers::start().await.expect("docker compose up");
    let smb = std::sync::Arc::new(listings_delayed_by(Duration::from_secs(5)).await);
    let path = "/private/beside-a-listing.txt";
    smb.write_atomic(path, b"here").await.expect("write");

    let lister = std::sync::Arc::clone(&smb);
    let listing = tokio::spawn(async move { lister.list_dir("/private").await });
    // Long enough for the listing to be sent and to be waiting on its page.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let info = tokio::time::timeout(Duration::from_secs(2), smb.get_info(path))
        .await
        .expect("a lookup answered while a listing was still paging")
        .expect("get_info");
    assert_eq!(info.name, "beside-a-listing.txt");
    assert!(!listing.is_finished(), "the listing was still in flight");

    let entries = listing.await.unwrap().expect("the listing finishes too");
    assert!(entries.iter().any(|e| e.name == "beside-a-listing.txt"));
    smb.delete(path).await.ok();
}

// ── The login probe: one strike, then HTTP ───────────────────────────────────

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_probe_refused_by_the_server_is_not_dialled_again() {
    // What got krg-nat blocked: a client per task, each probing SMB with a
    // username SMB would not accept. Every refusal was a DSM strike.
    use synology_filestation_smb::{probe_as, FallbackKind, Probe};

    let _servers = TestServers::start().await.expect("docker compose up");
    let host = format!("127.0.0.1:{}", smb2::testing::auth_port());

    let Probe::Http(first) = probe_as(&host, "testuser", "not the password", Some("")).await else {
        panic!("the server turns the login down");
    };
    assert_eq!(first.kind, FallbackKind::AuthRefused, "{}", first.detail);
    assert!(first.detail.contains("bare username"), "{}", first.detail);

    let Probe::Http(second) = tokio::time::timeout(
        Duration::from_secs(1),
        probe_as(&host, "testuser", "not the password", Some("")),
    )
    .await
    .expect("answered without a round trip") else {
        panic!("still HTTP");
    };
    assert_eq!(second.kind, FallbackKind::AuthCooldown, "{}", second.detail);
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_refusal_seen_by_the_probe_stops_another_transport_rebuilding_a_session() {
    // The gate covers every way this crate authenticates, not only the
    // first probe: N transports rebuilding sessions after the NAS restarts
    // were N strikes.
    use synology_filestation_smb::{probe_as, Probe};

    let _servers = TestServers::start().await.expect("docker compose up");
    let host = format!("127.0.0.1:{}", smb2::testing::auth_port());
    let Probe::Http(_) = probe_as(&host, "testuser", "rotated away", Some("")).await else {
        panic!("the server turns the login down");
    };

    let mut cfg = config_with("rotated away");
    cfg.host = host.clone();
    let other = SmbTransport::unconnected(&cfg, no_redial());
    // Nobody on the far end: if it tried to authenticate, it would hang.
    let (ours, _theirs) = tokio::io::duplex(1024);
    let refused = tokio::time::timeout(Duration::from_secs(2), other.adopt(Box::new(ours), &host))
        .await
        .expect("answered without a round trip")
        .expect_err("still refused");
    assert!(
        synology_filestation_smb::error::is_refused_login(&refused),
        "{refused:?}"
    );
    assert!(
        other.refused().is_some(),
        "and latched on that transport too"
    );
}
