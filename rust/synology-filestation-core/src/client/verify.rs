//! Proving an upload landed: MD5 over the wire, and what to do when it did not.

use super::*;

/// How long to let a just-written file settle before a size disagreement is
/// treated as corruption rather than a listing that has not caught up.
pub(super) const VERIFY_SETTLE_DELAY: Duration = Duration::from_millis(500);

/// How often the `SYNO.FileStation.MD5` task is polled — the same 1 s File
/// Station's own properties dialog uses.
pub(super) const MD5_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Ceiling on waiting for a hash. DSM reads the whole file to produce one, so
/// this is generous; it exists so a task that never finishes cannot park a FUSE
/// `flush` indefinitely.
pub(super) const MD5_MAX_WAIT: Duration = Duration::from_secs(15 * 60);

/// MD5 of a local file, streamed in 1 MiB reads so a multi-GB upload is not
/// re-buffered to hash it. Runs on the blocking pool: hashing 6 GB is seconds
/// of solid CPU, which is not something to do on a runtime worker.
pub(super) async fn md5_of_file(path: &Path) -> Result<String, SynoFsError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use md5::{Digest, Md5};
        use std::io::Read;

        let mut f = std::fs::File::open(&path)
            .map_err(|e| SynoFsError::Io(format!("md5: open {} failed: {e}", path.display())))?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = f.read(&mut buf).map_err(|e| {
                SynoFsError::Io(format!("md5: read {} failed: {e}", path.display()))
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    })
    .await
    .map_err(|e| SynoFsError::Io(format!("md5: hashing task failed: {e}")))?
}

impl SynologyClient {
    /// Check that what landed on the NAS is what we sent, and refuse to report
    /// success otherwise.
    ///
    /// Two levels, because they cost very different amounts:
    ///
    /// * **Size** — one metadata call, run after every sliced upload. A doubled
    ///   slice the server kept shows up here immediately.
    /// * **MD5** — only when a resend could have doubled a slice, because it
    ///   makes DSM read the whole file back (minutes, and disk the appliance
    ///   would rather spend elsewhere). It is the only check that catches a
    ///   doubled slice if DSM trims the partial back to `X-FILE-SIZE`.
    ///
    /// What happens when a check cannot run depends on who could not answer.
    /// A missing size, or DSM replying that it cannot hash (`SYNO.FileStation.MD5`
    /// absent or refused), is a verdict we can do nothing about: logged, and the
    /// upload accepted, since the write succeeded and nothing contradicts it.
    /// Failing to *reach* the hash after a risky resend is not — that returns an
    /// error rather than claiming a verified write, while leaving the file in
    /// place for the caller's retry to overwrite.
    ///
    /// Only positive evidence of a mismatch deletes: a file we know is wrong is
    /// worse than no file, but a file we merely cannot vouch for is not.
    pub(super) async fn verify_upload(
        &self,
        full_path: &str,
        local: &Path,
        total: u64,
        hash_it: bool,
    ) -> Result<(), SynoFsError> {
        if let Some(size) = self.landed_size(full_path).await {
            if size != total {
                // The listing can lag a write DSM has just accepted — the same
                // lag `clear_for_overwrite` polls through. Confirm before
                // acting: a false positive here deletes a good upload.
                tokio::time::sleep(VERIFY_SETTLE_DELAY).await;
                if let Some(size) = self.landed_size(full_path).await {
                    if size != total {
                        return self
                            .reject_upload(
                                full_path,
                                format!("landed as {size} bytes, expected {total}"),
                            )
                            .await;
                    }
                }
            }
        }

        if !hash_it {
            return Ok(());
        }

        let remote = match self.md5(full_path).await {
            Ok(m) => m,
            // DSM answering (no such API, no permission) is a verdict: it
            // cannot hash for us, and no amount of retrying changes that. The
            // write succeeded and nothing contradicts it, so accept it.
            Err(e @ SynoFsError::ApiError(_)) => {
                warn!("upload verify: {full_path} cannot be hashed by the NAS ({e}), accepting");
                return Ok(());
            }
            // Not reaching the check at all is different. A resend may have
            // doubled a slice and we have no way to tell, so this is not a
            // verified write — but there is no evidence against the file
            // either, so it stays: the caller's retry re-uploads over it, and
            // deleting a probably-good upload is the worse mistake.
            Err(e) => {
                let msg =
                    format!("upload of {full_path} could not be verified after a resend: {e}");
                error!("{msg}");
                return Err(SynoFsError::Io(msg));
            }
        };
        let local_md5 = md5_of_file(local).await?;
        if !remote.eq_ignore_ascii_case(&local_md5) {
            return self
                .reject_upload(full_path, format!("md5 {remote} != {local_md5}"))
                .await;
        }
        debug!("upload verify: {full_path} matches after a resend");
        Ok(())
    }

