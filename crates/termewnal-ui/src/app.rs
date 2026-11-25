use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::futures::{SinkExt, Stream};
use iced::keyboard::{self, Key, Modifiers, key::Named};
use iced::widget::{
    button, column, container, operation, row, scrollable, space, stack, text,
};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Size, Subscription, Task, Theme,
    clipboard, event, window,
};
use termewnal_core::{
    AppConfig, ConfigFileKind, ConfigPaths, ConfigWatchEvent, ConfigWatcher, Profile,
    ProfileTarget, ProfilesConfig, SshAuthMethod, TerminalConfig, ThemePalette, TomlReloadError,
    VaultBackend, VaultSession, ensure_config_files,
};
use termewnal_pty::{
    GridSearchMatch, PtyEvent, PtySession, PtySpawnConfig, Scroll, SelectionType, ShellSpec, Side,
    TerminalBuffer, TerminalSnapshot, detect_default_shell,
};
use termewnal_ssh::{OsAgentConnector, SshConnectConfig, SshSession, TransportStatus};
use tracing::error;
use zeroize::Zeroize;

use crate::command_palette::{
    CommandPaletteMode, CommandPaletteState, PaletteAction, PaletteItem, PaletteSessionEntry,
    QuickConnectTarget, command_palette_input_id, quick_connect_input_id, render_command_palette,
};
use crate::config_reload_ui::ConfigReloadUiState;
use crate::sidebar::{
    ActiveTunnelEntry, SidebarSection, SidebarState, SystemSshProfile, render_sidebar,
};
use crate::split_view::{CapturedEventGate, PaneFrame, ShortcutInterceptor, SplitView};
use crate::ssh_ui::{SshTargetSpec, SshUiEvent, SshUiState, connect_ssh_task};
use crate::status_bar::{
    SessionKindLabel, SessionStatusHook, StatusBarAction, StatusBarSnapshot, StatusBarState,
    TransferSummaryHook, TunnelSummaryHook, render_status_bar,
};
use crate::terminal_ergonomics::{
    FileDropAction, PendingPaste, SearchBarState, classify_file_drop, cursor_style_to_shape,
    format_paste_bytes, iced_theme_from_name, is_multiline_paste, open_url_with_system,
    paste_confirm_modal_view, read_clipboard_via_arboard, search_bar_view, search_input_id,
};
use crate::terminal_widget::{TerminalViewOptions, TerminalViewport};
use crate::workspace::{
    ClosePaneOutcome, CycleDirection, FocusDirection, PaneId, SplitAxis, SplitId, SplitNode,
    TabId, Workspace, WorkspaceTab,
};

/// Injectable OS URL-launcher boundary (`open::that_detached` by default).
pub type UrlOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Unique identifier for a runtime terminal `Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// Placement target when launching a saved `Profile` or Quick Connect `Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaunchDisposition {
    #[default]
    NewTab,
    Split(SplitAxis),
}

/// Target metadata for an SSH `Session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSessionTarget {
    pub profile_id: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuthMethod,
    pub proxy_jump: Vec<String>,
    pub auto_connect_on_startup: bool,
    pub tmux_reattach: Option<String>,
}

impl SshSessionTarget {
    /// Returns the formatted `user@host:port` destination string.
    pub fn display_addr(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    fn from_target_spec(spec: &SshTargetSpec) -> Self {
        Self {
            profile_id: spec.profile_id.clone(),
            name: spec.title.clone(),
            host: spec.connect_config.host.clone(),
            port: spec.connect_config.port,
            username: spec.connect_config.username.clone(),
            auth: spec.connect_config.auth.clone(),
            proxy_jump: Vec::new(),
            auto_connect_on_startup: spec.auto_connect_on_startup,
            tmux_reattach: None,
        }
    }

    fn to_target_spec(
        &self,
        title: &str,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> SshTargetSpec {
        SshTargetSpec {
            profile_id: self.profile_id.clone(),
            title: title.to_string(),
            connect_config: SshConnectConfig {
                host: self.host.clone(),
                port: self.port,
                username: self.username.clone(),
                auth: self.auth.clone(),
                known_hosts_path: None,
                nodelay: true,
                term_name: "xterm-256color".to_string(),
                cols: cols.max(1),
                rows: rows.max(1),
                scrollback_lines,
            },
            agent_connector: Arc::new(OsAgentConnector::default()),
            auto_connect_on_startup: self.auto_connect_on_startup,
        }
    }
}

/// Runtime slot representing an SSH `Session` and its `russh` `Primary Transport`.
#[derive(Clone)]
pub struct SshSessionSlot {
    id: SessionId,
    title: String,
    target: SshSessionTarget,
    spec: SshTargetSpec,
    transport_status: TransportStatus,
    status_message: String,
    cols: u16,
    rows: u16,
    buffer: Arc<TerminalBuffer>,
    snapshot: Arc<TerminalSnapshot>,
    exited: Option<Option<i32>>,
    generation: u64,
    active: Option<Arc<SshSession>>,
    last_error: Option<String>,
}

impl std::fmt::Debug for SshSessionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshSessionSlot")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("target", &self.target)
            .field("transport_status", &self.transport_status)
            .field("status_message", &self.status_message)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("generation", &self.generation)
            .field("connected", &self.active.is_some())
            .finish()
    }
}

impl SshSessionSlot {
    /// Returns this SSH `Session`'s identifier.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Returns the display title of this SSH `Session`.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the SSH target configuration associated with this slot.
    pub fn target(&self) -> &SshSessionTarget {
        &self.target
    }

    /// Returns the current SSH `TransportStatus`.
    pub fn transport_status(&self) -> &TransportStatus {
        &self.transport_status
    }

    /// Returns the descriptive status message shown inside this slot.
    pub fn status_message(&self) -> &str {
        &self.status_message
    }

    /// Returns the tracked `(cols, rows)` terminal grid dimensions for this SSH slot.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Returns the shared [`TerminalBuffer`] backing this SSH `Session`.
    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
    }

    /// Returns the most recent terminal grid snapshot for this SSH `Session`.
    pub fn snapshot(&self) -> &Arc<TerminalSnapshot> {
        &self.snapshot
    }

    /// Returns `Some(exit_code)` once the remote shell channel has exited.
    pub fn exited(&self) -> Option<Option<i32>> {
        self.exited
    }

    /// Returns the current SSH connection attempt generation counter.
    pub fn ssh_generation(&self) -> u64 {
        self.generation
    }

    /// Returns the active [`SshSession`] handle if this SSH `Session` is currently connected.
    pub fn ssh_session(&self) -> Option<&Arc<SshSession>> {
        self.active.as_ref()
    }

    /// Returns the [`SshTargetSpec`] used to connect this SSH `Session`.
    pub fn spec(&self) -> &SshTargetSpec {
        &self.spec
    }

    /// Returns the last connection/MITM error recorded on this SSH `Session`, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

