//! SSH, tunnel, and SFTP transport engine (stub crate for Ticket 01).

use thiserror::Error;

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

/// Errors returned by SSH `Transport`, `Tunnel`, and SFTP `Transfer` operations.
#[derive(Debug, Error)]
pub enum SshError {
    /// Feature scheduled for a later implementation ticket.
    #[error("SSH transport operation not implemented yet ({0})")]
    NotImplemented(&'static str),
}
