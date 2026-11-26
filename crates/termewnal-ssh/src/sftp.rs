use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::Metadata;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
use termewnal_core::vault::SecretStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, Notify, watch};
use zeroize::Zeroizing;

use crate::auth::{
    AgentConnector, BoxFuture, HostKeyDecision, HostKeyPrompt, KeyboardInteractiveChallenge,
    OsAgentConnector, SshInteractionHandler,
};
use crate::client::{SshConnectConfig, SshTransport, await_channel_reply};
use crate::error::SshError;

/// Default maximum SFTP packet length (`256 KiB`).
pub const DEFAULT_SFTP_MAX_PACKET_LEN: u32 = 262_144;
/// Default maximum SFTP write packet payload limit (`256 KiB`, overriding `russh-sftp` 32 KiB default).
pub const DEFAULT_SFTP_MAX_WRITE_PACKET_LEN: u32 = 262_144;
/// Default number of pipelined write requests kept in flight per SFTP file handle.
pub const DEFAULT_SFTP_MAX_CONCURRENT_WRITES: usize = 4;
/// Default number of pipelined read requests kept in flight per SFTP file handle.
pub const DEFAULT_SFTP_MAX_CONCURRENT_READS: usize = 4;
/// Default timeout in seconds for individual SFTP request replies.
pub const DEFAULT_SFTP_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Builds the pipelined `russh-sftp` client configuration (`128–256 KiB` chunks, 16 in-flight).
pub fn pipelined_sftp_client_config() -> russh_sftp::client::Config {
    russh_sftp::client::Config {
        max_packet_len: DEFAULT_SFTP_MAX_PACKET_LEN,
        max_concurrent_reads: DEFAULT_SFTP_MAX_CONCURRENT_READS,
        max_concurrent_writes: DEFAULT_SFTP_MAX_CONCURRENT_WRITES,
        max_write_packet_len: DEFAULT_SFTP_MAX_WRITE_PACKET_LEN,
        request_timeout_secs: DEFAULT_SFTP_REQUEST_TIMEOUT_SECS,
    }
}

/// Opens an SFTP subsystem channel on `transport` and initializes a pipelined [`SftpSession`].
pub async fn open_sftp_session(transport: &SshTransport) -> Result<SftpSession, SshError> {
    let mut channel = transport.open_session_channel().await?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|err| SshError::Sftp(format!("failed requesting SFTP subsystem: {err}")))?;

    await_channel_reply(&mut channel, "SFTP subsystem", |_| {})
        .await
        .map_err(|err| SshError::Sftp(err.to_string()))?;

    Ok(SftpSession::new_with_config(channel.into_stream(), pipelined_sftp_client_config()).await?)
}

/// Stable cache key identifying a `Profile` or ad-hoc SSH target for `Transfer Transport` reuse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileTransportKey {
    pub profile_id: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
}

impl ProfileTransportKey {
    /// Creates a [`ProfileTransportKey`] for a `Profile` or endpoint tuple.
    pub fn new(
        profile_id: Option<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
    ) -> Self {
        Self {
            profile_id,
            host: host.into(),
            port,
            username: username.into(),
        }
    }

    /// Builds a [`ProfileTransportKey`] from an optional `profile_id` and [`SshConnectConfig`].
    pub fn from_connect_config(profile_id: Option<String>, cfg: &SshConnectConfig) -> Self {
        Self::new(profile_id, cfg.host.clone(), cfg.port, cfg.username.clone())
    }
}

/// Transport mode currently serving SFTP browser and `Transfer` operations for a `Profile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpTransportMode {
    /// Dedicated isolated `Transfer Transport` separate from the terminal `Primary Transport`.
    IsolatedTransferTransport,
    /// Multiplexed SFTP channel on `Primary Transport` because 2FA/interactive auth prevented
    /// opening a silent secondary connection.
    MuxFallback,
}

impl SftpTransportMode {
    /// Returns a concise UI label describing this transport mode.
    pub fn label(self) -> &'static str {
        match self {
            Self::IsolatedTransferTransport => "Transfer Transport",
            Self::MuxFallback => "Mux Fallback (Primary Transport)",
        }
    }
}

/// Non-blocking status notice describing the active SFTP transport mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpStatusNotice {
    pub mode: SftpTransportMode,
    pub message: Option<String>,
}

