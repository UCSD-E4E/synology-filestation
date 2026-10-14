//! Moving a file or folder on the NAS without its bytes leaving the NAS.
//!
//! A cross-directory `mv` on the mount used to download the whole file,
//! upload it again and delete the original: a full copy through this
//! machine, held in memory, for what both SMB and FileStation can do on the
//! appliance. `move_to` asks a metadata backend first (SMB renames across
//! directories in one SET_INFO), and otherwise FileStation's CopyMove task
//! with `remove_src`. Its parameters are JSON, as every v2+ call's are: the
//! MD5 task's bare id is what made DSM answer 599 in 0.9.0.
//!
//! What it may replace is `rename(2)`'s rule, not CopyMove's: a file
//! replaces a file, a folder only an empty folder. CopyMove with `overwrite`
//! merges a folder into one already there and overwrites what both hold.

use std::collections::HashMap;

use super::*;
use crate::transport::{plan_move, Destination, MovePlan};

const TASKID: &str = "FileStation_7A8B9C0D1E2F";

/// What DSM knows: each path, a file or a folder; a folder's entries.
struct Nas {
    entries: Vec<(&'static str, bool)>,
}

impl Nas {
    /// Answer `getinfo` per path, absent ones the way DSM does (a per-entry
    /// 408), and `list` per folder with what is under it.
    async fn mount(&self, server: &MockServer) {
        for (path, isdir) in &self.entries {
            Mock::given(method("GET"))
                .and(query_param("method", "getinfo"))
                .and(query_param("path", serde_json::to_string(&[path]).unwrap().as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": true,
                    "data": {"files": [{"name": path.rsplit('/').next().unwrap(), "path": path, "isdir": isdir}]}
                })))
                .mount(server)
                .await;
            if *isdir {
                let prefix = format!("{path}/");
                let children: Vec<_> = self
                    .entries
                    .iter()
                    .filter(|(p, _)| p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
                    .map(|(p, d)| serde_json::json!({"name": p.rsplit('/').next().unwrap(), "path": p, "isdir": d}))
                    .collect();
                Mock::given(method("GET"))
                    .and(query_param("method", "list"))
                    .and(query_param("folder_path", *path))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "success": true,
                        "data": {"total": children.len(), "offset": 0, "files": children}
                    })))
                    .mount(server)
                    .await;
            }
        }
        Mock::given(method("GET"))
            .and(query_param("method", "getinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "data": {"files": [{"path": "", "code": 408}]}
            })))
            .with_priority(10)
            .mount(server)
            .await;
    }
}

/// The CopyMove task: `start` hands back an id, `status` answers under the
/// JSON-quoted id only, with `status`.
async fn mount_copymove(server: &MockServer, status: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("api", "SYNO.FileStation.CopyMove"))
        .and(query_param("method", "start"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"taskid": TASKID}
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("api", "SYNO.FileStation.CopyMove"))
        .and(query_param("method", "status"))
        .and(query_param("taskid", format!("\"{TASKID}\"").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(status))
        .mount(server)
        .await;
}

async fn mount_rename(server: &MockServer, reply: serde_json::Value) {
    Mock::given(method("GET"))
        .and(query_param("api", "SYNO.FileStation.Rename"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .mount(server)
        .await;
}

fn renamed_ok() -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "data": {"files": [{"name": "x", "path": "/share/a/x", "isdir": false}]}
    })
}

fn finished() -> serde_json::Value {
    serde_json::json!({"success": true, "data": {"finished": true, "progress": 1}})
}

/// Every request naming `api`, as its parameters.
async fn calls_to(server: &MockServer, api: &str) -> Vec<HashMap<String, String>> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.query_pairs().into_owned().collect::<HashMap<_, _>>())
        .filter(|q| q.get("api").map(String::as_str) == Some(api))
        .collect()
}

async fn any_bytes_moved(server: &MockServer) -> bool {
    server.received_requests().await.unwrap().iter().any(|r| {
        r.method.as_str() == "POST"
            || r.url
                .query()
                .is_some_and(|q| q.contains("SYNO.FileStation.Download"))
    })
}

// ── What a move may replace ──────────────────────────────────────────────

