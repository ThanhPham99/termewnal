use std::fs::{self, OpenOptions};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore, watch};

use crate::auth::BoxFuture;
use crate::error::SshError;
use crate::sftp::{ProfileTransportKey, SftpEndpoint, SftpManager, SftpTransportMode};

/// Default maximum number of simultaneous active `Transfer`s.
pub const DEFAULT_MAX_CONCURRENT_TRANSFERS: usize = 3;
/// Default pipelined chunk size (`255 KiB`, fitting within a single `256 KiB` SFTP write packet).
pub const DEFAULT_TRANSFER_CHUNK_SIZE: usize = 255 * 1024;
const MIN_TRANSFER_CHUNK_SIZE: usize = 128 * 1024;
const MAX_TRANSFER_CHUNK_SIZE: usize = 256 * 1024;

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

/// Lifecycle state of an individual [`TransferSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferStatus {
    Queued,
    InProgress,
    Paused,
    Completed,
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

    /// Returns `true` if this `Transfer` reached a terminal state (`Completed`, `Failed`, `Cancelled`).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed { .. } | Self::Cancelled
        )
    }

    /// Returns a concise human-readable status label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::InProgress => "In Progress",
            Self::Paused => "Paused",
            Self::Completed => "Completed",
            Self::Failed { .. } => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
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
    pub resume_offset: u64,
    pub using_part_file: bool,
}

impl TransferSnapshot {
    /// Returns normalized progress in `[0.0, 1.0]`.
    pub fn progress_fraction(&self) -> f32 {
        if matches!(self.status, TransferStatus::Completed) && self.total_bytes == 0 {
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
    pub completed_count: usize,
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
}

/// Target preparation descriptor produced by [`TransferExecutorHook::prepare_download`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTargetPlan {
    pub total_bytes: u64,
    pub resume_offset: u64,
    pub truncate: bool,
    pub status_notice: Option<String>,
}

/// Extension hook allowing Ticket 14 to inject `.termewnal.part` atomic uploads, byte-offset
/// resume, and conflict resolution without rewriting [`TransferQueue`].
pub trait TransferExecutorHook: Send + Sync {
    /// Resolves the remote target path, truncation mode, and resume offset before uploading.
    fn prepare_upload<'a>(
        &'a self,
        sftp: &'a SftpSession,
        local_path: &'a Path,
        remote_path: &'a str,
        total_bytes: u64,
        resume_hint: u64,
    ) -> BoxFuture<'a, Result<UploadTargetPlan, SshError>>;

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
}

/// Default Ticket 13 [`TransferExecutorHook`] performing direct pipelined file transfers.
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
                    Err(_) => (0, true),
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
                total_bytes,
                resume_offset,
                truncate,
                status_notice: None,
            })
        })
    }
}

/// Configuration for [`TransferQueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferQueueConfig {
    pub max_concurrent_transfers: usize,
    pub chunk_size: usize,
}

