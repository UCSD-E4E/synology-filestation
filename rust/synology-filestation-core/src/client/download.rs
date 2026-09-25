//! Reading file content: ranged reads, and the atomic whole-file download.

use super::throttle::{classify_download_body, http_status_is_transient, TransferOutcome};
use super::*;

impl SynologyClient {
    /// Download a remote file atomically to `local_path`, preferring a
    /// [`StreamReadTransport`] backend (SMB) so a large file streams straight to
    /// disk with no in-memory copy.
    ///
    /// The destination is never observed partial — either complete or absent.
    /// On a transport failure the backend's breaker trips and we fall back to
    /// the buffering HTTP download; a definitive error (not-found / permission)
    /// propagates. With no stream backend injected this is exactly the HTTP
    /// download — today's behavior.
    #[allow(dead_code)] // unused by the FUSE binary today; consumed by python bindings.
    pub async fn download_to_path(
        &self,
        remote_path: &str,
        local_path: &Path,
    ) -> Result<(), SynoFsError> {
        for entry in &self.stream_read_transports {
            let allowed = entry.breaker.lock().unwrap().allows(Instant::now());
            if !allowed {
                continue;
            }
            match entry.transport.read_to_path(remote_path, local_path).await {
                Ok(()) => {
                    entry.breaker.lock().unwrap().on_success();
                    return Ok(());
                }
                // Not this backend, not now; see `download`.
                Err(e) if e.category() == ErrorCategory::NotSupported => {
                    debug!("stream read backend declined {remote_path}, using HTTP: {e}");
                    entry.answered();
                    continue;
                }
                Err(e) if e.category() == ErrorCategory::Transport => {
                    warn!("stream read backend failed for {remote_path} (transient), falling back: {e}");
                    entry.breaker.lock().unwrap().on_failure(Instant::now());
                    continue;
                }
                Err(e) => {
                    entry.breaker.lock().unwrap().on_success();
                    return Err(e);
                }
            }
        }
        self.http_download_to_path(remote_path, local_path).await
    }

