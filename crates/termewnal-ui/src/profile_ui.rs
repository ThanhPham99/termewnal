//! Tabby-inspired GUI Profile Manager modal for creating, editing, and deleting Saved `Profile`s
//! across SSH (with forwarding, scripts, keepalive & tunnels), Local Shell, Serial, Telnet, and Docker.

use std::fmt;
use std::path::PathBuf;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::Widget;
use iced::widget::{button, column, container, pick_list, row, scrollable, space, text, text_input};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Padding, Rectangle, Shadow, Size, Theme,
    mouse,
};
use termewnal_core::{
    DockerRuntime, LoginScriptStep, NewlineMode, Profile, ProfileTarget, SerialFlowControl,
    SerialInputMode, SerialOutputMode, SerialParity, SshAuthMethod, SshProxyConfig, SshProxyKind,
    ThemePalette, TunnelDirection, TunnelRule,
};
use zeroize::Zeroizing;

use crate::app::Message;
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, PillTone, anchored_menu, apple_checkbox, badge_pill,
    hairline_h, inset_well_style, modal_body_height, modal_card_style, modal_overlay, modal_width,
    mono_font, pill_button_style, styled_input_style, tab_bar_button_style, tabby_auth_tile_style,
    tabby_menu_style, tabby_pick_list_handle, tabby_pick_list_style, tabby_scrollable_style,
    tabby_switch, tabby_underline_tab, ui_font, ui_font_medium, ui_font_semibold,
};

/// Origin of a selectable Jump Host candidate offered in the SSH Profile editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpHostKind {
    SavedProfile,
    SystemSsh,
}

/// Selectable Jump Host entry populated from Saved SSH `Profile`s or `~/.ssh/config` hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpHostCandidate {
    pub token: String,
    pub endpoint: String,
    pub kind: JumpHostKind,
    pub transitive_hops: Vec<String>,
}

/// Tabby right-pane navigation tabs inside the Edit Profile modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileEditorTab {
    #[default]
    General,
    Ports,
    Advanced,
    LoginScripts,
}

/// Connection target kind in the GUI Profile editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileKindChoice {
    #[default]
    Ssh,
    LocalShell,
    Serial,
    Telnet,
    Docker,
}

/// SSH authentication choice in the GUI Profile editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileAuthChoice {
    #[default]
    Agent,
    PublicKey,
    Password,
    KeyboardInteractive,
}

/// SSH transport connection mode selectable in the Profile editor `Connection` dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SshConnectionMode {
    #[default]
    Direct,
    ProxyCommand,
    JumpHost,
    SocksProxy,
    HttpProxy,
}

impl SshConnectionMode {
    pub const ALL: [Self; 5] = [
        Self::Direct,
        Self::ProxyCommand,
        Self::JumpHost,
        Self::SocksProxy,
        Self::HttpProxy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "Direct",
            Self::ProxyCommand => "Proxy command",
            Self::JumpHost => "Jump host",
            Self::SocksProxy => "SOCKS proxy",
            Self::HttpProxy => "HTTP proxy",
        }
    }

    pub fn subtitle(self) -> Option<&'static str> {
        match self {
            Self::Direct => None,
            Self::ProxyCommand => {
                Some("Command's stdin/stdout is used instead of a network connection")
            }
            Self::JumpHost => Some("Connect to a different host first and use it as a proxy"),
            Self::SocksProxy => Some("Connect through a proxy server"),
            Self::HttpProxy => Some("Using CONNECT method"),
        }
    }
}

/// Editable endpoint fields for a SOCKS5 or HTTP `CONNECT` proxy server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProxyEndpointDraft {
    pub host: String,
    pub port: String,
    pub username: String,
    pub secret_draft: String,
    pub existing_secret_ref: Option<String>,
}

/// Result produced when saving a [`ProfileEditorState`] draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedProfileDraft {
    pub profile: Profile,
    pub secret_to_store: Option<(String, Zeroizing<String>)>,
    pub extra_secrets_to_store: Vec<(String, Zeroizing<String>)>,
}

/// Interactive state for creating/editing/deleting a Saved `Profile`.
#[derive(Debug, Clone)]
pub struct ProfileEditorState {
    pub open: bool,
    pub active_tab: ProfileEditorTab,
    pub editing_id: Option<String>,
    pub name: String,
    pub group: String,
    pub tags: String,
    pub kind: ProfileKindChoice,
    pub local_shell: String,
    pub local_working_dir: String,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_username: String,
    pub auth_choice: ProfileAuthChoice,
    pub key_path: String,
    pub existing_secret_ref: Option<String>,
    pub secret_draft: String,
    pub connection_mode: SshConnectionMode,
    pub connection_menu_open: bool,
    pub proxy_jump: String,
    pub jump_host_candidates: Vec<JumpHostCandidate>,
    pub jump_panel_open: bool,
    pub jump_custom_open: bool,
    pub jump_custom_draft: String,
    pub proxy_command: String,
    pub socks_proxy: ProxyEndpointDraft,
    pub http_proxy: ProxyEndpointDraft,
    pub forward_agent: bool,
    pub forward_x11: bool,
    pub ssh_keepalive_interval: String,
    pub ssh_keepalive_missed: String,
    pub auto_connect_on_startup: bool,
    pub auto_attach_mux: bool,
    pub mux_attach_command: String,
    pub login_scripts: Vec<LoginScriptStep>,
    pub script_expect_draft: String,
    pub script_send_draft: String,
    pub script_is_regex_draft: bool,
    pub tunnels: Vec<TunnelRule>,
    pub tunnel_name_draft: String,
    pub tunnel_dir_draft: TunnelDirection,
    pub tunnel_bind_host_draft: String,
    pub tunnel_bind_port_draft: String,
    pub tunnel_target_host_draft: String,
    pub tunnel_target_port_draft: String,
    pub serial_port: String,
    pub serial_baud_rate: String,
    pub serial_data_bits: u8,
    pub serial_parity: SerialParity,
    pub serial_stop_bits: u8,
    pub serial_flow_control: SerialFlowControl,
    pub serial_input_mode: SerialInputMode,
    pub serial_output_mode: SerialOutputMode,
    pub serial_newline_mode: NewlineMode,
    pub serial_auto_reconnect: bool,
    pub telnet_host: String,
    pub telnet_port: String,
    pub telnet_auto_reconnect: bool,
    pub docker_runtime: DockerRuntime,
    pub docker_container: String,
    pub docker_shell: String,
    pub docker_user: String,
    pub docker_working_dir: String,
    pub validation_error: Option<String>,
}

impl Default for ProfileEditorState {
    fn default() -> Self {
        Self {
            open: false,
            active_tab: ProfileEditorTab::default(),
            editing_id: None,
            name: String::new(),
            group: String::new(),
            tags: String::new(),
            kind: ProfileKindChoice::default(),
            local_shell: String::new(),
            local_working_dir: String::new(),
            ssh_host: String::new(),
            ssh_port: String::new(),
            ssh_username: String::new(),
            auth_choice: ProfileAuthChoice::default(),
            key_path: String::new(),
            existing_secret_ref: None,
            secret_draft: String::new(),
            connection_mode: SshConnectionMode::default(),
            connection_menu_open: false,
            proxy_jump: String::new(),
            jump_host_candidates: Vec::new(),
            jump_panel_open: false,
            jump_custom_open: false,
            jump_custom_draft: String::new(),
            proxy_command: String::new(),
            socks_proxy: ProxyEndpointDraft {
                port: "1080".to_string(),
                ..ProxyEndpointDraft::default()
            },
            http_proxy: ProxyEndpointDraft {
                port: "8080".to_string(),
                ..ProxyEndpointDraft::default()
            },
            forward_agent: false,
            forward_x11: false,
            ssh_keepalive_interval: String::new(),
            ssh_keepalive_missed: String::new(),
            auto_connect_on_startup: false,
            auto_attach_mux: false,
            mux_attach_command: String::new(),
            login_scripts: Vec::new(),
            script_expect_draft: String::new(),
            script_send_draft: String::new(),
            script_is_regex_draft: false,
            tunnels: Vec::new(),
            tunnel_name_draft: String::new(),
            tunnel_dir_draft: TunnelDirection::Local,
            tunnel_bind_host_draft: String::new(),
            tunnel_bind_port_draft: String::new(),
            tunnel_target_host_draft: String::new(),
            tunnel_target_port_draft: String::new(),
            serial_port: String::new(),
            serial_baud_rate: String::new(),
            serial_data_bits: 8,
            serial_parity: SerialParity::default(),
            serial_stop_bits: 1,
            serial_flow_control: SerialFlowControl::default(),
            serial_input_mode: SerialInputMode::default(),
            serial_output_mode: SerialOutputMode::default(),
            serial_newline_mode: NewlineMode::default(),
            serial_auto_reconnect: true,
            telnet_host: String::new(),
            telnet_port: String::new(),
            telnet_auto_reconnect: false,
            docker_runtime: DockerRuntime::default(),
            docker_container: String::new(),
            docker_shell: String::new(),
            docker_user: String::new(),
            docker_working_dir: String::new(),
            validation_error: None,
        }
    }
}

impl ProfileEditorState {
    /// Returns the active [`ProxyEndpointDraft`] when `connection_mode` is SOCKS or HTTP proxy.
    pub fn active_proxy_draft(&self) -> Option<&ProxyEndpointDraft> {
        match self.connection_mode {
            SshConnectionMode::SocksProxy => Some(&self.socks_proxy),
            SshConnectionMode::HttpProxy => Some(&self.http_proxy),
            _ => None,
        }
    }

    /// Returns a mutable handle to the active [`ProxyEndpointDraft`] when using SOCKS or HTTP proxy.
    pub fn active_proxy_draft_mut(&mut self) -> Option<&mut ProxyEndpointDraft> {
        match self.connection_mode {
            SshConnectionMode::SocksProxy => Some(&mut self.socks_proxy),
            SshConnectionMode::HttpProxy => Some(&mut self.http_proxy),
            _ => None,
        }
    }

    /// Returns `true` when the Profile editor modal is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the modal in "New Profile" mode with sensible defaults.
    pub fn open_new(&mut self, default_user: String) {
        self.open_new_in_group(default_user, None);
    }

    /// Opens the modal in "New Profile" mode optionally pre-assigning `group`.
    pub fn open_new_in_group(&mut self, default_user: String, group: Option<String>) {
        *self = Self {
            open: true,
            editing_id: None,
            group: group.unwrap_or_default(),
            kind: ProfileKindChoice::Ssh,
            ssh_port: "22".to_string(),
            ssh_username: default_user,
            auth_choice: ProfileAuthChoice::Agent,
            tunnel_bind_host_draft: "127.0.0.1".to_string(),
            tunnel_target_host_draft: "127.0.0.1".to_string(),
            serial_baud_rate: "115200".to_string(),
            serial_data_bits: 8,
            serial_stop_bits: 1,
            serial_auto_reconnect: true,
            telnet_port: "23".to_string(),
            ..Self::default()
        };
    }