#[test]
fn a_move_replaces_what_rename_would_and_nothing_else() {
    assert!(matches!(
        plan_move(false, Destination::Absent),
        Ok(MovePlan::Fresh)
    ));
    assert!(matches!(
        plan_move(true, Destination::Absent),
        Ok(MovePlan::Fresh)
    ));
    assert!(matches!(
        plan_move(false, Destination::File),
        Ok(MovePlan::ReplaceFile)
    ));
    assert!(matches!(
        plan_move(true, Destination::Dir { empty: true }),
        Ok(MovePlan::ReplaceEmptyDir)
    ));
    assert!(matches!(
        plan_move(true, Destination::Dir { empty: false }),
        Err(SynoFsError::NotEmpty)
    ));
    // A file and a folder never replace each other. The kernel refuses these
    // before a FUSE mount is asked; anyone else calling `move_to` is told.
    assert!(plan_move(false, Destination::Dir { empty: true }).is_err());
    assert!(plan_move(true, Destination::File).is_err());
}

// ── FileStation's CopyMove ───────────────────────────────────────────────

#[tokio::test]
async fn a_move_is_one_copymove_task_with_its_parameters_as_json() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/img.ORF", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_copymove(&server, finished()).await;

    client_for(&server)
        .move_to("/share/a/img.ORF", "/share/b/img.ORF")
        .await
        .expect("moved on the NAS");

    let calls = calls_to(&server, "SYNO.FileStation.CopyMove").await;
    let start = &calls[0];
    assert_eq!(start["method"], "start");
    assert_eq!(start["path"], r#"["/share/a/img.ORF"]"#);
    assert_eq!(start["dest_folder_path"], r#""/share/b""#);
    assert_eq!(start["remove_src"], "true", "a move, not a copy");
    assert!(
        !start.contains_key("overwrite"),
        "nothing to replace, so a name that turns up meanwhile is an error, not a casualty"
    );
    assert_eq!(calls[1]["method"], "status");
    assert_eq!(calls.len(), 2, "nothing after the task reported finished");
    assert!(
        !any_bytes_moved(&server).await,
        "no file bytes left the NAS"
    );
}

/// `rename(2)` replaces a file with a file.
#[tokio::test]
async fn a_file_replaces_a_file() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/f.bin", false), ("/share/b/f.bin", false)],
    }
    .mount(&server)
    .await;
    mount_copymove(&server, finished()).await;

    client_for(&server)
        .move_to("/share/a/f.bin", "/share/b/f.bin")
        .await
        .expect("replaced");
    assert_eq!(
        calls_to(&server, "SYNO.FileStation.CopyMove").await[0]["overwrite"],
        "true"
    );
}

/// Regression (review of #319): CopyMove with `overwrite` merged a folder
/// into one already there, overwrote what both held, then deleted the
/// source. `rename(2)` refuses: ENOTEMPTY.
#[tokio::test]
async fn a_folder_is_never_merged_into_a_folder_with_something_in_it() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![
            ("/share/a/photos", true),
            ("/share/a/photos/x.ORF", false),
            ("/share/b/photos", true),
            ("/share/b/photos/x.ORF", false),
        ],
    }
    .mount(&server)
    .await;
    mount_copymove(&server, finished()).await;

    let err = client_for(&server)
        .move_to("/share/a/photos", "/share/b/photos")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::NotEmpty), "got {err:?}");
    assert!(calls_to(&server, "SYNO.FileStation.CopyMove")
        .await
        .is_empty());
}

/// An empty folder in the way is replaced, as `rename(2)` replaces it: gone
/// first, then the move, with nothing to overwrite.
#[tokio::test]
async fn a_folder_replaces_an_empty_folder() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/photos", true), ("/share/b/photos", true)],
    }
    .mount(&server)
    .await;
    Mock::given(method("GET"))
        .and(query_param("api", "SYNO.FileStation.Delete"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true})),
        )
        .mount(&server)
        .await;
    mount_copymove(&server, finished()).await;

    client_for(&server)
        .move_to("/share/a/photos", "/share/b/photos")
        .await
        .expect("replaced the empty folder");
    let deleted = calls_to(&server, "SYNO.FileStation.Delete").await;
    assert_eq!(deleted.len(), 1);
    let start = &calls_to(&server, "SYNO.FileStation.CopyMove").await[0];
    assert!(!start.contains_key("overwrite"));
}

#[tokio::test]
async fn a_task_that_reports_failure_is_an_error() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/f.bin", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_copymove(
        &server,
        serde_json::json!({"success": false, "error": {"code": 1001}}),
    )
    .await;

    let err = client_for(&server)
        .move_to("/share/a/f.bin", "/share/b/f.bin")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::ApiError(1001)), "got {err:?}");
}

