//! Making a slice upload safe: retry, verification, MD5, and lost partials.

use super::*;

// ── slice upload: retry, and the verification that makes it safe ──────────
//
// DSM offers no resume. The server appends each slice to its tmpfile and
// never reports how many bytes it holds — `FileUploader_T9JY.js` computes
// every offset client-side and, on any error, gives up on the whole file.
// So a resent slice is exact only when the request never reached the
// server; if the body went out and the answer was lost, resending may
// append the same 10 MiB twice.
//
// We resend anyway, because the alternative is discarding a multi-GB
// upload over one blip, and we make it safe by checking what actually
// landed: the size always, plus a server-side MD5 (SYNO.FileStation.MD5
// v2 — the API File Station's own properties dialog calls) whenever a
// resend could have doubled a slice. A retry on the *first* slice can't
// double anything: without a tmpfile handle the resend opens a fresh
// partial, so it skips the hash.

/// md5 of `scratch_file(_, 2500)`'s byte pattern, from `md5sum` rather than
/// from our own hasher, so the test can disagree with the implementation.
const SCRATCH_2500_MD5: &str = "babbd9d63dca99cb8d4cc054ba70829d";

fn slice_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "success": true,
        "data": {"blSkip": false, "tmpfile": "slice.1.0.9224"}
    }))
}

/// Answer `getinfo` for the uploaded file with `size` bytes.
async fn mount_getinfo_size(server: &MockServer, size: u64) {
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "getinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "data": {"files": [{
                "name": "big.bin",
                "path": "/share/big.bin",
                "isdir": false,
                "additional": {"size": size, "owner": null, "time": null, "perm": null}
            }]}
        })))
        .mount(server)
        .await;
}

/// Answer the two-step MD5 task API with `digest`.
async fn mount_md5(server: &MockServer, digest: &str) {
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("api", "SYNO.FileStation.MD5"))
        .and(query_param("method", "start"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"taskid": "md5-1"}
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("api", "SYNO.FileStation.MD5"))
        .and(query_param("method", "status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"finished": true, "md5": digest}
        })))
        .mount(server)
        .await;
}

async fn md5_calls(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.url
                .query()
                .is_some_and(|q| q.contains("SYNO.FileStation.MD5"))
        })
        .count()
}

