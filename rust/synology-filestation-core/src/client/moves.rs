//! Moving a file or folder without its bytes leaving the NAS.
//!
//! FileStation's Rename API takes a new *name*, never a new directory, so a
//! move between directories used to be a download, an upload and a delete:
//! a full copy through this machine for what the appliance can do on its own.
//! A metadata backend is asked first — SMB renames across directories in one
//! `SET_INFO` — and otherwise FileStation's CopyMove task, with `remove_src`,
//! moves on the appliance.
//!
//! What a move may replace is `rename(2)`'s rule ([`plan_move`]), not
//! CopyMove's: with `overwrite`, CopyMove merges a folder into one already
//! there and overwrites what both hold.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{debug, warn};

use super::verify::task_param;
use super::SynologyClient;
use crate::error::{ErrorCategory, SynoFsError};
use crate::transport::{plan_move, Destination, MovePlan};
use crate::types::{ListData, SynoApiError, SynoResponse};

/// How often the CopyMove task is polled. A move within one volume is a
/// rename and finishes before the first poll; one across volumes copies on
/// the appliance and takes as long as that does.
const MOVE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long a move is waited for. Past it the task is **not** stopped: a
/// move stopped halfway leaves a folder split between two places, which is
/// worse than one that finishes late. The caller is told it is still running.
const MOVE_MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// Request-level errors that mean the task was never started: no such API,
/// method or version, or parameters it did not understand.
const NEVER_STARTED: [u32; 4] = [101, 102, 103, 104];

#[derive(Deserialize)]
struct StartData {
    taskid: String,
}

#[derive(Deserialize)]
struct StatusData {
    #[serde(default)]
    finished: bool,
    #[serde(default)]
    error: Option<SynoApiError>,
}

/// A failed CopyMove, and whether the task can be known not to have run.
struct MoveFailure {
    error: SynoFsError,
    /// The appliance certainly did nothing, so a rename in place made for
    /// this move may be put back.
    nothing_ran: bool,
}

impl SynologyClient {
    /// Move `old_path` to the full path `new_path` on the NAS — another
    /// directory, another name, or both — replacing what is there only as
    /// `rename(2)` would ([`plan_move`]).
    ///
    /// [`SynoFsError::NotSupported`] means nothing was moved and nothing could
    /// be without copying: FileStation refused the task before it started, or
    /// the move renames onto a name already taken in the source directory. A
    /// caller that can copy may do so then. Any other error may come from a
    /// task that ran, and moved part of a folder.
    pub async fn move_to(&self, old_path: &str, new_path: &str) -> Result<(), SynoFsError> {
        self.move_within(old_path, new_path, MOVE_MAX_WAIT).await
    }

    /// [`Self::move_to`], waiting at most `max_wait` for FileStation's task.
    pub(super) async fn move_within(
        &self,
        old_path: &str,
        new_path: &str,
        max_wait: Duration,
    ) -> Result<(), SynoFsError> {
        debug!("move: {old_path} -> {new_path}");
        via_metadata!(self, move_to(old_path, new_path));

        let deadline = Instant::now() + max_wait;
        let (src_dir, old_name) = Self::split_parent(old_path).ok_or(SynoFsError::InvalidArg)?;
        let (dest_dir, new_name) = Self::split_parent(new_path).ok_or(SynoFsError::InvalidArg)?;
        let dest_dir = if dest_dir.is_empty() { "/" } else { dest_dir };

        let source_is_dir = self.get_info(old_path).await?.isdir;
        let plan = plan_move(source_is_dir, self.destination(new_path).await?)?;

        // CopyMove keeps the name, so a move that renames is a rename in
        // place first. DSM's Rename never overwrites, which makes it the check
        // that the new name is free here; moving first, with `overwrite`,
        // could replace a file of the old name in the destination.
        let renamed = if old_name == new_name {
            None
        } else {
            match self.rename(old_path, new_name).await {
                Ok(_) => Some(format!("{src_dir}/{new_name}")),
                Err(e) if e.category() == ErrorCategory::PermissionDenied => return Err(e),
                Err(e) => {
                    debug!("move: renaming {old_path} in place first failed: {e}");
                    return Err(SynoFsError::NotSupported);
                }
            }
        };
        let moving = renamed.as_deref().unwrap_or(old_path);

        let outcome = async {
            if plan == MovePlan::ReplaceEmptyDir {
                self.delete(new_path).await.map_err(|error| MoveFailure {
                    error,
                    nothing_ran: true,
                })?;
            }
            self.copymove(moving, dest_dir, plan == MovePlan::ReplaceFile, deadline)
                .await
        }
        .await;

        match outcome {
            Ok(()) => Ok(()),
            Err(failure) => {
                // Only when the task cannot have run. A status poll that failed
                // says nothing about the task, which may be moving the file
                // right now; renaming it underneath would split it.
                if failure.nothing_ran {
                    if let Some(renamed) = &renamed {
                        if let Err(back) = self.rename(renamed, old_name).await {
                            warn!("move: could not rename {renamed} back to {old_name}: {back}");
                        }
                    }
                }
                Err(failure.error)
            }
        }
    }

