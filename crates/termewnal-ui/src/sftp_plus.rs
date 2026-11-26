//! Remote-only SFTP Panel (`SFTP+`) UI, browsing state, file operations, context menu, and modals.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Local};
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::widget::{
    Id, button, column, container, mouse_area, opaque, row, scrollable, space, stack, text,
    text_editor, text_input,
};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Padding, Point, Rectangle, Size,
    Theme, keyboard, mouse,
};
use termewnal_core::{SftpState, ThemePalette};
use termewnal_ssh::{RemoteDirEntry, RemoteEntryKind, SftpTransportMode, TransferId};

use crate::app::{Message, SessionId};
use crate::app_picker::{InstalledApp, filter_installed_apps};
use crate::sftp_drawer::{
    SftpConnectionState, SftpDrawerState, format_bytes, render_transfer_queue_section,
};
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, FONT_SUBHEAD, PillTone, anchored_menu, apple_checkbox,
    badge_pill, hairline_h, inset_well_style, list_row_button_style, modal_body_height,
    modal_card_style, modal_overlay, modal_width, mono_font, pill_button_style, status_dot,
    styled_input_style, tab_bar_button_style, tabby_scrollable_style, ui_font, ui_font_medium,
    ui_font_semibold,
};

const DOUBLE_CLICK_MAX_MS: u64 = 400;
const SFTP_MODAL_INPUT_ID: &str = "sftp-plus-modal-input";

/// Sortable columns displayed in the SFTP Panel file table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SftpSortColumn {
    #[default]
    Name,
    Size,
    Modified,
    Permission,
}

/// Remote directory entry view model rendered in the SFTP Panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBrowserEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub size: u64,
    pub modified_unix_secs: Option<i64>,
    pub permissions: Option<u32>,
}

impl FileBrowserEntry {
    /// Converts a remote [`RemoteDirEntry`] returned by `termewnal-ssh`.
    pub fn from_remote(entry: &RemoteDirEntry) -> Self {
        let is_symlink = matches!(entry.kind, RemoteEntryKind::Symlink);
        let is_dir = matches!(entry.kind, RemoteEntryKind::Dir) || entry.is_dir();
        Self {
            name: entry.name.clone(),
            full_path: entry.path.clone(),
            is_dir,
            is_symlink,
            symlink_target: None,
            size: entry.size,
            modified_unix_secs: entry.modified_epoch_secs.map(i64::from),
            permissions: entry.permissions,
        }
    }
}

/// Remote navigation, history, filtering, sorting, and selection state for one SSH `Session`.
#[derive(Debug, Clone)]
pub struct SftpPaneBrowseState {
    pub initialized: bool,
    pub current_path: String,
    pub path_input: String,
    pub home_path: Option<String>,
    pub history: Vec<String>,
    pub history_index: usize,
    pub navigating_history: bool,
    pub show_hidden: bool,
    pub filter_open: bool,
    pub filter_query: String,
    pub bookmarks_open: bool,
    pub sort_col: SftpSortColumn,
    pub sort_asc: bool,
    pub selected_paths: BTreeSet<String>,
    pub loading: bool,
    pub error: Option<String>,
}

impl Default for SftpPaneBrowseState {
    fn default() -> Self {
        Self {
            initialized: false,
            current_path: String::new(),
            path_input: String::new(),
            home_path: None,
            history: Vec::new(),
            history_index: 0,
            navigating_history: false,
            show_hidden: true,
            filter_open: false,
            filter_query: String::new(),
            bookmarks_open: false,
            sort_col: SftpSortColumn::Name,
            sort_asc: true,
            selected_paths: BTreeSet::new(),
            loading: false,
            error: None,
        }
    }
}

impl SftpPaneBrowseState {
    /// Returns `true` when there is an earlier directory in navigation history.
    pub fn can_go_back(&self) -> bool {
        self.history_index > 0 && !self.history.is_empty()
    }

    /// Returns `true` when there is a forward directory in navigation history.
    pub fn can_go_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    /// Returns the path that `Back` would open and marks history navigation active.
    pub fn step_back(&mut self) -> Option<String> {
        if !self.can_go_back() {
            return None;
        }
        self.history_index -= 1;
        self.navigating_history = true;
        self.history.get(self.history_index).cloned()
    }

    /// Returns the path that `Forward` would open and marks history navigation active.
    pub fn step_forward(&mut self) -> Option<String> {
        if !self.can_go_forward() {
            return None;
        }
        self.history_index += 1;
        self.navigating_history = true;
        self.history.get(self.history_index).cloned()
    }

    /// Records a resolved directory arrival into history and clears stale selection.
    pub fn record_arrival(&mut self, resolved_path: &str) {
        let trimmed = resolved_path.trim();
        if trimmed.is_empty() {
            return;
        }
        self.initialized = true;
        if self.home_path.is_none() {
            self.home_path = Some(trimmed.to_string());
        }
        if self.navigating_history {
            self.navigating_history = false;
            if let Some(slot) = self.history.get_mut(self.history_index) {
                *slot = trimmed.to_string();
            }
        } else if self.history.is_empty() {
            self.history.push(trimmed.to_string());
            self.history_index = 0;
        } else if self.history.get(self.history_index).map(String::as_str) != Some(trimmed) {
            self.history.truncate(self.history_index + 1);
            self.history.push(trimmed.to_string());
            self.history_index = self.history.len().saturating_sub(1);
        }
        if self.current_path != trimmed {
            self.selected_paths.clear();
        }
        self.current_path = trimmed.to_string();
        self.path_input = trimmed.to_string();
        self.error = None;
    }

    /// Toggles sort column or direction when the same column header is clicked.
    pub fn toggle_sort(&mut self, col: SftpSortColumn) {
        if self.sort_col == col {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort_col = col;
            self.sort_asc = true;
        }
    }

    /// Resets remote session-scoped navigation while keeping user preferences (`show_hidden`, sort).
    pub fn reset_remote_session(&mut self) {
        self.initialized = false;
        self.current_path.clear();
        self.path_input.clear();
        self.home_path = None;
        self.history.clear();
        self.history_index = 0;
        self.navigating_history = false;
        self.bookmarks_open = false;
        self.selected_paths.clear();
        self.loading = false;
        self.error = None;
    }
}

/// Clipboard operation kind for `Copy` / `Cut` / `Paste` inside the remote SFTP Panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpClipboardOp {
    Copy,
    Cut,
}

/// Internal remote file clipboard for the SFTP Panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpClipboard {
    pub op: SftpClipboardOp,
    pub entries: Vec<FileBrowserEntry>,
}

/// Right-click context menu state inside the SFTP Panel.
#[derive(Debug, Clone, Default)]
pub struct SftpContextMenuState {
    pub open: bool,
    pub target_path: Option<String>,
    pub position: Point,
}

impl SftpContextMenuState {
    /// Opens the floating context menu at `position`.
    pub fn show(&mut self, target_path: Option<String>, position: Point) {
        self.open = true;
        self.target_path = target_path;
        self.position = position;
    }

    /// Closes the context menu.
    pub fn close(&mut self) {
        self.open = false;
        self.target_path = None;
    }
}

/// Actions offered in the SFTP Panel right-click context menu and keyboard shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpContextActionKind {
    Download,
    DownloadTo,
    UploadFiles,
    UploadFolder,
    View,
    Edit,
    OpenWith,
    Copy,
    Cut,
    Paste,
    Rename,
    Delete,
    EditPermissions,
    Properties,
    NewFolder,
    NewFile,
    Refresh,
    SelectAll,
    InvertSelection,
    CopyPath,
}

/// Modal dialogs owned by the SFTP Panel (`SFTP+`).
#[derive(Debug, Clone, Default)]
pub enum SftpModalState {
    #[default]
    None,
    CreateEntry {
        parent_dir: String,
        is_dir: bool,
        name: String,
        error: Option<String>,
    },
    Rename {
        old_path: String,
        new_name: String,
        error: Option<String>,
    },
    EditPermissions {
        target_paths: Vec<String>,
        octal_input: String,
        mode_bits: u32,
        error: Option<String>,
    },
    Properties {
        entry: FileBrowserEntry,
        extra_selected: usize,
        total_selected_bytes: u64,
    },
    FileViewer {
        path: String,
        file_name: String,
        editable: bool,
        loading: bool,
        saving: bool,
        dirty: bool,
        error: Option<String>,
        content: text_editor::Content<iced::Renderer>,
    },
    OpenWith {
        entry: FileBrowserEntry,
        loading: bool,
        search_query: String,
        apps: Vec<InstalledApp>,
        selected_launch_path: Option<PathBuf>,
        last_clicked: Option<(PathBuf, Instant)>,
        error: Option<String>,
    },
}

impl SftpModalState {
    /// Returns `true` when any SFTP Panel dialog is currently displayed.
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Metadata for a single remote file downloaded via `Open with...` and monitored for local saves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedRemoteFile {
    pub id: u64,
    pub session_id: SessionId,
    pub remote_path: String,
    pub file_name: String,
    pub local_path: PathBuf,
    pub app_path: PathBuf,
    pub app_name: String,
    pub last_synced_hash: u64,
    pub in_flight_upload_hash: Option<u64>,
    pub pending_upload_id: Option<TransferId>,
    pub pending_local_change: bool,
    pub auto_upload: bool,
    pub upload_failed: bool,
    pub status_note: Option<String>,
}

/// Remote-only SFTP+ browsing and interaction state embedded inside [`SftpDrawerState`].
#[derive(Debug, Clone, Default)]
pub struct SftpPlusState {
    pub transfers_panel_open: bool,
    pub remote: SftpPaneBrowseState,
    pub clipboard: Option<SftpClipboard>,
    pub context_menu: SftpContextMenuState,
    pub modal: SftpModalState,
    pub last_click: Option<(String, Instant)>,
    pub watched_files: Vec<WatchedRemoteFile>,
}

impl SftpPlusState {
    /// Clears session-specific remote history/selection when switching or resetting SSH sessions.
    pub fn on_remote_session_changed(&mut self) {
        self.remote.reset_remote_session();
        self.context_menu.close();
        self.modal = SftpModalState::None;
        self.watched_files.clear();
    }

