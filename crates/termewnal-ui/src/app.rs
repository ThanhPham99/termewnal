use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::futures::{SinkExt, Stream};
use iced::keyboard::{Key, Modifiers, key::Named};
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Size, Subscription, Task, Theme,
    window,
};
use termewnal_core::{
    AppConfig, ConfigFileKind, ConfigPaths, ConfigWatchEvent, ConfigWatcher, Profile,
    ProfilesConfig, TomlReloadError, VaultBackend, VaultSession, ensure_config_files,
};
use termewnal_pty::{
    PtyEvent, PtySession, PtySpawnConfig, ShellSpec, TerminalBuffer, TerminalSnapshot,
    detect_default_shell,
};
use termewnal_ssh::{SshSession, TransportStatus};
use tracing::error;
use zeroize::Zeroize;

use crate::config_reload_ui::ConfigReloadUiState;
use crate::split_view::{PaneFrame, ShortcutInterceptor, SplitView};
use crate::ssh_ui::{SshTargetSpec, SshUiEvent, SshUiState, connect_ssh_task};
use crate::terminal_widget::TerminalViewport;
use crate::workspace::{
    ClosePaneOutcome, CycleDirection, FocusDirection, PaneId, SplitAxis, SplitId, SplitNode,
    TabId, Workspace, WorkspaceTab,
};

/// Unique identifier for a runtime terminal `Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

enum SessionBacking {
    Local {
        pty: Arc<PtySession>,
    },
    Ssh {
        spec: SshTargetSpec,
        generation: u64,
        active: Option<Arc<SshSession>>,
        last_error: Option<String>,
    },
}

/// Runtime state of an active local shell or SSH `Session`.
pub struct LocalSession {
    id: SessionId,
    title: String,
    backing: SessionBacking,
    buffer: Arc<TerminalBuffer>,
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

    /// Returns `true` if this `Session` is an SSH session rather than a local shell.
    pub fn is_ssh(&self) -> bool {
        matches!(self.backing, SessionBacking::Ssh { .. })
    }

    /// Returns a shared handle to the backing [`PtySession`] for local shell sessions.
    pub fn pty(&self) -> &Arc<PtySession> {
        self.try_pty()
            .expect("pty() called on non-local SSH Session")
    }

    /// Returns `Some(&Arc<PtySession>)` for local shell sessions, or `None` for SSH sessions.
    pub fn try_pty(&self) -> Option<&Arc<PtySession>> {
        match &self.backing {
            SessionBacking::Local { pty } => Some(pty),
            SessionBacking::Ssh { .. } => None,
        }
    }

    /// Returns the active [`SshSession`] handle if this SSH session is currently connected.
    pub fn ssh_session(&self) -> Option<&Arc<SshSession>> {
        match &self.backing {
            SessionBacking::Ssh { active, .. } => active.as_ref(),
            SessionBacking::Local { .. } => None,
        }
    }

    /// Returns the current SSH connection attempt generation counter for SSH sessions.
    pub fn ssh_generation(&self) -> Option<u64> {
        match &self.backing {
            SessionBacking::Ssh { generation, .. } => Some(*generation),
            SessionBacking::Local { .. } => None,
        }
    }

    /// Returns the [`SshTargetSpec`] for SSH sessions.
    pub fn ssh_spec(&self) -> Option<&SshTargetSpec> {
        match &self.backing {
            SessionBacking::Ssh { spec, .. } => Some(spec),
            SessionBacking::Local { .. } => None,
        }
    }