    /// Opens the modal pre-populated from an existing [`Profile`].
    pub fn open_edit(&mut self, profile: &Profile) {
        let mut next = Self {
            open: true,
            editing_id: Some(profile.id.clone()),
            name: profile.name.clone(),
            group: profile.group.clone().unwrap_or_default(),
            tags: profile.tags.join(", "),
            ssh_port: "22".to_string(),
            tunnel_bind_host_draft: "127.0.0.1".to_string(),
            tunnel_target_host_draft: "127.0.0.1".to_string(),
            serial_baud_rate: "115200".to_string(),
            serial_data_bits: 8,
            serial_stop_bits: 1,
            serial_auto_reconnect: true,
            telnet_port: "23".to_string(),
            tunnels: profile.tunnels.clone(),
            ..Self::default()
        };

        match &profile.target {
            ProfileTarget::LocalShell {
                shell, working_dir, ..
            } => {
                next.kind = ProfileKindChoice::LocalShell;
                next.local_shell = shell.clone().unwrap_or_default();
                next.local_working_dir = working_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
            }
            ProfileTarget::Ssh {
                host,
                port,
                username,
                auth,
                proxy_jump,
                proxy_command,
                proxy,
                forward_agent,
                forward_x11,
                login_scripts,
                auto_connect_on_startup,
                tmux_reattach,
                keepalive_interval_secs,
                keepalive_max_missed,
                auto_attach_mux,
                mux_attach_command,
            } => {
                next.kind = ProfileKindChoice::Ssh;
                next.ssh_host = host.clone();
                next.ssh_port = port.to_string();
                next.ssh_username = username.clone();
                next.proxy_jump = proxy_jump.join(", ");
                next.proxy_command = proxy_command.clone().unwrap_or_default();
                if !proxy_jump.is_empty() {
                    next.connection_mode = SshConnectionMode::JumpHost;
                } else if let Some(p) = proxy {
                    let draft = ProxyEndpointDraft {
                        host: p.host.clone(),
                        port: p.port.to_string(),
                        username: p.username.clone().unwrap_or_default(),
                        secret_draft: String::new(),
                        existing_secret_ref: p.vault_secret_ref.clone(),
                    };
                    match p.kind {
                        SshProxyKind::Socks5 => {
                            next.connection_mode = SshConnectionMode::SocksProxy;
                            next.socks_proxy = draft;
                        }
                        SshProxyKind::HttpConnect => {
                            next.connection_mode = SshConnectionMode::HttpProxy;
                            next.http_proxy = draft;
                        }
                    }
                } else if proxy_command
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
                {
                    next.connection_mode = SshConnectionMode::ProxyCommand;
                } else {
                    next.connection_mode = SshConnectionMode::Direct;
                }
                next.forward_agent = *forward_agent;
                next.forward_x11 = *forward_x11;
                next.login_scripts = login_scripts.clone();
                next.ssh_keepalive_interval = keepalive_interval_secs
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                next.ssh_keepalive_missed = keepalive_max_missed
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                next.auto_connect_on_startup = *auto_connect_on_startup;
                next.auto_attach_mux = *auto_attach_mux || tmux_reattach.is_some();
                next.mux_attach_command = mux_attach_command
                    .clone()
                    .or_else(|| tmux_reattach.clone())
                    .unwrap_or_default();
                match auth {
                    SshAuthMethod::Agent => next.auth_choice = ProfileAuthChoice::Agent,
                    SshAuthMethod::PublicKey {
                        key_path,
                        vault_secret_ref,
                    } => {
                        next.auth_choice = ProfileAuthChoice::PublicKey;
                        next.key_path = key_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                        next.existing_secret_ref = vault_secret_ref.clone();
                    }
                    SshAuthMethod::Password { vault_secret_ref } => {
                        next.auth_choice = ProfileAuthChoice::Password;
                        next.existing_secret_ref = vault_secret_ref.clone();
                    }
                    SshAuthMethod::KeyboardInteractive => {
                        next.auth_choice = ProfileAuthChoice::KeyboardInteractive;
                    }
                }
            }
            ProfileTarget::Serial {
                port,
                baud_rate,
                data_bits,
                parity,
                stop_bits,
                flow_control,
                input_mode,
                output_mode,
                newline_mode,
                auto_reconnect,
            } => {
                next.kind = ProfileKindChoice::Serial;
                next.serial_port = port.clone();
                next.serial_baud_rate = baud_rate.to_string();
                next.serial_data_bits = *data_bits;
                next.serial_parity = *parity;
                next.serial_stop_bits = *stop_bits;
                next.serial_flow_control = *flow_control;
                next.serial_input_mode = *input_mode;
                next.serial_output_mode = *output_mode;
                next.serial_newline_mode = *newline_mode;
                next.serial_auto_reconnect = *auto_reconnect;
            }
            ProfileTarget::Telnet {
                host,
                port,
                auto_reconnect,
                login_scripts,
            } => {
                next.kind = ProfileKindChoice::Telnet;
                next.telnet_host = host.clone();
                next.telnet_port = port.to_string();
                next.telnet_auto_reconnect = *auto_reconnect;
                next.login_scripts = login_scripts.clone();
            }
            ProfileTarget::Docker {
                runtime,
                container,
                shell,
                user,
                working_dir,
            } => {
                next.kind = ProfileKindChoice::Docker;
                next.docker_runtime = *runtime;
                next.docker_container = container.clone();
                next.docker_shell = shell.clone().unwrap_or_default();
                next.docker_user = user.clone().unwrap_or_default();
                next.docker_working_dir = working_dir.clone().unwrap_or_default();
            }
        }
        *self = next;
    }

    /// Closes the modal without saving.
    pub fn close(&mut self) {
        self.open = false;
        self.connection_menu_open = false;
        self.secret_draft.clear();
        self.socks_proxy.secret_draft.clear();
        self.http_proxy.secret_draft.clear();
        self.jump_custom_open = false;
        self.jump_custom_draft.clear();
        self.validation_error = None;
    }

    /// Parses comma-separated `ProxyJump` hops cleanly.
    pub fn parse_jump_hops(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect()
    }

    /// Returns ordered `ProxyJump` hop tokens currently configured on this SSH draft.
    pub fn jump_hops(&self) -> Vec<String> {
        Self::parse_jump_hops(&self.proxy_jump)
    }

    /// Overwrites the ordered `ProxyJump` hops cleanly.
    pub fn set_jump_hops(&mut self, hops: &[String]) {
        self.proxy_jump = hops.join(", ");
    }

    /// Populates selectable Jump Host candidates (Saved SSH profiles + System SSH config hosts).
    pub fn set_jump_host_candidates(&mut self, candidates: Vec<JumpHostCandidate>) {
        self.jump_host_candidates = candidates;
    }

    /// Appends a single Jump Host hop (alias or `[user@]host[:port]`) to the route chain.
    pub fn add_jump_hop(&mut self, raw_hop: &str) -> Result<(), String> {
        let token = raw_hop.trim().trim_matches(',').trim();
        if token.is_empty() {
            return Err("Jump host cannot be empty".to_string());
        }
        if token.contains(',') {
            return Err("Enter one jump host at a time when adding to the route".to_string());
        }
        let own_name = self.name.trim();
        if !own_name.is_empty() && token.eq_ignore_ascii_case(own_name) {
            return Err(format!(
                "Cannot select '{own_name}' as its own jump host (cycle)"
            ));
        }

        let mut hops = self.jump_hops();
        if hops.iter().any(|h| h.eq_ignore_ascii_case(token)) {
            return Err(format!("Jump host '{token}' is already in the route"));
        }
        hops.push(token.to_string());
        self.set_jump_hops(&hops);
        self.connection_mode = SshConnectionMode::JumpHost;
        self.validation_error = None;
        Ok(())
    }

    /// Appends the typed custom jump host draft and clears the draft field on success.
    pub fn add_custom_jump_hop_from_draft(&mut self) -> Result<(), String> {
        let draft = self.jump_custom_draft.clone();
        self.add_jump_hop(&draft)?;
        self.jump_custom_draft.clear();
        self.jump_custom_open = false;
        Ok(())
    }

    /// Removes the Jump Host hop at `index`, if present.
    pub fn remove_jump_hop(&mut self, index: usize) {
        let mut hops = self.jump_hops();
        if index < hops.len() {
            hops.remove(index);
            self.set_jump_hops(&hops);
            self.validation_error = None;
        }
    }

    /// Shifts a Jump Host hop earlier (`delta < 0`) or later (`delta > 0`) in the chain.
    pub fn move_jump_hop(&mut self, index: usize, delta: isize) {
        let mut hops = self.jump_hops();
        let target = index as isize + delta;
        if index < hops.len() && target >= 0 && (target as usize) < hops.len() {
            hops.swap(index, target as usize);
            self.set_jump_hops(&hops);
            self.validation_error = None;
        }
    }

    /// Clears all Jump Host hops back to a direct SSH connection.
    pub fn clear_jump_hops(&mut self) {
        self.proxy_jump.clear();
        self.validation_error = None;
    }

    pub fn add_login_script_step(&mut self) -> Result<(), String> {
        let send = self.script_send_draft.trim().to_string();
        if send.is_empty() && self.script_expect_draft.trim().is_empty() {
            return Err("Provide expect or send text for login script step".to_string());
        }
        self.login_scripts.push(LoginScriptStep {
            expect: self.script_expect_draft.clone(),
            send,
            is_regex: self.script_is_regex_draft,
            append_newline: true,
            delay_ms: 0,
            secret_ref: None,
        });
        self.script_expect_draft.clear();
        self.script_send_draft.clear();
        self.script_is_regex_draft = false;
        self.validation_error = None;
        Ok(())
    }

    pub fn add_tunnel_rule(&mut self) -> Result<(), String> {
        let name = self.tunnel_name_draft.trim().to_string();
        if name.is_empty() {
            return Err("Tunnel name is required".to_string());
        }
        let bind_port: u16 = self
            .tunnel_bind_port_draft
            .trim()
            .parse()
            .ok()
            .filter(|&p| p > 0)
            .ok_or_else(|| "Tunnel bind port must be 1..=65535".to_string())?;
        let bind_host = {
            let trimmed = self.tunnel_bind_host_draft.trim();
            if trimmed.is_empty() {
                "127.0.0.1".to_string()
            } else {
                trimmed.to_string()
            }
        };
        let (target_host, target_port) = if self.tunnel_dir_draft == TunnelDirection::Dynamic {
            (None, None)
        } else {
            let th = self.tunnel_target_host_draft.trim();
            if th.is_empty() {
                return Err("Tunnel target host is required for Local/Remote forward".to_string());
            }
            let tp: u16 = self
                .tunnel_target_port_draft
                .trim()
                .parse()
                .ok()
                .filter(|&p| p > 0)
                .ok_or_else(|| "Tunnel target port must be 1..=65535".to_string())?;
            (Some(th.to_string()), Some(tp))
        };

        let mut rule = TunnelRule::local(
            name,
            bind_port,
            target_host.as_deref().unwrap_or(""),
            target_port.unwrap_or(0),
        );
        rule.direction = self.tunnel_dir_draft;
        rule.bind_host = bind_host;
        rule.target_host = target_host;
        rule.target_port = target_port;
        self.tunnels.push(rule);
        self.tunnel_name_draft.clear();
        self.tunnel_bind_port_draft.clear();
        self.tunnel_target_port_draft.clear();
        self.validation_error = None;
        Ok(())
    }

