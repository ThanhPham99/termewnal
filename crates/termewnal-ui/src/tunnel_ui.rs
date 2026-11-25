use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use iced::widget::{
    Id, button, column, container, mouse_area, opaque, row, rule, scrollable, space, text,
    text_input,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Shadow, Theme, Vector};
use termewnal_core::{
    DEFAULT_TUNNEL_HOLD_GRACE_SECS, Profile, ProfileTarget, ThemePalette, TunnelDirection,
    TunnelRule,
};
use termewnal_ssh::{TunnelManagerSnapshot, TunnelState};

use crate::app::{Message, SessionId};
use crate::sidebar::{ActiveTunnelEntry, TunnelRuntimeState};

const MODAL_WIDTH: f32 = 540.0;

/// Returns the static widget [`Id`] for the Tunnel creation form name input.
pub const fn tunnel_name_input_id() -> Id {
    Id::new("tunnel-create-name-input")
}

/// Returns the static widget [`Id`] for the Tunnel bind host input.
pub const fn tunnel_bind_host_input_id() -> Id {
    Id::new("tunnel-create-bind-host-input")
}

/// Returns the static widget [`Id`] for the Tunnel bind port input.
pub const fn tunnel_bind_port_input_id() -> Id {
    Id::new("tunnel-create-bind-port-input")
}

/// Returns the static widget [`Id`] for the Tunnel target host input.
pub const fn tunnel_target_host_input_id() -> Id {
    Id::new("tunnel-create-target-host-input")
}

/// Returns the static widget [`Id`] for the Tunnel target port input.
pub const fn tunnel_target_port_input_id() -> Id {
    Id::new("tunnel-create-target-port-input")
}

/// Runtime-defined `Tunnel` rule attached to a Quick Connect / System SSH session or standalone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdHocTunnelRule {
    pub rule: TunnelRule,
    pub profile_id: Option<String>,
    pub session_id: Option<SessionId>,
}

/// State backing the modal form for defining a Local (`-L`) or Dynamic SOCKS5 (`-D`) `Tunnel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelCreateForm {
    pub open: bool,
    pub name: String,
    pub profile_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub direction: TunnelDirection,
    pub bind_host: String,
    pub bind_port: String,
    pub target_host: String,
    pub target_port: String,
    pub standalone: bool,
    pub hold_grace_secs: String,
    pub validation_error: Option<String>,
}

impl Default for TunnelCreateForm {
    fn default() -> Self {
        Self {
            open: false,
            name: String::new(),
            profile_id: None,
            session_id: None,
            direction: TunnelDirection::Local,
            bind_host: "127.0.0.1".to_string(),
            bind_port: "5432".to_string(),
            target_host: "127.0.0.1".to_string(),
            target_port: "5432".to_string(),
            standalone: false,
            hold_grace_secs: DEFAULT_TUNNEL_HOLD_GRACE_SECS.to_string(),
            validation_error: None,
        }
    }
}

impl TunnelCreateForm {
    /// Opens the modal seeded with default loopback (`127.0.0.1`) settings.
    pub fn open_for(&mut self, profile_id: Option<String>, session_id: Option<SessionId>) {
        *self = Self {
            open: true,
            profile_id,
            session_id,
            ..Self::default()
        };
    }

    /// Closes the modal and clears transient validation errors.
    pub fn close(&mut self) {
        self.open = false;
        self.validation_error = None;
    }

