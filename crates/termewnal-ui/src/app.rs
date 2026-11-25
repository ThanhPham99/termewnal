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
    ConfigWatcher, LayoutSplitAxis, LayoutSplitNode, OpenSshHost, Profile, ProfileTarget,
    ProfilesConfig, SecretStore, SessionDescriptor, SshAuthMethod, TabLayout, TerminalConfig,
    ThemePalette, TomlReloadError, TunnelDirection, TunnelRule, VaultBackend, VaultSession,
    WindowLayout, WorkspaceLayout, ensure_config_files, import_tabby_config_file_into_profiles,
    load_layout, load_system_ssh_hosts, resolve_mux_attach_command, save_cloned_openssh_profile,
    save_layout, save_profiles,
};
use termewnal_pty::{
    GridSearchMatch, PtyEvent, PtySession, PtySpawnConfig, Scroll, SelectionType, ShellSpec, Side,
    TerminalBuffer, TerminalSnapshot, detect_default_shell,
};
use termewnal_ssh::{
    ConnectionSupervisor, OsAgentConnector, ProfileTransportKey, RejectAllInteractionHandler,
    RemoteListing, SftpEndpoint, SftpManager, SshConnectConfig, SshSession,
    StandaloneTransportSpec, SupervisorConfig, TransferId, TransferQueue, TransferQueueSnapshot,
    TransportStatus, TunnelManager, TunnelManagerSnapshot, join_remote_path, parent_remote_path,
};
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
use crate::sftp_drawer::{SftpDrawerDock, SftpDrawerState, render_sftp_drawer};
use crate::sidebar::{
    ActiveTunnelEntry, SidebarSection, SidebarState, SystemSshProfile, render_sidebar,
};
use crate::split_view::{CapturedEventGate, PaneFrame, ShortcutInterceptor, SplitView};
use crate::ssh_ui::{SshTargetSpec, SshUiEvent, SshUiState, connect_ssh_task};
use crate::status_bar::{
    SessionKindLabel, SessionStatusHook, StatusBarAction, StatusBarSnapshot, StatusBarState,
    TransferSummaryHook, TunnelSummaryHook, render_status_bar,
};
use crate::tabby_import_ui::{TabbyImportUiState, tabby_import_path_input_id};
use crate::terminal_ergonomics::{
    FileDropAction, PendingPaste, SearchBarState, classify_file_drop, cursor_style_to_shape,
    format_paste_bytes, iced_theme_from_name, is_multiline_paste, open_url_with_system,
    paste_confirm_modal_view, read_clipboard_via_arboard, search_bar_view, search_input_id,
};
use crate::terminal_widget::{TerminalViewOptions, TerminalViewport};
use crate::tunnel_ui::{
    AdHocTunnelRule, TunnelUiState, apply_known_hosts_override, build_active_tunnel_entries,
    tunnel_name_input_id,
};
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
    pub keepalive_interval_secs: Option<u64>,
    pub keepalive_max_missed: Option<u32>,
    pub auto_attach_mux: bool,
    pub mux_attach_command: Option<String>,
}

impl SshSessionTarget {
    /// Returns the formatted `user@host:port` destination string.
    pub fn display_addr(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    fn effective_mux_command(&self) -> Option<String> {
        resolve_mux_attach_command(
            self.auto_attach_mux,
            self.mux_attach_command.as_deref(),
            self.tmux_reattach.as_deref(),
        )
    }

    fn resolve_keepalive(&self, app_config: &AppConfig) -> (u64, u32) {
        (
            self.keepalive_interval_secs
                .unwrap_or(app_config.keepalive_interval_secs),
            self.keepalive_max_missed
                .unwrap_or(app_config.keepalive_max_missed),
        )
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
            keepalive_interval_secs: Some(spec.supervisor_config.keepalive_interval.as_secs()),
            keepalive_max_missed: Some(spec.supervisor_config.keepalive_max_missed),
            auto_attach_mux: spec.supervisor_config.mux_attach_command.is_some(),
            mux_attach_command: spec.supervisor_config.mux_attach_command.clone(),
        }
    }

    fn to_target_spec(
        &self,
        title: &str,
        cols: u16,
        rows: u16,
        app_config: &AppConfig,
    ) -> SshTargetSpec {
        let (ka_secs, ka_max) = self.resolve_keepalive(app_config);
        let mut supervisor_config = SupervisorConfig::from_app_config(app_config);
        supervisor_config.keepalive_interval = Duration::from_secs(ka_secs);
        supervisor_config.keepalive_max_missed = ka_max.max(1);
        supervisor_config.mux_attach_command = self.effective_mux_command();

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
                scrollback_lines: app_config.terminal.scrollback_lines,
            },
            supervisor_config,
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
    supervisor: Option<ConnectionSupervisor>,
    probe_count: u64,
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

    /// Returns the active [`ConnectionSupervisor`] handle if supervised transport is attached.
    pub fn supervisor(&self) -> Option<&ConnectionSupervisor> {
        self.supervisor.as_ref()
    }

    /// Returns the number of manual keepalive probes triggered on this SSH `Session`.
    pub fn probe_count(&self) -> u64 {
        self.probe_count
    }

    /// Returns the [`SshTargetSpec`] used to connect this SSH `Session`.
    pub fn spec(&self) -> &SshTargetSpec {
        &self.spec
    }