    /// Validates draft fields and returns a [`SavedProfileDraft`].
    pub fn build_profile(&mut self, existing: Option<&Profile>) -> Result<SavedProfileDraft, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("Profile name is required".to_string());
        }

        let group = {
            let trimmed = self.group.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let tags: Vec<String> = self
            .tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();

        match self.kind {
            ProfileKindChoice::LocalShell => {
                let shell = {
                    let trimmed = self.local_shell.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                };
                let mut p = Profile::local_shell(name, shell);
                if let Some(prev) = existing {
                    p.id = prev.id.clone();
                }
                if let ProfileTarget::LocalShell {
                    working_dir, args, ..
                } = &mut p.target
                {
                    let wd = self.local_working_dir.trim();
                    *working_dir = (!wd.is_empty()).then(|| PathBuf::from(wd));
                    if let Some(prev) = existing
                        && let ProfileTarget::LocalShell { args: prev_args, .. } = &prev.target
                    {
                        *args = prev_args.clone();
                    }
                }
                p.group = group;
                p.tags = tags;
                p.tunnels = self.tunnels.clone();
                return Ok(SavedProfileDraft {
                    profile: p,
                    secret_to_store: None,
                    extra_secrets_to_store: Vec::new(),
                });
            }
            ProfileKindChoice::Serial => {
                let port = self.serial_port.trim().to_string();
                if port.is_empty() {
                    return Err("Serial port device path is required".to_string());
                }
                let baud: u32 = self
                    .serial_baud_rate
                    .trim()
                    .parse()
                    .ok()
                    .filter(|&b| b > 0)
                    .ok_or_else(|| "Serial baud rate must be a positive integer".to_string())?;
                let mut p = Profile::serial(name, port, baud);
                if let Some(prev) = existing {
                    p.id = prev.id.clone();
                }
                if let ProfileTarget::Serial {
                    data_bits,
                    parity,
                    stop_bits,
                    flow_control,
                    input_mode,
                    output_mode,
                    newline_mode,
                    auto_reconnect,
                    ..
                } = &mut p.target
                {
                    *data_bits = self.serial_data_bits.clamp(5, 8);
                    *parity = self.serial_parity;
                    *stop_bits = if self.serial_stop_bits == 2 { 2 } else { 1 };
                    *flow_control = self.serial_flow_control;
                    *input_mode = self.serial_input_mode;
                    *output_mode = self.serial_output_mode;
                    *newline_mode = self.serial_newline_mode;
                    *auto_reconnect = self.serial_auto_reconnect;
                }
                p.group = group;
                p.tags = tags;
                p.tunnels = self.tunnels.clone();
                return Ok(SavedProfileDraft {
                    profile: p,
                    secret_to_store: None,
                    extra_secrets_to_store: Vec::new(),
                });
            }
            ProfileKindChoice::Telnet => {
                let host = self.telnet_host.trim().to_string();
                if host.is_empty() {
                    return Err("Telnet Host is required".to_string());
                }
                let port: u16 = self
                    .telnet_port
                    .trim()
                    .parse()
                    .ok()
                    .filter(|&p| p > 0)
                    .ok_or_else(|| "Telnet Port must be between 1 and 65535".to_string())?;
                let mut p = Profile::telnet(name, host, port);
                if let Some(prev) = existing {
                    p.id = prev.id.clone();
                }
                if let ProfileTarget::Telnet {
                    auto_reconnect,
                    login_scripts,
                    ..
                } = &mut p.target
                {
                    *auto_reconnect = self.telnet_auto_reconnect;
                    *login_scripts = self.login_scripts.clone();
                }
                p.group = group;
                p.tags = tags;
                p.tunnels = self.tunnels.clone();
                return Ok(SavedProfileDraft {
                    profile: p,
                    secret_to_store: None,
                    extra_secrets_to_store: Vec::new(),
                });
            }
            ProfileKindChoice::Docker => {
                let container = self.docker_container.trim().to_string();
                if container.is_empty() {
                    return Err("Docker/Podman container name or ID is required".to_string());
                }
                let mut p = Profile::docker(name, self.docker_runtime, container);
                if let Some(prev) = existing {
                    p.id = prev.id.clone();
                }
                if let ProfileTarget::Docker {
                    shell,
                    user,
                    working_dir,
                    ..
                } = &mut p.target
                {
                    let trimmed_shell = self.docker_shell.trim();
                    *shell = (!trimmed_shell.is_empty()).then(|| trimmed_shell.to_string());
                    let trimmed_user = self.docker_user.trim();
                    *user = (!trimmed_user.is_empty()).then(|| trimmed_user.to_string());
                    let trimmed_wd = self.docker_working_dir.trim();
                    *working_dir = (!trimmed_wd.is_empty()).then(|| trimmed_wd.to_string());
                }
                p.group = group;
                p.tags = tags;
                p.tunnels = self.tunnels.clone();
                return Ok(SavedProfileDraft {
                    profile: p,
                    secret_to_store: None,
                    extra_secrets_to_store: Vec::new(),
                });
            }
            ProfileKindChoice::Ssh => {}
        }

        let host = self.ssh_host.trim().to_string();
        if host.is_empty() {
            return Err("SSH Host is required".to_string());
        }
        let port: u16 = self
            .ssh_port
            .trim()
            .parse()
            .ok()
            .filter(|&p| p > 0)
            .ok_or_else(|| "SSH Port must be between 1 and 65535".to_string())?;
        let username = self.ssh_username.trim().to_string();
        if username.is_empty() {
            return Err("SSH Username is required".to_string());
        }

        let keepalive_interval_opt = {
            let trimmed = self.ssh_keepalive_interval.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(
                    trimmed
                        .parse::<u64>()
                        .map_err(|_| "Keepalive interval must be a non-negative integer".to_string())?,
                )
            }
        };
        let keepalive_missed_opt = {
            let trimmed = self.ssh_keepalive_missed.trim();
            if trimmed.is_empty() {
                None
            } else {
                let v: u32 = trimmed
                    .parse()
                    .map_err(|_| "Keepalive missed probes must be >= 1".to_string())?;
                if v == 0 {
                    return Err("Keepalive missed probes must be >= 1".to_string());
                }
                Some(v)
            }
        };

        let mut profile = Profile::ssh(name, host, port, username, SshAuthMethod::Agent);
        if let Some(prev) = existing {
            profile.id = prev.id.clone();
        }
        profile.group = group;
        profile.tags = tags;
        profile.tunnels = self.tunnels.clone();

        let mut secret_to_store: Option<(String, Zeroizing<String>)> = None;
        let has_secret_draft = !self.secret_draft.is_empty();
        let default_secret_ref = || format!("termewnal/profile/{}", profile.id);

        let auth = match self.auth_choice {
            ProfileAuthChoice::Agent => SshAuthMethod::Agent,
            ProfileAuthChoice::KeyboardInteractive => SshAuthMethod::KeyboardInteractive,
            ProfileAuthChoice::PublicKey => {
                let key_path = {
                    let trimmed = self.key_path.trim();
                    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
                };
                let secret_ref = if has_secret_draft {
                    let r = self
                        .existing_secret_ref
                        .clone()
                        .unwrap_or_else(default_secret_ref);
                    secret_to_store =
                        Some((r.clone(), Zeroizing::new(self.secret_draft.clone())));
                    Some(r)
                } else {
                    self.existing_secret_ref.clone()
                };
                SshAuthMethod::PublicKey {
                    key_path,
                    vault_secret_ref: secret_ref,
                }
            }
            ProfileAuthChoice::Password => {
                let secret_ref = if has_secret_draft {
                    let r = self
                        .existing_secret_ref
                        .clone()
                        .unwrap_or_else(default_secret_ref);
                    secret_to_store =
                        Some((r.clone(), Zeroizing::new(self.secret_draft.clone())));
                    Some(r)
                } else {
                    self.existing_secret_ref.clone()
                };
                SshAuthMethod::Password {
                    vault_secret_ref: secret_ref,
                }
            }
        };

        let mut extra_secrets_to_store = Vec::new();
        let (hops, proxy_cmd, proxy_cfg) = match self.connection_mode {
            SshConnectionMode::Direct => (Vec::new(), None, None),
            SshConnectionMode::ProxyCommand => {
                let trimmed = self.proxy_command.trim();
                if trimmed.is_empty() {
                    return Err(
                        "Proxy command is required when Connection is set to Proxy command"
                            .to_string(),
                    );
                }
                (Vec::new(), Some(trimmed.to_string()), None)
            }
            SshConnectionMode::JumpHost => {
                let hops: Vec<String> = self
                    .proxy_jump
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
                    .collect();
                (hops, None, None)
            }
            SshConnectionMode::SocksProxy | SshConnectionMode::HttpProxy => {
                let (kind, label, draft) = match self.connection_mode {
                    SshConnectionMode::SocksProxy => {
                        (SshProxyKind::Socks5, "SOCKS", &self.socks_proxy)
                    }
                    SshConnectionMode::HttpProxy => {
                        (SshProxyKind::HttpConnect, "HTTP", &self.http_proxy)
                    }
                    _ => unreachable!(),
                };
                let proxy_host = draft.host.trim().to_string();
                if proxy_host.is_empty() {
                    return Err(format!("{label} proxy host is required"));
                }
                let proxy_port: u16 = draft
                    .port
                    .trim()
                    .parse()
                    .ok()
                    .filter(|&p| p > 0)
                    .ok_or_else(|| format!("{label} proxy port must be between 1 and 65535"))?;
                let proxy_user = {
                    let u = draft.username.trim();
                    (!u.is_empty()).then(|| u.to_string())
                };
                let proxy_secret_ref = if !draft.secret_draft.is_empty() {
                    let r = draft
                        .existing_secret_ref
                        .clone()
                        .unwrap_or_else(|| format!("termewnal/profile/{}/proxy", profile.id));
                    extra_secrets_to_store
                        .push((r.clone(), Zeroizing::new(draft.secret_draft.clone())));
                    Some(r)
                } else {
                    draft.existing_secret_ref.clone()
                };
                (
                    Vec::new(),
                    None,
                    Some(SshProxyConfig {
                        kind,
                        host: proxy_host,
                        port: proxy_port,
                        username: proxy_user,
                        vault_secret_ref: proxy_secret_ref,
                    }),
                )
            }
        };
        let mux_cmd = {
            let trimmed = self.mux_attach_command.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };

        if let ProfileTarget::Ssh {
            auth: target_auth,
            proxy_jump,
            proxy_command,
            proxy,
            forward_agent,
            forward_x11,
            login_scripts,
            auto_connect_on_startup,
            auto_attach_mux,
            mux_attach_command,
            keepalive_interval_secs,
            keepalive_max_missed,
            tmux_reattach,
            ..
        } = &mut profile.target
        {
            *target_auth = auth;
            *proxy_jump = hops;
            *proxy_command = proxy_cmd;
            *proxy = proxy_cfg;
            *forward_agent = self.forward_agent;
            *forward_x11 = self.forward_x11;
            *login_scripts = self.login_scripts.clone();
            *auto_connect_on_startup = self.auto_connect_on_startup;
            *auto_attach_mux = self.auto_attach_mux;
            *mux_attach_command = mux_cmd;
            *keepalive_interval_secs = keepalive_interval_opt;
            *keepalive_max_missed = keepalive_missed_opt;
            if let Some(prev) = existing
                && let ProfileTarget::Ssh {
                    tmux_reattach: prev_tmux,
                    ..
                } = &prev.target
            {
                *tmux_reattach = prev_tmux.clone();
            }
        }

        self.validation_error = None;
        Ok(SavedProfileDraft {
            profile,
            secret_to_store,
            extra_secrets_to_store,
        })
    }

    /// Renders the Profile create/edit modal overlay when open.
    pub fn view<'a>(
        &'a self,
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        self.view_responsive(palette, 1280.0, 800.0)
    }

    pub fn view_responsive<'a>(
        &'a self,
        palette: &ThemePalette,
        window_width: f32,
        window_height: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(860.0, window_width);
        let body_h = modal_body_height(490.0, window_height, 200.0);
        let two_col = card_w >= 660.0;
        let compact_fields = if two_col {
            card_w - 274.0 < 440.0
        } else {
            card_w < 460.0
        };

        // Clean Tabby modal title (`10.0.0.10` when editing or `"New Profile"`).
        let title_text = if !self.name.trim().is_empty() {
            self.name.clone()
        } else if self.editing_id.is_some() {
            "Edit Profile".to_string()
        } else {
            "New Profile".to_string()
        };
        let header = row![
            container(
                text(title_text)
                    .size(21.0)
                    .font(ui_font())
                    .color(chrome.text_emphasis)
                    .wrapping(text::Wrapping::None),
            )
            .width(Length::Fill)
            .clip(true),
            button(text("×").size(16.0).font(ui_font()))
                .padding([2, 8])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::CloseProfileEditor),
        ]
        .spacing(12)
        .align_y(Alignment::Center);

        // Left column: stacked Tabby form groups (Type, Name, Group, Tags, session options).
        let kind_track = container(
            row![
                kind_btn("SSH", ProfileKindChoice::Ssh, self.kind, chrome),
                kind_btn("Local", ProfileKindChoice::LocalShell, self.kind, chrome),
                kind_btn("Serial", ProfileKindChoice::Serial, self.kind, chrome),
                kind_btn("Telnet", ProfileKindChoice::Telnet, self.kind, chrome),
                kind_btn("Docker", ProfileKindChoice::Docker, self.kind, chrome),
            ]
            .spacing(2),
        )
        .padding(3)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.input_bg)),
            border: Border {
                color: chrome.border_subtle,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        });

        let mut left_col = column![
            field_stack("Type", kind_track.into(), chrome),
            field_stack(
                "Name",
                form_input("Prod Bastion", &self.name, Message::ProfileFormNameChanged, chrome)
                    .width(Length::Fill)
                    .into(),
                chrome,
            ),
            field_stack(
                "Group",
                form_input(
                    "Ungrouped",
                    &self.group,
                    Message::ProfileFormGroupChanged,
                    chrome,
                )
                .width(Length::Fill)
                .into(),
                chrome,
            ),
            field_stack(
                "Tags",
                form_input(
                    "Comma-separated tags",
                    &self.tags,
                    Message::ProfileFormTagsChanged,
                    chrome,
                )
                .width(Length::Fill)
                .into(),
                chrome,
            ),
        ]
        .spacing(12);

        match self.kind {
            ProfileKindChoice::Ssh => {
                left_col = left_col.push(tabby_switch(
                    self.auto_connect_on_startup,
                    "Auto-connect on startup",
                    Some("Open session automatically when restoring layout"),
                    Message::ProfileFormAutoConnectToggled,
                    chrome,
                ));
            }
            ProfileKindChoice::Serial => {
                left_col = left_col.push(tabby_switch(
                    self.serial_auto_reconnect,
                    "Auto-reconnect",
                    Some("Reconnect automatically if serial device drops"),
                    Message::ProfileFormSerialAutoReconnectToggled,
                    chrome,
                ));
            }
            ProfileKindChoice::Telnet => {
                left_col = left_col.push(tabby_switch(
                    self.telnet_auto_reconnect,
                    "Auto-reconnect",
                    Some("Reconnect automatically when connection drops"),
                    Message::ProfileFormTelnetAutoReconnectToggled,
                    chrome,
                ));
            }
            ProfileKindChoice::LocalShell | ProfileKindChoice::Docker => {}
        }

        // Right column: Tabby underline tabs (`GENERAL` | `PORTS` | `ADVANCED` | `LOGIN SCRIPTS`).
        let mut right_tabs_row = row![tabby_underline_tab(
            "GENERAL",
            self.active_tab == ProfileEditorTab::General,
            Message::ProfileFormTabSelected(ProfileEditorTab::General),
            chrome,
        )]
        .spacing(18)
        .align_y(Alignment::Center);

        if self.kind == ProfileKindChoice::Ssh {
            right_tabs_row = right_tabs_row
                .push(tabby_underline_tab(
                    "PORTS",
                    self.active_tab == ProfileEditorTab::Ports,
                    Message::ProfileFormTabSelected(ProfileEditorTab::Ports),
                    chrome,
                ))
                .push(tabby_underline_tab(
                    "ADVANCED",
                    self.active_tab == ProfileEditorTab::Advanced,
                    Message::ProfileFormTabSelected(ProfileEditorTab::Advanced),
                    chrome,
                ))
                .push(tabby_underline_tab(
                    "LOGIN SCRIPTS",
                    self.active_tab == ProfileEditorTab::LoginScripts,
                    Message::ProfileFormTabSelected(ProfileEditorTab::LoginScripts),
                    chrome,
                ));
        } else if self.kind == ProfileKindChoice::Telnet {
            right_tabs_row = right_tabs_row.push(tabby_underline_tab(
                "LOGIN SCRIPTS",
                self.active_tab == ProfileEditorTab::LoginScripts,
                Message::ProfileFormTabSelected(ProfileEditorTab::LoginScripts),
                chrome,
            ));
        }

        let right_tab_body: Element<'a, Message, Theme, iced::Renderer> = match self.kind {
            ProfileKindChoice::Ssh => match self.active_tab {
                ProfileEditorTab::General => self.view_ssh_general_tab(chrome, compact_fields),
                ProfileEditorTab::Ports => self.view_attached_tunnels_editor(chrome, compact_fields),
                ProfileEditorTab::Advanced => self.view_ssh_advanced_tab(chrome, compact_fields),
                ProfileEditorTab::LoginScripts => self.view_login_scripts_editor(chrome, compact_fields),
            },
            ProfileKindChoice::Telnet => match self.active_tab {
                ProfileEditorTab::LoginScripts => self.view_login_scripts_editor(chrome, compact_fields),
                _ => self.view_telnet_general_tab(chrome, compact_fields),
            },
            ProfileKindChoice::LocalShell => self.view_local_general_tab(chrome),
            ProfileKindChoice::Serial => self.view_serial_general_tab(chrome, compact_fields),
            ProfileKindChoice::Docker => self.view_docker_general_tab(chrome, compact_fields),
        };

        let scrolled_body: Element<'a, Message, Theme, iced::Renderer> = if two_col {
            let right_scrolled = scrollable(
                container(right_tab_body)
                    .padding(Padding {
                        top: 2.0,
                        right: 14.0,
                        bottom: 8.0,
                        left: 0.0,
                    })
                    .width(Length::Fill),
            )
            .style(tabby_scrollable_style(chrome))
            .width(Length::Fill)
            .height(Length::Fill);

            let right_col = column![
                right_tabs_row.wrap().vertical_spacing(6),
                right_scrolled,
            ]
            .spacing(16)
            .width(Length::Fill)
            .height(Length::Fixed(body_h));

            container(
                row![left_col.width(Length::Fixed(256.0)), right_col]
                    .spacing(28)
                    .width(Length::Fill)
                    .height(Length::Fixed(body_h)),
            )
            .width(Length::Fill)
            .height(Length::Fixed(body_h))
            .into()
        } else {
            let right_col = column![
                right_tabs_row.wrap().vertical_spacing(6),
                right_tab_body,
            ]
            .spacing(16)
            .width(Length::Fill);

            let body_content = column![left_col.width(Length::Fill), hairline_h(chrome), right_col]
                .spacing(16)
                .width(Length::Fill);

            scrollable(
                container(body_content)
                    .padding(Padding {
                        top: 2.0,
                        right: 14.0,
                        bottom: 8.0,
                        left: 0.0,
                    })
                    .width(Length::Fill),
            )
            .style(tabby_scrollable_style(chrome))
            .width(Length::Fill)
            .height(Length::Fixed(body_h))
            .into()
        };

        let left_footer: Element<'a, Message, Theme, iced::Renderer> =
            if let Some(pid) = &self.editing_id {
                button(text("Delete").size(FONT_BODY))
                    .padding([7, 14])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(Message::ProfileDelete(pid.clone()))
                    .into()
            } else {
                space::horizontal().width(Length::Shrink).into()
            };
        let right_footer = row![
            button(text("Save").size(FONT_BODY).font(ui_font_medium()))
                .padding([7, 18])
                .style(pill_button_style(chrome, PillTone::Primary))
                .on_press(Message::ProfileSave),
            button(text("Cancel").size(FONT_BODY).font(ui_font_medium()))
                .padding([7, 16])
                .style(pill_button_style(chrome, PillTone::Danger))
                .on_press(Message::CloseProfileEditor),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let footer_buttons = row![left_footer, space::horizontal(), right_footer]
            .spacing(8)
            .width(Length::Fill)
            .align_y(Alignment::Center);

        let footer: Element<'a, Message, Theme, iced::Renderer> =
            if let Some(err) = &self.validation_error {
                column![
                    text(err.as_str()).size(FONT_BODY).color(chrome.danger),
                    footer_buttons,
                ]
                .spacing(8)
                .width(Length::Fill)
                .into()
            } else {
                footer_buttons.into()
            };

        let card = container(
            column![header, scrolled_body, footer]
                .spacing(18)
                .padding(22),
        )
        .width(Length::Fixed(card_w))
        .style(move |_t| modal_card_style(chrome, true));

        Some(modal_overlay(card.into(), Some(Message::CloseProfileEditor), 24.0))
    }

    fn view_ssh_general_tab<'a>(
        &'a self,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let hops = self.jump_hops();
        let conn_has_custom = match self.connection_mode {
            SshConnectionMode::Direct => false,
            SshConnectionMode::ProxyCommand => !self.proxy_command.trim().is_empty(),
            SshConnectionMode::JumpHost => !hops.is_empty(),
            SshConnectionMode::SocksProxy | SshConnectionMode::HttpProxy => self
                .active_proxy_draft()
                .is_some_and(|d| !d.host.trim().is_empty()),
        };
        let arrow = if self.connection_menu_open { "▴" } else { "▾" };
        let conn_label = format!("{} {arrow}", self.connection_mode.label());
        let conn_text_color = if conn_has_custom {
            chrome.accent
        } else if self.connection_menu_open {
            chrome.text_emphasis
        } else {
            chrome.text_muted
        };
        let conn_btn: Element<'a, Message, Theme, iced::Renderer> = button(
            container(
                text(conn_label)
                    .size(FONT_BODY)
                    .font(ui_font())
                    .color(conn_text_color)
                    .wrapping(text::Wrapping::None),
            )
            .center_x(Length::Fill),
        )
        .padding([8, 10])
        .width(Length::Fill)
        .style(pill_button_style(
            chrome,
            if self.connection_menu_open {
                PillTone::ActiveAccent
            } else {
                PillTone::Subtle
            },
        ))
        .on_press(Message::ProfileFormToggleConnectionMenu)
        .into();

        let conn_field = field_stack(
            "Connection",
            anchored_menu(
                conn_btn,
                self.connection_menu_open,
                self.view_connection_mode_menu(chrome),
                368.0,
                Message::ProfileFormCloseConnectionMenu,
            ),
            chrome,
        );
        let host_field = field_stack(
            "Host",
            form_input(
                "10.0.0.10 or host.example.com",
                &self.ssh_host,
                Message::ProfileFormHostChanged,
                chrome,
            )
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let port_field = field_stack(
            "Port",
            form_input(
                "22",
                &self.ssh_port,
                Message::ProfileFormPortChanged,
                chrome,
            )
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let conn_btn_w = if self.connection_mode == SshConnectionMode::ProxyCommand {
            132.0
        } else {
            116.0
        };
        let host_port_row: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![
                row![conn_field.width(Length::Fixed(conn_btn_w)), port_field.width(Length::Fill)]
                    .spacing(10)
                    .width(Length::Fill),
                host_field.width(Length::Fill),
            ]
            .spacing(10)
            .width(Length::Fill)
            .into()
        } else {
            row![
                conn_field.width(Length::Fixed(conn_btn_w)),
                host_field.width(Length::Fill),
                port_field.width(Length::Fixed(82.0)),
            ]
            .spacing(10)
            .width(Length::Fill)
            .into()
        };

        let auth_bar = container(
            row![
                auth_btn("Agent", ProfileAuthChoice::Agent, self.auth_choice, chrome),
                auth_btn("Key", ProfileAuthChoice::PublicKey, self.auth_choice, chrome),
                auth_btn("Password", ProfileAuthChoice::Password, self.auth_choice, chrome),
                auth_btn("Interactive", ProfileAuthChoice::KeyboardInteractive, self.auth_choice, chrome),
            ]
            .spacing(2)
            .width(Length::Fill),
        )
        .padding(3)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.input_bg)),
            border: Border {
                color: chrome.border_subtle,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        });

        let mut col = column![host_port_row].spacing(14).width(Length::Fill);
        match self.connection_mode {
            SshConnectionMode::Direct => {}
            SshConnectionMode::ProxyCommand => {
                col = col.push(
                    container(
                        column![
                            field_stack(
                                "Proxy command",
                                form_input(
                                    "e.g. cloudflared access ssh --hostname %h",
                                    &self.proxy_command,
                                    Message::ProfileFormProxyCommandChanged,
                                    chrome,
                                )
                                .font(mono_font())
                                .width(Length::Fill)
                                .into(),
                                chrome,
                            ),
                            text("Tokens: %h host, %p port, %r user, %% literal %")
                                .size(FONT_META)
                                .font(ui_font())
                                .color(chrome.text_muted),
                        ]
                        .spacing(6)
                        .width(Length::Fill),
                    )
                    .padding(10)
                    .width(Length::Fill)
                    .style(move |_| inset_well_style(chrome)),
                );
            }
            SshConnectionMode::JumpHost => {
                if self.jump_panel_open {
                    col = col.push(
                        container(self.view_jump_host_editor(chrome))
                            .padding(10)
                            .width(Length::Fill)
                            .style(move |_| inset_well_style(chrome)),
                    );
                } else {
                    let route_text = if hops.is_empty() {
                        "No jump hops selected".to_string()
                    } else {
                        hops.join(" → ")
                    };
                    let count_badge = badge_pill(
                        if hops.is_empty() {
                            "0 hops".to_string()
                        } else if hops.len() == 1 {
                            "1 hop".to_string()
                        } else {
                            format!("{} hops", hops.len())
                        },
                        if hops.is_empty() {
                            chrome.text_muted
                        } else {
                            chrome.accent
                        },
                        if hops.is_empty() {
                            chrome.secondary
                        } else {
                            chrome.accent_soft
                        },
                        Color::TRANSPARENT,
                    );
                    col = col.push(
                        container(
                            row![
                                count_badge,
                                container(
                                    text(route_text)
                                        .size(FONT_META)
                                        .font(mono_font())
                                        .color(chrome.text_secondary)
                                        .wrapping(text::Wrapping::None),
                                )
                                .width(Length::Fill)
                                .clip(true),
                                button(
                                    text("Edit ▾")
                                        .size(FONT_META)
                                        .font(ui_font_medium()),
                                )
                                .padding([4, 10])
                                .style(pill_button_style(chrome, PillTone::Subtle))
                                .on_press(Message::ProfileFormToggleJumpPanel),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center)
                            .width(Length::Fill),
                        )
                        .padding([8, 10])
                        .width(Length::Fill)
                        .style(move |_| inset_well_style(chrome)),
                    );
                }
            }
            SshConnectionMode::SocksProxy | SshConnectionMode::HttpProxy => {
                if let Some(draft) = self.active_proxy_draft() {
                    col = col.push(
                        container(self.view_proxy_endpoint_editor(draft, chrome, compact))
                            .padding(10)
                            .width(Length::Fill)
                            .style(move |_| inset_well_style(chrome)),
                    );
                }
            }
        }
        col = col
            .push(field_stack(
                "Username",
                form_input(
                    "deploy",
                    &self.ssh_username,
                    Message::ProfileFormUsernameChanged,
                    chrome,
                )
                .width(Length::Fill)
                .into(),
                chrome,
            ))
            .push(field_stack("Authentication method", auth_bar.into(), chrome));

        if self.auth_choice == ProfileAuthChoice::PublicKey {
            col = col.push(field_stack(
                "Private keys",
                form_input(
                    "~/.ssh/id_ed25519",
                    &self.key_path,
                    Message::ProfileFormKeyPathChanged,
                    chrome,
                )
                .font(mono_font())
                .width(Length::Fill)
                .into(),
                chrome,
            ));
        }

        if matches!(
            self.auth_choice,
            ProfileAuthChoice::Password | ProfileAuthChoice::PublicKey
        ) {
            let label = if self.auth_choice == ProfileAuthChoice::Password {
                "Password"
            } else {
                "Key passphrase"
            };
            if self.existing_secret_ref.is_some() {
                let saved_copy = if self.auth_choice == ProfileAuthChoice::Password {
                    "There is a saved password for this connection"
                } else {
                    "There is a saved passphrase for this key"
                };
                col = col.push(
                    column![
                        text(label)
                            .size(FONT_BODY)
                            .font(ui_font())
                            .color(chrome.text_secondary),
                        row![
                            text(saved_copy)
                                .size(FONT_META)
                                .font(ui_font())
                                .color(chrome.text_muted)
                                .width(Length::Fill),
                            button(text("Forget").size(FONT_META).font(ui_font_medium()))
                                .padding([5, 12])
                                .style(pill_button_style(chrome, PillTone::Danger))
                                .on_press(Message::ProfileFormForgetSecret),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                        text_input("Enter new secret to replace saved value", &self.secret_draft)
                            .secure(true)
                            .on_input(Message::ProfileFormSecretChanged)
                            .size(FONT_BODY)
                            .padding([8, 10])
                            .style(styled_input_style(chrome))
                            .width(Length::Fill),
                    ]
                    .spacing(5)
                    .width(Length::Fill),
                );
            } else {
                col = col.push(field_stack(
                    label,
                    text_input("Enter secret (stored in Vault)", &self.secret_draft)
                        .secure(true)
                        .on_input(Message::ProfileFormSecretChanged)
                        .size(FONT_BODY)
                        .padding([8, 10])
                        .style(styled_input_style(chrome))
                        .width(Length::Fill)
                        .into(),
                    chrome,
                ));
            }
        }

        col.into()
    }

    fn view_connection_mode_menu<'a>(
        &'a self,
        chrome: ChromePalette,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let menu_bg = if chrome.is_light {
            chrome.panel_bg
        } else {
            Color {
                r: (chrome.panel_bg.r * 0.56).max(0.04),
                g: (chrome.panel_bg.g * 0.56).max(0.04),
                b: (chrome.panel_bg.b * 0.56).max(0.05),
                a: 1.0,
            }
        };

        let mut list = column![].spacing(2).width(Length::Fill);
        for mode in SshConnectionMode::ALL {
            let active = mode == self.connection_mode;
            let mut item_col = column![
                text(mode.label())
                    .size(13.0)
                    .font(if active { ui_font_semibold() } else { ui_font() })
                    .color(if active {
                        chrome.accent
                    } else {
                        chrome.text_primary
                    }),
            ]
            .spacing(2)
            .width(Length::Fill);

            if let Some(sub) = mode.subtitle() {
                item_col = item_col.push(
                    text(sub)
                        .size(11.5)
                        .font(ui_font())
                        .color(chrome.text_muted),
                );
            }

            let item_btn = button(item_col)
                .padding([8, 12])
                .width(Length::Fill)
                .style(move |_, status| {
                    let hovered =
                        matches!(status, button::Status::Hovered | button::Status::Pressed);
                    button::Style {
                        background: if hovered {
                            Some(Background::Color(chrome.bg_less_2))
                        } else if active {
                            Some(Background::Color(chrome.accent_soft))
                        } else {
                            None
                        },
                        text_color: chrome.text_primary,
                        border: Border {
                            radius: 5.0.into(),
                            ..Default::default()
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    }
                })
                .on_press(Message::ProfileFormConnectionModeSelected(mode));

            list = list.push(item_btn);
        }

        container(list)
            .padding([6, 6])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(menu_bg)),
                border: Border {
                    color: chrome.border,
                    width: 1.0,
                    radius: 7.0.into(),
                },
                shadow: Shadow {
                    color: Color {
                        a: if chrome.is_light { 0.18 } else { 0.58 },
                        ..Color::BLACK
                    },
                    offset: iced::Vector::new(0.0, 8.0),
                    blur_radius: 22.0,
                },
                text_color: Some(chrome.text_primary),
                snap: true,
            })
            .into()
    }

    fn view_proxy_endpoint_editor<'a>(
        &'a self,
        draft: &'a ProxyEndpointDraft,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let (title, proto_tag, port_placeholder) = match self.connection_mode {
            SshConnectionMode::SocksProxy => ("SOCKS proxy", "SOCKS5", "1080"),
            SshConnectionMode::HttpProxy => ("HTTP proxy", "CONNECT", "8080"),
            _ => ("Proxy", "Proxy", "1080"),
        };

        let header = row![
            text(title)
                .size(FONT_BODY)
                .font(ui_font())
                .color(chrome.text_secondary),
            space::horizontal(),
            badge_pill(proto_tag, chrome.accent, chrome.accent_soft, Color::TRANSPARENT),
        ]
        .align_y(Alignment::Center);

        let host_field = field_stack(
            "Proxy host",
            form_input(
                "127.0.0.1 or proxy.example.com",
                &draft.host,
                Message::ProfileFormProxyHostChanged,
                chrome,
            )
            .font(mono_font())
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let port_field = field_stack(
            "Proxy port",
            form_input(
                port_placeholder,
                &draft.port,
                Message::ProfileFormProxyPortChanged,
                chrome,
            )
            .font(mono_font())
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let endpoint_row: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![host_field.width(Length::Fill), port_field.width(Length::Fill)]
                .spacing(10)
                .width(Length::Fill)
                .into()
        } else {
            row![host_field.width(Length::Fill), port_field.width(Length::Fixed(94.0))]
                .spacing(10)
                .width(Length::Fill)
                .into()
        };

        let user_field = field_stack(
            "Proxy user (optional)",
            form_input(
                "Username",
                &draft.username,
                Message::ProfileFormProxyUsernameChanged,
                chrome,
            )
            .width(Length::Fill)
            .into(),
            chrome,
        );

        let pass_field: Element<'a, Message, Theme, iced::Renderer> =
            if draft.existing_secret_ref.is_some() {
                column![
                    text("Proxy password")
                        .size(FONT_META)
                        .font(ui_font())
                        .color(chrome.text_secondary),
                    row![
                        text("Saved in Vault")
                            .size(FONT_META)
                            .font(ui_font())
                            .color(chrome.text_muted)
                            .width(Length::Fill),
                        button(text("Forget").size(FONT_META).font(ui_font_medium()))
                            .padding([4, 10])
                            .style(pill_button_style(chrome, PillTone::Danger))
                            .on_press(Message::ProfileFormForgetProxySecret),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                    text_input("New proxy password", &draft.secret_draft)
                        .secure(true)
                        .on_input(Message::ProfileFormProxySecretChanged)
                        .size(FONT_BODY)
                        .padding([8, 10])
                        .style(styled_input_style(chrome))
                        .width(Length::Fill),
                ]
                .spacing(4)
                .width(Length::Fill)
                .into()
            } else {
                field_stack(
                    "Proxy password (optional)",
                    text_input("Optional password (Vault)", &draft.secret_draft)
                        .secure(true)
                        .on_input(Message::ProfileFormProxySecretChanged)
                        .size(FONT_BODY)
                        .padding([8, 10])
                        .style(styled_input_style(chrome))
                        .width(Length::Fill)
                        .into(),
                    chrome,
                )
                .into()
            };

        let auth_row: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![user_field.width(Length::Fill), pass_field]
                .spacing(10)
                .width(Length::Fill)
                .into()
        } else {
            row![user_field.width(Length::Fill), pass_field]
                .spacing(10)
                .width(Length::Fill)
                .into()
        };

        column![header, endpoint_row, auth_row]
            .spacing(10)
            .width(Length::Fill)
            .into()
    }

    fn view_jump_host_editor<'a>(
        &'a self,
        chrome: ChromePalette,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let hops = self.jump_hops();
        let mut options: Vec<JumpDropdownOption> = Vec::new();
        if !hops.is_empty() {
            options.push(JumpDropdownOption {
                token: String::new(),
                label: "None (Direct connection)".to_string(),
            });
        }

        let mut selected_option: Option<JumpDropdownOption> = None;
        for cand in &self.jump_host_candidates {
            let already_selected = hops.iter().any(|h| h.eq_ignore_ascii_case(&cand.token));
            let is_single_selected =
                hops.len() == 1 && hops[0].eq_ignore_ascii_case(&cand.token);
            if already_selected && !is_single_selected {
                continue;
            }
            let origin_tag = match cand.kind {
                JumpHostKind::SavedProfile => "Saved",
                JumpHostKind::SystemSsh => "SSH Config",
            };
            let opt = JumpDropdownOption {
                token: cand.token.clone(),
                label: format!("{} ({}) · {}", cand.token, origin_tag, cand.endpoint),
            };
            if is_single_selected {
                selected_option = Some(opt.clone());
            }
            options.push(opt);
        }

        let mut header_row = row![
            text("Jump host")
                .size(FONT_BODY)
                .font(ui_font())
                .color(chrome.text_secondary),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if hops.is_empty() {
            header_row = header_row.push(
                text("Direct connection")
                    .size(FONT_META)
                    .font(ui_font())
                    .color(chrome.text_muted),
            );
        } else {
            let hop_count_lbl = if hops.len() == 1 {
                "1 hop".to_string()
            } else {
                format!("{} hops", hops.len())
            };
            header_row = header_row
                .push(badge_pill(
                    hop_count_lbl,
                    chrome.accent,
                    chrome.accent_soft,
                    Color::TRANSPARENT,
                ))
                .push(
                    button(text("Clear").size(FONT_META).font(ui_font()))
                        .padding([2, 8])
                        .style(pill_button_style(chrome, PillTone::Ghost))
                        .on_press(Message::ProfileFormClearJumpHops),
                );
        }

        let show_custom_row = self.jump_custom_open || self.jump_host_candidates.is_empty();
        let placeholder_text = if hops.is_empty() {
            if self.jump_host_candidates.is_empty() {
                "No saved SSH jump hosts yet".to_string()
            } else {
                "None (Direct connection)".to_string()
            }
        } else if hops.len() == 1 {
            format!("Selected: {}", hops[0])
        } else {
            format!("{} hops selected · Add another jump host...", hops.len())
        };

        let mut controls_col = column![].spacing(8).width(Length::Fill);
        if !options.is_empty() {
            let picker = pick_list(options, selected_option, |opt| {
                Message::ProfileFormJumpDropdownSelected(opt.token)
            })
            .placeholder(placeholder_text)
            .text_size(FONT_BODY)
            .font(ui_font())
            .padding([8, 10])
            .width(Length::Fill)
            .handle(tabby_pick_list_handle())
            .style(tabby_pick_list_style(chrome))
            .menu_style(tabby_menu_style(chrome));

            let mut picker_row = row![picker].spacing(8).align_y(Alignment::Center);
            if !self.jump_host_candidates.is_empty() {
                picker_row = picker_row.push(
                    button(
                        text(if self.jump_custom_open {
                            "Done"
                        } else {
                            "+ Custom"
                        })
                        .size(FONT_META)
                        .font(ui_font_medium()),
                    )
                    .padding([7, 11])
                    .style(pill_button_style(
                        chrome,
                        if self.jump_custom_open {
                            PillTone::ActiveAccent
                        } else {
                            PillTone::Subtle
                        },
                    ))
                    .on_press(Message::ProfileFormToggleCustomJumpInput),
                );
            }
            controls_col = controls_col.push(picker_row);
        }

        if show_custom_row {
            let can_add = !self.jump_custom_draft.trim().is_empty();
            let custom_input = text_input(
                "user@bastion.example.com:22 or SSH alias",
                &self.jump_custom_draft,
            )
            .on_input(Message::ProfileFormCustomJumpDraftChanged)
            .on_submit(Message::ProfileFormAddCustomJumpHop)
            .font(mono_font())
            .size(FONT_BODY)
            .padding([8, 10])
            .width(Length::Fill)
            .style(styled_input_style(chrome));

            let add_btn = button(text("+ Add hop").size(FONT_META).font(ui_font_medium()))
                .padding([7, 12])
                .style(pill_button_style(
                    chrome,
                    if can_add {
                        PillTone::Primary
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press_maybe(can_add.then_some(Message::ProfileFormAddCustomJumpHop));

            controls_col = controls_col.push(
                row![custom_input, add_btn]
                    .spacing(8)
                    .width(Length::Fill)
                    .align_y(Alignment::Center),
            );
        }

        let target_label = if !self.ssh_host.trim().is_empty() {
            let port_part = self.ssh_port.trim();
            if port_part.is_empty() || port_part == "22" {
                self.ssh_host.trim().to_string()
            } else {
                format!("{}:{port_part}", self.ssh_host.trim())
            }
        } else if !self.name.trim().is_empty() {
            self.name.trim().to_string()
        } else {
            "Target host".to_string()
        };

        if hops.is_empty() {
            controls_col = controls_col.push(
                container(
                    row![
                        container(space::horizontal().width(6).height(6)).style(move |_| {
                            container::Style {
                                background: Some(Background::Color(chrome.success)),
                                border: Border {
                                    radius: 999.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            }
                        }),
                        text("Local")
                            .size(FONT_META)
                            .font(ui_font())
                            .color(chrome.text_secondary),
                        text("→")
                            .size(FONT_META)
                            .font(ui_font())
                            .color(chrome.text_muted),
                        container(space::horizontal().width(6).height(6)).style(move |_| {
                            container::Style {
                                background: Some(Background::Color(chrome.accent)),
                                border: Border {
                                    radius: 999.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            }
                        }),
                        text(target_label)
                            .size(FONT_META)
                            .font(mono_font())
                            .color(chrome.text_primary),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .wrap()
                    .vertical_spacing(4),
                )
                .padding([6, 10])
                .width(Length::Fill)
                .style(move |_| inset_well_style(chrome)),
            );
        } else {
            let mut route_flow = row![
                badge_pill(
                    "Local",
                    chrome.success,
                    chrome.success_soft,
                    Color::TRANSPARENT,
                ),
            ]
            .spacing(5)
            .align_y(Alignment::Center);

            for (idx, hop) in hops.iter().enumerate() {
                route_flow = route_flow
                    .push(
                        text("→")
                            .size(FONT_META)
                            .font(ui_font())
                            .color(chrome.text_muted),
                    )
                    .push(badge_pill(
                        format!("{}. {}", idx + 1, hop),
                        chrome.accent,
                        chrome.accent_soft,
                        chrome.border_subtle,
                    ));
            }
            let route_flow = route_flow
                .push(
                    text("→")
                        .size(FONT_META)
                        .font(ui_font())
                        .color(chrome.text_muted),
                )
                .push(badge_pill(
                    target_label,
                    chrome.text_emphasis,
                    chrome.secondary,
                    chrome.border_subtle,
                ))
                .wrap()
                .vertical_spacing(6);

            let mut chain_col = column![route_flow, hairline_h(chrome)]
                .spacing(8)
                .width(Length::Fill);

            let total_hops = hops.len();
            let own_name = self.name.trim();
            for (idx, hop) in hops.into_iter().enumerate() {
                let matched = self
                    .jump_host_candidates
                    .iter()
                    .find(|c| c.token.eq_ignore_ascii_case(&hop));
                let is_self_cycle = !own_name.is_empty() && hop.eq_ignore_ascii_case(own_name);

                let (badge_lbl, badge_fg, badge_bg, subtitle) = if is_self_cycle {
                    (
                        "Cycle!",
                        chrome.danger,
                        chrome.danger_soft,
                        "Points to this profile itself — remove to avoid loop".to_string(),
                    )
                } else if let Some(cand) = matched {
                    let (lbl, fg, bg) = match cand.kind {
                        JumpHostKind::SavedProfile => ("Saved", chrome.accent, chrome.accent_soft),
                        JumpHostKind::SystemSsh => {
                            ("SSH Config", chrome.success, chrome.success_soft)
                        }
                    };
                    let detail = if cand.transitive_hops.is_empty() {
                        cand.endpoint.clone()
                    } else {
                        format!(
                            "{} · +transitive: {}",
                            cand.endpoint,
                            cand.transitive_hops.join(" → ")
                        )
                    };
                    (lbl, fg, bg, detail)
                } else {
                    (
                        "Custom",
                        chrome.text_secondary,
                        chrome.secondary,
                        "Direct [user@]host[:port] or alias".to_string(),
                    )
                };

                let step_badge = badge_pill(
                    format!("#{}", idx + 1),
                    chrome.text_secondary,
                    chrome.secondary,
                    chrome.border_subtle,
                );
                let origin_badge = badge_pill(badge_lbl, badge_fg, badge_bg, Color::TRANSPARENT);

                let info_col = column![
                    row![
                        text(hop)
                            .size(FONT_BODY)
                            .font(ui_font_medium())
                            .color(chrome.text_primary),
                        origin_badge,
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .wrap()
                    .vertical_spacing(2),
                    text(subtitle)
                        .size(FONT_META)
                        .font(mono_font())
                        .color(if is_self_cycle {
                            chrome.danger
                        } else {
                            chrome.text_muted
                        }),
                ]
                .spacing(2)
                .width(Length::Fill);

                let move_up_btn = button(
                    text("↑")
                        .size(FONT_META)
                        .font(ui_font_semibold()),
                )
                .padding([3, 7])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press_maybe((idx > 0).then_some(Message::ProfileFormMoveJumpHop {
                    index: idx,
                    delta: -1,
                }));

                let move_down_btn = button(
                    text("↓")
                        .size(FONT_META)
                        .font(ui_font_semibold()),
                )
                .padding([3, 7])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press_maybe((idx + 1 < total_hops).then_some(
                    Message::ProfileFormMoveJumpHop {
                        index: idx,
                        delta: 1,
                    },
                ));

                let remove_btn = button(text("×").size(FONT_BODY).font(ui_font()))
                    .padding([2, 8])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(Message::ProfileFormRemoveJumpHop(idx));

                let hop_row = row![
                    step_badge,
                    info_col,
                    move_up_btn,
                    move_down_btn,
                    remove_btn,
                ]
                .spacing(6)
                .padding([2, 2])
                .align_y(Alignment::Center);

                chain_col = chain_col.push(hop_row);
                if idx + 1 < total_hops {
                    chain_col = chain_col.push(hairline_h(chrome));
                }
            }

            controls_col = controls_col.push(
                container(chain_col)
                    .padding(10)
                    .width(Length::Fill)
                    .style(move |_| inset_well_style(chrome)),
            );
        }

        column![header_row, controls_col]
            .spacing(6)
            .width(Length::Fill)
            .into()
    }

    fn view_ssh_advanced_tab<'a>(
        &'a self,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let ka_interval = field_stack(
            "Keepalive interval (s)",
            form_input(
                "Global default (e.g. 15)",
                &self.ssh_keepalive_interval,
                Message::ProfileFormKeepaliveIntervalChanged,
                chrome,
            )
            .font(mono_font())
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let ka_missed = field_stack(
            "Max missed probes",
            form_input(
                "Global default (e.g. 3)",
                &self.ssh_keepalive_missed,
                Message::ProfileFormKeepaliveMissedChanged,
                chrome,
            )
            .font(mono_font())
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let keepalive_row: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![ka_interval.width(Length::Fill), ka_missed.width(Length::Fill)]
                .spacing(10)
                .width(Length::Fill)
                .into()
        } else {
            row![ka_interval.width(Length::Fill), ka_missed.width(Length::Fill)]
                .spacing(12)
                .width(Length::Fill)
                .into()
        };

        let mut col = column![
            field_stack(
                "ProxyCommand",
                form_input(
                    "e.g. cloudflared access ssh --hostname %h",
                    &self.proxy_command,
                    Message::ProfileFormProxyCommandChanged,
                    chrome,
                )
                .font(mono_font())
                .width(Length::Fill)
                .into(),
                chrome,
            ),
            keepalive_row,
            tabby_switch(
                self.forward_agent,
                "Forward SSH Agent",
                Some("Expose local SSH agent socket to the remote session"),
                Message::ProfileFormForwardAgentToggled,
                chrome,
            ),
            tabby_switch(
                self.forward_x11,
                "Forward X11 ($DISPLAY)",
                Some("Enable trusted X11 display forwarding"),
                Message::ProfileFormForwardX11Toggled,
                chrome,
            ),
            tabby_switch(
                self.auto_attach_mux,
                "Auto-attach multiplexer (tmux/screen)",
                Some("Attach or restore remote tmux session after connect"),
                Message::ProfileFormAutoMuxToggled,
                chrome,
            ),
        ]
        .spacing(14)
        .width(Length::Fill);

        if self.auto_attach_mux {
            col = col.push(field_stack(
                "Multiplexer command",
                form_input(
                    "tmux new-session -A -s main",
                    &self.mux_attach_command,
                    Message::ProfileFormMuxCommandChanged,
                    chrome,
                )
                .font(mono_font())
                .width(Length::Fill)
                .into(),
                chrome,
            ));
        }

        col.into()
    }

    fn view_local_general_tab<'a>(
        &'a self,
        chrome: ChromePalette,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        column![
            field_stack(
                "Shell",
                form_input(
                    "Optional shell path (default OS shell)",
                    &self.local_shell,
                    Message::ProfileFormLocalShellChanged,
                    chrome,
                )
                .font(mono_font())
                .width(Length::Fill)
                .into(),
                chrome,
            ),
            field_stack(
                "Working directory",
                form_input(
                    "Optional startup folder",
                    &self.local_working_dir,
                    Message::ProfileFormLocalDirChanged,
                    chrome,
                )
                .font(mono_font())
                .width(Length::Fill)
                .into(),
                chrome,
            ),
        ]
        .spacing(14)
        .width(Length::Fill)
        .into()
    }

    fn view_serial_general_tab<'a>(
        &'a self,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let port_field = field_stack(
            "Serial device",
            form_input(
                "/dev/ttyUSB0 or COM3",
                &self.serial_port,
                Message::ProfileFormSerialPortChanged,
                chrome,
            )
            .font(mono_font())
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let baud_field = field_stack(
            "Baud rate",
            form_input(
                "115200",
                &self.serial_baud_rate,
                Message::ProfileFormSerialBaudChanged,
                chrome,
            )
            .font(mono_font())
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let port_baud_row: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![port_field.width(Length::Fill), baud_field.width(Length::Fill)]
                .spacing(10)
                .width(Length::Fill)
                .into()
        } else {
            row![port_field.width(Length::Fill), baud_field.width(Length::Fixed(110.0))]
                .spacing(12)
                .width(Length::Fill)
                .into()
        };

        let in_track = container(
            row![
                serial_in_btn("Text", SerialInputMode::Text, self.serial_input_mode, chrome),
                serial_in_btn("Readline", SerialInputMode::Readline, self.serial_input_mode, chrome),
                serial_in_btn("Hex", SerialInputMode::HexBytes, self.serial_input_mode, chrome),
            ]
            .spacing(2)
            .width(Length::Fill),
        )
        .padding(3)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.input_bg)),
            border: Border {
                color: chrome.border_subtle,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        });

        let out_track = container(
            row![
                serial_out_btn("Text", SerialOutputMode::Text, self.serial_output_mode, chrome),
                serial_out_btn("Hexdump", SerialOutputMode::Hexdump, self.serial_output_mode, chrome),
            ]
            .spacing(2)
            .width(Length::Fill),
        )
        .padding(3)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.input_bg)),
            border: Border {
                color: chrome.border_subtle,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        });

        column![
            port_baud_row,
            field_stack("Input mode", in_track.into(), chrome),
            field_stack("Output mode", out_track.into(), chrome),
        ]
        .spacing(14)
        .width(Length::Fill)
        .into()
    }

    fn view_telnet_general_tab<'a>(
        &'a self,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let host_field = field_stack(
            "Host",
            form_input(
                "router.local",
                &self.telnet_host,
                Message::ProfileFormTelnetHostChanged,
                chrome,
            )
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let port_field = field_stack(
            "Port",
            form_input(
                "23",
                &self.telnet_port,
                Message::ProfileFormTelnetPortChanged,
                chrome,
            )
            .width(Length::Fill)
            .into(),
            chrome,
        );
        if compact {
            column![host_field.width(Length::Fill), port_field.width(Length::Fill)]
                .spacing(10)
                .width(Length::Fill)
                .into()
        } else {
            row![host_field.width(Length::Fill), port_field.width(Length::Fixed(92.0))]
                .spacing(12)
                .width(Length::Fill)
                .into()
        }
    }

    fn view_docker_general_tab<'a>(
        &'a self,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let rt_track = container(
            row![
                docker_rt_btn("Docker", DockerRuntime::Docker, self.docker_runtime, chrome),
                docker_rt_btn("Podman", DockerRuntime::Podman, self.docker_runtime, chrome),
            ]
            .spacing(2)
            .width(Length::Fill),
        )
        .padding(3)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.input_bg)),
            border: Border {
                color: chrome.border_subtle,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        });

        let user_field = field_stack(
            "User (-u)",
            form_input(
                "Optional container user",
                &self.docker_user,
                Message::ProfileFormDockerUserChanged,
                chrome,
            )
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let workdir_field = field_stack(
            "Working dir (-w)",
            form_input(
                "Optional folder inside container",
                &self.docker_working_dir,
                Message::ProfileFormDockerWorkdirChanged,
                chrome,
            )
            .font(mono_font())
            .width(Length::Fill)
            .into(),
            chrome,
        );
        let user_workdir_row: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![user_field.width(Length::Fill), workdir_field.width(Length::Fill)]
                .spacing(10)
                .width(Length::Fill)
                .into()
        } else {
            row![user_field.width(Length::Fill), workdir_field.width(Length::Fill)]
                .spacing(12)
                .width(Length::Fill)
                .into()
        };

        column![
            field_stack("Runtime", rt_track.into(), chrome),
            field_stack(
                "Container",
                form_input(
                    "Container name or ID",
                    &self.docker_container,
                    Message::ProfileFormDockerContainerChanged,
                    chrome,
                )
                .font(mono_font())
                .width(Length::Fill)
                .into(),
                chrome,
            ),
            field_stack(
                "Shell",
                form_input(
                    "Optional shell (e.g. /bin/bash)",
                    &self.docker_shell,
                    Message::ProfileFormDockerShellChanged,
                    chrome,
                )
                .font(mono_font())
                .width(Length::Fill)
                .into(),
                chrome,
            ),
            user_workdir_row,
        ]
        .spacing(14)
        .width(Length::Fill)
        .into()
    }

    fn view_login_scripts_editor<'a>(
        &'a self,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let draft_inputs: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![
                form_input(
                    "Expect (e.g. login:)",
                    &self.script_expect_draft,
                    Message::ProfileFormScriptExpectChanged,
                    chrome,
                )
                .width(Length::Fill),
                form_input(
                    "Send text",
                    &self.script_send_draft,
                    Message::ProfileFormScriptSendChanged,
                    chrome,
                )
                .width(Length::Fill),
            ]
            .spacing(8)
            .width(Length::Fill)
            .into()
        } else {
            row![
                form_input(
                    "Expect (e.g. login:)",
                    &self.script_expect_draft,
                    Message::ProfileFormScriptExpectChanged,
                    chrome,
                )
                .width(Length::Fill),
                form_input(
                    "Send text",
                    &self.script_send_draft,
                    Message::ProfileFormScriptSendChanged,
                    chrome,
                )
                .width(Length::Fill),
            ]
            .spacing(8)
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .into()
        };

        let mut col = column![
            draft_inputs,
            row![
                apple_checkbox(
                    self.script_is_regex_draft,
                    "Match Expect as regular expression",
                    Message::ProfileFormScriptRegexToggled,
                    chrome,
                ),
                space::horizontal(),
                button(text("+ Add Step").size(FONT_META).font(ui_font_medium()))
                    .padding([5, 11])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::ProfileFormAddScriptStep),
            ]
            .spacing(8)
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .wrap()
            .vertical_spacing(6),
        ]
        .spacing(10)
        .width(Length::Fill);

        if !self.login_scripts.is_empty() {
            col = col.push(hairline_h(chrome));
            let total = self.login_scripts.len();
            for (idx, step) in self.login_scripts.iter().enumerate() {
                col = col.push(
                    row![
                        badge_pill(
                            if step.is_regex { "Regex" } else { "Text" },
                            chrome.text_secondary,
                            chrome.bg_less_2,
                            chrome.border_subtle,
                        ),
                        text(format!("{} -> {}", step.expect, step.send))
                            .size(FONT_META)
                            .font(mono_font())
                            .color(chrome.text_secondary)
                            .width(Length::Fill),
                        button(text("×").size(FONT_BODY))
                            .padding([3, 8])
                            .style(pill_button_style(chrome, PillTone::Danger))
                            .on_press(Message::ProfileFormRemoveScriptStep(idx)),
                    ]
                    .spacing(8)
                    .padding([3, 2])
                    .align_y(Alignment::Center),
                );
                if idx + 1 < total {
                    col = col.push(hairline_h(chrome));
                }
            }
        }

        col.into()
    }

    fn view_attached_tunnels_editor<'a>(
        &'a self,
        chrome: ChromePalette,
        compact: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let name_host_row: Element<'a, Message, Theme, iced::Renderer> = if compact {
            column![
                form_input(
                    "Tunnel name",
                    &self.tunnel_name_draft,
                    Message::ProfileFormTunnelNameChanged,
                    chrome,
                )
                .width(Length::Fill),
                form_input(
                    "Target host (e.g. 127.0.0.1)",
                    &self.tunnel_target_host_draft,
                    Message::ProfileFormTunnelTargetHostChanged,
                    chrome,
                )
                .width(Length::Fill),
            ]
            .spacing(8)
            .width(Length::Fill)
            .into()
        } else {
            row![
                form_input(
                    "Tunnel name",
                    &self.tunnel_name_draft,
                    Message::ProfileFormTunnelNameChanged,
                    chrome,
                )
                .width(Length::FillPortion(2)),
                form_input(
                    "Target host (e.g. 127.0.0.1)",
                    &self.tunnel_target_host_draft,
                    Message::ProfileFormTunnelTargetHostChanged,
                    chrome,
                )
                .width(Length::FillPortion(3)),
            ]
            .spacing(8)
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .into()
        };

        let mut col = column![
            name_host_row,
            row![
                form_input(
                    "Bind port",
                    &self.tunnel_bind_port_draft,
                    Message::ProfileFormTunnelBindPortChanged,
                    chrome,
                )
                .width(Length::Fixed(92.0)),
                form_input(
                    "Target port",
                    &self.tunnel_target_port_draft,
                    Message::ProfileFormTunnelTargetPortChanged,
                    chrome,
                )
                .width(Length::Fixed(92.0)),
                space::horizontal(),
                button(text("+ Add Tunnel").size(FONT_META).font(ui_font_medium()))
                    .padding([5, 11])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::ProfileFormAddTunnel),
            ]
            .spacing(8)
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .wrap()
            .vertical_spacing(6),
        ]
        .spacing(10)
        .width(Length::Fill);

        if self.tunnels.is_empty() {
            col = col.push(
                text("No forwarded ports attached to this profile yet.")
                    .size(FONT_META)
                    .color(chrome.text_muted),
            );
        } else {
            col = col.push(hairline_h(chrome));
            let total = self.tunnels.len();
            for (idx, tr) in self.tunnels.iter().enumerate() {
                let desc = format!(
                    "{}: {}:{} -> {}:{}",
                    match tr.direction {
                        TunnelDirection::Local => "Local (-L)",
                        TunnelDirection::Remote => "Remote (-R)",
                        TunnelDirection::Dynamic => "SOCKS5 (-D)",
                    },
                    tr.bind_host,
                    tr.bind_port,
                    tr.target_host.as_deref().unwrap_or("*"),
                    tr.target_port.unwrap_or(0)
                );
                col = col.push(
                    row![
                        column![
                            text(tr.name.as_str())
                                .size(FONT_BODY)
                                .color(chrome.text_primary),
                            text(desc)
                                .size(FONT_META)
                                .font(mono_font())
                                .color(chrome.text_muted),
                        ]
                        .spacing(1)
                        .width(Length::Fill),
                        button(text("×").size(FONT_BODY))
                            .padding([3, 8])
                            .style(pill_button_style(chrome, PillTone::Danger))
                            .on_press(Message::ProfileFormRemoveTunnel(idx)),
                    ]
                    .spacing(8)
                    .padding([3, 2])
                    .align_y(Alignment::Center),
                );
                if idx + 1 < total {
                    col = col.push(hairline_h(chrome));
                }
            }
        }

        col.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JumpDropdownOption {
    token: String,
    label: String,
}

