use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{AppConfig, CURRENT_SCHEMA_VERSION, ConfigPaths};
use crate::error::CoreError;
use crate::storage::{load_toml_or_default, save_toml_atomic};

/// Top-level `profiles.toml` document holding saved `Profile` entries and explicit group names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilesConfig {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    pub profiles: Vec<Profile>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            groups: Vec::new(),
            profiles: Vec::new(),
        }
    }
}

impl ProfilesConfig {
    /// Returns deduplicated, sorted non-empty group names from explicit `groups` and profile memberships.
    pub fn all_groups(&self) -> Vec<String> {
        let mut by_key: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for group in &self.groups {
            if let Some(clean) = normalize_group_label(group) {
                by_key.entry(clean.to_ascii_lowercase()).or_insert(clean);
            }
        }
        for profile in &self.profiles {
            if let Some(group) = profile.group.as_deref().and_then(normalize_group_label) {
                by_key.entry(group.to_ascii_lowercase()).or_insert(group);
            }
        }
        by_key.into_values().collect()
    }

    /// Registers a new explicit group name, returning `true` if added.
    pub fn add_group(&mut self, name: &str) -> bool {
        let Some(clean) = normalize_group_label(name) else {
            return false;
        };
        let key = clean.to_ascii_lowercase();
        let exists = self
            .all_groups()
            .into_iter()
            .any(|g| g.to_ascii_lowercase() == key);
        if exists {
            return false;
        }
        self.groups.push(clean);
        true
    }

    /// Renames an existing group and updates matching profiles, returning `true` if anything changed.
    pub fn rename_group(&mut self, old_name: &str, new_name: &str) -> bool {
        let Some(old_clean) = normalize_group_label(old_name) else {
            return false;
        };
        let Some(new_clean) = normalize_group_label(new_name) else {
            return false;
        };
        let old_key = old_clean.to_ascii_lowercase();
        let new_key = new_clean.to_ascii_lowercase();
        if old_key != new_key
            && self
                .all_groups()
                .into_iter()
                .any(|g| g.to_ascii_lowercase() == new_key)
        {
            return false;
        }

        let mut changed = false;
        for group in &mut self.groups {
            if group.trim().eq_ignore_ascii_case(&old_clean) {
                if *group != new_clean {
                    *group = new_clean.clone();
                    changed = true;
                }
            }
        }
        for profile in &mut self.profiles {
            if profile
                .group
                .as_deref()
                .and_then(normalize_group_label)
                .is_some_and(|g| g.eq_ignore_ascii_case(&old_clean))
            {
                if profile.group.as_deref() != Some(new_clean.as_str()) {
                    profile.group = Some(new_clean.clone());
                    changed = true;
                }
            }
        }
        if !self
            .groups
            .iter()
            .any(|g| g.trim().eq_ignore_ascii_case(&new_clean))
            && (changed || self.profiles.iter().any(|p| p.group.as_deref() == Some(new_clean.as_str())))
        {
            self.groups.push(new_clean);
            changed = true;
        }
        changed
    }

    /// Removes `group_name` and moves its profiles to Ungrouped (`group = None`).
    pub fn delete_group(&mut self, group_name: &str) -> bool {
        let Some(clean) = normalize_group_label(group_name) else {
            return false;
        };
        let prev_len = self.groups.len();
        self.groups
            .retain(|g| !g.trim().eq_ignore_ascii_case(&clean));
        let mut changed = self.groups.len() != prev_len;
        for profile in &mut self.profiles {
            if profile
                .group
                .as_deref()
                .and_then(normalize_group_label)
                .is_some_and(|g| g.eq_ignore_ascii_case(&clean))
            {
                profile.group = None;
                changed = true;
            }
        }
        changed
    }

