//! Core configuration, profile schema, and persistence primitives for Termewnal.

pub mod config;
pub mod error;
pub mod hot_reload;
pub mod layout;
pub mod openssh_config;
pub mod profile;
mod storage;
pub mod tabby_importer;
pub mod theme;
pub mod vault;

pub use config::{
    AppConfig, CURRENT_SCHEMA_VERSION, ConfigPaths, CursorStyle,
    DEFAULT_KEEPALIVE_INTERVAL_SECS, DEFAULT_KEEPALIVE_MAX_MISSED, TerminalConfig, VaultBackend,
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
pub use openssh_config::{
    OpenSshConfigError, OpenSshHost, clone_openssh_host_to_profile, load_openssh_hosts_or_empty,
    load_system_ssh_hosts, parse_openssh_config_file, save_cloned_openssh_profile,
};
pub use profile::{
    Profile, ProfileTarget, ProfilesConfig, SshAuthMethod, SshProfileConfig, TunnelDirection,
    TunnelRule, load_profiles, resolve_mux_attach_command, save_profiles,
};
pub use storage::{MAX_BACKUPS, backup_path};
pub use tabby_importer::{
    TabbyCredentialKind, TabbyCredentialRequirement, TabbyImportError, TabbyImportOutcome,
    TabbyImportReport, TabbyVaultStatus, default_tabby_config_path, import_tabby_config_file,
    import_tabby_config_file_into_profiles, import_tabby_into_profiles, parse_tabby_config_str,
};
pub use theme::{BuiltinTheme, RgbColor, ThemePalette};
pub use vault::{
    ClockSample, DEFAULT_KEYRING_SERVICE, EncryptedVault, InMemorySecretStore, KeyringSecretStore,
    ManualVaultClock, SecretStore, SystemVaultClock, VaultClock, VaultError, VaultKdfParams,
    VaultSession,
};
