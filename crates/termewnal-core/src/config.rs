use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::keybindings::KeybindingConfig;
use crate::profile::{ProfilesConfig, load_profiles, save_profiles};
use crate::storage::{load_toml_or_default, save_toml_atomic};
use crate::theme::ThemeOverrides;

/// Current schema version written to `config.toml` and `profiles.toml`.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Default width in logical pixels for the Left Sidebar.
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 250.0;
/// Minimum width in logical pixels when resizing the Left Sidebar.
pub const MIN_SIDEBAR_WIDTH: f32 = 180.0;
/// Maximum width in logical pixels when resizing the Left Sidebar.
pub const MAX_SIDEBAR_WIDTH: f32 = 480.0;

/// Default width in logical pixels for a docked SFTP Panel (`SFTP+`).
pub const DEFAULT_SFTP_PANEL_WIDTH: f32 = 360.0;
/// Minimum width in logical pixels when resizing an SFTP Panel (`SFTP+`).
pub const MIN_SFTP_PANEL_WIDTH: f32 = 220.0;
/// Maximum width in logical pixels when resizing an SFTP Panel (`SFTP+`).
pub const MAX_SFTP_PANEL_WIDTH: f32 = 960.0;

/// Default SSH keepalive probe interval in seconds (`15s`).
pub const DEFAULT_KEEPALIVE_INTERVAL_SECS: u64 = 15;

/// Default maximum consecutive missed SSH keepalive probes (`3`, ~45s timeout).
pub const DEFAULT_KEEPALIVE_MAX_MISSED: u32 = 3;

/// Default size threshold in bytes (`1 MiB`) above which SFTP uploads use `.termewnal.part`.
pub const DEFAULT_PART_THRESHOLD_BYTES: u64 = 1_048_576;

const CONFIG_FILE_NAME: &str = "config.toml";
const PROFILES_FILE_NAME: &str = "profiles.toml";
const LAYOUT_FILE_NAME: &str = "layout.toml";
const SFTP_STATE_FILE_NAME: &str = "sftp_state.toml";
const QUICK_COMMANDS_FILE_NAME: &str = "quick_commands.toml";
const WORKSPACES_FILE_NAME: &str = "workspaces.toml";
const IPC_SOCKET_FILE_NAME: &str = "termewnal.sock";
const VAULT_FILE_NAME: &str = "vault.enc";

/// Standard XDG/OS directory locations for Termewnal configuration and runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    config_dir: PathBuf,
    data_local_dir: PathBuf,
    cache_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    download_dir: PathBuf,
}