    /// Validates form inputs and constructs a [`TunnelRule`] (`allow_public_bind` starts `false`).
    pub fn build_rule(&self) -> Result<TunnelRule, String> {
        let direction = match self.direction {
            TunnelDirection::Local => TunnelDirection::Local,
            TunnelDirection::Dynamic => TunnelDirection::Dynamic,
            TunnelDirection::Remote => {
                return Err("Remote (-R) tunnel creation is scheduled for a future update".to_string());
            }
        };

        let bind_host = {
            let trimmed = self.bind_host.trim();
            if trimmed.is_empty() {
                "127.0.0.1".to_string()
            } else {
                trimmed.to_string()
            }
        };

        let bind_port: u16 = self
            .bind_port
            .trim()
            .parse()
            .map_err(|_| "Bind port must be a valid TCP port (0..=65535)".to_string())?;

        let (target_host, target_port) = match direction {
            TunnelDirection::Dynamic => (None, None),
            TunnelDirection::Local => {
                let th = self.target_host.trim();
                if th.is_empty() {
                    return Err("Target host is required for Local (-L) tunnels".to_string());
                }
                let tp: u16 = self
                    .target_port
                    .trim()
                    .parse()
                    .ok()
                    .filter(|&p| p > 0)
                    .ok_or_else(|| {
                        "Target port must be a valid non-zero TCP port (1..=65535)".to_string()
                    })?;
                (Some(th.to_string()), Some(tp))
            }
            TunnelDirection::Remote => unreachable!(),
        };

        let hold_grace_secs = {
            let trimmed = self.hold_grace_secs.trim();
            if trimmed.is_empty() {
                None
            } else {
                let secs: u64 = trimmed
                    .parse()
                    .ok()
                    .filter(|&s| s > 0)
                    .ok_or_else(|| "Hold grace timeout must be a positive number of seconds".to_string())?;
                Some(secs)
            }
        };

        let name = {
            let trimmed = self.name.trim();
            if trimmed.is_empty() {
                let prefix = match direction {
                    TunnelDirection::Local => "Local",
                    TunnelDirection::Dynamic => "SOCKS5",
                    TunnelDirection::Remote => "Remote",
                };
                format!("{prefix} {bind_host}:{bind_port}")
            } else {
                trimmed.to_string()
            }
        };

        let mut rule = match direction {
            TunnelDirection::Dynamic => TunnelRule::dynamic_socks5(name, bind_port),
            TunnelDirection::Local => {
                let (th, tp) = target_host.zip(target_port).expect("validated local target");
                TunnelRule::local(name, bind_port, th, tp)
            }
            TunnelDirection::Remote => unreachable!(),
        };
        rule.bind_host = bind_host;
        rule.standalone = self.standalone;
        rule.hold_grace_secs = hold_grace_secs;
        Ok(rule)
    }
}

/// Explicit security warning confirmation state shown before binding a non-loopback address (`0.0.0.0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPublicBindWarning {
    pub rule: TunnelRule,
    pub profile_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub start_after_confirm: bool,
}

impl PendingPublicBindWarning {
    /// Returns explicit security warning copy explaining LAN exposure vs `127.0.0.1` loopback.
    pub fn warning_text(&self) -> String {
        format!(
            "SECURITY WARNING: Binding tunnel '{}' to {}:{} exposes this forwarded port to external / LAN hosts instead of restricting access to 127.0.0.1 (localhost). Only allow public bind if your network and target service are trusted.",
            self.rule.name,
            self.rule.effective_bind_host(),
            self.rule.bind_port
        )
    }
}

/// Top-level UI state for `Tunnel` creation, public-bind confirmation, and runtime error tracking.
#[derive(Debug, Default)]
pub struct TunnelUiState {
    create_form: TunnelCreateForm,
    pending_public_bind: Option<PendingPublicBindWarning>,
    ad_hoc_tunnels: Vec<AdHocTunnelRule>,
    manually_stopped: BTreeSet<String>,
    start_errors: BTreeMap<String, String>,
}

impl TunnelUiState {
    /// Returns the `Tunnel` creation modal form state.
    pub fn create_form(&self) -> &TunnelCreateForm {
        &self.create_form
    }

    /// Returns a mutable reference to the `Tunnel` creation modal form state.
    pub fn create_form_mut(&mut self) -> &mut TunnelCreateForm {
        &mut self.create_form
    }

    /// Returns the pending `0.0.0.0` public-bind security warning prompt, if active.
    pub fn pending_public_bind_warning(&self) -> Option<&PendingPublicBindWarning> {
        self.pending_public_bind.as_ref()
    }