    /// Updates remote history, home path, and selection upon a successful remote `list_dir`.
    pub fn on_remote_listed(&mut self, resolved_path: &str) {
        self.remote.loading = false;
        self.remote.record_arrival(resolved_path);
    }

    /// Records a single click on remote `path` and returns `true` when it qualifies as a double-click.
    pub fn handle_row_click(&mut self, path: &str, ctrl_held: bool) -> bool {
        self.context_menu.close();
        if ctrl_held {
            if !self.remote.selected_paths.insert(path.to_string()) {
                self.remote.selected_paths.remove(path);
            }
            self.last_click = None;
            return false;
        }

        self.remote.selected_paths.clear();
        self.remote.selected_paths.insert(path.to_string());

        let now = Instant::now();
        let is_double = self.last_click.as_ref().is_some_and(|(prev_path, prev_at)| {
            prev_path == path
                && now.duration_since(*prev_at).as_millis() <= u128::from(DOUBLE_CLICK_MAX_MS)
        });
        if is_double {
            self.last_click = None;
            true
        } else {
            self.last_click = Some((path.to_string(), now));
            false
        }
    }
}

/// Returns filtered & sorted remote entries for `state`.
pub fn visible_remote_entries(state: &SftpDrawerState) -> Vec<FileBrowserEntry> {
    let raw: Vec<FileBrowserEntry> = state
        .entries()
        .iter()
        .map(FileBrowserEntry::from_remote)
        .collect();
    sort_and_filter_entries(&raw, &state.plus.remote)
}

/// Filters hidden entries and substring query, then sorts directories-first using `prefs`.
pub fn sort_and_filter_entries(
    entries: &[FileBrowserEntry],
    prefs: &SftpPaneBrowseState,
) -> Vec<FileBrowserEntry> {
    let query = prefs.filter_query.trim().to_ascii_lowercase();
    let mut out: Vec<FileBrowserEntry> = entries
        .iter()
        .filter(|e| prefs.show_hidden || !e.name.starts_with('.'))
        .filter(|e| query.is_empty() || e.name.to_ascii_lowercase().contains(&query))
        .cloned()
        .collect();

    out.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return b.is_dir.cmp(&a.is_dir);
        }
        let primary = match prefs.sort_col {
            SftpSortColumn::Name => a
                .name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.name.cmp(&b.name)),
            SftpSortColumn::Size => a.size.cmp(&b.size),
            SftpSortColumn::Modified => a.modified_unix_secs.cmp(&b.modified_unix_secs),
            SftpSortColumn::Permission => a.permissions.cmp(&b.permissions),
        };
        let ordered = if prefs.sort_asc {
            primary
        } else {
            primary.reverse()
        };
        ordered.then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
    out
}

/// Formats byte count cleanly matching Tabby SFTP+ (`512 B`, `2.5 KB`, `362.3 KB`, `1.4 MB`).
pub fn format_sftp_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

