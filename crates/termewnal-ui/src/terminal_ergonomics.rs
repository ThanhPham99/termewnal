//! Terminal ergonomics UI state and overlays: `Ctrl+F` search bar, multi-line paste guard,
//! drag-and-drop routing, and OS clipboard/URL helpers.

use std::path::{Path, PathBuf};

use iced::widget::{Id, button, column, container, opaque, row, rule, space, text, text_input};
use iced::{Alignment, Color, Element, Length, Padding, Theme};
use termewnal_core::{CursorStyle, RgbColor, ThemePalette};
use termewnal_pty::{CursorShape, GridSearchMatch, PtySession, TerminalBuffer, shell_escape_path};

use crate::app::{Message, SessionId};
use crate::ui_theme::{
    ChromePalette, PillTone, card_container_style, modal_card_style, modal_overlay,
    pill_button_style, status_dot, styled_input_style,
};

/// Returns the widget `Id` for the `Ctrl+F` scrollback search input.
pub fn search_input_id() -> Id {
    Id::new("termewnal-scrollback-search-input")
}

/// Converts a persisted [`CursorStyle`] into `alacritty_terminal`'s [`CursorShape`].
pub const fn cursor_style_to_shape(style: CursorStyle) -> CursorShape {
    match style {
        CursorStyle::Block => CursorShape::Block,
        CursorStyle::Underline => CursorShape::Underline,
        CursorStyle::Beam => CursorShape::Beam,
    }
}

/// Converts a core [`RgbColor`] into an [`iced::Color`].
pub const fn rgb_to_iced(c: RgbColor) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

/// Converts a built-in theme name to the closest matching [`iced::Theme`].
pub fn iced_theme_from_name(name: &str) -> Theme {
    match termewnal_core::BuiltinTheme::resolve(name) {
        termewnal_core::BuiltinTheme::Catppuccin => Theme::CatppuccinMocha,
        termewnal_core::BuiltinTheme::TokyoNight => Theme::TokyoNight,
        termewnal_core::BuiltinTheme::Dracula => Theme::Dracula,
        termewnal_core::BuiltinTheme::Nord => Theme::Nord,
        termewnal_core::BuiltinTheme::Dark => Theme::Dark,
        termewnal_core::BuiltinTheme::Light => Theme::Light,
    }
}

/// Opens a URL or file path using the OS default handler without blocking the UI thread.
pub fn open_url_with_system(url: &str) -> Result<(), String> {
    open::that_detached(url).map_err(|err| format!("failed to open '{url}': {err}"))
}

/// Fallback clipboard read via `arboard` (including Wayland `wayland-data-control`).
pub fn read_clipboard_via_arboard() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok().filter(|s| !s.is_empty())
}

/// Returns `true` when pasting `text` would transmit at least one newline/carriage return.
pub fn is_multiline_paste(text: &str) -> bool {
    text.contains('\n') || text.contains('\r')
}