    /// The size the NAS reports for `full_path`, or `None` when it cannot say —
    /// an unreadable listing is not evidence against an upload, so the caller
    /// treats it as "unverified" rather than "wrong".
    pub(super) async fn landed_size(&self, full_path: &str) -> Option<u64> {
        match self.get_info(full_path).await {
            Ok(info) => match info.additional.and_then(|a| a.size) {
                Some(size) => Some(size),
                None => {
                    warn!("upload verify: no size reported for {full_path}, accepting as-is");
                    None
                }
            },
            Err(e) => {
                warn!("upload verify: getinfo {full_path} failed ({e}), accepting as-is");
                None
            }
        }
    }

    /// Remove a file whose contents we cannot vouch for, then report why.
    pub(super) async fn reject_upload(
        &self,
        full_path: &str,
        why: String,
    ) -> Result<(), SynoFsError> {
        let msg = format!("upload verification failed for {full_path}: {why}");
        error!("{msg}");
        if let Err(e) = self.delete(full_path).await {
            warn!("could not remove unverified upload {full_path}: {e}");
        }
        Err(SynoFsError::Io(msg))
    }

    /// Have DSM hash a file it holds, via `SYNO.FileStation.MD5`.
    ///
    /// Two steps, as File Station's own properties dialog does it: `start`
    /// hands back a task id, `status` is polled (it polls at 1 s) until
    /// `finished`. Bounded by [`MD5_MAX_WAIT`] so a task that never finishes
    /// cannot park the caller — for the FUSE backend, that caller is a `flush`.
    ///
    /// DSM reads the whole file to answer, which is the same load on the
    /// appliance as a download, so a throttled client holds a transfer slot
    /// for the length of the task. A task given up on — timed out, or its
    /// status refused — is stopped, or it would go on reading regardless.
    pub async fn md5(&self, path: &str) -> Result<String, SynoFsError> {
        self.md5_within(path, MD5_MAX_WAIT).await
    }

    /// [`Self::md5`], giving up after `max_wait` rather than [`MD5_MAX_WAIT`].
    pub(super) async fn md5_within(
        &self,
        path: &str,
        max_wait: Duration,
    ) -> Result<String, SynoFsError> {
        let _slot = self.acquire_transfer_slot().await;
        let url = format!("{}/entry.cgi", self.base_url);
        let text = self
            .get_text_retried(
                &url,
                &[
                    ("api", "SYNO.FileStation.MD5"),
                    ("version", "2"),
                    ("method", "start"),
                    ("file_path", path),
                ],
            )
            .await?;
        let parsed: SynoResponse<Md5StartData> = serde_json::from_str(&text)
            .map_err(|e| SynoFsError::Io(format!("md5 start parse error: {e}")))?;
        if !parsed.success {
            return Err(SynoFsError::ApiError(
                parsed.error.map(|e| e.code).unwrap_or(0),
            ));
        }
        let taskid = parsed
            .data
            .map(|d| d.taskid)
            .ok_or_else(|| SynoFsError::Io("md5 start returned no taskid".into()))?;

        let result = self.md5_wait(&url, path, &taskid, max_wait).await;
        if result.is_err() {
            self.md5_stop(&url, &taskid).await;
        }
        result
    }

    /// Poll a started MD5 task until it finishes or `max_wait` runs out.
    async fn md5_wait(
        &self,
        url: &str,
        path: &str,
        taskid: &str,
        max_wait: Duration,
    ) -> Result<String, SynoFsError> {
        let deadline = Instant::now() + max_wait;
        loop {
            let text = self
                .get_text_retried(
                    url,
                    &[
                        ("api", "SYNO.FileStation.MD5"),
                        ("version", "2"),
                        ("method", "status"),
                        ("taskid", taskid),
                    ],
                )
                .await?;
            let parsed: SynoResponse<Md5StatusData> = serde_json::from_str(&text)
                .map_err(|e| SynoFsError::Io(format!("md5 status parse error: {e}")))?;
            if !parsed.success {
                return Err(SynoFsError::ApiError(
                    parsed.error.map(|e| e.code).unwrap_or(0),
                ));
            }
            if let Some(data) = parsed.data {
                if data.finished {
                    return data.md5.filter(|m| !m.is_empty()).ok_or_else(|| {
                        SynoFsError::Io("md5 task finished without a digest".into())
                    });
                }
            }
            if Instant::now() >= deadline {
                return Err(SynoFsError::Io(format!(
                    "md5 of {path} did not finish within {}s",
                    max_wait.as_secs()
                )));
            }
            tokio::time::sleep(MD5_POLL_INTERVAL).await;
        }
    }

    /// Tell DSM to abandon an MD5 task. Best effort: the caller already has
    /// the error that matters, and a stop that fails leaves nothing worse than
    /// the task running to the end, which is what happened before.
    async fn md5_stop(&self, url: &str, taskid: &str) {
        let stopped = self
            .get_text_retried(
                url,
                &[
                    ("api", "SYNO.FileStation.MD5"),
                    ("version", "2"),
                    ("method", "stop"),
                    ("taskid", taskid),
                ],
            )
            .await;
        if let Err(e) = stopped {
            tracing::debug!(taskid, error = %e, "md5: stopping an abandoned task failed");
        }
    }
}