    /// What is at `path`, for [`plan_move`]. A name DSM will not show is
    /// treated as absent: the move then sends no `overwrite`, so if something
    /// is there after all the task fails rather than replacing it.
    async fn destination(&self, path: &str) -> Result<Destination, SynoFsError> {
        match self.get_info(path).await {
            Ok(info) if info.isdir => Ok(Destination::Dir {
                empty: !self.has_entries(path).await?,
            }),
            Ok(_) => Ok(Destination::File),
            Err(SynoFsError::NotFound | SynoFsError::ApiError(408 | 414)) => {
                Ok(Destination::Absent)
            }
            Err(e) => Err(e),
        }
    }

    /// Whether the folder `path` holds anything: one entry asked for, not a
    /// listing of what may be a million.
    async fn has_entries(&self, path: &str) -> Result<bool, SynoFsError> {
        let url = format!("{}/entry.cgi", self.base_url);
        let text = self
            .get_text_retried(
                &url,
                &[
                    ("api", "SYNO.FileStation.List"),
                    ("version", "2"),
                    ("method", "list"),
                    ("folder_path", path),
                    ("limit", "1"),
                    ("offset", "0"),
                ],
            )
            .await?;
        let resp: SynoResponse<ListData> = serde_json::from_str(&text)
            .map_err(|e| SynoFsError::Io(format!("move: list parse error: {e}")))?;
        if !resp.success {
            return Err(SynoFsError::ApiError(
                resp.error.map(|e| e.code).unwrap_or(0),
            ));
        }
        Ok(resp.data.is_some_and(|d| !d.files.is_empty()))
    }

    /// One `SYNO.FileStation.CopyMove` task moving `path` into `dest_dir`.
    async fn copymove(
        &self,
        path: &str,
        dest_dir: &str,
        overwrite: bool,
        deadline: Instant,
    ) -> Result<(), MoveFailure> {
        // It can be a copy on the appliance, across volumes: the same load as
        // a transfer, so a throttled client counts it as one.
        let _slot = self.acquire_transfer_slot().await;
        let taskid = self
            .copymove_start(path, dest_dir, overwrite)
            .await
            .map_err(|error| MoveFailure {
                // A refusal DSM answered means it never started; an answer
                // that was lost means it may have.
                nothing_ran: !matches!(error, SynoFsError::Io(_)),
                error,
            })?;
        let ran = |error| MoveFailure {
            error,
            nothing_ran: false,
        };

        let url = format!("{}/entry.cgi", self.base_url);
        loop {
            let text = self
                .get_text_retried(
                    &url,
                    &[
                        ("api", "SYNO.FileStation.CopyMove"),
                        ("version", "3"),
                        ("method", "status"),
                        ("taskid", &task_param(&taskid)),
                    ],
                )
                .await
                .map_err(ran)?;
            let status: SynoResponse<StatusData> = serde_json::from_str(&text)
                .map_err(|e| ran(SynoFsError::Io(format!("move status parse error: {e}"))))?;
            if !status.success {
                return Err(ran(SynoFsError::ApiError(
                    status.error.map(|e| e.code).unwrap_or(0),
                )));
            }
            if let Some(data) = status.data {
                if data.finished {
                    return match data.error {
                        // 1001 is FileStation's own "failed to move".
                        Some(e) => Err(ran(SynoFsError::ApiError(if e.code == 0 {
                            1001
                        } else {
                            e.code
                        }))),
                        None => Ok(()),
                    };
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ran(SynoFsError::Io(format!(
                    "move of {path} is still running on the NAS; it was left to finish"
                ))));
            }
            tokio::time::sleep(MOVE_POLL_INTERVAL.min(left)).await;
        }
    }

    /// Start the CopyMove task; its id.
    ///
    /// Sent once ([`get_text_once`](Self::get_text_once)): a resend after a
    /// lost answer would start a second task on the same path, which races
    /// the first. `overwrite` is sent only to replace a file; without it, a
    /// name taken at the destination fails the task instead of being
    /// overwritten.
    pub(super) async fn copymove_start(
        &self,
        path: &str,
        dest_dir: &str,
        overwrite: bool,
    ) -> Result<String, SynoFsError> {
        let url = format!("{}/entry.cgi", self.base_url);
        let path_json = serde_json::to_string(&[path]).expect("a string always encodes");
        let dest_json = serde_json::to_string(dest_dir).expect("a string always encodes");
        let mut params = vec![
            ("api", "SYNO.FileStation.CopyMove"),
            ("version", "3"),
            ("method", "start"),
            ("path", path_json.as_str()),
            ("dest_folder_path", dest_json.as_str()),
            ("remove_src", "true"),
        ];
        if overwrite {
            params.push(("overwrite", "true"));
        }

        let text = self.get_text_once(&url, &params).await?;
        let started: SynoResponse<StartData> = serde_json::from_str(&text)
            .map_err(|e| SynoFsError::Io(format!("move start parse error: {e}")))?;
        if !started.success {
            let code = started.error.map(|e| e.code).unwrap_or(0);
            return Err(if NEVER_STARTED.contains(&code) {
                debug!("move: FileStation refused the CopyMove task ({code})");
                SynoFsError::NotSupported
            } else {
                SynoFsError::ApiError(code)
            });
        }
        started
            .data
            .map(|d| d.taskid)
            .ok_or_else(|| SynoFsError::Io("move start returned no taskid".into()))
    }
}
