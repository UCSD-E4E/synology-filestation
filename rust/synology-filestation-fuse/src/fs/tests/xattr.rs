//! `user.synology.md5`: asking the NAS for a file's MD5 through the mount.
//!
//! `md5sum` on the mount reads every byte back over the network; there is no
//! filesystem operation for "give me this file's checksum" that it could ask
//! instead. An extended attribute is the nearest thing a stock tool can ask
//! for by name — `getfattr -n user.synology.md5 <file>` — and the NAS answers
//! it by hashing the file on its own disk (`SYNO.FileStation.MD5`).

use super::*;
use fuser::Errno;
use std::ffi::OsStr;

const DIGEST: &str = "9e107d9d372bb6826bd81d3542a419d6";
const TASKID: &str = "FileStation_6A1B2C3D4E5F";

/// DSM's MD5 task: `start` hands back a task id, `status` answers under the
/// JSON-quoted id only — as the real one does.
fn mount_md5(f: &Fixture, delay: Duration) {
    f.rt.block_on(async {
        Mock::given(http_method("GET"))
            .and(query_param("api", "SYNO.FileStation.MD5"))
            .and(query_param("method", "start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "data": {"taskid": TASKID}
            })))
            .mount(&f.server)
            .await;
        Mock::given(http_method("GET"))
            .and(query_param("api", "SYNO.FileStation.MD5"))
            .and(query_param("method", "status"))
            .and(query_param("taskid", format!("\"{TASKID}\"").as_str()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "success": true, "data": {"finished": true, "md5": DIGEST}
                    }))
                    .set_delay(delay),
            )
            .mount(&f.server)
            .await;
    });
}

fn md5_requests(f: &Fixture) -> usize {
    f.rt.block_on(f.server.received_requests())
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r.url
                .query()
                .is_some_and(|q| q.contains("SYNO.FileStation.MD5"))
        })
        .count()
}

fn seed(f: &Fixture, path: &str, isdir: bool) -> u64 {
    let ino = f.fs.cache.get_or_alloc_ino(path);
    f.fs.cache.insert(
        ino,
        SynoFileInfo {
            name: path.rsplit('/').next().unwrap().to_string(),
            path: path.to_string(),
            isdir,
            additional: Some(SynoAdditional {
                size: Some(15 << 20),
                owner: None,
                time: None,
                perm: None,
            }),
            code: None,
        },
    );
    ino
}

