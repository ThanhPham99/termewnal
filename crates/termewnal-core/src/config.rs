use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::profile::{ProfilesConfig, load_profiles, save_profiles};
use crate::storage::{load_toml_or_default, save_toml_atomic};

/// Current schema version written to `config.toml` and `profiles.toml`.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Default SSH keepalive probe interval in seconds (`15s`).
pub const DEFAULT_KEEPALIVE_INTERVAL_SECS: u64 = 15;

/// Default maximum consecutive missed SSH keepalive probes (`3`, ~45s timeout).
pub const DEFAULT_KEEPALIVE_MAX_MISSED: u32 = 3;

const CONFIG_FILE_NAME: &str = "config.toml";
const PROFILES_FILE_NAME: &str = "profiles.toml";
const LAYOUT_FILE_NAME: &str = "layout.toml";
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
        Ok(Self {
            config_dir: proj.config_dir().to_path_buf(),
            data_local_dir: proj.data_local_dir().to_path_buf(),
            cache_dir: proj.cache_dir().to_path_buf(),
            runtime_dir: proj.runtime_dir().map(Path::to_path_buf),
            home_dir,
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
        }
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
    /// Built-in UI/terminal theme name.
    pub theme: String,
    /// Copy selected terminal text to clipboard automatically.
    pub copy_on_select: bool,
    /// Prompt for confirmation before pasting text containing newlines.
    #[serde(default = "default_confirm_multiline_paste")]
    pub confirm_multiline_paste: bool,
}

fn default_confirm_multiline_paste() -> bool {
    true
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
            theme: "Dark".to_string(),
            copy_on_select: false,
            confirm_multiline_paste: true,
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

/// Non-secret application configuration persisted to `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub schema_version: u32,
    pub terminal: TerminalConfig,
    pub window: WindowConfig,
    pub vault: VaultConfig,
    /// Global SSH keepalive probe interval in seconds (`0` disables periodic probes).
    pub keepalive_interval_secs: u64,
    /// Global maximum consecutive missed SSH keepalive probes before reconnecting.
    pub keepalive_max_missed: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            terminal: TerminalConfig::default(),
            window: WindowConfig::default(),
            vault: VaultConfig::default(),
            keepalive_interval_secs: DEFAULT_KEEPALIVE_INTERVAL_SECS,
            keepalive_max_missed: DEFAULT_KEEPALIVE_MAX_MISSED,
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
