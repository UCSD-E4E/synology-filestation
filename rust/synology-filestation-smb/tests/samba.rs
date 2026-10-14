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
    // Regression (Copilot, #315): it asked nobody, so it was not refused —
    // the gate turned it away, and it says so.
    assert_eq!(
        other.fallback().map(|f| f.kind),
        Some(synology_filestation_smb::FallbackKind::AuthCooldown)
    );
}

// ── Moving without copying ───────────────────────────────────────────────────

/// A cross-directory `mv` on the mount was a download, an upload and a
/// delete. On SMB it is one rename, and it replaces what is at the
/// destination as `rename(2)` does.
#[tokio::test]
#[ignore = "needs Docker"]
async fn a_move_across_directories_is_one_rename_that_replaces() {
    let servers = TestServers::start().await.expect("docker compose up");
    let smb = connect(&servers).await;
    for dir in ["mv-a", "mv-b"] {
        let _ = smb.create_folder("/private", dir).await;
    }
    smb.write_atomic("/private/mv-a/img.ORF", b"the raw")
        .await
        .unwrap();
    smb.write_atomic("/private/mv-b/img.ORF", b"stale")
        .await
        .unwrap();

    smb.move_to("/private/mv-a/img.ORF", "/private/mv-b/img.ORF")
        .await
        .expect("moved");
    assert_eq!(
        &smb.read_full("/private/mv-b/img.ORF").await.unwrap()[..],
        b"the raw"
    );
    assert!(
        smb.get_info("/private/mv-a/img.ORF").await.is_err(),
        "the source is gone"
    );

    // Another directory and another name, in the same one operation.
    smb.move_to("/private/mv-b/img.ORF", "/private/mv-a/renamed.ORF")
        .await
        .expect("moved and renamed");
    assert_eq!(
        &smb.read_full("/private/mv-a/renamed.ORF").await.unwrap()[..],
        b"the raw"
    );

    // A folder moves the same way, with what is in it.
    smb.move_to("/private/mv-a", "/private/mv-b/mv-a")
        .await
        .expect("folder moved");
    assert_eq!(
        &smb.read_full("/private/mv-b/mv-a/renamed.ORF")
            .await
            .unwrap()[..],
        b"the raw"
    );

    smb.delete("/private/mv-b/mv-a/renamed.ORF").await.ok();
    smb.delete("/private/mv-b/mv-a").await.ok();
    smb.delete("/private/mv-b").await.ok();
}

/// Regression (review of #319): `rename(2)`'s rule, not a merge. A folder
/// replaces only an empty folder, a non-empty one is ENOTEMPTY and nothing
/// moves, and a file never replaces a folder.
#[tokio::test]
#[ignore = "needs Docker"]
async fn a_move_replaces_only_what_rename_would() {
    use synology_filestation_core::SynoFsError;

    let servers = TestServers::start().await.expect("docker compose up");
    let smb = connect(&servers).await;
    for dir in ["mv2-src", "mv2-full", "mv2-empty"] {
        let _ = smb.create_folder("/private", dir).await;
    }
    smb.write_atomic("/private/mv2-src/x.ORF", b"source")
        .await
        .unwrap();
    smb.write_atomic("/private/mv2-full/x.ORF", b"keep me")
        .await
        .unwrap();

    let err = smb
        .move_to("/private/mv2-src", "/private/mv2-full")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::NotEmpty), "got {err:?}");
    assert_eq!(
        &smb.read_full("/private/mv2-full/x.ORF").await.unwrap()[..],
        b"keep me"
    );
    assert_eq!(
        &smb.read_full("/private/mv2-src/x.ORF").await.unwrap()[..],
        b"source"
    );

    let err = smb
        .move_to("/private/mv2-src/x.ORF", "/private/mv2-full")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SynoFsError::AlreadyExists),
        "a file onto a folder: {err:?}"
    );

    smb.move_to("/private/mv2-src", "/private/mv2-empty")
        .await
        .expect("an empty folder is replaced");
    assert_eq!(
        &smb.read_full("/private/mv2-empty/x.ORF").await.unwrap()[..],
        b"source"
    );

    for p in [
        "/private/mv2-empty/x.ORF",
        "/private/mv2-empty",
        "/private/mv2-full/x.ORF",
        "/private/mv2-full",
    ] {
        smb.delete(p).await.ok();
    }
}

// ── Reading over several connections ─────────────────────────────────────────

/// Opens another connection to the auth container, counting how many it has
/// opened. Each one runs through a proxy whose task is kept, so a test can cut
/// the extra connections and leave the main one alone.
#[derive(Clone, Default)]
struct Extra {
    opened: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    proxies: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Extra {
    fn opener(
        &self,
    ) -> impl Fn() -> synology_filestation_smb::ConnectionFuture + Send + Sync + 'static {
        let this = self.clone();
        move || {
            let this = this.clone();
            Box::pin(async move {
                this.opened
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut server = a_stream_to_the_server().await;
                let (ours, mut theirs) = tokio::io::duplex(1 << 20);
                this.proxies.lock().unwrap().push(tokio::spawn(async move {
                    let _ = tokio::io::copy_bidirectional(&mut theirs, &mut server).await;
                }));
                Ok((
                    Box::new(ours) as synology_filestation_smb::BoxedStream,
                    "127.0.0.1".to_string(),
                ))
            })
        }
    }

