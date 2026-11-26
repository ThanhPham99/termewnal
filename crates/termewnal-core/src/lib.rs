//! Core configuration, profile schema, and persistence primitives for Termewnal.

pub mod collections;
pub mod config;
pub mod error;
pub mod hot_reload;
pub mod keybindings;
pub mod layout;
pub mod openssh_config;
pub mod profile;
pub mod sftp_state;
mod storage;
pub mod sync;
pub mod tabby_importer;
pub mod theme;
pub mod vault;

pub use collections::{
    QuickCommand, QuickCommandsConfig, WorkspaceTemplate, WorkspacesConfig, load_quick_commands,
    load_workspaces, save_quick_commands, save_workspaces,
};
pub use config::{
    AppConfig, BellBehavior, CURRENT_SCHEMA_VERSION, ConfigPaths, CursorStyle,
    DEFAULT_KEEPALIVE_INTERVAL_SECS, DEFAULT_KEEPALIVE_MAX_MISSED, DEFAULT_PART_THRESHOLD_BYTES,
    DEFAULT_SFTP_PANEL_WIDTH, DEFAULT_SIDEBAR_WIDTH, HighlightRule, MAX_SFTP_PANEL_WIDTH,
    MAX_SIDEBAR_WIDTH, MIN_SFTP_PANEL_WIDTH, MIN_SIDEBAR_WIDTH, QuakeConfig, QuakeEdge,
    RightClickAction, SyncConfig, SyncProviderKind, TabBarPosition, TerminalConfig, TransferConfig,
    TransferConflictPolicy, TrayConfig, VaultBackend, VaultConfig, WindowConfig,
    ensure_config_files, load_config, save_config,
};
pub use error::CoreError;
pub use hot_reload::{
    ConfigFileKind, ConfigReloader, ConfigWatchEvent, ConfigWatcher, DEFAULT_RELOAD_DEBOUNCE,
    RestoredSnapshot, TomlReloadError, reload_config_snapshot, reload_profiles_snapshot,
    restore_from_backup,
};
pub use keybindings::{
    ChordMatch, KeyAction, KeyChord, KeySequence, KeybindingConfig, KeybindingResolver,
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
    DEFAULT_TUNNEL_HOLD_GRACE_SECS, DockerRuntime, LoginScriptStep, NewlineMode, Profile,
    ProfileTarget, ProfilesConfig, SerialFlowControl, SerialInputMode, SerialOutputMode,
    SerialParity, SshAuthMethod, SshProfileConfig, SshProxyConfig, SshProxyKind, TunnelDirection,
    TunnelRule, is_public_bind_host, load_profiles, resolve_mux_attach_command, save_profiles,
};
pub use sftp_state::{SftpState, load_sftp_state, save_sftp_state};
pub use storage::{MAX_BACKUPS, backup_path};
pub use sync::{
    ConfigSyncService, ReqwestSyncHttpClient, SYNC_BUNDLE_SCHEMA_VERSION, SYNC_FILE_NAME,
    SyncApplyReport, SyncBundle, SyncEnvelope, SyncHttpClient, SyncHttpMethod, SyncHttpRequest,
    SyncHttpResponse, SyncPullOutcome, SyncPushOutcome, decode_sync_envelope, encode_sync_envelope,
    encode_sync_envelope_with_kdf,
};
pub use tabby_importer::{
    TabbyCredentialKind, TabbyCredentialRequirement, TabbyImportError, TabbyImportOutcome,
    TabbyImportReport, TabbyVaultStatus, default_tabby_config_path, import_tabby_config_file,
    import_tabby_config_file_into_profiles, import_tabby_into_profiles, parse_tabby_config_str,
};
pub use theme::{BuiltinTheme, RgbColor, ThemeOverrides, ThemePalette};
pub use vault::{
    ClockSample, DEFAULT_KEYRING_SERVICE, EncryptedVault, InMemorySecretStore, KeyringSecretStore,
    ManualVaultClock, SecretStore, SystemVaultClock, VaultClock, VaultError, VaultKdfParams,
    VaultSession,
};