#[tokio::test]
async fn slice_upload_resends_a_failed_slice_on_the_same_tmpfile() {
    let server = MockServer::start().await;
    // Slice 1 goes out, slice 2 gets a 503, then everything succeeds.
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;
    mount_md5(&server, SCRATCH_2500_MD5).await;

    let local = scratch_file("retry.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .expect("a blip on one slice does not cost the file");

    let posts = slice_posts(&server).await;
    assert_eq!(posts.len(), 4, "3 slices plus one resend");
    // The resend continues the same partial file rather than starting over.
    let tmps: Vec<_> = posts.iter().map(|r| header_of(r, "X-TMP-FILE")).collect();
    assert_eq!(tmps[1], tmps[2], "the resend targets the same tmpfile");
    assert_eq!(
        header_of(&posts[3], "X-FILE-CHUNK-END").as_deref(),
        Some("true"),
        "the upload still terminates on the final slice"
    );
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_hashes_the_result_after_a_resend_that_could_have_doubled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;
    mount_md5(&server, SCRATCH_2500_MD5).await;

    let local = scratch_file("hashed.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap();

    assert!(
        md5_calls(&server).await >= 2,
        "a risky resend is verified by start + status"
    );
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_skips_the_hash_when_nothing_could_have_doubled() {
    // The happy path pays for one getinfo, never for a NAS-side hash of a
    // multi-GB file.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;
    mount_md5(&server, SCRATCH_2500_MD5).await;

    let local = scratch_file("clean.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap();

    assert_eq!(md5_calls(&server).await, 0, "no resend, no hash");
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn resending_the_first_slice_needs_no_hash() {
    // Slice 1 has no tmpfile to append to, so its resend opens a fresh
    // partial file. Nothing can be doubled, and the orphaned partial is the
    // server's to reap.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;
    mount_md5(&server, SCRATCH_2500_MD5).await;

    let local = scratch_file("firstfail.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap();

    let posts = slice_posts(&server).await;
    assert_eq!(posts.len(), 4, "3 slices plus the first slice's resend");
    assert!(
        header_of(&posts[1], "X-TMP-FILE").is_none(),
        "the resend of slice 1 opens a new partial rather than continuing one"
    );
    assert_eq!(md5_calls(&server).await, 0, "nothing could have doubled");
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_fails_when_the_landed_size_is_wrong() {
    // The cheap half of the safety net: a doubled slice that DSM kept makes
    // the file too big, and no hash is needed to see it.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 3524).await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true
        })))
        .mount(&server)
        .await;

    let local = scratch_file("wrongsize.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    let err = client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");

    let deleted = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.query().is_some_and(|q| q.contains("method=delete")));
    assert!(deleted, "a file we cannot vouch for is not left behind");
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_fails_when_the_server_hash_disagrees() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    // Right size, wrong content — exactly what a doubled slice looks like
    // if DSM trims the partial back to X-FILE-SIZE.
    mount_getinfo_size(&server, 2500).await;
    mount_md5(&server, "00000000000000000000000000000000").await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true
        })))
        .mount(&server)
        .await;

    let local = scratch_file("badhash.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    let err = client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");

    let deleted = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.query().is_some_and(|q| q.contains("method=delete")));
    assert!(
        deleted,
        "the corrupt file is removed, not reported as success"
    );
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_accepts_a_result_it_cannot_verify() {
    // DSM answering "no such API" is a verdict, not a hiccup: the appliance
    // simply cannot hash for us. The upload itself succeeded and there is no
    // evidence of harm, so that answer is accepted with a warning rather
    // than turned into a failure — the documented residual risk of
    // resending a slice. Contrast the unreachable case below.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("api", "SYNO.FileStation.MD5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 102}
        })))
        .mount(&server)
        .await;

    let local = scratch_file("noverify.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .expect("an unverifiable upload is not a failed one");
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_gives_up_after_the_attempt_bound() {
    // Bounded, per the outer-retry contract: this client never spins on a
    // slice forever.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let local = scratch_file("hopeless.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    let err = client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
    assert_eq!(
        slice_posts(&server).await.len(),
        3,
        "one slice, three attempts, then the error surfaces"
    );
    std::fs::remove_file(&local).ok();
}

// ── SYNO.FileStation.MD5 ─────────────────────────────────────────────────

#[tokio::test]
async fn md5_polls_the_task_until_it_finishes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "start"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"taskid": "md5-7"}
        })))
        .mount(&server)
        .await;
    // DSM reads the file to answer, so the first status call says "not yet".
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"finished": false}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"finished": true, "md5": "d41d8cd98f00b204e9800998ecf8427e"}
        })))
        .mount(&server)
        .await;

    let digest = client_for(&server).md5("/share/big.bin").await.unwrap();
    assert_eq!(digest, "d41d8cd98f00b204e9800998ecf8427e");

    let taskids: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "taskid")
                .map(|(_, v)| v.to_string())
        })
        .collect();
    assert_eq!(
        taskids,
        vec!["md5-7", "md5-7"],
        "both polls carry the task id start handed back"
    );
}

#[tokio::test]
async fn md5_surfaces_an_api_error_rather_than_polling_forever() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 400}
        })))
        .mount(&server)
        .await;

    let err = client_for(&server).md5("/share/big.bin").await.unwrap_err();
    assert!(matches!(err, SynoFsError::ApiError(400)), "got {err:?}");
}

