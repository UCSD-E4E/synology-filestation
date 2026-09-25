//! Writing file content: the 10 MiB SLICEUPLOAD protocol and its fallbacks.

use super::throttle::http_status_is_transient;
use super::*;

/// Bytes per slice on the chunked upload path — the same 10 MiB DSM's own File
/// Station uploader uses (`chunksize` in `FileUploader.js`).
///
/// DSM only slices above `MAX_POST_FILESIZE` (4 GiB − 4096), because its
/// concern is the POST limit. Ours is memory: `http_upload` holds the whole
/// file in a `Vec<u8>`, so we slice anything that exceeds one slice.
pub const DEFAULT_SLICE_SIZE: usize = 10 * 1024 * 1024;

/// Deadline policy for a single upload request: `grace + bytes / floor_bps`.
///
/// An upload cannot use a flat timeout — the payload spans four orders of
/// magnitude (a text file to a 10 MiB slice) and the link spans three (LAN to
/// a congested VPN). A rate floor scales the allowance with the bytes actually
/// in flight, so a big slice on a slow link gets the minutes it needs while a
/// genuinely wedged connection still fails instead of parking a FUSE callback
/// forever.
#[derive(Clone, Copy, Debug)]
pub(super) struct UploadDeadline {
    /// Flat allowance on top of the transfer time, covering connect, TLS
    /// handshake and DSM writing the slice out before it answers.
    pub(super) grace: Duration,
    /// Slowest upload throughput we still treat as progress rather than a
    /// stall. Set well below any usable link: the point is to catch a dead
    /// connection, not to enforce a service level.
    pub(super) floor_bps: u64,
}

impl Default for UploadDeadline {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            // 32 KiB/s — a 10 MiB slice gets ~6 minutes.
            floor_bps: 32 * 1024,
        }
    }
}

impl UploadDeadline {
    pub(super) fn for_bytes(&self, bytes: u64) -> Duration {
        // Rounded up: truncating division would hand out slightly *less* than
        // the floor rate promises, which is the wrong direction for a timeout.
        self.grace + Duration::from_secs(bytes.div_ceil(self.floor_bps.max(1)))
    }
}

/// What a restart found before re-sending the file.
pub(super) enum Restart {
    /// The file is already on the NAS at its full size — the response we lost
    /// was the final slice's.
    AlreadyLanded,
    /// The local file is rewound and the way is clear to send it again.
    Ready,
}

/// Does this failure mean the server has thrown away the partial file we were
/// appending to?
///
/// DSM has no code for "your tmpfile is gone". A resend against a partial whose
/// upload session died answers 401 — its catch-all "unknown error of file
/// operation" — which is what e4e-nas returned after a connection timed out
/// mid-slice on a 540 MiB upload. Codes that carry a meaning (permission,
/// quota, no such folder) are answers about the write itself, and re-sending
/// gigabytes cannot change them.
pub(super) fn partial_was_rejected(err: &SynoFsError) -> bool {
    matches!(err, SynoFsError::ApiError(code) if dsm_code_to_category(*code) == ErrorCategory::Other)
}

/// Why one slice of a chunked upload failed, and what that implies for
/// resending it.
pub(super) enum SliceError {
    /// The server gave a definitive answer. Resending cannot change it.
    Fatal(SynoFsError),
    /// Worth another attempt. `may_have_landed` says whether the server might
    /// already hold these bytes — if it does, a resend appends them twice and
    /// the finished file has to be verified. `hard` doubles the backoff (DSM
    /// asking for a pause).
    Retryable {
        err: SynoFsError,
        may_have_landed: bool,
        hard: bool,
    },
}

/// A local file's modification time in milliseconds since the epoch, as DSM's
/// upload API expects it. `None` when the filesystem can't report one — the
/// upload still proceeds, it just lands with the NAS's own timestamp.
pub(super) async fn local_mtime_ms(path: &Path) -> Option<String> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis().to_string())
}