    /// HTTP download-to-file: buffer the bytes, then write atomically
    /// (`<local_path>.part`, fsync, rename). Used when no stream backend is
    /// injected, and as the fallback when one trips its breaker.
    ///
    /// Guards against the DSM footgun of `200 OK` responses whose body is
    /// `{"success":false,"error":{"code":119}}` — the synology-api PyPI package
    /// opens its destination in `'wb'` first and silently leaves a 0-byte file.
    pub(super) async fn http_download_to_path(
        &self,
        remote_path: &str,
        local_path: &Path,
    ) -> Result<(), SynoFsError> {
        let bytes = self
            .with_relogin_retry(|| self.download(remote_path, 0, 0))
            .await?;

        let tmp = {
            let mut t = local_path.as_os_str().to_os_string();
            t.push(".part");
            std::path::PathBuf::from(t)
        };

        let write_result: std::io::Result<()> = (|| {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, local_path)
        })();

        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp);
            return Err(SynoFsError::Io(format!(
                "download_to_path: write to {} failed: {e}",
                local_path.display()
            )));
        }
        Ok(())
    }

    /// Read file bytes, preferring any injected [`ReadTransport`] backend (SMB,
    /// …) over the HTTP Download API.
    ///
    /// Backends are tried in registration order; on a transport failure
    /// (`category() == Transport`) the backend's circuit breaker trips and we
    /// fall back to the next backend, ending at the HTTP path. A definitive
    /// error (not-found / permission) from a backend propagates unchanged — the
    /// backend answered, so re-asking HTTP the same question is pointless.
    ///
    /// With no backends injected this is exactly the HTTP download — today's
    /// behavior, unchanged.
    pub async fn download(
        &self,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Bytes, SynoFsError> {
        for entry in &self.read_transports {
            // Lock only to read/update breaker state — never held across await.
            let allowed = entry.breaker.lock().unwrap().allows(Instant::now());
            if !allowed {
                continue;
            }
            match entry.transport.read(path, offset, length).await {
                Ok(bytes) => {
                    entry.breaker.lock().unwrap().on_success();
                    return Ok(bytes);
                }
                // Not this backend, not now — an SMB transport attached before
                // it has a session says so until it gets one. An answer rather
                // than a failure, so the breaker stays shut and the backend is
                // asked again the moment it can serve.
                Err(e) if e.category() == ErrorCategory::NotSupported => {
                    debug!("read backend declined {path}, using HTTP: {e}");
                    entry.answered();
                    continue;
                }
                Err(e) if e.category() == ErrorCategory::Transport => {
                    warn!("read backend failed for {path} (transient), falling back: {e}");
                    entry.breaker.lock().unwrap().on_failure(Instant::now());
                    continue;
                }
                Err(e) => {
                    // Reachable backend, definitive answer — trust it, propagate.
                    entry.breaker.lock().unwrap().on_success();
                    return Err(e);
                }
            }
        }
        self.http_download(path, offset, length).await
    }

    /// The HTTP FileStation Download implementation: throttle, transient retry,
    /// and DSM JSON-envelope detection. Used directly when no read backend is
    /// injected, and as the fallback when a backend trips its breaker.
    ///
    /// DSM violates HTTP convention by returning `200 OK` with a JSON error
    /// envelope (`{"success":false,"error":{"code":119}}`) when the SID is
    /// invalid, instead of a 4xx. We detect that case via the response body and
    /// surface `ApiError(code)` rather than returning the JSON as file content.
    pub(super) async fn http_download(
        &self,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Bytes, SynoFsError> {
        let url = format!("{}/entry.cgi", self.base_url);
        let path_json = serde_json::to_string(&[path]).unwrap();
        debug!("download: {} offset={} len={}", path, offset, length);

        let range_header = if length > 0 {
            Some(format!("bytes={}-{}", offset, offset + length - 1))
        } else {
            None
        };

        let max_attempts = self.max_transfer_attempts();
        let mut last_err = SynoFsError::Io("no attempts".into());

        for attempt in 0..max_attempts {
            if attempt > 0 {
                debug!("download retry {} for {} offset={}", attempt, path, offset);
            }

            // One attempt. The concurrency permit is held only for the duration
            // of the request+body read (this inner block); it is released before
            // any backoff so a sleeping-then-retrying request never occupies a
            // slot.
            let outcome: TransferOutcome = {
                let _slot = self.acquire_transfer_slot().await;

                let mut req = self.attach_session(self.http.get(&url).query(&[
                    ("api", "SYNO.FileStation.Download"),
                    ("version", "2"),
                    ("method", "download"),
                    ("path", &path_json),
                    ("mode", "download"),
                ]));
                if let Some(ref range) = range_header {
                    req = req.header("Range", range.as_str());
                }

                match req.send().await {
                    Err(e) => TransferOutcome::Retry {
                        hard: false,
                        err: e.into(),
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        // 416 Range Not Satisfiable = range starts past EOF;
                        // return empty (EOF signal).
                        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                            TransferOutcome::Done(Bytes::new())
                        } else if !status.is_success() {
                            let err = SynoFsError::Io(format!("download HTTP {}", status));
                            if http_status_is_transient(status) {
                                TransferOutcome::Retry { hard: false, err }
                            } else {
                                TransferOutcome::Fatal(err)
                            }
                        } else {
                            match resp.bytes().await {
                                Err(e) => TransferOutcome::Retry {
                                    hard: false,
                                    err: e.into(),
                                },
                                Ok(body) => classify_download_body(body),
                            }
                        }
                    }
                }
            };

            match outcome {
                TransferOutcome::Done(bytes) => return Ok(bytes),
                TransferOutcome::Fatal(e) => return Err(e),
                TransferOutcome::Retry { hard, err } => {
                    last_err = err;
                    if attempt + 1 < max_attempts {
                        self.backoff_before_retry(attempt, hard).await;
                    }
                }
            }
        }
        Err(last_err)
    }
}
