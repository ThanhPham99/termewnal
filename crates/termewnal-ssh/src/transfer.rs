use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rand::RngExt;
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpClientError;
use russh_sftp::protocol::{OpenFlags, StatusCode};
use termewnal_core::{TransferConfig, TransferConflictPolicy};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore, watch};

use crate::auth::BoxFuture;
use crate::dir_transfer::{
    DownloadDirBatch, UploadDirBatch, apply_local_permissions, apply_remote_permissions,
    collect_local_dir_plan, collect_remote_dir_plan, extract_local_posix_mode,
};
pub use crate::dir_transfer::DirectoryTransferSummary;
use crate::error::{SshError, map_remote_sftp_io_error};
use crate::sftp::{
    ProfileTransportKey, SftpEndpoint, SftpManager, SftpTransportMode, join_remote_path,
    parent_remote_path,
};

/// Default maximum number of simultaneous active `Transfer`s.
pub const DEFAULT_MAX_CONCURRENT_TRANSFERS: usize = 3;
/// Default pipelined chunk size (`255 KiB`, fitting within a single `256 KiB` SFTP write packet).
pub const DEFAULT_TRANSFER_CHUNK_SIZE: usize = 255 * 1024;
/// Default automatic transport-drop reconnect attempts during an active `Transfer`.
pub const DEFAULT_MAX_TRANSPORT_RETRIES: u32 = 5;
/// Default base backoff delay between automatic transport-drop retries (`400 ms` exponential).
pub const DEFAULT_RECONNECT_RETRY_DELAY: Duration = Duration::from_millis(400);
const MAX_RECONNECT_BACKOFF_DELAY: Duration = Duration::from_secs(15);
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(120);
const MIN_TRANSFER_CHUNK_SIZE: usize = 128 * 1024;
const MAX_TRANSFER_CHUNK_SIZE: usize = 256 * 1024;

/// Returns the staging `.termewnal.part` remote path for `remote_path`.
pub fn part_path_for(remote_path: &str) -> String {
    format!("{remote_path}.termewnal.part")
}

/// Returns the local staging `.termewnal.part` path for `local_path`.
pub fn local_part_path_for(local_path: &Path) -> PathBuf {
    let mut raw: OsString = local_path.as_os_str().to_os_string();
    raw.push(".termewnal.part");
    PathBuf::from(raw)
}

/// Structured non-blocking status notice attached to a `Transfer` when fallback or safety rules apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferNotice {
    /// Creating or renaming `<filename>.termewnal.part` failed with permission/unsupported error;
    /// `Transfer` automatically fell back to direct-path writes.
    PartFallbackDirectWrite { reason: String },
    /// Remote or local POSIX `chmod` permission preservation was unsupported or denied.
    PermissionPreserveSkipped { reason: String },
    /// Symlink or symlink loop was skipped safely during recursive directory traversal.
    SymlinkSkipped { path: String, is_loop: bool },
}

impl TransferNotice {
    /// Formats a concise user-facing non-blocking notice message.
    pub fn summary(&self) -> String {
        match self {
            Self::PartFallbackDirectWrite { reason } => {
                format!("Atomic .part upload unavailable ({reason}); fell back to direct write.")
            }
            Self::PermissionPreserveSkipped { reason } => {
                format!("Could not preserve POSIX permissions ({reason}); data transfer completed.")
            }
            Self::SymlinkSkipped { path, is_loop: true } => {
                format!("Skipped symlink loop at {path}.")
            }
            Self::SymlinkSkipped {
                path,
                is_loop: false,
            } => format!("Skipped symlink at {path}."),
        }
    }
}

/// Unique identifier for a queued or historical SFTP `Transfer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(pub u64);

/// Direction of an SFTP `Transfer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

impl TransferDirection {
    /// Concise display label for UI badges.
    pub fn label(self) -> &'static str {
        match self {
            Self::Upload => "Upload",
            Self::Download => "Download",
        }
    }
}

/// User decision resolving a colliding `Transfer` paused in [`TransferStatus::AwaitingConflict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictResolution {
    Overwrite,
    Resume,
    Skip,
    Rename { new_name: String },
}

/// Collision context surfaced to the UI when a `Transfer` targets an existing file under `Ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferConflict {
    pub transfer_id: TransferId,
    pub direction: TransferDirection,
    pub file_name: String,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub source_size: u64,
    pub existing_target_size: u64,
    pub part_file_size: Option<u64>,
    pub suggested_name: String,
}

/// Result of pre-transfer target conflict inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictCheck {
    Proceed {
        overwrite_authorized: bool,
        resume_existing: bool,
    },
    AwaitingConflict(TransferConflict),
    Skip,
    Renamed {
        new_remote_path: String,
        new_local_path: PathBuf,
        new_file_name: String,
    },
}

/// Lifecycle state of an individual [`TransferSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferStatus {
    Queued,
    InProgress,
    Paused,
    AwaitingConflict,
    Completed,
    Skipped,
    Failed { error: String },
    Cancelled,
}

impl TransferStatus {
    /// Returns `true` if this `Transfer` is queued or currently streaming.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::InProgress)
    }

    /// Returns `true` if this `Transfer` is currently streaming chunks.
    pub fn is_in_progress(&self) -> bool {
        matches!(self, Self::InProgress)
    }

    /// Returns `true` when this `Transfer` is paused waiting for user conflict resolution.
    pub fn is_awaiting_conflict(&self) -> bool {
        matches!(self, Self::AwaitingConflict)
    }

    /// Returns `true` if this `Transfer` reached a terminal state (`Completed`, `Skipped`, `Failed`, `Cancelled`).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Skipped | Self::Failed { .. } | Self::Cancelled
        )
    }

    /// Returns a concise human-readable status label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::InProgress => "In Progress",
            Self::Paused => "Paused",
            Self::AwaitingConflict => "Awaiting Conflict",
            Self::Completed => "Completed",
            Self::Skipped => "Skipped",
            Self::Failed { .. } => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// Auto-reconnect lifecycle state for a single [`TransferSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TransferReconnectState {
    #[default]
    None,
    AutoReconnecting {
        attempt: u32,
        reason: String,
    },
    Reconnected,
}

/// Immutable telemetry snapshot of a single `Transfer` item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSnapshot {
    pub id: TransferId,
    pub profile_key: ProfileTransportKey,
    pub direction: TransferDirection,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub file_name: String,
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub throughput_bytes_per_sec: u64,
    pub status: TransferStatus,
    pub transport_mode: Option<SftpTransportMode>,
    pub status_notice: Option<String>,
    pub reconnect_state: TransferReconnectState,
    pub notice: Option<TransferNotice>,
    pub pending_conflict: Option<TransferConflict>,
    pub resume_offset: u64,
    pub using_part_file: bool,
}

impl TransferSnapshot {
    /// Returns normalized progress in `[0.0, 1.0]`.
    pub fn progress_fraction(&self) -> f32 {
        if matches!(
            self.status,
            TransferStatus::Completed | TransferStatus::Skipped
        ) && self.total_bytes == 0
        {
            return 1.0;
        }
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.bytes_transferred as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
    }
}

/// Aggregated queue telemetry consumed by the SFTP Drawer, Status Bar, and Close Guard.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransferQueueSnapshot {
    pub transfers: Vec<TransferSnapshot>,
    pub active_count: usize,
    pub queued_count: usize,
    pub paused_count: usize,
    pub awaiting_conflict_count: usize,
    pub completed_count: usize,
    pub skipped_count: usize,
    pub failed_count: usize,
    pub cancelled_count: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub total_bytes_per_sec: u64,
}

impl TransferQueueSnapshot {
    /// Returns `true` when at least one `Transfer` is queued or actively running.
    pub fn has_incomplete_work(&self) -> bool {
        self.active_count + self.queued_count > 0
    }

    /// Returns all pending target collisions awaiting user confirmation.
    pub fn pending_conflicts(&self) -> Vec<&TransferConflict> {
        self.transfers
            .iter()
            .filter_map(|t| {
                if t.status == TransferStatus::AwaitingConflict {
                    t.pending_conflict.as_ref()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns a derived [`TransferQueueSnapshot`] retaining only transfers matching `predicate`.
    pub fn filtered(&self, predicate: impl Fn(&TransferSnapshot) -> bool) -> Self {
        let transfers: Vec<TransferSnapshot> = self
            .transfers
            .iter()
            .filter(|t| predicate(t))
            .cloned()
            .collect();
        build_snapshot_from_transfers(transfers)
    }
}

/// Target preparation descriptor produced by [`TransferExecutorHook::prepare_upload`].
///
/// Ticket 14 plugs `.termewnal.part` atomic rename, byte-offset resume, and conflict policies
/// into this plan without changing [`TransferQueue`] concurrency or streaming internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTargetPlan {
    pub write_path: String,
    pub final_path: String,
    pub resume_offset: u64,
    pub truncate: bool,
    pub using_part_file: bool,
    pub status_notice: Option<String>,
    pub notice: Option<TransferNotice>,
    pub overwrite_authorized: bool,
}

/// Options supplied to [`TransferExecutorHook::prepare_upload_with_options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadPlanOptions {
    pub part_threshold_bytes: u64,
    pub resume_hint: u64,
    pub force_direct_fallback: bool,
    pub overwrite_authorized: bool,
    pub resume_existing_target: bool,
}

/// Options supplied to [`TransferExecutorHook::prepare_download_with_options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadPlanOptions {
    pub part_threshold_bytes: u64,
    pub resume_hint: u64,
    pub overwrite_authorized: bool,
    pub resume_existing_target: bool,
    pub expected_source_size: Option<u64>,
    pub expected_source_mtime: Option<u32>,
}

/// Target preparation descriptor produced by [`TransferExecutorHook::prepare_download`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTargetPlan {
    pub write_path: PathBuf,
    pub final_path: PathBuf,
    pub total_bytes: u64,
    pub resume_offset: u64,
    pub truncate: bool,
    pub using_part_file: bool,
    pub permissions: Option<u32>,
    pub modified_epoch_secs: Option<u32>,
    pub overwrite_authorized: bool,
    pub status_notice: Option<String>,
    pub notice: Option<TransferNotice>,
}

/// Extension hook allowing `.termewnal.part` atomic transfers, byte-offset resume, and conflict
/// resolution without rewriting [`TransferQueue`].
pub trait TransferExecutorHook: Send + Sync {
    /// Inspects target existence before uploading and applies `policy`.
    #[allow(clippy::too_many_arguments)]
    fn check_upload_conflict<'a>(
        &'a self,
        _sftp: &'a SftpSession,
        _transfer_id: TransferId,
        _local_path: &'a Path,
        _remote_path: &'a str,
        _total_bytes: u64,
        _part_threshold_bytes: u64,
        _policy: TransferConflictPolicy,
    ) -> BoxFuture<'a, Result<ConflictCheck, SshError>> {
        Box::pin(async {
            Ok(ConflictCheck::Proceed {
                overwrite_authorized: false,
                resume_existing: false,
            })
        })
    }

    /// Inspects local target existence before downloading and applies `policy`.
    fn check_download_conflict<'a>(
        &'a self,
        _sftp: &'a SftpSession,
        _transfer_id: TransferId,
        _remote_path: &'a str,
        _local_path: &'a Path,
        _policy: TransferConflictPolicy,
    ) -> BoxFuture<'a, Result<ConflictCheck, SshError>> {
        Box::pin(async {
            Ok(ConflictCheck::Proceed {
                overwrite_authorized: false,
                resume_existing: false,
            })
        })
    }

    /// Resolves the remote target path, truncation mode, and resume offset before uploading.
    fn prepare_upload<'a>(
        &'a self,
        sftp: &'a SftpSession,
        local_path: &'a Path,
        remote_path: &'a str,
        total_bytes: u64,
        resume_hint: u64,
    ) -> BoxFuture<'a, Result<UploadTargetPlan, SshError>>;

    /// Extended upload planner aware of `part_threshold_bytes` and fallback hints.
    fn prepare_upload_with_options<'a>(
        &'a self,
        sftp: &'a SftpSession,
        local_path: &'a Path,
        remote_path: &'a str,
        total_bytes: u64,
        options: UploadPlanOptions,
    ) -> BoxFuture<'a, Result<UploadTargetPlan, SshError>> {
        self.prepare_upload(
            sftp,
            local_path,
            remote_path,
            total_bytes,
            options.resume_hint,
        )
    }

    /// Finalizes an upload after all bytes are flushed and closed (e.g., atomic `.part` rename).
    fn finalize_upload<'a>(
        &'a self,
        sftp: &'a SftpSession,
        plan: &'a UploadTargetPlan,
    ) -> BoxFuture<'a, Result<(), SshError>>;

    /// Queries remote metadata and computes resume offset before downloading.
    fn prepare_download<'a>(
        &'a self,
        sftp: &'a SftpSession,
        remote_path: &'a str,
        local_path: &'a Path,
        resume_hint: u64,
    ) -> BoxFuture<'a, Result<DownloadTargetPlan, SshError>>;

    /// Extended download planner aware of `.termewnal.part` staging and source integrity checks.
    fn prepare_download_with_options<'a>(
        &'a self,
        sftp: &'a SftpSession,
        remote_path: &'a str,
        local_path: &'a Path,
        options: DownloadPlanOptions,
    ) -> BoxFuture<'a, Result<DownloadTargetPlan, SshError>> {
        self.prepare_download(sftp, remote_path, local_path, options.resume_hint)
    }
}