/// Live connection & recovery lifecycle state of an SFTP endpoint in [`SftpManager`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SftpTransportStatus {
    #[default]
    Idle,
    Connected {
        mode: SftpTransportMode,
    },
    AutoReconnecting {
        attempt: u32,
        reason: String,
    },
    Reconnected {
        mode: SftpTransportMode,
    },
    Error {
        reason: String,
    },
}

/// Classification of a remote filesystem entry returned by SFTP directory listings or `stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteEntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

impl From<FileType> for RemoteEntryKind {
    fn from(ft: FileType) -> Self {
        match ft {
            FileType::Dir => Self::Dir,
            FileType::File => Self::File,
            FileType::Symlink => Self::Symlink,
            FileType::Other => Self::Other,
        }
    }
}

/// Remote file or directory entry surfaced to the SFTP Browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDirEntry {
    pub name: String,
    pub path: String,
    pub kind: RemoteEntryKind,
    pub size: u64,
    pub permissions: Option<u32>,
    pub modified_epoch_secs: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl RemoteDirEntry {
    fn from_metadata(name: String, path: String, meta: &Metadata) -> Self {
        Self {
            name,
            path,
            kind: meta.file_type().into(),
            size: meta.len(),
            permissions: meta.permissions,
            modified_epoch_secs: meta.mtime,
            uid: meta.uid,
            gid: meta.gid,
        }
    }

    /// Returns `true` if this entry represents a directory.
    pub fn is_dir(&self) -> bool {
        self.kind == RemoteEntryKind::Dir
    }
}

/// Result of listing a remote directory via [`SftpManager::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteListing {
    pub resolved_path: String,
    pub entries: Vec<RemoteDirEntry>,
    pub transport_mode: SftpTransportMode,
    pub notice: Option<String>,
}

/// Target descriptor supplying authentication, `Vault`, and optional `Primary Transport` fallback.
#[derive(Clone)]
pub struct SftpEndpoint {
    pub key: ProfileTransportKey,
    pub connect_config: SshConnectConfig,
    pub vault: Option<Arc<dyn SecretStore>>,
    pub agent_connector: Arc<dyn AgentConnector>,
    pub primary_transport: Option<Arc<SshTransport>>,
}

impl SftpEndpoint {
    /// Creates an [`SftpEndpoint`] with no `Vault` or `Primary Transport` attached yet.
    pub fn new(key: ProfileTransportKey, connect_config: SshConnectConfig) -> Self {
        Self {
            key,
            connect_config,
            vault: None,
            agent_connector: Arc::new(OsAgentConnector::default()),
            primary_transport: None,
        }
    }

    /// Attaches a `Vault` [`SecretStore`] for silent secondary authentication.
    pub fn with_vault(mut self, vault: Arc<dyn SecretStore>) -> Self {
        self.vault = Some(vault);
        self
    }

    /// Attaches an [`AgentConnector`] for SSH Agent authentication.
    pub fn with_agent_connector(mut self, connector: Arc<dyn AgentConnector>) -> Self {
        self.agent_connector = connector;
        self
    }

    /// Attaches the active `Primary Transport` used for automatic 2FA mux fallback.
    pub fn with_primary_transport(mut self, transport: Arc<SshTransport>) -> Self {
        self.primary_transport = Some(transport);
        self
    }
}

/// Non-interactive handler used when opening background `Transfer Transport`s:
/// never prompts for passwords/2FA/TOFU (unknown host keys are rejected strictly).
#[derive(Debug, Default)]
struct SilentTransferInteractionHandler;

impl SshInteractionHandler for SilentTransferInteractionHandler {
    fn verify_unknown_host(&self, _prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async { HostKeyDecision::Reject })
    }

    fn prompt_key_passphrase(&self, _key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        Box::pin(async { None })
    }

    fn prompt_password(
        &self,
        _username: &str,
        _host: &str,
    ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        Box::pin(async { None })
    }

    fn prompt_keyboard_interactive(
        &self,
        _challenge: KeyboardInteractiveChallenge,
    ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
        Box::pin(async { None })
    }
}

/// Role requesting an SFTP channel from [`TransferTransportPool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpChannelRole {
    Browser,
    Transfer,
}

/// Acquired [`SftpSession`] together with its transport isolation status.
#[derive(Clone)]
pub struct AcquiredSftpSession {
    pub session: Arc<SftpSession>,
    pub mode: SftpTransportMode,
    pub notice: Option<String>,
}