/// Formats a UNIX epoch second timestamp as `YYYY-MM-DD HH:MM:SS` in local time.
pub fn format_modified_timestamp(unix_secs: Option<i64>) -> String {
    let Some(secs) = unix_secs else {
        return "—".to_string();
    };
    let Some(utc) = DateTime::from_timestamp(secs, 0) else {
        return "—".to_string();
    };
    let local: DateTime<Local> = utc.with_timezone(&Local);
    local.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Formats POSIX mode bits into 9-char `rwxr-xr-x` string matching Tabby SFTP+.
pub fn format_posix_mode(mode: Option<u32>) -> String {
    let Some(m) = mode else {
        return "---------".to_string();
    };
    let bit = |mask: u32, ch: char| if (m & mask) != 0 { ch } else { '-' };
    let mut s = String::with_capacity(9);
    s.push(bit(0o400, 'r'));
    s.push(bit(0o200, 'w'));
    s.push(bit(0o100, 'x'));
    s.push(bit(0o040, 'r'));
    s.push(bit(0o020, 'w'));
    s.push(bit(0o010, 'x'));
    s.push(bit(0o004, 'r'));
    s.push(bit(0o002, 'w'));
    s.push(bit(0o001, 'x'));
    s
}

/// Formats mode bits into a 3-digit octal string (`755`).
pub fn format_octal_mode(mode: u32) -> String {
    format!("{:03o}", mode & 0o777)
}

/// Renders the unified remote-only SFTP Panel (`SFTP+`) docked to the right of an SSH `Pane`.
#[allow(clippy::too_many_arguments)]
pub fn render_sftp_panel<'a>(
    session_id: SessionId,
    state: &'a SftpDrawerState,
    sftp_state: &'a SftpState,
    bookmark_key: Option<&'a str>,
    has_connected_ssh: bool,
    panel_width: f32,
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);
    let snap = state.queue_snapshot();
    let prefs = &state.plus.remote;
    let remote_label = state.session_label().unwrap_or("Disconnected");
    let is_focused = state.is_panel_focused();

    let mut title_row = row![
        SftpGlyph::widget(
            SftpGlyph::Globe,
            if is_focused || has_connected_ssh {
                chrome.accent
            } else {
                chrome.text_secondary
            },
            13.0,
        ),
        text("SFTP+")
            .size(FONT_BODY)
            .font(ui_font_semibold())
            .color(if is_focused {
                chrome.accent
            } else {
                chrome.text_primary
            }),
        text(remote_label)
            .size(FONT_META)
            .font(mono_font())
            .color(chrome.text_muted)
            .wrapping(text::Wrapping::None),
    ]
    .spacing(5)
    .align_y(Alignment::Center);

    if let Some(status_label) = state.connection_state().status_label() {
        let (fg, bg) = match state.connection_state() {
            SftpConnectionState::AutoReconnecting { .. } => (chrome.warning, chrome.warning_soft),
            SftpConnectionState::ReconnectedSuccessfully => (chrome.success, chrome.success_soft),
            SftpConnectionState::Disconnected { .. } => (chrome.danger, chrome.danger_soft),
            SftpConnectionState::Idle | SftpConnectionState::Connected => {
                (chrome.text_secondary, chrome.secondary)
            }
        };
        title_row = title_row.push(badge_pill(status_label, fg, bg, Color::TRANSPARENT));
    }

    let transfers_active = snap.active_count + snap.queued_count;
    let transfers_label = if transfers_active > 0 {
        format!("Transfers ({transfers_active})")
    } else if snap.transfers.is_empty() {
        "Transfers".to_string()
    } else {
        format!("Transfers ({})", snap.transfers.len())
    };
    let transfers_btn = button(
        row![
            SftpGlyph::widget(
                SftpGlyph::Transfers,
                if state.plus.transfers_panel_open {
                    chrome.accent_fg
                } else if transfers_active > 0 {
                    chrome.accent
                } else {
                    chrome.text_secondary
                },
                12.0,
            ),
            text(transfers_label)
                .size(FONT_META)
                .font(ui_font_medium())
                .color(if state.plus.transfers_panel_open {
                    chrome.accent_fg
                } else {
                    chrome.text_primary
                }),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([3, 8])
    .style(pill_button_style(
        chrome,
        if state.plus.transfers_panel_open {
            PillTone::Primary
        } else {
            PillTone::Subtle
        },
    ))
    .on_press(Message::SftpSessionToggleTransfersPanel(session_id));

    let header_bar = container(
        row![
            container(title_row).width(Length::Fill).clip(true),
            transfers_btn,
            button(text("×").size(14.0).color(chrome.text_secondary))
                .padding([2, 7])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::SftpSessionClose(session_id)),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([5, 8]))
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(chrome.chrome_bg.into()),
        border: Border {
            color: if is_focused {
                chrome.accent
            } else {
                Color::TRANSPARENT
            },
            width: if is_focused { 1.0 } else { 0.0 },
            radius: 0.0.into(),
        },
        ..container::Style::default()
    });

    let nav_icon = |glyph: SftpGlyph, enabled: bool, msg: Message| {
        let color = if enabled {
            chrome.text_secondary
        } else {
            Color {
                a: 0.35,
                ..chrome.text_muted
            }
        };
        let mut btn = button(SftpGlyph::widget(glyph, color, 13.0))
            .padding([4, 6])
            .style(tab_bar_button_style(chrome, false, false));
        if enabled {
            btn = btn.on_press(msg);
        }
        btn
    };

    let current_path = state.current_path();
    let bookmarks = bookmark_key
        .map(|key| sftp_state.remote_bookmarks(key))
        .unwrap_or(&[]);
    let current_bookmarked = !current_path.is_empty() && bookmarks.iter().any(|b| b == current_path);

    let hidden_btn = button(SftpGlyph::widget(
        SftpGlyph::Eye,
        if prefs.show_hidden {
            chrome.accent
        } else {
            chrome.text_muted
        },
        13.0,
    ))
    .padding([4, 6])
    .style(tab_bar_button_style(chrome, prefs.show_hidden, false))
    .on_press(Message::SftpSessionToggleHidden(session_id));

    let filter_btn = button(SftpGlyph::widget(
        SftpGlyph::Filter,
        if prefs.filter_open || !prefs.filter_query.is_empty() {
            chrome.accent
        } else {
            chrome.text_secondary
        },
        13.0,
    ))
    .padding([4, 6])
    .style(tab_bar_button_style(
        chrome,
        prefs.filter_open || !prefs.filter_query.is_empty(),
        false,
    ))
    .on_press(Message::SftpSessionToggleFilter(session_id));

    let star_btn = button(SftpGlyph::widget(
        if current_bookmarked {
            SftpGlyph::StarFilled
        } else {
            SftpGlyph::StarOutline
        },
        if current_bookmarked {
            chrome.warning
        } else {
            chrome.text_secondary
        },
        13.0,
    ))
    .padding([4, 6])
    .style(tab_bar_button_style(chrome, prefs.bookmarks_open, false))
    .on_press(Message::SftpSessionToggleBookmarks(session_id));

    let bookmark_menu = render_bookmarks_popover(
        session_id,
        current_path,
        current_bookmarked,
        bookmarks,
        chrome,
    );
    let bookmarks_anchor = anchored_menu(
        star_btn.into(),
        prefs.bookmarks_open,
        bookmark_menu,
        244.0,
        Message::SftpSessionDismissBookmarks(session_id),
    );

    let upload_btn = |glyph: SftpGlyph, label: &'static str, msg: Message| {
        let mut btn = button(
            row![
                SftpGlyph::widget(
                    glyph,
                    if has_connected_ssh {
                        chrome.text_secondary
                    } else {
                        Color {
                            a: 0.35,
                            ..chrome.text_muted
                        }
                    },
                    12.0,
                ),
                text(label)
                    .size(FONT_META)
                    .color(if has_connected_ssh {
                        chrome.text_secondary
                    } else {
                        Color {
                            a: 0.35,
                            ..chrome.text_muted
                        }
                    }),
            ]
            .spacing(3)
            .align_y(Alignment::Center),
        )
        .padding([3, 6])
        .style(tab_bar_button_style(chrome, false, false));
        if has_connected_ssh {
            btn = btn.on_press(msg);
        }
        btn
    };

    let controls_row = row![
        nav_icon(
            SftpGlyph::ChevronLeft,
            prefs.can_go_back(),
            Message::SftpSessionBack(session_id),
        ),
        nav_icon(
            SftpGlyph::ChevronRight,
            prefs.can_go_forward(),
            Message::SftpSessionForward(session_id),
        ),
        nav_icon(
            SftpGlyph::ArrowUp,
            !current_path.is_empty() && current_path != "/",
            Message::SftpSessionUp(session_id),
        ),
        nav_icon(
            SftpGlyph::Refresh,
            has_connected_ssh,
            Message::SftpSessionRefresh(session_id),
        ),
        nav_icon(
            SftpGlyph::Home,
            has_connected_ssh,
            Message::SftpSessionHome(session_id),
        ),
        upload_btn(
            SftpGlyph::UploadFile,
            "File",
            Message::SftpSessionUploadFiles(session_id),
        ),
        upload_btn(
            SftpGlyph::UploadFolder,
            "Dir",
            Message::SftpSessionUploadFolder(session_id),
        ),
        hidden_btn,
        filter_btn,
        bookmarks_anchor,
    ]
    .spacing(2)
    .align_y(Alignment::Center)
    .wrap()
    .vertical_spacing(3);

    let path_input = text_input("Remote path...", &prefs.path_input)
        .id(Id::from(format!("sftp-remote-path-{}", session_id.0)))
        .size(FONT_META)
        .font(mono_font())
        .padding([4, 8])
        .width(Length::Fill)
        .style(styled_input_style(chrome))
        .on_input(move |v| Message::SftpSessionPathInputChanged(session_id, v))
        .on_submit(Message::SftpSessionNavigateSubmitted(session_id));

    let toolbar = container(
        column![controls_row, path_input]
            .spacing(4)
            .width(Length::Fill),
    )
    .padding(Padding::from([5, 8]))
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(chrome.panel_bg.into()),
        ..container::Style::default()
    });

    let mut panel_col = column![header_bar, hairline_h(chrome), toolbar, hairline_h(chrome)]
        .width(Length::Fill)
        .height(Length::Fill);

    if prefs.filter_open {
        let filter_row = container(
            row![
                SftpGlyph::widget(SftpGlyph::Filter, chrome.text_muted, 12.0),
                text_input("Filter entries...", &prefs.filter_query)
                    .size(FONT_META)
                    .padding([3, 7])
                    .width(Length::Fill)
                    .style(styled_input_style(chrome))
                    .on_input(move |q| Message::SftpSessionFilterChanged(session_id, q)),
                button(text("×").size(13.0).color(chrome.text_muted))
                    .padding([2, 6])
                    .style(tab_bar_button_style(chrome, false, false))
                    .on_press(Message::SftpSessionToggleFilter(session_id)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding(Padding::from([4, 8]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(chrome.chrome_bg.into()),
            ..container::Style::default()
        });
        panel_col = panel_col.push(filter_row).push(hairline_h(chrome));
    }

    match state.connection_state() {
        SftpConnectionState::AutoReconnecting { .. } => {
            let label = state
                .connection_state()
                .status_label()
                .unwrap_or_else(|| "Auto-reconnecting...".to_string());
            let banner = container(
                row![
                    status_dot(chrome.warning),
                    text(label)
                        .size(FONT_META)
                        .color(chrome.warning)
                        .width(Length::Fill),
                    button(text("Retry now").size(FONT_META).color(chrome.text_primary))
                        .padding([2, 7])
                        .style(pill_button_style(chrome, PillTone::Subtle))
                        .on_press(Message::SftpSessionRefresh(session_id)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding(Padding::from([5, 8]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(chrome.warning_soft.into()),
                ..container::Style::default()
            });
            panel_col = panel_col.push(banner).push(hairline_h(chrome));
        }
        SftpConnectionState::ReconnectedSuccessfully => {
            let banner = container(
                row![
                    status_dot(chrome.success),
                    text("Reconnected successfully")
                        .size(FONT_META)
                        .font(ui_font_medium())
                        .color(chrome.success),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding(Padding::from([5, 8]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(chrome.success_soft.into()),
                ..container::Style::default()
            });
            panel_col = panel_col.push(banner).push(hairline_h(chrome));
        }
        _ => {}
    }

    if !has_connected_ssh
        && !matches!(
            state.connection_state(),
            SftpConnectionState::AutoReconnecting { .. }
        )
    {
        let banner = container(
            row![
                text("SSH session disconnected — reconnect to browse remote files.")
                    .size(FONT_META)
                    .color(chrome.text_muted)
                    .width(Length::Fill),
                button(text("Retry").size(FONT_META))
                    .padding([2, 7])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpSessionRefresh(session_id)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding(Padding::from([5, 8]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(chrome.well_bg.into()),
            ..container::Style::default()
        });
        panel_col = panel_col.push(banner).push(hairline_h(chrome));
    } else if let Some(err) = state.browser_error() {
        let err_bar = container(
            row![
                text(err.to_string())
                    .size(FONT_META)
                    .color(chrome.danger)
                    .width(Length::Fill),
                button(text("Retry").size(FONT_META).color(chrome.text_primary))
                    .padding([2, 7])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpSessionRefresh(session_id)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding(Padding::from([5, 8]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(chrome.danger_soft.into()),
            ..container::Style::default()
        });
        panel_col = panel_col.push(err_bar).push(hairline_h(chrome));
    }

    if let Some(notice) = state.transport_notice()
        && notice.mode == SftpTransportMode::MuxFallback
    {
        let copy = notice
            .message
            .clone()
            .unwrap_or_else(|| "Mux fallback on Primary Transport".to_string());
        panel_col = panel_col
            .push(
                container(
                    row![status_dot(chrome.warning), text(copy).size(11.0).color(chrome.text_muted)]
                        .spacing(6)
                        .align_y(Alignment::Center),
                )
                .padding(Padding::from([4, 8]))
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(chrome.bg_more.into()),
                    ..container::Style::default()
                }),
            )
            .push(hairline_h(chrome));
    }

    if let Some(notice) = state.last_transfer_notice() {
        panel_col = panel_col
            .push(
                container(text(notice.to_string()).size(11.0).color(chrome.warning))
                    .padding(Padding::from([4, 8]))
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(chrome.warning_soft.into()),
                        ..container::Style::default()
                    }),
            )
            .push(hairline_h(chrome));
    }

    let show_perm = panel_width >= 500.0;
    let show_modified = panel_width >= 370.0;

    let col_header = |title: &'static str, col: SftpSortColumn, width: Length| {
        let indicator = if prefs.sort_col == col {
            if prefs.sort_asc { " ▲" } else { " ▼" }
        } else {
            ""
        };
        let label = text(format!("{title}{indicator}"))
            .size(FONT_META)
            .font(ui_font_medium())
            .color(if prefs.sort_col == col {
                chrome.text_primary
            } else {
                chrome.text_secondary
            })
            .wrapping(text::Wrapping::None);
        button(container(label).width(Length::Fill).clip(true))
            .width(width)
            .padding(Padding::from([4, 5]))
            .style(list_row_button_style(chrome, false))
            .on_press(Message::SftpSessionSortBy(session_id, col))
    };

    let mut header_row = row![col_header("Name", SftpSortColumn::Name, Length::Fill)]
        .spacing(4)
        .align_y(Alignment::Center);
    header_row = header_row.push(col_header(
        "Size",
        SftpSortColumn::Size,
        Length::Fixed(72.0),
    ));
    if show_modified {
        header_row = header_row.push(col_header(
            "Modified",
            SftpSortColumn::Modified,
            Length::Fixed(132.0),
        ));
    }
    if show_perm {
        header_row = header_row.push(col_header(
            "Permission",
            SftpSortColumn::Permission,
            Length::Fixed(86.0),
        ));
    }

    let table_header = container(header_row)
        .padding(Padding::from([1, 4]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(chrome.chrome_bg.into()),
            ..container::Style::default()
        });

    panel_col = panel_col.push(table_header).push(hairline_h(chrome));

    let entries = visible_remote_entries(state);
    let total_items = entries.len();
    let selected_entries: Vec<&FileBrowserEntry> = entries
        .iter()
        .filter(|e| prefs.selected_paths.contains(&e.full_path))
        .collect();
    let selected_count = selected_entries.len();
    let selected_bytes: u64 = selected_entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.size)
        .sum();

    let rows_content: Element<'a, Message, Theme, iced::Renderer> = if entries.is_empty() {
        let empty_msg = if state.is_loading() {
            "Loading remote directory..."
        } else if !prefs.filter_query.is_empty() {
            "No matching entries"
        } else {
            "Directory is empty (drop files here to upload)"
        };
        container(text(empty_msg).size(FONT_META).color(chrome.text_muted))
            .padding(14)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        let mut list = column![].spacing(1).padding(Padding::from([2, 4]));
        for entry in &entries {
            let is_selected = prefs.selected_paths.contains(&entry.full_path);
            let is_cut = state.plus.clipboard.as_ref().is_some_and(|clip| {
                clip.op == SftpClipboardOp::Cut
                    && clip.entries.iter().any(|c| c.full_path == entry.full_path)
            });

            let icon_glyph = if entry.is_symlink {
                SftpGlyph::Link
            } else if entry.is_dir {
                SftpGlyph::Folder
            } else {
                SftpGlyph::File
            };
            let icon_color = if entry.is_dir {
                Color::from_rgb8(0xD7, 0x9A, 0x2B)
            } else if entry.is_symlink {
                chrome.accent
            } else {
                Color::from_rgb8(0x7A, 0xA2, 0xE3)
            };

            let name_color = if is_cut {
                Color {
                    a: 0.5,
                    ..chrome.text_muted
                }
            } else {
                chrome.text_primary
            };
            let meta_color = if is_cut {
                Color {
                    a: 0.45,
                    ..chrome.text_muted
                }
            } else if is_selected {
                chrome.text_primary
            } else {
                chrome.text_secondary
            };

            let name_cell = row![
                SftpGlyph::widget(icon_glyph, icon_color, 13.0),
                text(entry.name.clone())
                    .size(FONT_META)
                    .font(ui_font())
                    .color(name_color)
                    .wrapping(text::Wrapping::None),
            ]
            .spacing(6)
            .align_y(Alignment::Center);

            let size_str = if entry.is_dir {
                String::new()
            } else {
                format_sftp_size(entry.size)
            };
            let mut row_cells = row![
                container(name_cell).width(Length::Fill).clip(true),
                container(
                    text(size_str)
                        .size(FONT_META)
                        .font(ui_font())
                        .color(meta_color)
                        .wrapping(text::Wrapping::None),
                )
                .width(Length::Fixed(72.0))
                .clip(true),
            ]
            .spacing(4)
            .align_y(Alignment::Center);

            if show_modified {
                row_cells = row_cells.push(
                    container(
                        text(format_modified_timestamp(entry.modified_unix_secs))
                            .size(FONT_META)
                            .font(ui_font())
                            .color(meta_color)
                            .wrapping(text::Wrapping::None),
                    )
                    .width(Length::Fixed(132.0))
                    .clip(true),
                );
            }
            if show_perm {
                row_cells = row_cells.push(
                    container(
                        text(format_posix_mode(entry.permissions))
                            .size(FONT_META)
                            .font(mono_font())
                            .color(meta_color)
                            .wrapping(text::Wrapping::None),
                    )
                    .width(Length::Fixed(86.0))
                    .clip(true),
                );
            }

            let row_widget = SftpRowInteract::wrap(
                session_id,
                entry.full_path.clone(),
                is_selected,
                chrome,
                container(row_cells)
                    .padding(Padding::from([3, 6]))
                    .width(Length::Fill)
                    .into(),
            );
            list = list.push(row_widget);
        }
        scrollable(list)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(tabby_scrollable_style(chrome))
            .into()
    };

    let surface = SftpPaneSurface::wrap(session_id, rows_content);
    panel_col = panel_col.push(
        container(surface)
            .width(Length::Fill)
            .height(Length::Fill),
    );

    let footer_text = if selected_count > 0 {
        if selected_bytes > 0 {
            format!(
                "{total_items} items — Selected {selected_count} ({})",
                format_sftp_size(selected_bytes)
            )
        } else {
            format!("{total_items} items — Selected {selected_count}")
        }
    } else {
        format!("{total_items} items")
    };
    let mut footer_row = row![
        text(footer_text)
            .size(FONT_META)
            .color(chrome.text_secondary)
            .width(Length::Fill),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if state.is_loading() {
        footer_row = footer_row.push(
            text("Loading...")
                .size(FONT_META)
                .color(chrome.accent),
        );
    }

    let footer = container(footer_row)
        .padding(Padding::from([4, 8]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(chrome.chrome_bg.into()),
            ..container::Style::default()
        });

    panel_col = panel_col.push(hairline_h(chrome)).push(footer);

    if let Some(watched_bar) =
        render_watched_open_with_bar(session_id, &state.plus.watched_files, chrome)
    {
        panel_col = panel_col.push(hairline_h(chrome)).push(watched_bar);
    }

    if state.plus.transfers_panel_open {
        let queue_tray = container(
            scrollable(
                container(render_transfer_queue_section(state, chrome))
                    .padding(Padding::from([6, 8])),
            )
            .height(Length::Fixed(164.0))
            .style(tabby_scrollable_style(chrome)),
        )
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(chrome.panel_bg.into()),
            ..container::Style::default()
        });
        panel_col = panel_col.push(hairline_h(chrome)).push(queue_tray);
    }

    let panel_box = container(panel_col)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(chrome.term_bg.into()),
            ..container::Style::default()
        });

    SftpPanelInteract::wrap(session_id, panel_box.into())
}

fn render_watched_open_with_bar<'a>(
    session_id: SessionId,
    watched: &[WatchedRemoteFile],
    chrome: ChromePalette,
) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
    if watched.is_empty() {
        return None;
    }
    let mut col = column![].spacing(4);
    for item in watched {
        let watch_id = item.id;
        let is_uploading = item.pending_upload_id.is_some();
        let dot_color = if item.upload_failed {
            chrome.danger
        } else if item.pending_local_change {
            chrome.warning
        } else if is_uploading {
            chrome.accent
        } else {
            chrome.success
        };
        let summary = if is_uploading {
            format!("Uploading {} ({})...", item.file_name, item.app_name)
        } else if item.pending_local_change {
            if let Some(note) = &item.status_note {
                format!("{} — {note}", item.file_name)
            } else {
                format!("{} modified locally ({})", item.file_name, item.app_name)
            }
        } else if let Some(note) = &item.status_note {
            format!("{} ({}) — {note}", item.file_name, item.app_name)
        } else {
            format!("Watching {} ({})", item.file_name, item.app_name)
        };
        let summary_color = if item.upload_failed {
            chrome.danger
        } else if item.pending_local_change {
            chrome.warning
        } else {
            chrome.text_secondary
        };

        let mut row_el = row![
            status_dot(dot_color),
            text(summary)
                .size(11.0)
                .color(summary_color)
                .width(Length::Fill),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        if item.pending_local_change && !is_uploading {
            row_el = row_el.push(
                button(text("Upload").size(11.0).font(ui_font_medium()))
                    .padding([2, 7])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SftpOpenWithUploadNow {
                        session_id,
                        watch_id,
                    }),
            );
            row_el = row_el.push(
                button(text("Ignore").size(11.0))
                    .padding([2, 7])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpOpenWithIgnoreChange {
                        session_id,
                        watch_id,
                    }),
            );
        }

        let auto_label = if item.auto_upload {
            "Auto: ON"
        } else {
            "Auto"
        };
        let auto_tone = if item.auto_upload {
            PillTone::Primary
        } else {
            PillTone::Subtle
        };
        row_el = row_el.push(
            button(text(auto_label).size(11.0))
                .padding([2, 7])
                .style(pill_button_style(chrome, auto_tone))
                .on_press(Message::SftpOpenWithToggleAutoSync {
                    session_id,
                    watch_id,
                }),
        );
        row_el = row_el.push(
            button(text("×").size(12.0).color(chrome.text_muted))
                .padding([1, 5])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::SftpOpenWithStopWatch {
                    session_id,
                    watch_id,
                }),
        );

        col = col.push(row_el);
    }
    Some(
        container(col)
            .padding(Padding::from([5, 8]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(chrome.panel_bg.into()),
                ..container::Style::default()
            })
            .into(),
    )
}