    /// Reassigns `profile_id` to `group` (`None` = Ungrouped), returning `true` on change.
    pub fn move_profile_to_group(&mut self, profile_id: &str, group: Option<&str>) -> bool {
        let next_group = group.and_then(normalize_group_label);
        let Some(profile) = self.profiles.iter_mut().find(|p| p.id == profile_id) else {
            return false;
        };
        if profile.group == next_group {
            return false;
        }
        profile.group = next_group.clone();
        if let Some(group_name) = next_group
            && !self
                .groups
                .iter()
                .any(|g| g.trim().eq_ignore_ascii_case(&group_name))
        {
            self.groups.push(group_name);
        }
        true
    }
}

fn normalize_group_label(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("Ungrouped") {
        None
    } else {
        Some(trimmed.to_string())
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
                proxy_command: None,
                proxy: None,
                forward_agent: false,
                forward_x11: false,
                login_scripts: Vec::new(),
                auto_connect_on_startup: false,
                tmux_reattach: None,
                keepalive_interval_secs: None,
                keepalive_max_missed: None,
                auto_attach_mux: false,
                mux_attach_command: None,
            },
            tunnels: Vec::new(),
        }
    }

    /// Creates a new Serial RS-232/USB `Profile` with sensible 115200 8N1 defaults.
    pub fn serial(name: impl Into<String>, port: impl Into<String>, baud_rate: u32) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            group: None,
            tags: Vec::new(),
            target: ProfileTarget::Serial {
                port: port.into(),
                baud_rate,
                data_bits: 8,
                parity: SerialParity::None,
                stop_bits: 1,
                flow_control: SerialFlowControl::None,
                input_mode: SerialInputMode::Text,
                output_mode: SerialOutputMode::Text,
                newline_mode: NewlineMode::Cr,
                auto_reconnect: true,
            },
            tunnels: Vec::new(),
        }
    }

    /// Creates a new Telnet `Profile`.
    pub fn telnet(name: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            group: None,
            tags: Vec::new(),
            target: ProfileTarget::Telnet {
                host: host.into(),
                port,
                auto_reconnect: false,
                login_scripts: Vec::new(),
            },
            tunnels: Vec::new(),
        }
    }

    /// Creates a new Docker/Podman container exec `Profile`.
    pub fn docker(
        name: impl Into<String>,
        runtime: DockerRuntime,
        container: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            group: None,
            tags: Vec::new(),
            target: ProfileTarget::Docker {
                runtime,
                container: container.into(),
                shell: None,
                user: None,
                working_dir: None,
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
            _ => false,
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

    /// Returns the per-`Profile` [`SshProfileConfig`] overrides for SSH targets (`None` for local shells).
    pub fn ssh_config(&self) -> Option<SshProfileConfig> {
        match &self.target {
            ProfileTarget::Ssh {
                keepalive_interval_secs,
                keepalive_max_missed,
                auto_attach_mux,
                mux_attach_command,
                ..
            } => Some(SshProfileConfig {
                keepalive_interval_secs: *keepalive_interval_secs,
                keepalive_max_missed: *keepalive_max_missed,
                auto_attach_mux: *auto_attach_mux,
                mux_attach_command: mux_attach_command.clone(),
            }),
            _ => None,
        }
    }

    /// Applies per-`Profile` [`SshProfileConfig`] overrides on SSH targets (no-op for local shells).
    pub fn set_ssh_config(&mut self, config: SshProfileConfig) {
        if let ProfileTarget::Ssh {
            keepalive_interval_secs,
            keepalive_max_missed,
            auto_attach_mux,
            mux_attach_command,
            ..
        } = &mut self.target
        {
            *keepalive_interval_secs = config.keepalive_interval_secs;
            *keepalive_max_missed = config.keepalive_max_missed;
            *auto_attach_mux = config.auto_attach_mux;
            *mux_attach_command = config.mux_attach_command;
        }
    }

    /// Updates per-`Profile` keepalive interval and missed-probe overrides (no-op for local shells).
    pub fn set_keepalive_overrides(&mut self, interval_secs: Option<u64>, max_missed: Option<u32>) {
        if let ProfileTarget::Ssh {
            keepalive_interval_secs,
            keepalive_max_missed,
            ..
        } = &mut self.target
        {
            *keepalive_interval_secs = interval_secs;
            *keepalive_max_missed = max_missed;
        }
    }

    /// Enables or disables automatic `tmux`/`screen` attach on reconnect with an optional command.
    pub fn set_mux_attach(&mut self, enabled: bool, command: Option<String>) {
        if let ProfileTarget::Ssh {
            auto_attach_mux,
            mux_attach_command,
            ..
        } = &mut self.target
        {
            *auto_attach_mux = enabled;
            *mux_attach_command = command;
        }
    }

    /// Resolves effective `(keepalive_interval_secs, keepalive_max_missed)` by applying per-`Profile`
    /// overrides on top of global [`AppConfig`] defaults.
    pub fn resolve_keepalive(&self, global: &AppConfig) -> (u64, u32) {
        match &self.target {
            ProfileTarget::Ssh {
                keepalive_interval_secs,
                keepalive_max_missed,
                ..
            } => (
                keepalive_interval_secs.unwrap_or(global.keepalive_interval_secs),
                keepalive_max_missed.unwrap_or(global.keepalive_max_missed),
            ),
            _ => (global.keepalive_interval_secs, global.keepalive_max_missed),
        }
    }

    /// Returns the shell command to run upon reconnection when multiplexer auto-attach is enabled.
    pub fn effective_mux_attach_command(&self) -> Option<String> {
        let ProfileTarget::Ssh {
            auto_attach_mux,
            mux_attach_command,
            tmux_reattach,
            ..
        } = &self.target
        else {
            return None;
        };

        resolve_mux_attach_command(
            *auto_attach_mux,
            mux_attach_command.as_deref(),
            tmux_reattach.as_deref(),
        )
    }
}

/// Resolves the multiplexer attach command from `auto_attach_mux`, explicit `mux_attach_command`,
/// and legacy `tmux_reattach` session name/command fallback.
pub fn resolve_mux_attach_command(
    auto_attach_mux: bool,
    mux_attach_command: Option<&str>,
    tmux_reattach: Option<&str>,
) -> Option<String> {
    if !auto_attach_mux {
        return None;
    }

    if let Some(cmd) = mux_attach_command
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(cmd.to_string());
    }

    tmux_reattach
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|val| {
            if val.contains(char::is_whitespace) {
                val.to_string()
            } else {
                format!("tmux attach -t {val} || tmux new -s {val}")
            }
        })
}

