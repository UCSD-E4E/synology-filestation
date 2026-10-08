//! `user.synology.md5` through a real kernel mount, asked the way `getfattr`
//! asks: `getxattr(2)` with no buffer for the length, then again for the
//! value.
//!
//! The unit tests cover the policy; this covers the half they cannot — that
//! the kernel actually routes the call here, that the two-step size protocol
//! costs one hash and not two, and that the attribute stays out of
//! `listxattr`.
//!
//! Ignored by default: it needs a real FUSE mount, which CI containers do not
//! have. Run it deliberately with `--ignored`, on a machine with `/dev/fuse`.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use synology_filestation_core::SynologyClient;
use synology_filestation_fuse::{spawn_mount, MountOptions};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DIGEST: &str = "9e107d9d372bb6826bd81d3542a419d6";
const TASKID: &str = "FileStation_6A1B2C3D4E5F";

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true, "data": data}))
}

/// `getxattr(2)`: `Ok(len)` with a zero-length buffer, else the bytes.
fn getxattr(file: &Path, name: &str, size: usize) -> Result<Vec<u8>, std::io::Error> {
    let file = CString::new(file.as_os_str().as_bytes()).unwrap();
    let name = CString::new(name).unwrap();
    let mut buf = vec![0u8; size];
    let n = unsafe {
        libc::getxattr(
            file.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // With no buffer, `n` is the length; the test only wants that many bytes.
    buf.resize(n as usize, 0);
    Ok(buf)
}

#[test]
#[ignore = "needs a real FUSE mount; run with --ignored"]
fn getfattr_gets_the_digest_from_the_nas() {
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
            .respond_with(ok(serde_json::json!({
                "total": 1, "offset": 0,
                "files": [{"name": "img.ORF", "path": "/share/img.ORF", "isdir": false,
                           "additional": {"size": 15728640}}]
            })))
            .mount(&server)
            .await;
        // A cold lookup — `getfattr` on a path nothing has listed yet — asks
        // for the one file rather than its directory.
        Mock::given(query_param("method", "getinfo"))
            .respond_with(ok(serde_json::json!({
                "files": [{"name": "img.ORF", "path": "/share/img.ORF", "isdir": false,
                           "additional": {"size": 15728640}}]
            })))
            .mount(&server)
            .await;
        Mock::given(query_param("api", "SYNO.FileStation.MD5"))
            .and(query_param("method", "start"))
            .respond_with(ok(serde_json::json!({"taskid": TASKID})))
            .mount(&server)
            .await;
        Mock::given(query_param("api", "SYNO.FileStation.MD5"))
            .and(query_param("method", "status"))
            .and(query_param("taskid", format!("\"{TASKID}\"").as_str()))
            .respond_with(ok(serde_json::json!({"finished": true, "md5": DIGEST})))
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
    let file = mountpoint.join("share/img.ORF");

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let len = getxattr(&file, "user.synology.md5", 0).expect("the length");
        assert_eq!(len.len(), 32);
        let value = getxattr(&file, "user.synology.md5", 64).expect("the value");
        assert_eq!(std::str::from_utf8(&value).unwrap(), DIGEST);

        let selinux = getxattr(&file, "security.selinux", 64).unwrap_err();
        assert_eq!(selinux.raw_os_error(), Some(libc::ENODATA));

        let listed = xattr_names(&file);
        assert!(listed.is_empty(), "listed: {listed:?}");

        let hashes = rt
            .block_on(server.received_requests())
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.query().is_some_and(|q| q.contains("method=start")))
            .count();
        assert_eq!(hashes, 1, "the length is free; only the value is hashed");
    }));

    handle.stop();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn xattr_names(file: &Path) -> Vec<u8> {
    let file = CString::new(file.as_os_str().as_bytes()).unwrap();
    let mut buf = vec![0u8; 1024];
    let n = unsafe { libc::listxattr(file.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    assert!(n >= 0, "listxattr: {}", std::io::Error::last_os_error());
    buf.truncate(n as usize);
    buf
}
