use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use iced::futures::{SinkExt, Stream};
use iced::keyboard::{Key, Modifiers, key::Named};
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Size, Subscription, Task, Theme,
    window,
};
use termewnal_core::{AppConfig, ConfigPaths, ProfilesConfig, ensure_config_files};
use termewnal_pty::{
    PtyEvent, PtySession, PtySpawnConfig, ShellSpec, TerminalSnapshot, detect_default_shell,
};
use termewnal_ssh::TransportStatus;
use tracing::error;

use crate::split_view::{PaneFrame, ShortcutInterceptor, SplitView};
use crate::terminal_widget::TerminalViewport;
use crate::workspace::{
    ClosePaneOutcome, CycleDirection, FocusDirection, PaneId, SplitAxis, SplitId, SplitNode,
    TabId, Workspace, WorkspaceTab,
};

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
    NewTab,
    SelectTab(TabId),
    CycleTab(CycleDirection),
    MoveTab {
        tab_id: TabId,
        to_index: usize,
    },
    MoveTabBy {
        tab_id: TabId,
        delta: isize,
    },
    CloseTab(TabId),
    SplitFocusedPane(SplitAxis),
    SplitPane {
        pane_id: PaneId,
        axis: SplitAxis,
    },
    ClosePane(PaneId),
    CloseFocusedPane,
    ResizeSplit {
        split_id: SplitId,
        ratio: f32,
    },
    FocusPane(PaneId),
    FocusSession(SessionId),
    MoveFocus(FocusDirection),
    CycleFocus(CycleDirection),
    TogglePaneZoom,
    ToggleBroadcastInput,
    SetBroadcastInput(bool),
}