/// Ticket 13 [`TransferExecutorHook`] performing direct pipelined file transfers without `.part`.
#[derive(Debug, Default)]
pub struct DirectTransferHook;

impl TransferExecutorHook for DirectTransferHook {
    fn prepare_upload<'a>(
        &'a self,
        sftp: &'a SftpSession,
        _local_path: &'a Path,
        remote_path: &'a str,
        total_bytes: u64,
        resume_hint: u64,
    ) -> BoxFuture<'a, Result<UploadTargetPlan, SshError>> {
        Box::pin(async move {
            let (resume_offset, truncate) = if resume_hint > 0 {
                match sftp.metadata(remote_path).await {
                    Ok(meta) => {
                        let remote_len = meta.size.unwrap_or(0).min(total_bytes);
                        (remote_len, remote_len == 0)
                    }
                    Err(err) if is_sftp_not_found(&err) => (0, true),
                    Err(err) => return Err(err.into()),
                }
            } else {
                (0, true)
            };
            Ok(UploadTargetPlan {
                write_path: remote_path.to_string(),
                final_path: remote_path.to_string(),
                resume_offset,
                truncate,
                using_part_file: false,
                status_notice: None,
                notice: None,
                overwrite_authorized: false,
            })
        })
    }

    fn finalize_upload<'a>(
        &'a self,
        _sftp: &'a SftpSession,
        _plan: &'a UploadTargetPlan,
    ) -> BoxFuture<'a, Result<(), SshError>> {
        Box::pin(async { Ok(()) })
    }

    fn prepare_download<'a>(
        &'a self,
        sftp: &'a SftpSession,
        remote_path: &'a str,
        local_path: &'a Path,
        resume_hint: u64,
    ) -> BoxFuture<'a, Result<DownloadTargetPlan, SshError>> {
        Box::pin(async move {
            let meta = sftp.metadata(remote_path).await?;
            let total_bytes = meta.size.unwrap_or(0);
            let permissions = meta.permissions.map(|m| m & 0o7777);
            let (resume_offset, truncate) = if resume_hint > 0 {
                match fs::metadata(local_path) {
                    Ok(local_meta) => {
                        let local_len = local_meta.len().min(total_bytes);
                        (local_len, local_len == 0)
                    }
                    Err(_) => (0, true),
                }
            } else {
                (0, true)
            };
            Ok(DownloadTargetPlan {
                write_path: local_path.to_path_buf(),
                final_path: local_path.to_path_buf(),
                total_bytes,
                resume_offset,
                truncate,
                using_part_file: false,
                permissions,
                modified_epoch_secs: meta.mtime,
                overwrite_authorized: false,
                status_notice: None,
                notice: None,
            })
        })
    }
}

/// Default Ticket 14 [`TransferExecutorHook`] implementing `.termewnal.part` atomic uploads,
/// verified remote byte-offset resume, and atomic rename finalization.
#[derive(Debug, Default)]
pub struct ResilientTransferHook;

impl TransferExecutorHook for ResilientTransferHook {
    fn check_upload_conflict<'a>(
        &'a self,
        sftp: &'a SftpSession,
        transfer_id: TransferId,
        local_path: &'a Path,
        remote_path: &'a str,
        total_bytes: u64,
        part_threshold_bytes: u64,
        policy: TransferConflictPolicy,
    ) -> BoxFuture<'a, Result<ConflictCheck, SshError>> {
        Box::pin(async move {
            let existing_meta = match sftp.metadata(remote_path).await {
                Ok(meta) => meta,
                Err(err) if is_sftp_not_found(&err) => {
                    return Ok(ConflictCheck::Proceed {
                        overwrite_authorized: false,
                        resume_existing: false,
                    });
                }
                Err(err) => return Err(err.into()),
            };

            let existing_size = existing_meta.size.unwrap_or(0);
            match policy {
                TransferConflictPolicy::Ask => {
                    let part_file_size = if total_bytes > part_threshold_bytes {
                        sftp.metadata(part_path_for(remote_path).as_str())
                            .await
                            .ok()
                            .and_then(|m| m.size)
                    } else {
                        None
                    };
                    let file_name = remote_leaf_name(remote_path);
                    let suggested_name = disambiguate_filename(&file_name, 1);
                    Ok(ConflictCheck::AwaitingConflict(TransferConflict {
                        transfer_id,
                        direction: TransferDirection::Upload,
                        file_name,
                        local_path: local_path.to_path_buf(),
                        remote_path: remote_path.to_string(),
                        source_size: total_bytes,
                        existing_target_size: existing_size,
                        part_file_size,
                        suggested_name,
                    }))
                }
                TransferConflictPolicy::Overwrite => Ok(ConflictCheck::Proceed {
                    overwrite_authorized: true,
                    resume_existing: false,
                }),
                TransferConflictPolicy::Resume => Ok(ConflictCheck::Proceed {
                    overwrite_authorized: true,
                    resume_existing: true,
                }),
                TransferConflictPolicy::Skip => Ok(ConflictCheck::Skip),
                TransferConflictPolicy::Rename => {
                    let new_remote_path = next_available_remote_path(sftp, remote_path).await?;
                    let new_file_name = remote_leaf_name(&new_remote_path);
                    Ok(ConflictCheck::Renamed {
                        new_remote_path,
                        new_local_path: local_path.to_path_buf(),
                        new_file_name,
                    })
                }
            }
        })
    }

    fn check_download_conflict<'a>(
        &'a self,
        sftp: &'a SftpSession,
        transfer_id: TransferId,
        remote_path: &'a str,
        local_path: &'a Path,
        policy: TransferConflictPolicy,
    ) -> BoxFuture<'a, Result<ConflictCheck, SshError>> {
        Box::pin(async move {
            let local_meta = match tokio::fs::metadata(local_path).await {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ConflictCheck::Proceed {
                        overwrite_authorized: false,
                        resume_existing: false,
                    });
                }
                Err(err) => return Err(err.into()),
            };

            let remote_meta = sftp.metadata(remote_path).await?;
            let source_size = remote_meta.size.unwrap_or(0);
            let existing_target_size = local_meta.len();
            match policy {
                TransferConflictPolicy::Ask => {
                    let file_name = local_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| remote_leaf_name(remote_path));
                    let part_file_size = fs::metadata(local_part_path_for(local_path))
                        .ok()
                        .map(|m| m.len());
                    let suggested_name = disambiguate_filename(&file_name, 1);
                    Ok(ConflictCheck::AwaitingConflict(TransferConflict {
                        transfer_id,
                        direction: TransferDirection::Download,
                        file_name,
                        local_path: local_path.to_path_buf(),
                        remote_path: remote_path.to_string(),
                        source_size,
                        existing_target_size,
                        part_file_size,
                        suggested_name,
                    }))
                }
                TransferConflictPolicy::Overwrite => Ok(ConflictCheck::Proceed {
                    overwrite_authorized: true,
                    resume_existing: false,
                }),
                TransferConflictPolicy::Resume => Ok(ConflictCheck::Proceed {
                    overwrite_authorized: false,
                    resume_existing: true,
                }),
                TransferConflictPolicy::Skip => Ok(ConflictCheck::Skip),
                TransferConflictPolicy::Rename => {
                    let new_local_path = next_available_local_path(local_path);
                    let new_file_name = new_local_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| remote_leaf_name(remote_path));
                    Ok(ConflictCheck::Renamed {
                        new_remote_path: remote_path.to_string(),
                        new_local_path,
                        new_file_name,
                    })
                }
            }
        })
    }

    fn prepare_upload<'a>(
        &'a self,
        sftp: &'a SftpSession,
        local_path: &'a Path,
        remote_path: &'a str,
        total_bytes: u64,
        resume_hint: u64,
    ) -> BoxFuture<'a, Result<UploadTargetPlan, SshError>> {
        self.prepare_upload_with_options(
            sftp,
            local_path,
            remote_path,
            total_bytes,
            UploadPlanOptions {
                part_threshold_bytes: termewnal_core::DEFAULT_PART_THRESHOLD_BYTES,
                resume_hint,
                force_direct_fallback: false,
                overwrite_authorized: false,
                resume_existing_target: false,
            },
        )
    }

    fn prepare_upload_with_options<'a>(
        &'a self,
        sftp: &'a SftpSession,
        _local_path: &'a Path,
        remote_path: &'a str,
        total_bytes: u64,
        options: UploadPlanOptions,
    ) -> BoxFuture<'a, Result<UploadTargetPlan, SshError>> {
        Box::pin(async move {
            if options.resume_existing_target {
                // If an incomplete `.termewnal.part` exists for a >threshold upload, resume it and
                // atomically replace the destination; otherwise resume directly against `remote_path`.
                if !options.force_direct_fallback && total_bytes > options.part_threshold_bytes {
                    let part_path = part_path_for(remote_path);
                    match sftp.metadata(part_path.as_str()).await {
                        Ok(meta) => {
                            let part_len = meta.size.unwrap_or(0);
                            if part_len > 0 && part_len <= total_bytes {
                                return Ok(UploadTargetPlan {
                                    write_path: part_path,
                                    final_path: remote_path.to_string(),
                                    resume_offset: part_len,
                                    truncate: false,
                                    using_part_file: true,
                                    status_notice: None,
                                    notice: None,
                                    overwrite_authorized: true,
                                });
                            }
                        }
                        Err(err) if is_sftp_not_found(&err) => {}
                        Err(err) => return Err(err.into()),
                    }
                }
                let (resume_offset, truncate) = match sftp.metadata(remote_path).await {
                    Ok(meta) => {
                        let remote_len = meta.size.unwrap_or(0).min(total_bytes);
                        (remote_len, remote_len == 0)
                    }
                    Err(err) if is_sftp_not_found(&err) => (0, true),
                    Err(err) => return Err(err.into()),
                };
                return Ok(UploadTargetPlan {
                    write_path: remote_path.to_string(),
                    final_path: remote_path.to_string(),
                    resume_offset,
                    truncate,
                    using_part_file: false,
                    status_notice: None,
                    notice: None,
                    overwrite_authorized: options.overwrite_authorized,
                });
            }

            let use_part =
                !options.force_direct_fallback && total_bytes > options.part_threshold_bytes;
            if use_part {
                let write_path = part_path_for(remote_path);
                let (resume_offset, truncate) = match sftp.metadata(write_path.as_str()).await {
                    Ok(meta) => {
                        let remote_len = meta.size.unwrap_or(0);
                        if remote_len > 0 && remote_len <= total_bytes {
                            (remote_len, false)
                        } else {
                            (0, true)
                        }
                    }
                    Err(err) if is_sftp_not_found(&err) => (0, true),
                    Err(err) => return Err(err.into()),
                };
                return Ok(UploadTargetPlan {
                    write_path,
                    final_path: remote_path.to_string(),
                    resume_offset,
                    truncate,
                    using_part_file: true,
                    status_notice: None,
                    notice: None,
                    overwrite_authorized: options.overwrite_authorized,
                });
            }

            let (resume_offset, truncate) = if options.resume_hint > 0 {
                match sftp.metadata(remote_path).await {
                    Ok(meta) => {
                        let remote_len = meta.size.unwrap_or(0).min(total_bytes);
                        (remote_len, remote_len == 0)
                    }
                    Err(err) if is_sftp_not_found(&err) => (0, true),
                    Err(err) => return Err(err.into()),
                }
            } else {
                (0, true)
            };

            Ok(UploadTargetPlan {
                write_path: remote_path.to_string(),
                final_path: remote_path.to_string(),
                resume_offset,
                truncate,
                using_part_file: false,
                status_notice: None,
                notice: None,
                overwrite_authorized: options.overwrite_authorized,
            })
        })
    }

    fn finalize_upload<'a>(
        &'a self,
        sftp: &'a SftpSession,
        plan: &'a UploadTargetPlan,
    ) -> BoxFuture<'a, Result<(), SshError>> {
        Box::pin(async move {
            if !plan.using_part_file || plan.write_path == plan.final_path {
                return Ok(());
            }
            match sftp
                .posix_rename(plan.write_path.as_str(), plan.final_path.as_str())
                .await
            {
                Ok(true) => Ok(()),
                Ok(false) => {
                    rename_with_optional_overwrite(
                        sftp,
                        &plan.write_path,
                        &plan.final_path,
                        plan.overwrite_authorized,
                    )
                    .await
                }
                Err(SftpClientError::Status(status))
                    if status.status_code == StatusCode::OpUnsupported =>
                {
                    rename_with_optional_overwrite(
                        sftp,
                        &plan.write_path,
                        &plan.final_path,
                        plan.overwrite_authorized,
                    )
                    .await
                }
                Err(err) => Err(err.into()),
            }
        })
    }

    fn prepare_download<'a>(
        &'a self,
        sftp: &'a SftpSession,
        remote_path: &'a str,
        local_path: &'a Path,
        resume_hint: u64,
    ) -> BoxFuture<'a, Result<DownloadTargetPlan, SshError>> {
        DirectTransferHook
            .prepare_download(sftp, remote_path, local_path, resume_hint)
    }

    fn prepare_download_with_options<'a>(
        &'a self,
        sftp: &'a SftpSession,
        remote_path: &'a str,
        local_path: &'a Path,
        options: DownloadPlanOptions,
    ) -> BoxFuture<'a, Result<DownloadTargetPlan, SshError>> {
        Box::pin(async move {
            let meta = sftp.metadata(remote_path).await?;
            let total_bytes = meta.size.unwrap_or(0);
            let permissions = meta.permissions.map(|m| m & 0o7777);
            let modified_epoch_secs = meta.mtime;
            let source_changed = options
                .expected_source_size
                .is_some_and(|prev| prev != total_bytes)
                || (options.expected_source_mtime.is_some()
                    && modified_epoch_secs.is_some()
                    && options.expected_source_mtime != modified_epoch_secs);

            let local_part = local_part_path_for(local_path);
            if options.resume_existing_target {
                if total_bytes > options.part_threshold_bytes
                    && let Ok(part_meta) = fs::metadata(&local_part)
                {
                    let part_len = part_meta.len();
                    if !source_changed && part_len > 0 && part_len <= total_bytes {
                        return Ok(DownloadTargetPlan {
                            write_path: local_part,
                            final_path: local_path.to_path_buf(),
                            total_bytes,
                            resume_offset: part_len,
                            truncate: false,
                            using_part_file: true,
                            permissions,
                            modified_epoch_secs,
                            overwrite_authorized: true,
                            status_notice: None,
                            notice: None,
                        });
                    }
                }
                let (resume_offset, truncate) = match fs::metadata(local_path) {
                    Ok(local_meta) if !source_changed => {
                        let local_len = local_meta.len().min(total_bytes);
                        (local_len, local_len == 0)
                    }
                    _ => (0, true),
                };
                return Ok(DownloadTargetPlan {
                    write_path: local_path.to_path_buf(),
                    final_path: local_path.to_path_buf(),
                    total_bytes,
                    resume_offset,
                    truncate,
                    using_part_file: false,
                    permissions,
                    modified_epoch_secs,
                    overwrite_authorized: options.overwrite_authorized,
                    status_notice: None,
                    notice: None,
                });
            }

            if total_bytes > options.part_threshold_bytes {
                let (resume_offset, truncate) = match fs::metadata(&local_part) {
                    Ok(part_meta) if !source_changed => {
                        let part_len = part_meta.len();
                        if part_len > 0 && part_len <= total_bytes {
                            (part_len, false)
                        } else {
                            (0, true)
                        }
                    }
                    _ => (0, true),
                };
                return Ok(DownloadTargetPlan {
                    write_path: local_part,
                    final_path: local_path.to_path_buf(),
                    total_bytes,
                    resume_offset,
                    truncate,
                    using_part_file: true,
                    permissions,
                    modified_epoch_secs,
                    overwrite_authorized: options.overwrite_authorized,
                    status_notice: None,
                    notice: None,
                });
            }

            let (resume_offset, truncate) = if options.resume_hint > 0 && !source_changed {
                match fs::metadata(local_path) {
                    Ok(local_meta) => {
                        let local_len = local_meta.len().min(total_bytes);
                        (local_len, local_len == 0)
                    }
                    Err(_) => (0, true),
                }
            } else {
                (0, true)
            };
            Ok(DownloadTargetPlan {
                write_path: local_path.to_path_buf(),
                final_path: local_path.to_path_buf(),
                total_bytes,
                resume_offset,
                truncate,
                using_part_file: false,
                permissions,
                modified_epoch_secs,
                overwrite_authorized: options.overwrite_authorized,
                status_notice: None,
                notice: None,
            })
        })
    }
}