impl Default for TransferQueueConfig {
    fn default() -> Self {
        Self {
            max_concurrent_transfers: DEFAULT_MAX_CONCURRENT_TRANSFERS,
            chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlSignal {
    Run,
    Pause,
    Cancel,
}

struct TransferEntry {
    snapshot: TransferSnapshot,
    endpoint: Option<SftpEndpoint>,
    generation: u64,
    resume_hint: u64,
    control_tx: watch::Sender<ControlSignal>,
}

struct TransferQueueInner {
    sftp: SftpManager,
    config: TransferQueueConfig,
    hook: Arc<dyn TransferExecutorHook>,
    semaphore: Arc<Semaphore>,
    next_id: Mutex<u64>,
    entries: Mutex<Vec<TransferEntry>>,
    snapshot_tx: watch::Sender<TransferQueueSnapshot>,
    idle_notify: Notify,
}

/// Concurrent SFTP `Transfer` queue with concurrency cap, pipelined chunk streaming,
/// live throughput telemetry, and independent pause/resume/cancel/retry controls.
#[derive(Clone)]
pub struct TransferQueue {
    inner: Arc<TransferQueueInner>,
}

impl TransferQueue {
    /// Creates a [`TransferQueue`] with default settings (`max_concurrent_transfers = 3`, `256 KiB`).
    pub fn new(sftp: SftpManager) -> Self {
        Self::with_config(sftp, TransferQueueConfig::default())
    }

    /// Creates a [`TransferQueue`] with custom concurrency and chunk configuration.
    pub fn with_config(sftp: SftpManager, config: TransferQueueConfig) -> Self {
        let config = config.sanitized();
        let (snapshot_tx, _) = watch::channel(TransferQueueSnapshot::default());
        Self {
            inner: Arc::new(TransferQueueInner {
                sftp,
                config,
                hook: Arc::new(DirectTransferHook),
                semaphore: Arc::new(Semaphore::new(config.max_concurrent_transfers)),
                next_id: Mutex::new(1),
                entries: Mutex::new(Vec::new()),
                snapshot_tx,
                idle_notify: Notify::new(),
            }),
        }
    }

    /// Replaces the [`TransferExecutorHook`] (used by Ticket 14 for `.part` resume and conflicts).
    pub fn with_hook(self, hook: Arc<dyn TransferExecutorHook>) -> Self {
        let inner = Arc::new(TransferQueueInner {
            sftp: self.inner.sftp.clone(),
            config: self.inner.config,
            hook,
            semaphore: Arc::clone(&self.inner.semaphore),
            next_id: Mutex::new(*self.inner.next_id.lock().expect("id mutex poisoned")),
            entries: Mutex::new(Vec::new()),
            snapshot_tx: watch::channel(TransferQueueSnapshot::default()).0,
            idle_notify: Notify::new(),
        });
        Self { inner }
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
        let total_bytes = fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
        let file_name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| remote_path.clone());
        self.enqueue_entry(
            Some(endpoint),
            TransferDirection::Upload,
            local_path,
            remote_path,
            file_name,
            total_bytes,
            TransferStatus::Queued,
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
                resume_offset: 0,
                using_part_file: false,
            },
            endpoint: None,
            generation: 1,
            resume_hint: 0,
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
                resume_offset: 0,
                using_part_file: false,
            },
            endpoint,
            generation: 1,
            resume_hint: 0,
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

    /// Cancels a `Queued`, `InProgress`, or `Paused` `Transfer`.
    pub fn cancel(&self, id: TransferId) -> bool {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            let Some(entry) = entries.iter_mut().find(|e| e.snapshot.id == id) else {
                return false;
            };
            if entry.snapshot.status.is_terminal() {
                return false;
            }
            let _ = entry.control_tx.send(ControlSignal::Cancel);
            entry.snapshot.status = TransferStatus::Cancelled;
            entry.snapshot.throughput_bytes_per_sec = 0;
            build_queue_snapshot(&entries)
        };
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
                    TransferStatus::Failed { .. } | TransferStatus::Cancelled => {
                        if entry.endpoint.is_none() {
                            return None;
                        }
                        entry.generation += 1;
                        entry.resume_hint = 0;
                        entry.snapshot.bytes_transferred = 0;
                        entry.snapshot.resume_offset = 0;
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
                    entry.snapshot.throughput_bytes_per_sec = 0;
                }
            }
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    /// Removes `Completed` and `Cancelled` entries from the queue snapshot.
    pub fn clear_finished(&self) {
        let snap = {
            let mut entries = self.inner.entries.lock().expect("entries mutex poisoned");
            entries.retain(|e| {
                !matches!(
                    e.snapshot.status,
                    TransferStatus::Completed | TransferStatus::Cancelled
                )
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
        let (endpoint, direction, local_path, remote_path, resume_hint, mut control_rx) = {
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

        let acquired = match self.inner.sftp.acquire_transfer_session(&endpoint).await {
            Ok(session) => session,
            Err(err) => {
                self.mark_failed(id, generation, err.to_string());
                return;
            }
        };

        self.update_transport_info(id, generation, acquired.mode, acquired.notice.clone());

        let outcome = match direction {
            TransferDirection::Upload => {
                self.stream_upload(
                    id,
                    generation,
                    &acquired.session,
                    &local_path,
                    &remote_path,
                    resume_hint,
                    &mut control_rx,
                )
                .await
            }
            TransferDirection::Download => {
                self.stream_download(
                    id,
                    generation,
                    &acquired.session,
                    &remote_path,
                    &local_path,
                    resume_hint,
                    &mut control_rx,
                )
                .await
            }
        };

        match outcome {
            Ok(TransferOutcome::Completed { bytes, rate }) => {
                self.mark_completed(id, generation, bytes, rate);
            }
            Ok(TransferOutcome::Interrupted(signal)) => {
                self.apply_interrupt(id, generation, signal);
            }
            Err(err) => {
                if let Some(signal) = current_interrupt(&control_rx) {
                    self.apply_interrupt(id, generation, signal);
                } else {
                    self.mark_failed(id, generation, err.to_string());
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_upload(
        &self,
        id: TransferId,
        generation: u64,
        sftp: &SftpSession,
        local_path: &Path,
        remote_path: &str,
        resume_hint: u64,
        control_rx: &mut watch::Receiver<ControlSignal>,
    ) -> Result<TransferOutcome, SshError> {
        let local_meta = tokio::fs::metadata(local_path).await?;
        let total_bytes = local_meta.len();
        self.update_total_bytes(id, generation, total_bytes);

        let plan = self
            .inner
            .hook
            .prepare_upload(sftp, local_path, remote_path, total_bytes, resume_hint)
            .await?;

        self.update_upload_plan(id, generation, &plan);

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
                .await?;
        }

        let mut transferred = plan.resume_offset;
        let mut session_bytes = 0u64;
        let started_at = Instant::now();
        let mut last_rate = 0u64;
        let mut buf = vec![0u8; self.inner.config.chunk_size];

        loop {
            if let Some(signal) = current_interrupt(control_rx) {
                if signal == ControlSignal::Pause {
                    let _ = remote_file.shutdown().await;
                }
                return Ok(TransferOutcome::Interrupted(signal));
            }

            let read_n = tokio::select! {
                biased;
                _ = control_rx.changed() => {
                    if let Some(signal) = current_interrupt(control_rx) {
                        if signal == ControlSignal::Pause {
                            let _ = remote_file.shutdown().await;
                        }
                        return Ok(TransferOutcome::Interrupted(signal));
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
                            let _ = remote_file.shutdown().await;
                        }
                        return Ok(TransferOutcome::Interrupted(signal));
                    }
                }
                res = remote_file.write_all(&buf[..read_n]) => res?,
            }

            transferred += read_n as u64;
            session_bytes += read_n as u64;
            last_rate = compute_throughput(session_bytes, started_at);
            self.update_progress(id, generation, transferred, last_rate);
        }

        tokio::select! {
            biased;
            _ = control_rx.changed() => {
                if let Some(signal) = current_interrupt(control_rx) {
                    if signal == ControlSignal::Pause {
                        let _ = remote_file.shutdown().await;
                    }
                    return Ok(TransferOutcome::Interrupted(signal));
                }
                remote_file.shutdown().await?;
            }
            res = remote_file.shutdown() => res?,
        }
        self.inner.hook.finalize_upload(sftp, &plan).await?;
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
        local_path: &Path,
        resume_hint: u64,
        control_rx: &mut watch::Receiver<ControlSignal>,
    ) -> Result<TransferOutcome, SshError> {
        let plan = self
            .inner
            .hook
            .prepare_download(sftp, remote_path, local_path, resume_hint)
            .await?;
        self.update_total_bytes(id, generation, plan.total_bytes);

        if let Some(parent) = local_path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut remote_file = sftp.open_with_flags(remote_path, OpenFlags::READ).await?;
        let std_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(plan.truncate)
            .open(local_path)?;
        let mut local_file = tokio::fs::File::from_std(std_file);

        if plan.resume_offset > 0 {
            remote_file
                .seek(SeekFrom::Start(plan.resume_offset))
                .await?;
            local_file
                .seek(SeekFrom::Start(plan.resume_offset))
                .await?;
        }

        let mut transferred = plan.resume_offset;
        let mut session_bytes = 0u64;
        let started_at = Instant::now();
        let mut last_rate = 0u64;
        let mut buf = vec![0u8; self.inner.config.chunk_size];

        loop {
            if let Some(signal) = current_interrupt(control_rx) {
                let _ = local_file.flush().await;
                return Ok(TransferOutcome::Interrupted(signal));
            }

            let read_n = tokio::select! {
                biased;
                _ = control_rx.changed() => {
                    if let Some(signal) = current_interrupt(control_rx) {
                        let _ = local_file.flush().await;
                        return Ok(TransferOutcome::Interrupted(signal));
                    }
                    continue;
                }
                res = remote_file.read(&mut buf) => res?,
            };

            if read_n == 0 {
                break;
            }

            local_file.write_all(&buf[..read_n]).await?;
            transferred += read_n as u64;
            session_bytes += read_n as u64;
            last_rate = compute_throughput(session_bytes, started_at);
            self.update_progress(id, generation, transferred, last_rate);
        }

        local_file.flush().await?;
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
            entry.snapshot.status_notice = notice;
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
            build_queue_snapshot(&entries)
        };
        self.publish_snapshot(snap);
    }

    fn mark_failed(&self, id: TransferId, generation: u64, error: String) {
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
    Interrupted(ControlSignal),
}

fn current_interrupt(control_rx: &watch::Receiver<ControlSignal>) -> Option<ControlSignal> {
    match *control_rx.borrow() {
        ControlSignal::Run => None,
        signal => Some(signal),
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
    let mut snap = TransferQueueSnapshot {
        transfers: entries.iter().map(|e| e.snapshot.clone()).collect(),
        ..Default::default()
    };

    for entry in entries {
        let s = &entry.snapshot;
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
            TransferStatus::Completed => snap.completed_count += 1,
            TransferStatus::Failed { .. } => snap.failed_count += 1,
            TransferStatus::Cancelled => snap.cancelled_count += 1,
        }
    }

    snap
}
