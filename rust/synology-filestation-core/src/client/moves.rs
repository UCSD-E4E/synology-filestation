//! Moving a file or folder without its bytes leaving the NAS.
//!
//! FileStation's Rename API takes a new *name*, never a new directory, so a
//! move between directories used to be a download, an upload and a delete:
//! a full copy through this machine for what the appliance can do on its own.
//! A metadata backend is asked first — SMB renames across directories in one
//! `SET_INFO`, replacing the destination atomically — and otherwise
//! FileStation's CopyMove task, with `remove_src`, moves on the appliance.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{debug, warn};

use super::verify::task_param;
use super::SynologyClient;
use crate::error::{ErrorCategory, SynoFsError};
use crate::types::{SynoApiError, SynoResponse};

/// How often the CopyMove task is polled. A move within one volume is a
/// rename and finishes before the first poll; one across volumes copies on
/// the appliance and takes as long as that does.
const MOVE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Request-level errors that mean the task was never started: no such API,
/// method or version, or parameters it did not understand. Nothing moved, so
/// a caller may try another way.
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

impl SynologyClient {
    /// Move `old_path` to the full path `new_path` on the NAS — another
    /// directory, another name, or both. A file already at `new_path` is
    /// replaced, as `rename(2)` replaces it.
    ///
    /// [`SynoFsError::NotSupported`] means nothing was moved and nothing could
    /// be without copying: FileStation refused the task before it started, or
    /// the move renames onto a name already taken in the source directory. A
    /// caller that can copy may do so then. Any other error came from a task
    /// that ran, and may have moved part of a folder.
    pub async fn move_to(&self, old_path: &str, new_path: &str) -> Result<(), SynoFsError> {
        debug!("move: {old_path} -> {new_path}");
        via_metadata!(self, move_to(old_path, new_path));

        let (src_dir, old_name) = Self::split_parent(old_path).ok_or(SynoFsError::InvalidArg)?;
        let (dest_dir, new_name) = Self::split_parent(new_path).ok_or(SynoFsError::InvalidArg)?;
        let dest_dir = if dest_dir.is_empty() { "/" } else { dest_dir };
        if old_name == new_name {
            return self.copymove(old_path, dest_dir).await;
        }

        // CopyMove keeps the name, so a move that renames is a rename in
        // place first. DSM's Rename never overwrites, so this order cannot
        // replace an unrelated file anywhere; moving first, with `overwrite`,
        // could replace one called `old_name` in the destination.
        let renamed = format!("{src_dir}/{new_name}");
        match self.get_info(&renamed).await {
            Ok(_) => return Err(SynoFsError::NotSupported),
            Err(SynoFsError::NotFound | SynoFsError::ApiError(408 | 414)) => {}
            Err(e) => return Err(e),
        }
        if let Err(e) = self.rename(old_path, new_name).await {
            debug!("move: renaming {old_path} in place first failed: {e}");
            return Err(SynoFsError::NotSupported);
        }
        match self.copymove(&renamed, dest_dir).await {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(back) = self.rename(&renamed, old_name).await {
                    warn!("move: could not rename {renamed} back to {old_name}: {back}");
                }
                Err(e)
            }
        }
    }

    /// One `SYNO.FileStation.CopyMove` task moving `path` into `dest_dir`.
    async fn copymove(&self, path: &str, dest_dir: &str) -> Result<(), SynoFsError> {
        // It can be a copy on the appliance, across volumes: the same load as
        // a transfer, so a throttled client counts it as one.
        let _slot = self.acquire_transfer_slot().await;
        let url = format!("{}/entry.cgi", self.base_url);
        let path_json = serde_json::to_string(&[path]).expect("a string always encodes");
        let dest_json = serde_json::to_string(dest_dir).expect("a string always encodes");

        let text = self
            .get_text_retried(
                &url,
                &[
                    ("api", "SYNO.FileStation.CopyMove"),
                    ("version", "3"),
                    ("method", "start"),
                    ("path", &path_json),
                    ("dest_folder_path", &dest_json),
                    ("overwrite", "true"),
                    ("remove_src", "true"),
                ],
            )
            .await?;
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
        let taskid = started
            .data
            .map(|d| d.taskid)
            .ok_or_else(|| SynoFsError::Io("move start returned no taskid".into()))?;

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
                .await?;
            let status: SynoResponse<StatusData> = serde_json::from_str(&text)
                .map_err(|e| SynoFsError::Io(format!("move status parse error: {e}")))?;
            if !status.success {
                return Err(SynoFsError::ApiError(
                    status.error.map(|e| e.code).unwrap_or(0),
                ));
            }
            if let Some(data) = status.data {
                if data.finished {
                    return match data.error {
                        // 1001 is FileStation's own "failed to move".
                        Some(e) => Err(SynoFsError::ApiError(if e.code == 0 {
                            1001
                        } else {
                            e.code
                        })),
                        None => Ok(()),
                    };
                }
            }
            tokio::time::sleep(MOVE_POLL_INTERVAL).await;
        }
    }
}