async fn rename_with_optional_overwrite(
    sftp: &SftpSession,
    part_path: &str,
    final_path: &str,
    overwrite_authorized: bool,
) -> Result<(), SshError> {
    match sftp.rename(part_path, final_path).await {
        Ok(()) => Ok(()),
        Err(err) if overwrite_authorized && is_sftp_status(&err) => {
            if sftp.metadata(final_path).await.is_ok() {
                sftp.remove_file(final_path).await?;
                sftp.rename(part_path, final_path).await?;
                Ok(())
            } else {
                Err(err.into())
            }
        }
        Err(err) => Err(err.into()),
    }
}

fn is_sftp_not_found(err: &SftpClientError) -> bool {
    matches!(
        err,
        SftpClientError::Status(status) if status.status_code == StatusCode::NoSuchFile
    )
}

fn is_sftp_status(err: &SftpClientError) -> bool {
    matches!(err, SftpClientError::Status(_))
}

/// Configuration for [`TransferQueue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferQueueConfig {
    pub max_concurrent_transfers: usize,
    pub chunk_size: usize,
    pub transfer: TransferConfig,
    pub max_transport_retries: u32,
    pub reconnect_retry_delay: Duration,
}

impl Default for TransferQueueConfig {
    fn default() -> Self {
        Self {
            max_concurrent_transfers: DEFAULT_MAX_CONCURRENT_TRANSFERS,
            chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
            transfer: TransferConfig::default(),
            max_transport_retries: DEFAULT_MAX_TRANSPORT_RETRIES,
            reconnect_retry_delay: DEFAULT_RECONNECT_RETRY_DELAY,
        }
    }
}

