//! `mv` across directories through a real kernel mount: the NAS moves it.
//!
//! A cross-directory rename used to be a download, an upload and a delete,
//! and a folder could not be moved at all (`ENOSYS`). The unit tests cover
//! the policy; this covers the half they cannot — that `rename(2)` on the
//! mount, for a file and for a folder, reaches FileStation's CopyMove and
//! sends no file bytes through this machine.
//!
//! Ignored by default: it needs a real FUSE mount, which CI containers do not
//! have. Run it deliberately with `--ignored`, on a machine with `/dev/fuse`.
#![cfg(target_os = "linux")]

use std::sync::Arc;

use synology_filestation_core::SynologyClient;
use synology_filestation_fuse::{spawn_mount, MountOptions};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true, "data": data}))
}

fn entry(path: &str, isdir: bool) -> serde_json::Value {
    let name = path.rsplit('/').next().unwrap();
    serde_json::json!({"name": name, "path": path, "isdir": isdir, "additional": {"size": 4096}})
}

/// A cold lookup asks for the one entry rather than its directory.
async fn info(server: &MockServer, path: &str, isdir: bool) {
    Mock::given(query_param("method", "getinfo"))
        .and(query_param(
            "path",
            serde_json::to_string(&[path]).unwrap().as_str(),
        ))
        .respond_with(ok(serde_json::json!({"files": [entry(path, isdir)]})))
        .mount(server)
        .await;
}

async fn listing(server: &MockServer, folder: &str, files: Vec<serde_json::Value>) {
    Mock::given(query_param("method", "list"))
        .and(query_param("folder_path", folder))
        .respond_with(ok(serde_json::json!({
            "total": files.len(), "offset": 0, "files": files
        })))
        .mount(server)
        .await;
}

#[test]
#[ignore = "needs a real FUSE mount; run with --ignored"]
fn mv_across_directories_is_moved_by_the_nas() {
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
        listing(
            &server,
            "/share",
            vec![entry("/share/a", true), entry("/share/b", true)],
        )
        .await;
        listing(
            &server,
            "/share/a",
            vec![
                entry("/share/a/img.ORF", false),
                entry("/share/a/photos", true),
            ],
        )
        .await;
        listing(&server, "/share/b", vec![]).await;
        for (p, isdir) in [
            ("/share/a", true),
            ("/share/b", true),
            ("/share/a/img.ORF", false),
            ("/share/a/photos", true),
        ] {
            info(&server, p, isdir).await;
        }
        // Anything else is absent, the way DSM says it: a per-entry 408. The
        // kernel looks the destination up before it renames onto it.
        Mock::given(query_param("method", "getinfo"))
            .respond_with(ok(
                serde_json::json!({"files": [{"path": "", "code": 408}]}),
            ))
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(query_param("api", "SYNO.FileStation.CopyMove"))
            .and(query_param("method", "start"))
            .respond_with(ok(serde_json::json!({"taskid": "FileStation_1"})))
            .mount(&server)
            .await;
        Mock::given(query_param("api", "SYNO.FileStation.CopyMove"))
            .and(query_param("method", "status"))
            .respond_with(ok(serde_json::json!({"finished": true})))
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
    let mnt = dir.path().to_path_buf();
    let handle = spawn_mount(
        client,
        rt.handle().clone(),
        mnt.clone(),
        MountOptions::default(),
    )
    .expect("mounted");

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        std::fs::rename(mnt.join("share/a/img.ORF"), mnt.join("share/b/img.ORF"))
            .expect("a file moves");
        std::fs::rename(mnt.join("share/a/photos"), mnt.join("share/b/photos"))
            .expect("a folder moves, where it used to be ENOSYS");

        let requests = rt.block_on(server.received_requests()).unwrap_or_default();
        let moved: Vec<String> = requests
            .iter()
            .filter(|r| r.url.query().is_some_and(|q| q.contains("method=start")))
            .map(|r| {
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == "path")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(moved, [r#"["/share/a/img.ORF"]"#, r#"["/share/a/photos"]"#]);
        assert!(
            !requests.iter().any(|r| r.method.as_str() == "POST"
                && r.url.path() != "/webapi/auth.cgi"
                || r.url.query().is_some_and(|q| q.contains("method=download"))),
            "no file bytes through this machine"
        );
    }));

    handle.stop();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