/// Formats clipboard text for terminal PTY insertion, honoring bracketed paste mode when enabled.
pub fn format_paste_bytes(text: &str, bracketed_paste: bool) -> Vec<u8> {
    if bracketed_paste {
        let sanitized = text
            .replace("\x1b[201~", "")
            .replace("\x1b[200~", "")
            .replace("\r\n", "\n");
        let mut out = String::with_capacity(sanitized.len() + 12);
        out.push_str("\x1b[200~");
        out.push_str(&sanitized);
        out.push_str("\x1b[201~");
        out.into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Pending multi-line paste awaiting user confirmation before dispatch to a `Session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPaste {
    pub session_id: SessionId,
    pub text: String,
    pub line_count: usize,
}

impl PendingPaste {
    /// Creates a [`PendingPaste`] prompt descriptor from `text`.
    pub fn new(session_id: SessionId, text: String) -> Self {
        let normalized = text.replace("\r\n", "\n");
        let line_count = normalized.split(['\n', '\r']).count().max(2);
        Self {
            session_id,
            text,
            line_count,
        }
    }

    /// Produces a bounded preview snippet suitable for displaying inside the confirmation dialog.
    pub fn preview(&self) -> String {
        const MAX_LINES: usize = 6;
        const MAX_CHARS: usize = 320;

        let mut preview = String::new();
        for (idx, line) in self.text.lines().take(MAX_LINES).enumerate() {
            if idx > 0 {
                preview.push('\n');
            }
            preview.push_str(line);
            if preview.chars().count() >= MAX_CHARS {
                break;
            }
        }
        let truncated: String = preview.chars().take(MAX_CHARS).collect();
        if self.text.chars().count() > truncated.chars().count() {
            format!("{truncated}...")
        } else {
            truncated
        }
    }
}

/// Routed outcome when a desktop file is dropped onto the Termewnal window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDropAction {
    /// Insert a POSIX shell-escaped path into the target terminal `Session`.
    InsertShellPath {
        session_id: SessionId,
        escaped_text: String,
    },
    /// Enqueue an SFTP upload `Transfer` when dropped onto the SFTP drawer or `Shift`-dropped on SSH.
    UploadToSftp {
        session_id: SessionId,
        path: PathBuf,
    },
}

/// Classifies a file-drop event cleanly so Ticket 13's SFTP drawer can hook `Shift`-drops.
pub fn classify_file_drop(
    session_id: SessionId,
    path: &Path,
    is_ssh_session: bool,
    shift_held: bool,
    over_sftp_drawer: bool,
) -> FileDropAction {
    if is_ssh_session && (over_sftp_drawer || shift_held) {
        FileDropAction::UploadToSftp {
            session_id,
            path: path.to_path_buf(),
        }
    } else {
        FileDropAction::InsertShellPath {
            session_id,
            escaped_text: format!("{} ", shell_escape_path(path)),
        }
    }
}

/// Interactive `Ctrl+F` scrollback search bar state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchBarState {
    pub open: bool,
    pub input_focused: bool,
    pub query: String,
    pub matches: Vec<GridSearchMatch>,
    pub active_index: Option<usize>,
}

impl SearchBarState {
    /// Opens the search bar and refreshes matches against `session` if a query is already present.
    pub fn open(&mut self, session: Option<&PtySession>) -> Option<GridSearchMatch> {
        self.open_buffer(session.map(|pty| pty.buffer().as_ref()))
    }

    /// Opens the search bar against a shared [`TerminalBuffer`] (local or SSH `Session`).
    pub fn open_buffer(&mut self, buffer: Option<&TerminalBuffer>) -> Option<GridSearchMatch> {
        self.open = true;
        self.input_focused = true;
        self.refresh_matches_buffer(buffer, true)
    }

    /// Closes the search bar and clears active match highlights.
    pub fn close(&mut self) {
        self.open = false;
        self.input_focused = false;
        self.matches.clear();
        self.active_index = None;
    }

    /// Updates the incremental search `query` and returns the newly selected match, if any.
    pub fn set_query(
        &mut self,
        query: String,
        session: Option<&PtySession>,
    ) -> Option<GridSearchMatch> {
        self.set_query_buffer(query, session.map(|pty| pty.buffer().as_ref()))
    }

    /// Updates the incremental search `query` against a [`TerminalBuffer`].
    pub fn set_query_buffer(
        &mut self,
        query: String,
        buffer: Option<&TerminalBuffer>,
    ) -> Option<GridSearchMatch> {
        self.query = query;
        self.refresh_matches_buffer(buffer, true)
    }

    /// Re-evaluates search matches against `session` (e.g. when new PTY output arrives).
    pub fn refresh_matches(
        &mut self,
        session: Option<&PtySession>,
        reset_to_first: bool,
    ) -> Option<GridSearchMatch> {
        self.refresh_matches_buffer(session.map(|pty| pty.buffer().as_ref()), reset_to_first)
    }

    /// Re-evaluates search matches against a [`TerminalBuffer`].
    pub fn refresh_matches_buffer(
        &mut self,
        buffer: Option<&TerminalBuffer>,
        reset_to_first: bool,
    ) -> Option<GridSearchMatch> {
        if !self.open || self.query.is_empty() {
            self.matches.clear();
            self.active_index = None;
            return None;
        }

        self.matches = buffer
            .map(|buf| buf.search_scrollback(&self.query))
            .unwrap_or_default();

        if self.matches.is_empty() {
            self.active_index = None;
            None
        } else {
            let idx = if reset_to_first {
                0
            } else {
                self.active_index
                    .map(|prev| prev.min(self.matches.len() - 1))
                    .unwrap_or(0)
            };
            self.active_index = Some(idx);
            self.matches.get(idx).copied()
        }
    }

    /// Advances to the next match (wrapping to `0`), returning the selected match.
    pub fn step_next(&mut self) -> Option<GridSearchMatch> {
        if self.matches.is_empty() {
            self.active_index = None;
            return None;
        }
        let next = match self.active_index {
            Some(idx) => (idx + 1) % self.matches.len(),
            None => 0,
        };
        self.active_index = Some(next);
        self.matches.get(next).copied()
    }

    /// Moves to the previous match (wrapping to the last match), returning the selected match.
    pub fn step_prev(&mut self) -> Option<GridSearchMatch> {
        if self.matches.is_empty() {
            self.active_index = None;
            return None;
        }
        let prev = match self.active_index {
            Some(0) | None => self.matches.len() - 1,
            Some(idx) => idx - 1,
        };
        self.active_index = Some(prev);
        self.matches.get(prev).copied()
    }

    /// Returns the active match if one is selected.
    pub fn active_match(&self) -> Option<GridSearchMatch> {
        self.active_index
            .and_then(|idx| self.matches.get(idx).copied())
    }

    /// Formats a concise match counter label (e.g. `"1 / 4"` or `"No matches"`).
    pub fn status_label(&self) -> String {
        if self.query.is_empty() {
            String::new()
        } else if let Some(idx) = self.active_index {
            format!("{} / {}", idx + 1, self.matches.len())
        } else {
            "No matches".to_string()
        }
    }
}

/// Renders the floating top-right `Ctrl+F` search bar overlay.
pub fn search_bar_view<'a>(
    state: &SearchBarState,
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);

    let input = text_input("Find in scrollback...", &state.query)
        .id(search_input_id())
        .on_input(Message::SearchQueryChanged)
        .on_submit(Message::SearchNext)
        .size(12)
        .padding([5, 9])
        .style(styled_input_style(chrome))
        .width(Length::Fixed(220.0));

    let counter = text(state.status_label())
        .size(11)
        .color(chrome.text_secondary);

    let bar_row = row![
        input,
        counter,
        button(text("Prev").size(11))
            .padding([4, 9])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::SearchPrev),
        button(text("Next").size(11))
            .padding([4, 9])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::SearchNext),
        button(text("Close").size(11))
            .padding([4, 9])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::CloseSearch),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let card = container(bar_row)
        .padding(Padding::from([7, 10]))
        .style(move |_theme| modal_card_style(chrome, true));

    container(opaque(card))
        .width(Length::Fill)
        .padding(Padding::from([12, 16]))
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Top)
        .into()
}

/// Renders the multi-line paste confirmation modal overlay.
pub fn paste_confirm_modal_view<'a>(
    pending: &PendingPaste,
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);

    let heading = row![
        status_dot(chrome.warning),
        text(format!(
            "Paste {} lines into terminal?",
            pending.line_count
        ))
        .size(15)
        .color(chrome.text_primary),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let hint = text("Multi-line text may execute commands immediately.")
        .size(12)
        .color(chrome.text_muted);

    let preview_box = container(text(pending.preview()).size(12).color(chrome.text_secondary))
        .width(Length::Fill)
        .padding([8, 10])
        .style(move |_theme| card_container_style(chrome, false));

    let actions = row![
        space::horizontal(),
        button(text("Cancel").size(12))
            .padding([6, 12])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::CancelPendingPaste),
        button(text("Paste").size(12))
            .padding([6, 14])
            .style(pill_button_style(chrome, PillTone::Primary))
            .on_press(Message::ConfirmPendingPaste),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let card = container(
        column![heading, hint, preview_box, rule::horizontal(1), actions].spacing(10),
    )
    .width(Length::Fixed(460.0))
    .padding(16)
    .style(move |_theme| modal_card_style(chrome, true));

    modal_overlay(card.into(), Some(Message::CancelPendingPaste), 24.0)
}
