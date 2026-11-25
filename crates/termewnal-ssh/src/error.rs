use std::path::PathBuf;

use thiserror::Error;

/// Errors returned by SSH `Transport`, `Tunnel`, and SFTP `Transfer` operations.
#[derive(Debug, Error)]
pub enum SshError {
    /// Feature scheduled for a later implementation ticket.
    #[error("SSH transport operation not implemented yet ({0})")]
    NotImplemented(&'static str),

    /// Remote host key differed from the key recorded in `known_hosts` (possible MITM attack).
    #[error(
        "HOST KEY VERIFICATION FAILED for {host}:{port} (known_hosts line {line}): expected {expected_fingerprint}, received {received_fingerprint}"
    )]
    HostKeyMismatch {
        host: String,
        port: u16,
        line: usize,
        key_type: String,
        expected_fingerprint: String,
        received_fingerprint: String,
    },

    /// User rejected an unknown host key during Trust On First Use (TOFU) verification.
    #[error("SSH host key for {host}:{port} ({fingerprint}) was rejected by the user")]
    HostKeyRejected {
        host: String,
        port: u16,
        fingerprint: String,
    },

    /// Failed to resolve, read, parse, or update `known_hosts`.
    #[error("known_hosts error for {path}: {reason}")]
    KnownHosts { path: PathBuf, reason: String },

    /// Failed to read or decode a local private key file.
    #[error("failed to load private key from {path}: {reason}")]
    KeyLoad { path: PathBuf, reason: String },

    /// Encrypted private key requires a passphrase, and none was provided.
    #[error("passphrase required for encrypted private key {path}")]
    KeyPassphraseRequired { path: PathBuf },

    /// SSH Agent connection or signing error.
    #[error("SSH agent error: {0}")]
    Agent(String),

    /// Server rejected authentication credentials or no configured method succeeded.
    #[error("authentication failed for {username} ({method}): {detail}")]
    AuthenticationFailed {
        username: String,
        method: &'static str,
        detail: String,
    },

    /// User cancelled an interactive authentication or TOFU prompt.
    #[error("interactive SSH authentication cancelled by user")]
    InteractiveCancelled,

    /// Error reading credentials from `Vault`.
    #[error("Vault error: {0}")]
    Vault(#[from] termewnal_core::VaultError),

    /// TCP/SSH handshake or transport connection failure.
    #[error("SSH connection error to {host}:{port}: {reason}")]
    Connection {
        host: String,
        port: u16,
        reason: String,
    },

    /// Session channel, PTY request, or shell startup failure.
    #[error("SSH channel error: {0}")]
    Channel(String),

    /// `Tunnel` listener attempted to bind a non-loopback address without explicit opt-in.
    #[error(
        "refusing to bind tunnel '{tunnel_name}' on public address {bind_host}:{bind_port} without explicit opt-in"
    )]
    TunnelPublicBindDenied {
        tunnel_name: String,
        bind_host: String,
        bind_port: u16,
    },

    /// Invalid `Tunnel` rule configuration (e.g. missing target host/port).
    #[error("invalid tunnel configuration for '{tunnel_name}': {reason}")]
    TunnelConfig { tunnel_name: String, reason: String },

    /// SFTP protocol or remote filesystem operation error.
    #[error("SFTP error: {0}")]
    Sftp(String),

    /// Remote SFTP server returned an explicit `SSH_FXP_STATUS` failure.
    #[error("SFTP status {status_code:?}: {message}")]
    SftpStatus {
        status_code: russh_sftp::protocol::StatusCode,
        message: String,
    },

    /// `Session` or `Transport` is disconnected.
    #[error("SSH session is disconnected")]
    Disconnected,

    /// `Transfer Transport` or SFTP stream dropped unexpectedly mid-transfer.
    #[error("transfer transport dropped: {0}")]
    TransferTransportDropped(String),

    /// Underlying `russh` protocol error.
    #[error("SSH protocol error: {0}")]
    Russh(#[from] russh::Error),

    /// Underlying `russh::keys` error.
    #[error("SSH key error: {0}")]
    RusshKey(#[from] russh::keys::Error),

    /// Local filesystem or socket I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

fn is_sftp_client_transport_drop(err: &russh_sftp::client::error::Error) -> bool {
    match err {
        russh_sftp::client::error::Error::IO(_) | russh_sftp::client::error::Error::Timeout => true,
        russh_sftp::client::error::Error::UnexpectedBehavior(msg) => {
            let lower = msg.to_ascii_lowercase();
            lower.contains("sender dropped")
                || lower.contains("senderror")
                || lower.contains("recverror")
                || lower.contains("eof")
                || lower.contains("closed")
                || lower.contains("reset")
                || lower.contains("broken pipe")
        }
        _ => false,
    }
}

impl From<russh_sftp::client::error::Error> for SshError {
    fn from(err: russh_sftp::client::error::Error) -> Self {
        match err {
            russh_sftp::client::error::Error::Status(status) => Self::SftpStatus {
                status_code: status.status_code,
                message: status.error_message,
            },
            other if is_sftp_client_transport_drop(&other) => {
                Self::TransferTransportDropped(other.to_string())
            }
            other => Self::Sftp(other.to_string()),
        }
    }
}

/// Classifies an `io::Error` returned by remote `russh_sftp::client::fs::File` read/write/close
/// operations so remote transport drops are cleanly distinguished from local filesystem `Io` errors.
pub(crate) fn map_remote_sftp_io_error(err: std::io::Error) -> SshError {
    if let Some(sftp_err) = err
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<russh_sftp::client::error::Error>())
    {
        return match sftp_err {
            russh_sftp::client::error::Error::Status(status) => SshError::SftpStatus {
                status_code: status.status_code,
                message: status.error_message.clone(),
            },
            other if is_sftp_client_transport_drop(other) => {
                SshError::TransferTransportDropped(other.to_string())
            }
            other => SshError::Sftp(other.to_string()),
        };
    }
    match err.kind() {
        std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::TimedOut => SshError::TransferTransportDropped(err.to_string()),
        _ => SshError::Sftp(err.to_string()),
    }
}