fn render_bookmarks_popover<'a>(
    session_id: SessionId,
    current_path: &str,
    is_bookmarked: bool,
    bookmarks: &[String],
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let toggle_label = if is_bookmarked {
        "Remove Current Path Bookmark"
    } else {
        "Bookmark Current Path"
    };
    let mut col = column![].spacing(3).padding(6);
    let mut toggle_btn = button(
        text(toggle_label)
            .size(FONT_META)
            .font(ui_font_medium())
            .color(chrome.accent),
    )
    .width(Length::Fill)
    .padding([5, 8])
    .style(list_row_button_style(chrome, false));
    if !current_path.is_empty() {
        toggle_btn = toggle_btn.on_press(Message::SftpSessionToggleBookmarkCurrent(session_id));
    }
    col = col.push(toggle_btn).push(hairline_h(chrome));

    if bookmarks.is_empty() {
        col = col.push(
            container(
                text("No saved remote bookmarks")
                    .size(FONT_META)
                    .color(chrome.text_muted),
            )
            .padding([6, 8]),
        );
    } else {
        let mut list = column![].spacing(2);
        for bm in bookmarks {
            let path_for_jump = bm.clone();
            let path_for_del = bm.clone();
            let active = bm == current_path;
            let row_el = row![
                button(
                    text(bm.clone())
                        .size(FONT_META)
                        .font(mono_font())
                        .color(if active {
                            chrome.accent
                        } else {
                            chrome.text_primary
                        })
                        .wrapping(text::Wrapping::None),
                )
                .width(Length::Fill)
                .padding([4, 6])
                .style(list_row_button_style(chrome, active))
                .on_press(Message::SftpSessionSelectBookmark {
                    session_id,
                    path: path_for_jump,
                }),
                button(text("×").size(13.0).color(chrome.text_muted))
                    .padding([3, 6])
                    .style(tab_bar_button_style(chrome, false, false))
                    .on_press(Message::SftpSessionRemoveBookmark {
                        session_id,
                        path: path_for_del,
                    }),
            ]
            .spacing(2)
            .align_y(Alignment::Center);
            list = list.push(row_el);
        }
        col = col.push(
            container(scrollable(list).style(tabby_scrollable_style(chrome))).max_height(220.0),
        );
    }

    container(col)
        .width(Length::Fixed(244.0))
        .style(move |_| modal_card_style(chrome, false))
        .into()
}

