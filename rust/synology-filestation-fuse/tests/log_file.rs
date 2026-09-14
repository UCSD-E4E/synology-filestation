//! The log file has to hold the end of the story, not just the middle.

use std::process::Command;

/// The failure that ended the run is the most valuable line in a post-mortem,
/// and it was the one line the file did not have: `main` returned the error to
/// the runtime, which printed it to stderr — the sink that does not survive.
/// Somebody reading the file afterwards saw the mount getting ready and then
/// nothing, with no way to tell a crash from a clean exit.
#[test]
fn the_error_that_ended_the_run_is_in_the_log_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("mount.log");

    // Port 1 on loopback: refused immediately, with no DNS lookup and no
    // network to depend on.
    let out = Command::new(env!("CARGO_BIN_EXE_synology-filestation-fuse"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "-u",
            "tester",
            "--log-file",
            log.to_str().unwrap(),
            dir.path().join("mnt").to_str().unwrap(),
        ])
        .env("SYNO_PASSWORD", "irrelevant")
        .output()
        .expect("the binary runs");

    assert!(!out.status.success(), "connecting to port 1 cannot succeed");

    let written = std::fs::read_to_string(&log).expect("the log file was created");
    assert!(
        written.contains("ERROR"),
        "the failure has to be in the file, which held:\n{written}"
    );
    assert!(
        written.contains("Connecting to Synology NAS"),
        "and so does what led up to it, which held:\n{written}"
    );

    // The terminal is what it always was: the error once, on stderr, printed
    // by `Termination`. Sending it through `tracing` as well would put a
    // second copy on stdout, where the ordinary records go — the same failure
    // twice, in two places, for a run that failed once.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("ERROR"),
        "the fatal record must not reach the console, which held:\n{stdout}"
    );
    assert_eq!(
        stderr.matches("Error:").count(),
        1,
        "and stderr carries it exactly once, which held:\n{stderr}"
    );
}