    /// Returns the shared [`TerminalBuffer`] backing this `Session`.
    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
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
    ConfigFilesChanged(Vec<ConfigFileKind>),
    RestoreConfigBackup(ConfigFileKind),
    DismissReloadToast(ConfigFileKind),
    VaultUnlockInputChanged(String),
    VaultUnlockSubmitted(String),
    VaultInitializeSubmitted(String),
    VaultLock,
    VaultAutoLockTick,
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
    OpenSshProfile(Profile),
    RestoreSshProfile(Profile),
    QuickConnectSsh(SshTargetSpec),
    ConnectSshSession(SessionId),
    DisconnectSshSession(SessionId),
    SshEvent {
        session_id: SessionId,
        event: SshUiEvent,
    },
    RespondHostKeyPrompt {
        session_id: SessionId,
        accept: bool,
    },
    SshAuthInputChanged {
        session_id: SessionId,
        index: usize,
        value: String,
    },
    SubmitSshAuthPrompt(SessionId),
    CancelSshAuthPrompt(SessionId),
    DismissMitmAlert(SessionId),
}

/// Top-level Elm application state for Termewnal.
pub struct AppState {
    paths: ConfigPaths,
    config: AppConfig,
    profiles: ProfilesConfig,
    vault: Arc<VaultSession>,
    reload_ui: ConfigReloadUiState,
    ssh_ui: SshUiState,
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
        let vault = Arc::new(VaultSession::for_paths(&paths, &config.vault));
        Self::boot_with_loaded(paths, config, profiles, vault)
    }

    /// Initializes configuration files under `paths` using an injected [`VaultSession`] (for tests).
    pub fn boot_with_paths_and_vault(
        paths: ConfigPaths,
        vault: Arc<VaultSession>,
    ) -> Result<Self, String> {
        let (config, profiles) =
            ensure_config_files(&paths).map_err(|err| format!("failed to load config: {err}"))?;
        vault.apply_config(&config.vault);
        Self::boot_with_loaded(paths, config, profiles, vault)
    }

    fn boot_with_loaded(
        paths: ConfigPaths,
        config: AppConfig,
        profiles: ProfilesConfig,
        vault: Arc<VaultSession>,
    ) -> Result<Self, String> {
        let mut state = Self {
            paths,
            config,
            profiles,
            vault,
            reload_ui: ConfigReloadUiState::default(),
            ssh_ui: SshUiState::default(),
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
        let buffer = Arc::clone(pty.buffer());
        let snapshot = Arc::new(buffer.snapshot());
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;

        let session = LocalSession {
            id,
            title: shell.display_name,
            backing: SessionBacking::Local { pty },
            buffer,
            snapshot,
            transport_status: TransportStatus::Disconnected,
            exited: None,
        };

        self.sessions.insert(id, session);
        Ok(id)
    }

    /// Creates a new SSH `Session` tab from `spec`, connecting immediately when `auto_connect` is
    /// `true` or waiting in `Disconnected` state until user action otherwise.
    pub fn open_ssh_session(
        &mut self,
        spec: SshTargetSpec,
        auto_connect: bool,
    ) -> (SessionId, Task<Message>) {
        let buffer = Arc::new(TerminalBuffer::new(
            spec.connect_config.cols,
            spec.connect_config.rows,
            spec.connect_config.scrollback_lines,
        ));
        let snapshot = Arc::new(buffer.snapshot());
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;

        let title = spec.title.clone();
        let session = LocalSession {
            id,
            title: title.clone(),
            backing: SessionBacking::Ssh {
                spec,
                generation: 0,
                active: None,
                last_error: None,
            },
            buffer,
            snapshot,
            transport_status: TransportStatus::Disconnected,
            exited: None,
        };
        self.sessions.insert(id, session);
        self.workspace.create_tab(id, Some(title));

        if auto_connect {
            let task = self.start_ssh_connect(id);
            (id, task)
        } else {
            (id, Task::none())
        }
    }

    fn start_ssh_connect(&mut self, session_id: SessionId) -> Task<Message> {
        self.ssh_ui.clear_session_prompts(session_id);
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Task::none();
        };
        let (cols, rows) = session.buffer.dimensions();
        let buffer = Arc::clone(&session.buffer);
        let SessionBacking::Ssh {
            spec,
            generation,
            active,
            last_error,
        } = &mut session.backing
        else {
            return Task::none();
        };

        if let Some(prev) = active.take() {
            prev.disconnect();
        }
        *generation = generation.wrapping_add(1);
        *last_error = None;
        spec.connect_config.cols = cols;
        spec.connect_config.rows = rows;
        session.transport_status = TransportStatus::Connecting;
        session.exited = None;

        connect_ssh_task(
            session_id,
            *generation,
            spec.clone(),
            buffer,
            Arc::clone(&self.vault),
        )
    }

    fn disconnect_ssh_session(&mut self, session_id: SessionId) {
        self.ssh_ui.clear_session_prompts(session_id);
        if let Some(session) = self.sessions.get_mut(&session_id)
            && let SessionBacking::Ssh { active, .. } = &mut session.backing
        {
            if let Some(ssh) = active.take() {
                ssh.disconnect();
            }
            session.transport_status = TransportStatus::Disconnected;
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        self.ssh_ui.clear_session_prompts(session_id);
        if let Some(session) = self.sessions.remove(&session_id) {
            match session.backing {
                SessionBacking::Local { pty } => pty.terminate(),
                SessionBacking::Ssh { active, .. } => {
                    if let Some(ssh) = active {
                        ssh.disconnect();
                    }
                }
            }
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
                let mut connect_ids = Vec::new();
                for target_id in targets {
                    if let Some(session) = self.sessions.get(&target_id) {
                        if is_broadcast && session.exited.is_some() {
                            continue;
                        }
                        match &session.backing {
                            SessionBacking::Local { pty } => {
                                if let Err(err) = pty.write_bytes(&bytes) {
                                    error!(
                                        session = target_id.0,
                                        error = %err,
                                        "failed to write PTY input"
                                    );
                                    self.last_error = Some(err.to_string());
                                }
                            }
                            SessionBacking::Ssh { active, .. } => {
                                if let Some(ssh) = active {
                                    if let Err(err) = ssh.write_bytes(&bytes) {
                                        error!(
                                            session = target_id.0,
                                            error = %err,
                                            "failed to write SSH input"
                                        );
                                        self.last_error = Some(err.to_string());
                                    }
                                } else if !is_broadcast
                                    && session.transport_status == TransportStatus::Disconnected
                                    && (bytes.contains(&b'\r') || bytes.contains(&b'\n'))
                                {
                                    connect_ids.push(target_id);
                                }
                            }
                        }
                    }
                }
                if connect_ids.is_empty() {
                    Task::none()
                } else {
                    Task::batch(
                        connect_ids
                            .into_iter()
                            .map(|id| self.start_ssh_connect(id)),
                    )
                }
            }
            Message::ViewportResized {
                session_id,
                cols,
                rows,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    match &session.backing {
                        SessionBacking::Local { pty } => {
                            if let Err(err) = pty.resize(cols, rows) {
                                error!(session = session_id.0, error = %err, "failed to resize PTY");
                                self.last_error = Some(err.to_string());
                            } else {
                                session.snapshot = Arc::new(session.buffer.snapshot());
                            }
                        }
                        SessionBacking::Ssh { active, .. } => {
                            if let Some(ssh) = active {
                                if let Err(err) = ssh.resize(cols, rows) {
                                    error!(session = session_id.0, error = %err, "failed to resize SSH");
                                    self.last_error = Some(err.to_string());
                                }
                            } else {
                                session.buffer.resize(cols, rows);
                            }
                            session.snapshot = Arc::new(session.buffer.snapshot());
                        }
                    }
                }
                Task::none()
            }
            Message::PtyEvent { session_id, event } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    match event {
                        PtyEvent::Output => {
                            session.snapshot = Arc::new(session.buffer.snapshot());
                        }
                        PtyEvent::Title(title) => {
                            if !title.is_empty() {
                                session.title = title;
                            }
                        }
                        PtyEvent::Bell => {}
                        PtyEvent::Exited(code) => {
                            session.snapshot = Arc::new(session.buffer.snapshot());
                            session.exited = Some(code);
                            if let SessionBacking::Ssh { active, .. } = &mut session.backing {
                                *active = None;
                                session.transport_status = TransportStatus::Disconnected;
                            }
                        }
                    }
                }
                Task::none()
            }
            Message::ConfigFilesChanged(kinds) => {
                let mut seen = BTreeSet::new();
                for kind in kinds {
                    if !seen.insert(kind) {
                        continue;
                    }
                    let res = match kind {
                        ConfigFileKind::AppConfig => self.reload_ui.reload_config(
                            &self.paths,
                            &mut self.config,
                            &self.vault,
                        ),
                        ConfigFileKind::Profiles => self
                            .reload_ui
                            .reload_profiles(&self.paths, &mut self.profiles),
                    };
                    if let Err(err) = res {
                        error!(file = kind.file_name(), error = %err, "TOML hot-reload failed");
                        self.last_error = Some(err.to_string());
                    }
                }
                Task::none()
            }
            Message::RestoreConfigBackup(kind) => {
                match self.reload_ui.restore_backup(
                    &self.paths,
                    kind,
                    &mut self.config,
                    &mut self.profiles,
                    &self.vault,
                ) {
                    Ok(()) => {
                        self.last_error = None;
                    }
                    Err(err) => {
                        error!(file = kind.file_name(), error = %err, "failed to restore .bak.1");
                        self.last_error = Some(err);
                    }
                }
                Task::none()
            }
            Message::DismissReloadToast(kind) => {
                self.reload_ui.dismiss_error(kind);
                Task::none()
            }
            Message::VaultUnlockInputChanged(password) => {
                self.reload_ui.set_vault_password_draft(password);
                Task::none()
            }
            Message::VaultUnlockSubmitted(password) => {
                self.apply_vault_password_op(password, VaultSession::unlock);
                Task::none()
            }
            Message::VaultInitializeSubmitted(password) => {
                self.apply_vault_password_op(password, VaultSession::initialize_encrypted);
                Task::none()
            }
            Message::VaultLock => {
                self.vault.lock();
                self.reload_ui.clear_vault_prompt();
                Task::none()
            }
            Message::VaultAutoLockTick => {
                self.vault.tick_idle();
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
            Message::OpenSshProfile(profile) => {
                match SshTargetSpec::from_profile(
                    &profile,
                    80,
                    24,
                    self.config.terminal.scrollback_lines,
                ) {
                    Ok(spec) => {
                        let (_sid, task) = self.open_ssh_session(spec, true);
                        task
                    }
                    Err(err) => {
                        self.last_error = Some(err.to_string());
                        Task::none()
                    }
                }
            }
            Message::RestoreSshProfile(profile) => {
                match SshTargetSpec::from_profile(
                    &profile,
                    80,
                    24,
                    self.config.terminal.scrollback_lines,
                ) {
                    Ok(spec) => {
                        let auto_connect = spec.auto_connect_on_startup;
                        let (_sid, task) = self.open_ssh_session(spec, auto_connect);
                        task
                    }
                    Err(err) => {
                        self.last_error = Some(err.to_string());
                        Task::none()
                    }
                }
            }
            Message::QuickConnectSsh(spec) => {
                let (_sid, task) = self.open_ssh_session(spec, true);
                task
            }
            Message::ConnectSshSession(session_id) => self.start_ssh_connect(session_id),
            Message::DisconnectSshSession(session_id) => {
                self.disconnect_ssh_session(session_id);
                Task::none()
            }
            Message::SshEvent { session_id, event } => {
                self.handle_ssh_event(session_id, event);
                Task::none()
            }
            Message::RespondHostKeyPrompt { session_id, accept } => {
                self.ssh_ui.respond_host_key(session_id, accept);
                Task::none()
            }
            Message::SshAuthInputChanged {
                session_id,
                index,
                value,
            } => {
                self.ssh_ui.set_auth_draft(session_id, index, value);
                Task::none()
            }
            Message::SubmitSshAuthPrompt(session_id) => {
                self.ssh_ui.submit_auth_prompt(session_id);
                Task::none()
            }
            Message::CancelSshAuthPrompt(session_id) => {
                self.ssh_ui.cancel_auth_prompt(session_id);
                Task::none()
            }
            Message::DismissMitmAlert(session_id) => {
                self.ssh_ui.dismiss_mitm_alert(session_id);
                Task::none()
            }
        }
    }

    fn handle_ssh_event(&mut self, session_id: SessionId, event: SshUiEvent) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let buffer = Arc::clone(&session.buffer);
        let SessionBacking::Ssh {
            generation,
            active,
            last_error,
            ..
        } = &mut session.backing
        else {
            return;
        };

        let event_gen = match &event {
            SshUiEvent::Connected { generation, .. }
            | SshUiEvent::HostKeyPrompt { generation, .. }
            | SshUiEvent::AuthPrompt { generation, .. }
            | SshUiEvent::MitmBlocked { generation, .. }
            | SshUiEvent::Disconnected { generation, .. } => *generation,
        };
        if event_gen != *generation {
            return;
        }

        match event {
            SshUiEvent::Connected { session: ssh, .. } => {
                *active = Some(ssh);
                *last_error = None;
                session.transport_status = TransportStatus::Connected { latency_ms: None };
                session.exited = None;
                session.snapshot = Arc::new(buffer.snapshot());
            }
            SshUiEvent::HostKeyPrompt {
                prompt, responder, ..
            } => {
                self.ssh_ui
                    .push_host_key_prompt(session_id, prompt, responder);
            }
            SshUiEvent::AuthPrompt {
                prompt, responder, ..
            } => {
                self.ssh_ui.push_auth_prompt(session_id, prompt, responder);
            }
            SshUiEvent::MitmBlocked { alert, .. } => {
                *active = None;
                session.transport_status = TransportStatus::Disconnected;
                let msg = format!(
                    "HOST KEY VERIFICATION FAILED for {}:{} (known_hosts line {}): expected {}, received {}",
                    alert.host,
                    alert.port,
                    alert.line,
                    alert.expected_fingerprint,
                    alert.received_fingerprint
                );
                *last_error = Some(msg.clone());
                self.last_error = Some(msg);
                self.ssh_ui.record_mitm_alert(alert);
            }
            SshUiEvent::Disconnected { error, .. } => {
                *active = None;
                session.transport_status = TransportStatus::Disconnected;
                self.ssh_ui.clear_session_prompts(session_id);
                if let Some(err) = error {
                    *last_error = Some(err.clone());
                    self.last_error = Some(err);
                }
            }
        }
    }

    /// Renders the workspace tab strip, optional Broadcast Input / reload / Vault / SSH banners,
    /// and active split panes.
    pub fn view(&self, _window_id: window::Id) -> Element<'_, Message, Theme, iced::Renderer> {
        let mut root = column![self.render_tab_strip()].width(Length::Fill).height(Length::Fill);

        if let Some(banner_text) = self.broadcast_banner_text() {
            root = root.push(render_broadcast_banner(banner_text));
        }

        if let Some(mitm) = self.ssh_ui.view_mitm_alerts() {
            root = root.push(mitm);
        }
        if let Some(tofu) = self.ssh_ui.view_tofu_modal() {
            root = root.push(tofu);
        }
        if let Some(auth_prompt) = self.ssh_ui.view_auth_prompt() {
            root = root.push(auth_prompt);
        }

        if let Some(toasts) = self.reload_ui.view_reload_toasts() {
            root = root.push(toasts);
        }
        if let Some(vault_bar) = self.reload_ui.view_vault_banner(&self.vault) {
            root = root.push(vault_bar);
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
            Some(session) => {
                let viewport = TerminalViewport::with_focus(
                    session.id,
                    Arc::clone(&session.snapshot),
                    self.config.terminal.font_size,
                    focused,
                );
                if let SessionBacking::Ssh {
                    spec, last_error, ..
                } = &session.backing
                    && session.transport_status == TransportStatus::Disconnected
                {
                    let bar = SshUiState::view_disconnected_ssh_bar(
                        session.id,
                        &spec.connect_config.host,
                        spec.connect_config.port,
                        self.ssh_ui.mitm_alert(session.id).is_some(),
                        last_error.as_deref(),
                    );
                    column![bar, viewport]
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into()
                } else {
                    viewport
                }
            }
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

    /// Subscribes to window lifecycle events, live PTY output streams, config hot-reload, and
    /// Vault auto-lock ticks.
    pub fn subscription(&self) -> Subscription<Message> {
        let close_requests = window::close_requests().map(Message::CloseRequested);
        let closed = window::close_events().map(Message::WindowClosed);
        let config_watch = Subscription::run_with(
            ConfigWatchSubKey {
                config_dir: self.paths.config_dir().to_path_buf(),
                paths: self.paths.clone(),
            },
            config_watch_stream,
        );

        let pty_subs = self.sessions.values().map(|session| {
            Subscription::run_with(
                PtySubKey {
                    session_id: session.id,
                    buffer: Arc::clone(&session.buffer),
                },
                pty_event_stream,
            )
        });

        let vault_tick = (self.vault.backend() == VaultBackend::EncryptedFile
            && self.vault.is_unlocked())
        .then(|| iced::time::every(Duration::from_secs(15)).map(|_| Message::VaultAutoLockTick));

        Subscription::batch(
            [close_requests, closed, config_watch]
                .into_iter()
                .chain(vault_tick)
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

    /// Returns the process-level [`VaultSession`].
    pub fn vault(&self) -> &Arc<VaultSession> {
        &self.vault
    }

    /// Returns active hot-reload parse-error toasts keyed by file kind.
    pub fn reload_errors(&self) -> &BTreeMap<ConfigFileKind, TomlReloadError> {
        self.reload_ui.errors()
    }

    fn apply_vault_password_op(
        &mut self,
        mut password: String,
        op: impl FnOnce(&VaultSession, &str) -> Result<(), termewnal_core::VaultError>,
    ) {
        match op(&self.vault, &password) {
            Ok(()) => {
                self.reload_ui.clear_vault_prompt();
                self.last_error = None;
            }
            Err(err) => {
                let msg = err.to_string();
                self.reload_ui.set_vault_error(Some(msg.clone()));
                self.last_error = Some(msg);
            }
        }
        password.zeroize();
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

    /// Returns the SSH TOFU, MITM, and interactive auth prompt UI state.
    pub fn ssh_ui(&self) -> &SshUiState {
        &self.ssh_ui
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
struct ConfigWatchSubKey {
    config_dir: PathBuf,
    paths: ConfigPaths,
}

impl Hash for ConfigWatchSubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.config_dir.hash(state);
    }
}

fn config_watch_stream(
    key: &ConfigWatchSubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let paths = key.paths.clone();
    Box::pin(iced::stream::channel(16, async move |mut output| {
        let Ok((watcher, mut rx)) = ConfigWatcher::start(paths) else {
            return;
        };
        let _watcher = watcher;
        while let Some(event) = rx.recv().await {
            let msg = match event {
                ConfigWatchEvent::Changed(kinds) => Message::ConfigFilesChanged(kinds),
                ConfigWatchEvent::Rescan => Message::ConfigFilesChanged(vec![
                    ConfigFileKind::AppConfig,
                    ConfigFileKind::Profiles,
                ]),
                ConfigWatchEvent::Error(err) => {
                    error!(error = %err, "config directory watcher error");
                    continue;
                }
            };
            if output.send(msg).await.is_err() {
                break;
            }
        }
    }))
}

#[derive(Clone)]
struct PtySubKey {
    session_id: SessionId,
    buffer: Arc<TerminalBuffer>,
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
    let buffer = Arc::clone(&key.buffer);
    Box::pin(iced::stream::channel(64, async move |mut output| {
        if let Some(mut rx) = buffer.take_event_receiver() {
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