/// Renders the floating SFTP Panel right-click context menu over a click-dismiss backdrop.
pub fn render_sftp_context_menu_overlay<'a>(
    state: &'a SftpDrawerState,
    has_connected_ssh: bool,
    window_width: f32,
    window_height: f32,
    palette: &ThemePalette,
) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
    if !state.plus.context_menu.open {
        return None;
    }
    let chrome = ChromePalette::from_palette(palette);
    let visible = visible_remote_entries(state);
    let selected_entries: Vec<&FileBrowserEntry> = visible
        .iter()
        .filter(|e| state.plus.remote.selected_paths.contains(&e.full_path))
        .collect();
    let sel_count = selected_entries.len();
    let single_selected = sel_count == 1;
    let single_file = sel_count == 1 && !selected_entries[0].is_dir;
    let has_selection = sel_count > 0;
    let can_paste = state.plus.clipboard.is_some() && has_connected_ssh;

    let mut items = column![].spacing(2);
    let add_item = |col: iced::widget::Column<'a, Message, Theme, iced::Renderer>,
                    label: &'a str,
                    shortcut: Option<&'a str>,
                    enabled: bool,
                    action: SftpContextActionKind| {
        let fg = if enabled {
            chrome.text_primary
        } else {
            Color {
                a: 0.42,
                ..chrome.text_muted
            }
        };
        let mut r = row![
            text(label)
                .size(FONT_BODY)
                .font(ui_font())
                .color(fg),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(sc) = shortcut {
            r = r.push(space::horizontal()).push(
                text(sc)
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_muted),
            );
        }
        let mut btn = button(r)
            .width(Length::Fill)
            .padding([5, 10])
            .style(list_row_button_style(chrome, false));
        if enabled {
            btn = btn.on_press(Message::SftpContextAction(action));
        }
        col.push(btn)
    };

    items = add_item(
        items,
        "Download",
        None,
        has_selection && has_connected_ssh,
        SftpContextActionKind::Download,
    );
    items = add_item(
        items,
        "Download to...",
        None,
        has_selection && has_connected_ssh,
        SftpContextActionKind::DownloadTo,
    );
    items = add_item(
        items,
        "Upload Files...",
        None,
        has_connected_ssh,
        SftpContextActionKind::UploadFiles,
    );
    items = add_item(
        items,
        "Upload Folder...",
        None,
        has_connected_ssh,
        SftpContextActionKind::UploadFolder,
    );
    items = items.push(hairline_h(chrome));
    items = add_item(
        items,
        "View",
        None,
        single_file && has_connected_ssh,
        SftpContextActionKind::View,
    );
    items = add_item(
        items,
        "Edit",
        None,
        single_file && has_connected_ssh,
        SftpContextActionKind::Edit,
    );
    items = add_item(
        items,
        "Open with...",
        None,
        single_selected && has_connected_ssh,
        SftpContextActionKind::OpenWith,
    );
    items = items.push(hairline_h(chrome));
    items = add_item(
        items,
        "Copy",
        Some("Ctrl+C"),
        has_selection,
        SftpContextActionKind::Copy,
    );
    items = add_item(
        items,
        "Cut",
        Some("Ctrl+X"),
        has_selection,
        SftpContextActionKind::Cut,
    );
    if can_paste {
        items = add_item(
            items,
            "Paste",
            Some("Ctrl+V"),
            true,
            SftpContextActionKind::Paste,
        );
    }
    items = items.push(hairline_h(chrome));
    items = add_item(
        items,
        "Rename",
        Some("F2"),
        sel_count == 1 && has_connected_ssh,
        SftpContextActionKind::Rename,
    );
    items = add_item(
        items,
        "Delete",
        Some("Delete"),
        has_selection && has_connected_ssh,
        SftpContextActionKind::Delete,
    );
    items = items.push(hairline_h(chrome));
    items = add_item(
        items,
        "Edit Permissions",
        None,
        has_selection && has_connected_ssh,
        SftpContextActionKind::EditPermissions,
    );
    items = add_item(
        items,
        "Properties",
        None,
        sel_count == 1,
        SftpContextActionKind::Properties,
    );
    items = items.push(hairline_h(chrome));
    items = add_item(
        items,
        "New Folder",
        None,
        has_connected_ssh,
        SftpContextActionKind::NewFolder,
    );
    items = add_item(
        items,
        "New File",
        None,
        has_connected_ssh,
        SftpContextActionKind::NewFile,
    );
    items = items.push(hairline_h(chrome));
    items = add_item(
        items,
        "Refresh",
        Some("F5"),
        has_connected_ssh,
        SftpContextActionKind::Refresh,
    );
    items = add_item(
        items,
        "Select All",
        Some("Ctrl+A"),
        !visible.is_empty(),
        SftpContextActionKind::SelectAll,
    );
    items = add_item(
        items,
        "Invert Selection",
        None,
        !visible.is_empty(),
        SftpContextActionKind::InvertSelection,
    );
    items = add_item(
        items,
        "Copy Path",
        None,
        true,
        SftpContextActionKind::CopyPath,
    );

    let menu_w = 228.0_f32;
    let approx_h = if can_paste { 578.0_f32 } else { 550.0_f32 };
    let pos = state.plus.context_menu.position;
    let left = pos.x.clamp(8.0, (window_width - menu_w - 10.0).max(8.0));
    let top = pos.y.clamp(8.0, (window_height - approx_h - 10.0).max(8.0));

    let card = container(items)
        .width(Length::Fixed(menu_w))
        .padding(6)
        .style(move |_| modal_card_style(chrome, false));

    let scrim = mouse_area(
        container(space::horizontal().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::SftpDismissContextMenu)
    .on_right_press(Message::SftpDismissContextMenu);

    let positioned = container(opaque(card))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top,
            left,
            right: 8.0,
            bottom: 8.0,
        });

    Some(opaque(stack![scrim, positioned]).into())
}

