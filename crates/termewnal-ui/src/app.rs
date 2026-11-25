use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
    AppConfig, CURRENT_SCHEMA_VERSION, ConfigFileKind, ConfigPaths, ConfigWatchEvent,
    ConfigWatcher, LayoutSplitAxis, LayoutSplitNode, Profile, ProfileTarget, ProfilesConfig,
    SessionDescriptor, SshAuthMethod, TabLayout, TerminalConfig, ThemePalette, TomlReloadError,
    VaultBackend, VaultSession, WindowLayout, WorkspaceLayout, ensure_config_files, load_layout,
    save_layout,
};
use termewnal_pty::{
    GridSearchMatch, PtyEvent, PtySession, PtySpawnConfig, Scroll, SelectionType, ShellSpec, Side,
    TerminalSnapshot, detect_default_shell,
};
use termewnal_ssh::TransportStatus;
use tokio::sync::mpsc;
use tracing::{error, warn};
use zeroize::Zeroize;

use crate::close_guard::{
    BackgroundWorkSource, BackgroundWorkTracker, CloseGuardState, DaemonLifecycle,
    wrap_with_close_guard_modal,
};
use crate::command_palette::{
    CommandPaletteMode, CommandPaletteState, PaletteAction, PaletteItem, PaletteSessionEntry,
    QuickConnectTarget, command_palette_input_id, quick_connect_input_id, render_command_palette,
};
use crate::config_reload_ui::ConfigReloadUiState;
use crate::ipc::{IpcCommand, IpcServerHandle};
use crate::sidebar::{
    ActiveTunnelEntry, SidebarSection, SidebarState, SystemSshProfile, render_sidebar,
};
use crate::split_view::{CapturedEventGate, PaneFrame, ShortcutInterceptor, SplitView};
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
    TabId, Workspace, WorkspaceIds, WorkspaceTab,
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

/// Target metadata for an SSH `Session` slot ready for Ticket 07 `russh` `Primary Transport` wiring.
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
}

/// Runtime slot representing an SSH `Session` before/during `russh` `Primary Transport` attachment.
#[derive(Debug, Clone)]
pub struct SshSessionSlot {
    id: SessionId,
    title: String,
    target: SshSessionTarget,
    transport_status: TransportStatus,
    status_message: String,
    cols: u16,
    rows: u16,
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
}

/// Runtime state of an active local shell `Session`.
pub struct LocalSession {
    id: SessionId,
    profile_id: Option<String>,
    initial_working_dir: Option<PathBuf>,
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

/// Runtime descriptor for a restored or configured remote SSH `Session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSession {
    pub id: SessionId,
    pub profile_id: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub title: String,
    pub transport_status: TransportStatus,
    pub auto_connect_on_startup: bool,
    pub connect_requested: bool,
}

/// Connection intent emitted for a remote SSH `Session` (consumed by SSH supervisor wiring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConnectIntent {
    pub session_id: SessionId,
    pub profile_id: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
}

enum SessionEntry {
    Local(LocalSession),
    Remote(RemoteSession),
    Ssh(SshSessionSlot),
}

impl SessionEntry {
    fn title(&self) -> &str {
        match self {
            Self::Local(s) => &s.title,
            Self::Remote(s) => &s.title,
            Self::Ssh(s) => &s.title,
        }
    }

    fn as_local(&self) -> Option<&LocalSession> {
        match self {
            Self::Local(s) => Some(s),
            Self::Remote(_) | Self::Ssh(_) => None,
        }
    }

    fn as_local_mut(&mut self) -> Option<&mut LocalSession> {
        match self {
            Self::Local(s) => Some(s),
            Self::Remote(_) | Self::Ssh(_) => None,
        }
    }

    fn as_remote(&self) -> Option<&RemoteSession> {
        match self {
            Self::Remote(s) => Some(s),
            Self::Local(_) | Self::Ssh(_) => None,
        }
    }
}

/// Top-level Elm messages driving `AppState`.
#[derive(Debug, Clone)]
pub enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    WindowFocused(window::Id),
    WindowResized(window::Id, Size),
    CloseRequested(window::Id),
    NewWindow,
    IpcCommand(IpcCommand),
    InWindow {
        window_id: window::Id,
        inner: Box<Message>,
    },
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
    CloseGuardCancel,
    CloseGuardKeepRunningInBackground,
    CloseGuardStopAndQuit,
    ConnectRemoteSession(SessionId),
    SaveWorkspaceLayout,
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
    SetSessionTransportStatus {
        session_id: SessionId,
        status: TransportStatus,
    },
    ConnectSshSession(SessionId),
    DisconnectSshSession(SessionId),
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

struct WindowState {
    workspace: Workspace,
    width: u32,
    height: u32,
}

/// Top-level Elm application state for Termewnal.
pub struct AppState {
    paths: ConfigPaths,
    config: AppConfig,
    profiles: ProfilesConfig,
    vault: Arc<VaultSession>,
    reload_ui: ConfigReloadUiState,
    windows: BTreeSet<window::Id>,
    window_workspaces: BTreeMap<window::Id, WindowState>,
    pending_windows: VecDeque<WindowState>,
    retired_final_window: Option<(window::Id, WindowState)>,
    empty_workspace: Workspace,
    active_window: Option<window::Id>,
    workspace_ids: WorkspaceIds,
    sessions: BTreeMap<SessionId, SessionEntry>,
    pending_remote_connects: Vec<RemoteConnectIntent>,
    sidebar: SidebarState,
    command_palette: CommandPaletteState,
    status_bar: StatusBarState,
    next_session_id: u64,
    last_error: Option<String>,
    background_work_tracker: Arc<BackgroundWorkTracker>,
    background_work_source: Arc<dyn BackgroundWorkSource>,
    close_guard: CloseGuardState,
    lifecycle: DaemonLifecycle,
    ipc_handle: Option<IpcServerHandle>,
    ipc_rx: Option<Arc<Mutex<Option<mpsc::UnboundedReceiver<IpcCommand>>>>>,
    search: SearchBarState,
    pending_paste: Option<PendingPaste>,
    last_copied: Option<String>,
    url_opener: UrlOpener,
}

impl AppState {
    /// Boots the desktop daemon with XDG paths and opens the initial GUI window(s).
    pub fn boot_daemon() -> (Self, Task<Message>) {
        let paths = ConfigPaths::detect_xdg()
            .unwrap_or_else(|_| ConfigPaths::from_root(std::env::temp_dir().join("termewnal")));
        Self::boot_daemon_with_ipc(paths, None)
    }

    /// Boots the desktop daemon with explicit `paths` and an optional primary [`IpcServerHandle`].
    pub fn boot_daemon_with_ipc(
        paths: ConfigPaths,
        ipc_handle: Option<IpcServerHandle>,
    ) -> (Self, Task<Message>) {
        let mut state = Self::boot_with_paths(paths).unwrap_or_else(|err| {
            error!(error = %err, "failed to boot AppState");
            panic!("failed to initialize Termewnal: {err}");
        });
        if let Some(handle) = ipc_handle {
            state.attach_ipc(handle);
        }

        let mut tasks = state.open_pending_windows();

        for intent in &state.pending_remote_connects {
            tasks.push(Task::done(Message::ConnectRemoteSession(intent.session_id)));
        }

        (state, Task::batch(tasks))
    }

    fn open_pending_windows(&mut self) -> Vec<Task<Message>> {
        let mut tasks = Vec::new();
        while let Some(win_state) = self.pending_windows.pop_front() {
            let (win_id, open_task) = window::open(window::Settings {
                exit_on_close_request: false,
                size: Size::new(win_state.width as f32, win_state.height as f32),
                ..window::Settings::default()
            });
            self.windows.insert(win_id);
            self.window_workspaces.insert(win_id, win_state);
            self.active_window = Some(win_id);
            tasks.push(open_task.map(Message::WindowOpened));
        }
        tasks
    }

    fn close_window_sessions(&mut self, win_state: WindowState) {
        for sid in win_state.workspace.session_ids() {
            self.remove_session(sid);
        }
    }

