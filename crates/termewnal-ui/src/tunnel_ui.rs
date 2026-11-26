use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::widget::{
    Id, button, column, container, opaque, row, scrollable, space, text, text_input,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Shadow, Theme, Vector};
use termewnal_core::{
    DEFAULT_TUNNEL_HOLD_GRACE_SECS, Profile, ProfileTarget, ThemePalette, TunnelDirection,
    TunnelRule,
};
use termewnal_ssh::{TunnelManagerSnapshot, TunnelState};

use crate::app::{Message, SessionId};
use crate::sidebar::{ActiveTunnelEntry, TunnelRuntimeState};
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, FONT_SUBHEAD, PillTone, hairline_h, inset_well_style,
    modal_card_style, modal_overlay, modal_width, mono_font, pill_button_style,
    segmented_button_style, segmented_track_style, sheet_header, status_dot, styled_input_style,
    ui_font, ui_font_medium, ui_font_semibold,
};

const MODAL_WIDTH: f32 = 540.0;
pub(crate) const RECONNECT_NOTICE_TTL: Duration = Duration::from_secs(8);

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

/// State backing the modal form for defining or editing a Local (`-L`), Remote (`-R`), or Dynamic SOCKS5 (`-D`) `Tunnel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelCreateForm {
    pub open: bool,
    pub editing_tunnel_id: Option<String>,
    pub orig_public_bind_host: Option<String>,
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
            editing_tunnel_id: None,
            orig_public_bind_host: None,
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
    /// Opens the modal seeded with default loopback (`127.0.0.1`) settings for a new tunnel.
    pub fn open_for(&mut self, profile_id: Option<String>, session_id: Option<SessionId>) {
        *self = Self {
            open: true,
            profile_id,
            session_id,
            ..Self::default()
        };
    }

    /// Opens the modal pre-populated for editing an existing [`TunnelRule`].
    pub fn open_for_edit(
        &mut self,
        rule: &TunnelRule,
        profile_id: Option<String>,
        session_id: Option<SessionId>,
    ) {
        let bind_host = rule.effective_bind_host().to_string();
        let orig_public_bind_host = rule.allow_public_bind.then(|| bind_host.clone());
        *self = Self {
            open: true,
            editing_tunnel_id: Some(rule.id.clone()),
            orig_public_bind_host,
            name: rule.name.clone(),
            profile_id,
            session_id,
            direction: rule.direction,
            bind_host,
            bind_port: rule.bind_port.to_string(),
            target_host: rule
                .target_host
                .clone()
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            target_port: rule
                .target_port
                .unwrap_or(5432)
                .to_string(),
            standalone: rule.standalone,
            hold_grace_secs: rule
                .hold_grace_secs
                .unwrap_or(DEFAULT_TUNNEL_HOLD_GRACE_SECS)
                .to_string(),
            validation_error: None,
        };
    }

    /// Closes the modal and clears transient validation errors.
    pub fn close(&mut self) {
        self.open = false;
        self.editing_tunnel_id = None;
        self.orig_public_bind_host = None;
        self.validation_error = None;
    }

    /// Validates form inputs and constructs a [`TunnelRule`] (`allow_public_bind` starts `false`).
    pub fn build_rule(&self) -> Result<TunnelRule, String> {
        let direction = self.direction;

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
            TunnelDirection::Local | TunnelDirection::Remote => {
                let dir_label = if direction == TunnelDirection::Local {
                    "Local (-L)"
                } else {
                    "Remote (-R)"
                };
                let th = self.target_host.trim();
                if th.is_empty() {
                    return Err(format!("Target host is required for {dir_label} tunnels"));
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
                    TunnelDirection::Remote => "Remote",
                    TunnelDirection::Dynamic => "SOCKS5",
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
            TunnelDirection::Remote => {
                let (th, tp) = target_host.zip(target_port).expect("validated remote target");
                TunnelRule::remote(name, bind_port, th, tp)
            }
        };
        rule.bind_host = bind_host;
        rule.standalone = self.standalone;
        rule.hold_grace_secs = hold_grace_secs;
        if let Some(existing_id) = &self.editing_tunnel_id {
            rule.id = existing_id.clone();
            if self.orig_public_bind_host.as_deref() == Some(rule.bind_host.as_str()) {
                rule.allow_public_bind = true;
            }
        }
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
    was_recovering: BTreeSet<String>,
    reconnected_ack: BTreeSet<String>,
    reconnected_at: BTreeMap<String, Instant>,
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

    /// Removes an ad-hoc `TunnelRule` and clears any transient runtime error/stop state for it.
    pub fn remove_ad_hoc(&mut self, tunnel_id: &str) -> bool {
        let before = self.ad_hoc_tunnels.len();
        self.ad_hoc_tunnels.retain(|t| t.rule.id != tunnel_id);
        let removed = self.ad_hoc_tunnels.len() != before;
        self.manually_stopped.remove(tunnel_id);
        self.start_errors.remove(tunnel_id);
        self.was_recovering.remove(tunnel_id);
        self.reconnected_ack.remove(tunnel_id);
        self.reconnected_at.remove(tunnel_id);
        removed
    }

    /// Synchronizes recovery completion timestamps from a live [`TunnelManagerSnapshot`].
    pub fn sync_from_manager_snapshot(&mut self, snap: &TunnelManagerSnapshot, now: Instant) {
        let mut live_ids = BTreeSet::new();
        for t in &snap.tunnels {
            live_ids.insert(t.id.clone());
            match t.state {
                TunnelState::Holding | TunnelState::Reconnecting | TunnelState::Retrying { .. } => {
                    self.was_recovering.insert(t.id.clone());
                    self.reconnected_ack.remove(&t.id);
                    self.reconnected_at.remove(&t.id);
                }
                TunnelState::Active => {
                    if (t.reconnected || self.was_recovering.remove(&t.id))
                        && !self.reconnected_ack.contains(&t.id)
                    {
                        self.reconnected_ack.insert(t.id.clone());
                        self.reconnected_at.insert(t.id.clone(), now);
                    }
                }
                TunnelState::Stopped | TunnelState::Error(_) | TunnelState::Starting => {
                    self.was_recovering.remove(&t.id);
                    self.reconnected_ack.remove(&t.id);
                    self.reconnected_at.remove(&t.id);
                }
            }
        }
        self.was_recovering.retain(|id| live_ids.contains(id));
        self.reconnected_ack.retain(|id| live_ids.contains(id));
        self.reconnected_at.retain(|id, _| live_ids.contains(id));
    }

    /// Expires `"Reconnected successfully"` tunnel notices older than `RECONNECT_NOTICE_TTL`.
    pub fn expire_reconnected_notices(&mut self, now: Instant) -> bool {
        let before = self.reconnected_at.len();
        self.reconnected_at
            .retain(|_, ts| now.saturating_duration_since(*ts) < RECONNECT_NOTICE_TTL);
        self.reconnected_at.len() != before
    }

    /// Returns `true` while any tunnel holds an active `"Reconnected successfully"` notice.
    pub fn has_active_reconnected_notices(&self) -> bool {
        !self.reconnected_at.is_empty()
    }

    /// Returns `true` when `tunnel_id` has an active `"Reconnected successfully"` notice.
    pub fn is_reconnected(&self, tunnel_id: &str) -> bool {
        self.reconnected_at.contains_key(tunnel_id)
    }

    /// Returns `true` when recovery for `tunnel_id` has already been acknowledged.
    pub fn was_acknowledged(&self, tunnel_id: &str) -> bool {
        self.reconnected_ack.contains(tunnel_id)
    }

    /// Clears `pending_public_bind` when it targets `tunnel_id`.
    pub fn clear_pending_public_bind_if(&mut self, tunnel_id: &str) {
        if self
            .pending_public_bind
            .as_ref()
            .is_some_and(|p| p.rule.id == tunnel_id)
        {
            self.pending_public_bind = None;
        }
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
        adhoc_ssh_sessions: &[(SessionId, String)],
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        self.view_create_modal_responsive(profiles, adhoc_ssh_sessions, palette, 1280.0, 800.0)
    }

    /// Renders the `Tunnel` creation modal clamped responsively to `window_width` x `window_height`.
    pub fn view_create_modal_responsive<'a>(
        &'a self,
        profiles: &'a [Profile],
        adhoc_ssh_sessions: &[(SessionId, String)],
        palette: &ThemePalette,
        window_width: f32,
        window_height: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.create_form.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(MODAL_WIDTH, window_width);
        let compact_form = card_w < 480.0;
        let label_w = if compact_form { 88.0 } else { 104.0 };
        let field_row = |label: &'static str, control: Element<'a, Message, Theme, iced::Renderer>| -> Element<'a, Message, Theme, iced::Renderer> {
            if compact_form {
                column![
                    text(label).size(FONT_META).color(chrome.text_secondary),
                    control,
                ]
                .spacing(4)
                .width(Length::Fill)
                .into()
            } else {
                row![
                    text(label)
                        .size(FONT_BODY)
                        .width(Length::Fixed(label_w))
                        .color(chrome.text_secondary),
                    control,
                ]
                .spacing(8)
                .width(Length::Fill)
                .align_y(Alignment::Center)
                .into()
            }
        };
        let form = &self.create_form;
        let is_local = form.direction == TunnelDirection::Local;
        let is_remote = form.direction == TunnelDirection::Remote;
        let is_dynamic = form.direction == TunnelDirection::Dynamic;

        let dir_track = container(
            row![
                button(
                    text("Local (-L)")
                        .size(12.0)
                        .font(if is_local { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 11])
                .style(segmented_button_style(chrome, is_local, true))
                .on_press(Message::TunnelFormDirectionChanged(TunnelDirection::Local)),
                button(
                    text("Remote (-R)")
                        .size(12.0)
                        .font(if is_remote { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 11])
                .style(segmented_button_style(chrome, is_remote, true))
                .on_press(Message::TunnelFormDirectionChanged(TunnelDirection::Remote)),
                button(
                    text("SOCKS5 (-D)")
                        .size(12.0)
                        .font(if is_dynamic { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 11])
                .style(segmented_button_style(chrome, is_dynamic, true))
                .on_press(Message::TunnelFormDirectionChanged(
                    TunnelDirection::Dynamic
                )),
            ]
            .spacing(3)
            .wrap()
            .vertical_spacing(4),
        )
        .padding(3)
        .style(move |_| segmented_track_style(chrome));

        let dir_row = field_row("Direction", dir_track.into());

        let ssh_profiles: Vec<&Profile> = profiles
            .iter()
            .filter(|p| matches!(p.target, ProfileTarget::Ssh { .. }))
            .collect();

        let mut target_buttons: Vec<Element<'a, Message, Theme, iced::Renderer>> = Vec::new();
        for prof in ssh_profiles {
            let selected = form.profile_id.as_deref() == Some(prof.id.as_str());
            target_buttons.push(
                button(
                    text(&prof.name)
                        .size(FONT_META)
                        .font(if selected { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 10])
                .style(pill_button_style(
                    chrome,
                    if selected {
                        PillTone::ActiveAccent
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press(Message::TunnelFormProfileSelected(Some(prof.id.clone())))
                .into(),
            );
        }
        for (sid, label) in adhoc_ssh_sessions {
            let selected = form.profile_id.is_none() && form.session_id == Some(*sid);
            target_buttons.push(
                button(
                    text(format!("Session: {label}"))
                        .size(FONT_META)
                        .font(if selected { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 10])
                .style(pill_button_style(
                    chrome,
                    if selected {
                        PillTone::ActiveAccent
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press(Message::TunnelFormSessionSelected(*sid))
                .into(),
            );
        }

        let target_count = target_buttons.len();
        let target_picker: Element<'a, Message, Theme, iced::Renderer> =
            if target_buttons.is_empty() {
                text("No SSH Profile or active SSH Session")
                    .size(FONT_BODY)
                    .color(chrome.text_muted)
                    .into()
            } else {
                let wrapped = row(target_buttons)
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .wrap()
                    .vertical_spacing(6);
                if target_count > 6 {
                    scrollable(wrapped).height(Length::Fixed(82.0)).into()
                } else {
                    wrapped.into()
                }
            };

        let profile_row = field_row("SSH Target", target_picker);

        let scope_track = container(
            row![
                button(
                    text("Session-bound")
                        .size(12.0)
                        .font(if !form.standalone { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 11])
                .style(segmented_button_style(chrome, !form.standalone, true))
                .on_press(Message::TunnelFormStandaloneToggled(false)),
                button(
                    text("Standalone (Background)")
                        .size(12.0)
                        .font(if form.standalone { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 11])
                .style(segmented_button_style(chrome, form.standalone, true))
                .on_press(Message::TunnelFormStandaloneToggled(true)),
            ]
            .spacing(3)
            .wrap()
            .vertical_spacing(4),
        )
        .padding(3)
        .style(move |_| segmented_track_style(chrome));

        let scope_row = field_row("Lifecycle", scope_track.into());

        let bind_label = if is_remote {
            "Remote Bind"
        } else {
            "Bind Host"
        };

        let routing_col = column![
            dir_row,
            hairline_h(chrome),
            profile_row,
            hairline_h(chrome),
            scope_row,
        ]
        .spacing(9)
        .width(Length::Fill);

        let name_ctrl = text_input("Optional display name", &form.name)
            .id(tunnel_name_input_id())
            .on_input(Message::TunnelFormNameChanged)
            .size(FONT_BODY)
            .padding([7, 10])
            .style(styled_input_style(chrome))
            .width(Length::Fill)
            .into();

        let bind_ctrl = row![
            text_input("127.0.0.1", &form.bind_host)
                .id(tunnel_bind_host_input_id())
                .on_input(Message::TunnelFormBindHostChanged)
                .font(mono_font())
                .size(FONT_BODY)
                .padding([7, 10])
                .style(styled_input_style(chrome))
                .width(Length::Fill),
            text("Port").size(FONT_BODY).color(chrome.text_secondary),
            text_input("5432", &form.bind_port)
                .id(tunnel_bind_port_input_id())
                .on_input(Message::TunnelFormBindPortChanged)
                .font(mono_font())
                .width(Length::Fixed(80.0))
                .size(FONT_BODY)
                .padding([7, 10])
                .style(styled_input_style(chrome)),
        ]
        .spacing(8)
        .width(Length::Fill)
        .align_y(Alignment::Center)
        .into();

        let mut endpoint_col = column![
            field_row("Name", name_ctrl),
            hairline_h(chrome),
            field_row(bind_label, bind_ctrl),
        ]
        .spacing(9)
        .width(Length::Fill);

        if is_local || is_remote {
            let target_label = if is_remote {
                "Local Target"
            } else {
                "Target Host"
            };
            let target_ctrl = row![
                text_input("127.0.0.1", &form.target_host)
                    .id(tunnel_target_host_input_id())
                    .on_input(Message::TunnelFormTargetHostChanged)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
                text("Port").size(FONT_BODY).color(chrome.text_secondary),
                text_input("5432", &form.target_port)
                    .id(tunnel_target_port_input_id())
                    .on_input(Message::TunnelFormTargetPortChanged)
                    .font(mono_font())
                    .width(Length::Fixed(80.0))
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome)),
            ]
            .spacing(8)
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .into();
            endpoint_col = endpoint_col.push(hairline_h(chrome)).push(field_row(target_label, target_ctrl));
        }

        let grace_ctrl = row![
            text_input("10", &form.hold_grace_secs)
                .on_input(Message::TunnelFormHoldGraceChanged)
                .font(mono_font())
                .width(Length::Fixed(80.0))
                .size(FONT_BODY)
                .padding([7, 10])
                .style(styled_input_style(chrome)),
            text("Held sockets wait before resetting")
                .size(FONT_META)
                .color(chrome.text_muted),
        ]
        .spacing(8)
        .width(Length::Fill)
        .align_y(Alignment::Center)
        .wrap()
        .vertical_spacing(4)
        .into();

        endpoint_col = endpoint_col
            .push(hairline_h(chrome))
            .push(field_row("Hold Grace (s)", grace_ctrl));

        let editing_id = form.editing_tunnel_id.clone();
        let is_editing = editing_id.is_some();
        let header = sheet_header(
            if is_editing {
                "Edit SSH Tunnel"
            } else {
                "New SSH Tunnel"
            },
            if is_editing {
                "Update tunnel direction, SSH target, endpoints, or lifecycle"
            } else {
                "Create a local, remote, or dynamic SOCKS5 port forward"
            },
            Some("Esc"),
            Message::CloseTunnelCreateModal,
            chrome,
        );

        let mut form_sections = column![
            text("Routing & Target")
                .size(FONT_SUBHEAD)
                .font(ui_font_semibold())
                .color(chrome.text_primary),
            container(routing_col)
                .padding(12)
                .width(Length::Fill)
                .style(move |_| inset_well_style(chrome)),
            text("Endpoints & Resilience")
                .size(FONT_SUBHEAD)
                .font(ui_font_semibold())
                .color(chrome.text_primary),
            container(endpoint_col)
                .padding(12)
                .width(Length::Fill)
                .style(move |_| inset_well_style(chrome)),
        ]
        .spacing(10)
        .width(Length::Fill);

        if let Some(err) = &form.validation_error {
            form_sections = form_sections.push(text(err.as_str()).size(FONT_BODY).color(chrome.danger));
        }

        let mut footer = row![].spacing(8).align_y(Alignment::Center);
        if let Some(tid) = editing_id {
            footer = footer.push(
                button(text("Delete").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(Message::DeleteTunnel(tid)),
            );
        }
        let footer = footer
            .push(space::horizontal())
            .push(
                button(text("Cancel").size(FONT_BODY))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::CloseTunnelCreateModal),
            )
            .push(
                button(
                    text(if is_editing { "Save" } else { "Create & Start" })
                        .size(FONT_BODY)
                        .font(ui_font_medium()),
                )
                .padding([6, 16])
                .style(pill_button_style(chrome, PillTone::Primary))
                .on_press(Message::SubmitTunnelCreate),
            );

        let max_body_h = (window_height - 200.0).max(160.0).min(460.0);
        let scrolled = container(
            scrollable(
                container(form_sections)
                    .padding(Padding {
                        top: 0.0,
                        right: 8.0,
                        bottom: 2.0,
                        left: 0.0,
                    })
                    .width(Length::Fill),
            )
            .width(Length::Fill),
        )
        .max_height(max_body_h)
        .width(Length::Fill);
        let card = container(
            column![
                header,
                hairline_h(chrome),
                scrolled,
                hairline_h(chrome),
                footer,
            ]
            .spacing(12)
            .padding(18),
        )
        .width(Length::Fixed(card_w))
        .style(move |_| modal_card_style(chrome, true));

        Some(modal_overlay(card.into(), Some(Message::CloseTunnelCreateModal), 24.0))
    }

    /// Renders the `0.0.0.0` public-bind security warning confirmation modal when active.
    pub fn view_public_bind_warning<'a>(
        &'a self,
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        self.view_public_bind_warning_responsive(palette, 1280.0)
    }

    /// Renders the `0.0.0.0` public-bind security warning modal clamped to `window_width`.
    pub fn view_public_bind_warning_responsive<'a>(
        &'a self,
        palette: &ThemePalette,
        window_width: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        let pending = self.pending_public_bind.as_ref()?;
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(MODAL_WIDTH, window_width);

        let card = opaque(
            container(
                column![
                    row![
                        status_dot(chrome.warning),
                        text("Public Network Bind Warning")
                            .size(16)
                            .color(chrome.warning),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                    text(pending.warning_text())
                        .size(13)
                        .color(chrome.text_primary),
                    hairline_h(chrome),
                    row![
                        button(text("Cancel (Keep Safe)").size(12))
                            .padding([6, 12])
                            .style(pill_button_style(chrome, PillTone::Subtle))
                            .on_press(Message::TunnelPublicBindCancel),
                        button(text("Allow Public Bind & Start").size(12))
                            .padding([6, 14])
                            .style(pill_button_style(chrome, PillTone::Danger))
                            .on_press(Message::TunnelPublicBindConfirm),
                    ]
                    .spacing(8)
                    .wrap()
                    .vertical_spacing(6),
                ]
                .spacing(12)
                .padding(18),
            )
            .width(Length::Fixed(card_w))
            .style(move |_| container::Style {
                background: Some(Background::Color(chrome.panel_bg)),
                border: Border {
                    color: chrome.warning,
                    width: 1.5,
                    radius: 14.0.into(),
                },
                shadow: Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, if chrome.is_light { 0.18 } else { 0.48 }),
                    offset: Vector::new(0.0, 12.0),
                    blur_radius: 30.0,
                },
                text_color: Some(chrome.text_primary),
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
            .or_else(|| {
                snap.remote_bound_port
                    .map(|p| format!("{}:{p}", snap.bind_host))
            })
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
        let reconnect_attempt = snap.reconnect_attempt.or_else(|| match &state {
            TunnelRuntimeState::Retrying { attempt, .. } => Some(*attempt),
            _ => None,
        });
        let next_retry_secs = snap.next_retry_secs.or_else(|| match &state {
            TunnelRuntimeState::Retrying {
                next_retry_secs, ..
            } => Some(*next_retry_secs),
            _ => None,
        });
        let reconnected_notice = if state == TunnelRuntimeState::Active
            && (tunnel_ui.is_reconnected(&snap.id)
                || (snap.reconnected && !tunnel_ui.was_acknowledged(&snap.id)))
        {
            Some("Reconnected successfully".to_string())
        } else {
            None
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
            reconnect_attempt,
            next_retry_secs,
            last_error: snap.last_error.clone(),
            reconnected_notice,
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
                reconnect_attempt: None,
                next_retry_secs: None,
                last_error: None,
                reconnected_notice: None,
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
            reconnect_attempt: None,
            next_retry_secs: None,
            last_error: None,
            reconnected_notice: None,
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
        TunnelState::Retrying {
            attempt,
            next_retry_secs,
            reason,
        } => TunnelRuntimeState::Retrying {
            attempt: *attempt,
            next_retry_secs: *next_retry_secs,
            reason: reason.clone(),
        },
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