/// Renders active SFTP Panel dialog modals (New Folder/File, Rename, Edit Permissions, Properties, View/Edit).
pub fn render_sftp_plus_modal<'a>(
    state: &'a SftpDrawerState,
    window_width: f32,
    window_height: f32,
    palette: &ThemePalette,
) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
    let chrome = ChromePalette::from_palette(palette);
    match &state.plus.modal {
        SftpModalState::None => None,
        SftpModalState::CreateEntry {
            parent_dir,
            is_dir,
            name,
            error,
        } => {
            let title = if *is_dir { "New Folder" } else { "New File" };
            let placeholder = if *is_dir {
                "Folder name..."
            } else {
                "File name..."
            };
            let mut body = column![
                text(format!("Remote path: {parent_dir}"))
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_secondary),
                text_input(placeholder, name)
                    .id(Id::new(SFTP_MODAL_INPUT_ID))
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .on_input(Message::SftpModalNameChanged)
                    .on_submit(Message::SftpModalConfirmCreateOrRename),
            ]
            .spacing(10);
            if let Some(err) = error {
                body = body.push(text(err.clone()).size(FONT_META).color(chrome.danger));
            }
            let footer = row![
                space::horizontal(),
                button(text("Cancel").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpDismissModal),
                button(text("Create").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 16])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SftpModalConfirmCreateOrRename),
            ]
            .spacing(8);

            let card = build_modal_shell(
                title,
                body.into(),
                footer.into(),
                modal_width(440.0, window_width),
                chrome,
            );
            Some(modal_overlay(card, Some(Message::SftpDismissModal), 90.0))
        }
        SftpModalState::Rename {
            old_path,
            new_name,
            error,
        } => {
            let mut body = column![
                text(format!("Remote item: {old_path}"))
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_secondary),
                text_input("New name...", new_name)
                    .id(Id::new(SFTP_MODAL_INPUT_ID))
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .on_input(Message::SftpModalNameChanged)
                    .on_submit(Message::SftpModalConfirmCreateOrRename),
            ]
            .spacing(10);
            if let Some(err) = error {
                body = body.push(text(err.clone()).size(FONT_META).color(chrome.danger));
            }
            let footer = row![
                space::horizontal(),
                button(text("Cancel").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpDismissModal),
                button(text("Rename").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 16])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SftpModalConfirmCreateOrRename),
            ]
            .spacing(8);

            let card = build_modal_shell(
                "Rename",
                body.into(),
                footer.into(),
                modal_width(440.0, window_width),
                chrome,
            );
            Some(modal_overlay(card, Some(Message::SftpDismissModal), 90.0))
        }
        SftpModalState::EditPermissions {
            target_paths,
            octal_input,
            mode_bits,
            error,
        } => {
            let target_summary = if target_paths.len() == 1 {
                target_paths[0].clone()
            } else {
                format!("{} items selected", target_paths.len())
            };

            let bit_row = |group_label: &'static str, r_bit: u32, w_bit: u32, x_bit: u32| {
                row![
                    container(
                        text(group_label)
                            .size(FONT_BODY)
                            .font(ui_font_medium())
                            .color(chrome.text_primary),
                    )
                    .width(Length::Fixed(74.0)),
                    apple_checkbox(
                        (mode_bits & r_bit) != 0,
                        "Read",
                        move |on| Message::SftpModalPermBitToggled {
                            bit: r_bit,
                            enabled: on,
                        },
                        chrome,
                    ),
                    apple_checkbox(
                        (mode_bits & w_bit) != 0,
                        "Write",
                        move |on| Message::SftpModalPermBitToggled {
                            bit: w_bit,
                            enabled: on,
                        },
                        chrome,
                    ),
                    apple_checkbox(
                        (mode_bits & x_bit) != 0,
                        "Execute",
                        move |on| Message::SftpModalPermBitToggled {
                            bit: x_bit,
                            enabled: on,
                        },
                        chrome,
                    ),
                ]
                .spacing(16)
                .align_y(Alignment::Center)
            };

            let mut body = column![
                text(format!("Remote target: {target_summary}"))
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_secondary),
                container(
                    column![
                        bit_row("Owner", 0o400, 0o200, 0o100),
                        bit_row("Group", 0o040, 0o020, 0o010),
                        bit_row("Other", 0o004, 0o002, 0o001),
                    ]
                    .spacing(10),
                )
                .padding(12)
                .width(Length::Fill)
                .style(move |_| inset_well_style(chrome)),
                row![
                    text("Octal Mode:")
                        .size(FONT_BODY)
                        .font(ui_font_medium())
                        .color(chrome.text_primary),
                    text_input("755", octal_input)
                        .width(Length::Fixed(86.0))
                        .size(FONT_BODY)
                        .font(mono_font())
                        .padding([6, 10])
                        .style(styled_input_style(chrome))
                        .on_input(Message::SftpModalOctalChanged)
                        .on_submit(Message::SftpModalConfirmPermissions),
                    text(format_posix_mode(Some(*mode_bits)))
                        .size(FONT_BODY)
                        .font(mono_font())
                        .color(chrome.accent),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            ]
            .spacing(12);
            if let Some(err) = error {
                body = body.push(text(err.clone()).size(FONT_META).color(chrome.danger));
            }

            let footer = row![
                space::horizontal(),
                button(text("Cancel").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpDismissModal),
                button(text("Apply Permissions").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 16])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SftpModalConfirmPermissions),
            ]
            .spacing(8);

            let card = build_modal_shell(
                "Edit Permissions",
                body.into(),
                footer.into(),
                modal_width(460.0, window_width),
                chrome,
            );
            Some(modal_overlay(card, Some(Message::SftpDismissModal), 80.0))
        }
        SftpModalState::Properties {
            entry,
            extra_selected,
            total_selected_bytes,
        } => {
            let kind_str = if entry.is_symlink {
                "Symbolic Link"
            } else if entry.is_dir {
                "Directory"
            } else {
                "Regular File"
            };
            let size_line = if *extra_selected > 0 {
                format!(
                    "{} ({} items selected, total {})",
                    format_bytes(entry.size),
                    extra_selected + 1,
                    format_sftp_size(*total_selected_bytes)
                )
            } else if entry.is_dir {
                "—".to_string()
            } else {
                format!("{} ({} bytes)", format_sftp_size(entry.size), entry.size)
            };
            let perm_str = match entry.permissions {
                Some(bits) => {
                    format!("{} ({})", format_posix_mode(Some(bits)), format_octal_mode(bits))
                }
                None => "—".to_string(),
            };

            let prop_row = |k: &'static str, v: String| {
                row![
                    container(
                        text(k)
                            .size(FONT_META)
                            .font(ui_font_medium())
                            .color(chrome.text_secondary),
                    )
                    .width(Length::Fixed(104.0)),
                    text(v)
                        .size(FONT_META)
                        .font(mono_font())
                        .color(chrome.text_primary),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            };

            let mut props = column![
                prop_row("Name:", entry.name.clone()),
                prop_row("Path:", entry.full_path.clone()),
                prop_row("Kind:", kind_str.to_string()),
                prop_row("Size:", size_line),
                prop_row("Modified:", format_modified_timestamp(entry.modified_unix_secs)),
                prop_row("Permissions:", perm_str),
            ]
            .spacing(8);

            if let Some(target) = &entry.symlink_target {
                props = props.push(prop_row("Link Target:", target.clone()));
            }

            let well = container(props)
                .padding(12)
                .width(Length::Fill)
                .style(move |_| inset_well_style(chrome));

            let footer = row![
                space::horizontal(),
                button(text("Close").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 16])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SftpDismissModal),
            ];

            let card = build_modal_shell(
                "Properties",
                well.into(),
                footer.into(),
                modal_width(480.0, window_width),
                chrome,
            );
            Some(modal_overlay(card, Some(Message::SftpDismissModal), 80.0))
        }
        SftpModalState::FileViewer {
            path,
            file_name,
            editable,
            loading,
            saving,
            dirty,
            error,
            content,
        } => {
            let mode_badge = if *editable { "EDIT" } else { "VIEW" };
            let title = if *dirty && *editable {
                format!("{file_name} • ({mode_badge})")
            } else {
                format!("{file_name} ({mode_badge})")
            };
            let editor_h = modal_body_height(460.0, window_height, 220.0);

            let mut body = column![
                row![
                    text(format!("Remote · {path}"))
                        .size(FONT_META)
                        .font(mono_font())
                        .color(chrome.text_secondary)
                        .width(Length::Fill),
                    badge_pill(
                        mode_badge,
                        if *editable {
                            chrome.accent_fg
                        } else {
                            chrome.text_secondary
                        },
                        if *editable {
                            chrome.accent
                        } else {
                            chrome.well_bg
                        },
                        chrome.border_subtle,
                    ),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ]
            .spacing(8);

            if *loading {
                body = body.push(
                    container(
                        text("Loading remote file contents...")
                            .size(FONT_BODY)
                            .color(chrome.text_muted),
                    )
                    .height(Length::Fixed(editor_h))
                    .width(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .style(move |_| inset_well_style(chrome)),
                );
            } else {
                let editor = text_editor(content)
                    .height(Length::Fixed(editor_h))
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding(10)
                    .style(move |_, _| text_editor::Style {
                        background: chrome.well_bg.into(),
                        border: Border {
                            color: chrome.border_subtle,
                            width: 1.0,
                            radius: 6.0.into(),
                        },
                        placeholder: chrome.text_muted,
                        value: chrome.text_primary,
                        selection: chrome.accent_soft,
                    })
                    .on_action(Message::SftpModalEditorAction);
                body = body.push(editor);
            }

            if let Some(err) = error {
                body = body.push(text(err.clone()).size(FONT_META).color(chrome.danger));
            }

            let mut footer = row![space::horizontal()].spacing(8);
            footer = footer.push(
                button(text("Close").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpDismissModal),
            );
            if *editable {
                let save_label = if *saving { "Saving..." } else { "Save" };
                let mut save_btn = button(text(save_label).size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 16])
                    .style(pill_button_style(chrome, PillTone::Primary));
                if !*loading && !*saving {
                    save_btn = save_btn.on_press(Message::SftpModalSaveFile);
                }
                footer = footer.push(save_btn);
            }

            let card = build_modal_shell(
                &title,
                body.into(),
                footer.into(),
                modal_width(760.0, window_width),
                chrome,
            );
            Some(modal_overlay(card, Some(Message::SftpDismissModal), 44.0))
        }
        SftpModalState::OpenWith {
            entry,
            loading,
            search_query,
            apps,
            selected_launch_path,
            error,
            ..
        } => {
            let kind_badge = if entry.is_dir { "FOLDER" } else { "FILE" };
            let title = format!("Open \"{}\" with...", entry.name);
            let list_h = modal_body_height(320.0, window_height, 270.0);
            let filtered = filter_installed_apps(apps, search_query);
            let has_valid_selection = selected_launch_path
                .as_ref()
                .is_some_and(|sel| filtered.iter().any(|app| &app.launch_path == sel));

            let mut body = column![
                row![
                    badge_pill(
                        kind_badge,
                        chrome.text_secondary,
                        chrome.well_bg,
                        chrome.border_subtle,
                    ),
                    text(entry.full_path.clone())
                        .size(FONT_META)
                        .font(mono_font())
                        .color(chrome.text_secondary)
                        .width(Length::Fill),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                text_input("Search installed applications...", search_query)
                    .id(Id::new(SFTP_MODAL_INPUT_ID))
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .on_input(Message::SftpOpenWithSearchChanged)
                    .on_submit(Message::SftpOpenWithConfirm),
            ]
            .spacing(10);

            if *loading {
                body = body.push(
                    container(
                        text("Scanning installed applications...")
                            .size(FONT_BODY)
                            .color(chrome.text_muted),
                    )
                    .height(Length::Fixed(list_h))
                    .width(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .style(move |_| inset_well_style(chrome)),
                );
            } else if filtered.is_empty() {
                let empty_hint = if apps.is_empty() {
                    "No desktop applications discovered. Click \"Browse App...\" to select an executable."
                } else {
                    "No applications match your filter. Clear the search or click \"Browse App...\"."
                };
                body = body.push(
                    container(
                        column![
                            text("No applications found")
                                .size(FONT_BODY)
                                .font(ui_font_medium())
                                .color(chrome.text_primary),
                            text(empty_hint)
                                .size(FONT_META)
                                .color(chrome.text_muted),
                        ]
                        .spacing(4)
                        .align_x(Alignment::Center),
                    )
                    .height(Length::Fixed(list_h))
                    .width(Length::Fill)
                    .padding(16)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .style(move |_| inset_well_style(chrome)),
                );
            } else {
                let recommended: Vec<&InstalledApp> = filtered
                    .iter()
                    .copied()
                    .filter(|app| app.is_default || app.is_recommended)
                    .collect();
                let others: Vec<&InstalledApp> = filtered
                    .iter()
                    .copied()
                    .filter(|app| !app.is_default && !app.is_recommended)
                    .collect();

                let render_app_row = |app: &InstalledApp| -> Element<'a, Message, Theme, iced::Renderer> {
                    let is_selected = selected_launch_path
                        .as_ref()
                        .is_some_and(|sel| sel == &app.launch_path);

                    let mut title_col = column![
                        text(app.name.clone())
                            .size(FONT_BODY)
                            .font(if is_selected {
                                ui_font_semibold()
                            } else {
                                ui_font_medium()
                            })
                            .color(if is_selected {
                                chrome.accent
                            } else {
                                chrome.text_primary
                            }),
                    ]
                    .spacing(1)
                    .width(Length::Fill);

                    if let Some(sub) = app.subtitle.as_ref() {
                        title_col = title_col.push(
                            text(sub.clone())
                                .size(FONT_META)
                                .color(chrome.text_secondary),
                        );
                    }

                    let mut meta_row = row![].spacing(6).align_y(Alignment::Center);
                    if app.is_default {
                        meta_row = meta_row.push(badge_pill(
                            "Default",
                            chrome.accent_fg,
                            chrome.accent,
                            chrome.border_subtle,
                        ));
                    } else if app.is_recommended {
                        meta_row = meta_row.push(badge_pill(
                            "Recommended",
                            chrome.accent,
                            chrome.accent_soft,
                            chrome.border_subtle,
                        ));
                    }
                    if app.terminal {
                        meta_row = meta_row.push(badge_pill(
                            "Terminal",
                            chrome.text_secondary,
                            chrome.well_bg,
                            chrome.border_subtle,
                        ));
                    }
                    meta_row = meta_row.push(
                        text(app.command_label.clone())
                            .size(FONT_META)
                            .font(mono_font())
                            .color(chrome.text_muted),
                    );

                    button(
                        row![title_col, meta_row]
                            .spacing(10)
                            .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([7, 10])
                    .style(list_row_button_style(chrome, is_selected))
                    .on_press(Message::SftpOpenWithSelectApp(app.launch_path.clone()))
                    .into()
                };

                let mut app_list = column![].spacing(4).padding([4, 4]);
                if !recommended.is_empty() {
                    app_list = app_list.push(
                        container(
                            text("Recommended")
                                .size(FONT_META)
                                .font(ui_font_semibold())
                                .color(chrome.text_secondary),
                        )
                        .padding([4, 6]),
                    );
                    for app in &recommended {
                        app_list = app_list.push(render_app_row(app));
                    }
                }
                if !others.is_empty() {
                    let other_label = if recommended.is_empty() {
                        "Installed Applications"
                    } else {
                        "Other Applications"
                    };
                    app_list = app_list.push(
                        container(
                            text(other_label)
                                .size(FONT_META)
                                .font(ui_font_semibold())
                                .color(chrome.text_secondary),
                        )
                        .padding([6, 6]),
                    );
                    for app in &others {
                        app_list = app_list.push(render_app_row(app));
                    }
                }

                body = body.push(
                    container(scrollable(app_list).style(tabby_scrollable_style(chrome)))
                        .height(Length::Fixed(list_h))
                        .width(Length::Fill)
                        .padding(4)
                        .style(move |_| inset_well_style(chrome)),
                );
            }

            if let Some(err) = error {
                body = body.push(text(err.clone()).size(FONT_META).color(chrome.danger));
            }

            let mut open_btn = button(text("Open").size(FONT_BODY).font(ui_font_medium()))
                .padding([6, 16])
                .style(pill_button_style(chrome, PillTone::Primary));
            if has_valid_selection {
                open_btn = open_btn.on_press(Message::SftpOpenWithConfirm);
            }

            let footer = row![
                button(text("Browse App...").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 12])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpOpenWithBrowseCustom),
                space::horizontal(),
                button(text("Cancel").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SftpDismissModal),
                open_btn,
            ]
            .spacing(8)
            .align_y(Alignment::Center);

            let card = build_modal_shell(
                &title,
                body.into(),
                footer.into(),
                modal_width(540.0, window_width),
                chrome,
            );
            Some(modal_overlay(card, Some(Message::SftpDismissModal), 64.0))
        }
    }
}

