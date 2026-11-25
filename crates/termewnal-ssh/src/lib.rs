//! SSH client, authentication, strict `known_hosts` verification, tunnel, and SFTP engine.

pub mod auth;
pub mod client;
pub mod error;
pub mod known_hosts;
pub mod session;

pub use auth::{
    AgentConnector, AuthOutcome, BoxFuture, BoxedAgentClient, HostKeyDecision, HostKeyPrompt,
    InteractivePromptItem, KeyboardInteractiveChallenge, OsAgentConnector,
    RejectAllInteractionHandler, SshInteractionHandler, authenticate_session,
};
pub use client::{SshClientHandler, SshConnectConfig, SshTransport};
pub use error::SshError;
pub use known_hosts::{
    KnownHostStatus, default_known_hosts_path, fingerprint_sha256, record_known_host,
    record_known_host_path, verify_known_host, verify_known_host_path,
};
pub use session::SshSession;

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