impl TransferQueueConfig {
    fn sanitized(self) -> Self {
        Self {
            max_concurrent_transfers: self.max_concurrent_transfers.max(1),
            chunk_size: self
                .chunk_size
                .clamp(MIN_TRANSFER_CHUNK_SIZE, MAX_TRANSFER_CHUNK_SIZE),
            transfer: self.transfer,
            max_transport_retries: self.max_transport_retries,
            reconnect_retry_delay: self.reconnect_retry_delay.max(Duration::from_millis(1)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlSignal {
    Run,
    Pause,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedConflict {
    Overwrite,
    Resume,
    AutoRename,
}

struct TransferEntry {
    snapshot: TransferSnapshot,
    endpoint: Option<SftpEndpoint>,
    generation: u64,
    resume_hint: u64,
    expected_source_size: Option<u64>,
    expected_source_mtime: Option<u32>,
    failed_on_transport_drop: bool,
    was_reconnecting: bool,
    force_direct_fallback: bool,
    resolved_conflict: Option<ResolvedConflict>,
    overwrite_authorized: bool,
    upload_dir_batch: Option<Arc<UploadDirBatch>>,
    download_dir_batch: Option<Arc<DownloadDirBatch>>,
    control_tx: watch::Sender<ControlSignal>,
}

struct TransferQueueInner {
    sftp: SftpManager,
    config: TransferQueueConfig,
    transfer_config: Mutex<TransferConfig>,
    batch_conflict_override: Mutex<Option<TransferConflictPolicy>>,
    hook: Arc<dyn TransferExecutorHook>,
    semaphore: Arc<Semaphore>,
    next_id: Mutex<u64>,
    entries: Mutex<Vec<TransferEntry>>,
    snapshot_tx: watch::Sender<TransferQueueSnapshot>,
    idle_notify: Notify,
}

/// Concurrent SFTP `Transfer` queue with concurrency cap, pipelined chunk streaming,
/// live throughput telemetry, `.termewnal.part` atomic resume, and pause/resume/cancel/retry controls.
#[derive(Clone)]
pub struct TransferQueue {
    inner: Arc<TransferQueueInner>,
}

impl TransferQueue {
    /// Creates a [`TransferQueue`] with default settings (`max_concurrent_transfers = 3`, `256 KiB`).
    pub fn new(sftp: SftpManager) -> Self {
        Self::with_config(sftp, TransferQueueConfig::default())
    }

    /// Creates a [`TransferQueue`] with custom concurrency, chunk, and transfer policy configuration.
    pub fn with_config(sftp: SftpManager, config: TransferQueueConfig) -> Self {
        let config = config.sanitized();
        let transfer_cfg = config.transfer.clone();
        let (snapshot_tx, _) = watch::channel(TransferQueueSnapshot::default());
        Self {
            inner: Arc::new(TransferQueueInner {
                sftp,
                config: config.clone(),
                transfer_config: Mutex::new(transfer_cfg),
                batch_conflict_override: Mutex::new(None),
                hook: Arc::new(ResilientTransferHook),
                semaphore: Arc::new(Semaphore::new(config.max_concurrent_transfers)),
                next_id: Mutex::new(1),
                entries: Mutex::new(Vec::new()),
                snapshot_tx,
                idle_notify: Notify::new(),
            }),
        }
    }

    /// Replaces the [`TransferExecutorHook`] (e.g. [`DirectTransferHook`] or custom hook).
    pub fn with_hook(self, hook: Arc<dyn TransferExecutorHook>) -> Self {
        let transfer_cfg = self
            .inner
            .transfer_config
            .lock()
            .expect("transfer_config mutex poisoned")
            .clone();
        let inner = Arc::new(TransferQueueInner {
            sftp: self.inner.sftp.clone(),
            config: self.inner.config.clone(),
            transfer_config: Mutex::new(transfer_cfg),
            batch_conflict_override: Mutex::new(None),
            hook,
            semaphore: Arc::clone(&self.inner.semaphore),
            next_id: Mutex::new(*self.inner.next_id.lock().expect("id mutex poisoned")),
            entries: Mutex::new(Vec::new()),
            snapshot_tx: watch::channel(TransferQueueSnapshot::default()).0,
            idle_notify: Notify::new(),
        });
        Self { inner }
    }

    /// Clears any transient `Apply to all` conflict policy override so future batches consult
    /// [`TransferConfig::default_conflict_policy`] again.
    pub fn reset_conflict_policy_override(&self) {
        *self
            .inner
            .batch_conflict_override
            .lock()
            .expect("batch_conflict_override mutex poisoned") = None;
    }

    /// Resolves a `Transfer` paused in [`TransferStatus::AwaitingConflict`].
    ///
    /// When `apply_to_all` is `true`, the chosen action becomes the active override for all other
    /// currently awaiting transfers and subsequent collisions in the batch.
    pub fn resolve_conflict(
        &self,
        id: TransferId,
        resolution: ConflictResolution,
        apply_to_all: bool,
    ) -> bool {
        let batch_policy = if apply_to_all {
            Some(match &resolution {
                ConflictResolution::Overwrite => TransferConflictPolicy::Overwrite,
                ConflictResolution::Resume => TransferConflictPolicy::Resume,
                ConflictResolution::Skip => TransferConflictPolicy::Skip,
                ConflictResolution::Rename { .. } => TransferConflictPolicy::Rename,
            })
        } else {
            None
        };

        let to_spawn = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(target_idx) = entries.iter().position(|e| e.snapshot.id == id) else {
                return false;
            };
            if entries[target_idx].snapshot.status != TransferStatus::AwaitingConflict {
                return false;
            }

            if let ConflictResolution::Rename { new_name } = &resolution {
                let sanitized = sanitize_leaf_filename(new_name);
                if sanitized.is_empty() {
                    return false;
                }
                let entry = &mut entries[target_idx];
                match entry.snapshot.direction {
                    TransferDirection::Upload => {
                        let parent = parent_remote_path(&entry.snapshot.remote_path);
                        entry.snapshot.remote_path = join_remote_path(&parent, &sanitized);
                    }
                    TransferDirection::Download => {
                        let new_local = entry
                            .snapshot
                            .local_path
                            .parent()
                            .filter(|p| !p.as_os_str().is_empty())
                            .map(|p| p.join(&sanitized))
                            .unwrap_or_else(|| PathBuf::from(&sanitized));
                        entry.snapshot.local_path = new_local;
                    }
                }
                entry.snapshot.file_name = sanitized;
                entry.resolved_conflict = None;
                entry.overwrite_authorized = false;
            }

            let mut spawn_list = Vec::new();
            let target_entry = &mut entries[target_idx];
            target_entry.snapshot.pending_conflict = None;
            target_entry.snapshot.throughput_bytes_per_sec = 0;
            match &resolution {
                ConflictResolution::Skip => {
                    target_entry.snapshot.status = TransferStatus::Skipped;
                }
                ConflictResolution::Overwrite => {
                    target_entry.resolved_conflict = Some(ResolvedConflict::Overwrite);
                    target_entry.overwrite_authorized = true;
                    target_entry.resume_hint = 0;
                    target_entry.generation += 1;
                    target_entry.snapshot.status = TransferStatus::Queued;
                    let _ = target_entry.control_tx.send(ControlSignal::Run);
                    spawn_list.push((target_entry.snapshot.id, target_entry.generation));
                }
                ConflictResolution::Resume => {
                    target_entry.resolved_conflict = Some(ResolvedConflict::Resume);
                    target_entry.overwrite_authorized = true;
                    target_entry.resume_hint = 0;
                    target_entry.generation += 1;
                    target_entry.snapshot.status = TransferStatus::Queued;
                    let _ = target_entry.control_tx.send(ControlSignal::Run);
                    spawn_list.push((target_entry.snapshot.id, target_entry.generation));
                }
                ConflictResolution::Rename { .. } => {
                    target_entry.resume_hint = 0;
                    target_entry.generation += 1;
                    target_entry.snapshot.status = TransferStatus::Queued;
                    let _ = target_entry.control_tx.send(ControlSignal::Run);
                    spawn_list.push((target_entry.snapshot.id, target_entry.generation));
                }
            }

            if let Some(policy) = batch_policy {
                *self
                    .inner
                    .batch_conflict_override
                    .lock()
                    .expect("batch_conflict_override mutex poisoned") = Some(policy);

                for entry in entries.iter_mut() {
                    if entry.snapshot.id == id
                        || entry.snapshot.status != TransferStatus::AwaitingConflict
                    {
                        continue;
                    }
                    entry.snapshot.pending_conflict = None;
                    entry.snapshot.throughput_bytes_per_sec = 0;
                    match policy {
                        TransferConflictPolicy::Skip => {
                            entry.snapshot.status = TransferStatus::Skipped;
                        }
                        TransferConflictPolicy::Overwrite => {
                            entry.resolved_conflict = Some(ResolvedConflict::Overwrite);
                            entry.overwrite_authorized = true;
                            entry.resume_hint = 0;
                            entry.generation += 1;
                            entry.snapshot.status = TransferStatus::Queued;
                            let _ = entry.control_tx.send(ControlSignal::Run);
                            spawn_list.push((entry.snapshot.id, entry.generation));
                        }
                        TransferConflictPolicy::Resume => {
                            entry.resolved_conflict = Some(ResolvedConflict::Resume);
                            entry.overwrite_authorized = true;
                            entry.resume_hint = 0;
                            entry.generation += 1;
                            entry.snapshot.status = TransferStatus::Queued;
                            let _ = entry.control_tx.send(ControlSignal::Run);
                            spawn_list.push((entry.snapshot.id, entry.generation));
                        }
                        TransferConflictPolicy::Rename => {
                            entry.resolved_conflict = Some(ResolvedConflict::AutoRename);
                            entry.overwrite_authorized = false;
                            entry.resume_hint = 0;
                            entry.generation += 1;
                            entry.snapshot.status = TransferStatus::Queued;
                            let _ = entry.control_tx.send(ControlSignal::Run);
                            spawn_list.push((entry.snapshot.id, entry.generation));
                        }
                        TransferConflictPolicy::Ask => {}
                    }
                }
            }

            spawn_list
        };

        let snap = self.snapshot();
        self.publish_snapshot(snap);
        for (spawn_id, generation) in to_spawn {
            self.spawn_worker_if_runtime(spawn_id, generation);
        }
        true
    }

    /// Updates [`TransferConfig`] at runtime (e.g. after `config.toml` hot-reload).
    pub fn set_transfer_config(&self, config: TransferConfig) {
        *self
            .inner
            .transfer_config
            .lock()
            .expect("transfer_config mutex poisoned") = config;
    }

    /// Returns the active [`TransferConfig`] snapshot.
    pub fn transfer_config(&self) -> TransferConfig {
        self.inner
            .transfer_config
            .lock()
            .expect("transfer_config mutex poisoned")
            .clone()
    }

    /// Returns the underlying [`SftpManager`].
    pub fn sftp_manager(&self) -> &SftpManager {
        &self.inner.sftp
    }

    /// Returns a stable process-unique identifier for this [`TransferQueue`] instance.
    pub fn instance_id(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    /// Subscribes to live [`TransferQueueSnapshot`] updates via a `watch` receiver.
    pub fn subscribe(&self) -> watch::Receiver<TransferQueueSnapshot> {
        self.inner.snapshot_tx.subscribe()
    }

    /// Captures the current [`TransferQueueSnapshot`].
    pub fn snapshot(&self) -> TransferQueueSnapshot {
        let entries = self.inner.entries.lock().expect("entries mutex poisoned");
        build_queue_snapshot(&entries)
    }

    /// Returns a snapshot of a single `Transfer` by `id`, if present.
    pub fn get(&self, id: TransferId) -> Option<TransferSnapshot> {
        let entries = self.inner.entries.lock().expect("entries mutex poisoned");
        entries
            .iter()
            .find(|e| e.snapshot.id == id)
            .map(|e| e.snapshot.clone())
    }

    /// Enqueues a file upload `Transfer` from `local_path` to `remote_path`.
    pub fn enqueue_upload(
        &self,
        endpoint: SftpEndpoint,
        local_path: PathBuf,
        remote_path: String,
    ) -> TransferId {
        self.enqueue_upload_with_conflict(endpoint, local_path, remote_path, None, false)
    }

    /// Enqueues a file upload `Transfer` from `local_path` to `remote_path` that
    /// overwrites any existing remote target without pausing on a conflict prompt.
    pub fn enqueue_upload_overwrite(
        &self,
        endpoint: SftpEndpoint,
        local_path: PathBuf,
        remote_path: String,
    ) -> TransferId {
        self.enqueue_upload_with_conflict(
            endpoint,
            local_path,
            remote_path,
            Some(ResolvedConflict::Overwrite),
            true,
        )
    }

    fn enqueue_upload_with_conflict(
        &self,
        endpoint: SftpEndpoint,
        local_path: PathBuf,
        remote_path: String,
        resolved_conflict: Option<ResolvedConflict>,
        overwrite_authorized: bool,
    ) -> TransferId {
        let total_bytes = fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
        let file_name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| remote_path.clone());
        self.enqueue_entry_with_conflict(
            Some(endpoint),
            TransferDirection::Upload,
            local_path,
            remote_path,
            file_name,
            total_bytes,
            TransferStatus::Queued,
            resolved_conflict,
            overwrite_authorized,
        )
    }

    /// Enqueues a file download `Transfer` from `remote_path` to `local_path`.
    pub fn enqueue_download(
        &self,
        endpoint: SftpEndpoint,
        remote_path: String,
        local_path: PathBuf,
    ) -> TransferId {
        let file_name = Path::new(&remote_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| remote_path.clone());
        self.enqueue_entry(
            Some(endpoint),
            TransferDirection::Download,
            local_path,
            remote_path,
            file_name,
            0,
            TransferStatus::Queued,
        )
    }

    /// Enqueues a local file or recursive directory upload `Transfer` under `remote_path`,
    /// preserving POSIX permissions and safely skipping symlinks / symlink loops per [`TransferConfig`].
    pub fn enqueue_upload_path(
        &self,
        endpoint: SftpEndpoint,
        local_path: PathBuf,
        remote_path: String,
    ) -> Result<DirectoryTransferSummary, SshError> {
        let follow_symlinks = self.transfer_config().follow_symlinks;
        let lstat = fs::symlink_metadata(&local_path)?;
        if lstat.file_type().is_symlink() && !follow_symlinks {
            return Ok(DirectoryTransferSummary {
                skipped_symlinks: vec![local_path.to_string_lossy().into_owned()],
                ..Default::default()
            });
        }
        let meta = fs::metadata(&local_path)?;
        if !meta.is_dir() {
            let id = self.enqueue_upload(endpoint, local_path, remote_path);
            return Ok(DirectoryTransferSummary {
                transfer_ids: vec![id],
                ..Default::default()
            });
        }
        let plan = collect_local_dir_plan(&local_path, &remote_path, follow_symlinks)?;
        let created_dir_count = plan.dirs.len();
        let skipped_symlinks = plan.skipped_symlinks;
        let skipped_symlink_loops = plan.skipped_symlink_loops;

        let ids: Vec<TransferId> = (0..plan.files.len()).map(|_| self.alloc_id()).collect();
        let batch = Arc::new(UploadDirBatch::new(plan.dirs, &ids));

        if plan.files.is_empty() {
            if created_dir_count > 0
                && let Ok(handle) = tokio::runtime::Handle::try_current()
            {
                let sftp_mgr = self.inner.sftp.clone();
                let ep = endpoint.clone();
                let batch_clone = Arc::clone(&batch);
                handle.spawn(async move {
                    if let Ok(acquired) = sftp_mgr.acquire_transfer_session(&ep).await
                        && batch_clone.ensure_dirs_created(&acquired.session).await.is_ok()
                    {
                        let _ = batch_clone
                            .apply_dir_permissions_bottom_up(&acquired.session)
                            .await;
                    }
                });
            }
            return Ok(DirectoryTransferSummary {
                transfer_ids: Vec::new(),
                created_dir_count,
                skipped_symlinks,
                skipped_symlink_loops,
                permission_notices: Vec::new(),
            });
        }

        let mut new_entries = Vec::with_capacity(plan.files.len());
        let mut spawn_list = Vec::with_capacity(plan.files.len());
        let notice = (!skipped_symlinks.is_empty()).then(|| TransferNotice::SymlinkSkipped {
            path: skipped_symlinks[0].clone(),
            is_loop: !skipped_symlink_loops.is_empty(),
        });
        for (idx, file_spec) in plan.files.into_iter().enumerate() {
            let id = ids[idx];
            let file_name = file_spec
                .local_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_spec.remote_path.clone());
            let (control_tx, _) = watch::channel(ControlSignal::Run);
            let entry = TransferEntry {
                snapshot: TransferSnapshot {
                    id,
                    profile_key: endpoint.key.clone(),
                    direction: TransferDirection::Upload,
                    local_path: file_spec.local_path,
                    remote_path: file_spec.remote_path,
                    file_name,
                    bytes_transferred: 0,
                    total_bytes: file_spec.total_bytes,
                    throughput_bytes_per_sec: 0,
                    status: TransferStatus::Queued,
                    transport_mode: None,
                    status_notice: notice.as_ref().map(TransferNotice::summary),
                    reconnect_state: TransferReconnectState::None,
                    notice: notice.clone(),
                    pending_conflict: None,
                    resume_offset: 0,
                    using_part_file: false,
                },
                endpoint: Some(endpoint.clone()),
                generation: 1,
                resume_hint: 0,
                failed_on_transport_drop: false,
                was_reconnecting: false,
                expected_source_size: None,
                expected_source_mtime: None,
                force_direct_fallback: false,
                resolved_conflict: None,
                overwrite_authorized: false,
                upload_dir_batch: Some(Arc::clone(&batch)),
                download_dir_batch: None,
                control_tx,
            };
            new_entries.push(entry);
            spawn_list.push((id, 1));
        }

        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            entries.extend(new_entries);
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
        for (id, generation) in spawn_list {
            self.spawn_worker_if_runtime(id, generation);
        }

        Ok(DirectoryTransferSummary {
            transfer_ids: ids,
            created_dir_count,
            skipped_symlinks,
            skipped_symlink_loops,
            permission_notices: Vec::new(),
        })
    }

    /// Recursively traverses `local_dir` and enqueues all non-skipped files for upload to `remote_dir`,
    /// creating remote directories and preserving POSIX permissions (`chmod`) within the concurrency cap.
    pub async fn enqueue_upload_dir(
        &self,
        endpoint: SftpEndpoint,
        local_dir: PathBuf,
        remote_dir: String,
    ) -> Result<DirectoryTransferSummary, SshError> {
        let mut summary =
            self.enqueue_upload_path(endpoint.clone(), local_dir.clone(), remote_dir.clone())?;
        if summary.transfer_ids.is_empty() && summary.created_dir_count > 0 {
            let follow = self.transfer_config().follow_symlinks;
            let plan = collect_local_dir_plan(&local_dir, &remote_dir, follow)?;
            let batch = UploadDirBatch::new(plan.dirs, &[]);
            let acquired = self.inner.sftp.acquire_transfer_session(&endpoint).await?;
            batch.ensure_dirs_created(&acquired.session).await?;
            summary.permission_notices = batch
                .apply_dir_permissions_bottom_up(&acquired.session)
                .await;
        }
        Ok(summary)
    }

    /// Recursively traverses `remote_dir` via SFTP `lstat`/`read_dir` and enqueues all non-skipped
    /// files for download into `local_dir`, preserving POSIX permissions and skipping symlink loops.
    pub async fn enqueue_download_dir(
        &self,
        endpoint: SftpEndpoint,
        remote_dir: String,
        local_dir: PathBuf,
    ) -> Result<DirectoryTransferSummary, SshError> {
        let acquired = self.inner.sftp.acquire_transfer_session(&endpoint).await?;
        let follow_symlinks = self.transfer_config().follow_symlinks;
        let plan =
            collect_remote_dir_plan(&acquired.session, &remote_dir, &local_dir, follow_symlinks)
                .await?;
        let created_dir_count = plan.dirs.len();
        let skipped_symlinks = plan.skipped_symlinks;
        let skipped_symlink_loops = plan.skipped_symlink_loops;

        for (dir_path, _) in &plan.dirs {
            tokio::fs::create_dir_all(dir_path).await?;
        }

        let ids: Vec<TransferId> = (0..plan.files.len()).map(|_| self.alloc_id()).collect();
        let batch = Arc::new(DownloadDirBatch::new(plan.dirs, &ids));

        if plan.files.is_empty() {
            let permission_notices = batch.apply_dir_permissions_bottom_up().await;
            return Ok(DirectoryTransferSummary {
                transfer_ids: Vec::new(),
                created_dir_count,
                skipped_symlinks,
                skipped_symlink_loops,
                permission_notices,
            });
        }

        let notice = (!skipped_symlinks.is_empty()).then(|| TransferNotice::SymlinkSkipped {
            path: skipped_symlinks[0].clone(),
            is_loop: !skipped_symlink_loops.is_empty(),
        });
        let mut new_entries = Vec::with_capacity(plan.files.len());
        let mut spawn_list = Vec::with_capacity(plan.files.len());
        for (idx, file_spec) in plan.files.into_iter().enumerate() {
            let id = ids[idx];
            let file_name = file_spec
                .local_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_spec.remote_path.clone());
            let (control_tx, _) = watch::channel(ControlSignal::Run);
            let entry = TransferEntry {
                snapshot: TransferSnapshot {
                    id,
                    profile_key: endpoint.key.clone(),
                    direction: TransferDirection::Download,
                    local_path: file_spec.local_path,
                    remote_path: file_spec.remote_path,
                    file_name,
                    bytes_transferred: 0,
                    total_bytes: file_spec.total_bytes,
                    throughput_bytes_per_sec: 0,
                    status: TransferStatus::Queued,
                    transport_mode: None,
                    status_notice: notice.as_ref().map(TransferNotice::summary),
                    reconnect_state: TransferReconnectState::None,
                    notice: notice.clone(),
                    pending_conflict: None,
                    resume_offset: 0,
                    using_part_file: false,
                },
                endpoint: Some(endpoint.clone()),
                generation: 1,
                resume_hint: 0,
                failed_on_transport_drop: false,
                was_reconnecting: false,
                expected_source_size: None,
                expected_source_mtime: None,
                force_direct_fallback: false,
                resolved_conflict: None,
                overwrite_authorized: false,
                upload_dir_batch: None,
                download_dir_batch: Some(Arc::clone(&batch)),
                control_tx,
            };
            new_entries.push(entry);
            spawn_list.push((id, 1));
        }

        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            entries.extend(new_entries);
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
        for (id, generation) in spawn_list {
            self.spawn_worker_if_runtime(id, generation);
        }

        Ok(DirectoryTransferSummary {
            transfer_ids: ids,
            created_dir_count,
            skipped_symlinks,
            skipped_symlink_loops,
            permission_notices: Vec::new(),
        })
    }

    /// Enqueues a local UI-tracked `Transfer` entry when no live [`SftpEndpoint`] is connected yet.
    pub fn enqueue_disconnected_placeholder(
        &self,
        profile_key: ProfileTransportKey,
        direction: TransferDirection,
        local_path: PathBuf,
        remote_path: String,
        error: String,
    ) -> TransferId {
        let total_bytes = if direction == TransferDirection::Upload {
            fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let file_name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| remote_path.clone());
        let id = self.alloc_id();
        let (control_tx, _) = watch::channel(ControlSignal::Run);
        let entry = TransferEntry {
            snapshot: TransferSnapshot {
                id,
                profile_key,
                direction,
                local_path,
                remote_path,
                file_name,
                bytes_transferred: 0,
                total_bytes,
                throughput_bytes_per_sec: 0,
                status: TransferStatus::Failed { error },
                transport_mode: None,
                status_notice: None,
                reconnect_state: TransferReconnectState::None,
                notice: None,
                pending_conflict: None,
                resume_offset: 0,
                using_part_file: false,
            },
            endpoint: None,
            generation: 1,
            resume_hint: 0,
            failed_on_transport_drop: false,
            was_reconnecting: false,
            expected_source_size: None,
            expected_source_mtime: None,
            force_direct_fallback: false,
            resolved_conflict: None,
            overwrite_authorized: false,
            upload_dir_batch: None,
            download_dir_batch: None,
            control_tx,
        };
        self.push_entry(entry);
        id
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_entry(
        &self,
        endpoint: Option<SftpEndpoint>,
        direction: TransferDirection,
        local_path: PathBuf,
        remote_path: String,
        file_name: String,
        total_bytes: u64,
        status: TransferStatus,
    ) -> TransferId {
        self.enqueue_entry_with_conflict(
            endpoint,
            direction,
            local_path,
            remote_path,
            file_name,
            total_bytes,
            status,
            None,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_entry_with_conflict(
        &self,
        endpoint: Option<SftpEndpoint>,
        direction: TransferDirection,
        local_path: PathBuf,
        remote_path: String,
        file_name: String,
        total_bytes: u64,
        status: TransferStatus,
        resolved_conflict: Option<ResolvedConflict>,
        overwrite_authorized: bool,
    ) -> TransferId {
        let id = self.alloc_id();
        let (control_tx, _) = watch::channel(ControlSignal::Run);
        let profile_key = endpoint
            .as_ref()
            .map(|e| e.key.clone())
            .unwrap_or_else(|| ProfileTransportKey::new(None, "", 0, ""));
        let entry = TransferEntry {
            snapshot: TransferSnapshot {
                id,
                profile_key,
                direction,
                local_path,
                remote_path,
                file_name,
                bytes_transferred: 0,
                total_bytes,
                throughput_bytes_per_sec: 0,
                status,
                transport_mode: None,
                status_notice: None,
                reconnect_state: TransferReconnectState::None,
                notice: None,
                pending_conflict: None,
                resume_offset: 0,
                using_part_file: false,
            },
            endpoint,
            generation: 1,
            resume_hint: 0,
            failed_on_transport_drop: false,
            was_reconnecting: false,
            expected_source_size: None,
            expected_source_mtime: None,
            force_direct_fallback: false,
            resolved_conflict,
            overwrite_authorized,
            upload_dir_batch: None,
            download_dir_batch: None,
            control_tx,
        };
        self.push_entry(entry);
        self.spawn_worker_if_runtime(id, 1);
        id
    }

    fn alloc_id(&self) -> TransferId {
        let mut next = self.inner.next_id.lock().expect("id mutex poisoned");
        let id = TransferId(*next);
        *next += 1;
        id
    }

    fn push_entry(&self, entry: TransferEntry) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            entries.push(entry);
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    /// Pauses a `Queued` or `InProgress` `Transfer`, freeing its concurrency slot.
    pub fn pause(&self, id: TransferId) -> bool {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries.iter_mut().find(|e| e.snapshot.id == id) else {
                return false;
            };
            if !entry.snapshot.status.is_active() {
                return false;
            }
            let _ = entry.control_tx.send(ControlSignal::Pause);
            entry.snapshot.status = TransferStatus::Paused;
            entry.snapshot.throughput_bytes_per_sec = 0;
            entry.resume_hint = entry.snapshot.bytes_transferred;
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
        true
    }

    /// Resumes a `Paused` `Transfer` from its verified transferred offset.
    pub fn resume(&self, id: TransferId) -> bool {
        let spawn_gen = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            entries.iter_mut().find(|e| e.snapshot.id == id).and_then(|entry| {
                if entry.snapshot.status != TransferStatus::Paused {
                    return None;
                }
                entry.generation += 1;
                entry.resume_hint = entry.snapshot.bytes_transferred;
                entry.snapshot.status = TransferStatus::Queued;
                entry.snapshot.throughput_bytes_per_sec = 0;
                let _ = entry.control_tx.send(ControlSignal::Run);
                let generation = entry.generation;
                Some(generation)
            })
        };
        let Some(generation) = spawn_gen else {
            return false;
        };
        let snap = self.snapshot();
        self.publish_snapshot(snap);
        self.spawn_worker_if_runtime(id, generation);
        true
    }

    /// Re-queues `Failed` transfers for `key` that failed due to a `Transport` drop once the
    /// `Transport` reconnects, preserving byte-offset resume hints.
    pub fn resume_reconnected(&self, key: &ProfileTransportKey) -> usize {
        let to_spawn: Vec<(TransferId, u64)> = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let mut out = Vec::new();
            for entry in entries.iter_mut() {
                if &entry.snapshot.profile_key == key
                    && entry.failed_on_transport_drop
                    && matches!(entry.snapshot.status, TransferStatus::Failed { .. })
                    && entry.endpoint.is_some()
                {
                    entry.generation += 1;
                    entry.resume_hint = entry.snapshot.bytes_transferred.max(1);
                    entry.failed_on_transport_drop = false;
                    entry.was_reconnecting = true;
                    entry.snapshot.throughput_bytes_per_sec = 0;
                    entry.snapshot.status = TransferStatus::Queued;
                    entry.snapshot.reconnect_state = TransferReconnectState::AutoReconnecting {
                        attempt: 1,
                        reason: "Session reconnected; resuming transfer...".to_string(),
                    };
                    entry.snapshot.status_notice = Some("Auto-reconnecting transfer...".to_string());
                    let _ = entry.control_tx.send(ControlSignal::Run);
                    out.push((entry.snapshot.id, entry.generation));
                }
            }
            out
        };
        if to_spawn.is_empty() {
            return 0;
        }
        let snap = self.snapshot();
        self.publish_snapshot(snap);
        for (id, generation) in &to_spawn {
            self.spawn_worker_if_runtime(*id, *generation);
        }
        to_spawn.len()
    }

    /// Returns `true` when at least one `Failed` transfer is eligible for automatic recovery
    /// because it terminated on a transient transport drop.
    pub fn has_reconnectable_failures(&self) -> bool {
        let entries = self.inner.entries.lock().expect("entries mutex poisoned");
        entries.iter().any(|e| {
            e.failed_on_transport_drop
                && matches!(e.snapshot.status, TransferStatus::Failed { .. })
                && e.endpoint.is_some()
        })
    }

    /// Cancels a `Queued`, `InProgress`, or `Paused` `Transfer`.
    pub fn cancel(&self, id: TransferId) -> bool {
        let mut part_to_remove: Option<PathBuf> = None;
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries.iter_mut().find(|e| e.snapshot.id == id) else {
                return false;
            };
            if entry.snapshot.status.is_terminal() {
                return false;
            }
            if entry.snapshot.direction == TransferDirection::Download && entry.snapshot.using_part_file
            {
                part_to_remove = Some(local_part_path_for(&entry.snapshot.local_path));
            }
            let _ = entry.control_tx.send(ControlSignal::Cancel);
            entry.snapshot.status = TransferStatus::Cancelled;
            entry.snapshot.pending_conflict = None;
            entry.snapshot.throughput_bytes_per_sec = 0;
            build_queue_snapshot(&entries)
        };
        if let Some(part) = part_to_remove {
            let _ = fs::remove_file(part);
        }
        self.publish_snapshot(snap);
        true
    }

    /// Retries a `Failed` or `Cancelled` `Transfer` (and resumes if `Paused`).
    pub fn retry(&self, id: TransferId) -> bool {
        let spawn_gen = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            entries.iter_mut().find(|e| e.snapshot.id == id).and_then(|entry| {
                match entry.snapshot.status {
                    TransferStatus::Paused => {
                        entry.generation += 1;
                        entry.resume_hint = entry.snapshot.bytes_transferred;
                        entry.snapshot.status = TransferStatus::Queued;
                        entry.snapshot.throughput_bytes_per_sec = 0;
                        let _ = entry.control_tx.send(ControlSignal::Run);
                        Some(entry.generation)
                    }
                    TransferStatus::Failed { .. } => {
                        if entry.endpoint.is_none() {
                            return None;
                        }
                        entry.generation += 1;
                        let preserve_resume =
                            entry.snapshot.using_part_file || entry.failed_on_transport_drop;
                        if preserve_resume && entry.snapshot.bytes_transferred > 0 {
                            entry.resume_hint = entry.snapshot.bytes_transferred;
                        } else {
                            entry.resume_hint = 0;
                            entry.snapshot.bytes_transferred = 0;
                            entry.snapshot.resume_offset = 0;
                            entry.expected_source_size = None;
                            entry.expected_source_mtime = None;
                        }
                        entry.failed_on_transport_drop = false;
                        entry.was_reconnecting = false;
                        entry.snapshot.reconnect_state = TransferReconnectState::None;
                        entry.snapshot.status_notice = None;
                        entry.snapshot.throughput_bytes_per_sec = 0;
                        entry.snapshot.status = TransferStatus::Queued;
                        let _ = entry.control_tx.send(ControlSignal::Run);
                        Some(entry.generation)
                    }
                    TransferStatus::Cancelled => {
                        if entry.endpoint.is_none() {
                            return None;
                        }
                        entry.generation += 1;
                        entry.resume_hint = 0;
                        entry.snapshot.bytes_transferred = 0;
                        entry.snapshot.resume_offset = 0;
                        entry.failed_on_transport_drop = false;
                        entry.was_reconnecting = false;
                        entry.expected_source_size = None;
                        entry.expected_source_mtime = None;
                        entry.snapshot.reconnect_state = TransferReconnectState::None;
                        entry.snapshot.status_notice = None;
                        entry.resolved_conflict = None;
                        entry.overwrite_authorized = false;
                        entry.snapshot.pending_conflict = None;
                        entry.snapshot.throughput_bytes_per_sec = 0;
                        entry.snapshot.status = TransferStatus::Queued;
                        let _ = entry.control_tx.send(ControlSignal::Run);
                        Some(entry.generation)
                    }
                    _ => None,
                }
            })
        };
        let Some(generation) = spawn_gen else {
            return false;
        };
        let snap = self.snapshot();
        self.publish_snapshot(snap);
        self.spawn_worker_if_runtime(id, generation);
        true
    }

    /// Cancels all non-terminal `Transfer`s (invoked on Stop & Quit).
    pub fn cancel_all(&self) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            for entry in entries.iter_mut() {
                if !entry.snapshot.status.is_terminal() {
                    let _ = entry.control_tx.send(ControlSignal::Cancel);
                    entry.snapshot.status = TransferStatus::Cancelled;
                    entry.snapshot.pending_conflict = None;
                    entry.snapshot.throughput_bytes_per_sec = 0;
                }
            }
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    /// Removes `Completed`, `Skipped`, and `Cancelled` entries from the queue snapshot.
    pub fn clear_finished(&self) {
        self.clear_finished_matching(|_| true);
    }

    /// Removes `Completed`, `Skipped`, and `Cancelled` entries matching `predicate`.
    pub fn clear_finished_matching(&self, predicate: impl Fn(&TransferSnapshot) -> bool) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            entries.retain(|e| {
                let is_finished = matches!(
                    e.snapshot.status,
                    TransferStatus::Completed | TransferStatus::Skipped | TransferStatus::Cancelled
                );
                !(is_finished && predicate(&e.snapshot))
            });
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    /// Waits asynchronously until no `Queued` or `InProgress` `Transfer`s remain, or `timeout` elapses.
    pub async fn wait_until_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if !self.snapshot().has_incomplete_work() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, self.inner.idle_notify.notified())
                .await
                .is_err()
            {
                return !self.snapshot().has_incomplete_work();
            }
        }
    }

    fn publish_snapshot(&self, snap: TransferQueueSnapshot) {
        let idle = !snap.has_incomplete_work();
        let _ = self.inner.snapshot_tx.send(snap);
        if idle {
            self.inner.idle_notify.notify_waiters();
        }
    }

    fn spawn_worker_if_runtime(&self, id: TransferId, generation: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let queue = self.clone();
        handle.spawn(async move {
            queue.run_transfer_worker(id, generation).await;
        });
    }

    async fn run_transfer_worker(&self, id: TransferId, generation: u64) {
        let (
            endpoint,
            direction,
            local_path,
            remote_path,
            resume_hint,
            upload_batch,
            download_batch,
            mut control_rx,
        ) = {
            let entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            let Some(endpoint) = entry.endpoint.clone() else {
                return;
            };
            (
                endpoint,
                entry.snapshot.direction,
                entry.snapshot.local_path.clone(),
                entry.snapshot.remote_path.clone(),
                entry.resume_hint,
                entry.upload_dir_batch.clone(),
                entry.download_dir_batch.clone(),
                entry.control_tx.subscribe(),
            )
        };

        // Acquire semaphore permit while respecting pause/cancel before starting I/O.
        let _permit = loop {
            if let Some(signal) = current_interrupt(&control_rx) {
                self.apply_interrupt(id, generation, signal);
                return;
            }
            tokio::select! {
                biased;
                _ = control_rx.changed() => {
                    if let Some(signal) = current_interrupt(&control_rx) {
                        self.apply_interrupt(id, generation, signal);
                        return;
                    }
                }
                permit = self.inner.semaphore.clone().acquire_owned() => {
                    match permit {
                        Ok(p) => break p,
                        Err(_) => return,
                    }
                }
            }
        };

        if let Some(signal) = current_interrupt(&control_rx) {
            self.apply_interrupt(id, generation, signal);
            return;
        }

        if !self.mark_in_progress(id, generation) {
            return;
        }

        let reconnect_notifier = self.inner.sftp.pool().reconnect_notifier();
        let mut transport_retries = 0u32;
        let mut current_resume_hint = resume_hint;

        loop {
            if let Some(signal) = current_interrupt(&control_rx) {
                self.apply_interrupt(id, generation, signal);
                return;
            }

            let acquired = match self.inner.sftp.acquire_transfer_session(&endpoint).await {
                Ok(session) => session,
                Err(err) => {
                    let drop_err = err.is_connection_error();
                    if drop_err
                        && transport_retries < self.inner.config.max_transport_retries
                    {
                        transport_retries += 1;
                        current_resume_hint =
                            self.current_bytes_transferred(id, generation).max(1);
                        self.set_reconnecting_notice(
                            id,
                            generation,
                            current_resume_hint,
                            transport_retries,
                            &err.to_string(),
                        );
                        if self
                            .wait_reconnect_backoff(
                                transport_retries,
                                &mut control_rx,
                                &reconnect_notifier,
                            )
                            .await
                            .is_some()
                        {
                            if let Some(signal) = current_interrupt(&control_rx) {
                                self.apply_interrupt(id, generation, signal);
                            }
                            return;
                        }
                        continue;
                    }
                    self.mark_failed_with_drop(id, generation, err.to_string(), drop_err);
                    return;
                }
            };

            self.update_transport_info(id, generation, acquired.mode, acquired.notice.clone());

            let outcome = match direction {
                TransferDirection::Upload => {
                    if let Some(ref batch) = upload_batch
                        && let Err(err) = batch.ensure_dirs_created(&acquired.session).await
                    {
                        Err(err)
                    } else {
                        self.stream_upload(
                            id,
                            generation,
                            &acquired.session,
                            &local_path,
                            &remote_path,
                            current_resume_hint,
                            &mut control_rx,
                        )
                        .await
                    }
                }
                TransferDirection::Download => {
                    self.stream_download(
                        id,
                        generation,
                        &acquired.session,
                        &remote_path,
                        &local_path,
                        current_resume_hint,
                        &mut control_rx,
                    )
                    .await
                }
            };

            match outcome {
                Ok(TransferOutcome::Completed { bytes, rate }) => {
                    self.finalize_dir_batches(
                        id,
                        generation,
                        &acquired.session,
                        upload_batch.as_ref(),
                        download_batch.as_ref(),
                    )
                    .await;
                    self.mark_completed(id, generation, bytes, rate);
                    return;
                }
                Ok(TransferOutcome::AwaitingConflict(conflict)) => {
                    self.mark_awaiting_conflict(id, generation, conflict);
                    return;
                }
                Ok(TransferOutcome::Skipped) => {
                    self.finalize_dir_batches(
                        id,
                        generation,
                        &acquired.session,
                        upload_batch.as_ref(),
                        download_batch.as_ref(),
                    )
                    .await;
                    self.mark_skipped(id, generation);
                    return;
                }
                Ok(TransferOutcome::Interrupted(signal)) => {
                    if signal == ControlSignal::Cancel {
                        self.finalize_dir_batches(
                            id,
                            generation,
                            &acquired.session,
                            upload_batch.as_ref(),
                            download_batch.as_ref(),
                        )
                        .await;
                    }
                    self.apply_interrupt(id, generation, signal);
                    return;
                }
                Err(err) => {
                    if let Some(signal) = current_interrupt(&control_rx) {
                        self.apply_interrupt(id, generation, signal);
                        return;
                    }
                    let drop_err = err.is_connection_error();
                    if drop_err {
                        self.inner
                            .sftp
                            .pool()
                            .invalidate_session(&endpoint.key, &acquired.session);
                    }
                    if drop_err && transport_retries < self.inner.config.max_transport_retries {
                        transport_retries += 1;
                        current_resume_hint = self.current_bytes_transferred(id, generation).max(1);
                        self.set_reconnecting_notice(
                            id,
                            generation,
                            current_resume_hint,
                            transport_retries,
                            &err.to_string(),
                        );
                        if self
                            .wait_reconnect_backoff(
                                transport_retries,
                                &mut control_rx,
                                &reconnect_notifier,
                            )
                            .await
                            .is_some()
                        {
                            if let Some(signal) = current_interrupt(&control_rx) {
                                self.apply_interrupt(id, generation, signal);
                            }
                            return;
                        }
                        continue;
                    }
                    self.finalize_dir_batches(
                        id,
                        generation,
                        &acquired.session,
                        upload_batch.as_ref(),
                        download_batch.as_ref(),
                    )
                    .await;
                    self.mark_failed_with_drop(id, generation, err.to_string(), drop_err);
                    return;
                }
            }
        }
    }

    async fn finalize_dir_batches(
        &self,
        id: TransferId,
        generation: u64,
        sftp: &SftpSession,
        upload_batch: Option<&Arc<UploadDirBatch>>,
        download_batch: Option<&Arc<DownloadDirBatch>>,
    ) {
        let mut notices = Vec::new();
        if let Some(batch) = upload_batch {
            notices.extend(batch.finish_file_and_finalize_dirs(id, sftp).await);
        }
        if let Some(batch) = download_batch {
            notices.extend(batch.finish_file_and_finalize_dirs(id).await);
        }
        for notice in notices {
            self.record_transfer_notice(id, generation, notice);
        }
    }

    async fn wait_reconnect_backoff(
        &self,
        attempt: u32,
        control_rx: &mut watch::Receiver<ControlSignal>,
        reconnect_notifier: &Arc<Notify>,
    ) -> Option<ControlSignal> {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1).min(5))
            .unwrap_or(32);
        let base = self
            .inner
            .config
            .reconnect_retry_delay
            .saturating_mul(factor)
            .min(MAX_RECONNECT_BACKOFF_DELAY);
        let jitter_cap_ms = (base.as_millis() / 4).min(u128::from(u64::MAX)) as u64;
        let jitter = if jitter_cap_ms > 0 {
            Duration::from_millis(rand::rng().random_range(0..=jitter_cap_ms))
        } else {
            Duration::ZERO
        };
        let delay = base.saturating_add(jitter);
        tokio::select! {
            biased;
            _ = control_rx.changed() => current_interrupt(control_rx),
            _ = reconnect_notifier.notified() => None,
            _ = tokio::time::sleep(delay) => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_upload(
        &self,
        id: TransferId,
        generation: u64,
        sftp: &SftpSession,
        local_path: &Path,
        initial_remote_path: &str,
        resume_hint: u64,
        control_rx: &mut watch::Receiver<ControlSignal>,
    ) -> Result<TransferOutcome, SshError> {
        let local_meta = tokio::fs::metadata(local_path).await?;
        let total_bytes = local_meta.len();
        let local_mtime = local_meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs().min(u32::MAX as u64) as u32);
        let source_changed = self.check_or_record_source_meta(id, generation, total_bytes, local_mtime);
        self.update_total_bytes(id, generation, total_bytes);

        let transfer_cfg = self.transfer_config();
        let effective_resume_hint = if source_changed {
            if total_bytes > transfer_cfg.part_threshold_bytes {
                let _ = sftp
                    .remove_file(part_path_for(initial_remote_path).as_str())
                    .await;
            }
            0
        } else {
            resume_hint
        };
        let (resolved_conflict, mut overwrite_authorized, force_direct) =
            self.entry_conflict_flags(id, generation);
        let mut remote_path = initial_remote_path.to_string();
        let mut resume_existing_target = false;

        if let Some(decision) = resolved_conflict {
            match decision {
                ResolvedConflict::Overwrite => overwrite_authorized = true,
                ResolvedConflict::Resume => {
                    overwrite_authorized = true;
                    resume_existing_target = !source_changed;
                }
                ResolvedConflict::AutoRename => {
                    let check = self
                        .inner
                        .hook
                        .check_upload_conflict(
                            sftp,
                            id,
                            local_path,
                            &remote_path,
                            total_bytes,
                            transfer_cfg.part_threshold_bytes,
                            TransferConflictPolicy::Rename,
                        )
                        .await?;
                    if let ConflictCheck::Renamed {
                        new_remote_path,
                        new_file_name,
                        ..
                    } = check
                    {
                        remote_path = new_remote_path.clone();
                        self.update_target_paths(id, generation, None, Some((new_remote_path, new_file_name)));
                    }
                }
            }
        } else if effective_resume_hint == 0 {
            let policy = self.effective_conflict_policy();
            match self
                .inner
                .hook
                .check_upload_conflict(
                    sftp,
                    id,
                    local_path,
                    &remote_path,
                    total_bytes,
                    transfer_cfg.part_threshold_bytes,
                    policy,
                )
                .await?
            {
                ConflictCheck::Proceed {
                    overwrite_authorized: ow,
                    resume_existing,
                } => {
                    overwrite_authorized = ow;
                    resume_existing_target = resume_existing && !source_changed;
                    if ow {
                        self.record_resolved_conflict(id, generation, ResolvedConflict::Overwrite, true);
                    } else if resume_existing {
                        self.record_resolved_conflict(id, generation, ResolvedConflict::Resume, true);
                    }
                }
                ConflictCheck::AwaitingConflict(conflict) => {
                    return Ok(TransferOutcome::AwaitingConflict(conflict));
                }
                ConflictCheck::Skip => return Ok(TransferOutcome::Skipped),
                ConflictCheck::Renamed {
                    new_remote_path,
                    new_file_name,
                    ..
                } => {
                    remote_path = new_remote_path.clone();
                    self.update_target_paths(id, generation, None, Some((new_remote_path, new_file_name)));
                }
            }
        }

        let plan = self
            .inner
            .hook
            .prepare_upload_with_options(
                sftp,
                local_path,
                &remote_path,
                total_bytes,
                UploadPlanOptions {
                    part_threshold_bytes: transfer_cfg.part_threshold_bytes,
                    resume_hint: effective_resume_hint,
                    force_direct_fallback: force_direct,
                    overwrite_authorized,
                    resume_existing_target,
                },
            )
            .await?;

        match self
            .execute_upload_plan(id, generation, sftp, local_path, &plan, control_rx)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(err) if plan.using_part_file && is_part_fallback_status(&err) => {
                let _ = sftp.remove_file(plan.write_path.as_str()).await;
                self.set_force_direct_fallback(id, generation);
                let notice = TransferNotice::PartFallbackDirectWrite {
                    reason: err.to_string(),
                };
                let mut fallback_plan = self
                    .inner
                    .hook
                    .prepare_upload_with_options(
                        sftp,
                        local_path,
                        &remote_path,
                        total_bytes,
                        UploadPlanOptions {
                            part_threshold_bytes: transfer_cfg.part_threshold_bytes,
                            resume_hint: 0,
                            force_direct_fallback: true,
                            overwrite_authorized: plan.overwrite_authorized,
                            resume_existing_target: false,
                        },
                    )
                    .await?;
                fallback_plan.status_notice = Some(notice.summary());
                fallback_plan.notice = Some(notice);
                self.execute_upload_plan(
                    id,
                    generation,
                    sftp,
                    local_path,
                    &fallback_plan,
                    control_rx,
                )
                .await
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_upload_plan(
        &self,
        id: TransferId,
        generation: u64,
        sftp: &SftpSession,
        local_path: &Path,
        plan: &UploadTargetPlan,
        control_rx: &mut watch::Receiver<ControlSignal>,
    ) -> Result<TransferOutcome, SshError> {
        self.update_upload_plan(id, generation, plan);

        let mut flags = OpenFlags::CREATE | OpenFlags::WRITE;
        if plan.truncate {
            flags |= OpenFlags::TRUNCATE;
        }
        let mut remote_file = sftp.open_with_flags(plan.write_path.as_str(), flags).await?;
        let mut local_file = tokio::fs::File::open(local_path).await?;

        if plan.resume_offset > 0 {
            local_file
                .seek(SeekFrom::Start(plan.resume_offset))
                .await?;
            remote_file
                .seek(SeekFrom::Start(plan.resume_offset))
                .await
                .map_err(map_remote_sftp_io_error)?;
        }

        let mut transferred = plan.resume_offset;
        let mut session_bytes = 0u64;
        let started_at = Instant::now();
        let mut last_rate = 0u64;
        let mut last_progress_emit: Option<Instant> = None;
        let mut buf = vec![0u8; self.inner.config.chunk_size];

        loop {
            if let Some(signal) = current_interrupt(control_rx) {
                if signal == ControlSignal::Pause {
                    self.update_progress(id, generation, transferred, last_rate);
                }
                return Ok(cleanup_upload_interrupt(signal, sftp, remote_file, plan).await);
            }

            let read_n = tokio::select! {
                biased;
                _ = control_rx.changed() => {
                    if let Some(signal) = current_interrupt(control_rx) {
                        if signal == ControlSignal::Pause {
                            self.update_progress(id, generation, transferred, last_rate);
                        }
                        return Ok(cleanup_upload_interrupt(signal, sftp, remote_file, plan).await);
                    }
                    continue;
                }
                res = local_file.read(&mut buf) => res?,
            };

            if read_n == 0 {
                break;
            }

            tokio::select! {
                biased;
                _ = control_rx.changed() => {
                    if let Some(signal) = current_interrupt(control_rx) {
                        if signal == ControlSignal::Pause {
                            self.update_progress(id, generation, transferred, last_rate);
                        }
                        return Ok(cleanup_upload_interrupt(signal, sftp, remote_file, plan).await);
                    }
                }
                res = remote_file.write_all(&buf[..read_n]) => res.map_err(map_remote_sftp_io_error)?,
            }

            transferred += read_n as u64;
            session_bytes += read_n as u64;
            last_rate = compute_throughput(session_bytes, started_at);
            if last_progress_emit.is_none_or(|ts| ts.elapsed() >= PROGRESS_EMIT_INTERVAL) {
                last_progress_emit = Some(Instant::now());
                self.update_progress(id, generation, transferred, last_rate);
            }
        }

        tokio::select! {
            biased;
            _ = control_rx.changed() => {
                if let Some(signal) = current_interrupt(control_rx) {
                    return Ok(cleanup_upload_interrupt(signal, sftp, remote_file, plan).await);
                }
                remote_file.shutdown().await.map_err(map_remote_sftp_io_error)?;
            }
            res = remote_file.shutdown() => res.map_err(map_remote_sftp_io_error)?,
        }
        self.inner.hook.finalize_upload(sftp, plan).await?;
        if let Ok(meta) = tokio::fs::metadata(local_path).await
            && let Some(mode) = extract_local_posix_mode(&meta)
            && let Err(err) = apply_remote_permissions(sftp, &plan.final_path, mode).await
        {
            self.record_transfer_notice(
                id,
                generation,
                TransferNotice::PermissionPreserveSkipped {
                    reason: err.to_string(),
                },
            );
        }
        Ok(TransferOutcome::Completed {
            bytes: transferred,
            rate: last_rate.max(1),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_download(
        &self,
        id: TransferId,
        generation: u64,
        sftp: &SftpSession,
        remote_path: &str,
        initial_local_path: &Path,
        resume_hint: u64,
        control_rx: &mut watch::Receiver<ControlSignal>,
    ) -> Result<TransferOutcome, SshError> {
        let transfer_cfg = self.transfer_config();
        let (resolved_conflict, mut overwrite_authorized, _) =
            self.entry_conflict_flags(id, generation);
        let mut local_path = initial_local_path.to_path_buf();
        let mut effective_resume_hint = resume_hint;
        let mut resume_existing_target = false;

        if let Some(decision) = resolved_conflict {
            match decision {
                ResolvedConflict::Overwrite => {
                    overwrite_authorized = true;
                }
                ResolvedConflict::Resume => {
                    overwrite_authorized = true;
                    resume_existing_target = true;
                    effective_resume_hint = effective_resume_hint.max(1);
                }
                ResolvedConflict::AutoRename => {
                    if let ConflictCheck::Renamed {
                        new_local_path,
                        new_file_name,
                        ..
                    } = self
                        .inner
                        .hook
                        .check_download_conflict(
                            sftp,
                            id,
                            remote_path,
                            &local_path,
                            TransferConflictPolicy::Rename,
                        )
                        .await?
                    {
                        local_path = new_local_path.clone();
                        self.update_target_paths(
                            id,
                            generation,
                            Some((new_local_path, new_file_name)),
                            None,
                        );
                    }
                }
            }
        } else if resume_hint == 0 {
            let policy = self.effective_conflict_policy();
            match self
                .inner
                .hook
                .check_download_conflict(sftp, id, remote_path, &local_path, policy)
                .await?
            {
                ConflictCheck::Proceed {
                    overwrite_authorized: ow,
                    resume_existing,
                } => {
                    overwrite_authorized = ow;
                    if ow {
                        self.record_resolved_conflict(
                            id,
                            generation,
                            ResolvedConflict::Overwrite,
                            true,
                        );
                    } else if resume_existing {
                        resume_existing_target = true;
                        effective_resume_hint = 1;
                        self.record_resolved_conflict(
                            id,
                            generation,
                            ResolvedConflict::Resume,
                            false,
                        );
                    }
                }
                ConflictCheck::AwaitingConflict(conflict) => {
                    return Ok(TransferOutcome::AwaitingConflict(conflict));
                }
                ConflictCheck::Skip => return Ok(TransferOutcome::Skipped),
                ConflictCheck::Renamed {
                    new_local_path,
                    new_file_name,
                    ..
                } => {
                    local_path = new_local_path.clone();
                    self.update_target_paths(
                        id,
                        generation,
                        Some((new_local_path, new_file_name)),
                        None,
                    );
                }
            }
        }

        let (expected_source_size, expected_source_mtime) = self.source_meta_snapshot(id, generation);
        let plan = self
            .inner
            .hook
            .prepare_download_with_options(
                sftp,
                remote_path,
                &local_path,
                DownloadPlanOptions {
                    part_threshold_bytes: transfer_cfg.part_threshold_bytes,
                    resume_hint: effective_resume_hint,
                    overwrite_authorized,
                    resume_existing_target,
                    expected_source_size,
                    expected_source_mtime,
                },
            )
            .await?;
        let _ = self.check_or_record_source_meta(
            id,
            generation,
            plan.total_bytes,
            plan.modified_epoch_secs,
        );
        self.update_download_plan(id, generation, &plan);

        if let Some(parent) = plan.write_path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut remote_file = sftp.open_with_flags(remote_path, OpenFlags::READ).await?;
        let std_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(plan.truncate)
            .open(&plan.write_path)?;
        let mut local_file = tokio::fs::File::from_std(std_file);

        if plan.resume_offset > 0 {
            remote_file
                .seek(SeekFrom::Start(plan.resume_offset))
                .await
                .map_err(map_remote_sftp_io_error)?;
            local_file.set_len(plan.resume_offset).await?;
            local_file
                .seek(SeekFrom::Start(plan.resume_offset))
                .await?;
        }

        let mut transferred = plan.resume_offset;
        let mut session_bytes = 0u64;
        let started_at = Instant::now();
        let mut last_rate = 0u64;
        let mut last_progress_emit: Option<Instant> = None;
        let mut buf = vec![0u8; self.inner.config.chunk_size];

        loop {
            if let Some(signal) = current_interrupt(control_rx) {
                if signal == ControlSignal::Pause {
                    self.update_progress(id, generation, transferred, last_rate);
                }
                return Ok(cleanup_download_interrupt(signal, local_file, &plan).await);
            }

            let read_n = tokio::select! {
                biased;
                _ = control_rx.changed() => {
                    if let Some(signal) = current_interrupt(control_rx) {
                        if signal == ControlSignal::Pause {
                            self.update_progress(id, generation, transferred, last_rate);
                        }
                        return Ok(cleanup_download_interrupt(signal, local_file, &plan).await);
                    }
                    continue;
                }
                res = remote_file.read(&mut buf) => res.map_err(map_remote_sftp_io_error)?,
            };

            if read_n == 0 {
                break;
            }

            local_file.write_all(&buf[..read_n]).await?;
            transferred += read_n as u64;
            session_bytes += read_n as u64;
            last_rate = compute_throughput(session_bytes, started_at);
            if last_progress_emit.is_none_or(|ts| ts.elapsed() >= PROGRESS_EMIT_INTERVAL) {
                last_progress_emit = Some(Instant::now());
                self.update_progress(id, generation, transferred, last_rate);
            }
        }

        local_file.flush().await?;
        if plan.using_part_file {
            local_file.sync_data().await?;
        }
        drop(local_file);
        if plan.using_part_file && plan.write_path != plan.final_path {
            rename_local_part_file(&plan.write_path, &plan.final_path, plan.overwrite_authorized)
                .await?;
        }
        if let Some(mode) = plan.permissions
            && let Err(err) = apply_local_permissions(&plan.final_path, mode).await
        {
            self.record_transfer_notice(
                id,
                generation,
                TransferNotice::PermissionPreserveSkipped {
                    reason: err.to_string(),
                },
            );
        }
        let _ = remote_file.close().await;
        Ok(TransferOutcome::Completed {
            bytes: transferred,
            rate: last_rate.max(1),
        })
    }

    fn mark_in_progress(&self, id: TransferId, generation: u64) -> bool {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return false;
            };
            if entry.snapshot.status != TransferStatus::Queued {
                return false;
            }
            entry.snapshot.status = TransferStatus::InProgress;
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
        true
    }

    fn record_transfer_notice(&self, id: TransferId, generation: u64, notice: TransferNotice) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            let summary = notice.summary();
            if entry.snapshot.notice.is_none() {
                entry.snapshot.notice = Some(notice);
            }
            entry.snapshot.status_notice = Some(match entry.snapshot.status_notice.take() {
                Some(existing) if !existing.contains(&summary) => format!("{existing} {summary}"),
                _ => summary,
            });
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn update_transport_info(
        &self,
        id: TransferId,
        generation: u64,
        mode: SftpTransportMode,
        notice: Option<String>,
    ) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            entry.snapshot.transport_mode = Some(mode);
            if entry.was_reconnecting
                || matches!(
                    entry.snapshot.reconnect_state,
                    TransferReconnectState::AutoReconnecting { .. }
                )
            {
                entry.was_reconnecting = false;
                entry.snapshot.reconnect_state = TransferReconnectState::Reconnected;
                entry.snapshot.status_notice = Some(match notice {
                    Some(n) => format!("Reconnected successfully ({n})"),
                    None => "Reconnected successfully; resuming transfer...".to_string(),
                });
            } else {
                entry.snapshot.status_notice = notice;
            }
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn update_total_bytes(&self, id: TransferId, generation: u64, total_bytes: u64) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            entry.snapshot.total_bytes = total_bytes;
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn source_meta_snapshot(&self, id: TransferId, generation: u64) -> (Option<u64>, Option<u32>) {
        let entries = self.inner.entries.lock().expect("entries mutex poisoned");
        entries
            .iter()
            .find(|e| e.snapshot.id == id && e.generation == generation)
            .map(|e| (e.expected_source_size, e.expected_source_mtime))
            .unwrap_or((None, None))
    }

    fn check_or_record_source_meta(
        &self,
        id: TransferId,
        generation: u64,
        size: u64,
        mtime: Option<u32>,
    ) -> bool {
        let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
        let Some(entry) = entries
            .iter_mut()
            .find(|e| e.snapshot.id == id && e.generation == generation)
        else {
            return false;
        };
        if let Some(prev_size) = entry.expected_source_size {
            let changed = prev_size != size
                || (entry.expected_source_mtime.is_some()
                    && mtime.is_some()
                    && entry.expected_source_mtime != mtime);
            if changed {
                entry.expected_source_size = Some(size);
                entry.expected_source_mtime = mtime;
                entry.resume_hint = 0;
                entry.snapshot.resume_offset = 0;
                entry.snapshot.bytes_transferred = 0;
            }
            changed
        } else {
            entry.expected_source_size = Some(size);
            entry.expected_source_mtime = mtime;
            false
        }
    }

    fn update_upload_plan(&self, id: TransferId, generation: u64, plan: &UploadTargetPlan) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            entry.snapshot.resume_offset = plan.resume_offset;
            entry.snapshot.bytes_transferred = plan.resume_offset;
            entry.snapshot.using_part_file = plan.using_part_file;
            if let Some(notice) = &plan.notice {
                entry.snapshot.notice = Some(notice.clone());
            }
            if let Some(notice) = &plan.status_notice {
                entry.snapshot.status_notice = Some(notice.clone());
            }
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn update_download_plan(&self, id: TransferId, generation: u64, plan: &DownloadTargetPlan) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            entry.snapshot.total_bytes = plan.total_bytes;
            entry.snapshot.resume_offset = plan.resume_offset;
            entry.snapshot.bytes_transferred = plan.resume_offset;
            entry.snapshot.using_part_file = plan.using_part_file;
            if let Some(notice) = &plan.notice {
                entry.snapshot.notice = Some(notice.clone());
            }
            if let Some(notice) = &plan.status_notice {
                entry.snapshot.status_notice = Some(notice.clone());
            }
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn update_progress(&self, id: TransferId, generation: u64, bytes: u64, rate: u64) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            if entry.snapshot.status != TransferStatus::InProgress {
                return;
            }
            entry.snapshot.bytes_transferred = bytes;
            entry.snapshot.throughput_bytes_per_sec = rate;
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn mark_completed(&self, id: TransferId, generation: u64, bytes: u64, rate: u64) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            if entry.snapshot.status != TransferStatus::InProgress {
                return;
            }
            entry.snapshot.bytes_transferred = bytes;
            if entry.snapshot.total_bytes < bytes {
                entry.snapshot.total_bytes = bytes;
            }
            entry.snapshot.throughput_bytes_per_sec = rate;
            entry.snapshot.status = TransferStatus::Completed;
            if entry.was_reconnecting
                || matches!(
                    entry.snapshot.reconnect_state,
                    TransferReconnectState::AutoReconnecting { .. }
                        | TransferReconnectState::Reconnected
                )
            {
                entry.was_reconnecting = false;
                entry.snapshot.reconnect_state = TransferReconnectState::Reconnected;
                entry.snapshot.status_notice = Some("Reconnected successfully".to_string());
            } else {
                entry.snapshot.reconnect_state = TransferReconnectState::None;
            }
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn effective_conflict_policy(&self) -> TransferConflictPolicy {
        if let Some(override_policy) = *self
            .inner
            .batch_conflict_override
            .lock()
            .expect("batch_conflict_override mutex poisoned")
        {
            return override_policy;
        }
        self.transfer_config().default_conflict_policy
    }

    fn entry_conflict_flags(
        &self,
        id: TransferId,
        generation: u64,
    ) -> (Option<ResolvedConflict>, bool, bool) {
        let entries = self.inner.entries.lock().expect("entries mutex poisoned");
        entries
            .iter()
            .find(|e| e.snapshot.id == id && e.generation == generation)
            .map(|e| {
                (
                    e.resolved_conflict,
                    e.overwrite_authorized,
                    e.force_direct_fallback,
                )
            })
            .unwrap_or((None, false, false))
    }

    fn record_resolved_conflict(
        &self,
        id: TransferId,
        generation: u64,
        resolved: ResolvedConflict,
        overwrite_authorized: bool,
    ) {
        let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| e.snapshot.id == id && e.generation == generation)
        {
            entry.resolved_conflict = Some(resolved);
            entry.overwrite_authorized = overwrite_authorized;
        }
    }

    fn update_target_paths(
        &self,
        id: TransferId,
        generation: u64,
        local_update: Option<(PathBuf, String)>,
        remote_update: Option<(String, String)>,
    ) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            if let Some((path, name)) = local_update {
                entry.snapshot.local_path = path;
                entry.snapshot.file_name = name;
            }
            if let Some((path, name)) = remote_update {
                entry.snapshot.remote_path = path;
                entry.snapshot.file_name = name;
            }
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn mark_awaiting_conflict(
        &self,
        id: TransferId,
        generation: u64,
        conflict: TransferConflict,
    ) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            if entry.snapshot.status != TransferStatus::InProgress {
                return;
            }
            entry.snapshot.status = TransferStatus::AwaitingConflict;
            entry.snapshot.pending_conflict = Some(conflict);
            entry.snapshot.throughput_bytes_per_sec = 0;
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn mark_skipped(&self, id: TransferId, generation: u64) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            if entry.snapshot.status != TransferStatus::InProgress {
                return;
            }
            entry.snapshot.status = TransferStatus::Skipped;
            entry.snapshot.pending_conflict = None;
            entry.snapshot.throughput_bytes_per_sec = 0;
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn set_force_direct_fallback(&self, id: TransferId, generation: u64) {
        let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| e.snapshot.id == id && e.generation == generation)
        {
            entry.force_direct_fallback = true;
        }
    }

    fn current_bytes_transferred(&self, id: TransferId, generation: u64) -> u64 {
        let entries = self.inner.entries.lock().expect("entries mutex poisoned");
        entries
            .iter()
            .find(|e| e.snapshot.id == id && e.generation == generation)
            .map(|e| e.snapshot.bytes_transferred)
            .unwrap_or(0)
    }

    fn set_reconnecting_notice(
        &self,
        id: TransferId,
        generation: u64,
        resume_hint: u64,
        attempt: u32,
        reason: &str,
    ) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            entry.resume_hint = resume_hint;
            entry.was_reconnecting = true;
            entry.snapshot.throughput_bytes_per_sec = 0;
            entry.snapshot.reconnect_state = TransferReconnectState::AutoReconnecting {
                attempt,
                reason: reason.to_string(),
            };
            entry.snapshot.status_notice = Some(format!(
                "Auto-reconnecting transfer (attempt {attempt}): {reason}"
            ));
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn mark_failed_with_drop(
        &self,
        id: TransferId,
        generation: u64,
        error: String,
        failed_on_transport_drop: bool,
    ) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            if matches!(
                entry.snapshot.status,
                TransferStatus::Paused | TransferStatus::Cancelled
            ) {
                return;
            }
            entry.failed_on_transport_drop = failed_on_transport_drop;
            entry.was_reconnecting = false;
            entry.snapshot.reconnect_state = TransferReconnectState::None;
            entry.snapshot.status = TransferStatus::Failed { error };
            entry.snapshot.throughput_bytes_per_sec = 0;
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn apply_interrupt(&self, id: TransferId, generation: u64, signal: ControlSignal) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries
                .iter_mut()
                .find(|e| e.snapshot.id == id && e.generation == generation)
            else {
                return;
            };
            if entry.snapshot.status.is_terminal() {
                return;
            }
            entry.snapshot.throughput_bytes_per_sec = 0;
            entry.snapshot.status = match signal {
                ControlSignal::Pause => {
                    entry.resume_hint = entry.snapshot.bytes_transferred;
                    TransferStatus::Paused
                }
                ControlSignal::Cancel => TransferStatus::Cancelled,
                ControlSignal::Run => return,
            };
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }
}