fn build_modal_shell<'a>(
    title: &str,
    body: Element<'a, Message, Theme, iced::Renderer>,
    footer: Element<'a, Message, Theme, iced::Renderer>,
    width: f32,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let header = row![
        text(title.to_string())
            .size(FONT_SUBHEAD)
            .font(ui_font_semibold())
            .color(chrome.text_primary)
            .width(Length::Fill),
        button(text("×").size(15.0).color(chrome.text_secondary))
            .padding([2, 7])
            .style(tab_bar_button_style(chrome, false, false))
            .on_press(Message::SftpDismissModal),
    ]
    .align_y(Alignment::Center);

    container(
        column![header, hairline_h(chrome), body, footer]
            .spacing(12)
            .padding(16),
    )
    .width(Length::Fixed(width))
    .style(move |_| modal_card_style(chrome, false))
    .into()
}

/// Crisp geometric pixel/vector icon widget drawn with Iced quads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpGlyph {
    Globe,
    Transfers,
    UploadFile,
    UploadFolder,
    ChevronLeft,
    ChevronRight,
    ArrowUp,
    Refresh,
    Home,
    Eye,
    Filter,
    StarOutline,
    StarFilled,
    Folder,
    File,
    Link,
}

impl SftpGlyph {
    pub fn widget<'a>(
        glyph: Self,
        color: Color,
        size: f32,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(SftpGlyphWidget { glyph, color, size })
    }
}

struct SftpGlyphWidget {
    glyph: SftpGlyph,
    color: Color,
    size: f32,
}