impl fmt::Display for JumpDropdownOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

fn field_stack<'a>(
    label: &'static str,
    control: Element<'a, Message, Theme, iced::Renderer>,
    chrome: ChromePalette,
) -> iced::widget::Column<'a, Message, Theme, iced::Renderer> {
    column![
        text(label)
            .size(FONT_BODY)
            .font(ui_font())
            .color(chrome.text_secondary),
        control,
    ]
    .spacing(5)
}

fn form_input<'a>(
    placeholder: &'static str,
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
    chrome: ChromePalette,
) -> text_input::TextInput<'a, Message, Theme, iced::Renderer> {
    text_input(placeholder, value)
        .on_input(on_input)
        .size(FONT_BODY)
        .padding([8, 10])
        .style(styled_input_style(chrome))
}

fn kind_btn<'a>(
    label: &'static str,
    choice: ProfileKindChoice,
    current: ProfileKindChoice,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let active = choice == current;
    button(
        container(text(label).size(FONT_META).font(ui_font())).center_x(Length::Fill),
    )
    .padding([6, 4])
    .width(Length::Fill)
    .style(tabby_auth_tile_style(chrome, active))
    .on_press(Message::ProfileFormKindChanged(choice))
    .into()
}

fn centered_tile_btn<'a>(
    label: &'static str,
    active: bool,
    on_press: Message,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    button(container(text(label).size(12.0).font(ui_font())).center_x(Length::Fill))
        .padding([7, 8])
        .width(Length::Fill)
        .style(tabby_auth_tile_style(chrome, active))
        .on_press(on_press)
        .into()
}