enum TransferOutcome {
    Completed { bytes: u64, rate: u64 },
    AwaitingConflict(TransferConflict),
    Skipped,
    Interrupted(ControlSignal),
}

fn remote_leaf_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("file")
        .to_string()
}

fn sanitize_leaf_filename(raw: &str) -> String {
    let trimmed = raw.trim();
    let leaf = Path::new(trimmed)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if leaf == "." || leaf == ".." {
        String::new()
    } else {
        leaf
    }
}

/// Formats a disambiguated filename (`"report.txt"` + `1` -> `"report (1).txt"`).
pub fn disambiguate_filename(name: &str, index: usize) -> String {
    if let Some((stem, ext)) = name.rsplit_once('.')
        && !stem.is_empty()
    {
        format!("{stem} ({index}).{ext}")
    } else {
        format!("{name} ({index})")
    }
}

async fn next_available_remote_path(
    sftp: &SftpSession,
    remote_path: &str,
) -> Result<String, SshError> {
    let parent = parent_remote_path(remote_path);
    let leaf = remote_leaf_name(remote_path);
    for idx in 1..=1000usize {
        let candidate_name = disambiguate_filename(&leaf, idx);
        let candidate = join_remote_path(&parent, &candidate_name);
        match sftp.metadata(candidate.as_str()).await {
            Ok(_) => continue,
            Err(err) if is_sftp_not_found(&err) => return Ok(candidate),
            Err(err) => return Err(err.into()),
        }
    }
    Err(SshError::Sftp(format!(
        "could not find unused remote filename near {remote_path}"
    )))
}