    /// Opens the public-bind security warning modal requiring explicit user opt-in.
    pub fn prompt_public_bind(
        &mut self,
        rule: TunnelRule,
        profile_id: Option<String>,
        session_id: Option<SessionId>,
        start_after_confirm: bool,
    ) {
        self.pending_public_bind = Some(PendingPublicBindWarning {
            rule,
            profile_id,
            session_id,
            start_after_confirm,
        });
    }

    /// Dismisses the pending public-bind warning without opting in.
    pub fn cancel_public_bind(&mut self) {
        self.pending_public_bind = None;
    }

    /// Consumes and confirms the pending public-bind warning, marking `allow_public_bind = true`.
    pub fn confirm_public_bind(&mut self) -> Option<PendingPublicBindWarning> {
        let mut pending = self.pending_public_bind.take()?;
        pending.rule.allow_public_bind = true;
        if let Some(entry) = self
            .ad_hoc_tunnels
            .iter_mut()
            .find(|t| t.rule.id == pending.rule.id)
        {
            entry.rule.allow_public_bind = true;
        }
        Some(pending)
    }

    /// Registers an ad-hoc `TunnelRule` not persisted inside a saved `Profile`.
    pub fn add_ad_hoc(&mut self, entry: AdHocTunnelRule) {
        if let Some(existing) = self
            .ad_hoc_tunnels
            .iter_mut()
            .find(|t| t.rule.id == entry.rule.id)
        {
            *existing = entry;
        } else {
            self.ad_hoc_tunnels.push(entry);
        }
    }

    /// Returns all ad-hoc `TunnelRule` definitions.
    pub fn ad_hoc_tunnels(&self) -> &[AdHocTunnelRule] {
        &self.ad_hoc_tunnels
    }

    /// Finds an ad-hoc `TunnelRule` by ID.
    pub fn find_ad_hoc(&self, tunnel_id: &str) -> Option<&AdHocTunnelRule> {
        self.ad_hoc_tunnels.iter().find(|t| t.rule.id == tunnel_id)
    }

    /// Removes session-bound ad-hoc `Tunnel`s tied to a closed `SessionId`.
    pub fn prune_session(&mut self, session_id: SessionId) {
        self.ad_hoc_tunnels
            .retain(|entry| entry.rule.standalone || entry.session_id != Some(session_id));
    }

    /// Tracks whether the user explicitly stopped a `Tunnel` so reconnects won't auto-restart it.
    pub fn mark_manually_stopped(&mut self, tunnel_id: &str, stopped: bool) {
        if stopped {
            self.manually_stopped.insert(tunnel_id.to_string());
        } else {
            self.manually_stopped.remove(tunnel_id);
        }
    }

    /// Returns `true` if `tunnel_id` was explicitly stopped by the user.
    pub fn is_manually_stopped(&self, tunnel_id: &str) -> bool {
        self.manually_stopped.contains(tunnel_id)
    }

    /// Records or clears a pre-bind start error (e.g. `EADDRINUSE` or missing session).
    pub fn set_start_error(&mut self, tunnel_id: impl Into<String>, error: Option<String>) {
        let id = tunnel_id.into();
        match error {
            Some(err) => {
                self.start_errors.insert(id, err);
            }
            None => {
                self.start_errors.remove(&id);
            }
        }
    }

    /// Returns any pre-bind start error recorded for `tunnel_id`.
    pub fn start_error(&self, tunnel_id: &str) -> Option<&str> {
        self.start_errors.get(tunnel_id).map(String::as_str)
    }

    /// Renders the `Tunnel` creation modal overlay when open.
    pub fn view_create_modal<'a>(
        &'a self,
        profiles: &'a [Profile],
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.create_form.open {
            return None;
        }

        let form = &self.create_form;
        let is_local = form.direction == TunnelDirection::Local;

