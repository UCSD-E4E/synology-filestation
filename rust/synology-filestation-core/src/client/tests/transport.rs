//! Which leg a call takes: backend selection and the fallback to HTTP.

use super::*;

// ── read/write backend selection + fallback ──────────────────────────────
//
// Dependency-inversion: a backend (SMB today) is injected and preferred over
// HTTP, with a per-backend circuit breaker. These pin the selection contract
// with a configurable fake backend so no real SMB server is needed.

#[tokio::test]
async fn download_prefers_healthy_backend_and_skips_http() {
    let backend = FakeBackend::new(Behave::Ok(b"from-smb"));
    let client = offline_client().with_read_transport(backend.clone());
    // No HTTP server exists; if this returns, the backend served it.
    let bytes = client.download("/share/f", 0, 0).await.unwrap();
    assert_eq!(bytes.as_ref(), b"from-smb");
    assert_eq!(backend.call_count(), 1);
}

#[tokio::test]
async fn download_propagates_definitive_backend_error_without_http() {
    let backend = FakeBackend::new(Behave::NotFound);
    let client = offline_client().with_read_transport(backend.clone());
    let err = client.download("/share/missing", 0, 0).await.unwrap_err();
    assert!(matches!(err, SynoFsError::NotFound));
    assert_eq!(backend.call_count(), 1, "definitive error must not retry");
}

#[tokio::test]
async fn download_falls_back_to_http_on_transient_backend_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"from-http".to_vec()))
        .mount(&server)
        .await;

    let backend = FakeBackend::new(Behave::Transient);
    let client = client_for(&server).with_read_transport(backend.clone());
    let bytes = client.download("/share/f", 0, 0).await.unwrap();
    assert_eq!(bytes.as_ref(), b"from-http", "fell back to HTTP");
    assert_eq!(backend.call_count(), 1);
}

#[tokio::test]
async fn breaker_opens_and_stops_probing_a_failing_backend() {
    // Default breaker threshold is 2. A persistently-transient backend
    // should be tried on the first two downloads, then skipped entirely
    // while HTTP keeps serving.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"http".to_vec()))
        .mount(&server)
        .await;

    let backend = FakeBackend::new(Behave::Transient);
    let client = client_for(&server).with_read_transport(backend.clone());

    for _ in 0..5 {
        assert_eq!(
            client.download("/share/f", 0, 0).await.unwrap().as_ref(),
            b"http"
        );
    }
    // Tried twice (to reach the threshold), then the open breaker skips it.
    assert_eq!(backend.call_count(), 2, "breaker should stop probing");
}

#[tokio::test]
async fn upload_prefers_backend_then_falls_back_on_transient() {
    // Healthy write backend serves a replacing upload with no HTTP server.
    let ok_backend = FakeBackend::new(Behave::Ok(b""));
    let client = offline_client().with_write_transport(ok_backend.clone());
    client
        .upload("/share", "f.bin", b"data".to_vec(), true)
        .await
        .unwrap();
    assert_eq!(ok_backend.call_count(), 1);

    // Transient write failure falls back to the HTTP upload.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"blks": null}
        })))
        .mount(&server)
        .await;
    let bad_backend = FakeBackend::new(Behave::Transient);
    let client = client_for(&server).with_write_transport(bad_backend.clone());
    client
        .upload("/share", "f.bin", b"data".to_vec(), true)
        .await
        .unwrap();
    assert_eq!(
        bad_backend.call_count(),
        1,
        "attempted before HTTP fallback"
    );
}

#[tokio::test]
async fn upload_overwrite_false_bypasses_write_backend() {
    // A backend's write always replaces, so it can't honor overwrite=false's
    // "fail if the file exists" contract — that write must go to HTTP, not
    // silently clobber an existing file over SMB.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"blks": null}
        })))
        .mount(&server)
        .await;
    let backend = FakeBackend::new(Behave::Ok(b""));
    let client = client_for(&server).with_write_transport(backend.clone());
    client
        .upload("/share", "f.bin", b"data".to_vec(), false)
        .await
        .unwrap();
    assert_eq!(
        backend.call_count(),
        0,
        "overwrite=false must skip the replacing write backend"
    );
}

// ── a backend that declines ──────────────────────────────────────────────
//
// `NotSupported` means "not this one, ask the next", and the metadata and
// open-write paths have always read it that way. The byte paths did not: they
// handed it to the caller as a definitive answer. That was harmless while no
// backend ever declined a read, but an SMB transport attached before it has a
// session — so that it can be given one later, when SMB becomes reachable —
// declines everything until then, and every read on such a mount would fail
// rather than go over HTTP.

/// An HTTP server that serves one download body.
async fn http_serving(body: &'static [u8]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/webapi/entry.cgi"))
        .and(query_param("method", "download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn download_passes_over_a_backend_that_declines() {
    let server = http_serving(b"from-http").await;
    let backend = FakeBackend::new(Behave::Declines);
    let client = client_for(&server).with_read_transport(backend.clone());

    for _ in 0..3 {
        assert_eq!(
            client.download("/share/f", 0, 0).await.unwrap().as_ref(),
            b"from-http"
        );
    }
    assert_eq!(
        backend.call_count(),
        3,
        "declining is an answer, not a failure: the breaker stays closed, so \
         the backend is asked again and can serve as soon as it is able to"
    );
}

#[tokio::test]
async fn download_to_path_passes_over_a_backend_that_declines() {
    let server = http_serving(b"from-http").await;
    let backend = FakeBackend::new(Behave::Declines);
    let client = client_for(&server).with_stream_read_transport(backend.clone());
    let local = unique_tmp_path("declined-download");

    client.download_to_path("/share/f", &local).await.unwrap();

    assert_eq!(std::fs::read(&local).unwrap(), b"from-http");
    assert_eq!(backend.call_count(), 1);
    let _ = std::fs::remove_file(&local);
}

#[tokio::test]
async fn upload_passes_over_a_backend_that_declines() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webapi/entry.cgi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "data": {"blks": null}
        })))
        .mount(&server)
        .await;
    let backend = FakeBackend::new(Behave::Declines);
    let client = client_for(&server).with_write_transport(backend.clone());

    for _ in 0..3 {
        client
            .upload("/share", "f.bin", b"data".to_vec(), true)
            .await
            .expect("HTTP carries it");
    }
    assert_eq!(backend.call_count(), 3, "asked every time, never tripped");
}