/// Per-`Profile` SSH keepalive and multiplexer auto-attach settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SshProfileConfig {
    pub keepalive_interval_secs: Option<u64>,
    pub keepalive_max_missed: Option<u32>,
    pub auto_attach_mux: bool,
    pub mux_attach_command: Option<String>,
}

/// Ordered expect/send automation step executed after connecting an SSH, Telnet, or Serial `Session`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoginScriptStep {
    /// Substring or regex expected in incoming stream output before sending (empty sends immediately).
    pub expect: String,
    /// Text to send when matched (unused if `secret_ref` resolves a secret from `Vault`).
    pub send: String,
    /// Interpret `expect` as a regular expression instead of a literal substring.
    pub is_regex: bool,
    /// Append `\r` after `send` payload.
    #[serde(default = "default_script_newline")]
    pub append_newline: bool,
    /// Optional delay in milliseconds after matching before writing `send`.
    pub delay_ms: u64,
    /// Optional `Vault` secret reference substituted instead of plaintext `send`.
    pub secret_ref: Option<String>,
}

fn default_script_newline() -> bool {
    true
}

/// Serial port parity check setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialParity {
    #[default]
    None,
    Odd,
    Even,
}

/// Serial port flow control mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialFlowControl {
    #[default]
    None,
    Software,
    Hardware,
}