#[tokio::test]
async fn slice_upload_tolerates_a_size_that_settles() {
    // The listing can lag a write DSM has just accepted — the same lag
    // `clear_for_overwrite` polls through. A disagreement is confirmed
    // before it costs the file, because the alternative is deleting a
    // perfectly good upload.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "getinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "data": {"files": [{
                "name": "big.bin", "path": "/share/big.bin", "isdir": false,
                "additional": {"size": 1024, "owner": null, "time": null, "perm": null}
            }]}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;

    let local = scratch_file("settles.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .expect("a listing that catches up is not a corrupt upload");

    let deleted = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.query().is_some_and(|q| q.contains("method=delete")));
    assert!(!deleted, "a good upload is never deleted");
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_will_not_vouch_when_the_hash_check_cannot_run() {
    // A hash check we could not *reach* is different from one DSM refused:
    // it leaves us with a resend that may have doubled a slice and no way
    // to tell. Report the failure rather than claim a verified write — but
    // do not delete, because there is no evidence the file is bad and
    // destroying a probably-good upload is the worse mistake. The caller's
    // retry re-uploads over it.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("api", "SYNO.FileStation.MD5"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let local = scratch_file("unreachable.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    let err = client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");

    let deleted = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.query().is_some_and(|q| q.contains("method=delete")));
    assert!(
        !deleted,
        "an unverified file is kept; only a proven bad one goes"
    );
    std::fs::remove_file(&local).ok();
}

// ── when the server loses the partial ────────────────────────────────────
//
// Observed against e4e-nas on 2026-08-12: a 540 MiB upload lost its
// connection at slice 54 (`Connection timed out (os error 110)` off-campus,
// where SMB is firewalled and everything goes over HTTP), the slice was
// resent on the same X-TMP-FILE, and DSM answered 401 — "unknown error of
// file operation", its way of saying that partial is no longer a thing it
// will append to. Treating that as fatal loses the whole file.
//
// DSM offers exactly one recovery: a fresh partial. So the upload starts
// over rather than failing, bounded by the same attempt count. A restart
// also clears the doubt from the resend that provoked it — a new tmpfile
// cannot contain a doubled slice.

#[tokio::test]
async fn slice_upload_starts_over_when_the_server_rejects_the_partial() {
    let server = MockServer::start().await;
    // Slice 1 lands, slice 2's connection dies, and the resend is met with
    // 401 — the partial is gone.
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 401}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .mount(&server)
        .await;
    // Nothing of ours landed before the restart.
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "getinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 408}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;

    let local = scratch_file("restart.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .expect("a rejected partial costs the transfer, not the file");

    let posts = slice_posts(&server).await;
    assert_eq!(
        posts.len(),
        6,
        "slice 1, slice 2, its resend, then all 3 slices again"
    );
    assert!(
        header_of(&posts[3], "X-TMP-FILE").is_none(),
        "the restart opens a fresh partial instead of continuing the dead one"
    );
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_does_not_start_over_for_a_verdict_about_the_file() {
    // A restart is for a partial the server threw away. Permission, quota
    // and the like are answers about the write itself: re-uploading the
    // whole file cannot change them, and doing it anyway would hammer the
    // NAS with gigabytes for nothing.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 1805}
        })))
        .mount(&server)
        .await;

    let local = scratch_file("denied.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    let err = client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .unwrap_err();
    assert!(matches!(err, SynoFsError::ApiError(1805)), "got {err:?}");
    assert_eq!(
        slice_posts(&server).await.len(),
        2,
        "no restart, no further slices"
    );
    std::fs::remove_file(&local).ok();
}

#[tokio::test]
async fn slice_upload_notices_the_file_landed_before_starting_over() {
    // The final slice's response can be the thing that gets lost. Starting
    // over would then re-send the whole file (and, with overwrite=false,
    // collide with what we already wrote), so a restart looks first.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(slice_ok())
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 401}
        })))
        .mount(&server)
        .await;
    mount_getinfo_size(&server, 2500).await;
    mount_md5(&server, SCRATCH_2500_MD5).await;

    let local = scratch_file("landed.bin", 2500);
    let client = client_for(&server).with_slice_size(1024);
    client
        .upload_from_path(&local, "/share", "big.bin", false)
        .await
        .expect("the file is on the NAS; that is what success means");

    assert_eq!(
        slice_posts(&server).await.len(),
        4,
        "3 slices plus the resend — the file is not sent a second time"
    );
    assert!(
        md5_calls(&server).await >= 2,
        "it landed via a resend, so its contents are checked"
    );
    std::fs::remove_file(&local).ok();
}