fn serial_in_btn<'a>(
    label: &'static str,
    choice: SerialInputMode,
    current: SerialInputMode,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    centered_tile_btn(
        label,
        choice == current,
        Message::ProfileFormSerialInputModeChanged(choice),
        chrome,
    )
}

fn serial_out_btn<'a>(
    label: &'static str,
    choice: SerialOutputMode,
    current: SerialOutputMode,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    centered_tile_btn(
        label,
        choice == current,
        Message::ProfileFormSerialOutputModeChanged(choice),
        chrome,
    )
}

fn docker_rt_btn<'a>(
    label: &'static str,
    choice: DockerRuntime,
    current: DockerRuntime,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    centered_tile_btn(
        label,
        choice == current,
        Message::ProfileFormDockerRuntimeChanged(choice),
        chrome,
    )
}

struct ProfileAuthIcon {
    choice: ProfileAuthChoice,
    color: Color,
}

impl ProfileAuthIcon {
    fn new<'a>(
        choice: ProfileAuthChoice,
        color: Color,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self { choice, color })
    }
}

impl Widget<Message, Theme, iced::Renderer> for ProfileAuthIcon {
    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(14.0), Length::Fixed(14.0))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(Length::Fixed(14.0), Length::Fixed(14.0), Size::ZERO);
        layout::Node::new(size)
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let x0 = bounds.x.round() + 1.0;
        let y0 = bounds.y.round() + 1.0;
        let c = self.color;

        match self.choice {
            ProfileAuthChoice::Agent => {
                // Spy/agent silhouette (hat + head + collar).
                auth_fill(renderer, Rectangle::new([x0 + 3.0, y0 + 1.0].into(), [6.0, 2.5].into()), c);
                auth_fill(renderer, Rectangle::new([x0 + 1.5, y0 + 3.5].into(), [9.0, 1.2].into()), c);
                auth_stroke(renderer, Rectangle::new([x0 + 3.5, y0 + 5.0].into(), [5.0, 3.5].into()), c);
                auth_fill(renderer, Rectangle::new([x0 + 2.0, y0 + 9.5].into(), [8.0, 2.0].into()), c);
            }
            ProfileAuthChoice::PublicKey => {
                // Classic key ring + shaft + notch.
                auth_stroke(renderer, Rectangle::new([x0 + 1.0, y0 + 3.0].into(), [5.0, 5.0].into()), c);
                auth_fill(renderer, Rectangle::new([x0 + 5.5, y0 + 5.0].into(), [6.0, 1.5].into()), c);
                auth_fill(renderer, Rectangle::new([x0 + 8.5, y0 + 6.5].into(), [1.2, 2.0].into()), c);
                auth_fill(renderer, Rectangle::new([x0 + 10.5, y0 + 6.5].into(), [1.2, 2.0].into()), c);
            }
            ProfileAuthChoice::Password => {
                // Padlock body + shackle.
                auth_stroke(renderer, Rectangle::new([x0 + 3.5, y0 + 1.0].into(), [5.0, 4.5].into()), c);
                auth_fill(renderer, Rectangle::new([x0 + 2.0, y0 + 5.0].into(), [8.0, 6.0].into()), c);
            }
            ProfileAuthChoice::KeyboardInteractive => {
                // Keyboard outline + key rows.
                auth_stroke(renderer, Rectangle::new([x0, y0 + 2.5].into(), [12.0, 7.5].into()), c);
                for kx in [2.0, 4.5, 7.0, 9.0] {
                    auth_fill(renderer, Rectangle::new([x0 + kx, y0 + 4.5].into(), [1.5, 1.2].into()), c);
                }
                auth_fill(renderer, Rectangle::new([x0 + 3.0, y0 + 7.0].into(), [6.0, 1.2].into()), c);
            }
        }
    }
}

fn auth_fill(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    use iced::advanced::renderer::Renderer as _;
    renderer.fill_quad(
        Quad {
            bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(color),
    );
}

fn auth_stroke(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    use iced::advanced::renderer::Renderer as _;
    renderer.fill_quad(
        Quad {
            bounds,
            border: Border {
                color,
                width: 1.0,
                radius: 1.5.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(Color::TRANSPARENT),
    );
}

fn auth_btn<'a>(
    label: &'static str,
    choice: ProfileAuthChoice,
    current: ProfileAuthChoice,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let active = choice == current;
    let icon_color = if active {
        chrome.text_emphasis
    } else {
        chrome.text_muted
    };
    button(
        container(
            column![
                ProfileAuthIcon::new(choice, icon_color),
                text(label).size(11.5).font(ui_font()),
            ]
            .spacing(5)
            .align_x(Alignment::Center),
        )
        .center_x(Length::Fill),
    )
    .padding([9, 6])
    .width(Length::Fill)
    .style(tabby_auth_tile_style(chrome, active))
    .on_press(Message::ProfileFormAuthChanged(choice))
    .into()
}