        let dir_row = row![
            text("Direction:").size(12),
            button(text("Local (-L)").size(12))
                .padding([4, 10])
                .style(if is_local {
                    button::primary
                } else {
                    button::secondary
                })
                .on_press(Message::TunnelFormDirectionChanged(TunnelDirection::Local)),
            button(text("Dynamic SOCKS5 (-D)").size(12))
                .padding([4, 10])
                .style(if !is_local {
                    button::primary
                } else {
                    button::secondary
                })
                .on_press(Message::TunnelFormDirectionChanged(
                    TunnelDirection::Dynamic
                )),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let ssh_profiles: Vec<&Profile> = profiles
            .iter()
            .filter(|p| matches!(p.target, ProfileTarget::Ssh { .. }))
            .collect();

        let mut profile_row = row![text("SSH Target:").size(12)]
            .spacing(6)
            .align_y(Alignment::Center);
        if ssh_profiles.is_empty() {
            profile_row = profile_row.push(
                text("Active SSH Session")
                    .size(12)
                    .color(Color::from_rgb8(148, 163, 184)),
            );
        } else {
            for prof in ssh_profiles.into_iter().take(4) {
                let selected = form.profile_id.as_deref() == Some(prof.id.as_str());
                profile_row = profile_row.push(
                    button(text(&prof.name).size(11))
                        .padding([3, 8])
                        .style(if selected {
                            button::primary
                        } else {
                            button::subtle
                        })
                        .on_press(Message::TunnelFormProfileSelected(Some(prof.id.clone()))),
                );
            }
        }

        let scope_row = row![
            text("Lifecycle:").size(12),
            button(text("Session-bound").size(12))
                .padding([4, 10])
                .style(if !form.standalone {
                    button::primary
                } else {
                    button::secondary
                })
                .on_press(Message::TunnelFormStandaloneToggled(false)),
            button(text("Standalone (Background)").size(12))
                .padding([4, 10])
                .style(if form.standalone {
                    button::primary
                } else {
                    button::secondary
                })
                .on_press(Message::TunnelFormStandaloneToggled(true)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut fields = column![
            text("New Port Forwarding Tunnel").size(16),
            dir_row,
            profile_row,
            scope_row,
            row![
                text("Name:").size(12).width(Length::Fixed(95.0)),
                text_input("Optional display name", &form.name)
                    .id(tunnel_name_input_id())
                    .on_input(Message::TunnelFormNameChanged)
                    .size(12)
                    .padding([5, 8]),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            row![
                text("Bind Host:").size(12).width(Length::Fixed(95.0)),
                text_input("127.0.0.1", &form.bind_host)
                    .id(tunnel_bind_host_input_id())
                    .on_input(Message::TunnelFormBindHostChanged)
                    .size(12)
                    .padding([5, 8]),
                text("Port:").size(12),
                text_input("5432", &form.bind_port)
                    .id(tunnel_bind_port_input_id())
                    .on_input(Message::TunnelFormBindPortChanged)
                    .width(Length::Fixed(90.0))
                    .size(12)
                    .padding([5, 8]),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(8);

        if is_local {
            fields = fields.push(
                row![
                    text("Target Host:").size(12).width(Length::Fixed(95.0)),
                    text_input("127.0.0.1", &form.target_host)
                        .id(tunnel_target_host_input_id())
                        .on_input(Message::TunnelFormTargetHostChanged)
                        .size(12)
                        .padding([5, 8]),
                    text("Port:").size(12),
                    text_input("5432", &form.target_port)
                        .id(tunnel_target_port_input_id())
                        .on_input(Message::TunnelFormTargetPortChanged)
                        .width(Length::Fixed(90.0))
                        .size(12)
                        .padding([5, 8]),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }

        fields = fields.push(
            row![
                text("Hold Grace (s):").size(12).width(Length::Fixed(95.0)),
                text_input("10", &form.hold_grace_secs)
                    .on_input(Message::TunnelFormHoldGraceChanged)
                    .width(Length::Fixed(90.0))
                    .size(12)
                    .padding([5, 8]),
                text("Held sockets wait for reconnect before resetting")
                    .size(11)
                    .color(Color::from_rgb8(148, 163, 184)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );

        if let Some(err) = &form.validation_error {
            fields = fields.push(
                text(err.as_str())
                    .size(12)
                    .color(Color::from_rgb8(248, 113, 113)),
            );
        }

        fields = fields.push(rule::horizontal(1));
        fields = fields.push(
            row![
                space::horizontal(),
                button(text("Cancel").size(12))
                    .padding([5, 12])
                    .style(button::subtle)
                    .on_press(Message::CloseTunnelCreateModal),
                button(text("Create & Start").size(12))
                    .padding([5, 14])
                    .style(button::primary)
                    .on_press(Message::SubmitTunnelCreate),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );

        let bg = Color::from_rgb8(palette.background.r, palette.background.g, palette.background.b);
        let fg = Color::from_rgb8(palette.foreground.r, palette.foreground.g, palette.foreground.b);

        let card = opaque(
            container(scrollable(fields.padding(16)))
                .width(Length::Fixed(MODAL_WIDTH))
                .style(move |_| container::Style {
                    background: Some(Background::Color(bg)),
                    border: Border {
                        color: Color::from_rgb8(96, 165, 250),
                        width: 1.0,
                        radius: 8.0.into(),
                    },
                    shadow: Shadow {
                        color: Color::from_rgba(0.0, 0.0, 0.0, 0.5),
                        offset: Vector::new(0.0, 6.0),
                        blur_radius: 18.0,
                    },
                    text_color: Some(fg),
                    ..Default::default()
                }),
        );

        Some(
            mouse_area(
                container(card)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.55))),
                        ..Default::default()
                    }),
            )
            .on_press(Message::CloseTunnelCreateModal)
            .into(),
        )
    }

    /// Renders the `0.0.0.0` public-bind security warning confirmation modal when active.
    pub fn view_public_bind_warning<'a>(
        &'a self,
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        let pending = self.pending_public_bind.as_ref()?;
        let bg = Color::from_rgb8(palette.background.r, palette.background.g, palette.background.b);
        let fg = Color::from_rgb8(palette.foreground.r, palette.foreground.g, palette.foreground.b);

        let card = opaque(
            container(
                column![
                    text("Public Network Bind Warning")
                        .size(17)
                        .color(Color::from_rgb8(251, 191, 36)),
                    text(pending.warning_text()).size(13),
                    rule::horizontal(1),
                    row![
                        button(text("Cancel (Keep Safe)").size(12))
                            .padding([6, 12])
                            .style(button::subtle)
                            .on_press(Message::TunnelPublicBindCancel),
                        space::horizontal(),
                        button(text("Allow Public Bind & Start").size(12))
                            .padding([6, 14])
                            .style(button::danger)
                            .on_press(Message::TunnelPublicBindConfirm),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                ]
                .spacing(12)
                .padding(18),
            )
            .width(Length::Fixed(MODAL_WIDTH))
            .style(move |_| container::Style {
                background: Some(Background::Color(bg)),
                border: Border {
                    color: Color::from_rgb8(251, 191, 36),
                    width: 1.5,
                    radius: 8.0.into(),
                },
                shadow: Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.55),
                    offset: Vector::new(0.0, 8.0),
                    blur_radius: 20.0,
                },
                text_color: Some(fg),
                ..Default::default()
            }),
        );

        Some(
            container(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.6))),
                    ..Default::default()
                })
                .into(),
        )
    }
}

