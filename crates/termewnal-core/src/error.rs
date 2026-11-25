use std::path::PathBuf;
use thiserror::Error;

/// Errors returned by configuration, profile, and core persistence operations.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Failed to resolve standard OS XDG directories.
    #[error("unable to resolve XDG directories for Termewnal")]
    MissingXdgDirectories,

    /// Filesystem I/O failure while reading, writing, syncing, or rotating files.
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Failed to serialize configuration or profiles into TOML.
    #[error("failed to serialize {path} to TOML: {source}")]
    TomlSerialize {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },

    /// Failed to parse TOML configuration or profiles.
    #[error("failed to parse TOML in {path}: {message}")]
    TomlDeserialize {
        path: PathBuf,
        message: String,
        span: Option<std::ops::Range<usize>>,
    },

    /// Configuration file declares an unsupported `schema_version`.
    #[error("unsupported schema_version {found} in {path} (expected {expected})")]
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
}
