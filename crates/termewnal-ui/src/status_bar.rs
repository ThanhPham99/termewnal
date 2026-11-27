use iced::widget::{button, column, container, row, space, text};
use iced::{Alignment, Background, Color, Element, Length, Theme};
use termewnal_core::ThemePalette;
use termewnal_ssh::TransportStatus;

use crate::app::{Message, SessionId};
use crate::sidebar::{ActiveTunnelEntry, TunnelRuntimeState};
use crate::ui_theme::{
    ChromePalette, FONT_META, PillTone, ViewportTier, badge_pill, hairline_h, mono_font,
    pill_button_style, status_dot, ui_font_medium,
};

/// Kind of active terminal `Session` shown in the bottom Status Bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKindLabel {
    Local,
    Ssh,
    Serial,
    Telnet,
    Docker,
}

/// Severity/kind of a live recovery status indicator in the Status Bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryNoticeKind {
    AutoReconnecting,
    ReconnectedSuccessfully,
}

/// Concise status pill rendered in the Status Bar for active recovery or completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRecoveryNotice {
    pub kind: RecoveryNoticeKind,
    pub message: String,
}

/// Active `Session` metadata projected into the Status Bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatusHook {
    pub session_id: SessionId,
    pub kind: SessionKindLabel,
    pub title: String,
    pub target: String,
    pub cols: u16,
    pub rows: u16,
    pub exited: Option<Option<i32>>,
    pub transport_status: TransportStatus,
    pub is_auto_reconnecting: bool,
    pub zoomed: bool,
    pub broadcast: bool,
}

/// Extensible `Tunnel` summary counters rendered in the Status Bar (populated by Tickets 11-12).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TunnelSummaryHook {
    pub total: usize,
    pub active: usize,
    pub holding: usize,
    pub reconnecting: usize,
    pub retrying: usize,
    pub error: usize,
}

impl TunnelSummaryHook {
    /// Derives summary counters from a slice of sidebar [`ActiveTunnelEntry`] items.
    pub fn from_entries(entries: &[ActiveTunnelEntry]) -> Self {
        let mut summary = Self {
            total: entries.len(),
            ..Default::default()
        };
        for entry in entries {
            match entry.state {
                TunnelRuntimeState::Active => summary.active += 1,
                TunnelRuntimeState::Holding => summary.holding += 1,
                TunnelRuntimeState::Reconnecting => summary.reconnecting += 1,
                TunnelRuntimeState::Retrying { .. } => summary.retrying += 1,
                TunnelRuntimeState::Error(_) => summary.error += 1,
                TunnelRuntimeState::Stopped | TunnelRuntimeState::Starting => {}
            }
        }
        summary
    }
}

/// Extensible SFTP `Transfer` progress summary rendered in the Status Bar (populated by Tickets 13-14).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransferSummaryHook {
    pub active: usize,
    pub queued: usize,
    pub failed: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_per_sec: Option<u64>,
    pub drawer_open: bool,
}

/// Interactive actions emitted by clickable Status Bar segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusBarAction {
    ToggleSidebar,
    OpenCommandPalette,
    OpenSettings,
    ProbeAll,
    ReconnectSession(SessionId),
    CancelReconnect(SessionId),
    ToggleTunnelsView,
    ReconnectTunnels,
    ToggleTransferDrawer,
    LockVault,
}

/// Persistent state backing extensible Status Bar slots (`Tunnel` and `Transfer` hooks).
#[derive(Debug, Clone, Default)]
pub struct StatusBarState {
    tunnels: TunnelSummaryHook,
    transfers: TransferSummaryHook,
    tunnel_notice: Option<StatusRecoveryNotice>,
    sftp_notice: Option<StatusRecoveryNotice>,
}

impl StatusBarState {
    /// Creates an idle [`StatusBarState`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current `Tunnel` summary counters.
    pub fn tunnels(&self) -> &TunnelSummaryHook {
        &self.tunnels
    }

    /// Updates `Tunnel` summary counters.
    pub fn set_tunnels(&mut self, tunnels: TunnelSummaryHook) {
        self.tunnels = tunnels;
    }