impl ConfigPaths {
    /// Resolves OS-standard XDG directories via `directories::ProjectDirs`.
    pub fn detect_xdg() -> Result<Self, CoreError> {
        let proj = ProjectDirs::from("io", "termewnal", "Termewnal")
            .ok_or(CoreError::MissingXdgDirectories)?;
        let home_dir = BaseDirs::new()
            .map(|base| base.home_dir().to_path_buf())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            });
        let data_local_dir = proj.data_local_dir().to_path_buf();
        let download_dir = UserDirs::new()
            .and_then(|u| u.download_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| data_local_dir.join("downloads"));
        Ok(Self {
            config_dir: proj.config_dir().to_path_buf(),
            data_local_dir,
            cache_dir: proj.cache_dir().to_path_buf(),
            runtime_dir: proj.runtime_dir().map(Path::to_path_buf),
            home_dir,
            download_dir,
        })
    }

    /// Creates isolated directory paths rooted under `root`, intended for tests or portable mode.
    pub fn from_root(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config_dir: root.join("config"),
            data_local_dir: root.join("data"),
            cache_dir: root.join("cache"),
            runtime_dir: Some(root.join("runtime")),
            home_dir: Some(root.to_path_buf()),
            download_dir: root.join("downloads"),
        }
    }

    /// Default OS downloads directory (or isolated `downloads` directory under test/portable root).
    pub fn sftp_download_dir(&self) -> &Path {
        &self.download_dir
    }

    /// Optional user home directory used for read-only `~/.ssh/config` discovery.
    pub fn home_dir(&self) -> Option<&Path> {
        self.home_dir.as_deref()
    }

    /// Overrides the user home directory used for read-only `~/.ssh/config` discovery.
    pub fn with_home_dir(mut self, home_dir: impl Into<PathBuf>) -> Self {
        self.home_dir = Some(home_dir.into());
        self
    }

    /// Path to the read-only OpenSSH config file (`<home>/.ssh/config`), if a home directory is known.
    pub fn ssh_config_file(&self) -> Option<PathBuf> {
        self.home_dir
            .as_ref()
            .map(|home| home.join(".ssh").join("config"))
    }

    /// Directory containing `config.toml`, `profiles.toml`, and their rotating backups.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Directory for local persistent application data.
    pub fn data_local_dir(&self) -> &Path {
        &self.data_local_dir
    }

    /// Directory for cached non-critical state.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Optional runtime directory for sockets and ephemeral state.
    pub fn runtime_dir(&self) -> Option<&Path> {
        self.runtime_dir.as_deref()
    }

    /// Path to `config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE_NAME)
    }

    /// Path to `profiles.toml`.
    pub fn profiles_file(&self) -> PathBuf {
        self.config_dir.join(PROFILES_FILE_NAME)
    }

    /// Path to `layout.toml` storing persisted workspace windows, tabs, and split geometry.
    pub fn layout_file(&self) -> PathBuf {
        self.data_local_dir.join(LAYOUT_FILE_NAME)
    }

    /// Path to `sftp_state.toml` storing remembered per-profile SFTP working directories.
    pub fn sftp_state_file(&self) -> PathBuf {
        self.data_local_dir.join(SFTP_STATE_FILE_NAME)
    }

    /// Path to `quick_commands.toml` storing reusable `QuickCommand` snippets.
    pub fn quick_commands_file(&self) -> PathBuf {
        self.config_dir.join(QUICK_COMMANDS_FILE_NAME)
    }

    /// Path to `workspaces.toml` storing named `WorkspaceTemplate` presets.
    pub fn workspaces_file(&self) -> PathBuf {
        self.config_dir.join(WORKSPACES_FILE_NAME)
    }

    /// Directory where per-session output logs are saved by default.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_local_dir.join("logs")
    }

    /// Path to the single-instance local IPC socket (`runtime_dir` with `data_local_dir` fallback).
    pub fn ipc_socket_file(&self) -> PathBuf {
        self.runtime_dir
            .as_deref()
            .unwrap_or(&self.data_local_dir)
            .join(IPC_SOCKET_FILE_NAME)
    }

    /// Path to the optional local encrypted `Vault` file under `data_local_dir()`.
    pub fn vault_file(&self) -> PathBuf {
        self.data_local_dir.join(VAULT_FILE_NAME)
    }
}

/// Terminal cursor visual shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    #[default]
    Block,
    Underline,
    Beam,
}

/// Edge where the workspace tab bar is docked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabBarPosition {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

/// Behavior when right-clicking inside a non-mouse-mode terminal pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RightClickAction {
    #[default]
    Menu,
    Paste,
    Off,
}

/// Action taken when a terminal emits BEL (`\x07`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BellBehavior {
    Visual,
    Notification,
    #[default]
    VisualAndNotification,
    Off,
}

/// Regex-based terminal output `HighlightRule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRule {
    pub id: String,
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub foreground: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub case_insensitive: bool,
}

/// Screen edge where the Quake drop-down window anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuakeEdge {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

/// Quake drop-down console configuration stored in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuakeConfig {
    pub enabled: bool,
    pub edge: QuakeEdge,
    pub size_percent: u8,
    pub hotkey: String,
    pub hide_on_blur: bool,
}

impl Default for QuakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            edge: QuakeEdge::Top,
            size_percent: 45,
            hotkey: "Ctrl+Alt+Space".to_string(),
            hide_on_blur: false,
        }
    }
}

/// System Tray icon and background behavior stored in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrayConfig {
    pub enabled: bool,
    pub minimize_to_tray: bool,
    pub show_notifications: bool,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            minimize_to_tray: true,
            show_notifications: true,
        }
    }
}

/// Supported Config Sync backend providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncProviderKind {
    #[default]
    None,
    LocalDirectory,
    WebDav,
    GitHubGist,
}