    /// Returns the last connection/MITM error recorded on this SSH `Session`, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn sftp_key(&self) -> ProfileTransportKey {
        ProfileTransportKey::from_connect_config(
            self.target.profile_id.clone(),
            &self.spec.connect_config,
        )
    }

    fn sftp_endpoint(&self, vault: &Arc<VaultSession>) -> SftpEndpoint {
        let mut endpoint = SftpEndpoint::new(self.sftp_key(), self.spec.connect_config.clone())
            .with_vault(Arc::clone(vault) as Arc<dyn SecretStore>)
            .with_agent_connector(Arc::clone(&self.spec.agent_connector));
        if let Some(active) = &self.active {
            endpoint = endpoint.with_primary_transport(Arc::clone(active.primary_transport()));
        }
        endpoint
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

    fn buffer(&self) -> Option<&Arc<TerminalBuffer>> {
        match self {
            Self::Local(s) => Some(s.pty.buffer()),
            Self::Ssh(s) => Some(&s.buffer),
            Self::Remote(_) => None,
        }
    }

    fn refresh_snapshot(&mut self) {
        match self {
            Self::Local(s) => s.snapshot = Arc::new(s.pty.snapshot()),
            Self::Ssh(s) => s.snapshot = Arc::new(s.buffer.snapshot()),
            Self::Remote(_) => {}
        }
    }

    fn as_local(&self) -> Option<&LocalSession> {
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

    fn as_ssh(&self) -> Option<&SshSessionSlot> {
        match self {
            Self::Ssh(s) => Some(s),
            Self::Local(_) | Self::Remote(_) => None,
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
    ReloadSystemSshConfig,
    CloneSystemSshToProfile(String),
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
    ProbeAndReconnectAll,
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
    OpenTabbyImportModal,
    CloseTabbyImportModal,
    TabbyImportPathChanged(String),
    SubmitTabbyImport,
    ImportTabbyConfigFromPath(PathBuf),
    TabbyVaultCredentialChanged {
        profile_id: String,
        value: String,
    },
    SaveTabbyVaultCredential(String),
    ToggleSftpDrawer,
    SetSftpDrawerOpen(bool),
    ResizeSftpDrawer(f32),
    SftpPathInputChanged(String),
    SftpNavigateTo(String),
    SftpNavigateUp,
    SftpRefresh,
    SftpNewFolderInputChanged(String),
    SftpCreateFolder,
    SftpDeleteEntry {
        path: String,
        is_dir: bool,
    },
    SftpDownloadFile {
        remote_path: String,
        file_name: String,
    },
    SftpDrawerFileDropped {
        path: PathBuf,
    },
    SftpBrowseResult {
        session_id: SessionId,
        generation: u64,
        result: Result<RemoteListing, String>,
    },
    SftpMutationResult {
        session_id: SessionId,
        result: Result<(), String>,
    },
    TransferQueueUpdated(TransferQueueSnapshot),
    PauseTransfer(TransferId),
    ResumeTransfer(TransferId),
    CancelTransfer(TransferId),
    RetryTransfer(TransferId),
    ClearFinishedTransfers,
    OpenTunnelCreateModal,
    CloseTunnelCreateModal,
    TunnelFormDirectionChanged(TunnelDirection),
    TunnelFormNameChanged(String),
    TunnelFormProfileSelected(Option<String>),
    TunnelFormBindHostChanged(String),
    TunnelFormBindPortChanged(String),
    TunnelFormTargetHostChanged(String),
    TunnelFormTargetPortChanged(String),
    TunnelFormStandaloneToggled(bool),
    TunnelFormHoldGraceChanged(String),
    SubmitTunnelCreate,
    TunnelPublicBindConfirm,
    TunnelPublicBindCancel,
    ToggleTunnel(String),
    ReconnectTunnel(String),
    ReconnectAllTunnels,
    TunnelManagerUpdated(TunnelManagerSnapshot),
    FinalizeExit,
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
    ssh_ui: SshUiState,
    windows: BTreeSet<window::Id>,
    window_workspaces: BTreeMap<window::Id, WindowState>,
    pending_windows: VecDeque<WindowState>,
    retired_final_window: Option<(window::Id, WindowState)>,
    empty_workspace: Workspace,
    active_window: Option<window::Id>,
    workspace_ids: WorkspaceIds,
    sessions: BTreeMap<SessionId, SessionEntry>,
    pending_remote_connects: Vec<RemoteConnectIntent>,
    openssh_hosts: Vec<OpenSshHost>,
    sidebar: SidebarState,
    command_palette: CommandPaletteState,
    tabby_import: TabbyImportUiState,
    status_bar: StatusBarState,
    sftp_manager: SftpManager,
    transfer_queue: TransferQueue,
    sftp_drawer: SftpDrawerState,
    tunnel_manager: TunnelManager,
    tunnel_ui: TunnelUiState,
    synced_standalone_tunnels: usize,
    known_hosts_override: Option<PathBuf>,
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
        let sftp_manager = SftpManager::new();
        let transfer_queue = TransferQueue::new(sftp_manager.clone());
        let tunnel_manager = TunnelManager::new();
        let download_dir = paths.cache_dir().join("downloads");
        let mut state = Self {
            paths,
            config,
            profiles,
            vault,
            reload_ui: ConfigReloadUiState::default(),
            ssh_ui: SshUiState::default(),
            windows: BTreeSet::new(),
            window_workspaces: BTreeMap::new(),
            pending_windows: VecDeque::new(),
            retired_final_window: None,
            empty_workspace: Workspace::with_ids(workspace_ids.clone()),
            active_window: None,
            workspace_ids,
            sessions: BTreeMap::new(),
            pending_remote_connects: Vec::new(),
            openssh_hosts: Vec::new(),
            sidebar: SidebarState::new(),
            command_palette: CommandPaletteState::new(),
            tabby_import: TabbyImportUiState::default(),
            status_bar: StatusBarState::new(),
            sftp_manager,
            transfer_queue,
            sftp_drawer: SftpDrawerState::with_download_dir(download_dir),
            tunnel_manager,
            tunnel_ui: TunnelUiState::default(),
            synced_standalone_tunnels: 0,
            known_hosts_override: None,
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
        state.reload_system_ssh_config();
        state.sync_tunnel_telemetry();
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
        let _ = self.open_session_with_disposition(None, id, tab_title, LaunchDisposition::NewTab);

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
            supervisor: None,
            probe_count: 0,
            last_error: None,
        };
        self.sessions.insert(id, SessionEntry::Ssh(slot));
        id
    }

    fn start_ssh_connect(&mut self, session_id: SessionId) -> Task<Message> {
        self.ssh_ui.clear_session_prompts(session_id);
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id) else {
            return Task::none();
        };

        self.sftp_manager.clear_primary_transport(&slot.sftp_key());
        if let Some(prev_sup) = slot.supervisor.take() {
            prev_sup.disconnect();
            slot.active = None;
        } else if let Some(prev) = slot.active.take() {
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
        slot.spec.connect_config =
            apply_known_hosts_override(slot.spec.connect_config.clone(), self.known_hosts_override.as_ref());
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

    fn reconnect_ssh_session(&mut self, session_id: SessionId) -> Task<Message> {
        if self.ssh_ui.mitm_alert(session_id).is_some() {
            return Task::none();
        }
        if let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id)
            && let Some(sup) = &slot.supervisor
        {
            sup.reconnect_now();
            slot.last_error = None;
            slot.exited = None;
            slot.transport_status = TransportStatus::Connecting;
            slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
            return Task::none();
        }
        self.start_ssh_connect(session_id)
    }

    fn cancel_ssh_reconnect(&mut self, session_id: SessionId) {
        self.ssh_ui.clear_session_prompts(session_id);
        if let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id) {
            self.sftp_manager.clear_primary_transport(&slot.sftp_key());
            if let Some(sup) = &slot.supervisor {
                sup.cancel_reconnect();
                slot.active = None;
            } else if let Some(ssh) = slot.active.take() {
                ssh.disconnect();
            }
            slot.transport_status = TransportStatus::Disconnected;
            slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
        }
    }

    fn disconnect_ssh_session(&mut self, session_id: SessionId) {
        self.ssh_ui.clear_session_prompts(session_id);
        let (_, tunnel_cleanup) = self
            .tunnel_manager
            .stop_session_tunnels_with_barrier(session_id.0);
        if let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id) {
            self.sftp_manager.clear_primary_transport(&slot.sftp_key());
            let sup = slot.supervisor.take();
            let active = slot.active.take();
            spawn_ssh_disconnect_after(tunnel_cleanup, sup, active);
            slot.transport_status = TransportStatus::Disconnected;
            slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
        }
        self.sync_tunnel_telemetry();
    }

    fn probe_and_reconnect_all(&mut self) -> Task<Message> {
        let ssh_ids: Vec<SessionId> = self
            .sessions
            .iter()
            .filter_map(|(&id, entry)| matches!(entry, SessionEntry::Ssh(_)).then_some(id))
            .collect();

        let mut tasks = Vec::new();
        for sid in ssh_ids {
            if self.ssh_ui.mitm_alert(sid).is_some() {
                continue;
            }
            let action_needed = match self.sessions.get_mut(&sid) {
                Some(SessionEntry::Ssh(slot)) => match slot.transport_status {
                    TransportStatus::Connected { .. } | TransportStatus::Degraded { .. } => {
                        slot.probe_count = slot.probe_count.saturating_add(1);
                        if let Some(sup) = &slot.supervisor {
                            sup.probe_now();
                        }
                        false
                    }
                    TransportStatus::Reconnecting { .. } => true,
                    TransportStatus::Disconnected => slot.generation > 0,
                    TransportStatus::Connecting => false,
                },
                _ => false,
            };
            if action_needed {
                let task = self.reconnect_ssh_session(sid);
                tasks.push(task);
            }
        }
        Task::batch(tasks)
    }

    fn open_session_with_disposition(
        &mut self,
        window_scope: Option<window::Id>,
        session_id: SessionId,
        tab_title: Option<String>,
        disposition: LaunchDisposition,
    ) -> bool {
        let Some(ws) = self.target_workspace_mut(window_scope) else {
            self.remove_session(session_id);
            return false;
        };
        match disposition {
            LaunchDisposition::NewTab => {
                ws.create_tab(session_id, tab_title);
                true
            }
            LaunchDisposition::Split(axis) => {
                if let Some((tab_id, target_pane)) =
                    ws.active_tab().map(|tab| (tab.id, tab.focused_pane))
                {
                    if ws.split_pane(tab_id, target_pane, axis, session_id).is_none() {
                        self.remove_session(session_id);
                        false
                    } else {
                        true
                    }
                } else {
                    ws.create_tab(session_id, tab_title);
                    true
                }
            }
        }
    }

    fn launch_profile_entry(
        &mut self,
        window_scope: Option<window::Id>,
        profile: &Profile,
        disposition: LaunchDisposition,
    ) -> Task<Message> {
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
                        let _ = self.open_session_with_disposition(
                            window_scope,
                            sid,
                            title,
                            disposition,
                        );
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
                keepalive_interval_secs,
                keepalive_max_missed,
                auto_attach_mux,
                mux_attach_command,
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
                    keepalive_interval_secs: *keepalive_interval_secs,
                    keepalive_max_missed: *keepalive_max_missed,
                    auto_attach_mux: *auto_attach_mux,
                    mux_attach_command: mux_attach_command.clone(),
                };
                let tab_title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                let spec = target.to_target_spec(&profile.name, 80, 24, &self.config);
                let sid = self.insert_ssh_session_slot(target, spec);
                if self.open_session_with_disposition(window_scope, sid, tab_title, disposition) {
                    self.start_ssh_connect(sid)
                } else {
                    Task::none()
                }
            }
        }
    }

    fn launch_quick_connect(
        &mut self,
        window_scope: Option<window::Id>,
        target: QuickConnectTarget,
    ) -> Task<Message> {
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
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
        };
        let spec = ssh_target.to_target_spec(&label, 80, 24, &self.config);
        let sid = self.insert_ssh_session_slot(ssh_target, spec);
        if self.open_session_with_disposition(
            window_scope,
            sid,
            Some(label),
            LaunchDisposition::NewTab,
        ) {
            self.start_ssh_connect(sid)
        } else {
            Task::none()
        }
    }

    fn reload_system_ssh_config(&mut self) {
        match load_system_ssh_hosts(&self.paths) {
            Ok(hosts) => {
                let entries = hosts.iter().map(SystemSshProfile::from).collect();
                self.openssh_hosts = hosts;
                self.sidebar.set_system_ssh_profiles(entries);
            }
            Err(err) => {
                warn!(error = %err, "failed to load read-only ~/.ssh/config");
                self.last_error = Some(err.to_string());
            }
        }
    }

    fn clone_system_ssh_to_profile(&mut self, alias: &str) {
        let host = self
            .openssh_hosts
            .iter()
            .find(|h| h.alias == alias)
            .cloned()
            .or_else(|| {
                self.sidebar
                    .system_ssh_profiles()
                    .iter()
                    .find(|h| h.alias == alias)
                    .map(SystemSshProfile::to_openssh_host)
            });

        let Some(host) = host else {
            self.last_error = Some(format!("unknown System SSH host: {alias}"));
            return;
        };

        match save_cloned_openssh_profile(&self.paths, &host, &default_os_username()) {
            Ok(profile) => {
                self.profiles.profiles.push(profile);
                self.last_error = None;
            }
            Err(err) => {
                error!(alias = %alias, error = %err, "failed to clone System SSH host to Profile");
                self.last_error = Some(err.to_string());
            }
        }
    }

    fn launch_system_ssh_entry(
        &mut self,
        window_scope: Option<window::Id>,
        alias: &str,
        disposition: LaunchDisposition,
    ) -> Task<Message> {
        let Some(host) = self
            .sidebar
            .system_ssh_profiles()
            .iter()
            .find(|h| h.alias == alias)
            .cloned()
        else {
            return Task::none();
        };
        let auth = match host
            .identity_file
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(path) => SshAuthMethod::PublicKey {
                key_path: Some(PathBuf::from(path)),
                vault_secret_ref: None,
            },
            None => SshAuthMethod::Agent,
        };
        let target = SshSessionTarget {
            profile_id: None,
            name: host.alias.clone(),
            host: host.host_name.unwrap_or_else(|| host.alias.clone()),
            port: host.port,
            username: host.user.unwrap_or_else(default_os_username),
            auth,
            proxy_jump: host.proxy_jump,
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
        };
        let spec = target.to_target_spec(&host.alias, 80, 24, &self.config);
        let sid = self.insert_ssh_session_slot(target, spec);
        if self.open_session_with_disposition(
            window_scope,
            sid,
            Some(host.alias),
            disposition,
        ) {
            self.start_ssh_connect(sid)
        } else {
            Task::none()
        }
    }

    fn handle_command_palette_confirm(&mut self, window_scope: Option<window::Id>) -> Task<Message> {
        match self.command_palette.mode() {
            CommandPaletteMode::QuickConnect => {
                if let Some(target) =
                    QuickConnectTarget::parse(self.command_palette.quick_connect_input())
                {
                    self.last_error = None;
                    self.command_palette.close();
                    self.launch_quick_connect(window_scope, target)
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
            StatusBarAction::ProbeAll => self.probe_and_reconnect_all(),
            StatusBarAction::ReconnectSession(session_id) => {
                self.reconnect_ssh_session(session_id)
            }
            StatusBarAction::CancelReconnect(session_id) => {
                self.cancel_ssh_reconnect(session_id);
                Task::none()
            }
            StatusBarAction::ToggleTunnelsView => {
                self.sidebar.set_open(true);
                self.sidebar
                    .set_section_collapsed(SidebarSection::ActiveTunnels, false);
                Task::none()
            }
            StatusBarAction::ReconnectTunnels => {
                self.reconnect_all_tunnels();
                Task::none()
            }
            StatusBarAction::ToggleTransferDrawer => self.toggle_sftp_drawer(),
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
            PaletteAction::OpenTabbyImporter => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenTabbyImportModal)
            }
            PaletteAction::OpenTunnelCreator => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenTunnelCreateModal)
            }
            PaletteAction::ReloadSystemSsh => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::ReloadSystemSshConfig)
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
            PaletteAction::ToggleSftpDrawer => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::ToggleSftpDrawer)
            }
            PaletteAction::CloseFocusedPane => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::CloseFocusedPane)
            }
            PaletteAction::QuickConnect(target) => {
                self.command_palette.close();
                self.launch_quick_connect(window_scope, target)
            }
            PaletteAction::ProbeReconnectAll => {
                self.command_palette.close();
                self.probe_and_reconnect_all()
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
                self.toggle_tunnel(&tunnel_id)
            }
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        self.ssh_ui.clear_session_prompts(session_id);
        let (_, tunnel_cleanup) = self
            .tunnel_manager
            .stop_session_tunnels_with_barrier(session_id.0);
        self.tunnel_ui.prune_session(session_id);
        match self.sessions.remove(&session_id) {
            Some(SessionEntry::Local(session)) => session.pty.terminate(),
            Some(SessionEntry::Ssh(slot)) => {
                self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                spawn_ssh_disconnect_after(tunnel_cleanup, slot.supervisor, slot.active);
            }
            Some(SessionEntry::Remote(_)) | None => {}
        }
        self.sync_tunnel_telemetry();
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
                        let tunnels_cleanup = self.tunnel_manager.stop_all_with_barrier();
                        self.sync_tunnel_telemetry();
                        let mut ssh_handles = Vec::new();
                        for entry in self.sessions.values_mut() {
                            if let SessionEntry::Ssh(slot) = entry {
                                self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                                if slot.supervisor.is_some() || slot.active.is_some() {
                                    ssh_handles.push((slot.supervisor.take(), slot.active.take()));
                                }
                            }
                        }
                        spawn_teardown_and_exit(async move {
                            tunnels_cleanup.await;
                            disconnect_ssh_handles(ssh_handles);
                        })
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
                self.transfer_queue.cancel_all();
                self.sync_transfer_telemetry();
                let tunnels_cleanup = self.tunnel_manager.stop_all_with_barrier();
                self.sync_tunnel_telemetry();
                self.background_work_source.stop_all();
                self.ipc_handle.take();
                self.retired_final_window = None;
                let session_ids: Vec<SessionId> = self.sessions.keys().copied().collect();
                let mut ssh_handles = Vec::new();
                for sid in session_ids {
                    self.ssh_ui.clear_session_prompts(sid);
                    self.tunnel_ui.prune_session(sid);
                    match self.sessions.remove(&sid) {
                        Some(SessionEntry::Local(session)) => session.pty.terminate(),
                        Some(SessionEntry::Ssh(slot)) => {
                            self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                            if slot.supervisor.is_some() || slot.active.is_some() {
                                ssh_handles.push((slot.supervisor, slot.active));
                            }
                        }
                        Some(SessionEntry::Remote(_)) | None => {}
                    }
                }
                spawn_teardown_and_exit(async move {
                    tunnels_cleanup.await;
                    disconnect_ssh_handles(ssh_handles);
                })
            }
            Message::FinalizeExit => {
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
                let targets = self
                    .workspace_for_session_mut(session_id)
                    .map(|ws| ws.input_targets_for_session(session_id))
                    .unwrap_or_else(|| vec![session_id]);
                if targets.len() == 1
                    && (bytes.contains(&b'\r') || bytes.contains(&b'\n'))
                    && let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id)
                    && slot.active.is_none()
                    && slot.transport_status == TransportStatus::Disconnected
                    && self.ssh_ui.mitm_alert(session_id).is_none()
                {
                    return self.reconnect_ssh_session(session_id);
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
                        if let Some(sup) = &slot.supervisor {
                            if let Err(err) = sup.resize(cols, rows) {
                                error!(
                                    session = session_id.0,
                                    error = %err,
                                    "failed to resize supervised SSH channel"
                                );
                                self.last_error = Some(err.to_string());
                            }
                        } else if let Some(ssh) = &slot.active {
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
                    Some(SessionEntry::Remote(_)) | None => {}
                }
                Task::none()
            }
            Message::PtyEvent { session_id, event } => {
                let is_active_session = Some(session_id) == self.active_session_id();
                if let Some(entry) = self.sessions.get_mut(&session_id) {
                    match event {
                        PtyEvent::Output => {
                            entry.refresh_snapshot();
                            if is_active_session
                                && self.search.open
                                && !self.search.query.is_empty()
                            {
                                let buf = entry.buffer().cloned();
                                self.search.refresh_matches_buffer(buf.as_deref(), false);
                            }
                        }
                        PtyEvent::Title(title) => {
                            if !title.is_empty() {
                                match entry {
                                    SessionEntry::Local(s) => s.title = title,
                                    SessionEntry::Ssh(s) => s.title = title,
                                    SessionEntry::Remote(_) => {}
                                }
                            }
                        }
                        PtyEvent::Bell => {}
                        PtyEvent::Exited(code) => {
                            entry.refresh_snapshot();
                            match entry {
                                SessionEntry::Local(s) => s.exited = Some(code),
                                SessionEntry::Ssh(slot) => {
                                    slot.exited = Some(code);
                                    slot.active = None;
                                    slot.transport_status = TransportStatus::Disconnected;
                                    slot.status_message =
                                        format_ssh_slot_message(&slot.target, &slot.transport_status);
                                }
                                SessionEntry::Remote(_) => {}
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
                    .and_then(SessionEntry::buffer)
                    .cloned();
                let target = self.search.open_buffer(active_buf.as_deref());
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
                    .and_then(SessionEntry::buffer)
                    .cloned();
                let target = self.search.set_query_buffer(query, active_buf.as_deref());
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
                if self.tabby_import.is_open() {
                    self.tabby_import.close();
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
                if let Some(entry) = self.sessions.get_mut(&session_id)
                    && let Some(buf) = entry.buffer().cloned()
                {
                    buf.scroll_display(Scroll::Delta(delta_lines));
                    entry.refresh_snapshot();
                }
                Task::none()
            }
            Message::ScrollToTop(session_id) => {
                if let Some(entry) = self.sessions.get_mut(&session_id)
                    && let Some(buf) = entry.buffer().cloned()
                {
                    buf.scroll_display(Scroll::Top);
                    entry.refresh_snapshot();
                }
                Task::none()
            }
            Message::ScrollToBottom(session_id) => {
                if let Some(entry) = self.sessions.get_mut(&session_id)
                    && let Some(buf) = entry.buffer().cloned()
                {
                    buf.scroll_to_bottom();
                    entry.refresh_snapshot();
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
                if let Some(entry) = self.sessions.get_mut(&session_id)
                    && let Some(buf) = entry.buffer().cloned()
                {
                    buf.start_selection(col, row, side, selection_type);
                    entry.refresh_snapshot();
                }
                Task::none()
            }
            Message::SelectionUpdate {
                session_id,
                col,
                row,
                side,
            } => {
                if let Some(entry) = self.sessions.get_mut(&session_id)
                    && let Some(buf) = entry.buffer().cloned()
                {
                    buf.update_selection(col, row, side);
                    entry.refresh_snapshot();
                }
                Task::none()
            }
            Message::SelectionFinish { session_id } => {
                let copied = if let Some(entry) = self.sessions.get_mut(&session_id)
                    && let Some(buf) = entry.buffer().cloned()
                {
                    let selected = buf.finish_selection();
                    entry.refresh_snapshot();
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
                    .and_then(SessionEntry::buffer)
                    .and_then(|buf| buf.selection_text());
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
                    .and_then(SessionEntry::as_ssh)
                    .is_some();
                let action = classify_file_drop(session_id, &path, is_ssh, shift_held, false);
                match action {
                    FileDropAction::InsertShellPath {
                        session_id,
                        escaped_text,
                    } => {
                        self.write_bytes_to_session(session_id, escaped_text.as_bytes());
                    }
                    FileDropAction::UploadToSftp { session_id, path } => {
                        self.enqueue_sftp_upload(session_id, path);
                    }
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
                        ConfigFileKind::Profiles => {
                            let reloaded = self
                                .reload_ui
                                .reload_profiles(&self.paths, &mut self.profiles);
                            if matches!(reloaded, Ok(true)) {
                                self.sync_tunnel_telemetry();
                            }
                            reloaded
                        }
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
            Message::OpenSshProfile(profile) => {
                match SshTargetSpec::from_profile_with_config(
                    &profile,
                    &self.config,
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
                match SshTargetSpec::from_profile_with_config(
                    &profile,
                    &self.config,
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
            Message::SshEvent { session_id, event } => self.handle_ssh_event(session_id, event),
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
                self.openssh_hosts = entries
                    .iter()
                    .map(SystemSshProfile::to_openssh_host)
                    .collect();
                self.sidebar.set_system_ssh_profiles(entries);
                Task::none()
            }
            Message::ReloadSystemSshConfig => {
                self.reload_system_ssh_config();
                Task::none()
            }
            Message::CloneSystemSshToProfile(alias) => {
                self.clone_system_ssh_to_profile(&alias);
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
                    self.launch_profile_entry(window_scope, &profile, disposition)
                } else {
                    self.last_error = Some(format!("unknown profile id: {profile_id}"));
                    Task::none()
                }
            }
            Message::LaunchSystemSsh { alias, disposition } => {
                self.command_palette.close();
                self.launch_system_ssh_entry(window_scope, &alias, disposition)
            }
            Message::SetSessionTransportStatus { session_id, status } => {
                self.update_ssh_slot_status(session_id, status);
                Task::none()
            }
            Message::ConnectSshSession(session_id) => self.reconnect_ssh_session(session_id),
            Message::DisconnectSshSession(session_id) => {
                self.disconnect_ssh_session(session_id);
                Task::none()
            }
            Message::ProbeAndReconnectAll => self.probe_and_reconnect_all(),
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
                self.launch_quick_connect(window_scope, target)
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
            Message::OpenTabbyImportModal => {
                self.command_palette.close();
                self.tabby_import.open();
                operation::focus(tabby_import_path_input_id())
            }
            Message::CloseTabbyImportModal => {
                self.tabby_import.close();
                Task::none()
            }
            Message::TabbyImportPathChanged(path) => {
                self.tabby_import.set_path_input(path);
                Task::none()
            }
            Message::SubmitTabbyImport => {
                let path = PathBuf::from(self.tabby_import.path_input().trim());
                self.run_tabby_import(path);
                Task::none()
            }
            Message::ImportTabbyConfigFromPath(path) => {
                self.command_palette.close();
                self.tabby_import
                    .open_with_path(path.to_string_lossy().into_owned());
                self.run_tabby_import(path);
                Task::none()
            }
            Message::TabbyVaultCredentialChanged { profile_id, value } => {
                self.tabby_import.set_credential_draft(&profile_id, value);
                Task::none()
            }
            Message::SaveTabbyVaultCredential(profile_id) => {
                self.save_tabby_vault_credential(&profile_id);
                Task::none()
            }
            Message::ToggleSftpDrawer => self.toggle_sftp_drawer(),
            Message::SetSftpDrawerOpen(open) => self.set_sftp_drawer_open(open),
            Message::ResizeSftpDrawer(width) => {
                self.sftp_drawer.set_width(width);
                Task::none()
            }
            Message::SftpPathInputChanged(path) => {
                self.sftp_drawer.set_path_input(path);
                Task::none()
            }
            Message::SftpNavigateTo(path) => self.navigate_sftp_to(path),
            Message::SftpNavigateUp => {
                let parent = parent_remote_path(self.sftp_drawer.current_path());
                self.navigate_sftp_to(parent)
            }
            Message::SftpRefresh => {
                let path = self.sftp_drawer.current_path().to_string();
                self.navigate_sftp_to(path)
            }
            Message::SftpNewFolderInputChanged(name) => {
                self.sftp_drawer.set_new_folder_name(name);
                Task::none()
            }
            Message::SftpCreateFolder => self.create_sftp_folder(),
            Message::SftpDeleteEntry { path, is_dir } => self.delete_sftp_entry(path, is_dir),
            Message::SftpDownloadFile {
                remote_path,
                file_name,
            } => {
                self.enqueue_sftp_download(remote_path, file_name);
                Task::none()
            }
            Message::SftpDrawerFileDropped { path } => {
                if let Some(session_id) = self.resolve_sftp_session_id() {
                    let action =
                        classify_file_drop(session_id, &path, true, false, true);
                    if let FileDropAction::UploadToSftp { session_id, path } = action {
                        self.enqueue_sftp_upload(session_id, path);
                    }
                } else {
                    self.sftp_drawer.set_browser_error(Some(
                        "Connect an SSH Session before dropping files into the SFTP Drawer."
                            .to_string(),
                    ));
                }
                Task::none()
            }
            Message::SftpBrowseResult {
                session_id,
                generation,
                result,
            } => {
                self.sftp_drawer
                    .apply_browse_result(session_id, generation, result);
                Task::none()
            }
            Message::SftpMutationResult { session_id, result } => match result {
                Ok(()) => {
                    let path = self.sftp_drawer.current_path().to_string();
                    self.sftp_refresh_task(session_id, path)
                }
                Err(err) => {
                    self.sftp_drawer.set_browser_error(Some(err));
                    Task::none()
                }
            },
            Message::TransferQueueUpdated(snap) => {
                self.sftp_drawer.set_queue_snapshot(snap);
                self.sync_transfer_telemetry();
                Task::none()
            }
            Message::PauseTransfer(id) => {
                self.transfer_queue.pause(id);
                self.sync_transfer_telemetry();
                Task::none()
            }
            Message::ResumeTransfer(id) => {
                self.transfer_queue.resume(id);
                self.sync_transfer_telemetry();
                Task::none()
            }
            Message::CancelTransfer(id) => {
                self.transfer_queue.cancel(id);
                self.sync_transfer_telemetry();
                Task::none()
            }
            Message::RetryTransfer(id) => {
                self.transfer_queue.retry(id);
                self.sync_transfer_telemetry();
                Task::none()
            }
            Message::ClearFinishedTransfers => {
                self.transfer_queue.clear_finished();
                self.sync_transfer_telemetry();
                Task::none()
            }
            Message::OpenTunnelCreateModal => {
                self.command_palette.close();
                let default_session = self
                    .active_session_id()
                    .filter(|sid| self.sessions.get(sid).and_then(SessionEntry::as_ssh).is_some());
                let default_profile = default_session
                    .and_then(|sid| self.sessions.get(&sid).and_then(SessionEntry::as_ssh))
                    .and_then(|slot| slot.target.profile_id.clone())
                    .or_else(|| {
                        self.profiles
                            .profiles
                            .iter()
                            .find(|p| matches!(p.target, ProfileTarget::Ssh { .. }))
                            .map(|p| p.id.clone())
                    });
                self.tunnel_ui
                    .create_form_mut()
                    .open_for(default_profile, default_session);
                operation::focus(tunnel_name_input_id())
            }
            Message::CloseTunnelCreateModal => {
                self.tunnel_ui.create_form_mut().close();
                Task::none()
            }
            Message::TunnelFormDirectionChanged(dir) => {
                let form = self.tunnel_ui.create_form_mut();
                form.direction = dir;
                form.validation_error = None;
                if dir == TunnelDirection::Dynamic && form.bind_port == "5432" {
                    form.bind_port = "1080".to_string();
                } else if dir != TunnelDirection::Dynamic && form.bind_port == "1080" {
                    form.bind_port = "5432".to_string();
                }
                Task::none()
            }
            Message::TunnelFormNameChanged(val) => {
                let form = self.tunnel_ui.create_form_mut();
                form.name = val;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormProfileSelected(pid) => {
                let form = self.tunnel_ui.create_form_mut();
                form.profile_id = pid;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormBindHostChanged(val) => {
                let form = self.tunnel_ui.create_form_mut();
                form.bind_host = val;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormBindPortChanged(val) => {
                let form = self.tunnel_ui.create_form_mut();
                form.bind_port = val;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormTargetHostChanged(val) => {
                let form = self.tunnel_ui.create_form_mut();
                form.target_host = val;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormTargetPortChanged(val) => {
                let form = self.tunnel_ui.create_form_mut();
                form.target_port = val;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormStandaloneToggled(standalone) => {
                let form = self.tunnel_ui.create_form_mut();
                form.standalone = standalone;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormHoldGraceChanged(val) => {
                let form = self.tunnel_ui.create_form_mut();
                form.hold_grace_secs = val;
                form.validation_error = None;
                Task::none()
            }
            Message::SubmitTunnelCreate => self.submit_tunnel_create(),
            Message::TunnelPublicBindConfirm => self.confirm_public_bind(),
            Message::TunnelPublicBindCancel => {
                self.tunnel_ui.cancel_public_bind();
                Task::none()
            }
            Message::ToggleTunnel(tunnel_id) => self.toggle_tunnel(&tunnel_id),
            Message::ReconnectTunnel(tunnel_id) => {
                self.reconnect_tunnel(&tunnel_id);
                Task::none()
            }
            Message::ReconnectAllTunnels => {
                self.reconnect_all_tunnels();
                Task::none()
            }
            Message::TunnelManagerUpdated(_snap) => {
                self.sync_tunnel_telemetry();
                Task::none()
            }
        }
    }

    fn run_tabby_import(&mut self, path: PathBuf) {
        if path.as_os_str().is_empty() {
            self.tabby_import
                .set_import_error("Please enter the path to Tabby's config.yaml file");
            return;
        }
        match import_tabby_config_file_into_profiles(
            &path,
            &self.paths,
            &default_os_username(),
            Some(self.vault.as_ref()),
        ) {
            Ok(outcome) => {
                self.profiles = outcome.updated_profiles.clone();
                self.tabby_import.record_outcome(path, &outcome);
                self.last_error = None;
                self.sync_tunnel_telemetry();
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "Tabby config.yaml import failed");
                self.tabby_import.set_import_error(err.to_string());
            }
        }
    }

    fn save_tabby_vault_credential(&mut self, profile_id: &str) {
        let Some((secret_ref, draft)) = self.tabby_import.credential_entry(profile_id) else {
            return;
        };
        let secret_ref = secret_ref.to_string();
        let draft = zeroize::Zeroizing::new(draft.to_string());

        if draft.is_empty() {
            self.tabby_import
                .set_credential_error(profile_id, "Credential must not be empty");
            return;
        }
        if self.vault.requires_unlock() {
            self.tabby_import.set_credential_error(
                profile_id,
                "Termewnal Vault is locked — unlock Vault first before saving secrets",
            );
            return;
        }
        match self.vault.set_secret(&secret_ref, draft.as_str()) {
            Ok(()) => self.tabby_import.mark_credential_saved(profile_id),
            Err(err) => self
                .tabby_import
                .set_credential_error(profile_id, err.to_string()),
        }
    }

    fn handle_ssh_event(&mut self, session_id: SessionId, event: SshUiEvent) -> Task<Message> {
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id) else {
            return Task::none();
        };

        let event_gen = match &event {
            SshUiEvent::Connected { generation, .. }
            | SshUiEvent::TransportStatus { generation, .. }
            | SshUiEvent::HostKeyPrompt { generation, .. }
            | SshUiEvent::AuthPrompt { generation, .. }
            | SshUiEvent::MitmBlocked { generation, .. }
            | SshUiEvent::Disconnected { generation, .. } => *generation,
        };
        if event_gen != slot.generation {
            return Task::none();
        }

        match event {
            SshUiEvent::Connected {
                session: ssh,
                supervisor,
                ..
            } => {
                if let Some(ref session) = ssh {
                    self.sftp_manager.register_primary_transport(
                        slot.sftp_key(),
                        Arc::clone(session.primary_transport()),
                    );
                }
                slot.active = ssh;
                slot.supervisor = supervisor;
                slot.last_error = None;
                slot.exited = None;
                slot.transport_status = TransportStatus::Connected { latency_ms: None };
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                slot.snapshot = Arc::new(slot.buffer.snapshot());
                let profile_id = slot.target.profile_id.clone();
                let supervisor_handle = slot.supervisor.clone();
                let mut tasks = Vec::new();
                if let Some(sup) = supervisor_handle {
                    tasks.push(self.auto_start_session_tunnels(
                        session_id,
                        profile_id.as_deref(),
                        &sup,
                    ));
                } else {
                    self.sync_tunnel_telemetry();
                }
                if self.sftp_drawer.is_open()
                    && (self.sftp_drawer.bound_session_id().is_none()
                        || self.sftp_drawer.bound_session_id() == Some(session_id))
                {
                    let path = self.sftp_drawer.current_path().to_string();
                    tasks.push(self.sftp_refresh_task(session_id, path));
                }
                Task::batch(tasks)
            }
            SshUiEvent::TransportStatus {
                status, last_error, ..
            } => {
                if !matches!(
                    status,
                    TransportStatus::Connected { .. } | TransportStatus::Degraded { .. }
                ) {
                    self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                    slot.active = None;
                }
                if matches!(status, TransportStatus::Connected { .. }) {
                    slot.last_error = None;
                    slot.exited = None;
                    if let Some(sup) = &slot.supervisor {
                        if let Some(transport) = sup.current_transport() {
                            self.sftp_manager.register_primary_transport(
                                slot.sftp_key(),
                                Arc::clone(&transport),
                            );
                            slot.active = Some(Arc::new(SshSession::from_supervised(transport, sup)));
                        }
                    }
                } else if let Some(err) = last_error {
                    slot.last_error = Some(err.clone());
                    self.last_error = Some(err);
                }
                slot.transport_status = status;
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                slot.snapshot = Arc::new(slot.buffer.snapshot());
                Task::none()
            }
            SshUiEvent::HostKeyPrompt {
                prompt, responder, ..
            } => {
                self.ssh_ui
                    .push_host_key_prompt(session_id, prompt, responder);
                Task::none()
            }
            SshUiEvent::AuthPrompt {
                prompt, responder, ..
            } => {
                self.ssh_ui.push_auth_prompt(session_id, prompt, responder);
                Task::none()
            }
            SshUiEvent::MitmBlocked { alert, .. } => {
                self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                slot.active = None;
                slot.supervisor = None;
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
                Task::none()
            }
            SshUiEvent::Disconnected { error, .. } => {
                self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                slot.active = None;
                slot.supervisor = None;
                slot.transport_status = TransportStatus::Disconnected;
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                self.ssh_ui.clear_session_prompts(session_id);
                if let Some(err) = error {
                    slot.last_error = Some(err.clone());
                    slot.status_message = err.clone();
                    self.last_error = Some(err);
                }
                Task::none()
            }
        }
    }

    fn sync_terminal_options_to_sessions(&mut self) {
        let shape = cursor_style_to_shape(self.config.terminal.cursor_style);
        let scrollback = self.config.terminal.scrollback_lines;
        let app_cfg = self.config.clone();
        for entry in self.sessions.values_mut() {
            if let Some(buf) = entry.buffer() {
                buf.apply_options(scrollback, shape);
                entry.refresh_snapshot();
            }
            if let SessionEntry::Ssh(slot) = entry
                && let Some(sup) = &slot.supervisor
            {
                let (ka_secs, ka_max) = slot.target.resolve_keepalive(&app_cfg);
                sup.update_keepalive(Duration::from_secs(ka_secs), ka_max);
            }
        }
        if self.search.open && !self.search.query.is_empty() {
            let active_buf = self
                .active_session_id()
                .and_then(|id| self.sessions.get(&id))
                .and_then(SessionEntry::buffer)
                .cloned();
            self.search
                .refresh_matches_buffer(active_buf.as_deref(), false);
        }
    }

    fn focus_search_match(&mut self, target: Option<GridSearchMatch>) {
        if let Some(matched) = target
            && let Some(entry) = self
                .active_session_id()
                .and_then(|id| self.sessions.get_mut(&id))
            && let Some(buf) = entry.buffer().cloned()
        {
            buf.scroll_to_match(matched);
            entry.refresh_snapshot();
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
        let Some(buf) = self
            .sessions
            .get(&session_id)
            .and_then(SessionEntry::buffer)
        else {
            return;
        };
        let bytes = format_paste_bytes(text, buf.is_bracketed_paste());
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
            if is_broadcast {
                let skip = match self.sessions.get(&target_id) {
                    Some(SessionEntry::Local(s)) => s.exited.is_some(),
                    Some(SessionEntry::Ssh(s)) => {
                        s.active.is_none()
                            && !(s.supervisor.is_some()
                                && matches!(
                                    s.transport_status,
                                    TransportStatus::Connected { .. }
                                        | TransportStatus::Degraded { .. }
                                ))
                    }
                    Some(SessionEntry::Remote(_)) | None => true,
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
        let Some(entry) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let write_result = match entry {
            SessionEntry::Local(session) => session.pty.write_bytes(bytes).map_err(|e| e.to_string()),
            SessionEntry::Ssh(slot) => {
                if let Some(sup) = &slot.supervisor {
                    if matches!(
                        slot.transport_status,
                        TransportStatus::Connected { .. } | TransportStatus::Degraded { .. }
                    ) {
                        sup.write_bytes(bytes).map_err(|e| e.to_string())
                    } else {
                        return;
                    }
                } else if let Some(ssh) = &slot.active {
                    ssh.write_bytes(bytes).map_err(|e| e.to_string())
                } else {
                    return;
                }
            }
            SessionEntry::Remote(_) => return,
        };
        if let Err(err) = write_result {
            error!(session = session_id.0, error = %err, "failed to write terminal input");
            self.last_error = Some(err);
        } else if let Some(buf) = entry.buffer().cloned()
            && buf.display_offset() > 0
        {
            buf.scroll_to_bottom();
            entry.refresh_snapshot();
        }
    }

    /// Renders the workspace tab strip, collapsible Left Sidebar, status/reload/Vault/SSH banners,
    /// active split panes, Status Bar, Search/Paste overlays, Command Palette, and Close Guard modal.
    pub fn view(&self, window_id: window::Id) -> Element<'_, Message, Theme, iced::Renderer> {
        let ws = self.window_workspaces.get(&window_id).map(|w| &w.workspace);
        let mut main_pane_col = column![self.render_tab_strip(ws)]
            .width(Length::Fill)
            .height(Length::Fill);

        if let Some(banner_text) = ws.and_then(Self::broadcast_banner_for_workspace) {
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

        let body = match ws.and_then(|w| w.active_tab()) {
            Some(tab) => self.render_tab_body(tab),
            None => self.render_empty_workspace(),
        };
        main_pane_col = main_pane_col.push(body);

        let main_or_sidebar: Element<'_, Message, Theme, iced::Renderer> = if self.sidebar.is_open()
        {
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

        let workspace_row: Element<'_, Message, Theme, iced::Renderer> = if self.sftp_drawer.is_open()
        {
            let has_connected_ssh = self.resolve_sftp_session_id().is_some_and(|sid| {
                self.sessions
                    .get(&sid)
                    .and_then(SessionEntry::as_ssh)
                    .is_some_and(|slot| slot.active.is_some())
            });
            SftpDrawerDock::new(
                self.sftp_drawer.width(),
                main_or_sidebar,
                render_sftp_drawer(&self.sftp_drawer, has_connected_ssh),
            )
        } else {
            main_or_sidebar
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
        if self.tabby_import.is_open() {
            layers = layers.push(self.tabby_import.view(&palette, self.vault.requires_unlock()));
        }
        if let Some(tunnel_modal) = self
            .tunnel_ui
            .view_create_modal(&self.profiles.profiles, &palette)
        {
            layers = layers.push(tunnel_modal);
        }
        if let Some(bind_warning) = self.tunnel_ui.view_public_bind_warning(&palette) {
            layers = layers.push(bind_warning);
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
        let terminal_focused =
            focused && !self.command_palette.is_open() && !self.tabby_import.is_open();
        let ime_enabled = !self.command_palette.is_open()
            && !self.tabby_import.is_open()
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
            Some(SessionEntry::Ssh(slot)) => {
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

        let pty_subs = self.sessions.iter().filter_map(|(&session_id, entry)| {
            let buffer = entry.buffer()?;
            Some(Subscription::run_with(
                PtySubKey {
                    session_id,
                    buffer: Arc::clone(buffer),
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

        let transfer_sub = Subscription::run_with(
            TransferSubKey {
                queue: self.transfer_queue.clone(),
            },
            transfer_queue_stream,
        );
        let tunnel_sub = Subscription::run_with(
            TunnelSubKey {
                manager: self.tunnel_manager.clone(),
            },
            tunnel_manager_stream,
        );

        Subscription::batch(
            [
                close_requests,
                closed,
                win_events,
                shortcuts,
                config_watch,
                transfer_sub,
                tunnel_sub,
            ]
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

    /// Returns the Tabby `config.yaml` importer modal state.
    pub fn tabby_import(&self) -> &TabbyImportUiState {
        &self.tabby_import
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

    /// Returns the SSH TOFU, MITM, and interactive auth prompt UI state.
    pub fn ssh_ui(&self) -> &SshUiState {
        &self.ssh_ui
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

    /// Returns the SFTP File Browser & `Transfer` Queue drawer state.
    pub fn sftp_drawer(&self) -> &SftpDrawerState {
        &self.sftp_drawer
    }

    /// Overrides the local destination directory for SFTP downloads (useful in tests).
    pub fn set_sftp_download_dir(&mut self, dir: PathBuf) {
        self.sftp_drawer.set_download_dir(dir);
    }

    /// Returns the shared [`SftpManager`].
    pub fn sftp_manager(&self) -> &SftpManager {
        &self.sftp_manager
    }

    /// Returns the shared [`TransferQueue`].
    pub fn transfer_queue(&self) -> &TransferQueue {
        &self.transfer_queue
    }

    /// Returns the shared [`TunnelManager`].
    pub fn tunnel_manager(&self) -> &TunnelManager {
        &self.tunnel_manager
    }

    /// Returns the `Tunnel` creation & public-bind opt-in UI state.
    pub fn tunnel_ui(&self) -> &TunnelUiState {
        &self.tunnel_ui
    }

    /// Overrides the `known_hosts` path used by SSH connections and Standalone `Tunnel`s (for tests).
    pub fn set_known_hosts_path(&mut self, path: impl Into<PathBuf>) {
        self.known_hosts_override = Some(path.into());
    }

    fn resolve_sftp_session_id(&self) -> Option<SessionId> {
        if let Some(active_sid) = self.active_session_id()
            && self
                .sessions
                .get(&active_sid)
                .and_then(SessionEntry::as_ssh)
                .is_some()
        {
            return Some(active_sid);
        }
        if let Some(bound_sid) = self.sftp_drawer.bound_session_id()
            && self
                .sessions
                .get(&bound_sid)
                .and_then(SessionEntry::as_ssh)
                .is_some()
        {
            return Some(bound_sid);
        }
        self.sessions
            .iter()
            .find_map(|(&sid, entry)| {
                entry
                    .as_ssh()
                    .filter(|slot| slot.active.is_some())
                    .map(|_| sid)
            })
            .or_else(|| {
                self.sessions
                    .iter()
                    .find_map(|(&sid, entry)| entry.as_ssh().map(|_| sid))
            })
    }

    fn toggle_sftp_drawer(&mut self) -> Task<Message> {
        let next = !self.sftp_drawer.is_open();
        self.set_sftp_drawer_open(next)
    }

    fn set_sftp_drawer_open(&mut self, open: bool) -> Task<Message> {
        self.sftp_drawer.set_open(open);
        let mut transfers = self.status_bar.transfers().clone();
        transfers.drawer_open = open;
        self.status_bar.set_transfers(transfers);
        if open && let Some(sid) = self.resolve_sftp_session_id() {
            self.sftp_drawer.bind_session(Some(sid));
            let path = self.sftp_drawer.current_path().to_string();
            return self.sftp_refresh_task(sid, path);
        }
        Task::none()
    }

    fn navigate_sftp_to(&mut self, path: String) -> Task<Message> {
        let Some(sid) = self.resolve_sftp_session_id() else {
            self.sftp_drawer.set_current_path(path);
            return Task::none();
        };
        self.sftp_drawer.bind_session(Some(sid));
        self.sftp_refresh_task(sid, path)
    }

    fn sftp_refresh_task(&mut self, session_id: SessionId, path: String) -> Task<Message> {
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
            return Task::none();
        };
        if slot.active.is_none() {
            self.sftp_drawer.set_current_path(path);
            return Task::none();
        }
        let endpoint = slot.sftp_endpoint(&self.vault);
        let generation = self.sftp_drawer.begin_listing(session_id, &path);
        let target_path = self.sftp_drawer.current_path().to_string();
        let manager = self.sftp_manager.clone();
        Task::perform(
            async move {
                manager
                    .list_dir(&endpoint, &target_path)
                    .await
                    .map_err(|err| err.to_string())
            },
            move |result| Message::SftpBrowseResult {
                session_id,
                generation,
                result,
            },
        )
    }

    fn create_sftp_folder(&mut self) -> Task<Message> {
        let folder = self.sftp_drawer.new_folder_name().trim().to_string();
        if folder.is_empty() {
            return Task::none();
        }
        let Some(session_id) = self.resolve_sftp_session_id() else {
            self.sftp_drawer.set_browser_error(Some(
                "Connect an SSH Session before creating a remote folder.".to_string(),
            ));
            return Task::none();
        };
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
            return Task::none();
        };
        if slot.active.is_none() {
            self.sftp_drawer.set_browser_error(Some(
                "Connect an SSH Session before creating a remote folder.".to_string(),
            ));
            return Task::none();
        }
        let target = join_remote_path(self.sftp_drawer.current_path(), &folder);
        self.sftp_drawer.set_new_folder_name(String::new());
        let endpoint = slot.sftp_endpoint(&self.vault);
        let manager = self.sftp_manager.clone();
        Task::perform(
            async move {
                manager
                    .mkdir(&endpoint, &target)
                    .await
                    .map_err(|err| err.to_string())
            },
            move |result| Message::SftpMutationResult { session_id, result },
        )
    }

    fn delete_sftp_entry(&mut self, path: String, is_dir: bool) -> Task<Message> {
        let Some(session_id) = self.resolve_sftp_session_id() else {
            return Task::none();
        };
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
            return Task::none();
        };
        if slot.active.is_none() {
            return Task::none();
        }
        let endpoint = slot.sftp_endpoint(&self.vault);
        let manager = self.sftp_manager.clone();
        Task::perform(
            async move {
                let res = if is_dir {
                    manager.remove_dir(&endpoint, &path).await
                } else {
                    manager.remove_file(&endpoint, &path).await
                };
                res.map_err(|err| err.to_string())
            },
            move |result| Message::SftpMutationResult { session_id, result },
        )
    }

    fn enqueue_sftp_upload(&mut self, session_id: SessionId, local_path: PathBuf) {
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
            return;
        };
        let file_name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload.bin".to_string());
        let remote_dir = self.sftp_drawer.target_remote_dir_for(session_id);
        let remote_path = if remote_dir == "." || remote_dir.is_empty() {
            file_name
        } else {
            join_remote_path(&remote_dir, &file_name)
        };
        let endpoint = slot.sftp_endpoint(&self.vault);
        self.transfer_queue
            .enqueue_upload(endpoint, local_path, remote_path);
        self.sync_transfer_telemetry();
    }

    fn enqueue_sftp_download(&mut self, remote_path: String, file_name: String) {
        let Some(session_id) = self.resolve_sftp_session_id() else {
            self.sftp_drawer.set_browser_error(Some(
                "Connect an SSH Session before downloading files.".to_string(),
            ));
            return;
        };
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
            return;
        };
        let local_path = self.sftp_drawer.download_dir().join(&file_name);
        let endpoint = slot.sftp_endpoint(&self.vault);
        self.transfer_queue
            .enqueue_download(endpoint, remote_path, local_path);
        self.sync_transfer_telemetry();
    }

    fn sync_transfer_telemetry(&mut self) {
        let snap = self.transfer_queue.snapshot();
        self.background_work_tracker
            .set_active_transfers(snap.active_count + snap.queued_count);
        self.status_bar.set_transfers(TransferSummaryHook {
            active: snap.active_count,
            queued: snap.queued_count,
            failed: snap.failed_count,
            bytes_done: snap.bytes_done,
            bytes_total: snap.bytes_total,
            bytes_per_sec: (snap.total_bytes_per_sec > 0).then_some(snap.total_bytes_per_sec),
            drawer_open: self.sftp_drawer.is_open(),
        });
        self.sftp_drawer.set_queue_snapshot(snap);
    }

    fn sync_tunnel_telemetry(&mut self) {
        let mgr_snap = self.tunnel_manager.snapshot();
        if mgr_snap.active_standalone_count != self.synced_standalone_tunnels {
            self.synced_standalone_tunnels = mgr_snap.active_standalone_count;
            self.background_work_tracker
                .set_active_standalone_tunnels(mgr_snap.active_standalone_count);
        }

        let open_ssh: BTreeMap<SessionId, Option<String>> = self
            .sessions
            .iter()
            .filter_map(|(&sid, entry)| {
                entry
                    .as_ssh()
                    .map(|slot| (sid, slot.target.profile_id.clone()))
            })
            .collect();

        let entries = build_active_tunnel_entries(
            &self.profiles.profiles,
            &open_ssh,
            &mgr_snap,
            &self.tunnel_ui,
        );
        self.status_bar
            .set_tunnels(TunnelSummaryHook::from_entries(&entries));
        self.sidebar.set_active_tunnels(entries);
    }

    fn find_tunnel_definition(
        &self,
        tunnel_id: &str,
    ) -> Option<(TunnelRule, Option<String>, Option<SessionId>)> {
        for profile in &self.profiles.profiles {
            if let Some(rule) = profile.tunnels.iter().find(|r| r.id == tunnel_id) {
                let sid = self.sessions.iter().find_map(|(&sid, entry)| {
                    entry
                        .as_ssh()
                        .filter(|slot| slot.target.profile_id.as_deref() == Some(profile.id.as_str()))
                        .map(|_| sid)
                });
                return Some((rule.clone(), Some(profile.id.clone()), sid));
            }
        }
        self.tunnel_ui.find_ad_hoc(tunnel_id).map(|entry| {
            (
                entry.rule.clone(),
                entry.profile_id.clone(),
                entry.session_id,
            )
        })
    }

    fn submit_tunnel_create(&mut self) -> Task<Message> {
        let rule = match self.tunnel_ui.create_form().build_rule() {
            Ok(rule) => rule,
            Err(err) => {
                self.tunnel_ui.create_form_mut().validation_error = Some(err);
                return Task::none();
            }
        };

        let profile_id = self.tunnel_ui.create_form().profile_id.clone();
        let session_id = self.tunnel_ui.create_form().session_id.or_else(|| {
            profile_id.as_deref().and_then(|pid| {
                self.sessions.iter().find_map(|(&sid, entry)| {
                    entry
                        .as_ssh()
                        .filter(|slot| slot.target.profile_id.as_deref() == Some(pid))
                        .map(|_| sid)
                })
            })
        });

        let saved_to_profile = if let Some(pid) = profile_id.as_deref()
            && let Some(profile) = self.profiles.profiles.iter_mut().find(|p| p.id == pid)
        {
            profile.tunnels.push(rule.clone());
            true
        } else {
            false
        };

        if saved_to_profile {
            if let Err(err) = save_profiles(&self.paths, &self.profiles) {
                warn!(error = %err, "failed saving tunnel rule to profiles.toml");
                self.last_error = Some(err.to_string());
            }
        } else {
            self.tunnel_ui.add_ad_hoc(AdHocTunnelRule {
                rule: rule.clone(),
                profile_id: profile_id.clone(),
                session_id,
            });
        }

        self.tunnel_ui.create_form_mut().close();

        if rule.requires_public_bind_opt_in() {
            self.tunnel_ui
                .prompt_public_bind(rule, profile_id, session_id, true);
            self.sync_tunnel_telemetry();
            return Task::none();
        }

        self.start_tunnel_rule(rule, profile_id, session_id)
    }

    fn confirm_public_bind(&mut self) -> Task<Message> {
        let Some(pending) = self.tunnel_ui.confirm_public_bind() else {
            return Task::none();
        };

        if let Some(pid) = pending.profile_id.as_deref()
            && let Some(profile) = self.profiles.profiles.iter_mut().find(|p| p.id == pid)
            && let Some(rule) = profile
                .tunnels
                .iter_mut()
                .find(|r| r.id == pending.rule.id)
        {
            rule.allow_public_bind = true;
            if let Err(err) = save_profiles(&self.paths, &self.profiles) {
                warn!(error = %err, "failed persisting allow_public_bind opt-in");
                self.last_error = Some(err.to_string());
            }
        }

        if pending.start_after_confirm {
            self.start_tunnel_rule(pending.rule, pending.profile_id, pending.session_id)
        } else {
            self.sync_tunnel_telemetry();
            Task::none()
        }
    }

    fn toggle_tunnel(&mut self, tunnel_id: &str) -> Task<Message> {
        let snap = self.tunnel_manager.snapshot();
        if let Some(existing) = snap.tunnels.iter().find(|t| t.id == tunnel_id)
            && existing.state.is_running()
        {
            self.tunnel_ui.mark_manually_stopped(tunnel_id, true);
            self.tunnel_ui.set_start_error(tunnel_id, None);
            self.tunnel_manager.stop_tunnel(tunnel_id);
            self.sync_tunnel_telemetry();
            return Task::none();
        }

        let Some((rule, profile_id, session_id)) = self.find_tunnel_definition(tunnel_id) else {
            return Task::none();
        };

        if rule.requires_public_bind_opt_in() {
            self.tunnel_ui
                .prompt_public_bind(rule, profile_id, session_id, true);
            return Task::none();
        }

        self.start_tunnel_rule(rule, profile_id, session_id)
    }

    fn reconnect_tunnel(&mut self, tunnel_id: &str) {
        let snap = self.tunnel_manager.snapshot();
        if let Some(existing) = snap.tunnels.iter().find(|t| t.id == tunnel_id) {
            if matches!(existing.state, termewnal_ssh::TunnelState::Retrying { .. }) {
                let _ = self.tunnel_manager.reconnect_tunnel(tunnel_id);
                return;
            }
            if existing.standalone {
                if existing.state.is_running() {
                    let _ = self.tunnel_manager.reconnect_tunnel(tunnel_id);
                    return;
                }
            } else if let Some(sid) = existing.session_id {
                let _ = self.reconnect_ssh_session(SessionId(sid));
                return;
            }
        }
        let _ = self.toggle_tunnel(tunnel_id);
    }

    fn reconnect_all_tunnels(&mut self) {
        let snap = self.tunnel_manager.snapshot();
        let mut sessions_to_reconnect = BTreeSet::new();
        for t in &snap.tunnels {
            match (&t.state, t.standalone, t.session_id) {
                (termewnal_ssh::TunnelState::Retrying { .. }, _, _) => {
                    let _ = self.tunnel_manager.reconnect_tunnel(&t.id);
                }
                (
                    termewnal_ssh::TunnelState::Starting
                    | termewnal_ssh::TunnelState::Active
                    | termewnal_ssh::TunnelState::Holding
                    | termewnal_ssh::TunnelState::Reconnecting,
                    true,
                    _,
                ) => {
                    let _ = self.tunnel_manager.reconnect_tunnel(&t.id);
                }
                (termewnal_ssh::TunnelState::Error(_), true, _) => {
                    let _ = self.toggle_tunnel(&t.id);
                }
                (
                    termewnal_ssh::TunnelState::Holding
                    | termewnal_ssh::TunnelState::Reconnecting
                    | termewnal_ssh::TunnelState::Error(_),
                    false,
                    Some(sid),
                ) => {
                    sessions_to_reconnect.insert(SessionId(sid));
                }
                _ => {}
            }
        }
        for sid in sessions_to_reconnect {
            let _ = self.reconnect_ssh_session(sid);
        }
    }

    fn auto_start_session_tunnels(
        &mut self,
        session_id: SessionId,
        profile_id: Option<&str>,
        supervisor: &ConnectionSupervisor,
    ) -> Task<Message> {
        let Some(pid) = profile_id else {
            self.sync_tunnel_telemetry();
            return Task::none();
        };
        let rules: Vec<TunnelRule> = self
            .profiles
            .profiles
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.tunnels.clone())
            .unwrap_or_default();

        let mut tasks = Vec::new();
        for rule in rules {
            if rule.standalone
                || rule.requires_public_bind_opt_in()
                || self.tunnel_ui.is_manually_stopped(&rule.id)
            {
                continue;
            }
            tasks.push(self.spawn_session_tunnel(
                session_id,
                Some(pid.to_string()),
                rule,
                supervisor.subscribe_ready(),
            ));
        }
        self.sync_tunnel_telemetry();
        Task::batch(tasks)
    }

    fn start_tunnel_rule(
        &mut self,
        rule: TunnelRule,
        profile_id: Option<String>,
        session_id: Option<SessionId>,
    ) -> Task<Message> {
        self.tunnel_ui.mark_manually_stopped(&rule.id, false);
        self.tunnel_ui.set_start_error(&rule.id, None);

        if rule.standalone {
            let spec = self.resolve_standalone_spec(profile_id.as_deref(), session_id);
            let Some(target_spec) = spec else {
                self.tunnel_ui.set_start_error(
                    &rule.id,
                    Some("No SSH Profile or active SSH Session target for standalone tunnel".to_string()),
                );
                self.sync_tunnel_telemetry();
                return Task::none();
            };
            let connect_config = apply_known_hosts_override(
                target_spec.connect_config,
                self.known_hosts_override.as_ref(),
            );
            let standalone_spec = StandaloneTransportSpec::new(
                connect_config,
                target_spec.supervisor_config,
                Some(Arc::clone(&self.vault) as Arc<dyn SecretStore>),
                Arc::new(RejectAllInteractionHandler),
                target_spec.agent_connector,
            );

            let manager = self.tunnel_manager.clone();
            let fut = async move {
                if let Err(err) = manager
                    .start_standalone_tunnel(profile_id.clone(), rule.clone(), standalone_spec)
                    .await
                {
                    manager.record_start_error(None, profile_id, &rule, err.to_string());
                }
            };
            return spawn_or_task(fut, self.tunnel_manager.clone());
        }

        let target_slot = session_id
            .and_then(|sid| self.sessions.get(&sid).and_then(SessionEntry::as_ssh))
            .or_else(|| {
                profile_id.as_deref().and_then(|pid| {
                    self.sessions.values().find_map(|entry| {
                        entry
                            .as_ssh()
                            .filter(|slot| slot.target.profile_id.as_deref() == Some(pid))
                    })
                })
            });

        let Some(slot) = target_slot else {
            self.tunnel_ui.set_start_error(
                &rule.id,
                Some("Open an SSH Session before starting this session-bound tunnel".to_string()),
            );
            self.sync_tunnel_telemetry();
            return Task::none();
        };

        let Some(sup) = slot.supervisor.clone() else {
            self.tunnel_ui.set_start_error(
                &rule.id,
                Some("Connect the SSH Session before starting this session-bound tunnel".to_string()),
            );
            self.sync_tunnel_telemetry();
            return Task::none();
        };

        let sid = slot.id;
        let task = self.spawn_session_tunnel(sid, profile_id, rule, sup.subscribe_ready());
        self.sync_tunnel_telemetry();
        task
    }

    fn spawn_session_tunnel(
        &self,
        session_id: SessionId,
        profile_id: Option<String>,
        rule: TunnelRule,
        ready_rx: tokio::sync::watch::Receiver<termewnal_ssh::TransportReadySnapshot>,
    ) -> Task<Message> {
        let manager = self.tunnel_manager.clone();
        let fut = async move {
            if let Err(err) = manager
                .start_session_tunnel(session_id.0, profile_id.clone(), rule.clone(), ready_rx)
                .await
            {
                manager.record_start_error(Some(session_id.0), profile_id, &rule, err.to_string());
            }
        };
        spawn_or_task(fut, self.tunnel_manager.clone())
    }

    fn resolve_standalone_spec(
        &self,
        profile_id: Option<&str>,
        session_id: Option<SessionId>,
    ) -> Option<SshTargetSpec> {
        if let Some(pid) = profile_id
            && let Some(profile) = self.profiles.profiles.iter().find(|p| p.id == pid)
            && let Ok(spec) = SshTargetSpec::from_profile_with_config(
                profile,
                &self.config,
                80,
                24,
                self.config.terminal.scrollback_lines,
            )
        {
            return Some(spec);
        }
        if let Some(sid) = session_id
            && let Some(slot) = self.sessions.get(&sid).and_then(SessionEntry::as_ssh)
        {
            return Some(slot.spec.clone());
        }
        if let Some(active_sid) = self.active_session_id()
            && let Some(slot) = self.sessions.get(&active_sid).and_then(SessionEntry::as_ssh)
        {
            return Some(slot.spec.clone());
        }
        None
    }
}

fn disconnect_ssh_handles(
    ssh_handles: Vec<(Option<ConnectionSupervisor>, Option<Arc<SshSession>>)>,
) {
    for (sup, ssh) in ssh_handles {
        if let Some(s) = sup {
            s.disconnect();
        } else if let Some(a) = ssh {
            a.disconnect();
        }
    }
}

fn spawn_ssh_disconnect_after(
    tunnel_cleanup: impl std::future::Future<Output = ()> + Send + 'static,
    supervisor: Option<ConnectionSupervisor>,
    active: Option<Arc<SshSession>>,
) {
    if supervisor.is_none() && active.is_none() {
        return;
    }
    let fut = async move {
        tunnel_cleanup.await;
        disconnect_ssh_handles(vec![(supervisor, active)]);
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
    }
}

fn spawn_teardown_and_exit(
    teardown_fut: impl std::future::Future<Output = ()> + Send + 'static,
) -> Task<Message> {
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    let run = async move {
        teardown_fut.await;
        let _ = done_tx.send_replace(true);
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(run);
        Task::perform(
            async move {
                let _ = done_rx.wait_for(|&done| done).await;
            },
            |()| Message::FinalizeExit,
        )
    } else {
        Task::perform(run, |()| Message::FinalizeExit)
    }
}

fn spawn_or_task<F>(fut: F, manager: TunnelManager) -> Task<Message>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
        Task::none()
    } else {
        Task::perform(
            async move {
                fut.await;
                manager.snapshot()
            },
            Message::TunnelManagerUpdated,
        )
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

#[derive(Clone)]
struct TransferSubKey {
    queue: TransferQueue,
}

impl Hash for TransferSubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.queue.instance_id().hash(state);
    }
}

fn transfer_queue_stream(
    key: &TransferSubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let queue = key.queue.clone();
    Box::pin(iced::stream::channel(32, async move |mut output| {
        let mut rx = queue.subscribe();
        while rx.changed().await.is_ok() {
            let snap = rx.borrow_and_update().clone();
            if output.send(Message::TransferQueueUpdated(snap)).await.is_err() {
                break;
            }
        }
    }))
}

#[derive(Clone)]
struct TunnelSubKey {
    manager: TunnelManager,
}

impl Hash for TunnelSubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.manager.instance_id().hash(state);
    }
}

fn tunnel_manager_stream(
    key: &TunnelSubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let manager = key.manager.clone();
    Box::pin(iced::stream::channel(32, async move |mut output| {
        let mut rx = manager.subscribe();
        while rx.changed().await.is_ok() {
            let snap = rx.borrow_and_update().clone();
            if output.send(Message::TunnelManagerUpdated(snap)).await.is_err() {
                break;
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
            Key::Character("e") | Key::Character("E") => Some(Message::ToggleSftpDrawer),
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
                Message::ToggleCommandPalette
                    | Message::ToggleSidebar
                    | Message::ToggleSftpDrawer
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