    /// Returns the current SFTP `Transfer` summary counters.
    pub fn transfers(&self) -> &TransferSummaryHook {
        &self.transfers
    }

    /// Updates SFTP `Transfer` summary counters.
    pub fn set_transfers(&mut self, transfers: TransferSummaryHook) {
        self.transfers = transfers;
    }

    /// Returns the current `Tunnel` recovery notice shown in the Status Bar, if any.
    pub fn tunnel_notice(&self) -> Option<&StatusRecoveryNotice> {
        self.tunnel_notice.as_ref()
    }

    /// Updates the `Tunnel` recovery notice shown in the Status Bar.
    pub fn set_tunnel_notice(&mut self, notice: Option<StatusRecoveryNotice>) {
        self.tunnel_notice = notice;
    }

    /// Returns the current SFTP / `Transfer` recovery notice shown in the Status Bar, if any.
    pub fn sftp_notice(&self) -> Option<&StatusRecoveryNotice> {
        self.sftp_notice.as_ref()
    }

    /// Updates the SFTP / `Transfer` recovery notice shown in the Status Bar.
    pub fn set_sftp_notice(&mut self, notice: Option<StatusRecoveryNotice>) {
        self.sftp_notice = notice;
    }

    /// Toggles the extensible SFTP/Transfer drawer open flag for Ticket 13 wiring.
    pub fn toggle_transfer_drawer(&mut self) -> bool {
        self.transfers.drawer_open = !self.transfers.drawer_open;
        self.transfers.drawer_open
    }
}

/// Immutable Status Bar view-model snapshot computed from [`crate::AppState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBarSnapshot {
    pub active_session: Option<SessionStatusHook>,
    pub tunnels: TunnelSummaryHook,
    pub transfers: TransferSummaryHook,
    pub sidebar_open: bool,
    pub transfer_drawer_open: bool,
    pub vault_unlocked: bool,
    pub connection_notice: Option<StatusRecoveryNotice>,
    pub tunnel_notice: Option<StatusRecoveryNotice>,
    pub sftp_notice: Option<StatusRecoveryNotice>,
    pub zmodem_notice: Option<String>,
    pub last_error: Option<String>,
}

impl StatusBarSnapshot {
    /// Formats the active `Session` segment (`Kind | target/title | colsxrows`).
    pub fn session_text(&self) -> String {
        let Some(session) = &self.active_session else {
            return "No active session".to_string();
        };
        let exit_suffix = match session.exited {
            Some(Some(code)) => format!(" | exited({code})"),
            Some(None) => " | exited".to_string(),
            None => String::new(),
        };
        match session.kind {
            SessionKindLabel::Local => {
                format!(
                    "Local | {} | {}x{}{exit_suffix}",
                    session.title, session.cols, session.rows
                )
            }
            SessionKindLabel::Ssh => {
                format!(
                    "SSH | {} | {}x{}{exit_suffix}",
                    session.target, session.cols, session.rows
                )
            }
            SessionKindLabel::Serial => {
                format!(
                    "Serial | {} | {}x{}{exit_suffix}",
                    session.target, session.cols, session.rows
                )
            }
            SessionKindLabel::Telnet => {
                format!(
                    "Telnet | {} | {}x{}{exit_suffix}",
                    session.target, session.cols, session.rows
                )
            }
            SessionKindLabel::Docker => {
                format!(
                    "Docker | {} | {}x{}{exit_suffix}",
                    session.target, session.cols, session.rows
                )
            }
        }
    }

    /// Formats the `Transport` health & latency segment.
    pub fn transport_text(&self) -> String {
        let Some(session) = &self.active_session else {
            return "Transport: idle".to_string();
        };
        if matches!(session.kind, SessionKindLabel::Local | SessionKindLabel::Docker) {
            return if session.kind == SessionKindLabel::Docker {
                "Transport: Container PTY".to_string()
            } else {
                "Transport: Local PTY".to_string()
            };
        }
        match &session.transport_status {
            TransportStatus::Disconnected => "Transport: Disconnected".to_string(),
            TransportStatus::Connecting => "Transport: Connecting...".to_string(),
            TransportStatus::Connected {
                latency_ms: Some(ms),
            } => format!("Transport: Connected ({ms} ms)"),
            TransportStatus::Connected { latency_ms: None } => "Transport: Connected".to_string(),
            TransportStatus::Degraded { missed_probes } => {
                format!("Transport: Degraded ({missed_probes} missed)")
            }
            TransportStatus::Reconnecting {
                attempt,
                next_retry_secs,
            } => format!("Transport: Reconnecting #{attempt} ({next_retry_secs}s)"),
        }
    }