    /// Cut every extra connection opened so far, as a dropped link would.
    fn cut(&self) {
        for proxy in self.proxies.lock().unwrap().drain(..) {
            proxy.abort();
        }
    }

    fn opened(&self) -> usize {
        self.opened.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// A file's worth of bytes no two stripes of which are alike.
fn pattern(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i / 7) as u8).wrapping_add(salt).wrapping_mul(31))
        .collect()
}

/// A transport reading over `total` connections, once they are all up.
async fn reading_over(total: usize, extra: &Extra, warm: &str) -> SmbTransport {
    let smb = connect_plain()
        .await
        .with_read_connections(total, extra.opener());
    // The extra connections are opened in the background when reads begin,
    // so a read goes first.
    let _ = smb.read(warm, 0, 1).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while smb.read_connections() < total {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("every connection came up");
    smb
}

async fn connect_plain() -> SmbTransport {
    SmbTransport::connect(&config_with("testpass"))
        .await
        .expect("the auth container should accept these credentials")
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn reads_over_several_connections_return_the_same_bytes() {
    let _servers = TestServers::start().await.expect("docker compose up");
    let path = "/private/striped.bin";
    let original = pattern(3 * 1024 * 1024 + 12345, 1);
    connect_plain()
        .await
        .write_atomic(path, &original)
        .await
        .expect("write");

    let extra = Extra::default();
    let smb = reading_over(4, &extra, path).await;
    assert_eq!(extra.opened(), 3, "the main session plus three more");

    for (offset, length) in [
        (0u64, original.len() as u64),
        (1, 256 * 1024),
        (256 * 1024 - 3, 700_000),
        (2 * 1024 * 1024 + 5, 4 * 1024 * 1024), // runs past the end
        (original.len() as u64 + 10, 4096),     // starts past it
    ] {
        let got = smb.read(path, offset, length).await.expect("read");
        let start = (offset as usize).min(original.len());
        let end = ((offset + length) as usize).min(original.len());
        assert!(
            got.as_ref() == &original[start..end],
            "bytes {offset}+{length} came back as written"
        );
    }
    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn every_connection_sees_an_overwrite() {
    // Each connection holds its own open handle on the file. A write has to
    // invalidate all of them, or a stripe comes back from before the write.
    let _servers = TestServers::start().await.expect("docker compose up");
    let path = "/private/overwritten.bin";
    let len = 2 * 1024 * 1024;
    let first = pattern(len, 1);
    let second = pattern(len, 2);

    let extra = Extra::default();
    let smb = connect_plain()
        .await
        .with_read_connections(4, extra.opener());
    smb.write_atomic(path, &first).await.expect("first write");
    let smb = {
        let _ = smb.read(path, 0, 1).await;
        tokio::time::timeout(Duration::from_secs(20), async {
            while smb.read_connections() < 4 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("every connection came up");
        smb
    };
    assert!(smb.read(path, 0, len as u64).await.unwrap().as_ref() == first.as_slice());

    smb.write_atomic(path, &second).await.expect("second write");

    assert!(
        smb.read(path, 0, len as u64).await.unwrap().as_ref() == second.as_slice(),
        "no stripe came from a handle opened before the write"
    );
    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_read_on_a_connection_that_died_is_served_by_the_main_session() {
    let _servers = TestServers::start().await.expect("docker compose up");
    let path = "/private/survivor.bin";
    let original = pattern(1024 * 1024, 3);
    connect_plain()
        .await
        .write_atomic(path, &original)
        .await
        .expect("write");

    let extra = Extra::default();
    let smb = reading_over(4, &extra, path).await;

    extra.cut();

    let got = smb
        .read(path, 0, original.len() as u64)
        .await
        .expect("the read still succeeds");
    assert!(
        got.as_ref() == original.as_slice(),
        "and returns the right bytes"
    );
    smb.delete(path).await.ok();
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_write_after_reads_that_raced_to_open_the_file_still_replaces_it() {
    // Two first reads of one file at once both miss the handle cache and both
    // open a handle; the second displaces the first while it is still being
    // read, so that one is parked to be closed later. A write then has to
    // close it too, or the replacing rename meets a file still open and the
    // server refuses it. The mount reads a file from several threads at once,
    // so this is the ordinary case, not a contrived one.
    let _servers = TestServers::start().await.expect("docker compose up");
    let path = "/private/raced.bin";
    let smb = connect_plain().await;
    smb.write_atomic(path, &pattern(600_000, 1))
        .await
        .expect("first");

    let (a, b, c) = tokio::join!(
        smb.read(path, 0, 200_000),
        smb.read(path, 200_000, 200_000),
        smb.read(path, 400_000, 200_000)
    );
    a.and(b).and(c).expect("the reads");

    smb.write_atomic(path, &pattern(600_000, 2))
        .await
        .expect("the second write replaces the file");
    assert!(smb.read_full(path).await.unwrap().as_ref() == pattern(600_000, 2).as_slice());
    smb.delete(path).await.ok();
}
