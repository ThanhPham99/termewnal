//! SSH client, authentication, strict `known_hosts` verification, tunnel, and SFTP engine.

pub mod auth;
pub mod client;
mod dir_transfer;
pub mod error;
pub mod known_hosts;
pub mod session;
pub mod sftp;
pub mod supervisor;
pub mod transfer;

pub use auth::{
    AgentConnector, AuthOutcome, BoxFuture, BoxedAgentClient, HostKeyDecision, HostKeyPrompt,
    InteractivePromptItem, KeyboardInteractiveChallenge, OsAgentConnector,
    RejectAllInteractionHandler, SshInteractionHandler, authenticate_session,
};
pub use client::{SshClientHandler, SshConnectConfig, SshTransport, TcpKeepaliveSettings};
pub use error::SshError;
pub use known_hosts::{
    KnownHostStatus, default_known_hosts_path, fingerprint_sha256, record_known_host,
    record_known_host_path, verify_known_host, verify_known_host_path,
};
pub use session::SshSession;
pub use sftp::{
    AcquiredSftpSession, DEFAULT_SFTP_MAX_CONCURRENT_READS, DEFAULT_SFTP_MAX_CONCURRENT_WRITES,
    DEFAULT_SFTP_MAX_PACKET_LEN, DEFAULT_SFTP_MAX_WRITE_PACKET_LEN,
    DEFAULT_SFTP_REQUEST_TIMEOUT_SECS, ProfileTransportKey, RemoteDirEntry, RemoteEntryKind,
    RemoteListing, SftpChannelRole, SftpEndpoint, SftpManager, SftpStatusNotice, SftpTransportMode,
    TransferTransportPool, join_remote_path, open_sftp_session, parent_remote_path,
    pipelined_sftp_client_config,
};
pub use supervisor::{
    ConnectionSupervisor, DEFAULT_BACKOFF_MAX, DEFAULT_BACKOFF_MIN, DEFAULT_WAKE_PROBE_TIMEOUT,
    DefaultTransportConnector, ManualTimeSource, SupervisorConfig, SupervisorEvent,
    SupervisorInstant, SystemTimeSource, TimeSource, TransportConnector, TransportReadySnapshot,
    TransportStatusEvent, compute_reconnect_backoff, format_reconnected_divider,
};
pub use transfer::{
    ConflictCheck, ConflictResolution, DEFAULT_MAX_CONCURRENT_TRANSFERS,
    DEFAULT_MAX_TRANSPORT_RETRIES, DEFAULT_RECONNECT_RETRY_DELAY, DEFAULT_TRANSFER_CHUNK_SIZE,
    DirectTransferHook, DirectoryTransferSummary, DownloadTargetPlan, ResilientTransferHook,
    TransferConflict, TransferDirection, TransferExecutorHook, TransferId, TransferNotice,
    TransferQueue, TransferQueueConfig, TransferQueueSnapshot, TransferSnapshot, TransferStatus,
    UploadPlanOptions, UploadTargetPlan, disambiguate_filename, part_path_for,
};

/// Connection lifecycle state of an SSH `Transport`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TransportStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected {
        latency_ms: Option<u64>,
    },
    Degraded {
        missed_probes: u8,
    },
    Reconnecting {
        attempt: u32,
        next_retry_secs: u64,
    },
}

impl TransportStatus {
    /// Returns a concise human-readable status label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Connected { .. } => "Connected",
            Self::Degraded { .. } => "Degraded",
            Self::Reconnecting { .. } => "Reconnecting",
        }
    }
}
