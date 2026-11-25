use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use iced::futures::{SinkExt, Stream};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::{container, operation, stack};
use iced::{Element, Event, Length, Size, Subscription, Task, Theme, clipboard, event, window};
use termewnal_core::{
    AppConfig, ConfigPaths, ProfilesConfig, TerminalConfig, ThemePalette, ensure_config_files,
};
use termewnal_pty::{
    GridSearchMatch, PtyEvent, PtySession, PtySpawnConfig, Scroll, SelectionType, ShellSpec, Side,
    TerminalSnapshot, detect_default_shell,
};
use termewnal_ssh::TransportStatus;
use tracing::error;

use crate::terminal_ergonomics::{
    FileDropAction, PendingPaste, SearchBarState, classify_file_drop, cursor_style_to_shape,
    format_paste_bytes, iced_theme_from_name, is_multiline_paste, open_url_with_system,
    paste_confirm_modal_view, read_clipboard_via_arboard, search_bar_view, search_input_id,
};
use crate::terminal_widget::{TerminalViewOptions, TerminalViewport};

/// Injectable OS URL-launcher boundary (`open::that_detached` by default).
pub type UrlOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Unique identifier for a runtime terminal `Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// Runtime state of an active local shell `Session`.
pub struct LocalSession {
    id: SessionId,
    title: String,
    pty: Arc<PtySession>,
    snapshot: Arc<TerminalSnapshot>,
    transport_status: TransportStatus,
    exited: Option<Option<i32>>,
}

impl LocalSession {
    /// Returns this `Session`'s identifier.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Returns the display title of this `Session`.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns a shared handle to the backing [`PtySession`].
    pub fn pty(&self) -> &Arc<PtySession> {
        &self.pty
    }

    /// Returns the most recent terminal grid snapshot.
    pub fn snapshot(&self) -> &Arc<TerminalSnapshot> {
        &self.snapshot
    }

    /// Returns the SSH `TransportStatus` (`Disconnected` for local shell sessions).
    pub fn transport_status(&self) -> &TransportStatus {
        &self.transport_status
    }

    /// Returns `Some(exit_code)` once the underlying shell process has exited.
    pub fn exited(&self) -> Option<Option<i32>> {
        self.exited
    }
}

/// Top-level Elm messages driving `AppState`.
#[derive(Debug, Clone)]
pub enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    CloseRequested(window::Id),
    TerminalInput {
        session_id: SessionId,
        bytes: Vec<u8>,
    },
    ViewportResized {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    PtyEvent {
        session_id: SessionId,
        event: PtyEvent,
    },
    ApplyTerminalConfig(TerminalConfig),
    OpenSearch,
    CloseSearch,
    SearchQueryChanged(String),
    SearchNext,
    SearchPrev,
    EscapePressed,
    TerminalFocused,
    ScrollViewport {
        session_id: SessionId,
        delta_lines: i32,
    },
    ScrollToTop(SessionId),
    ScrollToBottom(SessionId),
    SelectionStart {
        session_id: SessionId,
        col: usize,
        row: usize,
        side: Side,
        selection_type: SelectionType,
    },
    SelectionUpdate {
        session_id: SessionId,
        col: usize,
        row: usize,
        side: Side,
    },
    SelectionFinish {
        session_id: SessionId,
    },
    CopySelection {
        session_id: SessionId,
    },
    PasteFromClipboard {
        session_id: SessionId,
    },
    PasteText {
        session_id: SessionId,
        text: String,
    },
    ConfirmPendingPaste,
    CancelPendingPaste,
    OpenUrl(String),
    FileDropped {
        session_id: SessionId,
        path: PathBuf,
        shift_held: bool,
    },
    ImeCommitted {
        session_id: SessionId,
        text: String,
    },
}

/// Top-level Elm application state for Termewnal.
pub struct AppState {
    paths: ConfigPaths,
    config: AppConfig,
    profiles: ProfilesConfig,
    windows: BTreeSet<window::Id>,
    sessions: BTreeMap<SessionId, LocalSession>,
    active_session: Option<SessionId>,
    next_session_id: u64,
    last_error: Option<String>,
    search: SearchBarState,
    pending_paste: Option<PendingPaste>,
    last_copied: Option<String>,
    url_opener: UrlOpener,
}

impl AppState {
    /// Boots the desktop daemon with XDG paths and opens the initial GUI window.
    pub fn boot_daemon() -> (Self, Task<Message>) {
        let paths = ConfigPaths::detect_xdg()
            .unwrap_or_else(|_| ConfigPaths::from_root(std::env::temp_dir().join("termewnal")));
        let state = Self::boot_with_paths(paths).unwrap_or_else(|err| {
            error!(error = %err, "failed to boot AppState");
            panic!("failed to initialize Termewnal: {err}");
        });

        let (_win_id, open_task) = window::open(window::Settings {
            exit_on_close_request: false,
            size: Size::new(
                state.config.window.width as f32,
                state.config.window.height as f32,
            ),
            ..window::Settings::default()
        });

        (state, open_task.map(Message::WindowOpened))
    }

    /// Initializes configuration files under `paths` and spawns the initial local shell `Session`.
    pub fn boot_with_paths(paths: ConfigPaths) -> Result<Self, String> {
        let (config, profiles) =
            ensure_config_files(&paths).map_err(|err| format!("failed to load config: {err}"))?;

        let mut state = Self {
            paths,
            config,
            profiles,
            windows: BTreeSet::new(),
            sessions: BTreeMap::new(),
            active_session: None,
            next_session_id: 1,
            last_error: None,
            search: SearchBarState::default(),
            pending_paste: None,
            last_copied: None,
            url_opener: Arc::new(open_url_with_system),
        };

        state.spawn_default_local_session()?;
        Ok(state)
    }

    /// Overrides the OS URL opener boundary (used by tests to avoid spawning a desktop browser).
    pub fn with_url_opener(mut self, opener: UrlOpener) -> Self {
        self.url_opener = opener;
        self
    }

    fn spawn_default_local_session(&mut self) -> Result<SessionId, String> {
        let shell = self
            .config
            .terminal
            .shell
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| ShellSpec::custom(s, Vec::new()))
            .unwrap_or_else(detect_default_shell);

        let spawn_cfg = PtySpawnConfig {
            shell: shell.clone(),
            cols: 80,
            rows: 24,
            scrollback_lines: self.config.terminal.scrollback_lines,
            cursor_shape: cursor_style_to_shape(self.config.terminal.cursor_style),
            working_dir: None,
        };

        let pty = Arc::new(
            PtySession::spawn(spawn_cfg).map_err(|err| format!("failed to spawn PTY: {err}"))?,
        );
        let snapshot = Arc::new(pty.snapshot());
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;

        let session = LocalSession {
            id,
            title: shell.display_name,
            pty,
            snapshot,
            transport_status: TransportStatus::Disconnected,
            exited: None,
        };

        self.sessions.insert(id, session);
        self.active_session = Some(id);
        Ok(id)
    }

    /// Processes a top-level Elm [`Message`] and returns any follow-up [`Task`].
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowOpened(id) => {
                self.windows.insert(id);
                Task::none()
            }
            Message::CloseRequested(id) => window::close(id),
            Message::WindowClosed(id) => {
                self.windows.remove(&id);
                if self.windows.is_empty() {
                    iced::exit()
                } else {
                    Task::none()
                }
            }
            Message::TerminalInput { session_id, bytes } => {
                self.write_bytes_to_session(session_id, &bytes);
                Task::none()
            }
            Message::ViewportResized {
                session_id,
                cols,
                rows,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    if let Err(err) = session.pty.resize(cols, rows) {
                        error!(session = session_id.0, error = %err, "failed to resize PTY");
                        self.last_error = Some(err.to_string());
                    } else {
                        session.snapshot = Arc::new(session.pty.snapshot());
                    }
                }
                Task::none()
            }
            Message::PtyEvent { session_id, event } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    match event {
                        PtyEvent::Output => {
                            session.snapshot = Arc::new(session.pty.snapshot());
                            if Some(session_id) == self.active_session
                                && self.search.open
                                && !self.search.query.is_empty()
                            {
                                let pty = Arc::clone(&session.pty);
                                self.search.refresh_matches(Some(pty.as_ref()), false);
                            }
                        }
                        PtyEvent::Title(title) => {
                            if !title.is_empty() {
                                session.title = title;
                            }
                        }
                        PtyEvent::Bell => {}
                        PtyEvent::Exited(code) => {
                            session.snapshot = Arc::new(session.pty.snapshot());
                            session.exited = Some(code);
                        }
                    }
                }
                Task::none()
            }
            Message::ApplyTerminalConfig(cfg) => {
                let shape = cursor_style_to_shape(cfg.cursor_style);
                let scrollback = cfg.scrollback_lines;
                self.config.terminal = cfg;
                for session in self.sessions.values_mut() {
                    session.pty.apply_options(scrollback, shape);
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                if self.search.open && !self.search.query.is_empty() {
                    let active_pty = self
                        .active_session
                        .and_then(|id| self.sessions.get(&id))
                        .map(|s| s.pty.as_ref());
                    self.search.refresh_matches(active_pty, false);
                }
                Task::none()
            }
            Message::OpenSearch => {
                let active_pty = self
                    .active_session
                    .and_then(|id| self.sessions.get(&id))
                    .map(|s| s.pty.as_ref());
                let target = self.search.open(active_pty);
                self.focus_search_match(target);
                operation::focus(search_input_id())
            }
            Message::CloseSearch => {
                self.search.close();
                Task::none()
            }
            Message::SearchQueryChanged(query) => {
                let active_pty = self
                    .active_session
                    .and_then(|id| self.sessions.get(&id))
                    .map(|s| s.pty.as_ref());
                let target = self.search.set_query(query, active_pty);
                self.focus_search_match(target);
                Task::none()
            }
            Message::SearchNext => {
                let target = self.search.step_next();
                self.focus_search_match(target);
                Task::none()
            }
            Message::SearchPrev => {
                let target = self.search.step_prev();
                self.focus_search_match(target);
                Task::none()
            }
            Message::EscapePressed => {
                if self.pending_paste.take().is_some() {
                    return Task::none();
                }
                if self.search.open && self.search.input_focused {
                    self.search.close();
                }
                Task::none()
            }
            Message::TerminalFocused => {
                self.search.input_focused = false;
                Task::none()
            }
            Message::ScrollViewport {
                session_id,
                delta_lines,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.pty.scroll_display(Scroll::Delta(delta_lines));
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::ScrollToTop(session_id) => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.pty.scroll_display(Scroll::Top);
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::ScrollToBottom(session_id) => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.pty.scroll_to_bottom();
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::SelectionStart {
                session_id,
                col,
                row,
                side,
                selection_type,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session
                        .pty
                        .start_selection(col, row, side, selection_type);
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::SelectionUpdate {
                session_id,
                col,
                row,
                side,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.pty.update_selection(col, row, side);
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::SelectionFinish { session_id } => {
                let copied = if let Some(session) = self.sessions.get_mut(&session_id) {
                    let selected = session.pty.finish_selection();
                    session.snapshot = Arc::new(session.pty.snapshot());
                    if self.config.terminal.copy_on_select {
                        selected
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(text) = copied {
                    self.copy_text_to_clipboard(text)
                } else {
                    Task::none()
                }
            }
            Message::CopySelection { session_id } => {
                let copied = self
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.pty.selection_text());
                if let Some(text) = copied {
                    self.copy_text_to_clipboard(text)
                } else {
                    Task::none()
                }
            }
            Message::PasteFromClipboard { session_id } => clipboard::read().map(move |opt| {
                let text = opt.or_else(read_clipboard_via_arboard).unwrap_or_default();
                Message::PasteText { session_id, text }
            }),
            Message::PasteText { session_id, text } => {
                if text.is_empty() {
                    return Task::none();
                }
                if self.config.terminal.confirm_multiline_paste && is_multiline_paste(&text) {
                    self.pending_paste = Some(PendingPaste::new(session_id, text));
                } else {
                    self.dispatch_paste_to_session(session_id, &text);
                }
                Task::none()
            }
            Message::ConfirmPendingPaste => {
                if let Some(pending) = self.pending_paste.take() {
                    self.dispatch_paste_to_session(pending.session_id, &pending.text);
                }
                Task::none()
            }
            Message::CancelPendingPaste => {
                self.pending_paste = None;
                Task::none()
            }
            Message::OpenUrl(url) => {
                if let Err(err) = (self.url_opener)(&url) {
                    error!(url = %url, error = %err, "failed to open URL");
                    self.last_error = Some(err);
                }
                Task::none()
            }
            Message::FileDropped {
                session_id,
                path,
                shift_held,
            } => {
                let action = classify_file_drop(session_id, &path, false, shift_held, false);
                match action {
                    FileDropAction::InsertShellPath {
                        session_id,
                        escaped_text,
                    } => {
                        self.write_bytes_to_session(session_id, escaped_text.as_bytes());
                    }
                    FileDropAction::UploadToSftp { .. } => {}
                }
                Task::none()
            }
            Message::ImeCommitted { session_id, text } => {
                if !text.is_empty() {
                    self.write_bytes_to_session(session_id, text.as_bytes());
                }
                Task::none()
            }
        }
    }

    fn focus_search_match(&mut self, target: Option<GridSearchMatch>) {
        if let Some(matched) = target
            && let Some(session) = self
                .active_session
                .and_then(|id| self.sessions.get_mut(&id))
        {
            session.pty.scroll_to_match(matched);
            session.snapshot = Arc::new(session.pty.snapshot());
        }
    }

    fn copy_text_to_clipboard(&mut self, text: String) -> Task<Message> {
        self.last_copied = Some(text.clone());
        Task::batch([
            clipboard::write(text.clone()),
            clipboard::write_primary(text),
        ])
    }

    fn dispatch_paste_to_session(&mut self, session_id: SessionId, text: &str) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let bytes = format_paste_bytes(text, session.pty.is_bracketed_paste());
        self.write_bytes_to_session(session_id, &bytes);
    }

    fn write_bytes_to_session(&mut self, session_id: SessionId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if let Err(err) = session.pty.write_bytes(bytes) {
            error!(session = session_id.0, error = %err, "failed to write PTY input");
            self.last_error = Some(err.to_string());
        } else if session.pty.display_offset() > 0 {
            session.pty.scroll_to_bottom();
            session.snapshot = Arc::new(session.pty.snapshot());
        }
    }

    /// Renders the active `Session` inside the given GUI `window_id`.
    pub fn view(&self, _window_id: window::Id) -> Element<'_, Message, Theme, iced::Renderer> {
        let Some(session_id) = self.active_session else {
            return container("No active terminal session")
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into();
        };

        let Some(session) = self.sessions.get(&session_id) else {
            return container("Missing terminal session")
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        };

        let ime_enabled =
            self.pending_paste.is_none() && !(self.search.open && self.search.input_focused);
        let options = TerminalViewOptions::from_config(&self.config.terminal, ime_enabled);
        let matches: Arc<[termewnal_pty::GridSearchMatch]> =
            Arc::from(self.search.matches.as_slice());

        let terminal_layer = TerminalViewport::new(
            session.id,
            Arc::clone(&session.snapshot),
            options,
            matches,
            self.search.active_index,
        );

        let palette = ThemePalette::for_name(&self.config.terminal.theme);
        match (self.search.open, &self.pending_paste) {
            (false, None) => terminal_layer,
            (true, None) => stack![terminal_layer, search_bar_view(&self.search, &palette)]
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            (false, Some(pending)) => {
                stack![terminal_layer, paste_confirm_modal_view(pending, &palette)]
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
            (true, Some(pending)) => stack![
                terminal_layer,
                search_bar_view(&self.search, &palette),
                paste_confirm_modal_view(pending, &palette)
            ]
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        }
    }

    /// Computes the window title for `window_id`.
    pub fn title(&self, _window_id: window::Id) -> String {
        if let Some(session) = self.active_session.and_then(|id| self.sessions.get(&id)) {
            match session.exited {
                Some(Some(code)) => format!("Termewnal — {} [exited: {code}]", session.title),
                Some(None) => format!("Termewnal — {} [exited]", session.title),
                None => format!("Termewnal — {}", session.title),
            }
        } else {
            "Termewnal".to_string()
        }
    }

    /// Resolves the active `iced::Theme` from configuration.
    pub fn theme(&self, _window_id: window::Id) -> Theme {
        iced_theme_from_name(&self.config.terminal.theme)
    }

    /// Subscribes to window lifecycle events and live PTY output streams.
    pub fn subscription(&self) -> Subscription<Message> {
        let close_requests = window::close_requests().map(Message::CloseRequested);
        let closed = window::close_events().map(Message::WindowClosed);
        let shortcuts = event::listen_with(window_shortcut_event);

        let pty_subs = self.sessions.values().map(|session| {
            Subscription::run_with(
                PtySubKey {
                    session_id: session.id,
                    pty: Arc::clone(&session.pty),
                },
                pty_event_stream,
            )
        });

        Subscription::batch(
            [close_requests, closed, shortcuts]
                .into_iter()
                .chain(pty_subs),
        )
    }

    /// Returns the resolved configuration directory paths.
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    /// Returns the active [`AppConfig`].
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Returns the active [`ProfilesConfig`].
    pub fn profiles(&self) -> &ProfilesConfig {
        &self.profiles
    }

    /// Returns the currently active `SessionId`.
    pub fn active_session_id(&self) -> Option<SessionId> {
        self.active_session
    }

    /// Returns an immutable reference to a runtime `Session` by `id`.
    pub fn session(&self, id: SessionId) -> Option<&LocalSession> {
        self.sessions.get(&id)
    }

    /// Returns the set of currently open GUI window IDs.
    pub fn windows(&self) -> &BTreeSet<window::Id> {
        &self.windows
    }

    /// Returns the most recent non-fatal runtime error message, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Returns the active `Ctrl+F` search bar state.
    pub fn search(&self) -> &SearchBarState {
        &self.search
    }

    /// Returns the pending multi-line paste confirmation payload, if any.
    pub fn pending_paste(&self) -> Option<&PendingPaste> {
        self.pending_paste.as_ref()
    }

    /// Returns the most recent text copied via selection or `Ctrl+Shift+C`.
    pub fn last_copied_text(&self) -> Option<&str> {
        self.last_copied.as_deref()
    }
}

fn window_shortcut_event(
    event: Event,
    status: event::Status,
    _window_id: window::Id,
) -> Option<Message> {
    if status == event::Status::Captured {
        return None;
    }

    let Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        modified_key,
        modifiers,
        ..
    }) = event
    else {
        return None;
    };

    if modifiers.control()
        && !modifiers.shift()
        && !modifiers.alt()
        && !modifiers.logo()
        && matches!(
            (key.as_ref(), modified_key.as_ref()),
            (Key::Character(s), _) | (_, Key::Character(s)) if s.eq_ignore_ascii_case("f")
        )
    {
        return Some(Message::OpenSearch);
    }

    match key {
        Key::Named(Named::Escape) => Some(Message::EscapePressed),
        Key::Named(Named::F3) => Some(if modifiers.shift() {
            Message::SearchPrev
        } else {
            Message::SearchNext
        }),
        _ => None,
    }
}

#[derive(Clone)]
struct PtySubKey {
    session_id: SessionId,
    pty: Arc<PtySession>,
}

impl Hash for PtySubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.session_id.hash(state);
    }
}

fn pty_event_stream(
    key: &PtySubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let session_id = key.session_id;
    let pty = Arc::clone(&key.pty);
    Box::pin(iced::stream::channel(64, async move |mut output| {
        if let Some(mut rx) = pty.take_event_receiver() {
            while let Some(event) = rx.recv().await {
                if output
                    .send(Message::PtyEvent { session_id, event })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }))
}