/// Serial terminal user input mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialInputMode {
    #[default]
    Text,
    Readline,
    HexBytes,
}

/// Serial terminal incoming byte rendering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialOutputMode {
    #[default]
    Text,
    Hexdump,
}

/// Line ending conversion mode for Serial and interactive byte streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewlineMode {
    #[default]
    Cr,
    Lf,
    CrLf,
    Keep,
}

/// Supported container engine runtime for `ProfileTarget::Docker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerRuntime {
    #[default]
    Docker,
    Podman,
}

/// Upstream network proxy protocol used when dialing an SSH target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshProxyKind {
    #[default]
    Socks5,
    HttpConnect,
}

fn default_proxy_port() -> u16 {
    1080
}

/// Outbound SOCKS5 or HTTP `CONNECT` proxy server configuration for an SSH endpoint.
///
/// Proxy credentials are never stored in plaintext TOML; `vault_secret_ref` references `Vault`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SshProxyConfig {
    pub kind: SshProxyKind,
    pub host: String,
    #[serde(default = "default_proxy_port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub vault_secret_ref: Option<String>,
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
        proxy_command: Option<String>,
        #[serde(default)]
        proxy: Option<SshProxyConfig>,
        #[serde(default)]
        forward_agent: bool,
        #[serde(default)]
        forward_x11: bool,
        #[serde(default)]
        login_scripts: Vec<LoginScriptStep>,
        #[serde(default)]
        auto_connect_on_startup: bool,
        #[serde(default)]
        tmux_reattach: Option<String>,
        #[serde(default)]
        keepalive_interval_secs: Option<u64>,
        #[serde(default)]
        keepalive_max_missed: Option<u32>,
        #[serde(default)]
        auto_attach_mux: bool,
        #[serde(default)]
        mux_attach_command: Option<String>,
    },
    /// RS-232 / USB Serial port terminal.
    Serial {
        port: String,
        #[serde(default = "default_serial_baud")]
        baud_rate: u32,
        #[serde(default = "default_serial_data_bits")]
        data_bits: u8,
        #[serde(default)]
        parity: SerialParity,
        #[serde(default = "default_serial_stop_bits")]
        stop_bits: u8,
        #[serde(default)]
        flow_control: SerialFlowControl,
        #[serde(default)]
        input_mode: SerialInputMode,
        #[serde(default)]
        output_mode: SerialOutputMode,
        #[serde(default)]
        newline_mode: NewlineMode,
        #[serde(default = "default_script_newline")]
        auto_reconnect: bool,
    },
    /// TCP Telnet client session.
    Telnet {
        host: String,
        #[serde(default = "default_telnet_port")]
        port: u16,
        #[serde(default)]
        auto_reconnect: bool,
        #[serde(default)]
        login_scripts: Vec<LoginScriptStep>,
    },
    /// Docker or Podman container interactive shell (`exec -it`).
    Docker {
        #[serde(default)]
        runtime: DockerRuntime,
        container: String,
        #[serde(default)]
        shell: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        working_dir: Option<String>,
    },
}

fn default_ssh_port() -> u16 {
    22
}

fn default_telnet_port() -> u16 {
    23
}

fn default_serial_baud() -> u32 {
    115_200
}

fn default_serial_data_bits() -> u8 {
    8
}

fn default_serial_stop_bits() -> u8 {
    1
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
    /// Local `Tunnel` (`-L`).
    Local,
    /// Remote `Tunnel` (`-R`).
    Remote,
    /// Dynamic SOCKS5 `Tunnel` (`-D`).
    Dynamic,
}

/// Default hold-until-reconnected grace timeout in seconds for Local/Dynamic `Tunnel` clients.
pub const DEFAULT_TUNNEL_HOLD_GRACE_SECS: u64 = 10;

