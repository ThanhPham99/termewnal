//! Terminal ergonomics UI state and overlays: `Ctrl+F` search bar, multi-line paste guard,
//! drag-and-drop routing, and OS clipboard/URL helpers.

use std::path::{Path, PathBuf};

use iced::widget::{Id, button, column, container, opaque, row, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Shadow, Theme};
use termewnal_core::{CursorStyle, RgbColor, ThemePalette};
use termewnal_pty::{CursorShape, GridSearchMatch, PtySession, shell_escape_path};

use crate::app::{Message, SessionId};

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
        self.open = true;
        self.input_focused = true;
        self.refresh_matches(session, true)
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
        self.query = query;
        self.refresh_matches(session, true)
    }

    /// Re-evaluates search matches against `session` (e.g. when new PTY output arrives).
    pub fn refresh_matches(
        &mut self,
        session: Option<&PtySession>,
        reset_to_first: bool,
    ) -> Option<GridSearchMatch> {
        if !self.open || self.query.is_empty() {
            self.matches.clear();
            self.active_index = None;
            return None;
        }

        self.matches = session
            .map(|pty| pty.search_scrollback(&self.query))
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
    let surface = rgb_to_iced(palette.ui_surface);
    let border_color = rgb_to_iced(palette.ui_border);
    let fg = rgb_to_iced(palette.foreground);
    let accent = rgb_to_iced(palette.ui_accent);

    let input = text_input("Find in scrollback...", &state.query)
        .id(search_input_id())
        .on_input(Message::SearchQueryChanged)
        .on_submit(Message::SearchNext)
        .size(13)
        .padding([4, 8])
        .width(Length::Fixed(220.0));

    let counter = text(state.status_label()).size(12).color(fg);

    let nav_button = |label: &'static str, msg: Message| {
        styled_action_button(label, msg, fg, border_color, accent, Color::TRANSPARENT)
    };

    let bar_row = row![
        input,
        counter,
        nav_button("Prev", Message::SearchPrev),
        nav_button("Next", Message::SearchNext),
        nav_button("Close", Message::CloseSearch),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let card = container(bar_row)
        .padding(Padding::from([6, 10]))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(surface)),
            text_color: Some(fg),
            border: Border {
                color: border_color,
                width: 1.0,
                radius: 6.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        });

    container(opaque(card))
        .width(Length::Fill)
        .padding(Padding::from([10, 14]))
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Top)
        .into()
}

/// Renders the multi-line paste confirmation modal overlay.
pub fn paste_confirm_modal_view<'a>(
    pending: &PendingPaste,
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let surface = rgb_to_iced(palette.ui_surface);
    let bg = rgb_to_iced(palette.background);
    let border_color = rgb_to_iced(palette.ui_border);
    let fg = rgb_to_iced(palette.foreground);
    let accent = rgb_to_iced(palette.ui_accent);

    let heading = text(format!(
        "Paste {} lines into terminal?",
        pending.line_count
    ))
    .size(15)
    .color(fg);

    let hint = text("Multi-line text may execute commands immediately.")
        .size(12)
        .color(Color { a: 0.8, ..fg });

    let preview_box = container(text(pending.preview()).size(12).color(fg))
        .width(Length::Fill)
        .padding(8)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(bg)),
            text_color: Some(fg),
            border: Border {
                color: border_color,
                width: 1.0,
                radius: 4.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        });

    let actions = row![
        styled_action_button(
            "Cancel",
            Message::CancelPendingPaste,
            fg,
            border_color,
            accent,
            Color::TRANSPARENT,
        ),
        styled_action_button(
            "Paste",
            Message::ConfirmPendingPaste,
            fg,
            accent,
            accent,
            Color { a: 0.25, ..accent },
        ),
    ]
    .spacing(8);

    let dialog = container(opaque(
        container(column![heading, hint, preview_box, actions].spacing(10))
            .width(Length::Fixed(460.0))
            .padding(16)
            .style(move |_theme| container::Style {
                background: Some(Background::Color(surface)),
                text_color: Some(fg),
                border: Border {
                    color: border_color,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                shadow: Shadow::default(),
                snap: true,
            }),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .style(|_theme| container::Style {
        background: Some(Background::Color(Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.45,
        })),
        ..container::Style::default()
    });

    opaque(dialog)
}

fn styled_action_button<'a>(
    label: &'static str,
    msg: Message,
    fg: Color,
    border_color: Color,
    accent: Color,
    base_bg: Color,
) -> Element<'a, Message, Theme, iced::Renderer> {
    button(text(label).size(12).color(fg))
        .padding([4, 10])
        .style(move |_theme, status| {
            let bg = match status {
                button::Status::Hovered | button::Status::Pressed => Color {
                    a: 0.35,
                    ..accent
                },
                _ => base_bg,
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: fg,
                border: Border {
                    color: border_color,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                shadow: Shadow::default(),
                snap: true,
            }
        })
        .on_press(msg)
        .into()
}