/// The task ran, and may have moved part of a folder: that is not a reason
/// to try a different way, which is what `NotSupported` would invite.
#[tokio::test]
async fn a_failure_reported_inside_a_finished_task_is_an_error_too() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/f.bin", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_copymove(
        &server,
        serde_json::json!({"success": true, "data": {"finished": true, "error": {"code": 1002}}}),
    )
    .await;

    let err = client_for(&server)
        .move_to("/share/a/f.bin", "/share/b/f.bin")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::ApiError(1002)), "got {err:?}");
}

/// Refused before it started — no such API, method or version, or the
/// parameters were not understood — nothing has moved, so the caller may
/// fall back to copying.
#[tokio::test]
async fn a_task_that_never_started_is_not_supported() {
    for code in [101, 102, 103, 104] {
        let server = MockServer::start().await;
        Nas {
            entries: vec![("/share/a/f.bin", false), ("/share/b", true)],
        }
        .mount(&server)
        .await;
        Mock::given(method("GET"))
            .and(query_param("api", "SYNO.FileStation.CopyMove"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false, "error": {"code": code}
            })))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .move_to("/share/a/f.bin", "/share/b/f.bin")
            .await
            .unwrap_err();
        assert!(
            matches!(err, SynoFsError::NotSupported),
            "{code}: got {err:?}"
        );
    }
}

/// Regression (review of #319): a task that never said `finished` was
/// polled forever, holding a transfer slot and the paths it was moving.
#[tokio::test]
async fn a_task_that_never_finishes_is_given_up_on_without_being_stopped() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/f.bin", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_copymove(
        &server,
        serde_json::json!({"success": true, "data": {"finished": false}}),
    )
    .await;

    let err = client_for(&server)
        .move_within(
            "/share/a/f.bin",
            "/share/b/f.bin",
            Duration::from_millis(1200),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SynoFsError::Io(m) if m.contains("still running")),
        "got {err:?}"
    );
    // Stopping a move halfway leaves a folder split between two places; the
    // task is left to finish.
    let stopped = calls_to(&server, "SYNO.FileStation.CopyMove")
        .await
        .iter()
        .any(|q| q["method"] == "stop");
    assert!(!stopped);
}

// ── A move that renames too ──────────────────────────────────────────────

/// CopyMove keeps the name. A move that also renames is a rename in place,
/// then the move: DSM's Rename never overwrites, so that order cannot
/// clobber an unrelated file in either directory.
#[tokio::test]
async fn a_move_that_renames_renames_in_place_first() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/old.ORF", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_rename(&server, renamed_ok()).await;
    mount_copymove(&server, finished()).await;

    client_for(&server)
        .move_to("/share/a/old.ORF", "/share/b/new.ORF")
        .await
        .expect("renamed and moved");

    let rename = &calls_to(&server, "SYNO.FileStation.Rename").await[0];
    assert_eq!(rename["path"], r#"["/share/a/old.ORF"]"#);
    assert_eq!(rename["name"], r#"["new.ORF"]"#);
    let start = &calls_to(&server, "SYNO.FileStation.CopyMove").await[0];
    assert_eq!(start["path"], r#"["/share/a/new.ORF"]"#);
    assert_eq!(start["dest_folder_path"], r#""/share/b""#);
}

/// The new name is taken where the file is, so the rename in place is
/// refused and nothing has moved: the caller may copy instead.
#[tokio::test]
async fn a_move_that_renames_onto_a_taken_name_is_not_supported() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/old.ORF", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_rename(
        &server,
        serde_json::json!({"success": false, "error": {"code": 418}}),
    )
    .await;

    let err = client_for(&server)
        .move_to("/share/a/old.ORF", "/share/b/new.ORF")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::NotSupported), "got {err:?}");
    assert!(calls_to(&server, "SYNO.FileStation.CopyMove")
        .await
        .is_empty());
}

/// Regression (review of #319): a rename refused for permission was turned
/// into "copy it instead", which only fails slower, after a download.
#[tokio::test]
async fn a_rename_refused_for_permission_is_reported_as_that() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/old.ORF", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_rename(
        &server,
        serde_json::json!({"success": false, "error": {"code": 408}}),
    )
    .await;

    let err = client_for(&server)
        .move_to("/share/a/old.ORF", "/share/b/new.ORF")
        .await
        .unwrap_err();
    assert_eq!(
        err.category(),
        crate::error::ErrorCategory::PermissionDenied,
        "got {err:?}"
    );
}