/// Resolves the `known_hosts` path override (if configured for isolated tests).
pub(crate) fn apply_known_hosts_override(
    mut cfg: termewnal_ssh::SshConnectConfig,
    override_path: Option<&PathBuf>,
) -> termewnal_ssh::SshConnectConfig {
    if cfg.known_hosts_path.is_none()
        && let Some(path) = override_path
    {
        cfg.known_hosts_path = Some(path.clone());
    }
    cfg
}

/// Builds sidebar [`ActiveTunnelEntry`] items by merging live [`TunnelManagerSnapshot`] runtime
/// state with configured `Profile.tunnels` and runtime ad-hoc tunnel definitions.
pub fn build_active_tunnel_entries(
    profiles: &[Profile],
    open_ssh_sessions: &BTreeMap<SessionId, Option<String>>,
    manager_snap: &TunnelManagerSnapshot,
    tunnel_ui: &TunnelUiState,
) -> Vec<ActiveTunnelEntry> {
    let mut entries = Vec::new();
    let mut seen_ids = BTreeSet::new();

    for snap in &manager_snap.tunnels {
        seen_ids.insert(snap.id.clone());
        let bind_addr = snap
            .bound_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| format!("{}:{}", snap.bind_host, snap.bind_port));
        let target_addr = format_target_addr(
            snap.direction,
            snap.target_host.as_deref(),
            snap.target_port,
        );
        let state = if let Some(err) = tunnel_ui.start_error(&snap.id) {
            TunnelRuntimeState::Error(err.to_string())
        } else {
            map_tunnel_state(&snap.state)
        };
        entries.push(ActiveTunnelEntry {
            id: snap.id.clone(),
            name: snap.name.clone(),
            direction: snap.direction,
            bind_addr,
            target_addr,
            state,
            standalone: snap.standalone,
            session_id: snap.session_id.map(SessionId),
        });
    }

    for profile in profiles {
        for rule in &profile.tunnels {
            if !seen_ids.insert(rule.id.clone()) {
                continue;
            }
            let matching_session = open_ssh_sessions
                .iter()
                .find_map(|(&sid, pid)| (pid.as_deref() == Some(profile.id.as_str())).then_some(sid));

            if !rule.standalone && matching_session.is_none() {
                continue;
            }

            let state = tunnel_ui
                .start_error(&rule.id)
                .map(|err| TunnelRuntimeState::Error(err.to_string()))
                .unwrap_or(TunnelRuntimeState::Stopped);

            entries.push(ActiveTunnelEntry {
                id: rule.id.clone(),
                name: rule.name.clone(),
                direction: rule.direction,
                bind_addr: format!("{}:{}", rule.effective_bind_host(), rule.bind_port),
                target_addr: format_target_addr(
                    rule.direction,
                    rule.target_host.as_deref(),
                    rule.target_port,
                ),
                state,
                standalone: rule.standalone,
                session_id: if rule.standalone {
                    None
                } else {
                    matching_session
                },
            });
        }
    }

    for ad_hoc in tunnel_ui.ad_hoc_tunnels() {
        let rule = &ad_hoc.rule;
        if !seen_ids.insert(rule.id.clone()) {
            continue;
        }
        let session_alive = ad_hoc
            .session_id
            .is_some_and(|sid| open_ssh_sessions.contains_key(&sid));
        if !rule.standalone && !session_alive {
            continue;
        }
        let state = tunnel_ui
            .start_error(&rule.id)
            .map(|err| TunnelRuntimeState::Error(err.to_string()))
            .unwrap_or(TunnelRuntimeState::Stopped);
        entries.push(ActiveTunnelEntry {
            id: rule.id.clone(),
            name: rule.name.clone(),
            direction: rule.direction,
            bind_addr: format!("{}:{}", rule.effective_bind_host(), rule.bind_port),
            target_addr: format_target_addr(
                rule.direction,
                rule.target_host.as_deref(),
                rule.target_port,
            ),
            state,
            standalone: rule.standalone,
            session_id: if rule.standalone {
                None
            } else {
                ad_hoc.session_id
            },
        });
    }

    entries
}

fn map_tunnel_state(state: &TunnelState) -> TunnelRuntimeState {
    match state {
        TunnelState::Stopped => TunnelRuntimeState::Stopped,
        TunnelState::Starting => TunnelRuntimeState::Starting,
        TunnelState::Active => TunnelRuntimeState::Active,
        TunnelState::Holding => TunnelRuntimeState::Holding,
        TunnelState::Reconnecting => TunnelRuntimeState::Reconnecting,
        TunnelState::Error(msg) => TunnelRuntimeState::Error(msg.clone()),
    }
}

fn format_target_addr(
    direction: TunnelDirection,
    target_host: Option<&str>,
    target_port: Option<u16>,
) -> String {
    match direction {
        TunnelDirection::Dynamic => "SOCKS5".to_string(),
        TunnelDirection::Local | TunnelDirection::Remote => {
            let host = target_host.unwrap_or("127.0.0.1");
            let port = target_port.unwrap_or(0);
            format!("{host}:{port}")
        }
    }
}