/// Returns `true` if `bind_host` exposes a `Tunnel` listener beyond loopback (e.g. `0.0.0.0`, `::`, LAN IP).
pub fn is_public_bind_host(bind_host: &str) -> bool {
    let trimmed = bind_host.trim();
    if trimmed.is_empty() {
        return false;
    }
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if unbracketed.eq_ignore_ascii_case("localhost") {
        return false;
    }
    match unbracketed.parse::<IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => true,
    }
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
    #[serde(default)]
    pub allow_public_bind: bool,
    #[serde(default)]
    pub hold_grace_secs: Option<u64>,
}

impl TunnelRule {
    /// Creates a new Local (`-L`) `TunnelRule` defaulting to `127.0.0.1` loopback bind.
    pub fn local(
        name: impl Into<String>,
        bind_port: u16,
        target_host: impl Into<String>,
        target_port: u16,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            direction: TunnelDirection::Local,
            bind_host: default_bind_host(),
            bind_port,
            target_host: Some(target_host.into()),
            target_port: Some(target_port),
            standalone: false,
            allow_public_bind: false,
            hold_grace_secs: None,
        }
    }

    /// Creates a new Remote (`-R`) `TunnelRule` defaulting to `127.0.0.1` remote bind host.
    pub fn remote(
        name: impl Into<String>,
        remote_bind_port: u16,
        target_host: impl Into<String>,
        target_port: u16,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            direction: TunnelDirection::Remote,
            bind_host: default_bind_host(),
            bind_port: remote_bind_port,
            target_host: Some(target_host.into()),
            target_port: Some(target_port),
            standalone: false,
            allow_public_bind: false,
            hold_grace_secs: None,
        }
    }

    /// Creates a new Dynamic SOCKS5 (`-D`) `TunnelRule` defaulting to `127.0.0.1` loopback bind.
    pub fn dynamic_socks5(name: impl Into<String>, bind_port: u16) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            direction: TunnelDirection::Dynamic,
            bind_host: default_bind_host(),
            bind_port,
            target_host: None,
            target_port: None,
            standalone: false,
            allow_public_bind: false,
            hold_grace_secs: None,
        }
    }

    /// Returns the normalized local bind host, falling back to `127.0.0.1` when blank.
    pub fn effective_bind_host(&self) -> &str {
        let trimmed = self.bind_host.trim();
        if trimmed.is_empty() {
            "127.0.0.1"
        } else {
            trimmed
        }
    }

    /// Returns the hold-until-reconnected grace [`Duration`], defaulting to `10s`.
    pub fn effective_hold_grace(&self) -> Duration {
        Duration::from_secs(
            self.hold_grace_secs
                .unwrap_or(DEFAULT_TUNNEL_HOLD_GRACE_SECS),
        )
    }

    /// Returns `true` when binding a Local (`-L`) or Dynamic (`-D`) `Tunnel` listener would expose
    /// a non-loopback local address without explicit opt-in.
    ///
    /// Remote (`-R`) `Tunnel`s bind on the remote SSH server and dial local `target_host:target_port`,
    /// so they do not expose local listeners to the LAN.
    pub fn requires_public_bind_opt_in(&self) -> bool {
        match self.direction {
            TunnelDirection::Local | TunnelDirection::Dynamic => {
                !self.allow_public_bind && is_public_bind_host(self.effective_bind_host())
            }
            TunnelDirection::Remote => false,
        }
    }
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

/// Returns a case-trimmed unique display name not colliding with any existing `Profile` name.
pub(crate) fn unique_profile_name(existing: &[Profile], desired: &str) -> String {
    let base = desired.trim();
    let base = if base.is_empty() { "Imported Profile" } else { base };
    if !existing.iter().any(|p| p.name == base) {
        return base.to_string();
    }
    for suffix in 2_usize.. {
        let candidate = format!("{base} ({suffix})");
        if !existing.iter().any(|p| p.name == candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded suffix range always finds a unique name")
}