    /// Initializes configuration files under `paths` and restores `layout.toml` (or spawns a
    /// default local shell `Session` when no persisted layout exists).
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
        let tracker = Arc::new(BackgroundWorkTracker::new());
        let source: Arc<dyn BackgroundWorkSource> =
            Arc::clone(&tracker) as Arc<dyn BackgroundWorkSource>;
        let workspace_ids = WorkspaceIds::new();
        let mut state = Self {
            paths,
            config,
            profiles,
            vault,
            reload_ui: ConfigReloadUiState::default(),
            windows: BTreeSet::new(),
            window_workspaces: BTreeMap::new(),
            pending_windows: VecDeque::new(),
            retired_final_window: None,
            empty_workspace: Workspace::with_ids(workspace_ids.clone()),
            active_window: None,
            workspace_ids,
            sessions: BTreeMap::new(),
            pending_remote_connects: Vec::new(),
            sidebar: SidebarState::new(),
            command_palette: CommandPaletteState::new(),
            status_bar: StatusBarState::new(),
            next_session_id: 1,
            last_error: None,
            background_work_tracker: tracker,
            background_work_source: source,
            close_guard: CloseGuardState::Idle,
            lifecycle: DaemonLifecycle::Active,
            ipc_handle: None,
            ipc_rx: None,
            search: SearchBarState::default(),
            pending_paste: None,
            last_copied: None,
            url_opener: Arc::new(open_url_with_system),
        };

        let restored = match load_layout(&state.paths) {
            Ok(layout) if !layout.windows.is_empty() => state.restore_from_layout(&layout)?,
            Ok(_) => false,
            Err(err) => {
                warn!(error = %err, "failed to load saved layout.toml; starting fresh workspace");
                state.last_error = Some(format!("failed to restore workspace layout: {err}"));
                false
            }
        };

