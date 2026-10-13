//! Moving a file or folder on the NAS without its bytes leaving the NAS.
//!
//! A cross-directory `mv` on the mount used to download the whole file,
//! upload it again and delete the original: a full copy through this
//! machine, held in memory, for what both SMB and FileStation can do on the
//! appliance. `move_to` asks a metadata backend first (SMB renames across
//! directories in one SET_INFO), and otherwise FileStation's CopyMove task
//! with `remove_src`. Its parameters are JSON, as every v2+ call's are: the
//! MD5 task's bare id is what made DSM answer 599 in 0.9.0.

use super::*;

const TASKID: &str = "FileStation_7A8B9C0D1E2F";

/// The CopyMove task as DSM answers it: `start` hands back an id, `status`
/// answers under the JSON-quoted id only, `finished` on the first poll.
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

fn finished() -> serde_json::Value {
    serde_json::json!({"success": true, "data": {"finished": true, "progress": 1}})
}

/// Every request's `(method, params)`, for the ones naming `api`.
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

use std::collections::HashMap;

#[tokio::test]
async fn a_move_is_one_copymove_task_with_its_parameters_as_json() {
    let server = MockServer::start().await;
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
    assert_eq!(
        start["overwrite"], "true",
        "rename(2) replaces the destination"
    );
    assert_eq!(calls[1]["method"], "status");
    assert_eq!(calls.len(), 2, "nothing after the task reported finished");
    assert!(
        !any_bytes_moved(&server).await,
        "no file bytes left the NAS"
    );
}

#[tokio::test]
async fn a_task_that_reports_failure_is_an_error() {
    let server = MockServer::start().await;
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

/// CopyMove keeps the name. A move that also renames is a rename in place,
/// then the move: DSM's Rename never overwrites, so that order cannot
/// clobber an unrelated file in either directory.
#[tokio::test]
async fn a_move_that_renames_renames_in_place_first() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("method", "getinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "data": {"files": [{"name": "new.ORF", "path": "/share/a/new.ORF", "code": 408}]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("api", "SYNO.FileStation.Rename"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "data": {"files": [{"name": "new.ORF", "path": "/share/a/new.ORF", "isdir": false}]}
        })))
        .mount(&server)
        .await;
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

/// The new name is taken where the file is: renaming in place would fail,
/// and moving first could replace the wrong file. Nothing is touched, and
/// the caller may copy instead.
#[tokio::test]
async fn a_move_that_renames_onto_a_taken_name_is_not_supported() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("method", "getinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "data": {"files": [{"name": "new.ORF", "path": "/share/a/new.ORF", "isdir": false}]}
        })))
        .mount(&server)
        .await;

    let err = client_for(&server)
        .move_to("/share/a/old.ORF", "/share/b/new.ORF")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::NotSupported), "got {err:?}");
    assert!(calls_to(&server, "SYNO.FileStation.Rename")
        .await
        .is_empty());
    assert!(calls_to(&server, "SYNO.FileStation.CopyMove")
        .await
        .is_empty());
}

/// Renamed in place, then the move failed: put the name back rather than
/// leave the file under a name nobody asked for, in a place nobody moved it
/// to.
#[tokio::test]
async fn a_failed_move_after_the_rename_puts_the_name_back() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("method", "getinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "data": {"files": [{"name": "new.ORF", "path": "/share/a/new.ORF", "code": 408}]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("api", "SYNO.FileStation.Rename"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "data": {"files": [{"name": "x", "path": "/share/a/x", "isdir": false}]}
        })))
        .mount(&server)
        .await;
    mount_copymove(
        &server,
        serde_json::json!({"success": false, "error": {"code": 1001}}),
    )
    .await;

    let err = client_for(&server)
        .move_to("/share/a/old.ORF", "/share/b/new.ORF")
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::ApiError(1001)), "got {err:?}");

    let renames = calls_to(&server, "SYNO.FileStation.Rename").await;
    assert_eq!(renames.len(), 2, "renamed, then renamed back");
    assert_eq!(renames[1]["path"], r#"["/share/a/new.ORF"]"#);
    assert_eq!(renames[1]["name"], r#"["old.ORF"]"#);
}

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