/// The `SYNO.FileStation.MD5` calls made with `method`, by task id.
async fn md5_method_calls(server: &MockServer, wanted: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.url
                .query_pairs()
                .any(|(k, v)| k == "method" && v == wanted)
        })
        .map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "taskid")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default()
        })
        .collect()
}

async fn mount_md5_start(server: &MockServer, taskid: &str) {
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "start"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"taskid": taskid}
        })))
        .mount(server)
        .await;
}

async fn mount_md5_stop(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "stop"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true
        })))
        .mount(server)
        .await;
}

/// Giving up on a hash is not the same as DSM giving up on it: the task goes
/// on reading the whole file unless it is told to stop. A caller verifying
/// thousands of files that each time out would leave thousands of whole-file
/// reads running on the appliance.
#[tokio::test]
async fn md5_stops_the_task_it_gives_up_waiting_for() {
    let server = MockServer::start().await;
    mount_md5_start(&server, "md5-slow").await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"finished": false}
        })))
        .mount(&server)
        .await;
    mount_md5_stop(&server).await;

    let err = client_for(&server)
        .md5_within("/share/huge.bin", Duration::from_millis(1500))
        .await
        .unwrap_err();

    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
    assert_eq!(md5_method_calls(&server, "stop").await, vec!["md5-slow"]);
}

/// Same when a status poll is refused: the task was started, so it is ours to
/// stop.
#[tokio::test]
async fn md5_stops_the_task_when_its_status_cannot_be_read() {
    let server = MockServer::start().await;
    mount_md5_start(&server, "md5-err").await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 401}
        })))
        .mount(&server)
        .await;
    mount_md5_stop(&server).await;

    let err = client_for(&server).md5("/share/f.bin").await.unwrap_err();

    assert!(matches!(err, SynoFsError::ApiError(401)), "got {err:?}");
    assert_eq!(md5_method_calls(&server, "stop").await, vec!["md5-err"]);
}

/// A hash is a whole-file read on the appliance — the same load as a
/// download — so it waits for a slot like one. Unthrottled, a loop verifying a
/// batch of files would start every read at once.
#[tokio::test]
async fn md5_waits_for_a_throttle_slot_like_a_download() {
    let server = MockServer::start().await;
    mount_md5_start(&server, "md5-t").await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "success": true,
                    "data": {"finished": true, "md5": "d41d8cd98f00b204e9800998ecf8427e"}
                }))
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&server)
        .await;

    let uri = server.uri();
    let (host, port) = uri.trim_start_matches("http://").rsplit_once(':').unwrap();
    let client = Arc::new(
        SynologyClient::new(host, port.parse().unwrap(), false).with_throttle(ThrottleConfig {
            max_concurrency: 1,
            min_interval: Duration::ZERO,
            ..ThrottleConfig::default()
        }),
    );

    let (a, b) = (Arc::clone(&client), Arc::clone(&client));
    let first = tokio::spawn(async move { a.md5("/share/a.bin").await });
    let second = tokio::spawn(async move { b.md5("/share/b.bin").await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        md5_method_calls(&server, "start").await.len(),
        1,
        "the second hash waited for the first to finish"
    );
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(md5_method_calls(&server, "start").await.len(), 2);
}