/// Non-secret Config Sync settings stored in `config.toml` (tokens live in `Vault`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub provider: SyncProviderKind,
    pub local_dir: Option<PathBuf>,
    pub gist_id: Option<String>,
    pub webdav_url: Option<String>,
    pub webdav_username: Option<String>,
    pub vault_token_ref: Option<String>,
    pub encrypt_bundle: bool,
    pub auto_sync_on_save: bool,
    pub last_sync_at: Option<String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            provider: SyncProviderKind::None,
            local_dir: None,
            gist_id: None,
            webdav_url: None,
            webdav_username: None,
            vault_token_ref: None,
            encrypt_bundle: false,
            auto_sync_on_save: false,
            last_sync_at: None,
        }
    }
}

/// Terminal emulation and rendering preferences stored in `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Optional explicit shell executable path; `None` auto-detects the OS default shell.
    pub shell: Option<String>,
    /// Number of scrollback lines retained per terminal `Session`.
    pub scrollback_lines: usize,
    /// Monospace font family name.
    pub font_family: String,
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Enable OpenType font ligatures during terminal text shaping.
    #[serde(default)]
    pub font_ligatures: bool,
    /// Terminal cursor shape.
    pub cursor_style: CursorStyle,
    /// Enable blinking cursor in active panes.
    #[serde(default)]
    pub cursor_blink: bool,
    /// Built-in UI/terminal theme name.
    pub theme: String,
    /// Optional custom hex color overrides on top of the selected built-in theme.
    #[serde(default)]
    pub theme_overrides: ThemeOverrides,
    /// Window/terminal surface opacity in `[0.3, 1.0]`.
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f32,
    /// Copy selected terminal text to clipboard automatically.
    pub copy_on_select: bool,
    /// Prompt for confirmation before pasting text containing newlines.
    #[serde(default = "default_confirm_multiline_paste")]
    pub confirm_multiline_paste: bool,
    /// Right-click behavior inside terminal viewport (`menu`, `paste`, or `off`).
    #[serde(default)]
    pub right_click_action: RightClickAction,
    /// Paste clipboard on middle mouse button click when not in PTY mouse mode.
    #[serde(default = "default_true")]
    pub middle_click_paste: bool,
    /// Action taken on BEL (`\x07`).
    #[serde(default)]
    pub bell_behavior: BellBehavior,
    /// Position of the workspace tab bar (`top`, `bottom`, `left`, `right`).
    #[serde(default)]
    pub tab_bar_position: TabBarPosition,
    /// Show the profile tree Left Sidebar by default in non-Quake desktop windows (`>= 860 px` wide).
    #[serde(default = "default_true")]
    pub show_profile_sidebar: bool,
    /// Persisted width in logical pixels of the Left Sidebar (`[180, 480]`).
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// Persisted default width in logical pixels of the per-pane SFTP Panel (`[220, 960]`).
    #[serde(default = "default_sftp_panel_width")]
    pub sftp_panel_width: f32,
    /// Automatically open and list the SFTP Panel when a new SSH `Session` connects.
    #[serde(default = "default_true")]
    pub auto_open_sftp: bool,
    /// Optional local destination directory override for SFTP downloads (`None` uses OS Downloads).
    #[serde(default)]
    pub sftp_download_dir: Option<PathBuf>,
    /// Default word wrap preference for the SFTP+ Remote File Editor (`false` = horizontal scroll).
    #[serde(default)]
    pub sftp_editor_word_wrap: bool,
    /// Default expanded modal preference for the SFTP+ Remote File Editor (`false` = 760x460).
    #[serde(default)]
    pub sftp_editor_expanded: bool,
    /// Default indentation mode for the SFTP+ Remote File Editor (`spaces4`, `spaces2`, `tab`).
    #[serde(default)]
    pub sftp_editor_indent_mode: EditorIndentMode,
    /// Show the optional bottom status bar (hidden by default to match Tabby's edge-to-edge UI).
    #[serde(default)]
    pub show_status_bar: bool,
    /// Show live server resource stats (CPU/RAM/DISK/NET) at the bottom of each SSH session.
    #[serde(default = "default_true")]
    pub show_server_stats: bool,
    /// Hide numeric 1..N tab indices inside tab headers (Tabby `hideTabIndex`, default false).
    #[serde(default)]
    pub hide_tab_index: bool,
    /// Size horizontal tabs dynamically to fill space instead of fixed 200px width (Tabby `flexTabs`).
    #[serde(default)]
    pub flex_tabs: bool,
    /// Treat Alt+<key> as ESC+<key> meta prefix in terminal input.
    #[serde(default = "default_true")]
    pub alt_is_meta: bool,
    /// Word boundary characters for double-click semantic selection.
    #[serde(default = "default_word_separators")]
    pub word_separators: String,
    /// Automatically record terminal output logs for new sessions.
    #[serde(default)]
    pub auto_log_sessions: bool,
    /// Optional directory override for session log files.
    #[serde(default)]
    pub log_directory: Option<PathBuf>,
    /// Regex-based terminal output `HighlightRule` entries.
    #[serde(default)]
    pub highlight_rules: Vec<HighlightRule>,
}

