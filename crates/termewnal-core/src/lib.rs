//! Core configuration, profile schema, and persistence primitives for Termewnal.

pub mod config;
pub mod error;
pub mod hot_reload;
pub mod layout;
pub mod profile;
mod storage;
pub mod theme;
pub mod vault;

pub use config::{
    AppConfig, CURRENT_SCHEMA_VERSION, ConfigPaths, CursorStyle, TerminalConfig, VaultBackend,
    VaultConfig, WindowConfig, ensure_config_files, load_config, save_config,
};
pub use error::CoreError;
pub use hot_reload::{
    ConfigFileKind, ConfigReloader, ConfigWatchEvent, ConfigWatcher, DEFAULT_RELOAD_DEBOUNCE,
    RestoredSnapshot, TomlReloadError, reload_config_snapshot, reload_profiles_snapshot,
    restore_from_backup,
};
pub use layout::{
    LayoutSplitAxis, LayoutSplitNode, SessionDescriptor, TabLayout, WindowLayout, WorkspaceLayout,
    load_layout, save_layout,
};
pub use profile::{
    Profile, ProfileTarget, ProfilesConfig, SshAuthMethod, TunnelDirection, TunnelRule,
    load_profiles, save_profiles,
};
pub use storage::{MAX_BACKUPS, backup_path};
pub use theme::{BuiltinTheme, RgbColor, ThemePalette};
pub use vault::{
    ClockSample, DEFAULT_KEYRING_SERVICE, EncryptedVault, InMemorySecretStore, KeyringSecretStore,
    ManualVaultClock, SecretStore, SystemVaultClock, VaultClock, VaultError, VaultKdfParams,
    VaultSession,
};
