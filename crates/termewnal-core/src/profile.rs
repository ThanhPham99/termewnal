use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{CURRENT_SCHEMA_VERSION, ConfigPaths};
use crate::error::CoreError;
use crate::storage::{load_toml_or_default, save_toml_atomic};

/// Top-level `profiles.toml` document holding saved `Profile` entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilesConfig {
    pub schema_version: u32,
    pub profiles: Vec<Profile>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            profiles: Vec::new(),
        }
    }
}

/// A saved connection definition specifying a local shell or SSH host and attached `Tunnel` rules.
///
/// Secrets (passwords and private-key passphrases) are never stored here; they live exclusively
/// in `Vault`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub target: ProfileTarget,
    #[serde(default)]
    pub tunnels: Vec<TunnelRule>,
}

impl Profile {
    /// Creates a new local shell `Profile` with a generated UUID identifier.
    pub fn local_shell(name: impl Into<String>, shell: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            group: None,
            tags: Vec::new(),
            target: ProfileTarget::LocalShell {
                shell,
                working_dir: None,
                args: Vec::new(),
            },
            tunnels: Vec::new(),
        }
    }

    /// Creates a new SSH `Profile` referencing non-secret auth metadata only.
    pub fn ssh(
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth: SshAuthMethod,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            group: None,
            tags: Vec::new(),
            target: ProfileTarget::Ssh {
                host: host.into(),
                port,
                username: username.into(),
                auth,
                proxy_jump: Vec::new(),
                auto_connect_on_startup: false,
                tmux_reattach: None,
            },
            tunnels: Vec::new(),
        }
    }

    /// Returns whether this `Profile` is configured to auto-connect on startup (SSH targets only).
    pub fn auto_connect_on_startup(&self) -> bool {
        match &self.target {
            ProfileTarget::Ssh {
                auto_connect_on_startup,
                ..
            } => *auto_connect_on_startup,
            ProfileTarget::LocalShell { .. } => false,
        }
    }

    /// Sets `auto_connect_on_startup` on SSH `Profile`s (no-op for local shell profiles).
    pub fn set_auto_connect_on_startup(&mut self, enabled: bool) {
        if let ProfileTarget::Ssh {
            auto_connect_on_startup,
            ..
        } = &mut self.target
        {
            *auto_connect_on_startup = enabled;
        }
    }
}

/// Connection target for a `Profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileTarget {
    /// Local OS shell session spawned via PTY.
    LocalShell {
        #[serde(default)]
        shell: Option<String>,
        #[serde(default)]
        working_dir: Option<PathBuf>,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Remote SSH session (authentication secrets live in `Vault`, never in TOML).
    Ssh {
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
        username: String,
        #[serde(default)]
        auth: SshAuthMethod,
        #[serde(default)]
        proxy_jump: Vec<String>,
        #[serde(default)]
        auto_connect_on_startup: bool,
        #[serde(default)]
        tmux_reattach: Option<String>,
    },
}

fn default_ssh_port() -> u16 {
    22
}

/// Non-secret SSH authentication method metadata stored on a `Profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SshAuthMethod {
    /// Authenticate via OS SSH Agent (`SSH_AUTH_SOCK`, OpenSSH pipe, or Pageant).
    #[default]
    Agent,
    /// Authenticate using an on-disk private key; any passphrase lives in `Vault`.
    PublicKey {
        #[serde(default)]
        key_path: Option<PathBuf>,
        #[serde(default)]
        vault_secret_ref: Option<String>,
    },
    /// Authenticate with a password stored strictly in `Vault` under `vault_secret_ref`.
    Password {
        #[serde(default)]
        vault_secret_ref: Option<String>,
    },
    /// Interactive keyboard-interactive / 2FA prompt.
    KeyboardInteractive,
}

/// Direction of a `Tunnel` attached to a `Profile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelDirection {
    /// Local port forward (`-L`).
    Local,
    /// Remote port forward (`-R`).
    Remote,
    /// Dynamic SOCKS5 proxy forward (`-D`).
    Dynamic,
}

/// Static port-forwarding `Tunnel` rule saved inside a `Profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelRule {
    pub id: String,
    pub name: String,
    pub direction: TunnelDirection,
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    pub bind_port: u16,
    #[serde(default)]
    pub target_host: Option<String>,
    #[serde(default)]
    pub target_port: Option<u16>,
    #[serde(default)]
    pub standalone: bool,
}

fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}

/// Loads `profiles.toml` from `paths`, returning `ProfilesConfig::default()` if missing.
pub fn load_profiles(paths: &ConfigPaths) -> Result<ProfilesConfig, CoreError> {
    load_toml_or_default(&paths.profiles_file(), |cfg: &ProfilesConfig| cfg.schema_version)
}

/// Atomically writes `profiles` to `profiles.toml` under `paths`.
pub fn save_profiles(paths: &ConfigPaths, profiles: &ProfilesConfig) -> Result<(), CoreError> {
    save_toml_atomic(&paths.profiles_file(), profiles, profiles.schema_version)
}