fn default_confirm_multiline_paste() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_sidebar_width() -> f32 {
    DEFAULT_SIDEBAR_WIDTH
}

fn default_sftp_panel_width() -> f32 {
    DEFAULT_SFTP_PANEL_WIDTH
}

fn default_window_opacity() -> f32 {
    1.0
}

fn default_word_separators() -> String {
    r#" /\|()"'-.,:;<>~!@#$%^&*+=[]{}~?│"#.to_string()
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: None,
            scrollback_lines: 10_000,
            font_family: "monospace".to_string(),
            font_size: 14.0,
            font_ligatures: false,
            cursor_style: CursorStyle::Block,
            cursor_blink: false,
            theme: "Dark".to_string(),
            theme_overrides: ThemeOverrides::default(),
            window_opacity: 1.0,
            copy_on_select: false,
            confirm_multiline_paste: true,
            right_click_action: RightClickAction::Menu,
            middle_click_paste: true,
            bell_behavior: BellBehavior::VisualAndNotification,
            tab_bar_position: TabBarPosition::Top,
            show_profile_sidebar: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sftp_panel_width: DEFAULT_SFTP_PANEL_WIDTH,
            auto_open_sftp: true,
            sftp_download_dir: None,
            sftp_editor_word_wrap: false,
            sftp_editor_expanded: false,
            sftp_editor_indent_mode: EditorIndentMode::default(),
            show_status_bar: false,
            show_server_stats: true,
            hide_tab_index: false,
            flex_tabs: false,
            alt_is_meta: true,
            word_separators: default_word_separators(),
            auto_log_sessions: false,
            log_directory: None,
            highlight_rules: Vec::new(),
        }
    }
}

/// Initial window geometry configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 800,
        }
    }
}

/// Secret storage backend preference stored in `config.toml` (contains no secrets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VaultBackend {
    /// Store credentials in the native OS keyring (`Secret Service` / `Keychain` / `Credential Manager`).
    #[default]
    Keyring,
    /// Store credentials in a local Argon2id + AES-256-GCM encrypted `Vault` file.
    EncryptedFile,
}

/// Non-secret `Vault` preferences stored in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultConfig {
    /// Active secret storage backend.
    pub backend: VaultBackend,
    /// Idle/sleep auto-lock timeout in minutes for the encrypted local `Vault` (`0` disables auto-lock).
    pub auto_lock_minutes: u64,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        }
    }
}

/// Default collision policy applied when an SFTP `Transfer` targets an existing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransferConflictPolicy {
    /// Pause the `Transfer` and prompt the user (`Overwrite`, `Resume`, `Skip`, `Rename`).
    #[default]
    Ask,
    /// Replace the existing target file.
    Overwrite,
    /// Resume from the existing destination byte offset (or `.termewnal.part` size).
    Resume,
    /// Skip the colliding file without modifying the target.
    Skip,
    /// Automatically disambiguate the target filename (e.g. `name (1).ext`).
    Rename,
}

/// Non-secret SFTP `Transfer` preferences stored in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferConfig {
    /// Policy applied when a target file already exists.
    pub default_conflict_policy: TransferConflictPolicy,
    /// Whether recursive directory transfers follow symlinks (with cycle detection) instead of skipping them.
    pub follow_symlinks: bool,
    /// Minimum byte threshold (`> part_threshold_bytes`) triggering `.termewnal.part` atomic uploads.
    pub part_threshold_bytes: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            default_conflict_policy: TransferConflictPolicy::Ask,
            follow_symlinks: false,
            part_threshold_bytes: DEFAULT_PART_THRESHOLD_BYTES,
        }
    }
}