struct PooledTransportEntry {
    mode: SftpTransportMode,
    notice: Option<String>,
    transfer_transport: Option<Arc<SshTransport>>,
    primary_transport: Option<Arc<SshTransport>>,
    browser_sftp: Option<Arc<SftpSession>>,
    transfer_sftp: Option<Arc<SftpSession>>,
}

/// Connection pool opening and reusing one dedicated `Transfer Transport` per `Profile`,
/// with automatic `Primary Transport` SFTP mux fallback when 2FA/interactive auth prevents
/// silent secondary authentication.
#[derive(Default)]
pub struct TransferTransportPool {
    entries: Mutex<HashMap<ProfileTransportKey, PooledTransportEntry>>,
    primary_transports: Mutex<HashMap<ProfileTransportKey, Arc<SshTransport>>>,
    key_locks: Mutex<HashMap<ProfileTransportKey, Arc<AsyncMutex<()>>>>,
    reconnect_notify: Arc<Notify>,
}

impl TransferTransportPool {
    /// Creates an empty [`TransferTransportPool`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a shared [`Notify`] triggered whenever a `Primary Transport` registers/reconnects.
    pub fn reconnect_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.reconnect_notify)
    }

    /// Registers or refreshes the active `Primary Transport` for `key` (used for 2FA mux fallback).
    pub fn register_primary_transport(
        &self,
        key: ProfileTransportKey,
        transport: Arc<SshTransport>,
    ) {
        if let Ok(mut primaries) = self.primary_transports.lock() {
            primaries.insert(key, transport);
        }
        self.reconnect_notify.notify_waiters();
    }

    /// Removes any registered `Primary Transport` and clears mux-fallback cache entries for `key`.
    pub fn clear_primary_transport(&self, key: &ProfileTransportKey) {
        if let Ok(mut primaries) = self.primary_transports.lock() {
            primaries.remove(key);
        }
        if let Ok(mut entries) = self.entries.lock()
            && let Some(entry) = entries.get(key)
            && entry.mode == SftpTransportMode::MuxFallback
        {
            entries.remove(key);
        }
    }

    /// Invalidates cached `Transfer Transport` / SFTP channels for `key` (e.g., after a drop).
    pub fn invalidate(&self, key: &ProfileTransportKey) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(key);
        }
    }

    /// Returns the most recently observed SFTP transport mode & notice for `key`, if active.
    pub fn status_notice(&self, key: &ProfileTransportKey) -> Option<SftpStatusNotice> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(key)?;
        Some(SftpStatusNotice {
            mode: entry.mode,
            message: entry.notice.clone(),
        })
    }

    fn key_mutex(&self, key: &ProfileTransportKey) -> Arc<AsyncMutex<()>> {
        let mut locks = self.key_locks.lock().expect("key locks mutex poisoned");
        Arc::clone(
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    fn resolve_primary(&self, endpoint: &SftpEndpoint) -> Option<Arc<SshTransport>> {
        endpoint
            .primary_transport
            .as_ref()
            .filter(|t| !t.is_closed())
            .cloned()
            .or_else(|| {
                self.primary_transports
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&endpoint.key).cloned())
                    .filter(|t| !t.is_closed())
            })
    }

    fn try_cached(
        &self,
        key: &ProfileTransportKey,
        role: SftpChannelRole,
        active_primary: Option<&Arc<SshTransport>>,
    ) -> Result<Option<AcquiredSftpSession>, CacheTransportReady> {
        let entries = self.entries.lock().expect("pool entries mutex poisoned");
        let Some(entry) = entries.get(key) else {
            return Ok(None);
        };

        match entry.mode {
            SftpTransportMode::IsolatedTransferTransport => {
                let Some(transport) = entry.transfer_transport.as_ref().filter(|t| !t.is_closed())
                else {
                    return Ok(None);
                };
                let cached_sftp = match role {
                    SftpChannelRole::Browser => entry.browser_sftp.as_ref(),
                    SftpChannelRole::Transfer => entry.transfer_sftp.as_ref(),
                };
                if let Some(session) = cached_sftp {
                    return Ok(Some(AcquiredSftpSession {
                        session: Arc::clone(session),
                        mode: entry.mode,
                        notice: entry.notice.clone(),
                    }));
                }
                Err(CacheTransportReady {
                    transport: Arc::clone(transport),
                    mode: entry.mode,
                    notice: entry.notice.clone(),
                })
            }
            SftpTransportMode::MuxFallback => {
                let transport = active_primary
                    .filter(|t| !t.is_closed())
                    .cloned()
                    .or_else(|| entry.primary_transport.as_ref().filter(|t| !t.is_closed()).cloned());
                let Some(transport) = transport else {
                    return Ok(None);
                };
                let cached_sftp = entry.transfer_sftp.as_ref().or(entry.browser_sftp.as_ref());
                if let Some(session) = cached_sftp {
                    return Ok(Some(AcquiredSftpSession {
                        session: Arc::clone(session),
                        mode: entry.mode,
                        notice: entry.notice.clone(),
                    }));
                }
                Err(CacheTransportReady {
                    transport,
                    mode: entry.mode,
                    notice: entry.notice.clone(),
                })
            }
        }
    }

    /// Acquires (and caches/reuses) an [`AcquiredSftpSession`] for `endpoint` and `role`.
    pub async fn acquire(
        &self,
        endpoint: &SftpEndpoint,
        role: SftpChannelRole,
    ) -> Result<AcquiredSftpSession, SshError> {
        let key_lock = self.key_mutex(&endpoint.key);
        let _guard = key_lock.lock().await;

        let active_primary = self.resolve_primary(endpoint);

        match self.try_cached(&endpoint.key, role, active_primary.as_ref()) {
            Ok(Some(acquired)) => return Ok(acquired),
            Err(ready) => {
                match open_sftp_session(&ready.transport).await {
                    Ok(session) => {
                        let sftp = Arc::new(session);
                        let mut entries = self.entries.lock().expect("pool entries mutex poisoned");
                        if let Some(entry) = entries.get_mut(&endpoint.key) {
                            match ready.mode {
                                SftpTransportMode::IsolatedTransferTransport => match role {
                                    SftpChannelRole::Browser => {
                                        entry.browser_sftp = Some(Arc::clone(&sftp))
                                    }
                                    SftpChannelRole::Transfer => {
                                        entry.transfer_sftp = Some(Arc::clone(&sftp))
                                    }
                                },
                                SftpTransportMode::MuxFallback => {
                                    entry.browser_sftp = Some(Arc::clone(&sftp));
                                    entry.transfer_sftp = Some(Arc::clone(&sftp));
                                }
                            }
                        }
                        return Ok(AcquiredSftpSession {
                            session: sftp,
                            mode: ready.mode,
                            notice: ready.notice,
                        });
                    }
                    Err(err) => {
                        // Cached transport died before channel open; invalidate and re-establish cleanly.
                        self.invalidate(&endpoint.key);
                        if ready.mode == SftpTransportMode::MuxFallback {
                            return Err(err);
                        }
                    }
                }
            }
            Ok(None) => {}
        }

        // If Primary Transport already required interactive auth (2FA/OTP or prompt),
        // immediately multiplex onto Primary Transport without re-prompting the user.
        if let Some(primary) = active_primary.as_ref()
            && primary.used_interactive_auth()
        {
            return self
                .establish_mux_fallback(
                    &endpoint.key,
                    Arc::clone(primary),
                    "Interactive 2FA/auth active on Primary Transport; using multiplexed SFTP channel.",
                )
                .await;
        }

        let silent_handler: Arc<dyn SshInteractionHandler> =
            Arc::new(SilentTransferInteractionHandler);
        match SshTransport::connect(
            &endpoint.connect_config,
            endpoint.vault.as_deref(),
            silent_handler,
            Arc::clone(&endpoint.agent_connector),
        )
        .await
        {
            Ok(transfer_transport) => {
                let transport = Arc::new(transfer_transport);
                let sftp = Arc::new(open_sftp_session(&transport).await?);
                let (browser_sftp, transfer_sftp) = match role {
                    SftpChannelRole::Browser => (Some(Arc::clone(&sftp)), None),
                    SftpChannelRole::Transfer => (None, Some(Arc::clone(&sftp))),
                };
                let mut entries = self.entries.lock().expect("pool entries mutex poisoned");
                entries.insert(
                    endpoint.key.clone(),
                    PooledTransportEntry {
                        mode: SftpTransportMode::IsolatedTransferTransport,
                        notice: None,
                        transfer_transport: Some(transport),
                        primary_transport: active_primary,
                        browser_sftp,
                        transfer_sftp,
                    },
                );
                Ok(AcquiredSftpSession {
                    session: sftp,
                    mode: SftpTransportMode::IsolatedTransferTransport,
                    notice: None,
                })
            }
            Err(err) if is_silent_auth_blocked(&err) => {
                if let Some(primary) = active_primary {
                    self.establish_mux_fallback(
                        &endpoint.key,
                        primary,
                        "Secondary connection required interactive 2FA/auth; fell back to multiplexed SFTP channel on Primary Transport.",
                    )
                    .await
                } else {
                    Err(err)
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn establish_mux_fallback(
        &self,
        key: &ProfileTransportKey,
        primary: Arc<SshTransport>,
        reason: &str,
    ) -> Result<AcquiredSftpSession, SshError> {
        let sftp = Arc::new(open_sftp_session(&primary).await?);
        let notice = Some(reason.to_string());
        let mut entries = self.entries.lock().expect("pool entries mutex poisoned");
        entries.insert(
            key.clone(),
            PooledTransportEntry {
                mode: SftpTransportMode::MuxFallback,
                notice: notice.clone(),
                transfer_transport: None,
                primary_transport: Some(primary),
                browser_sftp: Some(Arc::clone(&sftp)),
                transfer_sftp: Some(Arc::clone(&sftp)),
            },
        );
        Ok(AcquiredSftpSession {
            session: sftp,
            mode: SftpTransportMode::MuxFallback,
            notice,
        })
    }
}

struct CacheTransportReady {
    transport: Arc<SshTransport>,
    mode: SftpTransportMode,
    notice: Option<String>,
}

fn is_silent_auth_blocked(err: &SshError) -> bool {
    matches!(
        err,
        SshError::AuthenticationFailed { .. }
            | SshError::InteractiveCancelled
            | SshError::KeyPassphraseRequired { .. }
            | SshError::HostKeyRejected { .. }
    )
}

const DEFAULT_SFTP_OP_RETRY_DELAY: Duration = Duration::from_millis(200);
const DEFAULT_SFTP_OP_MAX_RETRIES: u32 = 2;

/// Non-blocking SFTP File Browser & transport coordinator built on [`TransferTransportPool`].
#[derive(Clone)]
pub struct SftpManager {
    pool: Arc<TransferTransportPool>,
    status_tx: Arc<watch::Sender<HashMap<ProfileTransportKey, SftpTransportStatus>>>,
    retry_delay: Duration,
}

impl Default for SftpManager {
    fn default() -> Self {
        Self::with_pool(Arc::new(TransferTransportPool::new()))
    }
}

impl SftpManager {
    /// Creates an [`SftpManager`] owning a fresh [`TransferTransportPool`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an [`SftpManager`] sharing an existing [`TransferTransportPool`].
    pub fn with_pool(pool: Arc<TransferTransportPool>) -> Self {
        let (status_tx, _) = watch::channel(HashMap::new());
        Self {
            pool,
            status_tx: Arc::new(status_tx),
            retry_delay: DEFAULT_SFTP_OP_RETRY_DELAY,
        }
    }

    /// Overrides the retry backoff delay used when an SFTP browser operation encounters a connection drop.
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay.max(Duration::from_millis(1));
        self
    }

    /// Returns the underlying [`TransferTransportPool`].
    pub fn pool(&self) -> &Arc<TransferTransportPool> {
        &self.pool
    }

    /// Subscribes to per-endpoint [`SftpTransportStatus`] transitions.
    pub fn subscribe_status(
        &self,
    ) -> watch::Receiver<HashMap<ProfileTransportKey, SftpTransportStatus>> {
        self.status_tx.subscribe()
    }

    /// Returns the current [`SftpTransportStatus`] for `key`.
    pub fn transport_status(&self, key: &ProfileTransportKey) -> SftpTransportStatus {
        self.status_tx
            .borrow()
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    fn set_transport_status(&self, key: &ProfileTransportKey, status: SftpTransportStatus) {
        self.status_tx.send_if_modified(|map| {
            if map.get(key) == Some(&status) {
                false
            } else {
                map.insert(key.clone(), status);
                true
            }
        });
    }

    /// Records an external reconnect state (e.g., when the primary SSH transport drops).
    pub fn mark_reconnecting(&self, key: &ProfileTransportKey, attempt: u32, reason: String) {
        self.pool.invalidate(key);
        self.set_transport_status(
            key,
            SftpTransportStatus::AutoReconnecting { attempt, reason },
        );
    }

    /// Clears SFTP connection status back to [`SftpTransportStatus::Idle`].
    pub fn mark_disconnected(&self, key: &ProfileTransportKey) {
        self.pool.invalidate(key);
        self.set_transport_status(key, SftpTransportStatus::Idle);
    }

    /// Registers an active `Primary Transport` for automatic 2FA mux fallback.
    pub fn register_primary_transport(
        &self,
        key: ProfileTransportKey,
        transport: Arc<SshTransport>,
    ) {
        self.pool.register_primary_transport(key, transport);
    }

    /// Clears a disconnected `Primary Transport` registration.
    pub fn clear_primary_transport(&self, key: &ProfileTransportKey) {
        self.pool.clear_primary_transport(key);
    }

    /// Returns the active [`SftpStatusNotice`] for `key`, if connected.
    pub fn status_notice(&self, key: &ProfileTransportKey) -> Option<SftpStatusNotice> {
        self.pool.status_notice(key)
    }

    /// Acquires an SFTP session for bulk `Transfer` streaming.
    pub async fn acquire_transfer_session(
        &self,
        endpoint: &SftpEndpoint,
    ) -> Result<AcquiredSftpSession, SshError> {
        self.pool.acquire(endpoint, SftpChannelRole::Transfer).await
    }

    async fn run_browser_with_reconnect<'a, T, F>(
        &'a self,
        endpoint: &'a SftpEndpoint,
        mut op: F,
    ) -> Result<(T, AcquiredSftpSession), SshError>
    where
        F: FnMut(Arc<SftpSession>) -> BoxFuture<'a, Result<T, SshError>>,
    {
        let notifier = self.pool.reconnect_notifier();
        let mut retry: u32 = 0;

        loop {
            let acquired = match self.pool.acquire(endpoint, SftpChannelRole::Browser).await {
                Ok(acq) => acq,
                Err(err) => {
                    if err.is_connection_error() && retry < DEFAULT_SFTP_OP_MAX_RETRIES {
                        retry += 1;
                        self.pool.invalidate(&endpoint.key);
                        self.set_transport_status(
                            &endpoint.key,
                            SftpTransportStatus::AutoReconnecting {
                                attempt: retry,
                                reason: err.to_string(),
                            },
                        );
                        tokio::select! {
                            _ = notifier.notified() => {}
                            _ = tokio::time::sleep(self.retry_delay.saturating_mul(retry)) => {}
                        }
                        continue;
                    }
                    if err.is_connection_error() {
                        self.set_transport_status(
                            &endpoint.key,
                            SftpTransportStatus::Error {
                                reason: err.to_string(),
                            },
                        );
                    }
                    return Err(err);
                }
            };

            let session = Arc::clone(&acquired.session);
            match op(session).await {
                Ok(val) => {
                    let was_recovering = retry > 0
                        || matches!(
                            self.transport_status(&endpoint.key),
                            SftpTransportStatus::AutoReconnecting { .. }
                                | SftpTransportStatus::Error { .. }
                        );
                    let next_status = if was_recovering {
                        SftpTransportStatus::Reconnected {
                            mode: acquired.mode,
                        }
                    } else {
                        SftpTransportStatus::Connected {
                            mode: acquired.mode,
                        }
                    };
                    self.set_transport_status(&endpoint.key, next_status);
                    return Ok((val, acquired));
                }
                Err(err) => {
                    if err.is_connection_error() && retry < DEFAULT_SFTP_OP_MAX_RETRIES {
                        retry += 1;
                        self.pool.invalidate(&endpoint.key);
                        self.set_transport_status(
                            &endpoint.key,
                            SftpTransportStatus::AutoReconnecting {
                                attempt: retry,
                                reason: err.to_string(),
                            },
                        );
                        tokio::select! {
                            _ = notifier.notified() => {}
                            _ = tokio::time::sleep(self.retry_delay.saturating_mul(retry)) => {}
                        }
                        continue;
                    }
                    if err.is_connection_error() {
                        self.set_transport_status(
                            &endpoint.key,
                            SftpTransportStatus::Error {
                                reason: err.to_string(),
                            },
                        );
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Lists `path` (or `"."` if empty), sorting directories first alphabetically.
    pub async fn list_dir(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
    ) -> Result<RemoteListing, SshError> {
        let requested = if path.trim().is_empty() { "." } else { path };
        let ((resolved_path, entries), acquired) = self
            .run_browser_with_reconnect(endpoint, |session| {
                Box::pin(async move {
                    let resolved_path = match session.canonicalize(requested).await {
                        Ok(resolved) => resolved,
                        Err(_) => requested.to_string(),
                    };

                    let read_dir = session.read_dir(resolved_path.as_str()).await?;
                    let mut entries: Vec<RemoteDirEntry> = read_dir
                        .map(|entry| {
                            let name = entry.file_name();
                            let full_path = entry.path();
                            let meta = entry.metadata();
                            RemoteDirEntry::from_metadata(name, full_path, &meta)
                        })
                        .collect();

                    entries.sort_by(|a, b| {
                        b.is_dir()
                            .cmp(&a.is_dir())
                            .then_with(|| {
                                a.name
                                    .to_ascii_lowercase()
                                    .cmp(&b.name.to_ascii_lowercase())
                            })
                            .then_with(|| a.name.cmp(&b.name))
                    });

                    Ok((resolved_path, entries))
                })
            })
            .await?;

        Ok(RemoteListing {
            resolved_path,
            entries,
            transport_mode: acquired.mode,
            notice: acquired.notice,
        })
    }

    /// Resolves `path` to its canonical remote POSIX path.
    pub async fn canonicalize(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
    ) -> Result<String, SshError> {
        let (resolved, _) = self
            .run_browser_with_reconnect(endpoint, |session| {
                Box::pin(async move { Ok(session.canonicalize(path).await?) })
            })
            .await?;
        Ok(resolved)
    }

    /// Queries metadata (`stat`, following symlinks) for `path`.
    pub async fn stat(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
    ) -> Result<RemoteDirEntry, SshError> {
        let (meta, _) = self
            .run_browser_with_reconnect(endpoint, |session| {
                Box::pin(async move { Ok(session.metadata(path).await?) })
            })
            .await?;
        let name = path_leaf_name(path);
        Ok(RemoteDirEntry::from_metadata(name, path.to_string(), &meta))
    }

    /// Queries link metadata (`lstat`, without following symlinks) for `path`.
    pub async fn lstat(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
    ) -> Result<RemoteDirEntry, SshError> {
        let (meta, _) = self
            .run_browser_with_reconnect(endpoint, |session| {
                Box::pin(async move { Ok(session.symlink_metadata(path).await?) })
            })
            .await?;
        let name = path_leaf_name(path);
        Ok(RemoteDirEntry::from_metadata(name, path.to_string(), &meta))
    }

    /// Creates a remote directory at `path`.
    pub async fn mkdir(&self, endpoint: &SftpEndpoint, path: &str) -> Result<(), SshError> {
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move { Ok(session.create_dir(path).await?) })
        })
        .await
        .map(|_| ())
    }

    /// Deletes a remote regular file or symlink at `path`.
    pub async fn remove_file(&self, endpoint: &SftpEndpoint, path: &str) -> Result<(), SshError> {
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move { Ok(session.remove_file(path).await?) })
        })
        .await
        .map(|_| ())
    }

    /// Deletes an empty remote directory at `path`.
    pub async fn remove_dir(&self, endpoint: &SftpEndpoint, path: &str) -> Result<(), SshError> {
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move { Ok(session.remove_dir(path).await?) })
        })
        .await
        .map(|_| ())
    }

    /// Renames or moves a remote file or directory from `old_path` to `new_path`.
    pub async fn rename(
        &self,
        endpoint: &SftpEndpoint,
        old_path: &str,
        new_path: &str,
    ) -> Result<(), SshError> {
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move { Ok(session.rename(old_path, new_path).await?) })
        })
        .await
        .map(|_| ())
    }

    /// Updates POSIX permissions on `path` (`setstat`), used by recursive permission preservation.
    pub async fn set_permissions(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
        permissions: u32,
    ) -> Result<(), SshError> {
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move {
                let attrs = FileAttributes {
                    permissions: Some(permissions),
                    ..Default::default()
                };
                Ok(session.set_metadata(path, attrs).await?)
            })
        })
        .await
        .map(|_| ())
    }

    /// Reads up to `max_bytes` UTF-8 text from remote `path` for the SFTP+ file viewer/editor.
    pub async fn read_text_file(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
        max_bytes: usize,
    ) -> Result<String, SshError> {
        let (content, _) = self
            .run_browser_with_reconnect(endpoint, |session| {
                Box::pin(async move {
                    let mut remote_file = session.open_with_flags(path, OpenFlags::READ).await?;
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 32 * 1024];
                    loop {
                        let n = remote_file.read(&mut chunk).await?;
                        if n == 0 {
                            break;
                        }
                        if buf.len() + n > max_bytes {
                            return Err(SshError::Sftp(format!(
                                "File exceeds {max_bytes} byte viewer limit; download instead."
                            )));
                        }
                        if chunk[..n].contains(&0) {
                            return Err(SshError::Sftp(
                                "Binary file detected (contains NUL bytes); download to inspect."
                                    .to_string(),
                            ));
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    String::from_utf8(buf).map_err(|_| {
                        SshError::Sftp(
                            "File is not valid UTF-8 text; download to inspect.".to_string(),
                        )
                    })
                })
            })
            .await?;
        Ok(content)
    }

    /// Writes `content` UTF-8 bytes back to remote `path` (creating or truncating).
    pub async fn write_text_file(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
        content: &str,
    ) -> Result<(), SshError> {
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move {
                let mut remote_file = session
                    .open_with_flags(
                        path,
                        OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
                    )
                    .await?;
                remote_file.write_all(content.as_bytes()).await?;
                remote_file.flush().await?;
                remote_file.shutdown().await?;
                Ok(())
            })
        })
        .await
        .map(|_| ())
    }

    /// Creates an empty regular file at `path` if it does not already exist.
    pub async fn create_empty_file(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
    ) -> Result<(), SshError> {
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move {
                if session.symlink_metadata(path).await.is_ok() {
                    return Err(SshError::Sftp(format!("Target already exists: {path}")));
                }
                let mut remote_file = session
                    .open_with_flags(path, OpenFlags::CREATE | OpenFlags::WRITE)
                    .await?;
                remote_file.flush().await?;
                remote_file.shutdown().await?;
                Ok(())
            })
        })
        .await
        .map(|_| ())
    }

    /// Copies a regular remote file at `src_path` to `dst_path` on the same SFTP endpoint.
    pub async fn copy_file(
        &self,
        endpoint: &SftpEndpoint,
        src_path: &str,
        dst_path: &str,
    ) -> Result<(), SshError> {
        if src_path == dst_path {
            return Ok(());
        }
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move {
                let mut src = session.open_with_flags(src_path, OpenFlags::READ).await?;
                let mut dst = session
                    .open_with_flags(
                        dst_path,
                        OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
                    )
                    .await?;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = src.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    dst.write_all(&buf[..n]).await?;
                }
                dst.flush().await?;
                dst.shutdown().await?;
                Ok(())
            })
        })
        .await
        .map(|_| ())
    }

    /// Deletes a remote directory and all nested contents safely without following symlinks.
    pub async fn remove_dir_recursive(
        &self,
        endpoint: &SftpEndpoint,
        path: &str,
    ) -> Result<(), SshError> {
        let clean = path.trim_end_matches('/');
        if clean.is_empty() || path == "/" {
            return Err(SshError::Sftp("refusing to recursively remove '/'".to_string()));
        }
        self.run_browser_with_reconnect(endpoint, |session| {
            Box::pin(async move {
                let mut pending = vec![path.to_string()];
                let mut dirs_bottom_up = Vec::new();
                while let Some(dir) = pending.pop() {
                    dirs_bottom_up.push(dir.clone());
                    let entries = session.read_dir(dir.as_str()).await?;
                    for entry in entries {
                        let name = entry.file_name();
                        if name == "." || name == ".." {
                            continue;
                        }
                        let child_path = entry.path();
                        if entry.metadata().file_type() == FileType::Dir {
                            pending.push(child_path);
                        } else {
                            session.remove_file(child_path.as_str()).await?;
                        }
                    }
                }
                while let Some(dir) = dirs_bottom_up.pop() {
                    session.remove_dir(dir.as_str()).await?;
                }
                Ok(())
            })
        })
        .await
        .map(|_| ())
    }
}

fn path_leaf_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    trimmed
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

/// Validates a single remote SFTP entry name so it cannot traverse or escape a local target directory.
pub fn validate_remote_entry_name(name: &str) -> Result<(), SshError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
    {
        return Err(SshError::Sftp(format!(
            "unsafe remote SFTP entry name {name:?}"
        )));
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(()),
        _ => Err(SshError::Sftp(format!(
            "unsafe remote SFTP entry name {name:?}"
        ))),
    }
}

/// Joins a remote POSIX directory path and a child file/directory name cleanly.
pub fn join_remote_path(dir: &str, name: &str) -> String {
    let clean_name = name.trim_start_matches('/');
    if dir.is_empty() || dir == "/" {
        format!("/{clean_name}")
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), clean_name)
    }
}

/// Returns the parent POSIX path for `path` (clamping at `"/"`).
pub fn parent_remote_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
        None => "/".to_string(),
    }
}
