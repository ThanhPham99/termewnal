use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use iced::widget::{button, column, container, opaque, row, stack, text};
use iced::{Alignment, Background, Color, Element, Length, Theme, window};
use termewnal_core::ThemePalette;

use crate::ui_theme::{
    ChromePalette, PillTone, hairline_h, modal_card_style, pill_button_style, status_dot,
};

/// Snapshot of active background operations that would be terminated on process exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackgroundWorkStatus {
    /// Active Standalone `Tunnel`s running independently of terminal tabs.
    pub active_standalone_tunnels: usize,
    /// In-flight SFTP `Transfer`s currently queued or running.
    pub active_transfers: usize,
}

impl BackgroundWorkStatus {
    /// Returns `true` when at least one Standalone `Tunnel` or active `Transfer` is running.
    pub fn has_active_work(&self) -> bool {
        self.active_standalone_tunnels > 0 || self.active_transfers > 0
    }
}

/// Predicate and shutdown hook queried by the window-close guard.
///
/// Future SSH `Tunnel` and SFTP `Transfer` engines plug into this trait so closing the last
/// GUI window prompts before terminating active background work.
pub trait BackgroundWorkSource: Send + Sync {
    /// Returns the current counts of active Standalone `Tunnel`s and active `Transfer`s.
    fn status(&self) -> BackgroundWorkStatus;

    /// Requests graceful shutdown of all active background work before exiting the daemon.
    fn stop_all(&self);
}

/// Thread-safe default [`BackgroundWorkSource`] backed by atomic counters.
#[derive(Debug, Default)]
pub struct BackgroundWorkTracker {
    active_standalone_tunnels: AtomicUsize,
    active_transfers: AtomicUsize,
    stop_requested: AtomicBool,
}

impl BackgroundWorkTracker {
    /// Creates an idle [`BackgroundWorkTracker`] with zero active tunnels or transfers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the number of active Standalone `Tunnel`s.
    pub fn set_active_standalone_tunnels(&self, count: usize) {
        self.active_standalone_tunnels.store(count, Ordering::SeqCst);
    }

    /// Sets the number of active SFTP `Transfer`s.
    pub fn set_active_transfers(&self, count: usize) {
        self.active_transfers.store(count, Ordering::SeqCst);
    }

    /// Returns the active Standalone `Tunnel` count.
    pub fn active_standalone_tunnels(&self) -> usize {
        self.active_standalone_tunnels.load(Ordering::SeqCst)
    }

    /// Returns the active SFTP `Transfer` count.
    pub fn active_transfers(&self) -> usize {
        self.active_transfers.load(Ordering::SeqCst)
    }

    /// Returns a combined [`BackgroundWorkStatus`] snapshot.
    pub fn status(&self) -> BackgroundWorkStatus {
        BackgroundWorkStatus {
            active_standalone_tunnels: self.active_standalone_tunnels(),
            active_transfers: self.active_transfers(),
        }
    }

    /// Returns `true` after [`BackgroundWorkSource::stop_all`] has been triggered.
    pub fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    /// Clears the `stop_requested` flag (useful when resuming background operations).
    pub fn reset_stop_requested(&self) {
        self.stop_requested.store(false, Ordering::SeqCst);
    }
}

impl BackgroundWorkSource for BackgroundWorkTracker {
    fn status(&self) -> BackgroundWorkStatus {
        self.status()
    }

    fn stop_all(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
    }
}

/// State machine governing confirmation when closing the final GUI window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseGuardState {
    /// No close confirmation modal is currently displayed.
    #[default]
    Idle,
    /// Waiting for user confirmation before closing `window_id` with active background work.
    Confirming {
        window_id: window::Id,
        active_standalone_tunnels: usize,
        active_transfers: usize,
    },
}

/// Daemon lifecycle phase reflecting whether background operations keep the process alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonLifecycle {
    /// At least one GUI window is open or starting up.
    #[default]
    Active,
    /// All GUI windows were closed while Standalone `Tunnel`s/`Transfer`s remain running.
    BackgroundRunning,
    /// Shutdown has been confirmed and the daemon is exiting.
    Exiting,
}

/// Formats a concise human-readable summary of active background work for the close dialog.
pub fn format_background_work_summary(tunnels: usize, transfers: usize) -> String {
    let tunnel_part = match tunnels {
        0 => None,
        1 => Some("1 Standalone Tunnel".to_string()),
        n => Some(format!("{n} Standalone Tunnels")),
    };
    let transfer_part = match transfers {
        0 => None,
        1 => Some("1 active Transfer".to_string()),
        n => Some(format!("{n} active Transfers")),
    };

    match (tunnel_part, transfer_part) {
        (Some(t), Some(f)) => {
            format!("{t} and {f} are still running. Closing now would interrupt active work.")
        }
        (Some(t), None) => {
            format!("{t} still running. Closing now would stop forwarding.")
        }
        (None, Some(f)) => {
            format!("{f} still in progress. Closing now would interrupt file transfers.")
        }
        (None, None) => "Background operations are still running.".to_string(),
    }
}

/// Overlays the Close Guard confirmation modal atop `base` when `Confirming` targets `window_id`.
pub fn wrap_with_close_guard_modal<'a, Msg: Clone + 'a>(
    base: Element<'a, Msg, Theme, iced::Renderer>,
    state: CloseGuardState,
    window_id: window::Id,
    on_cancel: Msg,
    on_keep_running: Msg,
    on_stop_and_quit: Msg,
    palette: &ThemePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    let CloseGuardState::Confirming {
        window_id: target_win,
        active_standalone_tunnels,
        active_transfers,
    } = state
    else {
        return base;
    };

    if target_win != window_id {
        return base;
    }

    let chrome = ChromePalette::from_palette(palette);
    let summary = format_background_work_summary(active_standalone_tunnels, active_transfers);

    let dialog_card = container(
        column![
            row![
                status_dot(chrome.warning),
                text("Background Work Active")
                    .size(16)
                    .color(chrome.text_primary),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text(summary).size(13).color(chrome.text_secondary),
            hairline_h(chrome),
            row![
                button(text("Cancel").size(12))
                    .padding([6, 12])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(on_cancel),
                button(text("Keep Running in Background").size(12))
                    .padding([6, 12])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(on_keep_running),
                button(text("Stop & Quit").size(12))
                    .padding([6, 12])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(on_stop_and_quit),
            ]
            .spacing(8)
            .wrap()
            .vertical_spacing(6),
        ]
        .spacing(12)
        .padding(18),
    )
    .max_width(520)
    .style(move |_theme: &Theme| modal_card_style(chrome, true));

    let backdrop = opaque(
        container(dialog_card)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(16)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.46))),
                ..Default::default()
            }),
    );

    stack![base, backdrop].into()
}