/// Runtime state of an active local shell `Session`.
pub struct LocalSession {
    id: SessionId,
    profile_id: Option<String>,
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

    /// Returns the originating saved `Profile` identifier, if launched from a `Profile`.
    pub fn profile_id(&self) -> Option<&str> {
        self.profile_id.as_deref()
    }

    /// Returns the display title of this `Session`.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns a shared handle to the backing [`PtySession`].
    pub fn pty(&self) -> &Arc<PtySession> {
        &self.pty
    }

    /// Returns the shared [`TerminalBuffer`] backing this `Session`.
    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        self.pty.buffer()
    }

    /// Returns the most recent terminal grid snapshot.
    pub fn snapshot(&self) -> &Arc<TerminalSnapshot> {
        &self.snapshot
    }

    /// Returns the `TransportStatus` (`Disconnected` for local shell sessions).
    pub fn transport_status(&self) -> &TransportStatus {
        &self.transport_status
    }

    /// Returns `Some(exit_code)` once the underlying shell process has exited.
    pub fn exited(&self) -> Option<Option<i32>> {
        self.exited
    }
}

enum SessionRecord {
    Local(LocalSession),
    Ssh(SshSessionSlot),
}

impl SessionRecord {
    fn title(&self) -> &str {
        match self {
            Self::Local(s) => &s.title,
            Self::Ssh(s) => &s.title,
        }
    }

    fn exited(&self) -> Option<Option<i32>> {
        match self {
            Self::Local(s) => s.exited,
            Self::Ssh(s) => s.exited,
        }
    }

    fn buffer(&self) -> &Arc<TerminalBuffer> {
        match self {
            Self::Local(s) => s.pty.buffer(),
            Self::Ssh(s) => &s.buffer,
        }
    }

    fn refresh_snapshot(&mut self) {
        match self {
            Self::Local(s) => s.snapshot = Arc::new(s.pty.snapshot()),
            Self::Ssh(s) => s.snapshot = Arc::new(s.buffer.snapshot()),
        }
    }

    fn as_ssh(&self) -> Option<&SshSessionSlot> {
        match self {
            Self::Local(_) => None,
            Self::Ssh(s) => Some(s),
        }
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
    ToggleSidebar,
    SetSidebarOpen(bool),
    SidebarFilterChanged(String),
    SidebarToggleTagFilter(String),
    SidebarToggleGroup(String),
    SidebarToggleSection(SidebarSection),
    SetSystemSshProfiles(Vec<SystemSshProfile>),
    SetActiveTunnels(Vec<ActiveTunnelEntry>),
    LaunchProfile {
        profile_id: String,
        disposition: LaunchDisposition,
    },
    LaunchSystemSsh {
        alias: String,
        disposition: LaunchDisposition,
    },
    OpenSshProfile(Profile),
    RestoreSshProfile(Profile),
    QuickConnectSsh(SshTargetSpec),
    SetSessionTransportStatus {
        session_id: SessionId,
        status: TransportStatus,
    },
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
    OpenCommandPalette,
    OpenQuickConnectPalette,
    CloseCommandPalette,
    ToggleCommandPalette,
    CommandPaletteQueryChanged(String),
    CommandPaletteSelectNext,
    CommandPaletteSelectPrevious,
    CommandPaletteConfirm,
    CommandPaletteExecuteItem(PaletteAction),
    CommandPaletteEnterQuickConnect,
    CommandPaletteExitQuickConnect,
    CommandPaletteQuickConnectInputChanged(String),
    QuickConnect(QuickConnectTarget),
    SetTunnelSummary(TunnelSummaryHook),
    SetTransferSummary(TransferSummaryHook),
    StatusBarAction(StatusBarAction),
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
    sessions: BTreeMap<SessionId, SessionRecord>,
    workspace: Workspace,
    sidebar: SidebarState,
    command_palette: CommandPaletteState,
    status_bar: StatusBarState,
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
            sidebar: SidebarState::new(),
            command_palette: CommandPaletteState::new(),
            status_bar: StatusBarState::new(),
            next_session_id: 1,
            last_error: None,
            search: SearchBarState::default(),
            pending_paste: None,
            last_copied: None,
            url_opener: Arc::new(open_url_with_system),
        };

