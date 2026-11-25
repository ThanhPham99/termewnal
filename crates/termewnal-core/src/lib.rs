//! Core configuration, profile schema, and persistence primitives for Termewnal.

pub mod config;
pub mod error;
pub mod profile;
mod storage;
pub mod theme;

pub use config::{
    AppConfig, CURRENT_SCHEMA_VERSION, ConfigPaths, CursorStyle, TerminalConfig, WindowConfig,
    ensure_config_files, load_config, save_config,
};
pub use error::CoreError;
pub use profile::{
    Profile, ProfileTarget, ProfilesConfig, SshAuthMethod, TunnelDirection, TunnelRule,
    load_profiles, save_profiles,
};
pub use storage::{MAX_BACKUPS, backup_path};
pub use theme::{BuiltinTheme, RgbColor, ThemePalette};
