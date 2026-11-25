use thiserror::Error;

/// Errors raised during local shell detection, PTY creation, I/O, or resizing.
#[derive(Debug, Error)]
pub enum PtyError {
    /// Failed to allocate or open a native OS PTY pair.
    #[error("failed to open PTY: {0}")]
    OpenPty(String),

    /// Failed to spawn the shell process attached to the slave PTY.
    #[error("failed to spawn shell '{program}': {reason}")]
    SpawnShell { program: String, reason: String },

    /// Failed to resize the PTY device.
    #[error("failed to resize PTY to {cols}x{rows}: {reason}")]
    Resize {
        cols: u16,
        rows: u16,
        reason: String,
    },

    /// Underlying I/O failure writing to the PTY master.
    #[error("PTY I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Attempted to write after the PTY writer was closed.
    #[error("PTY writer is closed")]
    WriterClosed,
}