impl SynologyClient {
    /// Override the slice size used by the chunked upload path (default
    /// [`DEFAULT_SLICE_SIZE`]). A file larger than this is uploaded slice by
    /// slice; anything smaller takes the one-shot path. Mostly useful for tests
    /// and for tuning against a slow link.
    pub fn with_slice_size(mut self, bytes: usize) -> Self {
        self.slice_size = bytes.max(1);
        self
    }

    /// Write a whole file, preferring any injected [`WriteTransport`] backend
    /// (SMB, …) over the HTTP Upload API.
    ///
    /// Same selection + circuit-breaker semantics as [`download`](Self::download).
    /// A backend's `write` is atomic (old-or-nothing), so falling back to HTTP
    /// after a failed attempt can't collide with a half-written file. With no
    /// backends injected this is exactly the HTTP upload.
    pub async fn upload(
        &self,
        folder_path: &str,
        filename: &str,
        data: Vec<u8>,
        overwrite: bool,
    ) -> Result<(), SynoFsError> {
        // Only route *replacing* writes through a write backend. A backend's
        // `write` unconditionally replaces the file, so it can't honor
        // `overwrite=false`'s "fail if the file already exists" contract —
        // those go straight to the HTTP path, which does.
        if overwrite && !self.write_transports.is_empty() {
            let full_path = format!("{}/{}", folder_path.trim_end_matches('/'), filename);
            for entry in &self.write_transports {
                let allowed = entry.breaker.lock().unwrap().allows(Instant::now());
                if !allowed {
                    continue;
                }
                match entry.transport.write(&full_path, &data).await {
                    Ok(()) => {
                        entry.breaker.lock().unwrap().on_success();
                        return Ok(());
                    }
                    // Not this backend, not now — an SMB transport with no
                    // session yet says so. An answer, so the breaker stays
                    // shut and it is asked again next time.
                    Err(e) if e.category() == ErrorCategory::NotSupported => {
                        debug!("write backend declined, using HTTP: {e}");
                        entry.answered();
                        continue;
                    }
                    Err(e) if e.category() == ErrorCategory::Transport => {
                        warn!("write backend failed (transient), falling back: {e}");
                        entry.breaker.lock().unwrap().on_failure(Instant::now());
                        continue;
                    }
                    Err(e) => {
                        entry.breaker.lock().unwrap().on_success();
                        return Err(e);
                    }
                }
            }
        }
        self.http_upload(folder_path, filename, data, overwrite, None)
            .await
    }

    /// Stream a local file to the NAS, preferring a [`StreamWriteTransport`]
    /// backend (SMB) so a large file is never buffered in memory.
    ///
    /// Same selection + circuit-breaker + `overwrite` semantics as
    /// [`upload`](Self::upload). On fallback (no stream backend, or a transient
    /// failure) the file is read into memory and sent over HTTP — so the memory
    /// win applies to the streaming path, and correctness holds either way.
    pub async fn upload_from_path(
        &self,
        local: &Path,
        folder_path: &str,
        filename: &str,
        overwrite: bool,
    ) -> Result<(), SynoFsError> {
        self.upload_from_path_with_progress(local, folder_path, filename, overwrite, None)
            .await
    }