/// Top-level Elm application state for Termewnal.
pub struct AppState {
    paths: ConfigPaths,
    config: AppConfig,
    profiles: ProfilesConfig,
    windows: BTreeSet<window::Id>,
    sessions: BTreeMap<SessionId, LocalSession>,
    workspace: Workspace,
    next_session_id: u64,
    last_error: Option<String>,
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
            workspace: Workspace::new(),
            next_session_id: 1,
            last_error: None,
        };

        let initial_session = state.spawn_default_local_session()?;
        state.workspace.create_tab(initial_session, None);
        Ok(state)
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
        Ok(id)
    }

    fn remove_session(&mut self, session_id: SessionId) {
        if let Some(session) = self.sessions.remove(&session_id) {
            session.pty.terminate();
        }
    }

    fn split_target_pane(&mut self, tab_id: TabId, target_pane: PaneId, axis: SplitAxis) {
        match self.spawn_default_local_session() {
            Ok(session_id) => {
                if self
                    .workspace
                    .split_pane(tab_id, target_pane, axis, session_id)
                    .is_none()
                {
                    self.remove_session(session_id);
                }
            }
            Err(err) => {
                error!(error = %err, "failed to spawn session for split pane");
                self.last_error = Some(err);
            }
        }
    }

    fn close_target_pane(&mut self, tab_id: TabId, pane_id: PaneId) {
        if let Some(outcome) = self.workspace.close_pane(tab_id, pane_id) {
            let sid = match outcome {
                ClosePaneOutcome::ClosedPane { session_id, .. }
                | ClosePaneOutcome::ClosedTab { session_id } => session_id,
            };
            self.remove_session(sid);
        }
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
                let targets = self.workspace.input_targets_for_session(session_id);
                let is_broadcast = targets.len() > 1;
                for target_id in targets {
                    if let Some(session) = self.sessions.get(&target_id) {
                        if is_broadcast && session.exited.is_some() {
                            continue;
                        }
                        if let Err(err) = session.pty.write_bytes(&bytes) {
                            error!(
                                session = target_id.0,
                                error = %err,
                                "failed to write PTY input"
                            );
                            self.last_error = Some(err.to_string());
                        }
                    }
                }
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
            Message::NewTab => {
                match self.spawn_default_local_session() {
                    Ok(session_id) => {
                        self.workspace.create_tab(session_id, None);
                    }
                    Err(err) => {
                        error!(error = %err, "failed to create new tab session");
                        self.last_error = Some(err);
                    }
                }
                Task::none()
            }
            Message::SelectTab(tab_id) => {
                self.workspace.select_tab(tab_id);
                Task::none()
            }
            Message::CycleTab(direction) => {
                self.workspace.cycle_tab(direction);
                Task::none()
            }
            Message::MoveTab { tab_id, to_index } => {
                self.workspace.reorder_tab(tab_id, to_index);
                Task::none()
            }
            Message::MoveTabBy { tab_id, delta } => {
                self.workspace.move_tab_by(tab_id, delta);
                Task::none()
            }
            Message::CloseTab(tab_id) => {
                if let Some(removed_sessions) = self.workspace.close_tab(tab_id) {
                    for sid in removed_sessions {
                        self.remove_session(sid);
                    }
                }
                Task::none()
            }
            Message::SplitFocusedPane(axis) => {
                if let Some((tab_id, target_pane)) = self
                    .workspace
                    .active_tab()
                    .map(|tab| (tab.id, tab.focused_pane))
                {
                    self.split_target_pane(tab_id, target_pane, axis);
                }
                Task::none()
            }
            Message::SplitPane { pane_id, axis } => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.split_target_pane(tab_id, pane_id, axis);
                }
                Task::none()
            }
            Message::ClosePane(pane_id) => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.close_target_pane(tab_id, pane_id);
                }
                Task::none()
            }
            Message::CloseFocusedPane => {
                if let Some((tab_id, pane_id)) = self
                    .workspace
                    .active_tab()
                    .map(|tab| (tab.id, tab.focused_pane))
                {
                    self.close_target_pane(tab_id, pane_id);
                }
                Task::none()
            }
            Message::ResizeSplit { split_id, ratio } => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.workspace.resize_split(tab_id, split_id, ratio);
                }
                Task::none()
            }
            Message::FocusPane(pane_id) => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.workspace.focus_pane(tab_id, pane_id);
                }
                Task::none()
            }
            Message::FocusSession(session_id) => {
                self.workspace.focus_session(session_id);
                Task::none()
            }
            Message::MoveFocus(direction) => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.workspace.move_focus(tab_id, direction);
                }
                Task::none()
            }
            Message::CycleFocus(direction) => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.workspace.cycle_focus(tab_id, direction);
                }
                Task::none()
            }
            Message::TogglePaneZoom => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.workspace.toggle_zoom(tab_id);
                }
                Task::none()
            }
            Message::ToggleBroadcastInput => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.workspace.toggle_broadcast_input(tab_id);
                }
                Task::none()
            }
            Message::SetBroadcastInput(enabled) => {
                if let Some(tab_id) = self.workspace.active_tab_id() {
                    self.workspace.set_broadcast_input(tab_id, enabled);
                }
                Task::none()
            }
        }
    }

    /// Renders the workspace tab strip, optional Broadcast Input banner, and active split panes.
    pub fn view(&self, _window_id: window::Id) -> Element<'_, Message, Theme, iced::Renderer> {
        let mut root = column![self.render_tab_strip()].width(Length::Fill).height(Length::Fill);

        if let Some(banner_text) = self.broadcast_banner_text() {
            root = root.push(render_broadcast_banner(banner_text));
        }

        let body = match self.workspace.active_tab() {
            Some(tab) => self.render_tab_body(tab),
            None => self.render_empty_workspace(),
        };
        root = root.push(body);

        ShortcutInterceptor::new(root.into(), self.active_tab_id())
    }

    fn render_tab_strip(&self) -> Element<'_, Message, Theme, iced::Renderer> {
        let active_tab_id = self.active_tab_id();
        let tab_count = self.workspace.tab_count();

        let mut tabs_row = row![].spacing(4).align_y(Alignment::Center);

        for (idx, tab) in self.workspace.tabs().iter().enumerate() {
            let is_active = Some(tab.id) == active_tab_id;
            let label = self.tab_display_title(tab);

            let mut tab_controls = row![].spacing(2).align_y(Alignment::Center);
            tab_controls = tab_controls.push(
                button(text(label).size(13))
                    .padding([4, 8])
                    .style(if is_active {
                        button::primary
                    } else {
                        button::secondary
                    })
                    .on_press(Message::SelectTab(tab.id)),
            );

            if tab_count > 1 {
                if idx > 0 {
                    tab_controls = tab_controls.push(
                        button(text("<").size(11))
                            .padding([4, 5])
                            .style(button::subtle)
                            .on_press(Message::MoveTabBy {
                                tab_id: tab.id,
                                delta: -1,
                            }),
                    );
                }
                if idx + 1 < tab_count {
                    tab_controls = tab_controls.push(
                        button(text(">").size(11))
                            .padding([4, 5])
                            .style(button::subtle)
                            .on_press(Message::MoveTabBy {
                                tab_id: tab.id,
                                delta: 1,
                            }),
                    );
                }
            }

            tab_controls = tab_controls.push(
                button(text("x").size(11))
                    .padding([4, 6])
                    .style(button::subtle)
                    .on_press(Message::CloseTab(tab.id)),
            );

            tabs_row = tabs_row.push(tab_controls);
        }

        let active_zoomed = self.active_tab_zoomed_pane().is_some();
        let active_broadcast = self.is_broadcast_banner_visible();

        let mut actions = row![
            button(text("+ Tab").size(12))
                .padding([4, 8])
                .style(button::secondary)
                .on_press(Message::NewTab),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        if active_tab_id.is_some() {
            actions = actions
                .push(
                    button(text("Split V").size(12))
                        .padding([4, 8])
                        .style(button::subtle)
                        .on_press(Message::SplitFocusedPane(SplitAxis::Vertical)),
                )
                .push(
                    button(text("Split H").size(12))
                        .padding([4, 8])
                        .style(button::subtle)
                        .on_press(Message::SplitFocusedPane(SplitAxis::Horizontal)),
                )
                .push(
                    button(text(if active_zoomed { "Unzoom" } else { "Zoom" }).size(12))
                        .padding([4, 8])
                        .style(if active_zoomed {
                            button::primary
                        } else {
                            button::subtle
                        })
                        .on_press(Message::TogglePaneZoom),
                )
                .push(
                    button(text("Broadcast").size(12))
                        .padding([4, 8])
                        .style(if active_broadcast {
                            button::danger
                        } else {
                            button::subtle
                        })
                        .on_press(Message::ToggleBroadcastInput),
                );
        }

        let tabs_scroll = scrollable(tabs_row)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(2).scroller_width(2),
            ))
            .width(Length::Fill);

        let bar = row![tabs_scroll, actions]
            .spacing(8)
            .padding([6, 8])
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

    fn render_tab_body(&self, tab: &WorkspaceTab) -> Element<'_, Message, Theme, iced::Renderer> {
        if let Some(zoomed_pane) = tab.zoomed_pane
            && let Some(session_id) = tab.root.session_for_pane(zoomed_pane)
        {
            return self.render_pane(
                zoomed_pane,
                session_id,
                true,
                tab.broadcast_input,
                false,
            );
        }

        let show_border = tab.root.pane_count() > 1;
        self.render_split_node(
            &tab.root,
            tab.focused_pane,
            tab.broadcast_input,
            show_border,
        )
    }

    fn render_split_node(
        &self,
        node: &SplitNode,
        focused_pane: PaneId,
        broadcast: bool,
        show_border: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        match node {
            SplitNode::Leaf {
                pane_id,
                session_id,
            } => self.render_pane(
                *pane_id,
                *session_id,
                *pane_id == focused_pane,
                broadcast,
                show_border,
            ),
            SplitNode::Split {
                split_id,
                axis,
                ratio,
                first,
                second,
            } => SplitView::new(
                *split_id,
                *axis,
                *ratio,
                self.render_split_node(first, focused_pane, broadcast, show_border),
                self.render_split_node(second, focused_pane, broadcast, show_border),
            ),
        }
    }

    fn render_pane(
        &self,
        pane_id: PaneId,
        session_id: SessionId,
        focused: bool,
        broadcast: bool,
        show_border: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let content = match self.sessions.get(&session_id) {
            Some(session) => TerminalViewport::with_focus(
                session.id,
                Arc::clone(&session.snapshot),
                self.config.terminal.font_size,
                focused,
            ),
            None => container("Missing terminal session")
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        };

        PaneFrame::new(pane_id, focused, broadcast, show_border, content)
    }

    fn render_empty_workspace(&self) -> Element<'_, Message, Theme, iced::Renderer> {
        let prompt = column![
            text("No open terminal tabs").size(16),
            button(text("Open New Tab (Ctrl+Shift+T)").size(14))
                .padding([8, 14])
                .style(button::primary)
                .on_press(Message::NewTab),
        ]
        .spacing(12)
        .align_x(Alignment::Center);

        container(prompt)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    fn tab_display_title(&self, tab: &WorkspaceTab) -> String {
        let base = tab
            .title
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                tab.focused_session_id()
                    .and_then(|sid| self.sessions.get(&sid))
                    .map(|s| s.title.as_str())
            })
            .unwrap_or("Shell");

        let pane_count = tab.root.pane_count();
        let mut label = if pane_count > 1 {
            format!("{base} ({pane_count})")
        } else {
            base.to_string()
        };
        if tab.zoomed_pane.is_some() {
            label.push_str(" [ZOOM]");
        }
        if tab.broadcast_input {
            label.push_str(" [BCAST]");
        }
        label
    }

    /// Computes the window title for `window_id`.
    pub fn title(&self, _window_id: window::Id) -> String {
        if let Some(session) = self.active_session_id().and_then(|id| self.sessions.get(&id)) {
            let mut title = match session.exited {
                Some(Some(code)) => format!("Termewnal — {} [exited: {code}]", session.title),
                Some(None) => format!("Termewnal — {} [exited]", session.title),
                None => format!("Termewnal — {}", session.title),
            };
            if self.active_tab_zoomed_pane().is_some() {
                title.push_str(" [ZOOM]");
            }
            if self.is_broadcast_banner_visible() {
                title.push_str(" [BROADCAST INPUT]");
            }
            title
        } else {
            "Termewnal".to_string()
        }
    }

    /// Resolves the active `iced::Theme` from configuration.
    pub fn theme(&self, _window_id: window::Id) -> Theme {
        theme_from_name(&self.config.terminal.theme)
    }

    /// Subscribes to window lifecycle events and live PTY output streams.
    pub fn subscription(&self) -> Subscription<Message> {
        let close_requests = window::close_requests().map(Message::CloseRequested);
        let closed = window::close_events().map(Message::WindowClosed);

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
            [close_requests, closed]
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

    /// Returns the workspace tab and split-pane layout state.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Returns the currently active `TabId`, if any tab is open.
    pub fn active_tab_id(&self) -> Option<TabId> {
        self.workspace.active_tab_id()
    }

    /// Returns the currently active `SessionId`.
    pub fn active_session_id(&self) -> Option<SessionId> {
        self.workspace
            .active_tab()
            .and_then(|tab| tab.focused_session_id())
    }

    /// Returns the zoomed `PaneId` in the active tab, if pane zoom is currently active.
    pub fn active_tab_zoomed_pane(&self) -> Option<PaneId> {
        self.workspace.active_tab().and_then(|tab| tab.zoomed_pane)
    }

    /// Returns `true` when the active workspace tab has Broadcast Input mode enabled.
    pub fn is_broadcast_banner_visible(&self) -> bool {
        self.workspace
            .active_tab()
            .is_some_and(|tab| tab.broadcast_input)
    }

    /// Returns the warning banner copy when Broadcast Input is active in the current tab.
    pub fn broadcast_banner_text(&self) -> Option<String> {
        let tab = self.workspace.active_tab()?;
        if !tab.broadcast_input {
            return None;
        }
        let count = tab.root.pane_count();
        let noun = if count == 1 { "pane" } else { "panes" };
        Some(format!(
            "BROADCAST INPUT ACTIVE — Keystrokes are mirrored to {count} {noun} in this tab"
        ))
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

/// Maps a pressed key + modifiers to a workspace-level [`Message`], if any matches.
pub fn shortcut_for_key_event(
    key: &Key,
    modifiers: Modifiers,
    active_tab: Option<TabId>,
) -> Option<Message> {
    if modifiers.logo() {
        return None;
    }

    if modifiers.control() && modifiers.shift() && !modifiers.alt() {
        return match key.as_ref() {
            Key::Character("t") | Key::Character("T") => Some(Message::NewTab),
            Key::Character("w") | Key::Character("W") => Some(Message::CloseFocusedPane),
            Key::Character("d")
            | Key::Character("D")
            | Key::Character("\\")
            | Key::Character("|") => Some(Message::SplitFocusedPane(SplitAxis::Vertical)),
            Key::Character("o")
            | Key::Character("O")
            | Key::Character("-")
            | Key::Character("_") => Some(Message::SplitFocusedPane(SplitAxis::Horizontal)),
            Key::Character("z") | Key::Character("Z") => Some(Message::TogglePaneZoom),
            Key::Character("b") | Key::Character("B") => Some(Message::ToggleBroadcastInput),
            Key::Character("]") | Key::Character("}") => {
                Some(Message::CycleFocus(CycleDirection::Next))
            }
            Key::Character("[") | Key::Character("{") => {
                Some(Message::CycleFocus(CycleDirection::Previous))
            }
            Key::Named(Named::Tab) => Some(Message::CycleTab(CycleDirection::Previous)),
            Key::Named(Named::PageUp) => active_tab.map(|tab_id| Message::MoveTabBy {
                tab_id,
                delta: -1,
            }),
            Key::Named(Named::PageDown) => active_tab.map(|tab_id| Message::MoveTabBy {
                tab_id,
                delta: 1,
            }),
            _ => None,
        };
    }

    if modifiers.control() && !modifiers.shift() && !modifiers.alt() {
        if let Key::Named(Named::Tab) = key {
            return Some(Message::CycleTab(CycleDirection::Next));
        }
        return None;
    }

    if modifiers.control() && modifiers.alt() && !modifiers.shift() {
        return match key {
            Key::Named(Named::ArrowLeft) => Some(Message::MoveFocus(FocusDirection::Left)),
            Key::Named(Named::ArrowRight) => Some(Message::MoveFocus(FocusDirection::Right)),
            Key::Named(Named::ArrowUp) => Some(Message::MoveFocus(FocusDirection::Up)),
            Key::Named(Named::ArrowDown) => Some(Message::MoveFocus(FocusDirection::Down)),
            _ => None,
        };
    }

    None
}

fn render_broadcast_banner(banner_text: String) -> Element<'static, Message, Theme, iced::Renderer> {
    let content = row![
        text(banner_text).size(13).color(Color::WHITE),
        space::horizontal(),
        button(text("Turn Off").size(12))
            .padding([3, 10])
            .style(button::secondary)
            .on_press(Message::SetBroadcastInput(false)),
    ]
    .spacing(12)
    .padding([6, 12])
    .align_y(Alignment::Center);

    container(content)
        .width(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(185, 28, 28))),
            border: Border {
                color: Color::from_rgb8(248, 113, 113),
                width: 1.0,
                radius: 0.0.into(),
            },
            text_color: Some(Color::WHITE),
            ..Default::default()
        })
        .into()
}

fn theme_from_name(name: &str) -> Theme {
    match name.trim().to_ascii_lowercase().as_str() {
        "light" => Theme::Light,
        "dracula" => Theme::Dracula,
        "nord" => Theme::Nord,
        "tokyo night" | "tokyonight" => Theme::TokyoNight,
        "catppuccin" | "catppuccin mocha" | "catppuccinmocha" => Theme::CatppuccinMocha,
        _ => Theme::Dark,
    }
}