        let initial_session = state.spawn_default_local_session()?;
        state.workspace.create_tab(initial_session, None);
        Ok(state)
    }

    /// Overrides the OS URL opener boundary (used by tests to avoid spawning a desktop browser).
    pub fn with_url_opener(mut self, opener: UrlOpener) -> Self {
        self.url_opener = opener;
        self
    }

    fn spawn_default_local_session(&mut self) -> Result<SessionId, String> {
        let shell = self.resolve_shell_spec(None, &[]);
        self.spawn_local_session(shell, None, None, None)
    }

    fn resolve_shell_spec(&self, profile_shell: Option<&str>, args: &[String]) -> ShellSpec {
        let mut spec = profile_shell
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| ShellSpec::custom(s, args.to_vec()))
            .or_else(|| {
                self.config
                    .terminal
                    .shell
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| ShellSpec::custom(s, args.to_vec()))
            })
            .unwrap_or_else(detect_default_shell);

        if spec.args.is_empty() && !args.is_empty() {
            spec.args = args.to_vec();
        }
        spec
    }

    fn spawn_local_session(
        &mut self,
        shell: ShellSpec,
        working_dir: Option<PathBuf>,
        title_override: Option<String>,
        profile_id: Option<String>,
    ) -> Result<SessionId, String> {
        let spawn_cfg = PtySpawnConfig {
            shell: shell.clone(),
            cols: 80,
            rows: 24,
            scrollback_lines: self.config.terminal.scrollback_lines,
            cursor_shape: cursor_style_to_shape(self.config.terminal.cursor_style),
            working_dir,
        };

        let pty = Arc::new(
            PtySession::spawn(spawn_cfg).map_err(|err| format!("failed to spawn PTY: {err}"))?,
        );
        let snapshot = Arc::new(pty.snapshot());
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;

        let title = title_override
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or(shell.display_name);

        let session = LocalSession {
            id,
            profile_id,
            title,
            pty,
            snapshot,
            transport_status: TransportStatus::Disconnected,
            exited: None,
        };

        self.sessions.insert(id, SessionRecord::Local(session));
        Ok(id)
    }

    /// Creates a new SSH `Session` tab from `spec`, connecting immediately when `auto_connect` is
    /// `true` or waiting in `Disconnected` state until user action otherwise.
    pub fn open_ssh_session(
        &mut self,
        spec: SshTargetSpec,
        auto_connect: bool,
    ) -> (SessionId, Task<Message>) {
        let target = SshSessionTarget::from_target_spec(&spec);
        let tab_title = (!spec.title.trim().is_empty()).then(|| spec.title.clone());
        let id = self.insert_ssh_session_slot(target, spec);
        self.workspace.create_tab(id, tab_title);

        if auto_connect {
            let task = self.start_ssh_connect(id);
            (id, task)
        } else {
            (id, Task::none())
        }
    }

    fn insert_ssh_session_slot(
        &mut self,
        target: SshSessionTarget,
        spec: SshTargetSpec,
    ) -> SessionId {
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        let title = if target.name.trim().is_empty() {
            target.display_addr()
        } else {
            target.name.clone()
        };
        let cols = spec.connect_config.cols.max(1);
        let rows = spec.connect_config.rows.max(1);
        let buffer = Arc::new(TerminalBuffer::new_with_cursor_shape(
            cols,
            rows,
            spec.connect_config.scrollback_lines,
            cursor_style_to_shape(self.config.terminal.cursor_style),
        ));
        let snapshot = Arc::new(buffer.snapshot());
        let status = TransportStatus::Disconnected;
        let status_message = format_ssh_slot_message(&target, &status);
        let slot = SshSessionSlot {
            id,
            title,
            target,
            spec,
            transport_status: status,
            status_message,
            cols,
            rows,
            buffer,
            snapshot,
            exited: None,
            generation: 0,
            active: None,
            last_error: None,
        };
        self.sessions.insert(id, SessionRecord::Ssh(slot));
        id
    }

    fn start_ssh_connect(&mut self, session_id: SessionId) -> Task<Message> {
        self.ssh_ui.clear_session_prompts(session_id);
        let Some(SessionRecord::Ssh(slot)) = self.sessions.get_mut(&session_id) else {
            return Task::none();
        };

        if let Some(prev) = slot.active.take() {
            prev.disconnect();
        }
        slot.generation = slot.generation.wrapping_add(1);
        slot.last_error = None;
        slot.exited = None;

        let (cols, rows) = slot.buffer.dimensions();
        slot.cols = cols;
        slot.rows = rows;
        slot.spec.connect_config.cols = cols;
        slot.spec.connect_config.rows = rows;
        slot.transport_status = TransportStatus::Connecting;
        slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);

        connect_ssh_task(
            session_id,
            slot.generation,
            slot.spec.clone(),
            Arc::clone(&slot.buffer),
            Arc::clone(&self.vault),
        )
    }

    fn disconnect_ssh_session(&mut self, session_id: SessionId) {
        self.ssh_ui.clear_session_prompts(session_id);
        if let Some(SessionRecord::Ssh(slot)) = self.sessions.get_mut(&session_id) {
            if let Some(ssh) = slot.active.take() {
                ssh.disconnect();
            }
            slot.transport_status = TransportStatus::Disconnected;
            slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
        }
    }

    fn open_session_with_disposition(
        &mut self,
        session_id: SessionId,
        tab_title: Option<String>,
        disposition: LaunchDisposition,
    ) -> bool {
        match disposition {
            LaunchDisposition::NewTab => {
                self.workspace.create_tab(session_id, tab_title);
                true
            }
            LaunchDisposition::Split(axis) => {
                if let Some((tab_id, target_pane)) = self
                    .workspace
                    .active_tab()
                    .map(|tab| (tab.id, tab.focused_pane))
                {
                    if self
                        .workspace
                        .split_pane(tab_id, target_pane, axis, session_id)
                        .is_none()
                    {
                        self.remove_session(session_id);
                        false
                    } else {
                        true
                    }
                } else {
                    self.workspace.create_tab(session_id, tab_title);
                    true
                }
            }
        }
    }

    fn launch_profile_entry(&mut self, profile: &Profile, disposition: LaunchDisposition) -> Task<Message> {
        match &profile.target {
            ProfileTarget::LocalShell {
                shell,
                working_dir,
                args,
            } => {
                let spec = self.resolve_shell_spec(shell.as_deref(), args);
                let title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                match self.spawn_local_session(
                    spec,
                    working_dir.clone(),
                    title.clone(),
                    Some(profile.id.clone()),
                ) {
                    Ok(sid) => {
                        let _ = self.open_session_with_disposition(sid, title, disposition);
                    }
                    Err(err) => {
                        error!(profile_id = %profile.id, error = %err, "failed to launch local Profile");
                        self.last_error = Some(err);
                    }
                }
                Task::none()
            }
            ProfileTarget::Ssh {
                host,
                port,
                username,
                auth,
                proxy_jump,
                auto_connect_on_startup,
                tmux_reattach,
            } => {
                let target = SshSessionTarget {
                    profile_id: Some(profile.id.clone()),
                    name: profile.name.clone(),
                    host: host.clone(),
                    port: *port,
                    username: username.clone(),
                    auth: auth.clone(),
                    proxy_jump: proxy_jump.clone(),
                    auto_connect_on_startup: *auto_connect_on_startup,
                    tmux_reattach: tmux_reattach.clone(),
                };
                let tab_title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                let spec = target.to_target_spec(
                    &profile.name,
                    80,
                    24,
                    self.config.terminal.scrollback_lines,
                );
                let sid = self.insert_ssh_session_slot(target, spec);
                if self.open_session_with_disposition(sid, tab_title, disposition) {
                    self.start_ssh_connect(sid)
                } else {
                    Task::none()
                }
            }
        }
    }

    fn launch_quick_connect(&mut self, target: QuickConnectTarget) -> Task<Message> {
        let label = target.display_addr();
        let ssh_target = SshSessionTarget {
            profile_id: None,
            name: label.clone(),
            host: target.host,
            port: target.port,
            username: target.username,
            auth: SshAuthMethod::Agent,
            proxy_jump: Vec::new(),
            auto_connect_on_startup: false,
            tmux_reattach: None,
        };
        let spec = ssh_target.to_target_spec(
            &label,
            80,
            24,
            self.config.terminal.scrollback_lines,
        );
        let sid = self.insert_ssh_session_slot(ssh_target, spec);
        if self.open_session_with_disposition(sid, Some(label), LaunchDisposition::NewTab) {
            self.start_ssh_connect(sid)
        } else {
            Task::none()
        }
    }

    fn launch_system_ssh_entry(&mut self, alias: &str, disposition: LaunchDisposition) -> Task<Message> {
        let Some(host) = self
            .sidebar
            .system_ssh_profiles()
            .iter()
            .find(|h| h.alias == alias)
            .cloned()
        else {
            return Task::none();
        };
        let target = SshSessionTarget {
            profile_id: None,
            name: host.alias.clone(),
            host: host.host_name.unwrap_or_else(|| host.alias.clone()),
            port: host.port,
            username: host.user.unwrap_or_else(default_os_username),
            auth: SshAuthMethod::Agent,
            proxy_jump: host.proxy_jump,
            auto_connect_on_startup: false,
            tmux_reattach: None,
        };
        let spec = target.to_target_spec(
            &host.alias,
            80,
            24,
            self.config.terminal.scrollback_lines,
        );
        let sid = self.insert_ssh_session_slot(target, spec);
        if self.open_session_with_disposition(sid, Some(host.alias), disposition) {
            self.start_ssh_connect(sid)
        } else {
            Task::none()
        }
    }

    fn handle_command_palette_confirm(&mut self) -> Task<Message> {
        match self.command_palette.mode() {
            CommandPaletteMode::QuickConnect => {
                if let Some(target) =
                    QuickConnectTarget::parse(self.command_palette.quick_connect_input())
                {
                    self.last_error = None;
                    self.command_palette.close();
                    self.launch_quick_connect(target)
                } else {
                    self.last_error =
                        Some("Invalid Quick Connect target (expected user@host[:port])".to_string());
                    Task::none()
                }
            }
            CommandPaletteMode::Search => {
                let items = self.command_palette_items();
                if items.is_empty() {
                    return Task::none();
                }
                let idx = self.command_palette.selected_index().min(items.len() - 1);
                let action = items[idx].action.clone();
                self.execute_palette_action(action)
            }
        }
    }

    fn handle_status_bar_action(&mut self, action: StatusBarAction) -> Task<Message> {
        match action {
            StatusBarAction::ToggleSidebar => {
                self.sidebar.toggle();
                Task::none()
            }
            StatusBarAction::OpenCommandPalette => self.update(Message::OpenCommandPalette),
            StatusBarAction::ReconnectSession(session_id) => self.start_ssh_connect(session_id),
            StatusBarAction::CancelReconnect(session_id) => {
                self.disconnect_ssh_session(session_id);
                Task::none()
            }
            StatusBarAction::ToggleTunnelsView => {
                self.sidebar.set_open(true);
                self.sidebar
                    .set_section_collapsed(SidebarSection::ActiveTunnels, false);
                Task::none()
            }
            StatusBarAction::ToggleTransferDrawer => {
                self.status_bar.toggle_transfer_drawer();
                Task::none()
            }
        }
    }

    fn execute_palette_action(&mut self, action: PaletteAction) -> Task<Message> {
        match action {
            PaletteAction::OpenQuickConnectPrompt => {
                self.command_palette.enter_quick_connect("");
                operation::focus(quick_connect_input_id())
            }
            PaletteAction::NewTab => {
                self.command_palette.close();
                self.update(Message::NewTab)
            }
            PaletteAction::SplitPane(axis) => {
                self.command_palette.close();
                self.update(Message::SplitFocusedPane(axis))
            }
            PaletteAction::TogglePaneZoom => {
                self.command_palette.close();
                self.update(Message::TogglePaneZoom)
            }
            PaletteAction::ToggleBroadcastInput => {
                self.command_palette.close();
                self.update(Message::ToggleBroadcastInput)
            }
            PaletteAction::ToggleSidebar => {
                self.command_palette.close();
                self.update(Message::ToggleSidebar)
            }
            PaletteAction::CloseFocusedPane => {
                self.command_palette.close();
                self.update(Message::CloseFocusedPane)
            }
            PaletteAction::QuickConnect(target) => {
                self.command_palette.close();
                self.launch_quick_connect(target)
            }
            PaletteAction::SwitchSession(session_id) => {
                self.command_palette.close();
                self.update(Message::FocusSession(session_id))
            }
            PaletteAction::LaunchProfile {
                profile_id,
                disposition,
            } => {
                self.command_palette.close();
                self.update(Message::LaunchProfile {
                    profile_id,
                    disposition,
                })
            }
            PaletteAction::LaunchSystemSsh { alias, disposition } => {
                self.command_palette.close();
                self.update(Message::LaunchSystemSsh { alias, disposition })
            }
            PaletteAction::ToggleTunnel(tunnel_id) => {
                self.command_palette.close();
                let _ = tunnel_id;
                Task::none()
            }
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        self.ssh_ui.clear_session_prompts(session_id);
        match self.sessions.remove(&session_id) {
            Some(SessionRecord::Local(session)) => session.pty.terminate(),
            Some(SessionRecord::Ssh(slot)) => {
                if let Some(ssh) = slot.active {
                    ssh.disconnect();
                }
            }
            None => {}
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
                if targets.len() == 1
                    && (bytes.contains(&b'\r') || bytes.contains(&b'\n'))
                    && let Some(SessionRecord::Ssh(slot)) = self.sessions.get(&session_id)
                    && slot.active.is_none()
                    && slot.transport_status == TransportStatus::Disconnected
                {
                    return self.start_ssh_connect(session_id);
                }
                self.dispatch_bytes_with_broadcast(session_id, &bytes);
                Task::none()
            }
            Message::ViewportResized {
                session_id,
                cols,
                rows,
            } => {
                match self.sessions.get_mut(&session_id) {
                    Some(SessionRecord::Local(session)) => {
                        if let Err(err) = session.pty.resize(cols, rows) {
                            error!(session = session_id.0, error = %err, "failed to resize PTY");
                            self.last_error = Some(err.to_string());
                        } else {
                            session.snapshot = Arc::new(session.pty.snapshot());
                        }
                    }
                    Some(SessionRecord::Ssh(slot)) => {
                        slot.cols = cols.max(1);
                        slot.rows = rows.max(1);
                        if let Some(ssh) = &slot.active {
                            if let Err(err) = ssh.resize(cols, rows) {
                                error!(
                                    session = session_id.0,
                                    error = %err,
                                    "failed to resize SSH channel"
                                );
                                self.last_error = Some(err.to_string());
                            }
                        } else {
                            slot.buffer.resize(cols, rows);
                        }
                        slot.snapshot = Arc::new(slot.buffer.snapshot());
                    }
                    None => {}
                }
                Task::none()
            }
            Message::PtyEvent { session_id, event } => {
                let is_active_session = Some(session_id) == self.active_session_id();
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    match event {
                        PtyEvent::Output => {
                            record.refresh_snapshot();
                            if is_active_session
                                && self.search.open
                                && !self.search.query.is_empty()
                            {
                                let buf = Arc::clone(record.buffer());
                                self.search.refresh_matches_buffer(Some(buf.as_ref()), false);
                            }
                        }
                        PtyEvent::Title(title) => {
                            if !title.is_empty() {
                                match record {
                                    SessionRecord::Local(s) => s.title = title,
                                    SessionRecord::Ssh(s) => s.title = title,
                                }
                            }
                        }
                        PtyEvent::Bell => {}
                        PtyEvent::Exited(code) => {
                            record.refresh_snapshot();
                            match record {
                                SessionRecord::Local(s) => s.exited = Some(code),
                                SessionRecord::Ssh(slot) => {
                                    slot.exited = Some(code);
                                    slot.active = None;
                                    slot.transport_status = TransportStatus::Disconnected;
                                    slot.status_message =
                                        format_ssh_slot_message(&slot.target, &slot.transport_status);
                                }
                            }
                        }
                    }
                }
                Task::none()
            }
            Message::ApplyTerminalConfig(cfg) => {
                self.config.terminal = cfg;
                self.sync_terminal_options_to_sessions();
                Task::none()
            }
            Message::OpenSearch => {
                let active_buf = self
                    .active_session_id()
                    .and_then(|id| self.sessions.get(&id))
                    .map(|r| Arc::clone(r.buffer()));
                let target = self
                    .search
                    .open_buffer(active_buf.as_deref());
                self.focus_search_match(target);
                operation::focus(search_input_id())
            }
            Message::CloseSearch => {
                self.search.close();
                Task::none()
            }
            Message::SearchQueryChanged(query) => {
                let active_buf = self
                    .active_session_id()
                    .and_then(|id| self.sessions.get(&id))
                    .map(|r| Arc::clone(r.buffer()));
                let target = self
                    .search
                    .set_query_buffer(query, active_buf.as_deref());
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
                if self.command_palette.is_open() {
                    self.command_palette.close();
                    return Task::none();
                }
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
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    record.buffer().scroll_display(Scroll::Delta(delta_lines));
                    record.refresh_snapshot();
                }
                Task::none()
            }
            Message::ScrollToTop(session_id) => {
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    record.buffer().scroll_display(Scroll::Top);
                    record.refresh_snapshot();
                }
                Task::none()
            }
            Message::ScrollToBottom(session_id) => {
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    record.buffer().scroll_to_bottom();
                    record.refresh_snapshot();
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
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    record
                        .buffer()
                        .start_selection(col, row, side, selection_type);
                    record.refresh_snapshot();
                }
                Task::none()
            }
            Message::SelectionUpdate {
                session_id,
                col,
                row,
                side,
            } => {
                if let Some(record) = self.sessions.get_mut(&session_id) {
                    record.buffer().update_selection(col, row, side);
                    record.refresh_snapshot();
                }
                Task::none()
            }
            Message::SelectionFinish { session_id } => {
                let copied = if let Some(record) = self.sessions.get_mut(&session_id) {
                    let selected = record.buffer().finish_selection();
                    record.refresh_snapshot();
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
                    .and_then(|record| record.buffer().selection_text());
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
                let is_ssh = self
                    .sessions
                    .get(&session_id)
                    .and_then(SessionRecord::as_ssh)
                    .is_some();
                let action = classify_file_drop(session_id, &path, is_ssh, shift_held, false);
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
                    self.dispatch_bytes_with_broadcast(session_id, text.as_bytes());
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
                        ConfigFileKind::AppConfig => {
                            let reloaded = self.reload_ui.reload_config(
                                &self.paths,
                                &mut self.config,
                                &self.vault,
                            );
                            if matches!(reloaded, Ok(true)) {
                                self.sync_terminal_options_to_sessions();
                            }
                            reloaded
                        }
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
                        if kind == ConfigFileKind::AppConfig {
                            self.sync_terminal_options_to_sessions();
                        }
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
            Message::ToggleSidebar => {
                self.sidebar.toggle();
                Task::none()
            }
            Message::SetSidebarOpen(open) => {
                self.sidebar.set_open(open);
                Task::none()
            }
            Message::SidebarFilterChanged(query) => {
                self.sidebar.set_filter_query(query);
                Task::none()
            }
            Message::SidebarToggleTagFilter(tag) => {
                self.sidebar.toggle_tag_filter(&tag);
                Task::none()
            }
            Message::SidebarToggleGroup(group_name) => {
                self.sidebar.toggle_group_collapsed(&group_name);
                Task::none()
            }
            Message::SidebarToggleSection(section) => {
                self.sidebar.toggle_section(section);
                Task::none()
            }
            Message::SetSystemSshProfiles(entries) => {
                self.sidebar.set_system_ssh_profiles(entries);
                Task::none()
            }
            Message::SetActiveTunnels(entries) => {
                self.status_bar
                    .set_tunnels(TunnelSummaryHook::from_entries(&entries));
                self.sidebar.set_active_tunnels(entries);
                Task::none()
            }
            Message::LaunchProfile {
                profile_id,
                disposition,
            } => {
                self.command_palette.close();
                if let Some(profile) = self
                    .profiles
                    .profiles
                    .iter()
                    .find(|p| p.id == profile_id)
                    .cloned()
                {
                    self.launch_profile_entry(&profile, disposition)
                } else {
                    self.last_error = Some(format!("unknown profile id: {profile_id}"));
                    Task::none()
                }
            }
            Message::LaunchSystemSsh { alias, disposition } => {
                self.command_palette.close();
                self.launch_system_ssh_entry(&alias, disposition)
            }
            Message::SetSessionTransportStatus { session_id, status } => {
                self.update_ssh_slot_status(session_id, status);
                Task::none()
            }
            Message::ConnectSshSession(session_id) => self.start_ssh_connect(session_id),
            Message::DisconnectSshSession(session_id) => {
                self.disconnect_ssh_session(session_id);
                Task::none()
            }
            Message::OpenCommandPalette => {
                self.command_palette.open();
                operation::focus(command_palette_input_id())
            }
            Message::OpenQuickConnectPalette => {
                self.command_palette.open_quick_connect();
                operation::focus(quick_connect_input_id())
            }
            Message::CloseCommandPalette => {
                self.command_palette.close();
                Task::none()
            }
            Message::ToggleCommandPalette => {
                if self.command_palette.toggle() {
                    operation::focus(command_palette_input_id())
                } else {
                    Task::none()
                }
            }
            Message::CommandPaletteQueryChanged(query) => {
                self.command_palette.set_query(query);
                Task::none()
            }
            Message::CommandPaletteSelectNext => {
                let count = self.command_palette_items().len();
                self.command_palette.select_next(count);
                Task::none()
            }
            Message::CommandPaletteSelectPrevious => {
                let count = self.command_palette_items().len();
                self.command_palette.select_previous(count);
                Task::none()
            }
            Message::CommandPaletteEnterQuickConnect => {
                self.command_palette.enter_quick_connect("");
                operation::focus(quick_connect_input_id())
            }
            Message::CommandPaletteExitQuickConnect => {
                self.command_palette.exit_quick_connect();
                operation::focus(command_palette_input_id())
            }
            Message::CommandPaletteQuickConnectInputChanged(input) => {
                self.command_palette.set_quick_connect_input(input);
                Task::none()
            }
            Message::CommandPaletteConfirm => self.handle_command_palette_confirm(),
            Message::CommandPaletteExecuteItem(action) => self.execute_palette_action(action),
            Message::QuickConnect(target) => {
                self.command_palette.close();
                self.launch_quick_connect(target)
            }
            Message::SetTunnelSummary(summary) => {
                self.status_bar.set_tunnels(summary);
                Task::none()
            }
            Message::SetTransferSummary(summary) => {
                self.status_bar.set_transfers(summary);
                Task::none()
            }
            Message::StatusBarAction(action) => self.handle_status_bar_action(action),
        }
    }

    fn handle_ssh_event(&mut self, session_id: SessionId, event: SshUiEvent) {
        let Some(SessionRecord::Ssh(slot)) = self.sessions.get_mut(&session_id) else {
            return;
        };

        let event_gen = match &event {
            SshUiEvent::Connected { generation, .. }
            | SshUiEvent::HostKeyPrompt { generation, .. }
            | SshUiEvent::AuthPrompt { generation, .. }
            | SshUiEvent::MitmBlocked { generation, .. }
            | SshUiEvent::Disconnected { generation, .. } => *generation,
        };
        if event_gen != slot.generation {
            return;
        }

        match event {
            SshUiEvent::Connected { session: ssh, .. } => {
                slot.active = Some(ssh);
                slot.last_error = None;
                slot.exited = None;
                slot.transport_status = TransportStatus::Connected { latency_ms: None };
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                slot.snapshot = Arc::new(slot.buffer.snapshot());
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
                slot.active = None;
                slot.transport_status = TransportStatus::Disconnected;
                let msg = format!(
                    "HOST KEY VERIFICATION FAILED for {}:{} (known_hosts line {}): expected {}, received {}",
                    alert.host,
                    alert.port,
                    alert.line,
                    alert.expected_fingerprint,
                    alert.received_fingerprint
                );
                slot.last_error = Some(msg.clone());
                slot.status_message = msg.clone();
                self.last_error = Some(msg);
                self.ssh_ui.record_mitm_alert(alert);
            }
            SshUiEvent::Disconnected { error, .. } => {
                slot.active = None;
                slot.transport_status = TransportStatus::Disconnected;
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                self.ssh_ui.clear_session_prompts(session_id);
                if let Some(err) = error {
                    slot.last_error = Some(err.clone());
                    slot.status_message = err.clone();
                    self.last_error = Some(err);
                }
            }
        }
    }

    fn sync_terminal_options_to_sessions(&mut self) {
        let shape = cursor_style_to_shape(self.config.terminal.cursor_style);
        let scrollback = self.config.terminal.scrollback_lines;
        for record in self.sessions.values_mut() {
            record.buffer().apply_options(scrollback, shape);
            record.refresh_snapshot();
        }
        if self.search.open && !self.search.query.is_empty() {
            let active_buf = self
                .active_session_id()
                .and_then(|id| self.sessions.get(&id))
                .map(|r| Arc::clone(r.buffer()));
            self.search
                .refresh_matches_buffer(active_buf.as_deref(), false);
        }
    }

    fn focus_search_match(&mut self, target: Option<GridSearchMatch>) {
        if let Some(matched) = target
            && let Some(record) = self
                .active_session_id()
                .and_then(|id| self.sessions.get_mut(&id))
        {
            record.buffer().scroll_to_match(matched);
            record.refresh_snapshot();
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
        let Some(record) = self.sessions.get(&session_id) else {
            return;
        };
        let bytes = format_paste_bytes(text, record.buffer().is_bracketed_paste());
        self.write_bytes_to_session(session_id, &bytes);
    }

    fn dispatch_bytes_with_broadcast(&mut self, session_id: SessionId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let targets = self.workspace.input_targets_for_session(session_id);
        let is_broadcast = targets.len() > 1;
        for target_id in targets {
            if is_broadcast {
                let skip = match self.sessions.get(&target_id) {
                    Some(SessionRecord::Local(s)) => s.exited.is_some(),
                    Some(SessionRecord::Ssh(s)) => s.active.is_none(),
                    None => true,
                };
                if skip {
                    continue;
                }
            }
            self.write_bytes_to_session(target_id, bytes);
        }
    }

    fn write_bytes_to_session(&mut self, session_id: SessionId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let write_result = match record {
            SessionRecord::Local(session) => session.pty.write_bytes(bytes).map_err(|e| e.to_string()),
            SessionRecord::Ssh(slot) => {
                if let Some(ssh) = &slot.active {
                    ssh.write_bytes(bytes).map_err(|e| e.to_string())
                } else {
                    return;
                }
            }
        };
        if let Err(err) = write_result {
            error!(session = session_id.0, error = %err, "failed to write terminal input");
            self.last_error = Some(err);
        } else if record.buffer().display_offset() > 0 {
            record.buffer().scroll_to_bottom();
            record.refresh_snapshot();
        }
    }

    /// Renders the workspace tab strip, collapsible Left Sidebar, status/reload/Vault/SSH banners,
    /// active split panes, Status Bar, Search/Paste overlays, and Command Palette modal.
    pub fn view(&self, _window_id: window::Id) -> Element<'_, Message, Theme, iced::Renderer> {
        let mut main_pane_col = column![self.render_tab_strip()]
            .width(Length::Fill)
            .height(Length::Fill);

        if let Some(banner_text) = self.broadcast_banner_text() {
            main_pane_col = main_pane_col.push(render_broadcast_banner(banner_text));
        }

        if let Some(mitm) = self.ssh_ui.view_mitm_alerts() {
            main_pane_col = main_pane_col.push(mitm);
        }
        if let Some(tofu) = self.ssh_ui.view_tofu_modal() {
            main_pane_col = main_pane_col.push(tofu);
        }
        if let Some(auth_prompt) = self.ssh_ui.view_auth_prompt() {
            main_pane_col = main_pane_col.push(auth_prompt);
        }

        if let Some(toasts) = self.reload_ui.view_reload_toasts() {
            main_pane_col = main_pane_col.push(toasts);
        }
        if let Some(vault_bar) = self.reload_ui.view_vault_banner(&self.vault) {
            main_pane_col = main_pane_col.push(vault_bar);
        }

        let body = match self.workspace.active_tab() {
            Some(tab) => self.render_tab_body(tab),
            None => self.render_empty_workspace(),
        };
        main_pane_col = main_pane_col.push(body);

        let workspace_row: Element<'_, Message, Theme, iced::Renderer> = if self.sidebar.is_open() {
            row![
                render_sidebar(&self.sidebar, &self.profiles.profiles),
                CapturedEventGate::new(main_pane_col.into()),
            ]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
        } else {
            main_pane_col.into()
        };

        let status_snapshot = self.status_bar_snapshot();
        let base_layout: Element<'_, Message, Theme, iced::Renderer> = column![
            workspace_row,
            render_status_bar(&status_snapshot),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

        let palette = ThemePalette::for_name(&self.config.terminal.theme);
        let mut layers = stack![base_layout]
            .width(Length::Fill)
            .height(Length::Fill);
        if self.search.open {
            layers = layers.push(search_bar_view(&self.search, &palette));
        }
        if let Some(pending) = &self.pending_paste {
            layers = layers.push(paste_confirm_modal_view(pending, &palette));
        }
        if self.command_palette.is_open() {
            let items = self.command_palette_items();
            layers = layers.push(render_command_palette(
                &self.command_palette,
                items,
                self.last_error(),
            ));
        }

        ShortcutInterceptor::with_palette(
            layers.into(),
            self.active_tab_id(),
            self.command_palette.is_open(),
        )
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
            button(text("Palette").size(12))
                .padding([4, 8])
                .style(button::subtle)
                .on_press(Message::ToggleCommandPalette),
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

        let sidebar_btn = button(text("Profiles").size(12))
            .padding([4, 8])
            .style(if self.sidebar.is_open() {
                button::primary
            } else {
                button::subtle
            })
            .on_press(Message::ToggleSidebar);

        let bar = row![sidebar_btn, tabs_scroll, actions]
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
        let terminal_focused = focused && !self.command_palette.is_open();
        let ime_enabled = !self.command_palette.is_open()
            && self.pending_paste.is_none()
            && !(self.search.open && self.search.input_focused);
        let options = TerminalViewOptions::from_config(&self.config.terminal, ime_enabled);
        let is_active_session = Some(session_id) == self.active_session_id();
        let (matches, active_match): (Arc<[GridSearchMatch]>, Option<usize>) = if is_active_session
        {
            (
                Arc::from(self.search.matches.as_slice()),
                self.search.active_index,
            )
        } else {
            (Arc::from([]), None)
        };

        let content = match self.sessions.get(&session_id) {
            Some(SessionRecord::Local(session)) => TerminalViewport::with_focus(
                session.id,
                Arc::clone(&session.snapshot),
                options,
                matches,
                active_match,
                terminal_focused,
            ),
            Some(SessionRecord::Ssh(slot)) => {
                let viewport = TerminalViewport::with_focus(
                    slot.id,
                    Arc::clone(&slot.snapshot),
                    options,
                    matches,
                    active_match,
                    terminal_focused,
                );
                if slot.transport_status == TransportStatus::Disconnected {
                    let bar = SshUiState::view_disconnected_ssh_bar(
                        slot.id,
                        &slot.target.host,
                        slot.target.port,
                        self.ssh_ui.mitm_alert(slot.id).is_some(),
                        slot.last_error.as_deref(),
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

        PaneFrame::new(pane_id, terminal_focused, broadcast, show_border, content)
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
                    .map(SessionRecord::title)
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
        if let Some(record) = self.active_session_id().and_then(|id| self.sessions.get(&id)) {
            let session_title = record.title();
            let mut title = match record.exited() {
                Some(Some(code)) => format!("Termewnal — {session_title} [exited: {code}]"),
                Some(None) => format!("Termewnal — {session_title} [exited]"),
                None => format!("Termewnal — {session_title}"),
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
        iced_theme_from_name(&self.config.terminal.theme)
    }

    /// Subscribes to window lifecycle events, live PTY output streams, config hot-reload, and
    /// Vault auto-lock ticks.
    pub fn subscription(&self) -> Subscription<Message> {
        let close_requests = window::close_requests().map(Message::CloseRequested);
        let closed = window::close_events().map(Message::WindowClosed);
        let shortcuts = event::listen_with(window_shortcut_event);
        let config_watch = Subscription::run_with(
            ConfigWatchSubKey {
                config_dir: self.paths.config_dir().to_path_buf(),
                paths: self.paths.clone(),
            },
            config_watch_stream,
        );

        let pty_subs = self.sessions.iter().map(|(&session_id, record)| {
            Subscription::run_with(
                PtySubKey {
                    session_id,
                    buffer: Arc::clone(record.buffer()),
                },
                pty_event_stream,
            )
        });

        let vault_tick = (self.vault.backend() == VaultBackend::EncryptedFile
            && self.vault.is_unlocked())
        .then(|| iced::time::every(Duration::from_secs(15)).map(|_| Message::VaultAutoLockTick));

        Subscription::batch(
            [close_requests, closed, shortcuts, config_watch]
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

    /// Returns the collapsible Left Sidebar state.
    pub fn sidebar(&self) -> &SidebarState {
        &self.sidebar
    }

    /// Returns the Command Palette modal state.
    pub fn command_palette(&self) -> &CommandPaletteState {
        &self.command_palette
    }

    /// Computes an inspectable [`StatusBarSnapshot`] from the active `Session`, `Tunnel`, and `Transfer` state.
    pub fn status_bar_snapshot(&self) -> StatusBarSnapshot {
        let active_tab = self.workspace.active_tab();
        let zoomed = active_tab.is_some_and(|tab| tab.zoomed_pane.is_some());
        let broadcast = active_tab.is_some_and(|tab| tab.broadcast_input);
        let active_session = active_tab
            .and_then(|tab| tab.focused_session_id())
            .and_then(|sid| self.sessions.get(&sid))
            .map(|record| match record {
                SessionRecord::Local(s) => {
                    let (cols, rows) = (s.snapshot.cols, s.snapshot.rows);
                    SessionStatusHook {
                        session_id: s.id,
                        kind: SessionKindLabel::Local,
                        title: s.title.clone(),
                        target: s.pty.shell().display_name.clone(),
                        cols,
                        rows,
                        exited: s.exited,
                        transport_status: s.transport_status.clone(),
                        zoomed,
                        broadcast,
                    }
                }
                SessionRecord::Ssh(slot) => SessionStatusHook {
                    session_id: slot.id,
                    kind: SessionKindLabel::Ssh,
                    title: slot.title.clone(),
                    target: slot.target.display_addr(),
                    cols: slot.snapshot.cols,
                    rows: slot.snapshot.rows,
                    exited: slot.exited,
                    transport_status: slot.transport_status.clone(),
                    zoomed,
                    broadcast,
                },
            });

        StatusBarSnapshot {
            active_session,
            tunnels: self.status_bar.tunnels().clone(),
            transfers: self.status_bar.transfers().clone(),
            sidebar_open: self.sidebar.is_open(),
            transfer_drawer_open: self.status_bar.transfers().drawer_open,
            last_error: self.last_error.clone(),
        }
    }

    /// Builds and fuzzy-filters the Command Palette entries for the current `AppState`.
    pub fn command_palette_items(&self) -> Vec<PaletteItem> {
        let active_sid = self.active_session_id();
        let mut sessions = Vec::new();
        for tab in self.workspace.tabs() {
            for sid in tab.root.session_ids() {
                if let Some(record) = self.sessions.get(&sid) {
                    let target_label = match record {
                        SessionRecord::Local(s) => {
                            format!("Local · {}", s.pty.shell().display_name)
                        }
                        SessionRecord::Ssh(slot) => {
                            format!(
                                "SSH · {} ({})",
                                slot.target.display_addr(),
                                slot.transport_status.label()
                            )
                        }
                    };
                    sessions.push(PaletteSessionEntry {
                        session_id: sid,
                        title: record.title().to_string(),
                        target_label,
                        is_active: active_sid == Some(sid),
                    });
                }
            }
        }
        self.command_palette.filtered_items(
            &self.profiles.profiles,
            &sessions,
            self.sidebar.system_ssh_profiles(),
            self.sidebar.active_tunnels(),
        )
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

    /// Returns an immutable reference to a runtime local shell `Session` by `id`.
    pub fn session(&self, id: SessionId) -> Option<&LocalSession> {
        match self.sessions.get(&id) {
            Some(SessionRecord::Local(session)) => Some(session),
            _ => None,
        }
    }

    /// Returns an immutable reference to an SSH `Session` slot by `id`.
    pub fn ssh_session(&self, id: SessionId) -> Option<&SshSessionSlot> {
        match self.sessions.get(&id) {
            Some(SessionRecord::Ssh(slot)) => Some(slot),
            _ => None,
        }
    }

    fn update_ssh_slot_status(&mut self, session_id: SessionId, status: TransportStatus) {
        if let Some(SessionRecord::Ssh(slot)) = self.sessions.get_mut(&session_id) {
            slot.status_message = format_ssh_slot_message(&slot.target, &status);
            slot.transport_status = status;
        }
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
            Key::Character("p") | Key::Character("P") => Some(Message::ToggleCommandPalette),
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
        return match key.as_ref() {
            Key::Named(Named::Tab) => Some(Message::CycleTab(CycleDirection::Next)),
            Key::Character("b") | Key::Character("B") => Some(Message::ToggleSidebar),
            Key::Character("k") | Key::Character("K") => Some(Message::ToggleCommandPalette),
            _ => None,
        };
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

/// Maps key events to workspace or Command Palette modal navigation [`Message`]s.
pub fn shortcut_for_key_event_with_palette(
    key: &Key,
    modifiers: Modifiers,
    active_tab: Option<TabId>,
    palette_open: bool,
) -> Option<Message> {
    if palette_open {
        if let Some(msg) = shortcut_for_key_event(key, modifiers, active_tab)
            && matches!(
                msg,
                Message::ToggleCommandPalette | Message::ToggleSidebar
            )
        {
            return Some(msg);
        }
        if modifiers.is_empty() {
            return match key {
                Key::Named(Named::Escape) => Some(Message::CloseCommandPalette),
                Key::Named(Named::ArrowDown) => Some(Message::CommandPaletteSelectNext),
                Key::Named(Named::ArrowUp) => Some(Message::CommandPaletteSelectPrevious),
                Key::Named(Named::Enter) => Some(Message::CommandPaletteConfirm),
                _ => None,
            };
        }
        return None;
    }

    shortcut_for_key_event(key, modifiers, active_tab)
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

pub(crate) fn default_os_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string())
}

fn format_ssh_slot_message(target: &SshSessionTarget, status: &TransportStatus) -> String {
    let addr = target.display_addr();
    match status {
        TransportStatus::Disconnected => format!("Disconnected ({addr})"),
        TransportStatus::Connecting => format!("Connecting to {addr}..."),
        TransportStatus::Connected {
            latency_ms: Some(ms),
        } => {
            format!("Connected to {addr} ({ms} ms)")
        }
        TransportStatus::Connected { latency_ms: None } => format!("Connected to {addr}"),
        TransportStatus::Degraded { missed_probes } => {
            format!("Transport degraded ({missed_probes} missed keepalive probes)")
        }
        TransportStatus::Reconnecting {
            attempt,
            next_retry_secs,
        } => format!("Reconnecting attempt #{attempt} in {next_retry_secs}s..."),
    }
}