    /// Returns `true` when non-modal reconnect controls (`Reconnect now` / `Cancel`) should appear.
    pub fn shows_reconnect_controls(&self) -> bool {
        self.active_session.as_ref().is_some_and(|session| {
            session.kind == SessionKindLabel::Ssh
                && (session.is_auto_reconnecting
                    || matches!(
                        session.transport_status,
                        TransportStatus::Degraded { .. } | TransportStatus::Reconnecting { .. }
                    ))
        })
    }

    /// Returns `true` when clicking the Transport health indicator or Probe control can probe/reconnect.
    pub fn shows_probe_control(&self) -> bool {
        self.active_session.as_ref().is_some_and(|session| {
            session.kind == SessionKindLabel::Ssh
                && matches!(
                    session.transport_status,
                    TransportStatus::Connected { .. }
                        | TransportStatus::Degraded { .. }
                        | TransportStatus::Reconnecting { .. }
                )
        })
    }

    /// Formats the `Tunnel` summary badge text.
    pub fn tunnels_text(&self) -> String {
        if self.tunnels.total == 0
            && self.tunnels.active == 0
            && self.tunnels.holding == 0
            && self.tunnels.reconnecting == 0
            && self.tunnels.retrying == 0
            && self.tunnels.error == 0
        {
            return "Tunnels: 0".to_string();
        }
        if self.tunnels.error > 0 {
            return format!(
                "Tunnels: {} active, {} err",
                self.tunnels.active, self.tunnels.error
            );
        }
        if self.tunnels.retrying > 0 {
            return format!(
                "Tunnels: {} active, {} retrying",
                self.tunnels.active, self.tunnels.retrying
            );
        }
        if self.tunnels.holding > 0 {
            return format!(
                "Tunnels: {} active, {} holding",
                self.tunnels.active, self.tunnels.holding
            );
        }
        if self.tunnels.reconnecting > 0 {
            return format!(
                "Tunnels: {} active, {} reconnecting",
                self.tunnels.active, self.tunnels.reconnecting
            );
        }
        format!("Tunnels: {} active", self.tunnels.active)
    }

    /// Formats the SFTP `Transfer` summary badge text.
    pub fn transfers_text(&self) -> String {
        if self.transfers.active == 0 && self.transfers.queued == 0 && self.transfers.failed == 0 {
            return "Transfers: idle".to_string();
        }
        if self.transfers.active > 0 {
            if self.transfers.bytes_total > 0 {
                let pct = ((self.transfers.bytes_done as f64 / self.transfers.bytes_total as f64)
                    * 100.0)
                    .clamp(0.0, 100.0)
                    .round() as u64;
                return format!("Transfers: {} active ({pct}%)", self.transfers.active);
            }
            return format!("Transfers: {} active", self.transfers.active);
        }
        if self.transfers.queued > 0 {
            return format!("Transfers: {} queued", self.transfers.queued);
        }
        format!("Transfers: {} failed", self.transfers.failed)
    }
}

fn render_recovery_pill(
    notice: &StatusRecoveryNotice,
    chrome: ChromePalette,
) -> Element<'static, Message, Theme, iced::Renderer> {
    let (fg, bg) = match notice.kind {
        RecoveryNoticeKind::AutoReconnecting => (chrome.warning, chrome.warning_soft),
        RecoveryNoticeKind::ReconnectedSuccessfully => (chrome.success, chrome.success_soft),
    };
    badge_pill(notice.message.clone(), fg, bg, Color::TRANSPARENT)
}