fn next_available_local_path(local_path: &Path) -> PathBuf {
    let parent = local_path.parent();
    let leaf = local_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    for idx in 1..=1000usize {
        let candidate_name = disambiguate_filename(&leaf, idx);
        let candidate = match parent.filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => p.join(&candidate_name),
            None => PathBuf::from(&candidate_name),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    local_path.with_extension("renamed")
}

fn is_part_fallback_status(err: &SshError) -> bool {
    matches!(
        err,
        SshError::SftpStatus {
            status_code: StatusCode::PermissionDenied
                | StatusCode::OpUnsupported
                | StatusCode::Failure,
            ..
        }
    )
}

fn current_interrupt(control_rx: &watch::Receiver<ControlSignal>) -> Option<ControlSignal> {
    match *control_rx.borrow() {
        ControlSignal::Run => None,
        signal => Some(signal),
    }
}

async fn cleanup_upload_interrupt(
    signal: ControlSignal,
    sftp: &SftpSession,
    mut remote_file: russh_sftp::client::fs::File,
    plan: &UploadTargetPlan,
) -> TransferOutcome {
    if signal == ControlSignal::Pause {
        let _ = remote_file.shutdown().await;
    } else if signal == ControlSignal::Cancel && plan.using_part_file {
        drop(remote_file);
        let _ = sftp.remove_file(plan.write_path.as_str()).await;
    }
    TransferOutcome::Interrupted(signal)
}

async fn cleanup_download_interrupt(
    signal: ControlSignal,
    mut local_file: tokio::fs::File,
    plan: &DownloadTargetPlan,
) -> TransferOutcome {
    if signal == ControlSignal::Pause {
        let _ = local_file.flush().await;
    } else if signal == ControlSignal::Cancel && plan.using_part_file {
        drop(local_file);
        let _ = tokio::fs::remove_file(&plan.write_path).await;
    }
    TransferOutcome::Interrupted(signal)
}

async fn rename_local_part_file(
    part_path: &Path,
    final_path: &Path,
    overwrite_authorized: bool,
) -> Result<(), SshError> {
    match tokio::fs::rename(part_path, final_path).await {
        Ok(()) => Ok(()),
        Err(err) if overwrite_authorized => {
            if tokio::fs::metadata(final_path).await.is_ok() {
                tokio::fs::remove_file(final_path).await?;
                tokio::fs::rename(part_path, final_path).await?;
                Ok(())
            } else {
                Err(err.into())
            }
        }
        Err(err) => Err(err.into()),
    }
}

fn compute_throughput(session_bytes: u64, started_at: Instant) -> u64 {
    if session_bytes == 0 {
        return 0;
    }
    let elapsed_secs = started_at.elapsed().as_secs_f64().max(0.001);
    ((session_bytes as f64 / elapsed_secs).round() as u64).max(1)
}

fn build_queue_snapshot(entries: &[TransferEntry]) -> TransferQueueSnapshot {
    let transfers: Vec<TransferSnapshot> = entries.iter().map(|e| e.snapshot.clone()).collect();
    build_snapshot_from_transfers(transfers)
}

fn build_snapshot_from_transfers(transfers: Vec<TransferSnapshot>) -> TransferQueueSnapshot {
    let mut snap = TransferQueueSnapshot {
        transfers,
        ..Default::default()
    };

    for s in &snap.transfers {
        match &s.status {
            TransferStatus::Queued => {
                snap.queued_count += 1;
                snap.bytes_done = snap.bytes_done.saturating_add(s.bytes_transferred);
                snap.bytes_total = snap.bytes_total.saturating_add(s.total_bytes);
            }
            TransferStatus::InProgress => {
                snap.active_count += 1;
                snap.bytes_done = snap.bytes_done.saturating_add(s.bytes_transferred);
                snap.bytes_total = snap.bytes_total.saturating_add(s.total_bytes);
                snap.total_bytes_per_sec = snap
                    .total_bytes_per_sec
                    .saturating_add(s.throughput_bytes_per_sec);
            }
            TransferStatus::Paused => snap.paused_count += 1,
            TransferStatus::AwaitingConflict => snap.awaiting_conflict_count += 1,
            TransferStatus::Completed => snap.completed_count += 1,
            TransferStatus::Skipped => snap.skipped_count += 1,
            TransferStatus::Failed { .. } => snap.failed_count += 1,
            TransferStatus::Cancelled => snap.cancelled_count += 1,
        }
    }

    snap
}