    /// [`upload_from_path`](Self::upload_from_path) with a progress sink called
    /// once per slice with cumulative bytes. Slice boundaries are internal to
    /// this crate, so a caller that wants a moving progress bar (the GUI, via
    /// the FFI) can only learn about them here.
    pub async fn upload_from_path_with_progress(
        &self,
        local: &Path,
        folder_path: &str,
        filename: &str,
        overwrite: bool,
        progress: Option<ProgressSink<'_>>,
    ) -> Result<(), SynoFsError> {
        if !self.stream_write_transports.is_empty() {
            let full_path = format!("{}/{}", folder_path.trim_end_matches('/'), filename);
            for entry in &self.stream_write_transports {
                let allowed = entry.breaker.lock().unwrap().allows(Instant::now());
                if !allowed {
                    continue;
                }
                // Creating and replacing are different promises, so they are
                // different calls. A new file used to skip the backends
                // entirely — which exempted the case a mount spends its
                // bandwidth on, since a large copy is a *new* file.
                let attempt = if overwrite {
                    entry.transport.write_from_path(&full_path, local).await
                } else {
                    entry.transport.write_new_from_path(&full_path, local).await
                };
                match attempt {
                    Ok(()) => {
                        entry.breaker.lock().unwrap().on_success();
                        return Ok(());
                    }
                    // Declining a case it cannot promise is not a failure:
                    // leave the breaker shut so this backend still gets the
                    // writes it can serve.
                    Err(e) if e.category() == ErrorCategory::NotSupported => {
                        debug!("stream write backend declined, using HTTP: {e}");
                        entry.answered();
                        continue;
                    }
                    Err(e) if e.category() == ErrorCategory::Transport => {
                        warn!("stream write backend failed (transient), falling back: {e}");
                        entry.breaker.lock().unwrap().on_failure(Instant::now());
                        continue;
                    }
                    Err(e) => {
                        entry.breaker.lock().unwrap().on_success();
                        return Err(e);
                    }
                }
            }
        }
        // Fallback: HTTP. A file bigger than one slice goes down the chunked
        // path so it is never resident in memory whole; smaller files keep the
        // existing one-shot behavior.
        let len = tokio::fs::metadata(local)
            .await
            .map_err(|e| {
                SynoFsError::Io(format!(
                    "upload_from_path: stat {} failed: {e}",
                    local.display()
                ))
            })?
            .len();
        if len > self.slice_size as u64 {
            return self
                .http_slice_upload(local, folder_path, filename, len, overwrite, progress)
                .await;
        }
        let data = tokio::fs::read(local).await.map_err(|e| {
            SynoFsError::Io(format!(
                "upload_from_path: read {} failed: {e}",
                local.display()
            ))
        })?;
        self.http_upload(
            folder_path,
            filename,
            data,
            overwrite,
            local_mtime_ms(local).await,
        )
        .await?;
        // One-shot: the only boundary we can report is the end of the file.
        if let Some(p) = progress {
            p(len, len);
        }
        Ok(())
    }

