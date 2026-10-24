//! A process walking the mount, seen through a real kernel mount.
//!
//! The unit tests (`walks`) cover the rules with thread ids made up by hand;
//! this covers what they cannot: that the id the kernel puts on an `opendir`
//! leads back to the process that asked, that the mount's own handle lists it,
//! and that stopping it from there ends it.
//!
//! Ignored by default: it needs a real FUSE mount, which CI containers do not
//! have. Run it deliberately with `--ignored`, on a machine with `/dev/fuse`.
#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use synology_filestation_core::SynologyClient;
use synology_filestation_fuse::walks::HELD_DIRS;
use synology_filestation_fuse::{spawn_mount, MountOptions};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true, "data": data}))
}

/// Polls `f` until it answers, or gives up after `within`.
fn eventually<T>(within: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
#[ignore = "needs a real FUSE mount; run with --ignored"]
fn a_process_holding_the_mount_open_is_named_and_can_be_stopped() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webapi/auth.cgi"))
            .respond_with(ok(serde_json::json!({"sid": "abc"})))
            .mount(&server)
            .await;
        Mock::given(query_param("method", "list_share"))
            .respond_with(ok(serde_json::json!({
                "total": 1, "offset": 0,
                "shares": [{"name": "share", "path": "/share", "isdir": true}]
            })))
            .mount(&server)
            .await;
        Mock::given(query_param("method", "list"))
            .respond_with(ok(
                serde_json::json!({"total": 0, "offset": 0, "files": []}),
            ))
            .mount(&server)
            .await;
        server
    });

    let url = server.uri();
    let hostport = url.strip_prefix("http://").expect("http").to_string();
    let (host, port) = hostport.split_once(':').expect("host:port");
    let client = Arc::new(SynologyClient::new(host, port.parse().unwrap(), false));
    rt.block_on(client.login("someone", "", None))
        .expect("logged in");

    let dir = tempfile::tempdir().expect("a mountpoint");
    let mountpoint = dir.path().to_path_buf();
    let handle = spawn_mount(
        client,
        rt.handle().clone(),
        mountpoint.clone(),
        MountOptions::default(),
    )
    .expect("mounted");
    let share = mountpoint.join("share");

    // What bfs does while it walks: hold a great many directories open. Each
    // `exec {fd}<dir` is an open(2) of the directory, so one OPENDIR. `exec
    // sleep` keeps the process (and its descriptors) and waits to be stopped.
    let script = format!(
        "ulimit -n {limit}; for i in $(seq {HELD_DIRS}); do exec {{fd}}<\"$1\" || exit 3; done; \
         echo ready; exec sleep 60",
        limit = HELD_DIRS + 64,
    );
    let mut child = Command::new("bash")
        .args(["-c", &script, "walker"])
        .arg(&share)
        .stdout(Stdio::piped())
        .spawn()
        .expect("bash");
    let pid = child.id();

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let walker = eventually(Duration::from_secs(20), || {
            handle.walkers().into_iter().find(|w| w.pid == pid)
        })
        .unwrap_or_else(|| panic!("pid {pid} was never listed: {:?}", handle.walkers()));
        assert!(walker.dirs_open >= HELD_DIRS, "{walker:?}");
        assert_eq!(walker.latest, share.display().to_string());

        handle.stop_walker(pid).expect("stopped");
        let status = eventually(Duration::from_secs(10), || child.try_wait().unwrap())
            .expect("the walker exits");
        assert_eq!(status.signal(), Some(libc::SIGTERM));

        // Its handles close as it dies, so it is no longer walking.
        eventually(Duration::from_secs(10), || {
            handle.walkers().is_empty().then_some(())
        })
        .unwrap_or_else(|| panic!("still listed: {:?}", handle.walkers()));
    }));

    let _ = child.kill();
    let _ = child.wait();
    handle.stop();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e);
    }
}
