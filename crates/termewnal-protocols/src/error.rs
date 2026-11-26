//! Shared error types for native non-SSH protocol engines (`Serial`, `Telnet`, `Docker`, `Zmodem`).

use thiserror::Error;

/// Unified error type returned by `termewnal-protocols` sessions and engines.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serial error: {0}")]
    Serial(String),

    #[error("telnet error: {0}")]
    Telnet(String),

    #[error("container error: {0}")]
    Docker(String),

    #[error("Zmodem protocol error: {0}")]
    Zmodem(String),

    #[error("invalid protocol input: {0}")]
    InvalidInput(String),

    #[error("protocol session disconnected")]
    Disconnected,

    #[error("operation cancelled")]
    Cancelled,
}