    /// Chunked ("slice") upload — the path DSM's own File Station uploader uses
    /// for large files, reimplemented from a capture of it plus its JS source.
    ///
    /// Each slice is one POST carrying the same body fields; the chunking rides
    /// in headers. The server hands back a `tmpfile` handle on the first slice,
    /// which every later slice echoes as `X-TMP-FILE` to append to the same
    /// partial file. The final slice sets `X-FILE-CHUNK-END: true` and its
    /// response is the result — there is no separate finalize call.
    ///
    /// Memory is bounded by one slice, in contrast to [`Self::http_upload`],
    /// which holds the whole file (and clones it per retry attempt).
    ///
    /// A failed slice is resent (bounded by [`Self::max_transfer_attempts`])
    /// rather than costing the whole file. DSM has no resume — it appends each
    /// slice blindly and never reports how much of the partial it holds — so a
    /// resend after the body may already have arrived can append the same bytes
    /// twice. That is why every completed upload is checked against what landed
    /// (see [`Self::verify_upload`]); a slice we cannot vouch for is never
    /// reported as a successful write.
    pub(super) async fn http_slice_upload(
        &self,
        local: &Path,
        folder_path: &str,
        filename: &str,
        total: u64,
        overwrite: bool,
        progress: Option<ProgressSink<'_>>,
    ) -> Result<(), SynoFsError> {
        use tokio::io::AsyncReadExt;

        let url = format!("{}/entry.cgi", self.base_url);
        let slice_size = self.slice_size;
        let slices = total.div_ceil(slice_size as u64).max(1);
        debug!(
            "slice upload: {}/{} ({} bytes, {} slices of {})",
            folder_path, filename, total, slices, slice_size
        );

        if overwrite {
            self.clear_for_overwrite(folder_path, filename).await;
        }

        let mtime_ms = local_mtime_ms(local).await;

        let mut file = tokio::fs::File::open(local).await.map_err(|e| {
            SynoFsError::Io(format!(
                "slice upload: open {} failed: {e}",
                local.display()
            ))
        })?;
        let mut buf = vec![0u8; slice_size];
        let max_attempts = self.max_transfer_attempts();
        let full_path = format!("{}/{}", folder_path.trim_end_matches('/'), filename);
        // Each pass builds one partial file on the server. A pass that ends
        // with the server disowning that partial starts another.
        let mut restarts = 0u32;

        let unverified_resend = 'session: loop {
            let mut tmpfile: Option<String> = None;
            // Set when a resend might have appended a slice the server already
            // held. Only then is the finished file worth hashing.
            let mut unverified_resend = false;

            for index in 0..slices {
                let want =
                    std::cmp::min(slice_size as u64, total - index * slice_size as u64) as usize;
                file.read_exact(&mut buf[..want]).await.map_err(|e| {
                    SynoFsError::Io(format!("slice upload: read slice {index} failed: {e}"))
                })?;
                let last = index + 1 == slices;

                let mut attempt = 0u32;
                let parsed = loop {
                    let outcome = self
                        .send_slice(
                            &url,
                            folder_path,
                            filename,
                            &buf[..want],
                            total,
                            last,
                            tmpfile.as_deref(),
                            mtime_ms.as_deref(),
                        )
                        .await;
                    match outcome {
                        Ok(parsed) => break parsed,
                        Err(SliceError::Fatal(e)) => {
                            // A verdict about the write (permission, quota, no
                            // such folder) stands. A catch-all code while we
                            // were appending to a partial means something else:
                            // the server no longer has that partial, and the
                            // only way back is a new one.
                            if tmpfile.is_none()
                                || !partial_was_rejected(&e)
                                || restarts + 1 >= max_attempts
                            {
                                return Err(e);
                            }
                            restarts += 1;
                            warn!(
                                "slice upload: {full_path} partial rejected at slice {index} ({e}); \
                                 starting the file over (restart {restarts})"
                            );
                            match self
                                .restart_slice_upload(
                                    &full_path,
                                    folder_path,
                                    filename,
                                    total,
                                    &mut file,
                                )
                                .await?
                            {
                                // The lost response was the final slice's: the
                                // file is already there. It got there through a
                                // resend, so its contents still get checked.
                                Restart::AlreadyLanded => break 'session true,
                                Restart::Ready => continue 'session,
                            }
                        }
                        Err(SliceError::Retryable {
                            err,
                            may_have_landed,
                            hard,
                        }) => {
                            attempt += 1;
                            if attempt >= max_attempts {
                                return Err(err);
                            }
                            // Resending the *first* slice cannot double
                            // anything: with no tmpfile handle it opens a fresh
                            // partial file, and the abandoned one is the
                            // server's to reap. From the second slice on, the
                            // server may already hold what we are about to send
                            // again.
                            if may_have_landed && tmpfile.is_some() {
                                unverified_resend = true;
                            }
                            warn!(
                                "slice upload: slice {index} attempt {attempt} failed, resending: {err}"
                            );
                            self.backoff_before_retry(attempt - 1, hard).await;
                        }
                    }
                };

                if let Some(p) = progress {
                    p(index * slice_size as u64 + want as u64, total);
                }

                if !last {
                    // Without a handle there is nothing for the next slice to
                    // append to; DSM's own client treats this as fatal rather
                    // than retrying.
                    tmpfile = match parsed
                        .data
                        .and_then(|d| d.tmpfile)
                        .filter(|t| !t.is_empty())
                    {
                        Some(t) => Some(t),
                        None => {
                            return Err(SynoFsError::Io(format!(
                                "slice upload: server returned no tmpfile after slice {index}"
                            )))
                        }
                    };
                }
            }

            break 'session unverified_resend;
        };