/// Regression: only an error stopped the task. A caller that gives up first
/// drops the call mid-poll — Python's `asyncio.wait_for(client.md5(p), 60)`
/// does exactly that on its timeout — and nothing told DSM, which went on
/// reading the whole file. Over a batch, that is one abandoned whole-file read
/// per timed-out file.
#[tokio::test]
async fn md5_stops_the_task_when_the_caller_gives_up_on_it() {
    let server = MockServer::start().await;
    mount_md5_start(&server, "md5-dropped").await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"finished": false}
        })))
        .mount(&server)
        .await;
    mount_md5_stop(&server).await;

    let client = client_for(&server);
    let gave_up =
        tokio::time::timeout(Duration::from_millis(1500), client.md5("/share/huge.bin")).await;
    assert!(gave_up.is_err(), "the caller's own timeout fired");

    // The stop is sent from the drop, so give it a moment to arrive.
    let deadline = Instant::now() + Duration::from_secs(5);
    while md5_method_calls(&server, "stop").await.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(md5_method_calls(&server, "stop").await, vec!["md5-dropped"]);
}

/// Regression: a session that expired during a long hash made the stop go
/// out on the dead session, where it failed quietly, and the caller's
/// relogin retry then started a second hash of the same file beside the
/// first. The old task has to be stopped on the new session, before the new
/// one starts — and the retry belongs here, where that order can be kept.
#[tokio::test]
async fn md5_stops_the_old_task_on_the_new_session_when_the_session_expires() {
    let server = MockServer::start().await;
    mount_probe_ok_for_verify(&server).await;
    Mock::given(method("POST"))
        .and(path("/webapi/auth.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"sid": "old_sid"}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/auth.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"sid": "new_sid"}
        })))
        .mount(&server)
        .await;
    for taskid in ["md5-a", "md5-b"] {
        Mock::given(method("GET"))
            .and(path("/webapi/entry.cgi"))
            .and(query_param("method", "start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "data": {"taskid": taskid}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .and(query_param("taskid", "md5-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 119}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .and(query_param("taskid", "md5-b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"finished": true, "md5": "d41d8cd98f00b204e9800998ecf8427e"}
        })))
        .mount(&server)
        .await;
    mount_md5_stop(&server).await;

    let client = client_auto_for(&server);
    client.login("alice", "secret", None).await.unwrap();
    let digest = client
        .with_relogin_retry(|| client.md5("/share/f.bin"))
        .await
        .unwrap();
    assert_eq!(digest, "d41d8cd98f00b204e9800998ecf8427e");

    let md5_requests: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| {
            r.url
                .query()
                .is_some_and(|q| q.contains("SYNO.FileStation.MD5"))
        })
        .collect();
    let order: Vec<String> = md5_requests
        .iter()
        .map(|r| {
            let q: std::collections::HashMap<_, _> = r.url.query_pairs().into_owned().collect();
            format!(
                "{} {}",
                q["method"],
                q.get("taskid").map(String::as_str).unwrap_or("-")
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![
            "start -",
            "status md5-a",
            "stop md5-a",
            "start -",
            "status md5-b"
        ],
        "the old task is stopped before the new one starts, and only one restart"
    );
    let stop = &md5_requests[2];
    let cookie = stop
        .headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        cookie.contains("new_sid"),
        "the stop went out on the new session, not the expired one: {cookie:?}"
    );
}

async fn mount_probe_ok_for_verify(server: &MockServer) {
    Mock::given(method("GET"))
        .and(query_param("method", "list_share"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"success": true, "data": {"total": 0, "shares": []}}),
        ))
        .mount(server)
        .await;
}

/// Logins that hand out `old_sid` first and `new_sid` after.
async fn mount_two_logins(server: &MockServer) {
    mount_probe_ok_for_verify(server).await;
    Mock::given(method("POST"))
        .and(path("/webapi/auth.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"sid": "old_sid"}
        })))
        .up_to_n_times(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webapi/auth.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"sid": "new_sid"}
        })))
        .mount(server)
        .await;
}