/// Default indentation style for the SFTP+ Remote File Editor (`config.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EditorIndentMode {
    /// Insert and unindent 4 spaces per Tab level.
    #[default]
    Spaces4,
    /// Insert and unindent 2 spaces per Tab level.
    Spaces2,
    /// Insert and unindent literal `\t` characters.
    Tab,
}

impl EditorIndentMode {
    /// Short status-bar label shown in the Remote File Editor.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spaces4 => "Spaces: 4",
            Self::Spaces2 => "Spaces: 2",
            Self::Tab => "Tab (\\t)",
        }
    }

    /// Cycles to the next indentation mode on status-bar click.
    pub const fn next(self) -> Self {
        match self {
            Self::Spaces4 => Self::Spaces2,
            Self::Spaces2 => Self::Tab,
            Self::Tab => Self::Spaces4,
        }
    }

    /// String inserted for one indentation level.
    pub const fn indent_str(self) -> &'static str {
        match self {
            Self::Spaces4 => "    ",
            Self::Spaces2 => "  ",
            Self::Tab => "\t",
        }
    }

    /// Number of columns per indent stop.
    pub const fn width(self) -> usize {
        match self {
            Self::Spaces4 => 4,
            Self::Spaces2 => 2,
            Self::Tab => 4,
        }
    }
}

/// Non-secret application configuration persisted to `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub schema_version: u32,
    pub terminal: TerminalConfig,
    pub window: WindowConfig,
    pub vault: VaultConfig,
    /// SFTP `Transfer` atomic `.part`, conflict policy, and symlink traversal settings.
    #[serde(default)]
    pub transfer: TransferConfig,
    /// Global SSH keepalive probe interval in seconds (`0` disables periodic probes).
    pub keepalive_interval_secs: u64,
    /// Global maximum consecutive missed SSH keepalive probes before reconnecting.
    pub keepalive_max_missed: u32,
    /// Custom keyboard shortcut overrides.
    #[serde(default)]
    pub keybindings: KeybindingConfig,
    /// Quake drop-down console configuration.
    #[serde(default)]
    pub quake: QuakeConfig,
    /// System Tray integration configuration.
    #[serde(default)]
    pub tray: TrayConfig,
    /// Configuration synchronization settings (tokens remain strictly in `Vault`).
    #[serde(default)]
    pub sync: SyncConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            terminal: TerminalConfig::default(),
            window: WindowConfig::default(),
            vault: VaultConfig::default(),
            transfer: TransferConfig::default(),
            keepalive_interval_secs: DEFAULT_KEEPALIVE_INTERVAL_SECS,
            keepalive_max_missed: DEFAULT_KEEPALIVE_MAX_MISSED,
            keybindings: KeybindingConfig::default(),
            quake: QuakeConfig::default(),
            tray: TrayConfig::default(),
            sync: SyncConfig::default(),
        }
    }
}

/// Loads `config.toml` from `paths`, returning `AppConfig::default()` if the file does not exist.
pub fn load_config(paths: &ConfigPaths) -> Result<AppConfig, CoreError> {
    load_toml_or_default(&paths.config_file(), |cfg: &AppConfig| cfg.schema_version)
}

/// Atomically writes `config` to `config.toml` under `paths`.
pub fn save_config(paths: &ConfigPaths, config: &AppConfig) -> Result<(), CoreError> {
    save_toml_atomic(&paths.config_file(), config, config.schema_version)
}

/// Loads `config.toml` and `profiles.toml`, writing initial defaults if either file is missing.
pub fn ensure_config_files(paths: &ConfigPaths) -> Result<(AppConfig, ProfilesConfig), CoreError> {
    let config_path = paths.config_file();
    let config = if config_path.exists() {
        load_config(paths)?
    } else {
        let cfg = AppConfig::default();
        save_config(paths, &cfg)?;
        cfg
    };

    let profiles_path = paths.profiles_file();
    let profiles = if profiles_path.exists() {
        load_profiles(paths)?
    } else {
        let pf = ProfilesConfig::default();
        save_profiles(paths, &pf)?;
        pf
    };

    Ok((config, profiles))
}
