use iced::widget::{button, container, row, space, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Theme};
use termewnal_ssh::TransportStatus;

use crate::app::{Message, SessionId};
use crate::sidebar::{ActiveTunnelEntry, TunnelRuntimeState};

/// Kind of active terminal `Session` shown in the bottom Status Bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKindLabel {
    Local,
    Ssh,
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
    ProbeAll,
    ReconnectSession(SessionId),
    CancelReconnect(SessionId),
    ToggleTunnelsView,
    ToggleTransferDrawer,
}

/// Persistent state backing extensible Status Bar slots (`Tunnel` and `Transfer` hooks).
#[derive(Debug, Clone, Default)]
pub struct StatusBarState {
    tunnels: TunnelSummaryHook,
    transfers: TransferSummaryHook,
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
    pub last_error: Option<String>,
}

impl StatusBarSnapshot {
    /// Formats the active `Session` segment (`kind · target/title · colsxrows`).
    pub fn session_text(&self) -> String {
        let Some(session) = &self.active_session else {
            return "No active session".to_string();
        };
        let exit_suffix = match session.exited {
            Some(Some(code)) => format!(" · exited({code})"),
            Some(None) => " · exited".to_string(),
            None => String::new(),
        };
        match session.kind {
            SessionKindLabel::Local => {
                format!(
                    "LOCAL · {} · {}x{}{exit_suffix}",
                    session.title, session.cols, session.rows
                )
            }
            SessionKindLabel::Ssh => {
                format!(
                    "SSH · {} · {}x{}{exit_suffix}",
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
        if session.kind == SessionKindLabel::Local {
            return "Transport: Local PTY".to_string();
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
                && matches!(
                    session.transport_status,
                    TransportStatus::Degraded { .. } | TransportStatus::Reconnecting { .. }
                )
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
        if self.tunnels.holding > 0 {
            return format!(
                "Tunnels: {} active, {} holding",
                self.tunnels.active, self.tunnels.holding
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

/// Renders the persistent bottom Status Bar from `snapshot`.
pub fn render_status_bar(
    snapshot: &StatusBarSnapshot,
) -> Element<'static, Message, Theme, iced::Renderer> {
    let sidebar_btn = button(text("Sidebar").size(11))
        .padding([2, 7])
        .style(if snapshot.sidebar_open {
            button::primary
        } else {
            button::subtle
        })
        .on_press(Message::StatusBarAction(StatusBarAction::ToggleSidebar));

    let mut left = row![sidebar_btn, text(snapshot.session_text()).size(11)]
        .spacing(8)
        .align_y(Alignment::Center);

    if let Some(session) = &snapshot.active_session {
        if session.zoomed {
            left = left.push(
                text("[ZOOM]")
                    .size(11)
                    .color(Color::from_rgb8(96, 165, 250)),
            );
        }
        if session.broadcast {
            left = left.push(
                text("[BCAST]")
                    .size(11)
                    .color(Color::from_rgb8(248, 113, 113)),
            );
        }
    }

    let transport_color = match snapshot
        .active_session
        .as_ref()
        .map(|s| (&s.kind, &s.transport_status))
    {
        Some((SessionKindLabel::Ssh, TransportStatus::Connected { .. })) => {
            Color::from_rgb8(74, 222, 128)
        }
        Some((SessionKindLabel::Ssh, TransportStatus::Degraded { .. }))
        | Some((SessionKindLabel::Ssh, TransportStatus::Reconnecting { .. })) => {
            Color::from_rgb8(251, 191, 36)
        }
        Some((SessionKindLabel::Ssh, TransportStatus::Disconnected)) => {
            Color::from_rgb8(148, 163, 184)
        }
        _ => Color::from_rgb8(203, 213, 225),
    };

    let transport_label = text(snapshot.transport_text())
        .size(11)
        .color(transport_color);
    let transport_widget: Element<'static, Message, Theme, iced::Renderer> =
        if snapshot.shows_probe_control() {
            button(transport_label)
                .padding([2, 6])
                .style(button::subtle)
                .on_press(Message::StatusBarAction(StatusBarAction::ProbeAll))
                .into()
        } else {
            transport_label.into()
        };

    let mut right = row![transport_widget]
        .spacing(6)
        .align_y(Alignment::Center);

    if snapshot.shows_reconnect_controls()
        && let Some(session) = &snapshot.active_session
    {
        right = right
            .push(
                button(text("Reconnect now").size(11))
                    .padding([2, 6])
                    .style(button::primary)
                    .on_press(Message::StatusBarAction(StatusBarAction::ReconnectSession(
                        session.session_id,
                    ))),
            )
            .push(
                button(text("Cancel").size(11))
                    .padding([2, 6])
                    .style(button::subtle)
                    .on_press(Message::StatusBarAction(StatusBarAction::CancelReconnect(
                        session.session_id,
                    ))),
            );
    }

    right = right
        .push(
            button(text(snapshot.tunnels_text()).size(11))
                .padding([2, 7])
                .style(button::subtle)
                .on_press(Message::StatusBarAction(StatusBarAction::ToggleTunnelsView)),
        )
        .push(
            button(text(snapshot.transfers_text()).size(11))
                .padding([2, 7])
                .style(if snapshot.transfer_drawer_open {
                    button::primary
                } else {
                    button::subtle
                })
                .on_press(Message::StatusBarAction(
                    StatusBarAction::ToggleTransferDrawer,
                )),
        )
        .push(
            button(text("Ctrl+K").size(11))
                .padding([2, 7])
                .style(button::subtle)
                .on_press(Message::StatusBarAction(
                    StatusBarAction::OpenCommandPalette,
                )),
        );

    let bar = row![left, space::horizontal(), right]
        .spacing(10)
        .padding([4, 10])
        .align_y(Alignment::Center);

    container(bar)
        .width(Length::Fill)
        .style(|theme: &Theme| {
            let ext = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(ext.background.weak.color)),
                border: Border {
                    color: ext.background.strong.color,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}