/// Renders the persistent bottom Status Bar from `snapshot`.
pub fn render_status_bar(
    snapshot: &StatusBarSnapshot,
    palette: &ThemePalette,
) -> Element<'static, Message, Theme, iced::Renderer> {
    render_status_bar_responsive(snapshot, palette, 1280.0)
}

/// Renders the bottom Status Bar responsively for `window_width`, wrapping cleanly on narrow screens.
pub fn render_status_bar_responsive(
    snapshot: &StatusBarSnapshot,
    palette: &ThemePalette,
    window_width: f32,
) -> Element<'static, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);
    let tier = ViewportTier::from_width(window_width);

    let session_dot = match snapshot
        .active_session
        .as_ref()
        .map(|s| (&s.kind, s.exited, &s.transport_status))
    {
        Some((SessionKindLabel::Local | SessionKindLabel::Docker, None, _)) => chrome.success,
        Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            _,
            TransportStatus::Connected { .. },
        )) => chrome.accent,
        Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            _,
            TransportStatus::Connecting
            | TransportStatus::Degraded { .. }
            | TransportStatus::Reconnecting { .. },
        )) => chrome.warning,
        Some((_, Some(_), _))
        | Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            _,
            TransportStatus::Disconnected,
        )) => chrome.text_muted,
        None => chrome.text_muted,
    };

    let mut left_items: Vec<Element<'static, Message, Theme, iced::Renderer>> = vec![
        row![
            status_dot(session_dot),
            text(snapshot.session_text())
                .size(FONT_META)
                .font(mono_font())
                .color(chrome.text_primary),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into(),
    ];

    if let Some(session) = &snapshot.active_session {
        if session.zoomed {
            left_items.push(badge_pill(
                "Zoom",
                chrome.accent,
                chrome.accent_soft,
                Color::TRANSPARENT,
            ));
        }
        if session.broadcast {
            left_items.push(badge_pill(
                "Broadcast",
                chrome.danger,
                chrome.danger_soft,
                Color::TRANSPARENT,
            ));
        }
    }
    if let Some(znotice) = &snapshot.zmodem_notice {
        left_items.push(badge_pill(
            znotice.clone(),
            chrome.accent,
            chrome.accent_soft,
            chrome.border_subtle,
        ));
    }

    let transport_color = match snapshot
        .active_session
        .as_ref()
        .map(|s| (&s.kind, &s.transport_status))
    {
        Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            TransportStatus::Connected { .. },
        )) => chrome.success,
        Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            TransportStatus::Degraded { .. },
        ))
        | Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            TransportStatus::Reconnecting { .. },
        ))
        | Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            TransportStatus::Connecting,
        )) => chrome.warning,
        Some((
            SessionKindLabel::Ssh | SessionKindLabel::Serial | SessionKindLabel::Telnet,
            TransportStatus::Disconnected,
        )) => chrome.text_muted,
        _ => chrome.text_secondary,
    };

    let transport_label = row![
        status_dot(transport_color),
        text(snapshot.transport_text())
            .size(FONT_META)
            .font(mono_font())
            .color(transport_color),
    ]
    .spacing(5)
    .align_y(Alignment::Center);

    let transport_widget: Element<'static, Message, Theme, iced::Renderer> =
        if snapshot.shows_probe_control() {
            button(transport_label)
                .padding([4, 8])
                .style(pill_button_style(chrome, PillTone::Ghost))
                .on_press(Message::StatusBarAction(StatusBarAction::ProbeAll))
                .into()
        } else {
            container(transport_label).padding([4, 4]).into()
        };

    let mut right_items: Vec<Element<'static, Message, Theme, iced::Renderer>> =
        vec![transport_widget];

    if let Some(notice) = &snapshot.connection_notice {
        right_items.push(render_recovery_pill(notice, chrome));
    }

    if snapshot.shows_reconnect_controls()
        && let Some(session) = &snapshot.active_session
    {
        right_items.push(
            button(text("Reconnect now").size(FONT_META).font(ui_font_medium()))
                .padding([4, 9])
                .style(pill_button_style(chrome, PillTone::Primary))
                .on_press(Message::StatusBarAction(StatusBarAction::ReconnectSession(
                    session.session_id,
                )))
                .into(),
        );
        right_items.push(
            button(text("Cancel").size(FONT_META))
                .padding([4, 8])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::StatusBarAction(StatusBarAction::CancelReconnect(
                    session.session_id,
                )))
                .into(),
        );
    }

    let tunnel_color = if snapshot.tunnels.error > 0 {
        chrome.danger
    } else if snapshot.tunnels.holding > 0
        || snapshot.tunnels.reconnecting > 0
        || snapshot.tunnels.retrying > 0
    {
        chrome.warning
    } else if snapshot.tunnels.active > 0 {
        chrome.success
    } else {
        chrome.text_secondary
    };

    right_items.push(
        button(
            row![
                status_dot(tunnel_color),
                text(snapshot.tunnels_text())
                    .size(FONT_META)
                    .font(mono_font())
                    .color(tunnel_color),
            ]
            .spacing(5)
            .align_y(Alignment::Center),
        )
        .padding([4, 8])
        .style(pill_button_style(chrome, PillTone::Ghost))
        .on_press(Message::StatusBarAction(StatusBarAction::ToggleTunnelsView))
        .into(),
    );

    if let Some(notice) = &snapshot.tunnel_notice {
        right_items.push(render_recovery_pill(notice, chrome));
    }

    if snapshot.tunnels.holding > 0
        || snapshot.tunnels.reconnecting > 0
        || snapshot.tunnels.retrying > 0
        || snapshot.tunnels.error > 0
    {
        right_items.push(
            button(text("Reconnect Tunnels").size(FONT_META).font(ui_font_medium()))
                .padding([4, 9])
                .style(pill_button_style(chrome, PillTone::Primary))
                .on_press(Message::StatusBarAction(StatusBarAction::ReconnectTunnels))
                .into(),
        );
    }

    right_items.push(
        button(
            text(snapshot.transfers_text())
                .size(FONT_META)
                .font(mono_font()),
        )
        .padding([4, 8])
        .style(pill_button_style(
            chrome,
            if snapshot.transfer_drawer_open {
                PillTone::Subtle
            } else {
                PillTone::Ghost
            },
        ))
        .on_press(Message::StatusBarAction(
            StatusBarAction::ToggleTransferDrawer,
        ))
        .into(),
    );

    if let Some(notice) = &snapshot.sftp_notice {
        right_items.push(render_recovery_pill(notice, chrome));
    }

    if snapshot.vault_unlocked {
        right_items.push(
            button(text("Lock Vault").size(FONT_META).font(ui_font_medium()))
                .padding([4, 8])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::StatusBarAction(StatusBarAction::LockVault))
                .into(),
        );
    }

    right_items.push(
        button(text("Settings").size(FONT_META).font(ui_font_medium()))
            .padding([4, 8])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::StatusBarAction(StatusBarAction::OpenSettings))
            .into(),
    );
    right_items.push(
        button(text("Ctrl+K").size(FONT_META).font(mono_font()))
            .padding([4, 9])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::StatusBarAction(
                StatusBarAction::OpenCommandPalette,
            ))
            .into(),
    );

    let bar_body: Element<'static, Message, Theme, iced::Renderer> = match tier {
        ViewportTier::Compact => {
            let left_wrap = row(left_items)
                .spacing(6)
                .align_y(Alignment::Center)
                .wrap()
                .vertical_spacing(4);
            let right_wrap = row(right_items)
                .spacing(6)
                .align_y(Alignment::Center)
                .wrap()
                .vertical_spacing(4);
            column![left_wrap, right_wrap]
                .spacing(4)
                .padding([5, 10])
                .width(Length::Fill)
                .into()
        }
        ViewportTier::Medium | ViewportTier::Wide => {
            let left = row(left_items).spacing(8).align_y(Alignment::Center);
            let right = row(right_items).spacing(5).align_y(Alignment::Center);
            row![left, space::horizontal(), right]
                .spacing(8)
                .padding([5, 12])
                .align_y(Alignment::Center)
                .into()
        }
    };

    container(column![hairline_h(chrome), bar_body].width(Length::Fill))
        .width(Length::Fill)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(chrome.chrome_bg)),
            text_color: Some(chrome.text_primary),
            ..Default::default()
        })
        .into()
}