impl Widget<Message, Theme, iced::Renderer> for SftpGlyphWidget {
    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(
            Length::Fixed(self.size),
            Length::Fixed(self.size),
            Size::ZERO,
        ))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let b = layout.bounds();
        let s = b.width.min(b.height);
        let x = b.x + (b.width - s) * 0.5;
        let y = b.y + (b.height - s) * 0.5;
        let c = self.color;

        let rect = |rx: f32, ry: f32, rw: f32, rh: f32| {
            Rectangle::new(
                Point::new(x + rx * s, y + ry * s),
                Size::new((rw * s).max(1.0), (rh * s).max(1.0)),
            )
        };
        let fill = |renderer: &mut iced::Renderer, bounds: Rectangle, color: Color| {
            use iced::advanced::renderer::Renderer as _;
            renderer.fill_quad(
                Quad {
                    bounds,
                    ..Quad::default()
                },
                Background::Color(color),
            );
        };
        let stroke = |renderer: &mut iced::Renderer, bounds: Rectangle, color: Color, t: f32| {
            use iced::advanced::renderer::Renderer as _;
            renderer.fill_quad(
                Quad {
                    bounds,
                    border: Border {
                        color,
                        width: t,
                        radius: 1.5.into(),
                    },
                    ..Quad::default()
                },
                Background::Color(Color::TRANSPARENT),
            );
        };

        match self.glyph {
            SftpGlyph::Globe => {
                use iced::advanced::renderer::Renderer as _;
                renderer.fill_quad(
                    Quad {
                        bounds: rect(0.10, 0.10, 0.80, 0.80),
                        border: Border {
                            color: c,
                            width: 1.3,
                            radius: (s * 0.45).into(),
                        },
                        ..Quad::default()
                    },
                    Background::Color(Color::TRANSPARENT),
                );
                fill(renderer, rect(0.14, 0.46, 0.72, 0.08), c);
                fill(renderer, rect(0.46, 0.14, 0.08, 0.72), c);
            }
            SftpGlyph::Transfers => {
                fill(renderer, rect(0.26, 0.16, 0.10, 0.60), c);
                fill(renderer, rect(0.16, 0.24, 0.30, 0.09), c);
                fill(renderer, rect(0.64, 0.24, 0.10, 0.60), c);
                fill(renderer, rect(0.54, 0.67, 0.30, 0.09), c);
            }
            SftpGlyph::UploadFile => {
                fill(renderer, rect(0.45, 0.16, 0.10, 0.48), c);
                fill(renderer, rect(0.32, 0.26, 0.36, 0.09), c);
                fill(renderer, rect(0.16, 0.74, 0.68, 0.10), c);
                fill(renderer, rect(0.16, 0.60, 0.10, 0.16), c);
                fill(renderer, rect(0.74, 0.60, 0.10, 0.16), c);
            }
            SftpGlyph::UploadFolder => {
                fill(renderer, rect(0.10, 0.20, 0.34, 0.12), c);
                stroke(renderer, rect(0.10, 0.30, 0.80, 0.54), c, 1.2);
                fill(renderer, rect(0.46, 0.40, 0.09, 0.34), c);
                fill(renderer, rect(0.35, 0.48, 0.30, 0.09), c);
            }
            SftpGlyph::ChevronLeft => {
                fill(renderer, rect(0.54, 0.22, 0.11, 0.12), c);
                fill(renderer, rect(0.44, 0.32, 0.11, 0.12), c);
                fill(renderer, rect(0.34, 0.44, 0.12, 0.12), c);
                fill(renderer, rect(0.44, 0.56, 0.11, 0.12), c);
                fill(renderer, rect(0.54, 0.66, 0.11, 0.12), c);
            }
            SftpGlyph::ChevronRight => {
                fill(renderer, rect(0.35, 0.22, 0.11, 0.12), c);
                fill(renderer, rect(0.45, 0.32, 0.11, 0.12), c);
                fill(renderer, rect(0.54, 0.44, 0.12, 0.12), c);
                fill(renderer, rect(0.45, 0.56, 0.11, 0.12), c);
                fill(renderer, rect(0.35, 0.66, 0.11, 0.12), c);
            }
            SftpGlyph::ArrowUp => {
                fill(renderer, rect(0.45, 0.22, 0.10, 0.58), c);
                fill(renderer, rect(0.36, 0.28, 0.10, 0.10), c);
                fill(renderer, rect(0.54, 0.28, 0.10, 0.10), c);
                fill(renderer, rect(0.27, 0.37, 0.10, 0.10), c);
                fill(renderer, rect(0.63, 0.37, 0.10, 0.10), c);
            }
            SftpGlyph::Refresh => {
                fill(renderer, rect(0.24, 0.20, 0.52, 0.09), c);
                fill(renderer, rect(0.18, 0.28, 0.09, 0.44), c);
                fill(renderer, rect(0.24, 0.71, 0.52, 0.09), c);
                fill(renderer, rect(0.73, 0.46, 0.09, 0.26), c);
                fill(renderer, rect(0.65, 0.14, 0.15, 0.18), c);
            }
            SftpGlyph::Home => {
                fill(renderer, rect(0.44, 0.14, 0.12, 0.10), c);
                fill(renderer, rect(0.30, 0.24, 0.40, 0.10), c);
                fill(renderer, rect(0.16, 0.34, 0.68, 0.10), c);
                fill(renderer, rect(0.24, 0.44, 0.52, 0.38), c);
                fill(
                    renderer,
                    rect(0.43, 0.56, 0.14, 0.26),
                    Color::from_rgba(0.0, 0.0, 0.0, 0.45),
                );
            }
            SftpGlyph::Eye => {
                use iced::advanced::renderer::Renderer as _;
                renderer.fill_quad(
                    Quad {
                        bounds: rect(0.10, 0.26, 0.80, 0.48),
                        border: Border {
                            color: c,
                            width: 1.3,
                            radius: (s * 0.24).into(),
                        },
                        ..Quad::default()
                    },
                    Background::Color(Color::TRANSPARENT),
                );
                renderer.fill_quad(
                    Quad {
                        bounds: rect(0.38, 0.38, 0.24, 0.24),
                        border: Border {
                            radius: (s * 0.12).into(),
                            ..Border::default()
                        },
                        ..Quad::default()
                    },
                    Background::Color(c),
                );
            }
            SftpGlyph::Filter => {
                fill(renderer, rect(0.14, 0.20, 0.72, 0.11), c);
                fill(renderer, rect(0.26, 0.34, 0.48, 0.11), c);
                fill(renderer, rect(0.38, 0.48, 0.24, 0.12), c);
                fill(renderer, rect(0.44, 0.60, 0.12, 0.22), c);
            }
            SftpGlyph::StarOutline => {
                stroke(renderer, rect(0.22, 0.22, 0.56, 0.56), c, 1.2);
                fill(renderer, rect(0.45, 0.10, 0.10, 0.80), c);
                fill(renderer, rect(0.10, 0.45, 0.80, 0.10), c);
            }
            SftpGlyph::StarFilled => {
                fill(renderer, rect(0.42, 0.10, 0.16, 0.80), c);
                fill(renderer, rect(0.10, 0.42, 0.80, 0.16), c);
                fill(renderer, rect(0.24, 0.24, 0.52, 0.52), c);
            }
            SftpGlyph::Folder => {
                fill(renderer, rect(0.10, 0.20, 0.36, 0.12), c);
                fill(renderer, rect(0.10, 0.30, 0.80, 0.52), c);
            }
            SftpGlyph::File => {
                stroke(renderer, rect(0.18, 0.12, 0.64, 0.76), c, 1.3);
                fill(renderer, rect(0.30, 0.36, 0.40, 0.08), c);
                fill(renderer, rect(0.30, 0.52, 0.40, 0.08), c);
                fill(renderer, rect(0.30, 0.68, 0.26, 0.08), c);
            }
            SftpGlyph::Link => {
                stroke(renderer, rect(0.16, 0.16, 0.64, 0.68), c, 1.2);
                fill(renderer, rect(0.44, 0.42, 0.36, 0.10), c);
                fill(renderer, rect(0.68, 0.24, 0.12, 0.28), c);
            }
        }
    }
}

struct SftpPanelInteract<'a> {
    session_id: SessionId,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> SftpPanelInteract<'a> {
    fn wrap(
        session_id: SessionId,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            session_id,
            content,
        })
    }
}

impl Widget<Message, Theme, iced::Renderer> for SftpPanelInteract<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Left | mouse::Button::Right | mouse::Button::Middle
            ))
        ) && cursor.is_over(layout.bounds())
        {
            shell.publish(Message::FocusSftpPanel(self.session_id));
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }
}

#[derive(Debug, Default)]
struct RowInteractState {
    modifiers: keyboard::Modifiers,
}

struct SftpRowInteract<'a> {
    session_id: SessionId,
    path: String,
    selected: bool,
    chrome: ChromePalette,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> SftpRowInteract<'a> {
    fn wrap(
        session_id: SessionId,
        path: String,
        selected: bool,
        chrome: ChromePalette,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            session_id,
            path,
            selected,
            chrome,
            content,
        })
    }
}

impl Widget<Message, Theme, iced::Renderer> for SftpRowInteract<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<RowInteractState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(RowInteractState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<RowInteractState>();
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(m)) = event {
            state.modifiers = *m;
        }
        if cursor.is_over(layout.bounds()) {
            match event {
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                    shell.publish(Message::SftpSessionRowClicked {
                        session_id: self.session_id,
                        path: self.path.clone(),
                        ctrl_held: state.modifiers.control() || state.modifiers.logo(),
                    });
                    shell.capture_event();
                    return;
                }
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                    if let Some(pos) = cursor.position() {
                        shell.publish(Message::SftpSessionOpenContextMenu {
                            session_id: self.session_id,
                            target_path: Some(self.path.clone()),
                            position: pos,
                        });
                        shell.capture_event();
                        return;
                    }
                }
                _ => {}
            }
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let hovered = cursor.is_over(bounds);
        if self.selected || hovered {
            use iced::advanced::renderer::Renderer as _;
            let bg = if self.selected {
                if self.chrome.is_light {
                    self.chrome.accent_soft
                } else {
                    Color {
                        r: 0.45,
                        g: 0.49,
                        b: 0.56,
                        a: 0.50,
                    }
                }
            } else {
                Color {
                    a: 0.06,
                    ..self.chrome.text_primary
                }
            };
            renderer.fill_quad(
                Quad {
                    bounds,
                    border: Border {
                        radius: 4.0.into(),
                        ..Border::default()
                    },
                    ..Quad::default()
                },
                Background::Color(bg),
            );
        }
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
}

struct SftpPaneSurface<'a> {
    session_id: SessionId,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> SftpPaneSurface<'a> {
    fn wrap(
        session_id: SessionId,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            session_id,
            content,
        })
    }
}

impl Widget<Message, Theme, iced::Renderer> for SftpPaneSurface<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if !shell.is_event_captured() && cursor.is_over(layout.bounds()) {
            if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event
                && let Some(pos) = cursor.position()
            {
                shell.publish(Message::SftpSessionOpenContextMenu {
                    session_id: self.session_id,
                    target_path: None,
                    position: pos,
                });
                shell.capture_event();
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }
}