async fn mount_md5_status_for(
    server: &MockServer,
    taskid: &str,
    body: serde_json::Value,
    delay: Duration,
) {
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "status"))
        .and(query_param("taskid", taskid))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(body)
                .set_delay(delay),
        )
        .mount(server)
        .await;
}

async fn mount_md5_starts(server: &MockServer, taskids: &[&str]) {
    for taskid in taskids {
        Mock::given(method("GET"))
            .and(path("/webapi/entry.cgi"))
            .and(query_param("method", "start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "data": {"taskid": taskid}
            })))
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
}

/// Regression: the restart after a renewed session got a deadline of its own,
/// so a session that expired near the 15-minute ceiling could keep the caller
/// waiting most of another 15. The ceiling is for the whole call.
#[tokio::test]
async fn md5_keeps_one_deadline_across_a_renewed_session() {
    let server = MockServer::start().await;
    mount_two_logins(&server).await;
    mount_md5_starts(&server, &["md5-a", "md5-b"]).await;
    mount_md5_status_for(
        &server,
        "md5-a",
        serde_json::json!({"success": false, "error": {"code": 119}}),
        Duration::from_millis(1000),
    )
    .await;
    mount_md5_status_for(
        &server,
        "md5-b",
        serde_json::json!({"success": true, "data": {"finished": false}}),
        Duration::ZERO,
    )
    .await;
    mount_md5_stop(&server).await;
    let client = client_auto_for(&server);
    client.login("alice", "secret", None).await.unwrap();

    let started = Instant::now();
    let err = client
        .md5_within("/share/f.bin", Duration::from_millis(1500))
        .await
        .unwrap_err();

    assert!(matches!(err, SynoFsError::Io(_)), "got {err:?}");
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "gave up {:?} after starting, against a 1.5 s ceiling",
        started.elapsed()
    );
}

/// The stop's answer was never read: a stop refused because the session had
/// just expired counted as sent, and the task went on reading the file.
#[tokio::test]
async fn md5_sends_the_stop_again_when_the_session_expired_under_it() {
    let server = MockServer::start().await;
    mount_two_logins(&server).await;
    mount_md5_starts(&server, &["md5-x"]).await;
    mount_md5_status_for(
        &server,
        "md5-x",
        serde_json::json!({"success": true, "data": {"finished": false}}),
        Duration::ZERO,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "stop"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false, "error": {"code": 119}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_md5_stop(&server).await;
    let client = client_auto_for(&server);
    client.login("alice", "secret", None).await.unwrap();

    client
        .md5_within("/share/f.bin", Duration::from_millis(1200))
        .await
        .unwrap_err();

    let stops: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.url
                .query_pairs()
                .any(|(k, v)| k == "method" && v == "stop")
        })
        .map(|r| {
            r.headers
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(stops.len(), 2, "refused once, sent again: {stops:?}");
    assert!(stops[1].contains("new_sid"), "{stops:?}");
}

/// A session that expires under both attempts must not come back as 119: a
/// caller's relogin wrapper reads that as "log in and call again", which is a
/// third full hash, with a fresh ceiling.
#[tokio::test]
async fn md5_does_not_invite_another_retry_after_two_expired_sessions() {
    let server = MockServer::start().await;
    mount_two_logins(&server).await;
    mount_md5_starts(&server, &["md5-a", "md5-b"]).await;
    for taskid in ["md5-a", "md5-b"] {
        mount_md5_status_for(
            &server,
            taskid,
            serde_json::json!({"success": false, "error": {"code": 119}}),
            Duration::ZERO,
        )
        .await;
    }
    mount_md5_stop(&server).await;
    let client = client_auto_for(&server);
    client.login("alice", "secret", None).await.unwrap();

    let err = client
        .with_relogin_retry(|| client.md5("/share/f.bin"))
        .await
        .unwrap_err();

    assert!(!matches!(err, SynoFsError::ApiError(119)), "got {err:?}");
    assert_eq!(
        md5_method_calls(&server, "start").await.len(),
        2,
        "no third hash"
    );
}