        if !restored {
            state.push_default_pending_window()?;
        }
        Ok(state)
    }

    /// Overrides the OS URL opener boundary (used by tests to avoid spawning a desktop browser).
    pub fn with_url_opener(mut self, opener: UrlOpener) -> Self {
        self.url_opener = opener;
        self
    }

    fn push_default_pending_window(&mut self) -> Result<(), String> {
        let mut workspace = Workspace::with_ids(self.workspace_ids.clone());
        let initial_session = self.spawn_default_local_session()?;
        workspace.create_tab(initial_session, None);
        self.pending_windows.push_back(WindowState {
            workspace,
            width: self.config.window.width,
            height: self.config.window.height,
        });
        Ok(())
    }

    fn restore_from_layout(&mut self, layout: &WorkspaceLayout) -> Result<bool, String> {
        let mut restored_any = false;
        for win_layout in &layout.windows {
            if win_layout.tabs.is_empty() {
                continue;
            }
            let mut workspace = Workspace::with_ids(self.workspace_ids.clone());
            for tab_layout in &win_layout.tabs {
                let mut leaf_panes = Vec::new();
                let root = self.restore_split_node(&tab_layout.root, &mut leaf_panes)?;
                let Some(&first_pane) = leaf_panes.first() else {
                    continue;
                };
                let focused_pane = leaf_panes
                    .get(tab_layout.focused_pane)
                    .copied()
                    .unwrap_or(first_pane);
                let zoomed_pane = tab_layout
                    .zoomed_pane
                    .and_then(|idx| leaf_panes.get(idx).copied());
                workspace.restore_tab(tab_layout.title.clone(), root, focused_pane, zoomed_pane);
            }
            if workspace.tab_count() == 0 {
                continue;
            }
            workspace.set_active_tab_index(win_layout.active_tab);
            self.pending_windows.push_back(WindowState {
                workspace,
                width: win_layout.width.max(200),
                height: win_layout.height.max(150),
            });
            restored_any = true;
        }
        Ok(restored_any)
    }

    fn restore_split_node(
        &mut self,
        node: &LayoutSplitNode,
        leaf_panes: &mut Vec<PaneId>,
    ) -> Result<SplitNode, String> {
        match node {
            LayoutSplitNode::Leaf { session } => {
                let session_id = self.restore_session_descriptor(session)?;
                let pane_id = self.workspace_ids.alloc_pane_id();
                leaf_panes.push(pane_id);
                Ok(SplitNode::Leaf {
                    pane_id,
                    session_id,
                })
            }
            LayoutSplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first_node = self.restore_split_node(first, leaf_panes)?;
                let second_node = self.restore_split_node(second, leaf_panes)?;
                let split_id = self.workspace_ids.alloc_split_id();
                let ui_axis = match axis {
                    LayoutSplitAxis::Horizontal => SplitAxis::Horizontal,
                    LayoutSplitAxis::Vertical => SplitAxis::Vertical,
                };
                Ok(SplitNode::Split {
                    split_id,
                    axis: ui_axis,
                    ratio: ratio.clamp(crate::workspace::MIN_SPLIT_RATIO, crate::workspace::MAX_SPLIT_RATIO),
                    first: Box::new(first_node),
                    second: Box::new(second_node),
                })
            }
        }
    }

    fn restore_session_descriptor(&mut self, desc: &SessionDescriptor) -> Result<SessionId, String> {
        match desc {
            SessionDescriptor::Local {
                profile_id,
                title,
                working_dir,
                shell,
            } => {
                let spec = self.resolve_shell_spec(shell.as_deref(), &[]);
                let valid_cwd = working_dir.as_ref().filter(|dir| dir.is_dir()).cloned();
                self.spawn_local_session(
                    spec,
                    valid_cwd,
                    title.clone(),
                    profile_id.clone(),
                )
            }
            SessionDescriptor::Remote {
                profile_id,
                host,
                port,
                username,
                title,
                auto_connect_on_startup,
            } => {
                let profile_auto = profile_id
                    .as_deref()
                    .and_then(|pid| self.profiles.profiles.iter().find(|p| p.id == pid))
                    .map(|p| p.auto_connect_on_startup());
                let effective_auto = profile_auto.unwrap_or(*auto_connect_on_startup);
                Ok(self.insert_remote_session(
                    profile_id.clone(),
                    host.clone(),
                    *port,
                    username.clone(),
                    title.clone(),
                    effective_auto,
                ))
            }
        }
    }

    fn insert_remote_session(
        &mut self,
        profile_id: Option<String>,
        host: String,
        port: u16,
        username: Option<String>,
        title: Option<String>,
        auto_connect_on_startup: bool,
    ) -> SessionId {
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        let display_title = title
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| match &username {
                Some(u) if !u.is_empty() => format!("{u}@{host}"),
                _ => host.clone(),
            });

        let connect_requested = auto_connect_on_startup;
        if connect_requested {
            self.pending_remote_connects.push(RemoteConnectIntent {
                session_id: id,
                profile_id: profile_id.clone(),
                host: host.clone(),
                port,
                username: username.clone(),
            });
        }

        self.sessions.insert(
            id,
            SessionEntry::Remote(RemoteSession {
                id,
                profile_id,
                host,
                port,
                username,
                title: display_title,
                transport_status: TransportStatus::Disconnected,
                auto_connect_on_startup,
                connect_requested,
            }),
        );
        id
    }

    /// Wires an active primary [`IpcServerHandle`] so incoming secondary launches drive `AppState`.
    pub fn attach_ipc(&mut self, handle: IpcServerHandle) {
        self.ipc_rx = handle
            .take_receiver()
            .map(|rx| Arc::new(Mutex::new(Some(rx))));
        self.ipc_handle = Some(handle);
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
            working_dir: working_dir.clone(),
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
            initial_working_dir: working_dir,
            title,
            pty,
            snapshot,
            transport_status: TransportStatus::Disconnected,
            exited: None,
        };

        self.sessions.insert(id, SessionEntry::Local(session));
        Ok(id)
    }

    fn insert_ssh_session_slot(
        &mut self,
        target: SshSessionTarget,
        status: TransportStatus,
    ) -> SessionId {
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        let title = if target.name.trim().is_empty() {
            target.display_addr()
        } else {
            target.name.clone()
        };
        let status_message = format_ssh_slot_message(&target, &status);
        let slot = SshSessionSlot {
            id,
            title,
            target,
            transport_status: status,
            status_message,
            cols: 80,
            rows: 24,
        };
        self.sessions.insert(id, SessionEntry::Ssh(slot));
        id
    }

    fn open_session_with_disposition(
        &mut self,
        window_scope: Option<window::Id>,
        session_id: SessionId,
        tab_title: Option<String>,
        disposition: LaunchDisposition,
    ) {
        let Some(ws) = self.target_workspace_mut(window_scope) else {
            self.remove_session(session_id);
            return;
        };
        match disposition {
            LaunchDisposition::NewTab => {
                ws.create_tab(session_id, tab_title);
            }
            LaunchDisposition::Split(axis) => {
                if let Some((tab_id, target_pane)) =
                    ws.active_tab().map(|tab| (tab.id, tab.focused_pane))
                {
                    if ws.split_pane(tab_id, target_pane, axis, session_id).is_none() {
                        self.remove_session(session_id);
                    }
                } else {
                    ws.create_tab(session_id, tab_title);
                }
            }
        }
    }

    fn launch_profile_entry(
        &mut self,
        window_scope: Option<window::Id>,
        profile: &Profile,
        disposition: LaunchDisposition,
    ) {
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
                        self.open_session_with_disposition(window_scope, sid, title, disposition)
                    }
                    Err(err) => {
                        error!(profile_id = %profile.id, error = %err, "failed to launch local Profile");
                        self.last_error = Some(err);
                    }
                }
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
                let sid = self.insert_ssh_session_slot(target, TransportStatus::Connecting);
                let tab_title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                self.open_session_with_disposition(window_scope, sid, tab_title, disposition);
            }
        }
    }

    fn launch_quick_connect(
        &mut self,
        window_scope: Option<window::Id>,
        target: QuickConnectTarget,
    ) {
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
        let sid = self.insert_ssh_session_slot(ssh_target, TransportStatus::Connecting);
        self.open_session_with_disposition(
            window_scope,
            sid,
            Some(label),
            LaunchDisposition::NewTab,
        );
    }

    fn launch_system_ssh_entry(
        &mut self,
        window_scope: Option<window::Id>,
        alias: &str,
        disposition: LaunchDisposition,
    ) {
        let Some(host) = self
            .sidebar
            .system_ssh_profiles()
            .iter()
            .find(|h| h.alias == alias)
            .cloned()
        else {
            return;
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
        let sid = self.insert_ssh_session_slot(target, TransportStatus::Connecting);
        self.open_session_with_disposition(
            window_scope,
            sid,
            Some(host.alias),
            disposition,
        );
    }

    fn handle_command_palette_confirm(&mut self, window_scope: Option<window::Id>) -> Task<Message> {
        match self.command_palette.mode() {
            CommandPaletteMode::QuickConnect => {
                if let Some(target) =
                    QuickConnectTarget::parse(self.command_palette.quick_connect_input())
                {
                    self.last_error = None;
                    self.command_palette.close();
                    self.launch_quick_connect(window_scope, target);
                } else {
                    self.last_error =
                        Some("Invalid Quick Connect target (expected user@host[:port])".to_string());
                }
                Task::none()
            }
            CommandPaletteMode::Search => {
                let items = self.command_palette_items();
                if items.is_empty() {
                    return Task::none();
                }
                let idx = self.command_palette.selected_index().min(items.len() - 1);
                let action = items[idx].action.clone();
                self.execute_palette_action(window_scope, action)
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
            StatusBarAction::ReconnectSession(session_id) => {
                self.update_ssh_slot_status(session_id, TransportStatus::Connecting);
                Task::none()
            }
            StatusBarAction::CancelReconnect(session_id) => {
                self.update_ssh_slot_status(session_id, TransportStatus::Disconnected);
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

    fn execute_palette_action(
        &mut self,
        window_scope: Option<window::Id>,
        action: PaletteAction,
    ) -> Task<Message> {
        match action {
            PaletteAction::OpenQuickConnectPrompt => {
                self.command_palette.enter_quick_connect("");
                operation::focus(quick_connect_input_id())
            }
            PaletteAction::NewTab => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::NewTab)
            }
            PaletteAction::SplitPane(axis) => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::SplitFocusedPane(axis))
            }
            PaletteAction::TogglePaneZoom => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::TogglePaneZoom)
            }
            PaletteAction::ToggleBroadcastInput => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::ToggleBroadcastInput)
            }
            PaletteAction::ToggleSidebar => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::ToggleSidebar)
            }
            PaletteAction::CloseFocusedPane => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::CloseFocusedPane)
            }
            PaletteAction::QuickConnect(target) => {
                self.command_palette.close();
                self.launch_quick_connect(window_scope, target);
                Task::none()
            }
            PaletteAction::SwitchSession(session_id) => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::FocusSession(session_id))
            }
            PaletteAction::LaunchProfile {
                profile_id,
                disposition,
            } => {
                self.command_palette.close();
                self.update_in_window(
                    window_scope,
                    Message::LaunchProfile {
                        profile_id,
                        disposition,
                    },
                )
            }
            PaletteAction::LaunchSystemSsh { alias, disposition } => {
                self.command_palette.close();
                self.update_in_window(
                    window_scope,
                    Message::LaunchSystemSsh { alias, disposition },
                )
            }
            PaletteAction::ToggleTunnel(tunnel_id) => {
                self.command_palette.close();
                let _ = tunnel_id;
                Task::none()
            }
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        if let Some(SessionEntry::Local(session)) = self.sessions.remove(&session_id) {
            session.pty.terminate();
        }
    }

    fn split_target_pane(
        &mut self,
        window_scope: Option<window::Id>,
        tab_id: TabId,
        target_pane: PaneId,
        axis: SplitAxis,
    ) {
        match self.spawn_default_local_session() {
            Ok(session_id) => {
                let inserted = self
                    .workspace_for_tab_mut(window_scope, tab_id)
                    .and_then(|ws| ws.split_pane(tab_id, target_pane, axis, session_id))
                    .is_some();
                if !inserted {
                    self.remove_session(session_id);
                }
            }
            Err(err) => {
                error!(error = %err, "failed to spawn session for split pane");
                self.last_error = Some(err);
            }
        }
    }

    fn close_target_pane(
        &mut self,
        window_scope: Option<window::Id>,
        tab_id: TabId,
        pane_id: PaneId,
    ) {
        if let Some(outcome) = self
            .workspace_for_tab_mut(window_scope, tab_id)
            .and_then(|ws| ws.close_pane(tab_id, pane_id))
        {
            let sid = match outcome {
                ClosePaneOutcome::ClosedPane { session_id, .. }
                | ClosePaneOutcome::ClosedTab { session_id } => session_id,
            };
            self.remove_session(sid);
        }
    }

    fn target_window_id(&self, window_scope: Option<window::Id>) -> Option<window::Id> {
        if let Some(wid) = window_scope
            && self.window_workspaces.contains_key(&wid)
        {
            return Some(wid);
        }
        if let Some(wid) = self.active_window
            && self.window_workspaces.contains_key(&wid)
        {
            return Some(wid);
        }
        self.window_workspaces.keys().next().copied()
    }

    fn primary_workspace(&self) -> Option<&Workspace> {
        if let Some(wid) = self.target_window_id(None) {
            return self.window_workspaces.get(&wid).map(|w| &w.workspace);
        }
        self.pending_windows
            .front()
            .map(|w| &w.workspace)
            .or_else(|| self.retired_final_window.as_ref().map(|(_, w)| &w.workspace))
    }

    fn target_workspace(&self, window_scope: Option<window::Id>) -> Option<&Workspace> {
        if let Some(wid) = self.target_window_id(window_scope) {
            return self.window_workspaces.get(&wid).map(|w| &w.workspace);
        }
        self.pending_windows.front().map(|w| &w.workspace)
    }

    fn target_workspace_mut(&mut self, window_scope: Option<window::Id>) -> Option<&mut Workspace> {
        if let Some(wid) = self.target_window_id(window_scope) {
            return self.window_workspaces.get_mut(&wid).map(|w| &mut w.workspace);
        }
        self.pending_windows.front_mut().map(|w| &mut w.workspace)
    }

    fn workspace_for_tab_mut(
        &mut self,
        window_scope: Option<window::Id>,
        tab_id: TabId,
    ) -> Option<&mut Workspace> {
        if let Some(wid) = self
            .window_workspaces
            .iter()
            .find_map(|(&wid, w)| w.workspace.contains_tab(tab_id).then_some(wid))
        {
            self.active_window = Some(wid);
            return self.window_workspaces.get_mut(&wid).map(|w| &mut w.workspace);
        }
        if let Some(idx) = self
            .pending_windows
            .iter()
            .position(|w| w.workspace.contains_tab(tab_id))
        {
            return self.pending_windows.get_mut(idx).map(|w| &mut w.workspace);
        }
        self.target_workspace_mut(window_scope)
    }

    fn workspace_for_split_mut(&mut self, split_id: SplitId) -> Option<&mut Workspace> {
        if let Some(wid) = self
            .window_workspaces
            .iter()
            .find_map(|(&wid, w)| w.workspace.contains_split(split_id).then_some(wid))
        {
            return self.window_workspaces.get_mut(&wid).map(|w| &mut w.workspace);
        }
        self.pending_windows
            .iter_mut()
            .find(|w| w.workspace.contains_split(split_id))
            .map(|w| &mut w.workspace)
    }

    fn workspace_for_pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Workspace> {
        if let Some(wid) = self
            .window_workspaces
            .iter()
            .find_map(|(&wid, w)| w.workspace.contains_pane(pane_id).then_some(wid))
        {
            self.active_window = Some(wid);
            return self.window_workspaces.get_mut(&wid).map(|w| &mut w.workspace);
        }
        self.pending_windows
            .iter_mut()
            .find(|w| w.workspace.contains_pane(pane_id))
            .map(|w| &mut w.workspace)
    }

    fn workspace_for_session_mut(&mut self, session_id: SessionId) -> Option<&mut Workspace> {
        if let Some(wid) = self
            .window_workspaces
            .iter()
            .find_map(|(&wid, w)| w.workspace.contains_session(session_id).then_some(wid))
        {
            return self.window_workspaces.get_mut(&wid).map(|w| &mut w.workspace);
        }
        self.pending_windows
            .iter_mut()
            .find(|w| w.workspace.contains_session(session_id))
            .map(|w| &mut w.workspace)
    }

    fn open_coordinated_window(&mut self) -> Task<Message> {
        let mut workspace = Workspace::with_ids(self.workspace_ids.clone());
        match self.spawn_default_local_session() {
            Ok(session_id) => {
                workspace.create_tab(session_id, None);
            }
            Err(err) => {
                error!(error = %err, "failed to spawn session for new window");
                self.last_error = Some(err);
            }
        }
        let width = self.config.window.width;
        let height = self.config.window.height;
        let (win_id, open_task) = window::open(window::Settings {
            exit_on_close_request: false,
            size: Size::new(width as f32, height as f32),
            ..window::Settings::default()
        });
        self.windows.insert(win_id);
        self.window_workspaces.insert(
            win_id,
            WindowState {
                workspace,
                width,
                height,
            },
        );
        self.active_window = Some(win_id);
        self.lifecycle = DaemonLifecycle::Active;
        open_task.map(Message::WindowOpened)
    }

    fn open_tab_in_window(
        &mut self,
        window_scope: Option<window::Id>,
        profile_id: Option<&str>,
    ) -> Task<Message> {
        let target_win = self.target_window_id(window_scope);
        if target_win.is_none() && self.pending_windows.is_empty() {
            let task = self.open_coordinated_window();
            if profile_id.is_none() {
                return task;
            }
            let new_win = self.active_window;
            return Task::batch([task, self.spawn_profile_tab(new_win, profile_id)]);
        }
        let extra = self.spawn_profile_tab(target_win, profile_id);
        if let Some(wid) = target_win {
            self.active_window = Some(wid);
            Task::batch([window::gain_focus(wid), extra])
        } else {
            extra
        }
    }

    fn spawn_profile_tab(
        &mut self,
        target_win: Option<window::Id>,
        profile_id: Option<&str>,
    ) -> Task<Message> {
        let profile = profile_id
            .and_then(|pid| self.profiles.profiles.iter().find(|p| p.id == pid))
            .cloned();

        let spawned = match profile {
            Some(prof) => match prof.target {
                ProfileTarget::LocalShell {
                    shell,
                    working_dir,
                    args,
                } => {
                    let spec = self.resolve_shell_spec(shell.as_deref(), &args);
                    let valid_cwd = working_dir.filter(|d| d.is_dir());
                    self.spawn_local_session(spec, valid_cwd, Some(prof.name), Some(prof.id))
                        .map(|sid| (sid, None))
                }
                ProfileTarget::Ssh {
                    host,
                    port,
                    username,
                    auto_connect_on_startup,
                    ..
                } => {
                    let sid = self.insert_remote_session(
                        Some(prof.id),
                        host,
                        port,
                        Some(username),
                        Some(prof.name),
                        auto_connect_on_startup,
                    );
                    Ok((sid, Some(sid)))
                }
            },
            None => self.spawn_default_local_session().map(|sid| (sid, None)),
        };

        match spawned {
            Ok((session_id, auto_remote)) => {
                if let Some(ws) = self.target_workspace_mut(target_win) {
                    ws.create_tab(session_id, None);
                } else {
                    self.remove_session(session_id);
                }
                if let Some(remote_sid) = auto_remote {
                    return Task::done(Message::ConnectRemoteSession(remote_sid));
                }
            }
            Err(err) => {
                error!(error = %err, "failed to create new tab session");
                self.last_error = Some(err);
            }
        }
        Task::none()
    }

    /// Processes a top-level Elm [`Message`] and returns any follow-up [`Task`].
    pub fn update(&mut self, message: Message) -> Task<Message> {
        self.update_in_window(None, message)
    }

    fn update_in_window(
        &mut self,
        window_scope: Option<window::Id>,
        message: Message,
    ) -> Task<Message> {
        match message {
            Message::InWindow { window_id, inner } => {
                self.active_window = Some(window_id);
                self.update_in_window(Some(window_id), *inner)
            }
            Message::WindowOpened(id) => {
                self.windows.insert(id);
                self.retired_final_window = None;
                self.lifecycle = DaemonLifecycle::Active;
                self.active_window = Some(id);
                if !self.window_workspaces.contains_key(&id) {
                    let win_state = if let Some(pending) = self.pending_windows.pop_front() {
                        pending
                    } else {
                        let mut workspace = Workspace::with_ids(self.workspace_ids.clone());
                        match self.spawn_default_local_session() {
                            Ok(session_id) => {
                                workspace.create_tab(session_id, None);
                            }
                            Err(err) => {
                                error!(error = %err, "failed to spawn session for opened window");
                                self.last_error = Some(err);
                            }
                        }
                        WindowState {
                            workspace,
                            width: self.config.window.width,
                            height: self.config.window.height,
                        }
                    };
                    self.window_workspaces.insert(id, win_state);
                }
                Task::none()
            }
            Message::WindowFocused(id) => {
                if self.windows.contains(&id) {
                    self.active_window = Some(id);
                }
                Task::none()
            }
            Message::WindowResized(id, size) => {
                if let Some(win_state) = self.window_workspaces.get_mut(&id) {
                    win_state.width = size.width.max(100.0).round() as u32;
                    win_state.height = size.height.max(100.0).round() as u32;
                }
                Task::none()
            }
            Message::NewWindow => self.open_coordinated_window(),
            Message::IpcCommand(cmd) => match cmd {
                IpcCommand::OpenWindow => {
                    if self.windows.is_empty()
                        && self.lifecycle == DaemonLifecycle::BackgroundRunning
                        && let Ok(layout) = load_layout(&self.paths)
                        && !layout.windows.is_empty()
                        && self.restore_from_layout(&layout).unwrap_or(false)
                    {
                        let tasks = self.open_pending_windows();
                        self.lifecycle = DaemonLifecycle::Active;
                        return Task::batch(tasks);
                    }
                    self.open_coordinated_window()
                }
                IpcCommand::OpenTab { profile_id } => {
                    self.open_tab_in_window(None, profile_id.as_deref())
                }
            },
            Message::CloseRequested(id) => {
                let is_last_window = self.windows.len() <= 1;
                if is_last_window {
                    let status = self.background_work_source.status();
                    if status.has_active_work() {
                        self.close_guard = CloseGuardState::Confirming {
                            window_id: id,
                            active_standalone_tunnels: status.active_standalone_tunnels,
                            active_transfers: status.active_transfers,
                        };
                        return Task::none();
                    }
                    self.save_layout_best_effort();
                }
                self.close_guard = CloseGuardState::Idle;
                window::close(id)
            }
            Message::WindowClosed(id) => {
                self.windows.remove(&id);
                let removed_win = self.window_workspaces.remove(&id);
                if self.active_window == Some(id) {
                    self.active_window = self.windows.iter().next_back().copied();
                }
                if matches!(self.close_guard, CloseGuardState::Confirming { window_id, .. } if window_id == id)
                {
                    self.close_guard = CloseGuardState::Idle;
                }
                if self.windows.is_empty() {
                    if self.lifecycle == DaemonLifecycle::BackgroundRunning {
                        if let Some(win_state) = removed_win {
                            self.close_window_sessions(win_state);
                        }
                        Task::none()
                    } else {
                        self.retired_final_window = removed_win.map(|w| (id, w));
                        self.lifecycle = DaemonLifecycle::Exiting;
                        self.ipc_handle.take();
                        iced::exit()
                    }
                } else {
                    if let Some(win_state) = removed_win {
                        self.close_window_sessions(win_state);
                    }
                    Task::none()
                }
            }
            Message::CloseGuardCancel => {
                self.close_guard = CloseGuardState::Idle;
                Task::none()
            }
            Message::CloseGuardKeepRunningInBackground => {
                self.save_layout_best_effort();
                let target_window = match self.close_guard {
                    CloseGuardState::Confirming { window_id, .. } => Some(window_id),
                    CloseGuardState::Idle => self.windows.iter().next().copied(),
                };
                self.close_guard = CloseGuardState::Idle;
                self.lifecycle = DaemonLifecycle::BackgroundRunning;
                match target_window {
                    Some(id) => window::close(id),
                    None => Task::none(),
                }
            }
            Message::CloseGuardStopAndQuit => {
                self.save_layout_best_effort();
                self.close_guard = CloseGuardState::Idle;
                self.lifecycle = DaemonLifecycle::Exiting;
                self.background_work_source.stop_all();
                self.ipc_handle.take();
                self.retired_final_window = None;
                let session_ids: Vec<SessionId> = self.sessions.keys().copied().collect();
                for sid in session_ids {
                    self.remove_session(sid);
                }
                let mut tasks: Vec<Task<Message>> =
                    self.windows.iter().copied().map(window::close).collect();
                tasks.push(iced::exit());
                Task::batch(tasks)
            }
            Message::ConnectRemoteSession(session_id) => {
                if let Some(SessionEntry::Remote(remote)) = self.sessions.get_mut(&session_id) {
                    remote.connect_requested = true;
                    let already_queued = self
                        .pending_remote_connects
                        .iter()
                        .any(|i| i.session_id == session_id);
                    if !already_queued {
                        self.pending_remote_connects.push(RemoteConnectIntent {
                            session_id,
                            profile_id: remote.profile_id.clone(),
                            host: remote.host.clone(),
                            port: remote.port,
                            username: remote.username.clone(),
                        });
                    }
                }
                Task::none()
            }
            Message::SaveWorkspaceLayout => {
                self.save_layout_best_effort();
                Task::none()
            }
            Message::TerminalInput { session_id, bytes } => {
                self.dispatch_bytes_with_broadcast(session_id, &bytes);
                Task::none()
            }
            Message::ViewportResized {
                session_id,
                cols,
                rows,
            } => {
                match self.sessions.get_mut(&session_id) {
                    Some(SessionEntry::Local(session)) => {
                        if let Err(err) = session.pty.resize(cols, rows) {
                            error!(session = session_id.0, error = %err, "failed to resize PTY");
                            self.last_error = Some(err.to_string());
                        } else {
                            session.snapshot = Arc::new(session.pty.snapshot());
                        }
                    }
                    Some(SessionEntry::Ssh(slot)) => {
                        slot.cols = cols.max(1);
                        slot.rows = rows.max(1);
                    }
                    Some(SessionEntry::Remote(_)) | None => {}
                }
                Task::none()
            }
            Message::PtyEvent { session_id, event } => {
                let is_active_session = Some(session_id) == self.active_session_id();
                if let Some(SessionEntry::Local(session)) = self.sessions.get_mut(&session_id) {
                    match event {
                        PtyEvent::Output => {
                            session.snapshot = Arc::new(session.pty.snapshot());
                            if is_active_session
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
                self.config.terminal = cfg;
                self.sync_terminal_options_to_sessions();
                Task::none()
            }
            Message::OpenSearch => {
                let active_pty = self
                    .active_session_id()
                    .and_then(|id| self.sessions.get(&id))
                    .and_then(SessionEntry::as_local)
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
                    .active_session_id()
                    .and_then(|id| self.sessions.get(&id))
                    .and_then(SessionEntry::as_local)
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
                if let Some(session) = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(SessionEntry::as_local_mut)
                {
                    session.pty.scroll_display(Scroll::Delta(delta_lines));
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::ScrollToTop(session_id) => {
                if let Some(session) = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(SessionEntry::as_local_mut)
                {
                    session.pty.scroll_display(Scroll::Top);
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::ScrollToBottom(session_id) => {
                if let Some(session) = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(SessionEntry::as_local_mut)
                {
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
                if let Some(session) = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(SessionEntry::as_local_mut)
                {
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
                if let Some(session) = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(SessionEntry::as_local_mut)
                {
                    session.pty.update_selection(col, row, side);
                    session.snapshot = Arc::new(session.pty.snapshot());
                }
                Task::none()
            }
            Message::SelectionFinish { session_id } => {
                let copied = if let Some(session) = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(SessionEntry::as_local_mut)
                {
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
                    .and_then(SessionEntry::as_local)
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
                let _ = self.open_tab_in_window(window_scope, None);
                Task::none()
            }
            Message::SelectTab(tab_id) => {
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    ws.select_tab(tab_id);
                }
                Task::none()
            }
            Message::CycleTab(direction) => {
                if let Some(ws) = self.target_workspace_mut(window_scope) {
                    ws.cycle_tab(direction);
                }
                Task::none()
            }
            Message::MoveTab { tab_id, to_index } => {
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    ws.reorder_tab(tab_id, to_index);
                }
                Task::none()
            }
            Message::MoveTabBy { tab_id, delta } => {
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    ws.move_tab_by(tab_id, delta);
                }
                Task::none()
            }
            Message::CloseTab(tab_id) => {
                if let Some(removed_sessions) = self
                    .workspace_for_tab_mut(window_scope, tab_id)
                    .and_then(|ws| ws.close_tab(tab_id))
                {
                    for sid in removed_sessions {
                        self.remove_session(sid);
                    }
                }
                Task::none()
            }
            Message::SplitFocusedPane(axis) => {
                if let Some((tab_id, target_pane)) = self
                    .target_workspace(window_scope)
                    .and_then(|ws| ws.active_tab())
                    .map(|tab| (tab.id, tab.focused_pane))
                {
                    self.split_target_pane(window_scope, tab_id, target_pane, axis);
                }
                Task::none()
            }
            Message::SplitPane { pane_id, axis } => {
                if let Some(tab_id) = self
                    .workspace_for_pane_mut(pane_id)
                    .and_then(|ws| ws.tabs().iter().find(|t| t.root.session_for_pane(pane_id).is_some()))
                    .map(|t| t.id)
                {
                    self.split_target_pane(window_scope, tab_id, pane_id, axis);
                }
                Task::none()
            }
            Message::ClosePane(pane_id) => {
                if let Some(tab_id) = self
                    .workspace_for_pane_mut(pane_id)
                    .and_then(|ws| ws.tabs().iter().find(|t| t.root.session_for_pane(pane_id).is_some()))
                    .map(|t| t.id)
                {
                    self.close_target_pane(window_scope, tab_id, pane_id);
                }
                Task::none()
            }
            Message::CloseFocusedPane => {
                if let Some((tab_id, pane_id)) = self
                    .target_workspace(window_scope)
                    .and_then(|ws| ws.active_tab())
                    .map(|tab| (tab.id, tab.focused_pane))
                {
                    self.close_target_pane(window_scope, tab_id, pane_id);
                }
                Task::none()
            }
            Message::ResizeSplit { split_id, ratio } => {
                if let Some(ws) = self.workspace_for_split_mut(split_id)
                    && let Some(tab_id) = ws
                        .tabs()
                        .iter()
                        .find(|t| t.root.find_split(split_id).is_some())
                        .map(|t| t.id)
                {
                    ws.resize_split(tab_id, split_id, ratio);
                }
                Task::none()
            }
            Message::FocusPane(pane_id) => {
                if let Some(ws) = self.workspace_for_pane_mut(pane_id)
                    && let Some(tab_id) = ws
                        .tabs()
                        .iter()
                        .find(|t| t.root.session_for_pane(pane_id).is_some())
                        .map(|t| t.id)
                {
                    ws.focus_pane(tab_id, pane_id);
                }
                Task::none()
            }
            Message::FocusSession(session_id) => {
                if let Some(wid) = self
                    .window_workspaces
                    .iter()
                    .find_map(|(&wid, w)| w.workspace.contains_session(session_id).then_some(wid))
                {
                    self.active_window = Some(wid);
                }
                if let Some(ws) = self.workspace_for_session_mut(session_id) {
                    ws.focus_session(session_id);
                }
                Task::none()
            }
            Message::MoveFocus(direction) => {
                if let Some(ws) = self.target_workspace_mut(window_scope)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.move_focus(tab_id, direction);
                }
                Task::none()
            }
            Message::CycleFocus(direction) => {
                if let Some(ws) = self.target_workspace_mut(window_scope)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.cycle_focus(tab_id, direction);
                }
                Task::none()
            }
            Message::TogglePaneZoom => {
                if let Some(ws) = self.target_workspace_mut(window_scope)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.toggle_zoom(tab_id);
                }
                Task::none()
            }
            Message::ToggleBroadcastInput => {
                if let Some(ws) = self.target_workspace_mut(window_scope)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.toggle_broadcast_input(tab_id);
                }
                Task::none()
            }
            Message::SetBroadcastInput(enabled) => {
                if let Some(ws) = self.target_workspace_mut(window_scope)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.set_broadcast_input(tab_id, enabled);
                }
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
                    self.launch_profile_entry(window_scope, &profile, disposition);
                } else {
                    self.last_error = Some(format!("unknown profile id: {profile_id}"));
                }
                Task::none()
            }
            Message::LaunchSystemSsh { alias, disposition } => {
                self.command_palette.close();
                self.launch_system_ssh_entry(window_scope, &alias, disposition);
                Task::none()
            }
            Message::SetSessionTransportStatus { session_id, status } => {
                self.update_ssh_slot_status(session_id, status);
                Task::none()
            }
            Message::ConnectSshSession(session_id) => {
                self.update_ssh_slot_status(session_id, TransportStatus::Connecting);
                Task::none()
            }
            Message::DisconnectSshSession(session_id) => {
                self.update_ssh_slot_status(session_id, TransportStatus::Disconnected);
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
            Message::CommandPaletteConfirm => self.handle_command_palette_confirm(window_scope),
            Message::CommandPaletteExecuteItem(action) => {
                self.execute_palette_action(window_scope, action)
            }
            Message::QuickConnect(target) => {
                self.command_palette.close();
                self.launch_quick_connect(window_scope, target);
                Task::none()
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

    fn sync_terminal_options_to_sessions(&mut self) {
        let shape = cursor_style_to_shape(self.config.terminal.cursor_style);
        let scrollback = self.config.terminal.scrollback_lines;
        for session in self
            .sessions
            .values_mut()
            .filter_map(SessionEntry::as_local_mut)
        {
            session.pty.apply_options(scrollback, shape);
            session.snapshot = Arc::new(session.pty.snapshot());
        }
        if self.search.open && !self.search.query.is_empty() {
            let active_pty = self
                .active_session_id()
                .and_then(|id| self.sessions.get(&id))
                .and_then(SessionEntry::as_local)
                .map(|s| s.pty.as_ref());
            self.search.refresh_matches(active_pty, false);
        }
    }

    fn focus_search_match(&mut self, target: Option<GridSearchMatch>) {
        if let Some(matched) = target
            && let Some(session) = self
                .active_session_id()
                .and_then(|id| self.sessions.get_mut(&id))
                .and_then(SessionEntry::as_local_mut)
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
        let Some(session) = self
            .sessions
            .get(&session_id)
            .and_then(SessionEntry::as_local)
        else {
            return;
        };
        let bytes = format_paste_bytes(text, session.pty.is_bracketed_paste());
        self.write_bytes_to_session(session_id, &bytes);
    }

    fn dispatch_bytes_with_broadcast(&mut self, session_id: SessionId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let targets = self
            .workspace_for_session_mut(session_id)
            .map(|ws| ws.input_targets_for_session(session_id))
            .unwrap_or_else(|| vec![session_id]);
        let is_broadcast = targets.len() > 1;
        for target_id in targets {
            if is_broadcast
                && self
                    .sessions
                    .get(&target_id)
                    .and_then(SessionEntry::as_local)
                    .is_some_and(|session| session.exited.is_some())
            {
                continue;
            }
            self.write_bytes_to_session(target_id, bytes);
        }
    }

    fn write_bytes_to_session(&mut self, session_id: SessionId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(session) = self
            .sessions
            .get_mut(&session_id)
            .and_then(SessionEntry::as_local_mut)
        else {
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

    /// Renders the workspace tab strip, collapsible Left Sidebar, status/reload/Vault banners,
    /// active split panes, Status Bar, Search/Paste overlays, Command Palette, and Close Guard modal.
    pub fn view(&self, window_id: window::Id) -> Element<'_, Message, Theme, iced::Renderer> {
        let ws = self.window_workspaces.get(&window_id).map(|w| &w.workspace);
        let mut main_pane_col = column![self.render_tab_strip(ws)]
            .width(Length::Fill)
            .height(Length::Fill);

        if let Some(banner_text) = ws.and_then(Self::broadcast_banner_for_workspace) {
            main_pane_col = main_pane_col.push(render_broadcast_banner(banner_text));
        }

        if let Some(toasts) = self.reload_ui.view_reload_toasts() {
            main_pane_col = main_pane_col.push(toasts);
        }
        if let Some(vault_bar) = self.reload_ui.view_vault_banner(&self.vault) {
            main_pane_col = main_pane_col.push(vault_bar);
        }

        let body = match ws.and_then(|w| w.active_tab()) {
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

        let status_snapshot = self.status_bar_snapshot_for_workspace(ws);
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

        let active_tab = ws.and_then(|w| w.active_tab_id());
        let intercepted = ShortcutInterceptor::with_palette(
            layers.into(),
            active_tab,
            self.command_palette.is_open(),
        );
        let scoped = intercepted.map(move |msg| Message::InWindow {
            window_id,
            inner: Box::new(msg),
        });
        wrap_with_close_guard_modal(
            scoped,
            self.close_guard,
            window_id,
            Message::CloseGuardCancel,
            Message::CloseGuardKeepRunningInBackground,
            Message::CloseGuardStopAndQuit,
        )
    }

    fn render_tab_strip(
        &self,
        ws: Option<&Workspace>,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let active_tab_id = ws.and_then(|w| w.active_tab_id());
        let tab_count = ws.map_or(0, |w| w.tab_count());

        let mut tabs_row = row![].spacing(4).align_y(Alignment::Center);

        if let Some(ws) = ws {
            for (idx, tab) in ws.tabs().iter().enumerate() {
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
        }

        let active_zoomed = ws
            .and_then(|w| w.active_tab())
            .is_some_and(|t| t.zoomed_pane.is_some());
        let active_broadcast = ws
            .and_then(|w| w.active_tab())
            .is_some_and(|t| t.broadcast_input);

        let mut actions = row![
            button(text("Palette").size(12))
                .padding([4, 8])
                .style(button::subtle)
                .on_press(Message::ToggleCommandPalette),
            button(text("+ Tab").size(12))
                .padding([4, 8])
                .style(button::secondary)
                .on_press(Message::NewTab),
            button(text("+ Win").size(12))
                .padding([4, 8])
                .style(button::subtle)
                .on_press(Message::NewWindow),
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
            Some(SessionEntry::Local(session)) => TerminalViewport::with_focus(
                session.id,
                Arc::clone(&session.snapshot),
                options,
                matches,
                active_match,
                terminal_focused,
            ),
            Some(SessionEntry::Remote(remote)) => render_remote_placeholder(remote),
            Some(SessionEntry::Ssh(slot)) => render_ssh_session_slot(slot),
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
                    .map(SessionEntry::title)
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
    pub fn title(&self, window_id: window::Id) -> String {
        let ws = self
            .window_workspaces
            .get(&window_id)
            .map(|w| &w.workspace)
            .or_else(|| {
                self.retired_final_window
                    .as_ref()
                    .filter(|(id, _)| *id == window_id)
                    .map(|(_, w)| &w.workspace)
            })
            .or_else(|| self.primary_workspace());
        let tab = ws.and_then(|w| w.active_tab());
        if let Some(entry) = tab
            .and_then(|t| t.focused_session_id())
            .and_then(|id| self.sessions.get(&id))
        {
            let mut title = match entry {
                SessionEntry::Local(session) => match session.exited {
                    Some(Some(code)) => format!("Termewnal — {} [exited: {code}]", session.title),
                    Some(None) => format!("Termewnal — {} [exited]", session.title),
                    None => format!("Termewnal — {}", session.title),
                },
                SessionEntry::Remote(remote) => format!(
                    "Termewnal — {} [{}]",
                    remote.title,
                    remote.transport_status.label()
                ),
                SessionEntry::Ssh(slot) => format!(
                    "Termewnal — {} [{}]",
                    slot.title,
                    slot.transport_status.label()
                ),
            };
            if tab.is_some_and(|t| t.zoomed_pane.is_some()) {
                title.push_str(" [ZOOM]");
            }
            if tab.is_some_and(|t| t.broadcast_input) {
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

    /// Subscribes to window lifecycle events, single-instance IPC commands, config hot-reload,
    /// Vault auto-lock ticks, and live PTY output streams.
    pub fn subscription(&self) -> Subscription<Message> {
        let close_requests = window::close_requests().map(Message::CloseRequested);
        let closed = window::close_events().map(Message::WindowClosed);
        let win_events = window::events().filter_map(map_window_event);
        let shortcuts = event::listen_with(window_shortcut_event);
        let config_watch = Subscription::run_with(
            ConfigWatchSubKey {
                config_dir: self.paths.config_dir().to_path_buf(),
                paths: self.paths.clone(),
            },
            config_watch_stream,
        );

        let pty_subs = self.sessions.values().filter_map(|entry| {
            let session = entry.as_local()?;
            Some(Subscription::run_with(
                PtySubKey {
                    session_id: session.id,
                    pty: Arc::clone(&session.pty),
                },
                pty_event_stream,
            ))
        });

        let ipc_sub = self.ipc_rx.as_ref().map(|rx| {
            Subscription::run_with(
                IpcSubKey {
                    rx: Arc::clone(rx),
                },
                ipc_event_stream,
            )
        });

        let vault_tick = (self.vault.backend() == VaultBackend::EncryptedFile
            && self.vault.is_unlocked())
        .then(|| iced::time::every(Duration::from_secs(15)).map(|_| Message::VaultAutoLockTick));

        Subscription::batch(
            [close_requests, closed, win_events, shortcuts, config_watch]
                .into_iter()
                .chain(vault_tick)
                .chain(ipc_sub)
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

    /// Returns the active window's workspace tab and split-pane layout state.
    pub fn workspace(&self) -> &Workspace {
        self.primary_workspace().unwrap_or(&self.empty_workspace)
    }

    /// Returns the workspace state owned by `window_id`, if present.
    pub fn window_workspace(&self, window_id: window::Id) -> Option<&Workspace> {
        self.window_workspaces
            .get(&window_id)
            .map(|w| &w.workspace)
    }

    /// Returns all currently open window IDs in deterministic order.
    pub fn window_ids(&self) -> Vec<window::Id> {
        self.windows.iter().copied().collect()
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
        self.status_bar_snapshot_for_workspace(self.primary_workspace())
    }

    fn status_bar_snapshot_for_workspace(&self, ws: Option<&Workspace>) -> StatusBarSnapshot {
        let active_tab = ws.and_then(|w| w.active_tab());
        let zoomed = active_tab.is_some_and(|tab| tab.zoomed_pane.is_some());
        let broadcast = active_tab.is_some_and(|tab| tab.broadcast_input);
        let active_session = active_tab
            .and_then(|tab| tab.focused_session_id())
            .and_then(|sid| self.sessions.get(&sid))
            .map(|entry| match entry {
                SessionEntry::Local(s) => {
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
                SessionEntry::Remote(remote) => {
                    let target = match &remote.username {
                        Some(u) if !u.is_empty() => format!("{u}@{}:{}", remote.host, remote.port),
                        _ => format!("{}:{}", remote.host, remote.port),
                    };
                    SessionStatusHook {
                        session_id: remote.id,
                        kind: SessionKindLabel::Ssh,
                        title: remote.title.clone(),
                        target,
                        cols: 80,
                        rows: 24,
                        exited: None,
                        transport_status: remote.transport_status.clone(),
                        zoomed,
                        broadcast,
                    }
                }
                SessionEntry::Ssh(slot) => SessionStatusHook {
                    session_id: slot.id,
                    kind: SessionKindLabel::Ssh,
                    title: slot.title.clone(),
                    target: slot.target.display_addr(),
                    cols: slot.cols,
                    rows: slot.rows,
                    exited: None,
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
        let workspaces: Box<dyn Iterator<Item = &Workspace> + '_> =
            if !self.window_workspaces.is_empty() {
                Box::new(self.window_workspaces.values().map(|w| &w.workspace))
            } else {
                Box::new(self.pending_windows.iter().map(|w| &w.workspace))
            };
        for ws in workspaces {
            for tab in ws.tabs() {
                for sid in tab.root.session_ids() {
                    if let Some(entry) = self.sessions.get(&sid) {
                        let target_label = match entry {
                            SessionEntry::Local(s) => {
                                format!("Local · {}", s.pty.shell().display_name)
                            }
                            SessionEntry::Remote(remote) => {
                                let addr = match &remote.username {
                                    Some(u) if !u.is_empty() => {
                                        format!("{u}@{}:{}", remote.host, remote.port)
                                    }
                                    _ => format!("{}:{}", remote.host, remote.port),
                                };
                                format!("SSH · {addr} ({})", remote.transport_status.label())
                            }
                            SessionEntry::Ssh(slot) => {
                                format!(
                                    "SSH · {} ({})",
                                    slot.target.display_addr(),
                                    slot.transport_status.label()
                                )
                            }
                        };
                        sessions.push(PaletteSessionEntry {
                            session_id: sid,
                            title: entry.title().to_string(),
                            target_label,
                            is_active: active_sid == Some(sid),
                        });
                    }
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
        self.primary_workspace().and_then(|w| w.active_tab_id())
    }

    /// Returns the currently active `SessionId`.
    pub fn active_session_id(&self) -> Option<SessionId> {
        self.primary_workspace()
            .and_then(|w| w.active_tab())
            .and_then(|tab| tab.focused_session_id())
    }

    /// Returns the zoomed `PaneId` in the active tab, if pane zoom is currently active.
    pub fn active_tab_zoomed_pane(&self) -> Option<PaneId> {
        self.primary_workspace()
            .and_then(|w| w.active_tab())
            .and_then(|tab| tab.zoomed_pane)
    }

    /// Returns `true` when the active workspace tab has Broadcast Input mode enabled.
    pub fn is_broadcast_banner_visible(&self) -> bool {
        self.primary_workspace()
            .and_then(|w| w.active_tab())
            .is_some_and(|tab| tab.broadcast_input)
    }

    fn broadcast_banner_for_workspace(ws: &Workspace) -> Option<String> {
        let tab = ws.active_tab()?;
        if !tab.broadcast_input {
            return None;
        }
        let count = tab.root.pane_count();
        let noun = if count == 1 { "pane" } else { "panes" };
        Some(format!(
            "BROADCAST INPUT ACTIVE — Keystrokes are mirrored to {count} {noun} in this tab"
        ))
    }

    /// Returns the warning banner copy when Broadcast Input is active in the current tab.
    pub fn broadcast_banner_text(&self) -> Option<String> {
        let ws = self.primary_workspace()?;
        Self::broadcast_banner_for_workspace(ws)
    }

    /// Returns an immutable reference to a runtime local `Session` by `id`.
    pub fn session(&self, id: SessionId) -> Option<&LocalSession> {
        self.sessions.get(&id).and_then(SessionEntry::as_local)
    }

    /// Returns an immutable reference to a restored or pending remote `Session` by `id`.
    pub fn remote_session(&self, id: SessionId) -> Option<&RemoteSession> {
        self.sessions.get(&id).and_then(SessionEntry::as_remote)
    }

    /// Returns an immutable reference to an SSH `Session` slot by `id`.
    pub fn ssh_session(&self, id: SessionId) -> Option<&SshSessionSlot> {
        match self.sessions.get(&id) {
            Some(SessionEntry::Ssh(slot)) => Some(slot),
            _ => None,
        }
    }

    fn update_ssh_slot_status(&mut self, session_id: SessionId, status: TransportStatus) {
        match self.sessions.get_mut(&session_id) {
            Some(SessionEntry::Ssh(slot)) => {
                slot.status_message = format_ssh_slot_message(&slot.target, &status);
                slot.transport_status = status;
            }
            Some(SessionEntry::Remote(remote)) => {
                remote.transport_status = status;
            }
            _ => {}
        }
    }

    /// Returns connection intents recorded for remote `Session`s on startup or manual connect.
    pub fn pending_remote_connects(&self) -> &[RemoteConnectIntent] {
        &self.pending_remote_connects
    }

    /// Exports the current multi-window workspace geometry and session descriptors to [`WorkspaceLayout`].
    pub fn to_workspace_layout(&self) -> WorkspaceLayout {
        let windows_iter: Box<dyn Iterator<Item = &WindowState> + '_> =
            if !self.window_workspaces.is_empty() {
                Box::new(self.window_workspaces.values())
            } else {
                Box::new(self.pending_windows.iter())
            };

        let windows = windows_iter
            .filter(|w| w.workspace.tab_count() > 0)
            .map(|win_state| {
                let ws = &win_state.workspace;
                let active_tab = ws
                    .active_tab_id()
                    .and_then(|aid| ws.tabs().iter().position(|t| t.id == aid))
                    .unwrap_or(0);
                let tabs = ws
                    .tabs()
                    .iter()
                    .map(|tab| {
                        let pane_ids = tab.root.pane_ids();
                        let focused_pane = pane_ids
                            .iter()
                            .position(|&pid| pid == tab.focused_pane)
                            .unwrap_or(0);
                        let zoomed_pane = tab
                            .zoomed_pane
                            .and_then(|zp| pane_ids.iter().position(|&pid| pid == zp));
                        TabLayout {
                            title: tab.title.clone(),
                            focused_pane,
                            zoomed_pane,
                            root: self.export_split_node(&tab.root),
                        }
                    })
                    .collect();

                WindowLayout {
                    width: win_state.width,
                    height: win_state.height,
                    active_tab,
                    tabs,
                }
            })
            .collect();

        WorkspaceLayout {
            schema_version: CURRENT_SCHEMA_VERSION,
            windows,
        }
    }

    fn export_split_node(&self, node: &SplitNode) -> LayoutSplitNode {
        match node {
            SplitNode::Leaf { session_id, .. } => LayoutSplitNode::Leaf {
                session: self.export_session_descriptor(*session_id),
            },
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
                ..
            } => LayoutSplitNode::Split {
                axis: match axis {
                    SplitAxis::Horizontal => LayoutSplitAxis::Horizontal,
                    SplitAxis::Vertical => LayoutSplitAxis::Vertical,
                },
                ratio: *ratio,
                first: Box::new(self.export_split_node(first)),
                second: Box::new(self.export_split_node(second)),
            },
        }
    }

    fn export_session_descriptor(&self, session_id: SessionId) -> SessionDescriptor {
        match self.sessions.get(&session_id) {
            Some(SessionEntry::Local(local)) => SessionDescriptor::Local {
                profile_id: local.profile_id.clone(),
                title: Some(local.title.clone()),
                working_dir: local
                    .pty
                    .current_working_dir()
                    .or_else(|| local.initial_working_dir.clone()),
                shell: Some(local.pty.shell().program.clone()),
            },
            Some(SessionEntry::Remote(remote)) => SessionDescriptor::Remote {
                profile_id: remote.profile_id.clone(),
                host: remote.host.clone(),
                port: remote.port,
                username: remote.username.clone(),
                title: Some(remote.title.clone()),
                auto_connect_on_startup: remote.auto_connect_on_startup,
            },
            Some(SessionEntry::Ssh(slot)) => SessionDescriptor::Remote {
                profile_id: slot.target.profile_id.clone(),
                host: slot.target.host.clone(),
                port: slot.target.port,
                username: Some(slot.target.username.clone()),
                title: Some(slot.title.clone()),
                auto_connect_on_startup: slot.target.auto_connect_on_startup,
            },
            None => SessionDescriptor::Local {
                profile_id: None,
                title: None,
                working_dir: None,
                shell: None,
            },
        }
    }

    /// Atomically persists the current workspace layout to `layout.toml`.
    pub fn save_workspace_layout(&self) -> Result<(), String> {
        let layout = self.to_workspace_layout();
        save_layout(&self.paths, &layout).map_err(|err| err.to_string())
    }

    fn save_layout_best_effort(&self) {
        let layout = self.to_workspace_layout();
        if layout.windows.is_empty() {
            return;
        }
        if let Err(err) = save_layout(&self.paths, &layout) {
            warn!(error = %err, "failed to save workspace layout");
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

    /// Returns the default atomic tracker for Standalone `Tunnel` and `Transfer` counts.
    pub fn background_work_tracker(&self) -> &Arc<BackgroundWorkTracker> {
        &self.background_work_tracker
    }

    /// Replaces the [`BackgroundWorkSource`] queried when closing the final window.
    pub fn set_background_work_source(&mut self, source: Arc<dyn BackgroundWorkSource>) {
        self.background_work_source = source;
    }

    /// Returns the current Close Guard modal state.
    pub fn close_guard(&self) -> CloseGuardState {
        self.close_guard
    }

    /// Returns the current daemon lifecycle state (`Active`, `BackgroundRunning`, or `Exiting`).
    pub fn lifecycle(&self) -> DaemonLifecycle {
        self.lifecycle
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

#[derive(Clone)]
struct IpcSubKey {
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<IpcCommand>>>>,
}

impl Hash for IpcSubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.rx) as usize).hash(state);
    }
}

fn ipc_event_stream(key: &IpcSubKey) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let rx_slot = Arc::clone(&key.rx);
    Box::pin(iced::stream::channel(32, async move |mut output| {
        let maybe_rx = rx_slot.lock().ok().and_then(|mut g| g.take());
        if let Some(mut rx) = maybe_rx {
            while let Some(command) = rx.recv().await {
                if output.send(Message::IpcCommand(command)).await.is_err() {
                    break;
                }
            }
        }
    }))
}

fn map_window_event((id, event): (window::Id, window::Event)) -> Option<Message> {
    match event {
        window::Event::Focused => Some(Message::WindowFocused(id)),
        window::Event::Resized(size) => Some(Message::WindowResized(id, size)),
        _ => None,
    }
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

fn render_remote_placeholder(
    remote: &RemoteSession,
) -> Element<'static, Message, Theme, iced::Renderer> {
    let target = match &remote.username {
        Some(user) if !user.is_empty() => format!("{user}@{}:{}", remote.host, remote.port),
        _ => format!("{}:{}", remote.host, remote.port),
    };
    let status_line = format!("Status: {}", remote.transport_status.label());
    let mut card = column![
        text(remote.title.clone()).size(18),
        text(target).size(14),
        text(status_line).size(13),
    ]
    .spacing(8)
    .align_x(Alignment::Center);

    if remote.connect_requested {
        card = card.push(text("Connection requested — awaiting SSH transport wiring").size(12));
    } else {
        card = card.push(
            button(text("Connect").size(14))
                .padding([6, 16])
                .style(button::primary)
                .on_press(Message::ConnectRemoteSession(remote.id)),
        );
    }

    container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
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
        TransportStatus::Connecting => {
            format!("SSH transport ready for Ticket 07 wiring ({addr})")
        }
        TransportStatus::Connected { latency_ms: Some(ms) } => {
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

fn render_ssh_session_slot(
    slot: &SshSessionSlot,
) -> Element<'static, Message, Theme, iced::Renderer> {
    let addr = slot.target.display_addr();
    let mut details = column![
        text(format!("SSH Session: {}", slot.title)).size(16),
        text(format!("Target: {addr}")).size(13),
        text(format!("Transport: {}", slot.transport_status.label())).size(13),
        text(slot.status_message.clone())
            .size(12)
            .color(Color::from_rgb8(148, 163, 184)),
    ]
    .spacing(6);

    if !slot.target.proxy_jump.is_empty() {
        details = details.push(
            text(format!("ProxyJump: {}", slot.target.proxy_jump.join(" -> ")))
                .size(12)
                .color(Color::from_rgb8(148, 163, 184)),
        );
    }
    if let Some(tmux) = &slot.target.tmux_reattach {
        details = details.push(
            text(format!("Multiplexer attach: {tmux}"))
                .size(12)
                .color(Color::from_rgb8(148, 163, 184)),
        );
    }

    let action_btn = match slot.transport_status {
        TransportStatus::Disconnected => button(text("Connect").size(12))
            .padding([6, 12])
            .style(button::primary)
            .on_press(Message::ConnectSshSession(slot.id)),
        _ => button(text("Disconnect").size(12))
            .padding([6, 12])
            .style(button::secondary)
            .on_press(Message::DisconnectSshSession(slot.id)),
    };

    details = details.push(action_btn);

    container(details)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .padding(20)
        .into()
}