/// Renamed in place, and the task was refused before it started: nothing
/// moved, so the name goes back.
#[tokio::test]
async fn a_move_refused_after_the_rename_puts_the_name_back() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/a/old.ORF", false), ("/share/b", true)],
    }
    .mount(&server)
    .await;
    mount_rename(&server, renamed_ok()).await;
    Mock::given(method("GET"))
        .and(query_param("api", "SYNO.FileStation.CopyMove"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 103}
        })))
        .mount(&server)
        .await;

    let err = client_for(&server)
        .move_to("/share/a/old.ORF", "/share/b/new.ORF")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::NotSupported), "got {err:?}");

    let renames = calls_to(&server, "SYNO.FileStation.Rename").await;
    assert_eq!(renames.len(), 2, "renamed, then renamed back");
    assert_eq!(renames[1]["path"], r#"["/share/a/new.ORF"]"#);
    assert_eq!(renames[1]["name"], r#"["old.ORF"]"#);
}

/// Regression (review of #319): any failure renamed the source back — even
/// one from a status poll, while the task might still be moving it, or
/// have finished.
#[tokio::test]
async fn a_task_that_started_is_never_undone_from_here() {
    for status in [
        serde_json::json!({"success": false, "error": {"code": 599}}),
        serde_json::json!({"success": false, "error": {"code": 1001}}),
    ] {
        let server = MockServer::start().await;
        Nas {
            entries: vec![("/share/a/old.ORF", false), ("/share/b", true)],
        }
        .mount(&server)
        .await;
        mount_rename(&server, renamed_ok()).await;
        mount_copymove(&server, status.clone()).await;

        assert!(client_for(&server)
            .move_to("/share/a/old.ORF", "/share/b/new.ORF")
            .await
            .is_err());
        assert_eq!(
            calls_to(&server, "SYNO.FileStation.Rename").await.len(),
            1,
            "{status}: the rename in place stands"
        );
    }
}

// ── Starting the task once ───────────────────────────────────────────────

/// A server that answers every request with headers promising more body than
/// it sends, then hangs up: the body read fails after the request has
/// arrived. Counts what it was sent.
async fn truncating_server() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"succ")
                    .await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), seen)
}

/// Regression (review of #319): `start` went through the retrying GET,
/// which sends again after a failed body read — by which point DSM may have
/// queued the move, and a second task races the first.
#[tokio::test]
async fn the_task_is_started_once_even_when_its_answer_is_lost() {
    let (addr, seen) = truncating_server().await;
    let (host, port) = addr.split_once(':').unwrap();
    let client = SynologyClient::new(host, port.parse().unwrap(), false);

    let err = client
        .copymove_start("/share/a/f.bin", "/share/b", false)
        .await
        .unwrap_err();
    assert!(
        !matches!(err, SynoFsError::NotSupported),
        "it may have started: {err:?}"
    );
    assert_eq!(seen.load(Ordering::SeqCst), 1, "sent once");
}

// ── Backends ─────────────────────────────────────────────────────────────

/// A backend that can move — SMB — is asked first, and HTTP is not touched.
#[tokio::test]
async fn a_backend_that_can_move_is_asked_first() {
    let server = MockServer::start().await;
    let backend = FakeMeta::new(Behave::Ok(b""));
    let client = client_for(&server).with_metadata_transport(backend.clone());

    client
        .move_to("/share/a/f.bin", "/share/b/f.bin")
        .await
        .expect("moved by the backend");

    assert_eq!(backend.call_count(), 1);
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// One that declines — across shares, for SMB — leaves it to FileStation.
#[tokio::test]
async fn a_backend_that_declines_leaves_it_to_filestation() {
    let server = MockServer::start().await;
    Nas {
        entries: vec![("/share/f.bin", false), ("/other", true)],
    }
    .mount(&server)
    .await;
    mount_copymove(&server, finished()).await;
    let backend = FakeMeta::new(Behave::Declines);
    let client = client_for(&server).with_metadata_transport(backend.clone());

    client
        .move_to("/share/f.bin", "/other/f.bin")
        .await
        .expect("moved by FileStation");
    assert_eq!(
        calls_to(&server, "SYNO.FileStation.CopyMove").await.len(),
        2
    );
}