        self.verify_upload(&full_path, local, total, unverified_resend)
            .await
    }

    /// Get ready to send the file again after the server disowned our partial.
    ///
    /// Looks before it leaps: the response that went missing may have been the
    /// *final* slice's, in which case the file is already on the NAS and
    /// re-sending it would be gigabytes of pointless traffic — and, with
    /// `overwrite=false`, a collision with our own write. Otherwise whatever we
    /// left behind is ours and is wrong, so it goes before the retry rewinds
    /// and starts over.
    pub(super) async fn restart_slice_upload(
        &self,
        full_path: &str,
        folder_path: &str,
        filename: &str,
        total: u64,
        file: &mut tokio::fs::File,
    ) -> Result<Restart, SynoFsError> {
        // Unlike `landed_size`, a missing file is the expected case here, so it
        // is not worth a warning.
        let landed = self
            .get_info(full_path)
            .await
            .ok()
            .and_then(|info| info.additional.and_then(|a| a.size));
        if landed == Some(total) {
            debug!("slice upload: {full_path} landed after all; not sending it again");
            return Ok(Restart::AlreadyLanded);
        }

        self.clear_for_overwrite(folder_path, filename).await;
        use tokio::io::AsyncSeekExt;
        file.seek(std::io::SeekFrom::Start(0)).await.map_err(|e| {
            SynoFsError::Io(format!("slice upload: rewind for restart failed: {e}"))
        })?;
        Ok(Restart::Ready)
    }

    /// Send one slice and classify what came back.
    ///
    /// The classification that matters is `may_have_landed`: whether the server
    /// could already hold these bytes. Only a failure in the connect/TLS phase
    /// proves it does not.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_slice(
        &self,
        url: &str,
        folder_path: &str,
        filename: &str,
        chunk: &[u8],
        total: u64,
        last: bool,
        tmpfile: Option<&str>,
        mtime_ms: Option<&str>,
    ) -> Result<SynoResponse<UploadData>, SliceError> {
        let file_part = multipart::Part::bytes(chunk.to_vec())
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| SliceError::Fatal(SynoFsError::Io(e.to_string())))?;
        let mut form = multipart::Form::new()
            .text("overwrite", "false")
            .text("create_parents", "true")
            .text("path", folder_path.to_string());
        if let Some(ms) = mtime_ms {
            form = form.text("mtime", ms.to_string());
        }
        let form = form.part("file", file_part);

        let mut req = self
            .attach_session(self.http_transfer.post(url).query(&[
                ("api", "SYNO.FileStation.Upload"),
                ("method", "upload"),
                ("version", "2"),
            ]))
            // Sized to this slice, not to the file: each slice is its own
            // request, so a 6 GB upload is a long series of ~6-minute deadlines
            // rather than one open-ended wait.
            .timeout(self.upload_deadline.for_bytes(chunk.len() as u64))
            .header("X-TYPE-NAME", "SLICEUPLOAD")
            .header("X-FILE-SIZE", total.to_string())
            .header("X-FILE-CHUNK-END", if last { "true" } else { "false" });
        if let Some(t) = tmpfile {
            req = req.header("X-TMP-FILE", t.to_string());
        }

        let text = {
            let _slot = self.acquire_transfer_slot().await;
            let resp = match req.multipart(form).send().await {
                Ok(r) => r,
                Err(e) => {
                    // A connect/TLS failure is the one case where the body
                    // provably never left this machine. A timeout, a reset
                    // mid-body or a lost response all leave the question open.
                    let may_have_landed = !e.is_connect();
                    return Err(SliceError::Retryable {
                        err: SynoFsError::from(e),
                        may_have_landed,
                        hard: false,
                    });
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let err = SynoFsError::Io(format!("slice upload HTTP {status}"));
                return Err(if http_status_is_transient(status) {
                    SliceError::Retryable {
                        err,
                        may_have_landed: true,
                        hard: false,
                    }
                } else {
                    SliceError::Fatal(err)
                });
            }
            match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    return Err(SliceError::Retryable {
                        err: SynoFsError::from(e),
                        may_have_landed: true,
                        hard: false,
                    })
                }
            }
        };

        let parsed: SynoResponse<UploadData> = serde_json::from_str(&text).map_err(|e| {
            SliceError::Fatal(SynoFsError::Io(format!("slice upload parse error: {e}")))
        })?;
        if !parsed.success {
            let code = parsed.error.map(|e| e.code).unwrap_or(0);
            // 402 is the appliance asking for a pause, not a verdict on the
            // request — back off harder and try the slice again.
            return Err(if dsm_code_to_category(code) == ErrorCategory::Busy {
                SliceError::Retryable {
                    err: SynoFsError::ApiError(code),
                    may_have_landed: true,
                    hard: true,
                }
            } else {
                SliceError::Fatal(SynoFsError::ApiError(code))
            });
        }
        Ok(parsed)
    }

    /// Set a file's length.
    ///
    /// A backend that can express this does it in one round trip. The HTTP
    /// FileStation API cannot: it has no length operation, only "upload this
    /// whole file", so the fallback reads the part worth keeping and writes it
    /// back — moving a file's contents to change one number.
    ///
    /// The fallback reads only what it keeps. Shrinking is the common case
    /// (`O_TRUNC`, `> file`, a writer rewinding), and fetching the tail that is
    /// about to be discarded would double an already regrettable transfer.
    pub async fn truncate(&self, path: &str, size: u64) -> Result<(), SynoFsError> {
        debug!("truncate: {} to {}", path, size);
        via_metadata!(self, truncate(path, size));

        let (parent, filename) = match Self::split_parent(path) {
            Some(v) => v,
            None => return Err(SynoFsError::InvalidArg),
        };

        let data = if size == 0 {
            // Nothing to keep, so nothing to fetch: the common case costs one
            // request instead of a download of a file we are discarding.
            Vec::new()
        } else {
            // `length = 0` means "the whole file". Ask for only `size` bytes
            // when the file is longer than that; when it is shorter (a grow)
            // we need all of it before padding.
            let current = self
                .get_info(path)
                .await
                .ok()
                .and_then(|i| i.additional.and_then(|a| a.size));
            let want = match current {
                Some(len) if len > size => size,
                _ => 0,
            };
            // Checked, not `as`: the fallback has to materialise the whole
            // result in memory, so a size this machine cannot address is an
            // argument error rather than a silently wrapped length.
            let len = usize::try_from(size).map_err(|_| SynoFsError::InvalidArg)?;
            let mut data = self.download(path, 0, want).await?.to_vec();
            data.resize(len, 0);
            data
        };

        self.upload(parent, filename, data, true).await
    }

    /// Delete a file that is in the way of an upload, then wait for it to
    /// actually disappear. Shared by the one-shot and slice upload paths:
    /// `overwrite=true` on the multipart API times out on some DSM versions, so
    /// both always upload with `overwrite=false` onto cleared ground. Delete is
    /// async on modern DSM, hence the poll — otherwise the upload races it and
    /// fails 418 AlreadyExists.
    pub(super) async fn clear_for_overwrite(&self, folder_path: &str, filename: &str) {
        let full_path = format!("{}/{}", folder_path.trim_end_matches('/'), filename);
        let _ = self.delete(&full_path).await; // ignore error — file may not exist yet
        for _ in 0..10u8 {
            match self.get_info(&full_path).await {
                Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(_) => break, // gone or inaccessible — safe to upload
            }
        }
    }

    /// The HTTP FileStation Upload implementation. Used directly when no write
    /// backend is injected, and as the fallback when a backend trips its breaker.
    pub(super) async fn http_upload(
        &self,
        folder_path: &str,
        filename: &str,
        data: Vec<u8>,
        overwrite: bool,
        // Local modification time in ms, when the caller has a file to take it
        // from. DSM stores it verbatim; without it the NAS stamps upload time.
        mtime_ms: Option<String>,
    ) -> Result<(), SynoFsError> {
        let url = format!("{}/entry.cgi", self.base_url);
        debug!(
            "upload: {}/{} ({} bytes)",
            folder_path,
            filename,
            data.len()
        );

        let max_attempts = self.max_transfer_attempts();
        let mut last_err = SynoFsError::Io("no attempts".into());
        for attempt in 0..max_attempts {
            if attempt > 0 {
                debug!("upload retry {} for {}/{}", attempt, folder_path, filename);
            }

            // Re-clear on *every* attempt, not once up front. Both upload paths
            // always POST with `overwrite=false` (DSM's multipart overwrite
            // times out on some versions), so an attempt only succeeds onto
            // cleared ground. A previous attempt whose response was lost may
            // well have landed the file — retrying without re-clearing would
            // then get 418 and report a write that *succeeded* as
            // AlreadyExists. Re-clearing is also what makes retrying an upload
            // safe at all.
            if overwrite {
                self.clear_for_overwrite(folder_path, filename).await;
            }

            let file_part = multipart::Part::bytes(data.clone())
                .file_name(filename.to_string())
                .mime_str("application/octet-stream")
                .map_err(|e| SynoFsError::Io(e.to_string()))?;

            let form = multipart::Form::new()
                .text("api", "SYNO.FileStation.Upload")
                .text("version", "3")
                .text("method", "upload")
                .text("path", folder_path.to_string())
                .text("create_parents", "true")
                .text("size", data.len().to_string())
                .text("overwrite", "false")
                .part("file", file_part);
            let form = match &mtime_ms {
                Some(ms) => form.text("mtime", ms.clone()),
                None => form,
            };

            // Hold a transfer slot only for the request+response read; drop it
            // before any backoff so a retrying upload doesn't hog a permit.
            // `Ok(text)` = a 2xx body to parse; `Err(e)` = retry this attempt.
            // A non-transient HTTP status returns immediately: the server gave
            // a definitive refusal, and resending the same request cannot help.
            let text = {
                let _slot = self.acquire_transfer_slot().await;
                match self
                    .attach_session(self.http_transfer.post(&url))
                    .timeout(self.upload_deadline.for_bytes(data.len() as u64))
                    .multipart(form)
                    .send()
                    .await
                {
                    Err(e) => Err(SynoFsError::from(e)),
                    Ok(r) => {
                        let status = r.status();
                        if status.is_success() {
                            r.text().await.map_err(SynoFsError::from)
                        } else {
                            let err = SynoFsError::Io(format!("upload HTTP {}", status));
                            if http_status_is_transient(status) {
                                Err(err)
                            } else {
                                return Err(err);
                            }
                        }
                    }
                }
            };

            let text = match text {
                Ok(t) => t,
                Err(e) => {
                    last_err = e;
                    if attempt + 1 < max_attempts {
                        self.backoff_before_retry(attempt, false).await;
                    }
                    continue;
                }
            };

            debug!("upload raw response: {}", text);

            let parsed: SynoResponse<UploadData> = serde_json::from_str(&text)
                .map_err(|e| SynoFsError::Io(format!("upload parse error: {e}")))?;

            return if parsed.success {
                Ok(())
            } else {
                let code = parsed.error.map(|e| e.code).unwrap_or(0);
                Err(SynoFsError::ApiError(code))
            };
        }
        Err(last_err)
    }
}