/// Ask, and wait for the answer from whichever thread gives it.
fn ask(f: &Fixture, ino: u64, name: &str, size: u32) -> XattrAnswer {
    let (tx, rx) = std::sync::mpsc::channel();
    f.fs.start_getxattr(ino, OsStr::new(name), size, move |a| {
        let _ = tx.send(a);
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("every getxattr is answered")
}

#[test]
fn the_nas_hashes_the_file_and_the_digest_comes_back() {
    let f = fixture();
    mount_md5(&f, Duration::ZERO);
    let ino = seed(&f, "/fishsense_data/REEF/img.ORF", false);

    assert_eq!(
        ask(&f, ino, MD5_XATTR, 64),
        XattrAnswer::Value(DIGEST.as_bytes().to_vec())
    );
    assert_eq!(md5_requests(&f), 2, "start, then one status");
}

/// The kernel asks for an attribute's length before its value. A digest is
/// always 32 hex characters, so the length is answered without a hash — or
/// `getfattr` would make the NAS read the file twice.
#[test]
fn the_length_is_answered_without_hashing() {
    let f = fixture();
    mount_md5(&f, Duration::ZERO);
    let ino = seed(&f, "/share/f.bin", false);

    assert_eq!(ask(&f, ino, MD5_XATTR, 0), XattrAnswer::Size(32));
    assert_eq!(md5_requests(&f), 0);
}

#[test]
fn a_buffer_too_small_for_the_digest_is_refused_before_hashing() {
    let f = fixture();
    mount_md5(&f, Duration::ZERO);
    let ino = seed(&f, "/share/f.bin", false);

    assert_eq!(
        ask(&f, ino, MD5_XATTR, 16),
        XattrAnswer::Error(Errno::ERANGE)
    );
    assert_eq!(md5_requests(&f), 0);
}

/// `ls` asks every file for `security.selinux` and the POSIX ACL attributes.
/// Those must be answered at once, and never reach the NAS.
#[test]
fn any_other_attribute_does_not_exist() {
    let f = fixture();
    let ino = seed(&f, "/share/f.bin", false);

    for name in ["security.selinux", "system.posix_acl_access", "user.other"] {
        assert_eq!(
            ask(&f, ino, name, 64),
            XattrAnswer::Error(Errno::ENODATA),
            "{name}"
        );
    }
    let asked =
        f.rt.block_on(f.server.received_requests())
            .unwrap_or_default();
    assert!(asked.is_empty(), "{} requests reached the NAS", asked.len());
}

#[test]
fn a_directory_has_no_digest() {
    let f = fixture();
    let dir = seed(&f, "/share/photos", true);

    assert_eq!(
        ask(&f, dir, MD5_XATTR, 64),
        XattrAnswer::Error(Errno::ENODATA)
    );
    assert_eq!(
        ask(&f, ROOT_INO, MD5_XATTR, 64),
        XattrAnswer::Error(Errno::ENODATA)
    );
    assert_eq!(md5_requests(&f), 0);
}

/// The point of asking is to check what landed. A file with writes the NAS
/// has not got yet would be hashed half-uploaded and answer with a digest
/// that matches nothing.
#[test]
fn a_file_with_writes_still_to_land_is_not_hashed() {
    let f = fixture();
    mount_md5(&f, Duration::ZERO);
    seed_dirty_buffer(&f, "/share/new.bin", b"not uploaded yet");
    let ino = f.fs.cache.get_or_alloc_ino("/share/new.bin");
    seed(&f, "/share/new.bin", false);

    assert_eq!(
        ask(&f, ino, MD5_XATTR, 64),
        XattrAnswer::Error(Errno::EBUSY)
    );
    assert_eq!(md5_requests(&f), 0);
}

/// Writes pending on another file are not this file's business.
#[test]
fn writes_to_another_file_do_not_hold_up_this_one() {
    let f = fixture();
    mount_md5(&f, Duration::ZERO);
    seed_dirty_buffer(&f, "/share/other.bin", b"not uploaded yet");
    let ino = seed(&f, "/share/f.bin", false);

    assert_eq!(
        ask(&f, ino, MD5_XATTR, 64),
        XattrAnswer::Value(DIGEST.as_bytes().to_vec())
    );
}

/// The NAS reads the whole file to answer; minutes for a large one. The FUSE
/// event-loop thread that asked must be free to serve the rest of the mount
/// meanwhile, as it is during an upload.
#[test]
fn hashing_does_not_block_the_calling_thread() {
    let f = fixture();
    mount_md5(&f, Duration::from_millis(600));
    let ino = seed(&f, "/share/f.bin", false);

    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    f.fs.start_getxattr(ino, OsStr::new(MD5_XATTR), 64, move |a| {
        let _ = tx.send(a);
    });
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the calling thread waited {:?}",
        started.elapsed()
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        XattrAnswer::Value(DIGEST.as_bytes().to_vec())
    );
}

#[test]
fn a_file_the_nas_does_not_have_is_enoent() {
    let f = fixture();
    f.rt.block_on(
        Mock::given(http_method("GET"))
            .and(query_param("api", "SYNO.FileStation.MD5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false, "error": {"code": 414}
            })))
            .mount(&f.server),
    );
    let ino = seed(&f, "/share/gone.bin", false);

    assert_eq!(
        ask(&f, ino, MD5_XATTR, 64),
        XattrAnswer::Error(Errno::ENOENT)
    );
}

#[test]
fn an_unknown_inode_is_enoent() {
    let f = fixture();
    assert_eq!(
        ask(&f, 9999, MD5_XATTR, 64),
        XattrAnswer::Error(Errno::ENOENT)
    );
}

/// Not listed. `cp -a`, `rsync -X` and file managers copy every attribute a
/// file lists by reading it, so a listed digest would have copying a folder
/// off the mount hash every file in it on the NAS.
#[test]
fn the_digest_is_not_listed() {
    let f = fixture();
    let ino = seed(&f, "/share/f.bin", false);
    assert!(f.fs.listed_xattrs(ino).is_empty());
}
