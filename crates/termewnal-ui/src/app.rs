use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iced::futures::{SinkExt, Stream};
use iced::keyboard::{self, Key, Modifiers, key::Named};
use iced::widget::{
    button, column, container, operation, row, scrollable, space, stack, text,
};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Padding, Point, Size,
    Subscription, Task, Theme, clipboard, event, window,
};
use termewnal_core::{
    AppConfig, BellBehavior, BuiltinTheme, CURRENT_SCHEMA_VERSION, ConfigFileKind, ConfigPaths,
    ConfigSyncService, ConfigWatchEvent, ConfigWatcher, CursorStyle, DockerRuntime, KeyAction,
    KeyChord, KeybindingResolver, LayoutSplitAxis, LayoutSplitNode, LoginScriptStep, NewlineMode,
    OpenSshHost, Profile, ProfileTarget, ProfilesConfig, QuickCommand, QuickCommandsConfig,
    RightClickAction, SecretStore, SerialInputMode, SerialOutputMode,
    SessionDescriptor, SftpState, SshAuthMethod, SshProxyConfig, SyncProviderKind, TabBarPosition,
    TabLayout,
    TerminalConfig, ThemePalette, TomlReloadError, TransferConflictPolicy, TunnelDirection,
    TunnelRule, VaultBackend, VaultSession, WindowLayout, WorkspaceLayout, WorkspaceTemplate,
    WorkspacesConfig, ensure_config_files, import_tabby_config_file_into_profiles, load_layout,
    load_quick_commands, load_sftp_state, load_system_ssh_hosts, load_workspaces,
    resolve_mux_attach_command, save_cloned_openssh_profile, save_config, save_layout,
    save_profiles, save_quick_commands, save_sftp_state, save_workspaces,
};
use termewnal_protocols::{
    CommandOutputRunner, ContainerExecConfig, DockerDiscovery, OsCommandRunner, OsSerialPortOpener,
    ProtocolStatus, SerialConnectConfig, SerialPortInfo, SerialPortOpener, SerialSession,
    TelnetConnectConfig, TelnetSession, ZmodemDirection, ZmodemEngine, ZmodemEvent,
    build_exec_shell_spec, list_available_serial_ports,
};
use termewnal_pty::{
    CompiledHighlightRule, GridSearchMatch, PtyEvent, PtySession, PtySpawnConfig, Scroll,
    SelectionType, ShellSpec, Side, TerminalBuffer, TerminalProgressState, TerminalSnapshot,
    compile_highlight_rules, default_session_log_path, detect_default_shell,
};
use termewnal_ssh::{
    ConflictResolution, ConnectionSupervisor, DirectoryTransferSummary, JumpHopSpec,
    OsAgentConnector, ProfileTransportKey, RejectAllInteractionHandler, RemoteListing,
    ServerStatsCollector, ServerStatsSnapshot, SftpEndpoint, SftpManager, SftpTransportStatus,
    SshConnectConfig, SshSession, SshTransport, StandaloneTransportSpec, SupervisorConfig,
    TransferId, TransferNotice, TransferQueue, TransferQueueConfig, TransferQueueSnapshot,
    TransferReconnectState, TransferSnapshot, TransferStatus, TransportStatus, TunnelManager,
    TunnelManagerSnapshot, join_remote_path, parent_remote_path, resolve_proxy_jump_hops,
    validate_remote_entry_name,
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
    QuickConnectTarget, command_palette_input_id, quick_connect_input_id,
    render_command_palette_responsive,
};
use crate::config_reload_ui::ConfigReloadUiState;
use crate::context_menu::{
    ContextMenuState, TabRightClickCapture, context_prompt_input_id, parse_hex_color,
    render_context_menu_overlay,
};
use crate::drag_drop::{
    DragCoordinator, DragDropState, DragPayload, DragSource, DropTarget, DropTargetZone,
    DropZoneKind, render_drag_overlay,
};
use crate::ipc::{IpcCommand, IpcServerHandle};
use crate::notifications::{DesktopNotifier, ZbusDesktopNotifier};
use crate::profile_ui::{
    JumpHostCandidate, JumpHostKind, ProfileAuthChoice, ProfileEditorState, ProfileEditorTab,
    ProfileKindChoice, SshConnectionMode,
};
use crate::quake_and_tray::{
    GlobalHotkeyRegistrar, HotkeyRegistrationGuard, NullGlobalHotkeyRegistrar, NullSystemTray,
    QuakeController, SystemTrayAdapter, TrayAction, TrayStateSnapshot, X11GlobalHotkeyRegistrar,
    ZbusStatusNotifierItem, quake_window_settings,
};
use crate::server_stats_bar::render_server_stats_bar;
use crate::settings_ui::{SettingsTab, SettingsUiState, ThemeOverrideField};
use crate::sftp_drawer::{
    DEFAULT_SFTP_DRAWER_WIDTH, MIN_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH, SftpConnectionState,
    SftpDrawerDock, SftpDrawerState, render_conflict_resolution_modal_responsive, render_sftp_drawer,
};
use crate::sftp_plus::{
    FileBrowserEntry, SftpClipboard, SftpClipboardOp, SftpContextActionKind, SftpLayoutMode,
    SftpModalState, SftpPaneSide, SftpSortColumn, copy_local_path_recursive, default_local_home_dir,
    format_octal_mode, join_pane_path, pane_prefs, pane_prefs_mut, parent_pane_path,
    remove_local_path, render_sftp_context_menu_overlay, render_sftp_plus_commander,
    render_sftp_plus_modal, set_local_permissions, unique_local_copy_path, visible_pane_entries,
};
use crate::sidebar::{
    ActiveTunnelEntry, SidebarFilterScope, SidebarSection, SidebarSessionEntry, SidebarState,
    SystemSshProfile, UNGROUPED_LABEL, profile_target_summary, render_sidebar_full,
};
use crate::split_view::{CapturedEventGate, PaneFrame, ShortcutInterceptor, SplitView};
use crate::ssh_ui::{SshTargetSpec, SshUiEvent, SshUiState, connect_ssh_task};
use crate::status_bar::{
    RecoveryNoticeKind, SessionKindLabel, SessionStatusHook, StatusBarAction, StatusBarSnapshot,
    StatusBarState, StatusRecoveryNotice, TransferSummaryHook, TunnelSummaryHook,
    render_status_bar_responsive,
};
use crate::tabby_import_ui::{TabbyImportUiState, tabby_import_path_input_id};
use crate::terminal_ergonomics::{
    FileDropAction, PendingPaste, SearchBarState, classify_file_drop, cursor_style_to_shape,
    format_paste_bytes, iced_theme_with_overrides, is_multiline_paste, open_url_with_system,
    paste_confirm_modal_view_responsive, read_clipboard_via_arboard, search_bar_view_responsive,
    search_input_id,
};
use crate::terminal_widget::{TerminalViewOptions, TerminalViewport};
use crate::tunnel_ui::{
    AdHocTunnelRule, RECONNECT_NOTICE_TTL, TunnelUiState, apply_known_hosts_override,
    build_active_tunnel_entries, tunnel_name_input_id,
};
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, FONT_SUBHEAD, TAB_BAR_HEIGHT, ViewportTier, badge_pill,
    hairline_h, hairline_v, keycap_pill, list_row_button_style, mono_font, sidebar_width_for_window, status_dot, styled_input_style, tab_bar_button_style,
    tab_bar_close_button_style, tab_button_style, tab_container_style_tinted,
    tabby_scrollable_style, ui_font, ui_font_medium, ui_font_semibold,
};
use crate::v2_modals::{
    ProtocolLauncherState, ProtocolLauncherTab, QuickCommandsUiState, WorkspacesUiState,
    ZmodemSendUiState,
};
use crate::window_chrome::{
    BorderlessResizeFrame, WindowControlGlyph, WindowControlIcon, main_window_settings,
    render_top_system_title_bar,
};
use crate::workspace::{
    ClosePaneOutcome, CycleDirection, FocusDirection, PaneId, SplitAxis, SplitId, SplitNode,
    TabId, Workspace, WorkspaceIds, WorkspaceTab,
};

/// Injectable OS URL-launcher boundary (`open::that_detached` by default).
pub type UrlOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Injectable OS local-file opener boundary for `Ctrl+Click` file paths (`path:line:col`).
pub type FileOpener = Arc<dyn Fn(&Path, Option<u32>, Option<u32>) -> Result<(), String> + Send + Sync>;

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
    pub proxy_command: Option<String>,
    pub proxy: Option<SshProxyConfig>,
    pub forward_agent: bool,
    pub forward_x11: bool,
    pub auto_connect_on_startup: bool,
    pub tmux_reattach: Option<String>,
    pub keepalive_interval_secs: Option<u64>,
    pub keepalive_max_missed: Option<u32>,
    pub auto_attach_mux: bool,
    pub mux_attach_command: Option<String>,
    pub login_scripts: Vec<LoginScriptStep>,
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
            proxy_jump: spec
                .connect_config
                .proxy_jump
                .iter()
                .map(JumpHopSpec::display_addr)
                .collect(),
            proxy_command: spec.connect_config.proxy_command.clone(),
            proxy: spec.connect_config.proxy.clone(),
            forward_agent: spec.connect_config.forward_agent,
            forward_x11: spec.connect_config.forward_x11,
            auto_connect_on_startup: spec.auto_connect_on_startup,
            tmux_reattach: None,
            keepalive_interval_secs: Some(spec.supervisor_config.keepalive_interval.as_secs()),
            keepalive_max_missed: Some(spec.supervisor_config.keepalive_max_missed),
            auto_attach_mux: spec.supervisor_config.mux_attach_command.is_some(),
            mux_attach_command: spec.supervisor_config.mux_attach_command.clone(),
            login_scripts: spec.connect_config.login_scripts.clone(),
        }
    }

    fn to_target_spec_with_hops(
        &self,
        title: &str,
        cols: u16,
        rows: u16,
        app_config: &AppConfig,
        proxy_jump: Vec<JumpHopSpec>,
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
                proxy_jump,
                proxy_command: self.proxy_command.clone(),
                proxy: self.proxy.clone(),
                forward_agent: self.forward_agent,
                forward_x11: self.forward_x11,
                nodelay: true,
                term_name: "xterm-256color".to_string(),
                cols: cols.max(1),
                rows: rows.max(1),
                scrollback_lines: app_config.terminal.scrollback_lines,
                login_scripts: self.login_scripts.clone(),
                ..SshConnectConfig::default()
            },
            supervisor_config,
            agent_connector: Arc::new(OsAgentConnector::default()),
            auto_connect_on_startup: self.auto_connect_on_startup,
        }
    }

    fn to_target_spec(
        &self,
        title: &str,
        cols: u16,
        rows: u16,
        app_config: &AppConfig,
    ) -> SshTargetSpec {
        let hops = resolve_proxy_jump_hops(&self.proxy_jump, &[], &[], &self.username, None)
            .unwrap_or_default();
        self.to_target_spec_with_hops(title, cols, rows, app_config, hops)
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
    is_auto_reconnecting: bool,
    reconnect_attempt: Option<u32>,
    reconnected_at: Option<Instant>,
    server_stats: Option<ServerStatsSnapshot>,
    server_stats_unsupported: bool,
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

    /// Returns the latest remote server resource snapshot (`CPU`, `RAM`, `DISK`, `NET`), if any.
    pub fn server_stats(&self) -> Option<&ServerStatsSnapshot> {
        self.server_stats.as_ref()
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

    /// Returns `true` when this SSH `Session` is actively auto-reconnecting.
    pub fn is_auto_reconnecting(&self) -> bool {
        self.is_auto_reconnecting
            || matches!(
                self.transport_status,
                TransportStatus::Degraded { .. } | TransportStatus::Reconnecting { .. }
            )
    }

    /// Returns the latest auto-reconnect attempt number, if recovering.
    pub fn reconnect_attempt(&self) -> Option<u32> {
        self.reconnect_attempt.or(match self.transport_status {
            TransportStatus::Reconnecting { attempt, .. } => Some(attempt),
            _ => None,
        })
    }

    /// Returns `true` when this SSH `Session` recently completed an automatic reconnect.
    pub fn is_recently_reconnected(&self) -> bool {
        self.reconnected_at.is_some()
            && matches!(self.transport_status, TransportStatus::Connected { .. })
    }

    fn record_transport_transition(&mut self, next: &TransportStatus, now: Instant) {
        match next {
            TransportStatus::Degraded { .. } => {
                self.is_auto_reconnecting = true;
                self.reconnected_at = None;
            }
            TransportStatus::Reconnecting { attempt, .. } => {
                self.is_auto_reconnecting = true;
                self.reconnect_attempt = Some(*attempt);
                self.reconnected_at = None;
            }
            TransportStatus::Connecting => {
                self.reconnected_at = None;
            }
            TransportStatus::Connected { .. } => {
                if self.is_auto_reconnecting {
                    self.is_auto_reconnecting = false;
                    self.reconnect_attempt = None;
                    self.reconnected_at = Some(now);
                }
            }
            TransportStatus::Disconnected => {
                self.is_auto_reconnecting = false;
                self.reconnect_attempt = None;
                self.reconnected_at = None;
            }
        }
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

/// Runtime state of an active local shell (or Docker container exec) `Session`.
pub struct LocalSession {
    id: SessionId,
    profile_id: Option<String>,
    initial_working_dir: Option<PathBuf>,
    docker_exec: Option<ContainerExecConfig>,
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

    /// Returns container attach configuration if this PTY runs `docker/podman exec -it`.
    pub fn docker_exec(&self) -> Option<&ContainerExecConfig> {
        self.docker_exec.as_ref()
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

/// Runtime slot representing an interactive Serial RS-232/USB `Session`.
pub struct SerialSessionSlot {
    id: SessionId,
    profile_id: Option<String>,
    title: String,
    config: SerialConnectConfig,
    opener: Arc<dyn SerialPortOpener>,
    buffer: Arc<TerminalBuffer>,
    snapshot: Arc<TerminalSnapshot>,
    active: Option<SerialSession>,
    status: ProtocolStatus,
    generation: u64,
    is_auto_reconnecting: bool,
    reconnect_attempt: Option<u32>,
    reconnected_at: Option<Instant>,
    hex_draft: String,
    last_error: Option<String>,
}

impl SerialSessionSlot {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn profile_id(&self) -> Option<&str> {
        self.profile_id.as_deref()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn config(&self) -> &SerialConnectConfig {
        &self.config
    }

    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
    }

    pub fn snapshot(&self) -> &Arc<TerminalSnapshot> {
        &self.snapshot
    }

    pub fn serial_session(&self) -> Option<&SerialSession> {
        self.active.as_ref()
    }

    pub fn status(&self) -> ProtocolStatus {
        if let Some(active) = &self.active {
            active.status()
        } else {
            self.status.clone()
        }
    }

    pub fn hex_draft(&self) -> &str {
        &self.hex_draft
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn is_auto_reconnecting(&self) -> bool {
        self.is_auto_reconnecting
            || matches!(self.status(), ProtocolStatus::Reconnecting { .. })
    }

    pub fn reconnect_attempt(&self) -> Option<u32> {
        self.reconnect_attempt.or(match self.status() {
            ProtocolStatus::Reconnecting { attempt, .. } => Some(attempt),
            _ => None,
        })
    }

    pub fn is_recently_reconnected(&self) -> bool {
        self.reconnected_at.is_some() && self.status() == ProtocolStatus::Connected
    }

    fn apply_protocol_status(&mut self, status: ProtocolStatus, now: Instant) {
        if let ProtocolStatus::Reconnecting {
            last_error: Some(err),
            ..
        } = &status
        {
            self.last_error = Some(err.clone());
        } else if status == ProtocolStatus::Connected {
            self.last_error = None;
        }
        update_protocol_recovery_state(
            &status,
            &mut self.is_auto_reconnecting,
            &mut self.reconnect_attempt,
            &mut self.reconnected_at,
            now,
        );
        self.status = status;
    }
}

/// Runtime slot representing a pure-Rust Telnet TCP `Session`.
pub struct TelnetSessionSlot {
    id: SessionId,
    profile_id: Option<String>,
    title: String,
    config: TelnetConnectConfig,
    buffer: Arc<TerminalBuffer>,
    snapshot: Arc<TerminalSnapshot>,
    active: Option<TelnetSession>,
    status: ProtocolStatus,
    generation: u64,
    is_auto_reconnecting: bool,
    reconnect_attempt: Option<u32>,
    reconnected_at: Option<Instant>,
    last_error: Option<String>,
}

impl TelnetSessionSlot {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn profile_id(&self) -> Option<&str> {
        self.profile_id.as_deref()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn config(&self) -> &TelnetConnectConfig {
        &self.config
    }

    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
    }

    pub fn snapshot(&self) -> &Arc<TerminalSnapshot> {
        &self.snapshot
    }

    pub fn telnet_session(&self) -> Option<&TelnetSession> {
        self.active.as_ref()
    }

    pub fn status(&self) -> ProtocolStatus {
        if let Some(active) = &self.active {
            active.status()
        } else {
            self.status.clone()
        }
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn is_auto_reconnecting(&self) -> bool {
        self.is_auto_reconnecting
            || matches!(self.status(), ProtocolStatus::Reconnecting { .. })
    }

    pub fn reconnect_attempt(&self) -> Option<u32> {
        self.reconnect_attempt.or(match self.status() {
            ProtocolStatus::Reconnecting { attempt, .. } => Some(attempt),
            _ => None,
        })
    }

    pub fn is_recently_reconnected(&self) -> bool {
        self.reconnected_at.is_some() && self.status() == ProtocolStatus::Connected
    }

    fn apply_protocol_status(&mut self, status: ProtocolStatus, now: Instant) {
        if let ProtocolStatus::Reconnecting {
            last_error: Some(err),
            ..
        } = &status
        {
            self.last_error = Some(err.clone());
        } else if status == ProtocolStatus::Connected {
            self.last_error = None;
        }
        update_protocol_recovery_state(
            &status,
            &mut self.is_auto_reconnecting,
            &mut self.reconnect_attempt,
            &mut self.reconnected_at,
            now,
        );
        self.status = status;
    }
}

fn update_protocol_recovery_state(
    status: &ProtocolStatus,
    is_auto_reconnecting: &mut bool,
    reconnect_attempt: &mut Option<u32>,
    reconnected_at: &mut Option<Instant>,
    now: Instant,
) {
    match status {
        ProtocolStatus::Reconnecting { attempt, .. } => {
            *is_auto_reconnecting = true;
            *reconnect_attempt = Some(*attempt);
            *reconnected_at = None;
        }
        ProtocolStatus::Connecting => {
            *reconnected_at = None;
        }
        ProtocolStatus::Connected => {
            if *is_auto_reconnecting {
                *is_auto_reconnecting = false;
                *reconnect_attempt = None;
                *reconnected_at = Some(now);
            }
        }
        ProtocolStatus::Disconnected => {
            *is_auto_reconnecting = false;
            *reconnect_attempt = None;
            *reconnected_at = None;
        }
    }
}

#[derive(Clone)]
struct ZmodemControllerHandle {
    engine: Arc<Mutex<ZmodemEngine>>,
    pending_events: Arc<Mutex<Vec<ZmodemEvent>>>,
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
    Ssh(SshSessionSlot),
    Serial(SerialSessionSlot),
    Telnet(TelnetSessionSlot),
}

impl SessionEntry {
    fn title(&self) -> &str {
        match self {
            Self::Local(s) => &s.title,
            Self::Ssh(s) => &s.title,
            Self::Serial(s) => &s.title,
            Self::Telnet(s) => &s.title,
        }
    }

    fn buffer(&self) -> Option<&Arc<TerminalBuffer>> {
        match self {
            Self::Local(s) => Some(s.pty.buffer()),
            Self::Ssh(s) => Some(&s.buffer),
            Self::Serial(s) => Some(&s.buffer),
            Self::Telnet(s) => Some(&s.buffer),
        }
    }

    fn refresh_snapshot(&mut self) {
        match self {
            Self::Local(s) => s.snapshot = Arc::new(s.pty.snapshot()),
            Self::Ssh(s) => s.snapshot = Arc::new(s.buffer.snapshot()),
            Self::Serial(s) => {
                if let Some(active) = &s.active {
                    s.status = active.status();
                }
                s.snapshot = Arc::new(s.buffer.snapshot());
            }
            Self::Telnet(s) => {
                if let Some(active) = &s.active {
                    s.status = active.status();
                }
                s.snapshot = Arc::new(s.buffer.snapshot());
            }
        }
    }

    fn as_local(&self) -> Option<&LocalSession> {
        match self {
            Self::Local(s) => Some(s),
            _ => None,
        }
    }

    fn as_ssh(&self) -> Option<&SshSessionSlot> {
        match self {
            Self::Ssh(s) => Some(s),
            _ => None,
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
    SetSidebarFilterScope(SidebarFilterScope),
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
    ToggleSftpDrawerForSession(SessionId),
    SetSftpDrawerOpen(bool),
    ResizeSftpDrawer(f32),
    SftpSetLayoutMode(SftpLayoutMode),
    SftpToggleTransfersPanel,
    SftpSetActivePane(SftpPaneSide),
    SftpShortcut(SftpContextActionKind),
    SftpPanePathInputChanged(SftpPaneSide, String),
    SftpPaneNavigateSubmitted(SftpPaneSide),
    SftpPaneNavigateTo {
        side: SftpPaneSide,
        path: String,
    },
    SftpPaneBack(SftpPaneSide),
    SftpPaneForward(SftpPaneSide),
    SftpPaneUp(SftpPaneSide),
    SftpPaneRefresh(SftpPaneSide),
    SftpPaneHome(SftpPaneSide),
    SftpPaneToggleHidden(SftpPaneSide),
    SftpPaneToggleFilter(SftpPaneSide),
    SftpPaneFilterChanged(SftpPaneSide, String),
    SftpPaneToggleBookmarks(SftpPaneSide),
    SftpPaneDismissBookmarks(SftpPaneSide),
    SftpPaneToggleBookmarkCurrent(SftpPaneSide),
    SftpPaneSelectBookmark {
        side: SftpPaneSide,
        path: String,
    },
    SftpPaneRemoveBookmark {
        side: SftpPaneSide,
        path: String,
    },
    SftpPaneSortBy(SftpPaneSide, SftpSortColumn),
    SftpPaneRowClicked {
        side: SftpPaneSide,
        path: String,
        ctrl_held: bool,
    },
    SftpOpenContextMenu {
        side: SftpPaneSide,
        target_path: Option<String>,
        position: Point,
    },
    SftpDismissContextMenu,
    SftpContextAction(SftpContextActionKind),
    SftpDismissModal,
    SftpModalNameChanged(String),
    SftpModalConfirmCreateOrRename,
    SftpModalOctalChanged(String),
    SftpModalPermBitToggled {
        bit: u32,
        enabled: bool,
    },
    SftpModalConfirmPermissions,
    SftpModalEditorAction(iced::widget::text_editor::Action),
    SftpModalSaveFile,
    SftpRemoteFileReadResult {
        session_id: SessionId,
        request_path: String,
        editable: bool,
        result: Result<String, String>,
    },
    SftpRemoteFileSaveResult {
        session_id: SessionId,
        path: String,
        result: Result<(), String>,
    },
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
    SftpDownloadDir {
        remote_path: String,
        dir_name: String,
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
    SftpDirEnqueueResult {
        result: Result<DirectoryTransferSummary, String>,
    },
    SftpSessionDirEnqueueResult {
        session_id: SessionId,
        result: Result<DirectoryTransferSummary, String>,
    },
    TransferQueueUpdated(TransferQueueSnapshot),
    TransferConflictRenameInputChanged(String),
    TransferConflictApplyToAllToggled(bool),
    ResolveTransferConflict {
        transfer_id: TransferId,
        resolution: ConflictResolution,
        apply_to_all: bool,
    },
    PauseTransfer(TransferId),
    ResumeTransfer(TransferId),
    CancelTransfer(TransferId),
    RetryTransfer(TransferId),
    ClearFinishedTransfers,
    OpenTunnelCreateModal,
    OpenEditTunnelModal(String),
    DeleteTunnel(String),
    OpenTunnelContextMenu {
        tunnel_id: String,
        position: Point,
    },
    CloseTunnelCreateModal,
    TunnelFormDirectionChanged(TunnelDirection),
    TunnelFormNameChanged(String),
    TunnelFormProfileSelected(Option<String>),
    TunnelFormSessionSelected(SessionId),
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
    OpenSettingsModal,
    CloseSettingsModal,
    SettingsSelectTab(SettingsTab),
    SettingsThemeSelected(BuiltinTheme),
    SettingsFontFamilyChanged(String),
    SettingsFontSizeChanged(String),
    SettingsStepFontSize(f32),
    SettingsLigaturesToggled(bool),
    SettingsCursorStyleSelected(CursorStyle),
    SettingsShellChanged(String),
    SettingsScrollbackChanged(String),
    SettingsCopyOnSelectToggled(bool),
    SettingsConfirmMultilinePasteToggled(bool),
    SettingsKeepaliveIntervalChanged(String),
    SettingsKeepaliveMissedChanged(String),
    SettingsConflictPolicySelected(TransferConflictPolicy),
    SettingsFollowSymlinksToggled(bool),
    SettingsVaultBackendSelected(VaultBackend),
    SettingsAutoLockChanged(String),
    SettingsSave,
    OpenNewProfileModal,
    OpenEditProfileModal(String),
    CloseProfileEditor,
    ProfileFormTabSelected(ProfileEditorTab),
    ProfileFormKindChanged(ProfileKindChoice),
    ProfileFormNameChanged(String),
    ProfileFormGroupChanged(String),
    ProfileFormTagsChanged(String),
    ProfileFormLocalShellChanged(String),
    ProfileFormLocalDirChanged(String),
    ProfileFormHostChanged(String),
    ProfileFormPortChanged(String),
    ProfileFormUsernameChanged(String),
    ProfileFormAuthChanged(ProfileAuthChoice),
    ProfileFormKeyPathChanged(String),
    ProfileFormSecretChanged(String),
    ProfileFormForgetSecret,
    ProfileFormToggleConnectionMenu,
    ProfileFormCloseConnectionMenu,
    ProfileFormConnectionModeSelected(SshConnectionMode),
    ProfileFormProxyHostChanged(String),
    ProfileFormProxyPortChanged(String),
    ProfileFormProxyUsernameChanged(String),
    ProfileFormProxySecretChanged(String),
    ProfileFormForgetProxySecret,
    ProfileFormProxyJumpChanged(String),
    ProfileFormToggleJumpPanel,
    ProfileFormJumpDropdownSelected(String),
    ProfileFormToggleCustomJumpInput,
    ProfileFormCustomJumpDraftChanged(String),
    ProfileFormAddCustomJumpHop,
    ProfileFormRemoveJumpHop(usize),
    ProfileFormMoveJumpHop {
        index: usize,
        delta: isize,
    },
    ProfileFormClearJumpHops,
    ProfileFormAutoConnectToggled(bool),
    ProfileFormAutoMuxToggled(bool),
    ProfileFormMuxCommandChanged(String),
    ProfileSave,
    ProfileDelete(String),
    TabRenameDraftChanged(String),
    SubmitTabRename,
    DismissContextMenu,
    ClearTerminalBuffer(SessionId),
    SaveScrollback(SessionId),
    ToggleSessionLogging(SessionId),
    OpenQuickCommandsModal,
    OpenWorkspacesModal,
    TriggerZmodemSend(SessionId),
    ToggleQuakeWindow,
    SetTabColor {
        tab_id: TabId,
        color: Option<String>,
    },
    BeginTabRename(TabId),
    TogglePinTab(TabId),
    DuplicateTab(TabId),
    CloseOtherTabs(TabId),
    CloseTabsToRight(TabId),
    OpenTerminalContextMenu {
        session_id: SessionId,
        position: Point,
    },
    OpenTabContextMenu {
        tab_id: TabId,
        position: Point,
    },
    OpenSidebarContextMenu {
        group_name: Option<String>,
        profile_id: Option<String>,
        position: Point,
    },
    OpenNewProfileInGroup(Option<String>),
    BeginCreateGroup {
        assign_profile_id: Option<String>,
    },
    CreateGroupDraftChanged(String),
    SubmitCreateGroup,
    BeginRenameGroup(String),
    RenameGroupDraftChanged(String),
    SubmitRenameGroup,
    DeleteProfileGroup(String),
    MoveProfileToGroup {
        profile_id: String,
        group: Option<String>,
    },
    DeleteSavedProfileById(String),
    DragStart {
        payload: DragPayload,
        position: Point,
    },
    DragCursorMoved(Point),
    DragHoverChanged(DropTarget),
    DragLeaveTarget(DropTarget),
    DragFinish,
    DragCancel,
    ExternalFileHovered(PathBuf),
    ExternalFilesLeft,
    OpenLocalFile {
        session_id: SessionId,
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    SettingsShortcutCaptured(String),
    SettingsCancelShortcutCapture,
    PushPendingKeyChord(KeyChord),
    ClearPendingKeyChord,
    AdjustFontSize(f32),
    ResetFontSize,
    SelectTabByIndex(usize),
    CursorBlinkTick,
    ClearVisualBell {
        session_id: SessionId,
        generation: u64,
    },
    SettingsOpacityChanged(String),
    SettingsStepOpacity(f32),
    SettingsCursorBlinkToggled(bool),
    SettingsTabBarPositionSelected(TabBarPosition),
    SettingsShowProfileSidebarToggled(bool),
    SettingsShowStatusBarToggled(bool),
    SettingsShowServerStatsToggled(bool),
    ServerStatsUpdated {
        session_id: SessionId,
        generation: u64,
        stats: ServerStatsSnapshot,
    },
    ServerStatsUnavailable {
        session_id: SessionId,
        generation: u64,
    },
    SettingsHideTabIndexToggled(bool),
    SettingsFlexTabsToggled(bool),
    SettingsThemeOverrideChanged {
        field: ThemeOverrideField,
        value: String,
    },
    SettingsRightClickActionSelected(RightClickAction),
    SettingsMiddleClickPasteToggled(bool),
    SettingsBellBehaviorSelected(BellBehavior),
    SettingsAltIsMetaToggled(bool),
    SettingsWordSeparatorsChanged(String),
    SettingsAutoLogToggled(bool),
    SettingsLogDirectoryChanged(String),
    SettingsKeybindingFilterChanged(String),
    SettingsKeybindingChanged {
        action: KeyAction,
        value: String,
    },
    SettingsBeginShortcutCapture(KeyAction),
    SettingsResetKeybinding(KeyAction),
    SettingsResetAllKeybindings,
    SettingsHighlightNameChanged(String),
    SettingsHighlightPatternChanged(String),
    SettingsHighlightFgChanged(String),
    SettingsHighlightBgChanged(String),
    SettingsHighlightBoldToggled(bool),
    SettingsHighlightCaseToggled(bool),
    SettingsAddHighlightRule,
    SettingsToggleHighlightRule(String),
    SettingsDeleteHighlightRule(String),
    ProfileFormProxyCommandChanged(String),
    ProfileFormForwardAgentToggled(bool),
    ProfileFormForwardX11Toggled(bool),
    ProfileFormKeepaliveIntervalChanged(String),
    ProfileFormKeepaliveMissedChanged(String),
    ProfileFormScriptExpectChanged(String),
    ProfileFormScriptSendChanged(String),
    ProfileFormScriptRegexToggled(bool),
    ProfileFormAddScriptStep,
    ProfileFormRemoveScriptStep(usize),
    ProfileFormTunnelNameChanged(String),
    ProfileFormTunnelBindPortChanged(String),
    ProfileFormTunnelTargetHostChanged(String),
    ProfileFormTunnelTargetPortChanged(String),
    ProfileFormAddTunnel,
    ProfileFormRemoveTunnel(usize),
    ProfileFormSerialPortChanged(String),
    ProfileFormSerialBaudChanged(String),
    ProfileFormSerialInputModeChanged(SerialInputMode),
    ProfileFormSerialOutputModeChanged(SerialOutputMode),
    ProfileFormSerialAutoReconnectToggled(bool),
    ProfileFormTelnetHostChanged(String),
    ProfileFormTelnetPortChanged(String),
    ProfileFormTelnetAutoReconnectToggled(bool),
    ProfileFormDockerRuntimeChanged(DockerRuntime),
    ProfileFormDockerContainerChanged(String),
    ProfileFormDockerShellChanged(String),
    ProfileFormDockerUserChanged(String),
    ProfileFormDockerWorkdirChanged(String),
    SettingsSyncProviderSelected(SyncProviderKind),
    SettingsSyncLocalDirChanged(String),
    SettingsSyncWebDavUrlChanged(String),
    SettingsSyncWebDavUserChanged(String),
    SettingsSyncGistIdChanged(String),
    SettingsSyncTokenChanged(String),
    SettingsSyncPassphraseChanged(String),
    SettingsSyncEncryptToggled(bool),
    SettingsSyncAutoToggled(bool),
    SettingsSyncPushNow,
    SettingsSyncPullNow,
    ConfigSyncFinished {
        is_push: bool,
        result: Result<(String, Option<String>), String>,
    },
    CloseQuickCommandsModal,
    QuickCommandFilterChanged(String),
    QuickCommandNameChanged(String),
    QuickCommandGroupChanged(String),
    QuickCommandBodyChanged(String),
    QuickCommandAppendNewlineToggled(bool),
    QuickCommandBroadcastToggled(bool),
    QuickCommandConfirmToggled(bool),
    QuickCommandSave,
    QuickCommandEdit(String),
    QuickCommandCancelEdit,
    QuickCommandDelete(String),
    ExecuteQuickCommand {
        id: String,
        broadcast_override: Option<bool>,
    },
    ConfirmExecuteQuickCommand(String),
    CancelQuickCommandConfirm,
    CloseWorkspacesModal,
    WorkspaceNameChanged(String),
    WorkspaceDescChanged(String),
    SaveCurrentWorkspaceTemplate,
    LaunchWorkspaceTemplate(String),
    DeleteWorkspaceTemplate(String),
    OpenProtocolLauncherModal(ProtocolLauncherTab),
    CloseProtocolLauncherModal,
    ProtocolLauncherSelectTab(ProtocolLauncherTab),
    RefreshProtocolDiscovery,
    LaunchDiscoveredContainer {
        runtime: DockerRuntime,
        container: String,
    },
    ProtocolSerialPortChanged(String),
    ProtocolSerialBaudChanged(String),
    LaunchQuickSerial,
    ProtocolTelnetHostChanged(String),
    ProtocolTelnetPortChanged(String),
    ProtocolTelnetAutoReconnectToggled(bool),
    LaunchQuickTelnet,
    CloseZmodemSendModal,
    ZmodemPathInputChanged(String),
    SubmitZmodemSend,
    StartZmodemSend {
        session_id: SessionId,
        files: Vec<PathBuf>,
    },
    CancelZmodem(SessionId),
    SerialSetInputMode {
        session_id: SessionId,
        mode: SerialInputMode,
    },
    SerialSetOutputMode {
        session_id: SessionId,
        mode: SerialOutputMode,
    },
    SerialSetNewlineMode {
        session_id: SessionId,
        mode: NewlineMode,
    },
    SerialHexDraftChanged {
        session_id: SessionId,
        value: String,
    },
    SerialSendHex(SessionId),
    SerialReconnect(SessionId),
    SerialDisconnect(SessionId),
    SerialStatusChanged {
        session_id: SessionId,
        generation: u64,
        status: ProtocolStatus,
    },
    TelnetReconnect(SessionId),
    TelnetDisconnect(SessionId),
    TelnetStatusChanged {
        session_id: SessionId,
        generation: u64,
        status: ProtocolStatus,
    },
    SftpManagerStatusUpdated(Vec<(ProfileTransportKey, SftpTransportStatus)>),
    SftpAutoReconnectTick {
        session_id: SessionId,
        retry_generation: u64,
    },
    ClearExpiredReconnectNotices,
    WindowUnfocused(window::Id),
    WindowDragStart(window::Id),
    WindowMinimize(window::Id),
    WindowToggleMaximize(window::Id),
    WindowShowSystemMenu(window::Id),
    WindowEdgeResize(window::Id, window::Direction),
    QuakeMonitorSize(window::Id, Option<Size>),
    GlobalHotkeyTriggered,
    TrayAction(TrayAction),
    FinalizeExit,
}

static NEXT_SFTP_SUB_ID: AtomicU64 = AtomicU64::new(1);

struct WindowState {
    workspace: Workspace,
    width: u32,
    height: u32,
    maximized: bool,
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
    settings_ui: SettingsUiState,
    settings_origin_config: Option<AppConfig>,
    profile_editor: ProfileEditorState,
    status_bar: StatusBarState,
    sftp_manager: SftpManager,
    sftp_sub_id: u64,
    transfer_queue: TransferQueue,
    sftp_state: SftpState,
    sftp_drawers: BTreeMap<SessionId, SftpDrawerState>,
    sftp_fallback_drawer: SftpDrawerState,
    sftp_drawer_width: f32,
    sftp_download_dir: PathBuf,
    transfer_sessions: BTreeMap<TransferId, SessionId>,
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
    file_opener: FileOpener,
    notifier: Arc<dyn DesktopNotifier>,
    keybinding_resolver: Arc<KeybindingResolver>,
    pending_key_chords: Vec<KeyChord>,
    compiled_highlights: Arc<[CompiledHighlightRule]>,
    cursor_visible: bool,
    visual_bell_generation: BTreeMap<SessionId, u64>,
    visual_bell_active: BTreeSet<SessionId>,
    context_menu: ContextMenuState,
    drag_drop: DragDropState,
    last_saved_scrollback_path: Option<PathBuf>,
    quick_commands: QuickCommandsConfig,
    workspaces_cfg: WorkspacesConfig,
    quick_commands_ui: QuickCommandsUiState,
    workspaces_ui: WorkspacesUiState,
    protocol_launcher: ProtocolLauncherState,
    zmodem_send_ui: ZmodemSendUiState,
    zmodem_controllers: BTreeMap<SessionId, ZmodemControllerHandle>,
    last_zmodem_notice: Option<String>,
    serial_opener: Arc<dyn SerialPortOpener>,
    serial_port_lister: Arc<dyn Fn() -> Result<Vec<SerialPortInfo>, String> + Send + Sync>,
    docker_runner_factory: Arc<dyn Fn() -> Box<dyn CommandOutputRunner> + Send + Sync>,
    sync_service: Arc<ConfigSyncService>,
    quake: QuakeController,
    tray_hidden: bool,
    tray_adapter: Arc<dyn SystemTrayAdapter>,
    tray_rx: Option<Arc<Mutex<Option<mpsc::UnboundedReceiver<TrayAction>>>>>,
    hotkey_registrar: Arc<dyn GlobalHotkeyRegistrar>,
    hotkey_guard: Option<Box<dyn HotkeyRegistrationGuard>>,
    hotkey_tx: Option<mpsc::UnboundedSender<()>>,
    hotkey_rx: Option<Arc<Mutex<Option<mpsc::UnboundedReceiver<()>>>>>,
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
        Self::boot_daemon_with_ipc_and_initial_command(paths, ipc_handle, IpcCommand::OpenWindow)
    }

    /// Boots the desktop daemon with explicit `paths`, IPC server, and parsed CLI launch command.
    pub fn boot_daemon_with_ipc_and_initial_command(
        paths: ConfigPaths,
        ipc_handle: Option<IpcServerHandle>,
        initial_cmd: IpcCommand,
    ) -> (Self, Task<Message>) {
        let mut state = Self::boot_with_paths(paths).unwrap_or_else(|err| {
            error!(error = %err, "failed to boot AppState");
            panic!("failed to initialize Termewnal: {err}");
        });
        if let Some(handle) = ipc_handle {
            state.attach_ipc(handle);
        }
        state.enable_desktop_integrations();

        let mut tasks = match initial_cmd {
            IpcCommand::ToggleQuake => {
                state.pending_windows.clear();
                vec![state.toggle_quake_window()]
            }
            IpcCommand::OpenTab { profile_id, cwd } => {
                let mut t = state.open_pending_windows();
                if profile_id.is_some() || cwd.is_some() {
                    t.push(state.open_tab_in_window_with_cwd(
                        None,
                        profile_id.as_deref(),
                        cwd,
                    ));
                }
                t
            }
            IpcCommand::OpenWindow => state.open_pending_windows(),
        };

        for intent in &state.pending_remote_connects {
            tasks.push(Task::done(Message::ConnectRemoteSession(intent.session_id)));
        }

        state.sync_tray_state();
        (state, Task::batch(tasks))
    }

    fn enable_desktop_integrations(&mut self) {
        if self.config.tray.enabled {
            let (tray_tx, tray_rx) = mpsc::unbounded_channel();
            self.tray_adapter = ZbusStatusNotifierItem::spawn(tray_tx);
            self.tray_rx = Some(Arc::new(Mutex::new(Some(tray_rx))));
        }
        self.hotkey_registrar = Arc::new(X11GlobalHotkeyRegistrar);
        let (hk_tx, hk_rx) = mpsc::unbounded_channel();
        self.hotkey_tx = Some(hk_tx);
        self.hotkey_rx = Some(Arc::new(Mutex::new(Some(hk_rx))));
        if let Err(err) = self.sync_global_hotkey() {
            warn!(error = %err, "failed registering Quake global hotkey");
        }
        self.sync_tray_state();
    }

    fn open_pending_windows(&mut self) -> Vec<Task<Message>> {
        let mut tasks = Vec::new();
        while let Some(win_state) = self.pending_windows.pop_front() {
            let (win_id, open_task) =
                window::open(main_window_settings(win_state.width, win_state.height));
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
        let transfer_queue = TransferQueue::with_config(
            sftp_manager.clone(),
            TransferQueueConfig {
                transfer: config.transfer.clone(),
                ..TransferQueueConfig::default()
            },
        );
        let tunnel_manager = TunnelManager::new();
        let download_dir = paths.cache_dir().join("downloads");
        let keybinding_resolver = Arc::new(config.keybindings.resolver());
        let compiled_highlights = Arc::from(compile_highlight_rules(&config.terminal.highlight_rules));
        let mut init_err: Option<String> = None;
        let quick_commands = match load_quick_commands(&paths) {
            Ok(qc) => qc,
            Err(err) => {
                warn!(error = %err, "failed loading quick_commands.toml; using empty collection");
                init_err = Some(err.to_string());
                QuickCommandsConfig::default()
            }
        };
        let workspaces_cfg = match load_workspaces(&paths) {
            Ok(ws) => ws,
            Err(err) => {
                warn!(error = %err, "failed loading workspaces.toml; using empty collection");
                if init_err.is_none() {
                    init_err = Some(err.to_string());
                }
                WorkspacesConfig::default()
            }
        };
        let sftp_state = match load_sftp_state(&paths) {
            Ok(st) => st,
            Err(err) => {
                warn!(error = %err, "failed loading sftp_state.toml; using empty state");
                SftpState::default()
            }
        };
        let sidebar = SidebarState::new();
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
            sidebar,
            command_palette: CommandPaletteState::new(),
            tabby_import: TabbyImportUiState::default(),
            status_bar: StatusBarState::new(),
            sftp_manager,
            sftp_sub_id: NEXT_SFTP_SUB_ID.fetch_add(1, Ordering::Relaxed),
            transfer_queue,
            sftp_state,
            sftp_drawers: BTreeMap::new(),
            sftp_fallback_drawer: SftpDrawerState::with_download_dir(download_dir.clone()),
            sftp_drawer_width: DEFAULT_SFTP_DRAWER_WIDTH,
            sftp_download_dir: download_dir,
            transfer_sessions: BTreeMap::new(),
            tunnel_manager,
            tunnel_ui: TunnelUiState::default(),
            settings_ui: SettingsUiState::default(),
            settings_origin_config: None,
            profile_editor: ProfileEditorState::default(),
            synced_standalone_tunnels: 0,
            known_hosts_override: None,
            next_session_id: 1,
            last_error: init_err,
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
            file_opener: Arc::new(open_local_file_with_system),
            notifier: ZbusDesktopNotifier::shared(),
            keybinding_resolver,
            pending_key_chords: Vec::new(),
            compiled_highlights,
            cursor_visible: true,
            visual_bell_generation: BTreeMap::new(),
            visual_bell_active: BTreeSet::new(),
            context_menu: ContextMenuState::default(),
            drag_drop: DragDropState::default(),
            last_saved_scrollback_path: None,
            quick_commands,
            workspaces_cfg,
            quick_commands_ui: QuickCommandsUiState::default(),
            workspaces_ui: WorkspacesUiState::default(),
            protocol_launcher: ProtocolLauncherState::default(),
            zmodem_send_ui: ZmodemSendUiState::default(),
            zmodem_controllers: BTreeMap::new(),
            last_zmodem_notice: None,
            serial_opener: Arc::new(OsSerialPortOpener),
            serial_port_lister: Arc::new(|| {
                list_available_serial_ports().map_err(|err| err.to_string())
            }),
            docker_runner_factory: Arc::new(|| Box::new(OsCommandRunner)),
            sync_service: Arc::new(ConfigSyncService::new()),
            quake: QuakeController::default(),
            tray_hidden: false,
            tray_adapter: Arc::new(NullSystemTray),
            tray_rx: None,
            hotkey_registrar: Arc::new(NullGlobalHotkeyRegistrar),
            hotkey_guard: None,
            hotkey_tx: None,
            hotkey_rx: None,
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

    /// Overrides the OS local-file opener boundary for `Ctrl+Click` file paths (used by tests).
    pub fn with_file_opener(mut self, opener: FileOpener) -> Self {
        self.file_opener = opener;
        self
    }

    /// Overrides the desktop notification sink (used by tests to record background bell alerts).
    pub fn with_notifier(mut self, notifier: Arc<dyn DesktopNotifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Overrides the serial port opener boundary (used by tests for in-memory serial loops).
    pub fn with_serial_opener(mut self, opener: Arc<dyn SerialPortOpener>) -> Self {
        self.serial_opener = opener;
        self
    }

    /// Overrides serial port discovery enumeration (used by tests).
    pub fn with_serial_port_lister<F>(mut self, lister: F) -> Self
    where
        F: Fn() -> Result<Vec<SerialPortInfo>, String> + Send + Sync + 'static,
    {
        self.serial_port_lister = Arc::new(lister);
        self
    }

    /// Overrides container discovery CLI runner factory (used by tests).
    pub fn with_docker_runner_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn() -> Box<dyn CommandOutputRunner> + Send + Sync + 'static,
    {
        self.docker_runner_factory = Arc::new(factory);
        self
    }

    /// Overrides the [`ConfigSyncService`] (used by tests with mock HTTP/fast KDF params).
    pub fn with_sync_service(mut self, sync_service: ConfigSyncService) -> Self {
        self.sync_service = Arc::new(sync_service);
        self
    }

    /// Overrides the System Tray adapter boundary and pushes an initial tray snapshot (used by tests).
    pub fn with_tray_adapter(mut self, adapter: Arc<dyn SystemTrayAdapter>) -> Self {
        self.tray_adapter = adapter;
        self.sync_tray_state();
        self
    }

    /// Overrides the global hotkey registrar boundary and prepares an event channel (used by tests).
    pub fn with_hotkey_registrar(mut self, registrar: Arc<dyn GlobalHotkeyRegistrar>) -> Self {
        self.hotkey_registrar = registrar;
        if self.hotkey_tx.is_none() {
            let (tx, rx) = mpsc::unbounded_channel();
            self.hotkey_tx = Some(tx);
            self.hotkey_rx = Some(Arc::new(Mutex::new(Some(rx))));
        }
        self
    }

    /// Synchronizes global hotkey registration against `self.config.quake`.
    pub fn sync_global_hotkey(&mut self) -> Result<(), String> {
        self.hotkey_guard = None;
        if !self.config.quake.enabled {
            return Ok(());
        }
        let Some(tx) = self.hotkey_tx.as_ref().cloned() else {
            return Ok(());
        };
        let cb: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(());
        });
        match self.hotkey_registrar.register(&self.config.quake.hotkey, cb) {
            Ok(guard) => {
                self.hotkey_guard = guard;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn attach_zmodem_controller(&mut self, session_id: SessionId, buffer: &Arc<TerminalBuffer>) {
        let download_dir = self.sftp_download_dir.clone();
        let engine = Arc::new(Mutex::new(ZmodemEngine::new(download_dir)));
        let pending_events = Arc::new(Mutex::new(Vec::<ZmodemEvent>::new()));
        let eng_ref = Arc::clone(&engine);
        let ev_ref = Arc::clone(&pending_events);
        buffer.set_output_interceptor(Some(Box::new(move |incoming: &[u8]| {
            let mut guard = eng_ref.lock().expect("zmodem engine mutex poisoned");
            match guard.feed_wire(incoming) {
                Ok(step) => {
                    if !step.events.is_empty() {
                        let mut ev_guard =
                            ev_ref.lock().expect("zmodem events mutex poisoned");
                        ev_guard.extend(step.events);
                    }
                    (step.terminal_output, step.wire_output)
                }
                Err(err) => {
                    let mut ev_guard = ev_ref.lock().expect("zmodem events mutex poisoned");
                    ev_guard.push(ZmodemEvent::Error(err.to_string()));
                    (incoming.to_vec(), Vec::new())
                }
            }
        })));
        self.zmodem_controllers.insert(
            session_id,
            ZmodemControllerHandle {
                engine,
                pending_events,
            },
        );
    }

    fn rebuild_runtime_config_caches(&mut self) {
        self.keybinding_resolver = Arc::new(self.config.keybindings.resolver());
        self.compiled_highlights =
            Arc::from(compile_highlight_rules(&self.config.terminal.highlight_rules));
        if !self.config.terminal.cursor_blink {
            self.cursor_visible = true;
        }
    }

    fn push_default_pending_window(&mut self) -> Result<(), String> {
        let mut workspace = Workspace::with_ids(self.workspace_ids.clone());
        let initial_session = self.spawn_default_local_session()?;
        workspace.create_tab(initial_session, None);
        self.pending_windows.push_back(WindowState {
            workspace,
            width: self.config.window.width,
            height: self.config.window.height,
            maximized: false,
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
                let tab_id =
                    workspace.restore_tab(tab_layout.title.clone(), root, focused_pane, zoomed_pane);
                workspace.set_tab_metadata(tab_id, tab_layout.color.clone(), tab_layout.pinned);
            }
            if workspace.tab_count() == 0 {
                continue;
            }
            workspace.set_active_tab_index(win_layout.active_tab);
            self.pending_windows.push_back(WindowState {
                workspace,
                width: win_layout.width.max(200),
                height: win_layout.height.max(150),
                maximized: false,
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
                let matched_profile = profile_id
                    .as_deref()
                    .and_then(|pid| self.profiles.profiles.iter().find(|p| p.id == pid))
                    .and_then(ssh_target_from_profile);

                let target = if let Some(mut prof_target) = matched_profile {
                    if let Some(custom_title) =
                        title.as_deref().map(str::trim).filter(|s| !s.is_empty())
                    {
                        prof_target.name = custom_title.to_string();
                    }
                    prof_target
                } else {
                    let display_name = title
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| match username.as_deref() {
                            Some(u) if !u.is_empty() => format!("{u}@{host}"),
                            _ => host.clone(),
                        });
                    SshSessionTarget {
                        profile_id: profile_id.clone(),
                        name: display_name,
                        host: host.clone(),
                        port: *port,
                        username: username
                            .as_deref()
                            .filter(|u| !u.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(default_os_username),
                        auth: SshAuthMethod::Agent,
                        proxy_jump: Vec::new(),
                        proxy_command: None,
                        proxy: None,
                        forward_agent: false,
                        forward_x11: false,
                        auto_connect_on_startup: *auto_connect_on_startup,
                        tmux_reattach: None,
                        keepalive_interval_secs: None,
                        keepalive_max_missed: None,
                        auto_attach_mux: false,
                        mux_attach_command: None,
                        login_scripts: Vec::new(),
                    }
                };

                let auto_connect = target.auto_connect_on_startup;
                let spec_title = target.name.clone();
                let (sid, _) = self.insert_ssh_slot_from_target(target, &spec_title);
                if auto_connect {
                    self.record_remote_connect_intent(sid);
                }
                Ok(sid)
            }
            SessionDescriptor::Serial {
                profile_id,
                port,
                baud_rate,
                data_bits,
                parity,
                stop_bits,
                flow_control,
                input_mode,
                output_mode,
                newline_mode,
                auto_reconnect,
                title,
            } => {
                let cfg = SerialConnectConfig {
                    port: port.clone(),
                    baud_rate: *baud_rate,
                    data_bits: *data_bits,
                    parity: *parity,
                    stop_bits: *stop_bits,
                    flow_control: *flow_control,
                    input_mode: *input_mode,
                    output_mode: *output_mode,
                    newline_mode: *newline_mode,
                    auto_reconnect: *auto_reconnect,
                    ..SerialConnectConfig::default()
                };
                Ok(self.insert_serial_session_slot(
                    cfg,
                    title.clone(),
                    profile_id.clone(),
                    true,
                ))
            }
            SessionDescriptor::Telnet {
                profile_id,
                host,
                port,
                auto_reconnect,
                title,
            } => {
                let login_scripts = profile_id
                    .as_deref()
                    .and_then(|pid| self.profiles.profiles.iter().find(|p| p.id == pid))
                    .and_then(|p| match &p.target {
                        ProfileTarget::Telnet { login_scripts, .. } => Some(login_scripts.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let cfg = TelnetConnectConfig {
                    host: host.clone(),
                    port: *port,
                    auto_reconnect: *auto_reconnect,
                    login_scripts,
                    ..TelnetConnectConfig::default()
                };
                Ok(self.insert_telnet_session_slot(
                    cfg,
                    title.clone(),
                    profile_id.clone(),
                    true,
                ))
            }
            SessionDescriptor::Docker {
                profile_id,
                runtime,
                container,
                shell,
                user,
                working_dir,
                title,
            } => {
                let exec_cfg = ContainerExecConfig {
                    runtime: *runtime,
                    container: container.clone(),
                    shell: shell.clone(),
                    user: user.clone(),
                    working_dir: working_dir.clone(),
                };
                self.spawn_docker_session(exec_cfg, title.clone(), profile_id.clone())
            }
        }
    }

    fn record_remote_connect_intent(&mut self, session_id: SessionId) {
        let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
            return;
        };
        let already_queued = self
            .pending_remote_connects
            .iter()
            .any(|i| i.session_id == session_id);
        if !already_queued {
            self.pending_remote_connects.push(RemoteConnectIntent {
                session_id,
                profile_id: slot.target.profile_id.clone(),
                host: slot.target.host.clone(),
                port: slot.target.port,
                username: (!slot.target.username.is_empty()).then(|| slot.target.username.clone()),
            });
        }
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
        self.spawn_local_session_inner(shell, working_dir, title_override, profile_id, None)
    }

    fn spawn_docker_session(
        &mut self,
        exec_cfg: ContainerExecConfig,
        title_override: Option<String>,
        profile_id: Option<String>,
    ) -> Result<SessionId, String> {
        let shell = build_exec_shell_spec(&exec_cfg).map_err(|e| e.to_string())?;
        let fallback_title = format!("docker: {}", exec_cfg.container);
        let title = title_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .unwrap_or(fallback_title);
        self.spawn_local_session_inner(shell, None, Some(title), profile_id, Some(exec_cfg))
    }

    fn spawn_local_session_inner(
        &mut self,
        shell: ShellSpec,
        working_dir: Option<PathBuf>,
        title_override: Option<String>,
        profile_id: Option<String>,
        docker_exec: Option<ContainerExecConfig>,
    ) -> Result<SessionId, String> {
        let spawn_cfg = PtySpawnConfig {
            shell: shell.clone(),
            cols: 80,
            rows: 24,
            scrollback_lines: self.config.terminal.scrollback_lines,
            cursor_shape: cursor_style_to_shape(self.config.terminal.cursor_style),
            cursor_blink: self.config.terminal.cursor_blink,
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

        let buffer = Arc::clone(pty.buffer());
        let session = LocalSession {
            id,
            profile_id,
            initial_working_dir: working_dir,
            docker_exec,
            title,
            pty,
            snapshot,
            transport_status: TransportStatus::Disconnected,
            exited: None,
        };

        self.sessions.insert(id, SessionEntry::Local(session));
        self.attach_zmodem_controller(id, &buffer);
        self.maybe_start_auto_log(id);
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
            buffer: Arc::clone(&buffer),
            snapshot,
            exited: None,
            generation: 0,
            active: None,
            supervisor: None,
            probe_count: 0,
            last_error: None,
            is_auto_reconnecting: false,
            reconnect_attempt: None,
            reconnected_at: None,
            server_stats: None,
            server_stats_unsupported: false,
        };
        self.sessions.insert(id, SessionEntry::Ssh(slot));
        let drawer = self.build_session_sftp_drawer(id);
        self.sftp_drawers.insert(id, drawer);
        self.attach_zmodem_controller(id, &buffer);
        self.maybe_start_auto_log(id);
        id
    }

    fn insert_serial_session_slot(
        &mut self,
        config: SerialConnectConfig,
        title_override: Option<String>,
        profile_id: Option<String>,
        connect_now: bool,
    ) -> SessionId {
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        let fallback_title = format!("{} @ {}", config.port, config.baud_rate);
        let title = title_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .unwrap_or(fallback_title);
        let buffer = Arc::new(TerminalBuffer::new_with_cursor_shape(
            80,
            24,
            self.config.terminal.scrollback_lines,
            cursor_style_to_shape(self.config.terminal.cursor_style),
        ));
        let opener = Arc::clone(&self.serial_opener);
        let (active, status) = if connect_now && tokio::runtime::Handle::try_current().is_ok() {
            let sess = SerialSession::connect_with_opener(
                config.clone(),
                Arc::clone(&buffer),
                Arc::clone(&opener),
            );
            let st = sess.status();
            (Some(sess), st)
        } else {
            (None, ProtocolStatus::Disconnected)
        };
        let snapshot = Arc::new(buffer.snapshot());
        let slot = SerialSessionSlot {
            id,
            profile_id,
            title,
            config,
            opener,
            buffer: Arc::clone(&buffer),
            snapshot,
            active,
            status,
            generation: 1,
            is_auto_reconnecting: false,
            reconnect_attempt: None,
            reconnected_at: None,
            hex_draft: String::new(),
            last_error: None,
        };
        self.sessions.insert(id, SessionEntry::Serial(slot));
        self.attach_zmodem_controller(id, &buffer);
        self.maybe_start_auto_log(id);
        id
    }

    /// Connects or reconnects a Serial `Session` (safe to call from async Tokio runtime).
    pub fn connect_serial_session(&mut self, session_id: SessionId) {
        if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id) {
            slot.last_error = None;
            slot.reconnected_at = None;
            if let Some(active) = &slot.active {
                active.reconnect_now();
                let st = active.status();
                slot.apply_protocol_status(st, Instant::now());
            } else if tokio::runtime::Handle::try_current().is_ok() {
                let sess = SerialSession::connect_with_opener(
                    slot.config.clone(),
                    Arc::clone(&slot.buffer),
                    Arc::clone(&slot.opener),
                );
                let st = sess.status();
                slot.generation = slot.generation.wrapping_add(1);
                slot.active = Some(sess);
                slot.apply_protocol_status(st, Instant::now());
            } else {
                slot.last_error =
                    Some("No active Tokio runtime to start async serial I/O task".to_string());
            }
        }
    }

    fn insert_telnet_session_slot(
        &mut self,
        config: TelnetConnectConfig,
        title_override: Option<String>,
        profile_id: Option<String>,
        connect_now: bool,
    ) -> SessionId {
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        let fallback_title = format!("{}:{}", config.host, config.port);
        let title = title_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .unwrap_or(fallback_title);
        let buffer = Arc::new(TerminalBuffer::new_with_cursor_shape(
            config.cols.max(1),
            config.rows.max(1),
            self.config.terminal.scrollback_lines,
            cursor_style_to_shape(self.config.terminal.cursor_style),
        ));
        let (active, status) = if connect_now && tokio::runtime::Handle::try_current().is_ok() {
            let sess = TelnetSession::connect_on_buffer(config.clone(), Arc::clone(&buffer));
            let st = sess.status();
            (Some(sess), st)
        } else {
            (None, ProtocolStatus::Disconnected)
        };
        let snapshot = Arc::new(buffer.snapshot());
        let slot = TelnetSessionSlot {
            id,
            profile_id,
            title,
            config,
            buffer: Arc::clone(&buffer),
            snapshot,
            active,
            status,
            generation: 1,
            is_auto_reconnecting: false,
            reconnect_attempt: None,
            reconnected_at: None,
            last_error: None,
        };
        self.sessions.insert(id, SessionEntry::Telnet(slot));
        self.attach_zmodem_controller(id, &buffer);
        self.maybe_start_auto_log(id);
        id
    }

    /// Connects or reconnects a Telnet `Session` (requires active Tokio runtime).
    pub fn connect_telnet_session(&mut self, session_id: SessionId) {
        if let Some(SessionEntry::Telnet(slot)) = self.sessions.get_mut(&session_id) {
            slot.last_error = None;
            slot.reconnected_at = None;
            if let Some(active) = &slot.active {
                active.reconnect_now();
                let st = active.status();
                slot.apply_protocol_status(st, Instant::now());
            } else if tokio::runtime::Handle::try_current().is_ok() {
                let sess =
                    TelnetSession::connect_on_buffer(slot.config.clone(), Arc::clone(&slot.buffer));
                let st = sess.status();
                slot.generation = slot.generation.wrapping_add(1);
                slot.active = Some(sess);
                slot.apply_protocol_status(st, Instant::now());
            } else {
                slot.last_error =
                    Some("No active Tokio runtime to start async Telnet TCP task".to_string());
            }
        }
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
        slot.is_auto_reconnecting = false;
        slot.reconnect_attempt = None;
        slot.reconnected_at = None;
        slot.exited = None;
        slot.server_stats = None;
        slot.server_stats_unsupported = false;

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
            slot.reconnected_at = None;
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
            slot.is_auto_reconnecting = false;
            slot.reconnect_attempt = None;
            slot.reconnected_at = None;
            slot.transport_status = TransportStatus::Disconnected;
            slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
            slot.server_stats = None;
            slot.server_stats_unsupported = false;
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
            slot.is_auto_reconnecting = false;
            slot.reconnect_attempt = None;
            slot.reconnected_at = None;
            slot.transport_status = TransportStatus::Disconnected;
            slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
            slot.server_stats = None;
            slot.server_stats_unsupported = false;
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
        let opened = match disposition {
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
        };
        if opened {
            self.sync_status_bar_drawer_open();
        }
        opened
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
            ProfileTarget::Ssh { .. } => {
                let target = ssh_target_from_profile(&profile).expect("matched SSH target");
                let tab_title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                self.launch_resolved_ssh_target(window_scope, target, &profile.name, tab_title, disposition)
            }
            ProfileTarget::Serial { .. } => {
                let cfg = SerialConnectConfig::from_profile_target(&profile.target)
                    .expect("matched Serial target");
                let title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                let sid = self.insert_serial_session_slot(
                    cfg,
                    title.clone(),
                    Some(profile.id.clone()),
                    true,
                );
                let _ = self.open_session_with_disposition(window_scope, sid, title, disposition);
                Task::none()
            }
            ProfileTarget::Telnet { .. } => {
                let cfg = TelnetConnectConfig::from_profile_target(&profile.target, 80, 24)
                    .expect("matched Telnet target");
                let title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                let sid = self.insert_telnet_session_slot(
                    cfg,
                    title.clone(),
                    Some(profile.id.clone()),
                    true,
                );
                let _ = self.open_session_with_disposition(window_scope, sid, title, disposition);
                Task::none()
            }
            ProfileTarget::Docker { .. } => {
                let exec_cfg = ContainerExecConfig::from_profile_target(&profile.target)
                    .expect("matched Docker target");
                let title = (!profile.name.trim().is_empty()).then(|| profile.name.clone());
                match self.spawn_docker_session(exec_cfg, title.clone(), Some(profile.id.clone())) {
                    Ok(sid) => {
                        let _ = self.open_session_with_disposition(
                            window_scope,
                            sid,
                            title,
                            disposition,
                        );
                    }
                    Err(err) => {
                        error!(profile_id = %profile.id, error = %err, "failed to launch Docker Profile");
                        self.last_error = Some(err);
                    }
                }
                Task::none()
            }
        }
    }

    fn build_target_spec(
        &self,
        target: &SshSessionTarget,
        title: &str,
        cols: u16,
        rows: u16,
    ) -> Result<SshTargetSpec, String> {
        let hops = resolve_proxy_jump_hops(
            &target.proxy_jump,
            &self.profiles.profiles,
            &self.openssh_hosts,
            &target.username,
            None,
        )
        .map_err(|err| err.to_string())?;
        Ok(target.to_target_spec_with_hops(title, cols, rows, &self.config, hops))
    }

    fn insert_ssh_slot_from_target(
        &mut self,
        target: SshSessionTarget,
        spec_title: &str,
    ) -> (SessionId, Option<String>) {
        match self.build_target_spec(&target, spec_title, 80, 24) {
            Ok(spec) => (self.insert_ssh_session_slot(target, spec), None),
            Err(err) => {
                warn!(target = %target.name, error = %err, "failed resolving ProxyJump hops");
                self.last_error = Some(err.clone());
                let fallback_spec = target.to_target_spec_with_hops(
                    spec_title,
                    80,
                    24,
                    &self.config,
                    Vec::new(),
                );
                let sid = self.insert_ssh_session_slot(target, fallback_spec);
                if let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&sid) {
                    slot.last_error = Some(err.clone());
                }
                (sid, Some(err))
            }
        }
    }

    fn launch_resolved_ssh_target(
        &mut self,
        window_scope: Option<window::Id>,
        target: SshSessionTarget,
        spec_title: &str,
        tab_title: Option<String>,
        disposition: LaunchDisposition,
    ) -> Task<Message> {
        let (sid, hop_err) = self.insert_ssh_slot_from_target(target, spec_title);
        if self.open_session_with_disposition(window_scope, sid, tab_title, disposition)
            && hop_err.is_none()
        {
            self.start_ssh_connect(sid)
        } else {
            Task::none()
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
            proxy_command: None,
            proxy: None,
            forward_agent: false,
            forward_x11: false,
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
            login_scripts: Vec::new(),
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

    fn mutate_profiles_and_save<F>(&mut self, mutate: F) -> bool
    where
        F: FnOnce(&mut ProfilesConfig) -> bool,
    {
        let mut next_profiles = self.profiles.clone();
        if !mutate(&mut next_profiles) {
            return false;
        }
        if let Err(err) = save_profiles(&self.paths, &next_profiles) {
            self.last_error = Some(err.to_string());
            return false;
        }
        self.profiles = next_profiles;
        self.last_error = None;
        self.sync_tunnel_telemetry();
        true
    }

    fn handle_drag_finish(&mut self, window_scope: Option<window::Id>) -> Task<Message> {
        let Some((payload, target_opt)) = self.drag_drop.take_drop() else {
            return Task::none();
        };
        let Some(target) = target_opt else {
            return Task::none();
        };

        match (payload, target) {
            (
                DragPayload::Tab { tab_id, .. },
                DropTarget::TabSlot {
                    tab_id: target_id,
                    after,
                },
            ) => {
                if tab_id != target_id
                    && let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id)
                    && let (Some(from_idx), Some(target_idx)) = (
                        ws.tabs().iter().position(|t| t.id == tab_id),
                        ws.tabs().iter().position(|t| t.id == target_id),
                    )
                {
                    let desired_slot = if after { target_idx + 1 } else { target_idx };
                    let to_idx = if from_idx < desired_slot {
                        desired_slot.saturating_sub(1)
                    } else {
                        desired_slot
                    };
                    ws.reorder_tab(tab_id, to_idx);
                }
                Task::none()
            }
            (DragPayload::Tab { tab_id, .. }, DropTarget::TabStripEnd) => {
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    let last_idx = ws.tab_count().saturating_sub(1);
                    ws.reorder_tab(tab_id, last_idx);
                }
                Task::none()
            }
            (DragPayload::SavedProfile { profile_id, .. }, DropTarget::ProfileGroup(group)) => {
                if self.mutate_profiles_and_save(|cfg| {
                    cfg.move_profile_to_group(&profile_id, group.as_deref())
                }) && let Some(g) = group
                {
                    let clean = g.trim();
                    if !clean.is_empty() && !clean.eq_ignore_ascii_case(UNGROUPED_LABEL) {
                        self.sidebar.set_group_collapsed(clean, false);
                    }
                }
                Task::none()
            }
            (DragPayload::SystemSsh { alias, .. }, DropTarget::ProfileGroup(group)) => {
                self.clone_system_ssh_to_profile_into_group(&alias, group.as_deref());
                Task::none()
            }
            (DragPayload::SavedProfile { profile_id, .. }, DropTarget::TabStripEnd) => self
                .update_in_window(
                    window_scope,
                    Message::LaunchProfile {
                        profile_id,
                        disposition: LaunchDisposition::NewTab,
                    },
                ),
            (
                DragPayload::SavedProfile { profile_id, .. },
                DropTarget::WorkspacePane {
                    pane_id,
                    split_axis,
                },
            ) => {
                if let Some(pid) = pane_id
                    && let Some(ws) = self.workspace_for_pane_mut(pid)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.focus_pane(tab_id, pid);
                }
                let disposition =
                    split_axis.map_or(LaunchDisposition::NewTab, LaunchDisposition::Split);
                self.update_in_window(
                    window_scope,
                    Message::LaunchProfile {
                        profile_id,
                        disposition,
                    },
                )
            }
            (DragPayload::SystemSsh { alias, .. }, DropTarget::TabStripEnd) => self
                .update_in_window(
                    window_scope,
                    Message::LaunchSystemSsh {
                        alias,
                        disposition: LaunchDisposition::NewTab,
                    },
                ),
            (
                DragPayload::SystemSsh { alias, .. },
                DropTarget::WorkspacePane {
                    pane_id,
                    split_axis,
                },
            ) => {
                if let Some(pid) = pane_id
                    && let Some(ws) = self.workspace_for_pane_mut(pid)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.focus_pane(tab_id, pid);
                }
                let disposition =
                    split_axis.map_or(LaunchDisposition::NewTab, LaunchDisposition::Split);
                self.update_in_window(
                    window_scope,
                    Message::LaunchSystemSsh { alias, disposition },
                )
            }
            _ => Task::none(),
        }
    }

    fn clone_system_ssh_to_profile(&mut self, alias: &str) {
        self.clone_system_ssh_to_profile_into_group(alias, None);
    }

    fn clone_system_ssh_to_profile_into_group(&mut self, alias: &str, group: Option<&str>) {
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
                let profile_id = profile.id.clone();
                self.profiles.profiles.push(profile);
                self.last_error = None;
                if let Some(target_group) = group {
                    let clean_group = target_group.trim().to_string();
                    if self.mutate_profiles_and_save(|cfg| {
                        cfg.move_profile_to_group(&profile_id, Some(clean_group.as_str()))
                    }) {
                        self.sidebar.set_group_collapsed(&clean_group, false);
                    }
                }
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
            proxy_command: host.proxy_command,
            proxy: None,
            forward_agent: host.forward_agent.unwrap_or(false),
            forward_x11: host.forward_x11.unwrap_or(false),
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
            login_scripts: Vec::new(),
        };
        let alias = host.alias.clone();
        self.launch_resolved_ssh_target(window_scope, target, &alias, Some(host.alias), disposition)
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
            StatusBarAction::OpenSettings => self.update(Message::OpenSettingsModal),
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
            StatusBarAction::ReconnectTunnels => self.reconnect_all_tunnels(),
            StatusBarAction::ToggleTransferDrawer => self.toggle_sftp_drawer(),
            StatusBarAction::LockVault => self.update(Message::VaultLock),
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
            PaletteAction::OpenSettings => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenSettingsModal)
            }
            PaletteAction::OpenNewProfile => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenNewProfileModal)
            }
            PaletteAction::CreateProfileGroup => {
                self.command_palette.close();
                self.update_in_window(
                    window_scope,
                    Message::BeginCreateGroup {
                        assign_profile_id: None,
                    },
                )
            }
            PaletteAction::OpenTabbyImporter => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenTabbyImportModal)
            }
            PaletteAction::OpenTunnelCreator => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenTunnelCreateModal)
            }
            PaletteAction::LockVault => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::VaultLock)
            }
            PaletteAction::ReloadSystemSsh => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::ReloadSystemSshConfig)
            }
            PaletteAction::NewTab => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::NewTab)
            }
            PaletteAction::DuplicateTab => {
                self.command_palette.close();
                if let Some(tab_id) = self
                    .target_workspace(window_scope)
                    .and_then(|ws| ws.active_tab_id())
                {
                    return self.update_in_window(window_scope, Message::DuplicateTab(tab_id));
                }
                Task::none()
            }
            PaletteAction::RenameTab => {
                self.command_palette.close();
                if let Some(tab_id) = self
                    .target_workspace(window_scope)
                    .and_then(|ws| ws.active_tab_id())
                {
                    return self.update_in_window(window_scope, Message::BeginTabRename(tab_id));
                }
                Task::none()
            }
            PaletteAction::ClearTerminal => {
                self.command_palette.close();
                if let Some(sid) = self.active_session_in_window(window_scope) {
                    return self.update_in_window(window_scope, Message::ClearTerminalBuffer(sid));
                }
                Task::none()
            }
            PaletteAction::SaveScrollback => {
                self.command_palette.close();
                if let Some(sid) = self.active_session_in_window(window_scope) {
                    return self.update_in_window(window_scope, Message::SaveScrollback(sid));
                }
                Task::none()
            }
            PaletteAction::ToggleSessionLogging => {
                self.command_palette.close();
                if let Some(sid) = self.active_session_in_window(window_scope) {
                    return self.update_in_window(window_scope, Message::ToggleSessionLogging(sid));
                }
                Task::none()
            }
            PaletteAction::OpenQuickCommands => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenQuickCommandsModal)
            }
            PaletteAction::OpenWorkspaces => {
                self.command_palette.close();
                self.update_in_window(window_scope, Message::OpenWorkspacesModal)
            }
            PaletteAction::OpenProtocolLauncher => {
                self.command_palette.close();
                self.update_in_window(
                    window_scope,
                    Message::OpenProtocolLauncherModal(ProtocolLauncherTab::Containers),
                )
            }
            PaletteAction::TriggerZmodemSend => {
                self.command_palette.close();
                if let Some(sid) = self.active_session_in_window(window_scope) {
                    return self.update_in_window(window_scope, Message::TriggerZmodemSend(sid));
                }
                Task::none()
            }
            PaletteAction::RunQuickCommand(id) => {
                self.command_palette.close();
                self.update_in_window(
                    window_scope,
                    Message::ExecuteQuickCommand {
                        id,
                        broadcast_override: None,
                    },
                )
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
        self.visual_bell_generation.remove(&session_id);
        self.visual_bell_active.remove(&session_id);
        self.zmodem_controllers.remove(&session_id);
        self.sftp_drawers.remove(&session_id);
        if let Some(entry) = self.sessions.get(&session_id)
            && let Some(buf) = entry.buffer()
        {
            let _ = buf.stop_logging();
        }
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
            Some(SessionEntry::Serial(slot)) => {
                if let Some(active) = slot.active {
                    active.disconnect();
                }
            }
            Some(SessionEntry::Telnet(slot)) => {
                if let Some(active) = slot.active {
                    active.disconnect();
                }
            }
            None => {}
        }
        self.sync_tunnel_telemetry();
        self.sync_status_bar_drawer_open();
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

    fn workspace_for_tab(
        &self,
        window_scope: Option<window::Id>,
        tab_id: TabId,
    ) -> Option<&Workspace> {
        if let Some(w) = self
            .window_workspaces
            .values()
            .find(|w| w.workspace.contains_tab(tab_id))
        {
            return Some(&w.workspace);
        }
        if let Some(w) = self
            .pending_windows
            .iter()
            .find(|w| w.workspace.contains_tab(tab_id))
        {
            return Some(&w.workspace);
        }
        if let Some(wid) = self.target_window_id(window_scope) {
            return self.window_workspaces.get(&wid).map(|w| &w.workspace);
        }
        self.primary_workspace()
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

    fn active_session_in_window(&self, window_scope: Option<window::Id>) -> Option<SessionId> {
        self.target_workspace(window_scope)
            .and_then(|ws| ws.active_tab())
            .and_then(|tab| tab.focused_session_id())
    }

    fn handle_terminal_bell(&mut self, session_id: SessionId) -> Task<Message> {
        let is_focused_active = self.active_session_id() == Some(session_id);
        let title = self
            .sessions
            .get(&session_id)
            .map(SessionEntry::title)
            .unwrap_or("Terminal")
            .to_string();

        if !is_focused_active
            && let Some(ws) = self.workspace_for_session_mut(session_id)
            && let Some(tab_id) = ws
                .tabs()
                .iter()
                .find(|t| t.root.pane_for_session(session_id).is_some())
                .map(|t| t.id)
        {
            ws.set_tab_bell(tab_id, true);
        }

        let behavior = self.config.terminal.bell_behavior;
        if !is_focused_active
            && matches!(
                behavior,
                BellBehavior::Notification | BellBehavior::VisualAndNotification
            )
        {
            self.notifier.notify(
                &format!("Bell: {title}"),
                "Activity alert in background terminal session",
            );
        }

        if matches!(
            behavior,
            BellBehavior::Visual | BellBehavior::VisualAndNotification
        ) {
            let next_gen = self
                .visual_bell_generation
                .get(&session_id)
                .copied()
                .unwrap_or(0)
                .wrapping_add(1);
            self.visual_bell_generation.insert(session_id, next_gen);
            self.visual_bell_active.insert(session_id);
            return Task::perform(
                async {
                    tokio::time::sleep(Duration::from_millis(220)).await;
                },
                move |()| Message::ClearVisualBell {
                    session_id,
                    generation: next_gen,
                },
            );
        }

        Task::none()
    }

    fn clear_terminal_buffer(&mut self, session_id: SessionId) {
        if let Some(entry) = self.sessions.get_mut(&session_id)
            && let Some(buf) = entry.buffer().cloned()
        {
            buf.clear_screen_and_scrollback();
            entry.refresh_snapshot();
        }
    }

    /// Exports buffer & scrollback text of `session_id` to `path`, returning the saved path.
    pub fn save_scrollback_to_path(
        &mut self,
        session_id: SessionId,
        path: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let buf = self
            .sessions
            .get(&session_id)
            .and_then(SessionEntry::buffer)
            .ok_or_else(|| format!("unknown session #{}", session_id.0))?;
        let text = buf.export_scrollback_text();
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed creating log directory: {err}"))?;
        }
        std::fs::write(&path, text)
            .map_err(|err| format!("failed saving scrollback to {}: {err}", path.display()))?;
        self.last_saved_scrollback_path = Some(path.clone());
        Ok(path)
    }

    /// Toggles streaming session output logging on `session_id`, returning active path when started.
    pub fn toggle_session_logging(
        &mut self,
        session_id: SessionId,
    ) -> Result<Option<PathBuf>, String> {
        let entry = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| format!("unknown session #{}", session_id.0))?;
        let title = entry.title().to_string();
        let buf = entry
            .buffer()
            .cloned()
            .ok_or_else(|| "session has no terminal buffer".to_string())?;
        if buf.log_path().is_some() {
            buf.stop_logging();
            return Ok(None);
        }
        let logs_dir = self
            .config
            .terminal
            .log_directory
            .clone()
            .unwrap_or_else(|| self.paths.logs_dir());
        let path = default_session_log_path(&logs_dir, &title);
        let started = buf
            .start_logging(path, true)
            .map_err(|err| format!("failed starting session log: {err}"))?;
        Ok(Some(started))
    }

    fn maybe_start_auto_log(&mut self, session_id: SessionId) {
        if !self.config.terminal.auto_log_sessions {
            return;
        }
        if let Some(entry) = self.sessions.get(&session_id)
            && let Some(buf) = entry.buffer()
            && buf.log_path().is_none()
        {
            let logs_dir = self
                .config
                .terminal
                .log_directory
                .clone()
                .unwrap_or_else(|| self.paths.logs_dir());
            let path = default_session_log_path(&logs_dir, entry.title());
            if let Err(err) = buf.start_logging(path, true) {
                warn!(session = session_id.0, error = %err, "auto session log failed to start");
            }
        }
    }

    fn duplicate_tab(
        &mut self,
        window_scope: Option<window::Id>,
        tab_id: TabId,
    ) -> Task<Message> {
        let Some((source_idx, tab_title, tab_color, sid)) = self
            .workspace_for_tab(window_scope, tab_id)
            .and_then(|ws| {
                let idx = ws.tabs().iter().position(|t| t.id == tab_id)?;
                let tab = &ws.tabs()[idx];
                Some((
                    idx,
                    tab.title.clone(),
                    tab.color.clone(),
                    tab.focused_session_id()?,
                ))
            })
        else {
            return Task::none();
        };

        enum CloneSource {
            Local {
                shell: ShellSpec,
                cwd: Option<PathBuf>,
                profile_id: Option<String>,
                docker_exec: Option<ContainerExecConfig>,
            },
            Ssh {
                target: SshSessionTarget,
                spec: SshTargetSpec,
            },
            Serial {
                config: SerialConnectConfig,
                profile_id: Option<String>,
            },
            Telnet {
                config: TelnetConnectConfig,
                profile_id: Option<String>,
            },
        }

        let source = match self.sessions.get(&sid) {
            Some(SessionEntry::Local(local)) => CloneSource::Local {
                shell: local.pty.shell().clone(),
                cwd: local
                    .pty
                    .current_working_dir()
                    .or_else(|| local.initial_working_dir.clone()),
                profile_id: local.profile_id.clone(),
                docker_exec: local.docker_exec.clone(),
            },
            Some(SessionEntry::Ssh(slot)) => CloneSource::Ssh {
                target: slot.target.clone(),
                spec: slot.spec.clone(),
            },
            Some(SessionEntry::Serial(slot)) => CloneSource::Serial {
                config: slot.config.clone(),
                profile_id: slot.profile_id.clone(),
            },
            Some(SessionEntry::Telnet(slot)) => CloneSource::Telnet {
                config: slot.config.clone(),
                profile_id: slot.profile_id.clone(),
            },
            None => return Task::none(),
        };

        match source {
            CloneSource::Local {
                shell,
                cwd,
                profile_id,
                docker_exec,
            } => match self.spawn_local_session_inner(
                shell,
                cwd,
                tab_title.clone(),
                profile_id,
                docker_exec,
            ) {
                Ok(new_sid) => {
                    if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                        let (new_tab, _) = ws.create_tab(new_sid, tab_title);
                        ws.set_tab_metadata(new_tab, tab_color, false);
                        ws.reorder_tab(new_tab, source_idx + 1);
                    } else {
                        self.remove_session(new_sid);
                    }
                    Task::none()
                }
                Err(err) => {
                    self.last_error = Some(err);
                    Task::none()
                }
            },
            CloneSource::Ssh { target, spec } => {
                let new_sid = self.insert_ssh_session_slot(target, spec);
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    let (new_tab, _) = ws.create_tab(new_sid, tab_title);
                    ws.set_tab_metadata(new_tab, tab_color, false);
                    ws.reorder_tab(new_tab, source_idx + 1);
                    self.start_ssh_connect(new_sid)
                } else {
                    self.remove_session(new_sid);
                    Task::none()
                }
            }
            CloneSource::Serial { config, profile_id } => {
                let new_sid = self.insert_serial_session_slot(
                    config,
                    tab_title.clone(),
                    profile_id,
                    true,
                );
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    let (new_tab, _) = ws.create_tab(new_sid, tab_title);
                    ws.set_tab_metadata(new_tab, tab_color, false);
                    ws.reorder_tab(new_tab, source_idx + 1);
                } else {
                    self.remove_session(new_sid);
                }
                Task::none()
            }
            CloneSource::Telnet { config, profile_id } => {
                let new_sid = self.insert_telnet_session_slot(
                    config,
                    tab_title.clone(),
                    profile_id,
                    true,
                );
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    let (new_tab, _) = ws.create_tab(new_sid, tab_title);
                    ws.set_tab_metadata(new_tab, tab_color, false);
                    ws.reorder_tab(new_tab, source_idx + 1);
                } else {
                    self.remove_session(new_sid);
                }
                Task::none()
            }
        }
    }

    fn open_local_file(
        &mut self,
        session_id: SessionId,
        raw_path: &str,
        line: Option<u32>,
        col: Option<u32>,
    ) {
        let resolved = if let Some(rest) = raw_path.strip_prefix("~/") {
            if let Some(home) = self.paths.home_dir() {
                home.join(rest)
            } else {
                PathBuf::from(raw_path)
            }
        } else {
            let p = PathBuf::from(raw_path);
            if p.is_absolute() {
                p
            } else if let Some(cwd) = self
                .sessions
                .get(&session_id)
                .and_then(SessionEntry::as_local)
                .and_then(|s| {
                    s.pty
                        .current_working_dir()
                        .or_else(|| s.initial_working_dir.clone())
                })
            {
                cwd.join(p)
            } else {
                p
            }
        };
        if let Err(err) = (self.file_opener)(&resolved, line, col) {
            warn!(path = %resolved.display(), error = %err, "failed opening local file link");
            self.last_error = Some(err);
        }
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
        let (win_id, open_task) = window::open(main_window_settings(width, height));
        self.windows.insert(win_id);
        self.window_workspaces.insert(
            win_id,
            WindowState {
                workspace,
                width,
                height,
                maximized: false,
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
        self.open_tab_in_window_with_cwd(window_scope, profile_id, None)
    }

    fn open_tab_in_window_with_cwd(
        &mut self,
        window_scope: Option<window::Id>,
        profile_id: Option<&str>,
        cwd_override: Option<PathBuf>,
    ) -> Task<Message> {
        let target_win = self.target_window_id(window_scope);
        if target_win.is_none() && self.pending_windows.is_empty() {
            let task = self.open_coordinated_window();
            if profile_id.is_none() && cwd_override.is_none() {
                return task;
            }
            let new_win = self.active_window;
            return Task::batch([
                task,
                self.spawn_profile_tab_with_cwd(new_win, profile_id, cwd_override),
            ]);
        }
        let extra = self.spawn_profile_tab_with_cwd(target_win, profile_id, cwd_override);
        if let Some(wid) = target_win {
            self.active_window = Some(wid);
            Task::batch([window::gain_focus(wid), extra])
        } else {
            extra
        }
    }

    fn spawn_profile_tab_with_cwd(
        &mut self,
        target_win: Option<window::Id>,
        profile_id: Option<&str>,
        cwd_override: Option<PathBuf>,
    ) -> Task<Message> {
        let valid_override_cwd = cwd_override.filter(|d| d.is_dir());
        let profile = profile_id
            .and_then(|pid| {
                self.profiles
                    .profiles
                    .iter()
                    .find(|p| p.id == pid)
                    .or_else(|| {
                        self.profiles
                            .profiles
                            .iter()
                            .find(|p| p.name.eq_ignore_ascii_case(pid))
                    })
            })
            .cloned();

        let spawned = match profile {
            Some(prof) => match &prof.target {
                ProfileTarget::LocalShell {
                    shell,
                    working_dir,
                    args,
                } => {
                    let spec = self.resolve_shell_spec(shell.as_deref(), args);
                    let valid_cwd = valid_override_cwd.or_else(|| working_dir.clone().filter(|d| d.is_dir()));
                    let title = (!prof.name.trim().is_empty()).then(|| prof.name.clone());
                    self.spawn_local_session(spec, valid_cwd, title, Some(prof.id))
                        .map(|sid| (sid, None))
                }
                ProfileTarget::Ssh { .. } => {
                    let target = ssh_target_from_profile(&prof).expect("matched SSH target");
                    let auto_connect = target.auto_connect_on_startup;
                    let (sid, hop_err) = self.insert_ssh_slot_from_target(target, &prof.name);
                    let connect_sid = (auto_connect && hop_err.is_none()).then_some(sid);
                    Ok((sid, connect_sid))
                }
                ProfileTarget::Serial { .. } => {
                    let cfg = SerialConnectConfig::from_profile_target(&prof.target)
                        .expect("matched Serial target");
                    let title = (!prof.name.trim().is_empty()).then(|| prof.name.clone());
                    Ok((
                        self.insert_serial_session_slot(cfg, title, Some(prof.id), true),
                        None,
                    ))
                }
                ProfileTarget::Telnet { .. } => {
                    let cfg = TelnetConnectConfig::from_profile_target(&prof.target, 80, 24)
                        .expect("matched Telnet target");
                    let title = (!prof.name.trim().is_empty()).then(|| prof.name.clone());
                    Ok((
                        self.insert_telnet_session_slot(cfg, title, Some(prof.id), true),
                        None,
                    ))
                }
                ProfileTarget::Docker { .. } => {
                    let exec_cfg = ContainerExecConfig::from_profile_target(&prof.target)
                        .expect("matched Docker target");
                    let title = (!prof.name.trim().is_empty()).then(|| prof.name.clone());
                    self.spawn_docker_session(exec_cfg, title, Some(prof.id))
                        .map(|sid| (sid, None))
                }
            },
            None => {
                let shell = self.resolve_shell_spec(None, &[]);
                self.spawn_local_session(shell, valid_override_cwd, None, None)
                    .map(|sid| (sid, None))
            }
        };

        match spawned {
            Ok((session_id, auto_remote)) => {
                if let Some(ws) = self.target_workspace_mut(target_win) {
                    ws.create_tab(session_id, None);
                    if let Some(remote_sid) = auto_remote {
                        self.record_remote_connect_intent(remote_sid);
                        return self.start_ssh_connect(remote_sid);
                    }
                } else {
                    self.remove_session(session_id);
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
                if Some(id) != self.quake.window_id() {
                    self.tray_hidden = false;
                }
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
                            maximized: false,
                        }
                    };
                    self.window_workspaces.insert(id, win_state);
                }
                self.sync_tray_state();
                if Some(id) == self.quake.window_id() {
                    window::monitor_size(id).map(move |sz| Message::QuakeMonitorSize(id, sz))
                } else {
                    Task::none()
                }
            }
            Message::WindowFocused(id) => {
                if self.windows.contains(&id) {
                    self.active_window = Some(id);
                }
                Task::none()
            }
            Message::WindowUnfocused(id) => {
                self.drag_drop.cancel();
                self.drag_drop.clear_external_file_hover();
                if self.config.quake.hide_on_blur
                    && self.quake.window_id() == Some(id)
                    && self.quake.is_visible()
                {
                    self.quake.set_visible(false);
                    self.sync_tray_state();
                    return window::set_mode(id, window::Mode::Hidden);
                }
                Task::none()
            }
            Message::QuakeMonitorSize(id, size_opt) => {
                if Some(id) == self.quake.window_id()
                    && let Some(sz) = size_opt
                {
                    self.quake.set_monitor_size(sz);
                    if self.quake.is_visible() {
                        let geom = self.quake.current_bounds(&self.config.quake);
                        return Task::batch([
                            window::move_to(id, Point::new(geom.x, geom.y)),
                            window::resize(id, Size::new(geom.width, geom.height)),
                        ]);
                    }
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
            Message::WindowDragStart(id) => window::drag(id),
            Message::WindowMinimize(id) => window::minimize(id, true),
            Message::WindowToggleMaximize(id) => {
                if let Some(win_state) = self.window_workspaces.get_mut(&id) {
                    win_state.maximized = !win_state.maximized;
                }
                window::toggle_maximize(id)
            }
            Message::WindowShowSystemMenu(id) => window::show_system_menu(id),
            Message::WindowEdgeResize(id, dir) => window::drag_resize(id, dir),
            Message::NewWindow => self.open_coordinated_window(),
            Message::IpcCommand(cmd) => match cmd {
                IpcCommand::OpenWindow => {
                    self.tray_hidden = false;
                    if self.windows.is_empty()
                        && self.lifecycle == DaemonLifecycle::BackgroundRunning
                        && let Ok(layout) = load_layout(&self.paths)
                        && !layout.windows.is_empty()
                        && self.restore_from_layout(&layout).unwrap_or(false)
                    {
                        let mut tasks = self.open_pending_windows();
                        for intent in &self.pending_remote_connects {
                            if self
                                .ssh_session(intent.session_id)
                                .is_some_and(|slot| slot.generation == 0)
                            {
                                tasks.push(Task::done(Message::ConnectRemoteSession(
                                    intent.session_id,
                                )));
                            }
                        }
                        self.lifecycle = DaemonLifecycle::Active;
                        self.sync_tray_state();
                        return Task::batch(tasks);
                    }
                    let task = self.open_coordinated_window();
                    self.sync_tray_state();
                    task
                }
                IpcCommand::OpenTab { profile_id, cwd } => {
                    self.tray_hidden = false;
                    let task = self.open_tab_in_window_with_cwd(None, profile_id.as_deref(), cwd);
                    self.sync_tray_state();
                    task
                }
                IpcCommand::ToggleQuake => self.toggle_quake_window(),
            },
            Message::CloseRequested(id) => {
                if Some(id) == self.quake.window_id() {
                    self.quake.set_visible(false);
                    self.sync_tray_state();
                    return window::set_mode(id, window::Mode::Hidden);
                }
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
                if Some(id) == self.quake.window_id() {
                    self.quake.clear_window();
                }
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
                            match entry {
                                SessionEntry::Ssh(slot) => {
                                    self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                                    if slot.supervisor.is_some() || slot.active.is_some() {
                                        ssh_handles.push((slot.supervisor.take(), slot.active.take()));
                                    }
                                }
                                SessionEntry::Serial(slot) => {
                                    if let Some(active) = slot.active.take() {
                                        active.disconnect();
                                    }
                                }
                                SessionEntry::Telnet(slot) => {
                                    if let Some(active) = slot.active.take() {
                                        active.disconnect();
                                    }
                                }
                                SessionEntry::Local(_) => {}
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
                        Some(SessionEntry::Serial(slot)) => {
                            if let Some(active) = slot.active {
                                active.disconnect();
                            }
                        }
                        Some(SessionEntry::Telnet(slot)) => {
                            if let Some(active) = slot.active {
                                active.disconnect();
                            }
                        }
                        None => {}
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
                self.record_remote_connect_intent(session_id);
                self.reconnect_ssh_session(session_id)
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
                    Some(SessionEntry::Serial(slot)) => {
                        slot.buffer.resize(cols, rows);
                        slot.snapshot = Arc::new(slot.buffer.snapshot());
                    }
                    Some(SessionEntry::Telnet(slot)) => {
                        slot.config.cols = cols.max(1);
                        slot.config.rows = rows.max(1);
                        if let Some(telnet) = &slot.active {
                            if let Err(err) = telnet.resize(cols, rows) {
                                error!(
                                    session = session_id.0,
                                    error = %err,
                                    "failed to resize Telnet NAWS"
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
                let mut should_drain_zmodem = false;
                if let Some(entry) = self.sessions.get_mut(&session_id) {
                    match event {
                        PtyEvent::Output => {
                            entry.refresh_snapshot();
                            should_drain_zmodem = true;
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
                                    SessionEntry::Serial(s) => s.title = title,
                                    SessionEntry::Telnet(s) => s.title = title,
                                }
                            }
                        }
                        PtyEvent::Bell => {
                            return self.handle_terminal_bell(session_id);
                        }
                        PtyEvent::Progress(progress) => {
                            if let Some(ws) = self.workspace_for_session_mut(session_id)
                                && let Some(tab_id) = ws
                                    .tabs()
                                    .iter()
                                    .find(|t| t.root.pane_for_session(session_id).is_some())
                                    .map(|t| t.id)
                            {
                                ws.set_tab_progress(tab_id, progress);
                            }
                        }
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
                                SessionEntry::Serial(slot) => {
                                    slot.status = ProtocolStatus::Disconnected;
                                }
                                SessionEntry::Telnet(slot) => {
                                    slot.status = ProtocolStatus::Disconnected;
                                }
                            }
                        }
                    }
                }
                if should_drain_zmodem {
                    self.drain_zmodem_events(session_id);
                }
                Task::none()
            }
            Message::ApplyTerminalConfig(cfg) => {
                self.config.terminal = cfg;
                if self.settings_ui.is_open() {
                    self.settings_origin_config = Some(self.config.clone());
                    self.settings_ui.sync_from_config(&self.config);
                }
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
                if self.drag_drop.is_dragging() {
                    self.drag_drop.cancel();
                    return Task::none();
                }
                if self.context_menu.is_open() {
                    self.context_menu.close();
                    return Task::none();
                }
                if self.sftp_drawer().plus.context_menu.open {
                    self.active_sftp_drawer_mut().plus.context_menu.close();
                    return Task::none();
                }
                if self.sftp_drawer().plus.modal.is_open() {
                    self.active_sftp_drawer_mut().plus.modal = SftpModalState::None;
                    return Task::none();
                }
                if self.settings_ui.is_open() {
                    self.close_settings_canceling_preview();
                    return Task::none();
                }
                if self.profile_editor.is_open() {
                    self.profile_editor.close();
                    return Task::none();
                }
                if self.tunnel_ui.pending_public_bind_warning().is_some() {
                    self.tunnel_ui.cancel_public_bind();
                    return Task::none();
                }
                if self.tunnel_ui.create_form().open {
                    self.tunnel_ui.create_form_mut().close();
                    return Task::none();
                }
                if self.command_palette.is_open() {
                    self.command_palette.close();
                    return Task::none();
                }
                if self.tabby_import.is_open() {
                    self.tabby_import.close();
                    return Task::none();
                }
                if self.zmodem_send_ui.session_id.is_some() {
                    self.zmodem_send_ui.close();
                    return Task::none();
                }
                if self.protocol_launcher.open {
                    self.protocol_launcher.open = false;
                    return Task::none();
                }
                if self.workspaces_ui.open {
                    self.workspaces_ui.close();
                    return Task::none();
                }
                if self.quick_commands_ui.open {
                    self.quick_commands_ui.close();
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
            Message::OpenLocalFile {
                session_id,
                path,
                line,
                col,
            } => {
                self.open_local_file(session_id, &path, line, col);
                Task::none()
            }
            Message::FileDropped {
                session_id,
                path,
                shift_held,
            } => {
                self.drag_drop.clear_external_file_hover();
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
                                self.rebuild_runtime_config_caches();
                                self.sync_terminal_options_to_sessions();
                                self.transfer_queue
                                    .set_transfer_config(self.config.transfer.clone());
                                if self.settings_ui.is_open() {
                                    self.settings_origin_config = Some(self.config.clone());
                                    self.settings_ui.sync_from_config(&self.config);
                                }
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
                            self.rebuild_runtime_config_caches();
                            self.sync_terminal_options_to_sessions();
                            self.transfer_queue
                                .set_transfer_config(self.config.transfer.clone());
                            if self.settings_ui.is_open() {
                                self.settings_origin_config = Some(self.config.clone());
                                self.settings_ui.sync_from_config(&self.config);
                            }
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
            Message::ClearExpiredReconnectNotices => {
                self.clear_expired_reconnect_notices(Instant::now());
                Task::none()
            }
            Message::NewTab => self.open_tab_in_window(window_scope, None),
            Message::SelectTab(tab_id) => {
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    ws.select_tab(tab_id);
                }
                self.sync_status_bar_drawer_open();
                Task::none()
            }
            Message::CycleTab(direction) => {
                if let Some(ws) = self.target_workspace_mut(window_scope) {
                    ws.cycle_tab(direction);
                }
                self.sync_status_bar_drawer_open();
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
                self.sync_status_bar_drawer_open();
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
                self.sync_status_bar_drawer_open();
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
                self.sync_status_bar_drawer_open();
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
                self.sync_status_bar_drawer_open();
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
                self.sync_status_bar_drawer_open();
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
                self.sync_status_bar_drawer_open();
                Task::none()
            }
            Message::MoveFocus(direction) => {
                if let Some(ws) = self.target_workspace_mut(window_scope)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.move_focus(tab_id, direction);
                }
                self.sync_status_bar_drawer_open();
                Task::none()
            }
            Message::CycleFocus(direction) => {
                if let Some(ws) = self.target_workspace_mut(window_scope)
                    && let Some(tab_id) = ws.active_tab_id()
                {
                    ws.cycle_focus(tab_id, direction);
                }
                self.sync_status_bar_drawer_open();
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
                match SshTargetSpec::from_profile_with_sources(
                    &profile,
                    &self.config,
                    &self.profiles.profiles,
                    &self.openssh_hosts,
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
                match SshTargetSpec::from_profile_with_sources(
                    &profile,
                    &self.config,
                    &self.profiles.profiles,
                    &self.openssh_hosts,
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
            Message::ServerStatsUpdated {
                session_id,
                generation,
                stats,
            } => {
                if let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id)
                    && slot.generation == generation
                {
                    slot.server_stats = Some(stats);
                    slot.server_stats_unsupported = false;
                }
                Task::none()
            }
            Message::ServerStatsUnavailable {
                session_id,
                generation,
            } => {
                if let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id)
                    && slot.generation == generation
                {
                    slot.server_stats_unsupported = true;
                    slot.server_stats = None;
                }
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
            Message::SetSidebarFilterScope(scope) => {
                self.sidebar.set_filter_scope(scope);
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
            Message::ToggleSftpDrawerForSession(session_id) => {
                self.toggle_sftp_drawer_for_session(session_id)
            }
            Message::SetSftpDrawerOpen(open) => self.set_sftp_drawer_open(open),
            Message::ResizeSftpDrawer(width) => {
                self.set_sftp_drawer_width(width);
                Task::none()
            }
            msg @ (Message::SftpSetLayoutMode(_)
            | Message::SftpToggleTransfersPanel
            | Message::SftpSetActivePane(_)
            | Message::SftpShortcut(_)
            | Message::SftpPanePathInputChanged(_, _)
            | Message::SftpPaneNavigateSubmitted(_)
            | Message::SftpPaneNavigateTo { .. }
            | Message::SftpPaneBack(_)
            | Message::SftpPaneForward(_)
            | Message::SftpPaneUp(_)
            | Message::SftpPaneRefresh(_)
            | Message::SftpPaneHome(_)
            | Message::SftpPaneToggleHidden(_)
            | Message::SftpPaneToggleFilter(_)
            | Message::SftpPaneFilterChanged(_, _)
            | Message::SftpPaneToggleBookmarks(_)
            | Message::SftpPaneDismissBookmarks(_)
            | Message::SftpPaneToggleBookmarkCurrent(_)
            | Message::SftpPaneSelectBookmark { .. }
            | Message::SftpPaneRemoveBookmark { .. }
            | Message::SftpPaneSortBy(_, _)
            | Message::SftpPaneRowClicked { .. }
            | Message::SftpOpenContextMenu { .. }
            | Message::SftpDismissContextMenu
            | Message::SftpContextAction(_)
            | Message::SftpDismissModal
            | Message::SftpModalNameChanged(_)
            | Message::SftpModalConfirmCreateOrRename
            | Message::SftpModalOctalChanged(_)
            | Message::SftpModalPermBitToggled { .. }
            | Message::SftpModalConfirmPermissions
            | Message::SftpModalEditorAction(_)
            | Message::SftpModalSaveFile
            | Message::SftpRemoteFileReadResult { .. }
            | Message::SftpRemoteFileSaveResult { .. }) => self.handle_sftp_plus_message(msg),
            Message::SftpPathInputChanged(path) => {
                self.active_sftp_drawer_mut().set_path_input(path);
                Task::none()
            }
            Message::SftpNavigateTo(path) => self.navigate_sftp_to(path),
            Message::SftpNavigateUp => {
                let parent = parent_remote_path(self.sftp_drawer().current_path());
                self.navigate_sftp_to(parent)
            }
            Message::SftpRefresh => {
                let path = self.sftp_drawer().current_path().to_string();
                self.navigate_sftp_to(path)
            }
            Message::SftpNewFolderInputChanged(name) => {
                self.active_sftp_drawer_mut().set_new_folder_name(name);
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
            Message::SftpDownloadDir {
                remote_path,
                dir_name,
            } => self.enqueue_sftp_download_dir(remote_path, dir_name),
            Message::SftpDrawerFileDropped { path } => {
                self.drag_drop.clear_external_file_hover();
                if let Some(session_id) = self.active_ssh_session_id() {
                    let action =
                        classify_file_drop(session_id, &path, true, false, true);
                    if let FileDropAction::UploadToSftp { session_id, path } = action {
                        self.enqueue_sftp_upload(session_id, path);
                    }
                } else {
                    self.active_sftp_drawer_mut().set_browser_error(Some(
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
                if let Err(err) = &result
                    && is_sftp_connection_error_message(err)
                    && self
                        .sftp_drawers
                        .get(&session_id)
                        .is_some_and(|d| d.is_current_listing(session_id, generation))
                {
                    return self.trigger_sftp_auto_reconnect(session_id, err.clone());
                }
                let resolved_path = self.sftp_drawers.get_mut(&session_id).and_then(|drawer| {
                    drawer.apply_browse_result_tracked(session_id, generation, result)
                });
                if let Some(path) = resolved_path {
                    self.record_profile_sftp_dir(session_id, &path);
                }
                self.sync_sftp_status_notice();
                Task::none()
            }
            Message::SftpMutationResult { session_id, result } => match result {
                Ok(()) => {
                    let path = if let Some(drawer) = self.sftp_drawers.get_mut(&session_id) {
                        drawer.mark_connected_or_recovered(Instant::now());
                        drawer.current_path().to_string()
                    } else {
                        ".".to_string()
                    };
                    self.sync_sftp_status_notice();
                    self.sftp_refresh_task(session_id, path)
                }
                Err(err) => {
                    if is_sftp_connection_error_message(&err) {
                        return self.trigger_sftp_auto_reconnect(session_id, err);
                    }
                    if let Some(drawer) = self.sftp_drawers.get_mut(&session_id) {
                        drawer.set_browser_error(Some(err));
                    }
                    Task::none()
                }
            },
            Message::SftpManagerStatusUpdated(entries) => {
                let now = Instant::now();
                let matching_sessions: Vec<(SessionId, SftpTransportStatus)> = self
                    .sessions
                    .iter()
                    .filter_map(|(&sid, entry)| {
                        let slot = entry.as_ssh()?;
                        let key = slot.sftp_key();
                        entries
                            .iter()
                            .find(|(k, _)| *k == key)
                            .map(|(_, st)| (sid, st.clone()))
                    })
                    .collect();
                for (sid, status) in matching_sessions {
                    if let Some(drawer) = self.sftp_drawers.get_mut(&sid) {
                        match status {
                            SftpTransportStatus::AutoReconnecting { attempt, reason } => {
                                drawer.update_auto_reconnecting_progress(attempt, None, reason);
                            }
                            SftpTransportStatus::Reconnected { .. } => {
                                drawer.mark_reconnected_now(now);
                            }
                            SftpTransportStatus::Connected { .. } => {
                                drawer.mark_connected_or_recovered(now);
                            }
                            SftpTransportStatus::Idle | SftpTransportStatus::Error { .. } => {}
                        }
                    }
                }
                self.sync_sftp_status_notice();
                Task::none()
            }
            Message::SftpAutoReconnectTick {
                session_id,
                retry_generation,
            } => {
                let Some(drawer) = self.sftp_drawers.get(&session_id) else {
                    return Task::none();
                };
                if drawer.retry_generation() != retry_generation || !drawer.is_auto_reconnecting() {
                    return Task::none();
                }
                let path = drawer.current_path().to_string();
                let ssh_active = self
                    .sessions
                    .get(&session_id)
                    .and_then(SessionEntry::as_ssh)
                    .is_some_and(|slot| slot.active.is_some());
                let ssh_disconnected = self
                    .sessions
                    .get(&session_id)
                    .and_then(SessionEntry::as_ssh)
                    .is_some_and(|slot| {
                        slot.transport_status == TransportStatus::Disconnected
                            && !slot.is_auto_reconnecting
                    });
                if ssh_active {
                    return self.sftp_refresh_task(session_id, path);
                } else if ssh_disconnected && let Some(drawer) = self.sftp_drawers.get_mut(&session_id)
                {
                    drawer.set_connection_state(SftpConnectionState::Disconnected {
                        reason: "SSH connection closed".to_string(),
                    });
                    self.sync_sftp_status_notice();
                }
                Task::none()
            }
            Message::SftpDirEnqueueResult { result } => {
                self.handle_dir_enqueue_result(None, result);
                Task::none()
            }
            Message::SftpSessionDirEnqueueResult { session_id, result } => {
                self.handle_dir_enqueue_result(Some(session_id), result);
                Task::none()
            }
            Message::TransferQueueUpdated(snap) => {
                let prev_done = self.sftp_drawer().queue_snapshot().completed_count;
                self.sync_transfer_telemetry_from(snap);
                let new_done = self.sftp_drawer().queue_snapshot().completed_count;
                if new_done > prev_done && self.sftp_drawer().is_open() {
                    return self.refresh_open_sftp_after_transfer();
                }
                Task::none()
            }
            Message::TransferConflictRenameInputChanged(input) => {
                self.conflict_drawer_mut().set_conflict_rename_input(input);
                Task::none()
            }
            Message::TransferConflictApplyToAllToggled(apply_to_all) => {
                self.conflict_drawer_mut()
                    .set_conflict_apply_to_all(apply_to_all);
                Task::none()
            }
            Message::ResolveTransferConflict {
                transfer_id,
                resolution,
                apply_to_all,
            } => {
                let _ = self
                    .transfer_queue
                    .resolve_conflict(transfer_id, resolution, apply_to_all);
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
                self.clear_finished_transfers_for_active_drawer();
                Task::none()
            }
            Message::OpenTunnelCreateModal => {
                self.command_palette.close();
                let default_session = self
                    .active_session_id()
                    .filter(|sid| self.sessions.get(sid).and_then(SessionEntry::as_ssh).is_some());
                let default_profile = match default_session {
                    Some(sid) => self
                        .sessions
                        .get(&sid)
                        .and_then(SessionEntry::as_ssh)
                        .and_then(|slot| slot.target.profile_id.clone()),
                    None => self
                        .profiles
                        .profiles
                        .iter()
                        .find(|p| matches!(p.target, ProfileTarget::Ssh { .. }))
                        .map(|p| p.id.clone()),
                };
                let effective_session = if default_profile.is_some() {
                    default_session
                } else {
                    default_session.or_else(|| {
                        self.sessions.iter().find_map(|(&sid, entry)| {
                            entry
                                .as_ssh()
                                .filter(|slot| slot.target.profile_id.is_none())
                                .map(|_| sid)
                        })
                    })
                };
                self.tunnel_ui
                    .create_form_mut()
                    .open_for(default_profile, effective_session);
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
                form.session_id = None;
                form.validation_error = None;
                Task::none()
            }
            Message::TunnelFormSessionSelected(sid) => {
                let form = self.tunnel_ui.create_form_mut();
                form.profile_id = None;
                form.session_id = Some(sid);
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
            Message::OpenEditTunnelModal(tunnel_id) => self.open_edit_tunnel_modal(&tunnel_id),
            Message::DeleteTunnel(tunnel_id) => {
                self.delete_tunnel(&tunnel_id);
                Task::none()
            }
            Message::OpenTunnelContextMenu {
                tunnel_id,
                position,
            } => {
                self.open_tunnel_context_menu(&tunnel_id, position);
                Task::none()
            }
            Message::ReconnectTunnel(tunnel_id) => self.reconnect_tunnel(&tunnel_id),
            Message::ReconnectAllTunnels => self.reconnect_all_tunnels(),
            Message::TunnelManagerUpdated(snap) => {
                self.sync_tunnel_telemetry_from(snap);
                Task::none()
            }
            Message::OpenSettingsModal => {
                self.command_palette.close();
                self.profile_editor.close();
                if self.settings_origin_config.is_none() {
                    self.settings_origin_config = Some(self.config.clone());
                }
                self.settings_ui.open_with(&self.config);
                Task::none()
            }
            Message::CloseSettingsModal => {
                self.close_settings_canceling_preview();
                Task::none()
            }
            Message::SettingsSelectTab(tab) => {
                self.settings_ui.set_tab(tab);
                Task::none()
            }
            Message::SettingsThemeSelected(theme) => {
                self.settings_ui.set_theme(theme);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsFontFamilyChanged(family) => {
                self.settings_ui.set_font_family(family);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsFontSizeChanged(value) => {
                self.settings_ui.set_font_size_text(value);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsStepFontSize(delta) => {
                let _ = self.settings_ui.step_font_size(delta);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsLigaturesToggled(enabled) => {
                self.settings_ui.set_font_ligatures(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsCursorStyleSelected(style) => {
                self.settings_ui.set_cursor_style(style);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsShellChanged(value) => {
                self.settings_ui.set_shell_text(value);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsScrollbackChanged(value) => {
                self.settings_ui.set_scrollback_text(value);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsCopyOnSelectToggled(enabled) => {
                self.settings_ui.set_copy_on_select(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsConfirmMultilinePasteToggled(enabled) => {
                self.settings_ui.set_confirm_multiline_paste(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsKeepaliveIntervalChanged(value) => {
                self.settings_ui.set_keepalive_interval_text(value);
                Task::none()
            }
            Message::SettingsKeepaliveMissedChanged(value) => {
                self.settings_ui.set_keepalive_missed_text(value);
                Task::none()
            }
            Message::SettingsConflictPolicySelected(policy) => {
                self.settings_ui.set_conflict_policy(policy);
                Task::none()
            }
            Message::SettingsFollowSymlinksToggled(enabled) => {
                self.settings_ui.set_follow_symlinks(enabled);
                Task::none()
            }
            Message::SettingsVaultBackendSelected(backend) => {
                self.settings_ui.set_vault_backend(backend);
                Task::none()
            }
            Message::SettingsAutoLockChanged(value) => {
                self.settings_ui.set_auto_lock_text(value);
                Task::none()
            }
            Message::SettingsSave => self.apply_settings_save(),
            Message::OpenNewProfileModal => {
                self.command_palette.close();
                self.close_settings_canceling_preview();
                self.profile_editor.open_new(default_os_username());
                self.populate_profile_editor_jump_candidates(None);
                Task::none()
            }
            Message::OpenEditProfileModal(profile_id) => {
                self.command_palette.close();
                self.close_settings_canceling_preview();
                if let Some(profile) = self
                    .profiles
                    .profiles
                    .iter()
                    .find(|p| p.id == profile_id)
                    .cloned()
                {
                    self.profile_editor.open_edit(&profile);
                    self.populate_profile_editor_jump_candidates(Some(&profile.id));
                }
                Task::none()
            }
            Message::CloseProfileEditor => {
                self.profile_editor.close();
                Task::none()
            }
            Message::ProfileFormTabSelected(tab) => {
                self.profile_editor.active_tab = tab;
                Task::none()
            }
            Message::ProfileFormKindChanged(kind) => {
                self.profile_editor.kind = kind;
                self.profile_editor.active_tab = ProfileEditorTab::General;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormNameChanged(val) => {
                self.profile_editor.name = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormGroupChanged(val) => {
                self.profile_editor.group = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormTagsChanged(val) => {
                self.profile_editor.tags = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormLocalShellChanged(val) => {
                self.profile_editor.local_shell = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormLocalDirChanged(val) => {
                self.profile_editor.local_working_dir = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormHostChanged(val) => {
                self.profile_editor.ssh_host = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormPortChanged(val) => {
                self.profile_editor.ssh_port = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormUsernameChanged(val) => {
                self.profile_editor.ssh_username = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormAuthChanged(auth) => {
                self.profile_editor.auth_choice = auth;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormKeyPathChanged(val) => {
                self.profile_editor.key_path = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormSecretChanged(val) => {
                self.profile_editor.secret_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormForgetSecret => {
                self.profile_editor.existing_secret_ref = None;
                self.profile_editor.secret_draft.clear();
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormToggleConnectionMenu => {
                self.profile_editor.connection_menu_open =
                    !self.profile_editor.connection_menu_open;
                Task::none()
            }
            Message::ProfileFormCloseConnectionMenu => {
                self.profile_editor.connection_menu_open = false;
                Task::none()
            }
            Message::ProfileFormConnectionModeSelected(mode) => {
                self.profile_editor.connection_mode = mode;
                self.profile_editor.connection_menu_open = false;
                if mode == SshConnectionMode::JumpHost {
                    self.profile_editor.jump_panel_open = true;
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormProxyHostChanged(val) => {
                if let Some(proxy) = self.profile_editor.active_proxy_draft_mut() {
                    proxy.host = val;
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormProxyPortChanged(val) => {
                if let Some(proxy) = self.profile_editor.active_proxy_draft_mut() {
                    proxy.port = val;
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormProxyUsernameChanged(val) => {
                if let Some(proxy) = self.profile_editor.active_proxy_draft_mut() {
                    proxy.username = val;
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormProxySecretChanged(val) => {
                if let Some(proxy) = self.profile_editor.active_proxy_draft_mut() {
                    proxy.secret_draft = val;
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormForgetProxySecret => {
                if let Some(proxy) = self.profile_editor.active_proxy_draft_mut() {
                    proxy.existing_secret_ref = None;
                    proxy.secret_draft.clear();
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormToggleJumpPanel => {
                self.profile_editor.jump_panel_open = !self.profile_editor.jump_panel_open;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormProxyJumpChanged(val) => {
                self.profile_editor.proxy_jump = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormJumpDropdownSelected(token) => {
                if token.trim().is_empty() {
                    self.profile_editor.clear_jump_hops();
                } else {
                    let current = self.profile_editor.jump_hops();
                    if !(current.len() == 1 && current[0].eq_ignore_ascii_case(token.trim()))
                        && let Err(err) = self.profile_editor.add_jump_hop(&token)
                    {
                        self.profile_editor.validation_error = Some(err);
                    }
                }
                Task::none()
            }
            Message::ProfileFormToggleCustomJumpInput => {
                self.profile_editor.jump_custom_open = !self.profile_editor.jump_custom_open;
                if self.profile_editor.jump_custom_open {
                    self.profile_editor.jump_panel_open = true;
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormCustomJumpDraftChanged(val) => {
                self.profile_editor.jump_custom_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormAddCustomJumpHop => {
                if let Err(err) = self.profile_editor.add_custom_jump_hop_from_draft() {
                    self.profile_editor.validation_error = Some(err);
                }
                Task::none()
            }
            Message::ProfileFormRemoveJumpHop(idx) => {
                self.profile_editor.remove_jump_hop(idx);
                Task::none()
            }
            Message::ProfileFormMoveJumpHop { index, delta } => {
                self.profile_editor.move_jump_hop(index, delta);
                Task::none()
            }
            Message::ProfileFormClearJumpHops => {
                self.profile_editor.clear_jump_hops();
                Task::none()
            }
            Message::ProfileFormAutoConnectToggled(val) => {
                self.profile_editor.auto_connect_on_startup = val;
                Task::none()
            }
            Message::ProfileFormAutoMuxToggled(val) => {
                self.profile_editor.auto_attach_mux = val;
                Task::none()
            }
            Message::ProfileFormMuxCommandChanged(val) => {
                self.profile_editor.mux_attach_command = val;
                Task::none()
            }
            Message::ProfileSave => {
                self.apply_profile_save();
                Task::none()
            }
            Message::ProfileDelete(profile_id) => {
                self.apply_profile_delete(&profile_id);
                Task::none()
            }
            Message::OpenTerminalContextMenu {
                session_id,
                position,
            } => {
                let has_selection = self
                    .sessions
                    .get(&session_id)
                    .and_then(SessionEntry::buffer)
                    .and_then(|b| b.selection_text())
                    .is_some();
                let logging_active = self.is_session_logging(session_id);
                let is_ssh = self
                    .sessions
                    .get(&session_id)
                    .and_then(SessionEntry::as_ssh)
                    .is_some();
                self.context_menu.open_terminal(
                    session_id,
                    position,
                    has_selection,
                    logging_active,
                    is_ssh,
                );
                Task::none()
            }
            Message::OpenTabContextMenu { tab_id, position } => {
                if let Some(ws) = self.workspace_for_tab(window_scope, tab_id)
                    && let Some(tab) = ws.tab(tab_id)
                {
                    let tab_idx = ws
                        .tabs()
                        .iter()
                        .position(|t| t.id == tab_id)
                        .unwrap_or(0);
                    let can_close_others = ws.tabs().iter().any(|t| t.id != tab_id && !t.pinned);
                    let can_close_right = ws.tabs().iter().skip(tab_idx + 1).any(|t| !t.pinned);
                    self.context_menu.open_tab(
                        tab_id,
                        position,
                        tab.pinned,
                        can_close_others,
                        can_close_right,
                        tab.color.clone(),
                    );
                }
                Task::none()
            }
            Message::OpenSidebarContextMenu {
                group_name,
                profile_id,
                position,
            } => {
                let profile_name = profile_id.as_deref().and_then(|pid| {
                    self.profiles
                        .profiles
                        .iter()
                        .find(|p| p.id == pid)
                        .map(|p| p.name.clone())
                });
                let available_groups = self.profiles.all_groups();
                self.context_menu.open_sidebar(
                    position,
                    group_name,
                    profile_id,
                    profile_name,
                    available_groups,
                );
                Task::none()
            }
            Message::OpenNewProfileInGroup(group) => {
                self.context_menu.close();
                self.command_palette.close();
                self.close_settings_canceling_preview();
                let clean_group = group
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty() && !g.eq_ignore_ascii_case(UNGROUPED_LABEL));
                self.profile_editor
                    .open_new_in_group(default_os_username(), clean_group);
                self.populate_profile_editor_jump_candidates(None);
                Task::none()
            }
            Message::BeginCreateGroup { assign_profile_id } => {
                self.context_menu.start_create_group(assign_profile_id);
                operation::focus(context_prompt_input_id())
            }
            Message::CreateGroupDraftChanged(val) => {
                if let Some((draft, _)) = &mut self.context_menu.creating_group {
                    *draft = val;
                }
                Task::none()
            }
            Message::SubmitCreateGroup => {
                if let Some((draft, assign_profile_id)) = self.context_menu.creating_group.take() {
                    let clean = draft.trim().to_string();
                    if !clean.is_empty() && !clean.eq_ignore_ascii_case(UNGROUPED_LABEL) {
                        let changed = if let Some(pid) = assign_profile_id {
                            self.mutate_profiles_and_save(|cfg| {
                                let added = cfg.add_group(&clean);
                                let moved = cfg.move_profile_to_group(&pid, Some(&clean));
                                added || moved
                            })
                        } else {
                            self.mutate_profiles_and_save(|cfg| cfg.add_group(&clean))
                        };
                        if changed {
                            self.sidebar.set_open(true);
                            self.sidebar.set_group_collapsed(&clean, false);
                        }
                    }
                }
                Task::none()
            }
            Message::BeginRenameGroup(old_name) => {
                self.context_menu.start_rename_group(old_name);
                operation::focus(context_prompt_input_id())
            }
            Message::RenameGroupDraftChanged(val) => {
                if let Some((_, draft)) = &mut self.context_menu.renaming_group {
                    *draft = val;
                }
                Task::none()
            }
            Message::SubmitRenameGroup => {
                if let Some((old_name, draft)) = self.context_menu.renaming_group.take() {
                    let clean = draft.trim().to_string();
                    let was_collapsed = self.sidebar.is_group_collapsed(&old_name);
                    if self.mutate_profiles_and_save(|cfg| cfg.rename_group(&old_name, &clean)) {
                        self.sidebar.set_group_collapsed(&clean, was_collapsed);
                    }
                }
                Task::none()
            }
            Message::DeleteProfileGroup(group_name) => {
                self.context_menu.close();
                let _ = self.mutate_profiles_and_save(|cfg| cfg.delete_group(&group_name));
                Task::none()
            }
            Message::MoveProfileToGroup { profile_id, group } => {
                self.context_menu.close();
                if self.mutate_profiles_and_save(|cfg| {
                    cfg.move_profile_to_group(&profile_id, group.as_deref())
                }) && let Some(g) = group
                {
                    let clean = g.trim();
                    if !clean.is_empty() && !clean.eq_ignore_ascii_case(UNGROUPED_LABEL) {
                        self.sidebar.set_group_collapsed(clean, false);
                    }
                }
                Task::none()
            }
            Message::DeleteSavedProfileById(profile_id) => {
                self.context_menu.close();
                self.apply_profile_delete(&profile_id);
                Task::none()
            }
            Message::DragStart { payload, position } => {
                self.context_menu.close();
                self.drag_drop.begin(payload, position);
                Task::none()
            }
            Message::DragCursorMoved(position) => {
                self.drag_drop.update_cursor(position);
                Task::none()
            }
            Message::DragHoverChanged(target) => {
                self.drag_drop.set_hovered(Some(target));
                Task::none()
            }
            Message::DragLeaveTarget(target) => {
                self.drag_drop.clear_hovered_if_matches(&target);
                Task::none()
            }
            Message::DragFinish => self.handle_drag_finish(window_scope),
            Message::DragCancel => {
                self.drag_drop.cancel();
                Task::none()
            }
            Message::ExternalFileHovered(path) => {
                self.drag_drop.set_external_file_hovered(path);
                Task::none()
            }
            Message::ExternalFilesLeft => {
                self.drag_drop.clear_external_file_hover();
                Task::none()
            }
            Message::DismissContextMenu => {
                self.context_menu.close();
                Task::none()
            }
            Message::BeginTabRename(tab_id) => {
                self.context_menu.close();
                let current_title = self
                    .workspace_for_tab(window_scope, tab_id)
                    .and_then(|ws| ws.tab(tab_id))
                    .and_then(|t| t.title.clone())
                    .unwrap_or_default();
                self.context_menu.start_rename(tab_id, current_title);
                operation::focus(context_prompt_input_id())
            }
            Message::TabRenameDraftChanged(val) => {
                if let Some((_, draft)) = &mut self.context_menu.renaming_tab {
                    *draft = val;
                }
                Task::none()
            }
            Message::SubmitTabRename => {
                if let Some((tab_id, draft)) = self.context_menu.renaming_tab.take()
                    && let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id)
                {
                    let trimmed = draft.trim();
                    let next = (!trimmed.is_empty()).then(|| trimmed.to_string());
                    ws.rename_tab(tab_id, next);
                }
                Task::none()
            }
            Message::SetTabColor { tab_id, color } => {
                self.context_menu.close();
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    ws.set_tab_color(tab_id, color);
                }
                Task::none()
            }
            Message::TogglePinTab(tab_id) => {
                self.context_menu.close();
                if let Some(ws) = self.workspace_for_tab_mut(window_scope, tab_id) {
                    ws.toggle_pin_tab(tab_id);
                }
                Task::none()
            }
            Message::DuplicateTab(tab_id) => {
                self.context_menu.close();
                self.duplicate_tab(window_scope, tab_id)
            }
            Message::CloseOtherTabs(tab_id) => {
                self.context_menu.close();
                if let Some(closed) = self
                    .workspace_for_tab_mut(window_scope, tab_id)
                    .map(|ws| ws.close_other_tabs(tab_id))
                {
                    for sid in closed {
                        self.remove_session(sid);
                    }
                }
                Task::none()
            }
            Message::CloseTabsToRight(tab_id) => {
                self.context_menu.close();
                if let Some(closed) = self
                    .workspace_for_tab_mut(window_scope, tab_id)
                    .map(|ws| ws.close_tabs_to_right(tab_id))
                {
                    for sid in closed {
                        self.remove_session(sid);
                    }
                }
                Task::none()
            }
            Message::ClearTerminalBuffer(session_id) => {
                self.context_menu.close();
                self.clear_terminal_buffer(session_id);
                Task::none()
            }
            Message::SaveScrollback(session_id) => {
                self.context_menu.close();
                let logs_dir = self
                    .config
                    .terminal
                    .log_directory
                    .clone()
                    .unwrap_or_else(|| self.paths.logs_dir());
                let title = self
                    .sessions
                    .get(&session_id)
                    .map(SessionEntry::title)
                    .unwrap_or("session");
                let clean: String = title
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let out_path = logs_dir.join(format!("scrollback-{clean}-{ts}.txt"));
                if let Err(err) = self.save_scrollback_to_path(session_id, &out_path) {
                    self.last_error = Some(err);
                }
                Task::none()
            }
            Message::ToggleSessionLogging(session_id) => {
                self.context_menu.close();
                if let Err(err) = self.toggle_session_logging(session_id) {
                    self.last_error = Some(err);
                }
                Task::none()
            }
            Message::AdjustFontSize(delta) => {
                let next = (self.config.terminal.font_size + delta).clamp(8.0, 36.0);
                self.config.terminal.font_size = next;
                if self.settings_ui.is_open() {
                    self.settings_ui.sync_from_config(&self.config);
                }
                self.sync_terminal_options_to_sessions();
                Task::none()
            }
            Message::ResetFontSize => {
                self.config.terminal.font_size = TerminalConfig::default().font_size;
                if self.settings_ui.is_open() {
                    self.settings_ui.sync_from_config(&self.config);
                }
                self.sync_terminal_options_to_sessions();
                Task::none()
            }
            Message::SelectTabByIndex(idx) => {
                if let Some(ws) = self.target_workspace_mut(window_scope) {
                    let target = if idx == 8 {
                        ws.tabs().get(idx).or_else(|| ws.tabs().last()).map(|t| t.id)
                    } else {
                        ws.tabs().get(idx).map(|t| t.id)
                    };
                    if let Some(tid) = target {
                        ws.select_tab(tid);
                    }
                }
                self.sync_status_bar_drawer_open();
                Task::none()
            }
            Message::PushPendingKeyChord(chord) => {
                self.pending_key_chords.push(chord);
                Task::none()
            }
            Message::ClearPendingKeyChord => {
                self.pending_key_chords.clear();
                Task::none()
            }
            Message::CursorBlinkTick => {
                if self.config.terminal.cursor_blink {
                    self.cursor_visible = !self.cursor_visible;
                } else {
                    self.cursor_visible = true;
                }
                Task::none()
            }
            Message::ClearVisualBell {
                session_id,
                generation,
            } => {
                if self.visual_bell_generation.get(&session_id).copied() == Some(generation) {
                    self.visual_bell_active.remove(&session_id);
                }
                Task::none()
            }
            Message::SettingsOpacityChanged(value) => {
                self.settings_ui.set_opacity_text(value);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsStepOpacity(delta) => {
                let _ = self.settings_ui.step_opacity(delta);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsCursorBlinkToggled(enabled) => {
                self.settings_ui.set_cursor_blink(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsTabBarPositionSelected(pos) => {
                self.settings_ui.set_tab_bar_position(pos);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsShowProfileSidebarToggled(enabled) => {
                self.settings_ui.set_show_profile_sidebar(enabled);
                self.sidebar.set_open(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsShowStatusBarToggled(enabled) => {
                self.settings_ui.set_show_status_bar(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsShowServerStatsToggled(enabled) => {
                self.settings_ui.set_show_server_stats(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsHideTabIndexToggled(enabled) => {
                self.settings_ui.set_hide_tab_index(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsFlexTabsToggled(enabled) => {
                self.settings_ui.set_flex_tabs(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsThemeOverrideChanged { field, value } => {
                self.settings_ui.set_theme_override(field, value);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsRightClickActionSelected(action) => {
                self.settings_ui.set_right_click_action(action);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsMiddleClickPasteToggled(enabled) => {
                self.settings_ui.set_middle_click_paste(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsBellBehaviorSelected(behavior) => {
                self.settings_ui.set_bell_behavior(behavior);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsAltIsMetaToggled(enabled) => {
                self.settings_ui.set_alt_is_meta(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsWordSeparatorsChanged(val) => {
                self.settings_ui.set_word_separators(val);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsAutoLogToggled(enabled) => {
                self.settings_ui.set_auto_log_sessions(enabled);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsLogDirectoryChanged(val) => {
                self.settings_ui.set_log_directory_text(val);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsKeybindingFilterChanged(filter) => {
                self.settings_ui.set_keybinding_filter(filter);
                Task::none()
            }
            Message::SettingsKeybindingChanged { action, value } => {
                self.settings_ui.set_keybinding_text(action, value);
                self.config.keybindings = self.settings_ui.draft().keybindings.clone();
                self.rebuild_runtime_config_caches();
                Task::none()
            }
            Message::SettingsBeginShortcutCapture(action) => {
                self.settings_ui.begin_shortcut_capture(action);
                Task::none()
            }
            Message::SettingsShortcutCaptured(chord) => {
                if self.settings_ui.apply_captured_shortcut(chord) {
                    self.config.keybindings = self.settings_ui.draft().keybindings.clone();
                    self.rebuild_runtime_config_caches();
                }
                Task::none()
            }
            Message::SettingsCancelShortcutCapture => {
                self.settings_ui.cancel_shortcut_capture();
                Task::none()
            }
            Message::SettingsResetKeybinding(action) => {
                self.settings_ui.reset_keybinding(action);
                self.config.keybindings = self.settings_ui.draft().keybindings.clone();
                self.rebuild_runtime_config_caches();
                Task::none()
            }
            Message::SettingsResetAllKeybindings => {
                self.settings_ui.reset_all_keybindings();
                self.config.keybindings = self.settings_ui.draft().keybindings.clone();
                self.rebuild_runtime_config_caches();
                Task::none()
            }
            Message::SettingsHighlightNameChanged(val) => {
                self.settings_ui.set_highlight_name(val);
                Task::none()
            }
            Message::SettingsHighlightPatternChanged(val) => {
                self.settings_ui.set_highlight_pattern(val);
                Task::none()
            }
            Message::SettingsHighlightFgChanged(val) => {
                self.settings_ui.set_highlight_fg(val);
                Task::none()
            }
            Message::SettingsHighlightBgChanged(val) => {
                self.settings_ui.set_highlight_bg(val);
                Task::none()
            }
            Message::SettingsHighlightBoldToggled(val) => {
                self.settings_ui.set_highlight_bold(val);
                Task::none()
            }
            Message::SettingsHighlightCaseToggled(val) => {
                self.settings_ui.set_highlight_case(val);
                Task::none()
            }
            Message::SettingsAddHighlightRule => {
                let _ = self.settings_ui.add_highlight_rule();
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsToggleHighlightRule(id) => {
                self.settings_ui.toggle_highlight_rule(&id);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::SettingsDeleteHighlightRule(id) => {
                self.settings_ui.delete_highlight_rule(&id);
                self.apply_live_settings_preview();
                Task::none()
            }
            Message::OpenQuickCommandsModal => {
                self.command_palette.close();
                self.workspaces_ui.close();
                self.protocol_launcher.open = false;
                self.quick_commands_ui.open_fresh();
                Task::none()
            }
            Message::CloseQuickCommandsModal => {
                self.quick_commands_ui.close();
                Task::none()
            }
            Message::QuickCommandFilterChanged(val) => {
                self.quick_commands_ui.filter = val;
                Task::none()
            }
            Message::QuickCommandNameChanged(val) => {
                self.quick_commands_ui.draft_name = val;
                self.quick_commands_ui.error = None;
                Task::none()
            }
            Message::QuickCommandGroupChanged(val) => {
                self.quick_commands_ui.draft_group = val;
                Task::none()
            }
            Message::QuickCommandBodyChanged(val) => {
                self.quick_commands_ui.draft_command = val;
                self.quick_commands_ui.error = None;
                Task::none()
            }
            Message::QuickCommandAppendNewlineToggled(enabled) => {
                self.quick_commands_ui.draft_append_newline = enabled;
                Task::none()
            }
            Message::QuickCommandBroadcastToggled(enabled) => {
                self.quick_commands_ui.draft_broadcast = enabled;
                Task::none()
            }
            Message::QuickCommandConfirmToggled(enabled) => {
                self.quick_commands_ui.draft_confirm = enabled;
                Task::none()
            }
            Message::QuickCommandSave => {
                match self.quick_commands_ui.build_command() {
                    Ok(cmd) => {
                        if let Some(pos) = self
                            .quick_commands
                            .commands
                            .iter()
                            .position(|c| c.id == cmd.id)
                        {
                            self.quick_commands.commands[pos] = cmd;
                        } else {
                            self.quick_commands.commands.push(cmd);
                        }
                        if let Err(err) = save_quick_commands(&self.paths, &self.quick_commands) {
                            self.quick_commands_ui.error = Some(err.to_string());
                        } else {
                            self.quick_commands_ui.clear_draft();
                            self.last_error = None;
                        }
                    }
                    Err(err) => {
                        self.quick_commands_ui.error = Some(err);
                    }
                }
                Task::none()
            }
            Message::QuickCommandEdit(id) => {
                if let Some(cmd) = self
                    .quick_commands
                    .commands
                    .iter()
                    .find(|c| c.id == id)
                    .cloned()
                {
                    self.quick_commands_ui.load_for_edit(&cmd);
                }
                Task::none()
            }
            Message::QuickCommandCancelEdit => {
                self.quick_commands_ui.clear_draft();
                Task::none()
            }
            Message::QuickCommandDelete(id) => {
                self.quick_commands.commands.retain(|c| c.id != id);
                if self.quick_commands_ui.editing_id.as_deref() == Some(id.as_str()) {
                    self.quick_commands_ui.clear_draft();
                }
                if self.quick_commands_ui.confirm_pending_id.as_deref() == Some(id.as_str()) {
                    self.quick_commands_ui.confirm_pending_id = None;
                }
                if let Err(err) = save_quick_commands(&self.paths, &self.quick_commands) {
                    self.quick_commands_ui.error = Some(err.to_string());
                }
                Task::none()
            }
            Message::ExecuteQuickCommand {
                id,
                broadcast_override,
            } => {
                if let Some(cmd) = self
                    .quick_commands
                    .commands
                    .iter()
                    .find(|c| c.id == id)
                    .cloned()
                {
                    if cmd.confirm_before_run {
                        self.quick_commands_ui.open_fresh();
                        self.quick_commands_ui.confirm_pending_id = Some(id);
                    } else {
                        self.run_quick_command_now(window_scope, &cmd, broadcast_override);
                    }
                }
                Task::none()
            }
            Message::ConfirmExecuteQuickCommand(id) => {
                self.quick_commands_ui.confirm_pending_id = None;
                if let Some(cmd) = self
                    .quick_commands
                    .commands
                    .iter()
                    .find(|c| c.id == id)
                    .cloned()
                {
                    self.run_quick_command_now(window_scope, &cmd, None);
                }
                Task::none()
            }
            Message::CancelQuickCommandConfirm => {
                self.quick_commands_ui.confirm_pending_id = None;
                Task::none()
            }
            Message::OpenWorkspacesModal => {
                self.command_palette.close();
                self.quick_commands_ui.close();
                self.protocol_launcher.open = false;
                self.workspaces_ui.open_fresh();
                Task::none()
            }
            Message::CloseWorkspacesModal => {
                self.workspaces_ui.close();
                Task::none()
            }
            Message::WorkspaceNameChanged(val) => {
                self.workspaces_ui.draft_name = val;
                self.workspaces_ui.error = None;
                Task::none()
            }
            Message::WorkspaceDescChanged(val) => {
                self.workspaces_ui.draft_description = val;
                Task::none()
            }
            Message::SaveCurrentWorkspaceTemplate => {
                self.save_current_workspace_template(window_scope);
                Task::none()
            }
            Message::LaunchWorkspaceTemplate(id) => {
                let task = self.launch_workspace_template(window_scope, &id);
                self.workspaces_ui.close();
                task
            }
            Message::DeleteWorkspaceTemplate(id) => {
                self.workspaces_cfg.workspaces.retain(|w| w.id != id);
                if let Err(err) = save_workspaces(&self.paths, &self.workspaces_cfg) {
                    self.workspaces_ui.error = Some(err.to_string());
                }
                Task::none()
            }
            Message::OpenProtocolLauncherModal(tab) => {
                self.command_palette.close();
                self.quick_commands_ui.close();
                self.workspaces_ui.close();
                self.protocol_launcher.open = true;
                self.protocol_launcher.tab = tab;
                self.protocol_launcher.error = None;
                self.refresh_protocol_discovery();
                Task::none()
            }
            Message::CloseProtocolLauncherModal => {
                self.protocol_launcher.open = false;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::ProtocolLauncherSelectTab(tab) => {
                self.protocol_launcher.tab = tab;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::RefreshProtocolDiscovery => {
                self.refresh_protocol_discovery();
                Task::none()
            }
            Message::LaunchDiscoveredContainer { runtime, container } => {
                let exec_cfg = ContainerExecConfig {
                    runtime,
                    container: container.clone(),
                    shell: None,
                    user: None,
                    working_dir: None,
                };
                match self.spawn_docker_session(exec_cfg, Some(container), None) {
                    Ok(sid) => {
                        let _ = self.open_session_with_disposition(
                            window_scope,
                            sid,
                            None,
                            LaunchDisposition::NewTab,
                        );
                        self.protocol_launcher.open = false;
                        self.protocol_launcher.error = None;
                    }
                    Err(err) => {
                        self.protocol_launcher.error = Some(err);
                    }
                }
                Task::none()
            }
            Message::ProtocolSerialPortChanged(val) => {
                self.protocol_launcher.serial_port_input = val;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::ProtocolSerialBaudChanged(val) => {
                self.protocol_launcher.serial_baud_input = val;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::LaunchQuickSerial => {
                let port = self.protocol_launcher.serial_port_input.trim().to_string();
                if port.is_empty() {
                    self.protocol_launcher.error = Some("Serial port path is required".to_string());
                    return Task::none();
                }
                let baud_rate: u32 = match self.protocol_launcher.serial_baud_input.trim().parse() {
                    Ok(b) if b > 0 => b,
                    _ => {
                        self.protocol_launcher.error =
                            Some("Baud rate must be a positive integer".to_string());
                        return Task::none();
                    }
                };
                let cfg = SerialConnectConfig {
                    port,
                    baud_rate,
                    ..SerialConnectConfig::default()
                };
                let sid = self.insert_serial_session_slot(cfg, None, None, true);
                let _ = self.open_session_with_disposition(
                    window_scope,
                    sid,
                    None,
                    LaunchDisposition::NewTab,
                );
                self.protocol_launcher.open = false;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::ProtocolTelnetHostChanged(val) => {
                self.protocol_launcher.telnet_host_input = val;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::ProtocolTelnetPortChanged(val) => {
                self.protocol_launcher.telnet_port_input = val;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::ProtocolTelnetAutoReconnectToggled(enabled) => {
                self.protocol_launcher.telnet_auto_reconnect = enabled;
                Task::none()
            }
            Message::LaunchQuickTelnet => {
                let host = self.protocol_launcher.telnet_host_input.trim().to_string();
                if host.is_empty() {
                    self.protocol_launcher.error = Some("Telnet host is required".to_string());
                    return Task::none();
                }
                let port: u16 = match self.protocol_launcher.telnet_port_input.trim().parse() {
                    Ok(p) if p > 0 => p,
                    _ => {
                        self.protocol_launcher.error =
                            Some("Telnet port must be 1..65535".to_string());
                        return Task::none();
                    }
                };
                let cfg = TelnetConnectConfig {
                    host,
                    port,
                    auto_reconnect: self.protocol_launcher.telnet_auto_reconnect,
                    ..TelnetConnectConfig::default()
                };
                let sid = self.insert_telnet_session_slot(cfg, None, None, true);
                let _ = self.open_session_with_disposition(
                    window_scope,
                    sid,
                    None,
                    LaunchDisposition::NewTab,
                );
                self.protocol_launcher.open = false;
                self.protocol_launcher.error = None;
                Task::none()
            }
            Message::TriggerZmodemSend(session_id) => {
                self.command_palette.close();
                self.zmodem_send_ui.open_for(session_id);
                Task::none()
            }
            Message::CloseZmodemSendModal => {
                self.zmodem_send_ui.close();
                Task::none()
            }
            Message::ZmodemPathInputChanged(val) => {
                self.zmodem_send_ui.file_path_draft = val;
                self.zmodem_send_ui.error = None;
                Task::none()
            }
            Message::SubmitZmodemSend => {
                let Some(sid) = self.zmodem_send_ui.session_id else {
                    return Task::none();
                };
                let files: Vec<PathBuf> = self
                    .zmodem_send_ui
                    .file_path_draft
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\''))
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect();
                if files.is_empty() {
                    self.zmodem_send_ui.error = Some("Enter at least one local file path".to_string());
                    return Task::none();
                }
                match self.start_zmodem_send(sid, files) {
                    Ok(()) => {
                        self.zmodem_send_ui.close();
                    }
                    Err(err) => {
                        self.zmodem_send_ui.error = Some(err);
                    }
                }
                Task::none()
            }
            Message::StartZmodemSend { session_id, files } => {
                if let Err(err) = self.start_zmodem_send(session_id, files) {
                    self.last_error = Some(err.clone());
                    self.last_zmodem_notice = Some(format!("Zmodem send failed: {err}"));
                }
                Task::none()
            }
            Message::CancelZmodem(session_id) => {
                self.cancel_zmodem_session(session_id);
                Task::none()
            }
            Message::SerialSetInputMode { session_id, mode } => {
                if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id) {
                    slot.config.input_mode = mode;
                    if let Some(active) = &slot.active {
                        active.set_input_mode(mode);
                    }
                }
                Task::none()
            }
            Message::SerialSetOutputMode { session_id, mode } => {
                if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id) {
                    slot.config.output_mode = mode;
                    if let Some(active) = &slot.active {
                        active.set_output_mode(mode);
                    }
                }
                Task::none()
            }
            Message::SerialSetNewlineMode { session_id, mode } => {
                if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id) {
                    slot.config.newline_mode = mode;
                    if let Some(active) = &slot.active {
                        active.set_newline_mode(mode);
                    }
                }
                Task::none()
            }
            Message::SerialHexDraftChanged { session_id, value } => {
                if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id) {
                    slot.hex_draft = value;
                    slot.last_error = None;
                }
                Task::none()
            }
            Message::SerialSendHex(session_id) => {
                if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id) {
                    let hex = slot.hex_draft.clone();
                    if let Some(active) = &slot.active {
                        match active.send_hex(&hex) {
                            Ok(()) => {
                                slot.hex_draft.clear();
                                slot.last_error = None;
                            }
                            Err(err) => {
                                slot.last_error = Some(err.to_string());
                            }
                        }
                    } else {
                        slot.last_error = Some("Serial port is disconnected".to_string());
                    }
                }
                Task::none()
            }
            Message::SerialReconnect(session_id) => {
                self.connect_serial_session(session_id);
                Task::none()
            }
            Message::SerialDisconnect(session_id) => {
                if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id) {
                    if let Some(active) = &slot.active {
                        active.disconnect();
                    }
                    slot.apply_protocol_status(ProtocolStatus::Disconnected, Instant::now());
                }
                Task::none()
            }
            Message::SerialStatusChanged {
                session_id,
                generation,
                status,
            } => {
                if let Some(SessionEntry::Serial(slot)) = self.sessions.get_mut(&session_id)
                    && slot.generation == generation
                {
                    slot.apply_protocol_status(status, Instant::now());
                    slot.snapshot = Arc::new(slot.buffer.snapshot());
                }
                Task::none()
            }
            Message::TelnetReconnect(session_id) => {
                self.connect_telnet_session(session_id);
                Task::none()
            }
            Message::TelnetDisconnect(session_id) => {
                if let Some(SessionEntry::Telnet(slot)) = self.sessions.get_mut(&session_id) {
                    if let Some(active) = &slot.active {
                        active.disconnect();
                    }
                    slot.apply_protocol_status(ProtocolStatus::Disconnected, Instant::now());
                }
                Task::none()
            }
            Message::TelnetStatusChanged {
                session_id,
                generation,
                status,
            } => {
                if let Some(SessionEntry::Telnet(slot)) = self.sessions.get_mut(&session_id)
                    && slot.generation == generation
                {
                    slot.apply_protocol_status(status, Instant::now());
                    slot.snapshot = Arc::new(slot.buffer.snapshot());
                }
                Task::none()
            }
            Message::ProfileFormProxyCommandChanged(val) => {
                self.profile_editor.proxy_command = val;
                if !self.profile_editor.proxy_command.trim().is_empty()
                    && self.profile_editor.connection_mode == SshConnectionMode::Direct
                {
                    self.profile_editor.connection_mode = SshConnectionMode::ProxyCommand;
                }
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormForwardAgentToggled(enabled) => {
                self.profile_editor.forward_agent = enabled;
                Task::none()
            }
            Message::ProfileFormForwardX11Toggled(enabled) => {
                self.profile_editor.forward_x11 = enabled;
                Task::none()
            }
            Message::ProfileFormKeepaliveIntervalChanged(val) => {
                self.profile_editor.ssh_keepalive_interval = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormKeepaliveMissedChanged(val) => {
                self.profile_editor.ssh_keepalive_missed = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormScriptExpectChanged(val) => {
                self.profile_editor.script_expect_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormScriptSendChanged(val) => {
                self.profile_editor.script_send_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormScriptRegexToggled(enabled) => {
                self.profile_editor.script_is_regex_draft = enabled;
                Task::none()
            }
            Message::ProfileFormAddScriptStep => {
                if let Err(err) = self.profile_editor.add_login_script_step() {
                    self.profile_editor.validation_error = Some(err);
                }
                Task::none()
            }
            Message::ProfileFormRemoveScriptStep(idx) => {
                if idx < self.profile_editor.login_scripts.len() {
                    self.profile_editor.login_scripts.remove(idx);
                }
                Task::none()
            }
            Message::ProfileFormTunnelNameChanged(val) => {
                self.profile_editor.tunnel_name_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormTunnelBindPortChanged(val) => {
                self.profile_editor.tunnel_bind_port_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormTunnelTargetHostChanged(val) => {
                self.profile_editor.tunnel_target_host_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormTunnelTargetPortChanged(val) => {
                self.profile_editor.tunnel_target_port_draft = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormAddTunnel => {
                if let Err(err) = self.profile_editor.add_tunnel_rule() {
                    self.profile_editor.validation_error = Some(err);
                }
                Task::none()
            }
            Message::ProfileFormRemoveTunnel(idx) => {
                if idx < self.profile_editor.tunnels.len() {
                    self.profile_editor.tunnels.remove(idx);
                }
                Task::none()
            }
            Message::ProfileFormSerialPortChanged(val) => {
                self.profile_editor.serial_port = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormSerialBaudChanged(val) => {
                self.profile_editor.serial_baud_rate = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormSerialInputModeChanged(mode) => {
                self.profile_editor.serial_input_mode = mode;
                Task::none()
            }
            Message::ProfileFormSerialOutputModeChanged(mode) => {
                self.profile_editor.serial_output_mode = mode;
                Task::none()
            }
            Message::ProfileFormSerialAutoReconnectToggled(enabled) => {
                self.profile_editor.serial_auto_reconnect = enabled;
                Task::none()
            }
            Message::ProfileFormTelnetHostChanged(val) => {
                self.profile_editor.telnet_host = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormTelnetPortChanged(val) => {
                self.profile_editor.telnet_port = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormTelnetAutoReconnectToggled(enabled) => {
                self.profile_editor.telnet_auto_reconnect = enabled;
                Task::none()
            }
            Message::ProfileFormDockerRuntimeChanged(runtime) => {
                self.profile_editor.docker_runtime = runtime;
                Task::none()
            }
            Message::ProfileFormDockerContainerChanged(val) => {
                self.profile_editor.docker_container = val;
                self.profile_editor.validation_error = None;
                Task::none()
            }
            Message::ProfileFormDockerShellChanged(val) => {
                self.profile_editor.docker_shell = val;
                Task::none()
            }
            Message::ProfileFormDockerUserChanged(val) => {
                self.profile_editor.docker_user = val;
                Task::none()
            }
            Message::ProfileFormDockerWorkdirChanged(val) => {
                self.profile_editor.docker_working_dir = val;
                Task::none()
            }
            Message::SettingsSyncProviderSelected(provider) => {
                self.settings_ui.set_sync_provider(provider);
                Task::none()
            }
            Message::SettingsSyncLocalDirChanged(val) => {
                self.settings_ui.set_sync_local_dir(val);
                Task::none()
            }
            Message::SettingsSyncWebDavUrlChanged(val) => {
                self.settings_ui.set_sync_webdav_url(val);
                Task::none()
            }
            Message::SettingsSyncWebDavUserChanged(val) => {
                self.settings_ui.set_sync_webdav_username(val);
                Task::none()
            }
            Message::SettingsSyncGistIdChanged(val) => {
                self.settings_ui.set_sync_gist_id(val);
                Task::none()
            }
            Message::SettingsSyncTokenChanged(val) => {
                self.settings_ui.set_sync_token_draft(val);
                Task::none()
            }
            Message::SettingsSyncPassphraseChanged(val) => {
                self.settings_ui.set_sync_passphrase_draft(val);
                Task::none()
            }
            Message::SettingsSyncEncryptToggled(enabled) => {
                self.settings_ui.set_sync_encrypt(enabled);
                Task::none()
            }
            Message::SettingsSyncAutoToggled(enabled) => {
                self.settings_ui.set_sync_auto_save(enabled);
                Task::none()
            }
            Message::SettingsSyncPushNow => self.trigger_sync_task(true),
            Message::SettingsSyncPullNow => self.trigger_sync_task(false),
            Message::ConfigSyncFinished { is_push, result } => {
                self.apply_sync_finished(is_push, result);
                Task::none()
            }
            Message::ToggleQuakeWindow | Message::GlobalHotkeyTriggered => {
                self.toggle_quake_window()
            }
            Message::TrayAction(action) => self.handle_tray_action(action),
        }
    }

    fn toggle_quake_window(&mut self) -> Task<Message> {
        let geom = self.quake.current_bounds(&self.config.quake);
        if let Some(qid) = self.quake.window_id()
            && self.window_workspaces.contains_key(&qid)
        {
            if self.quake.is_visible() {
                self.quake.set_visible(false);
                self.sync_tray_state();
                return window::set_mode(qid, window::Mode::Hidden);
            }
            self.quake.set_visible(true);
            self.active_window = Some(qid);
            self.lifecycle = DaemonLifecycle::Active;
            self.sync_tray_state();
            return Task::batch([
                window::set_mode(qid, window::Mode::Windowed),
                window::move_to(qid, Point::new(geom.x, geom.y)),
                window::resize(qid, Size::new(geom.width, geom.height)),
                window::gain_focus(qid),
            ]);
        }

        let mut workspace = Workspace::with_ids(self.workspace_ids.clone());
        match self.spawn_default_local_session() {
            Ok(sid) => {
                workspace.create_tab(sid, Some("Quake".to_string()));
            }
            Err(err) => {
                error!(error = %err, "failed spawning Quake shell session");
                self.last_error = Some(err);
            }
        }
        let (win_id, open_task) = window::open(quake_window_settings(
            &self.config.quake,
            self.quake.monitor_size(),
        ));
        self.windows.insert(win_id);
        self.window_workspaces.insert(
            win_id,
            WindowState {
                workspace,
                width: geom.width.round() as u32,
                height: geom.height.round() as u32,
                maximized: false,
            },
        );
        self.quake.set_open(win_id);
        self.active_window = Some(win_id);
        self.lifecycle = DaemonLifecycle::Active;
        self.sync_tray_state();
        Task::batch([
            open_task.map(Message::WindowOpened),
            window::monitor_size(win_id).map(move |sz| Message::QuakeMonitorSize(win_id, sz)),
        ])
    }

    fn handle_tray_action(&mut self, action: TrayAction) -> Task<Message> {
        match action {
            TrayAction::ShowOrHideWindow => {
                let quake_id = self.quake.window_id();
                let non_quake: Vec<window::Id> = self
                    .windows
                    .iter()
                    .copied()
                    .filter(|id| Some(*id) != quake_id)
                    .collect();
                if !non_quake.is_empty() && !self.tray_hidden {
                    self.tray_hidden = true;
                    self.lifecycle = DaemonLifecycle::BackgroundRunning;
                    self.sync_tray_state();
                    Task::batch(
                        non_quake
                            .into_iter()
                            .map(|id| window::set_mode(id, window::Mode::Hidden)),
                    )
                } else if !non_quake.is_empty() {
                    self.tray_hidden = false;
                    self.lifecycle = DaemonLifecycle::Active;
                    let first = non_quake[0];
                    self.active_window = Some(first);
                    let mut tasks: Vec<Task<Message>> = non_quake
                        .into_iter()
                        .map(|id| window::set_mode(id, window::Mode::Windowed))
                        .collect();
                    tasks.push(window::gain_focus(first));
                    self.sync_tray_state();
                    Task::batch(tasks)
                } else {
                    self.update(Message::IpcCommand(IpcCommand::OpenWindow))
                }
            }
            TrayAction::ToggleQuake => self.toggle_quake_window(),
            TrayAction::LaunchProfile(profile_id) => {
                self.tray_hidden = false;
                self.lifecycle = DaemonLifecycle::Active;
                let task = self.open_tab_in_window_with_cwd(None, Some(&profile_id), None);
                self.sync_tray_state();
                task
            }
            TrayAction::Quit => {
                self.hotkey_guard = None;
                self.update(Message::CloseGuardStopAndQuit)
            }
        }
    }

    fn sync_tray_state(&self) {
        let non_quake_present = self
            .windows
            .iter()
            .any(|id| Some(*id) != self.quake.window_id());
        let window_visible = non_quake_present && !self.tray_hidden;
        let quick_profiles = self
            .profiles
            .profiles
            .iter()
            .take(8)
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        let bw = self.background_work_source.status();
        self.tray_adapter.update_state(TrayStateSnapshot {
            window_visible,
            quake_visible: self.quake.is_visible(),
            active_tunnel_count: self.tunnel_manager.snapshot().active_count,
            active_transfer_count: bw.active_transfers,
            quick_profiles,
        });
    }

    fn run_quick_command_now(
        &mut self,
        window_scope: Option<window::Id>,
        cmd: &QuickCommand,
        broadcast_override: Option<bool>,
    ) {
        let payload = cmd.payload_bytes();
        if payload.is_empty() {
            return;
        }
        let use_broadcast = broadcast_override.unwrap_or(cmd.broadcast);
        let targets: Vec<SessionId> = if use_broadcast {
            self.target_workspace(window_scope)
                .and_then(|ws| ws.active_tab())
                .map(|tab| tab.root.session_ids())
                .unwrap_or_default()
        } else {
            self.active_session_in_window(window_scope)
                .into_iter()
                .collect()
        };
        for sid in targets {
            self.write_bytes_to_session(sid, &payload);
        }
    }

    fn save_current_workspace_template(&mut self, window_scope: Option<window::Id>) {
        let name = self.workspaces_ui.draft_name.trim().to_string();
        if name.is_empty() {
            self.workspaces_ui.error = Some("Workspace name is required".to_string());
            return;
        }
        let desc = {
            let trimmed = self.workspaces_ui.draft_description.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let Some(ws) = self.target_workspace(window_scope) else {
            self.workspaces_ui.error = Some("No active workspace to snapshot".to_string());
            return;
        };
        let (tabs, active_tab) = self.export_workspace_tabs(ws);
        if tabs.is_empty() {
            self.workspaces_ui.error = Some("Open at least one tab before saving".to_string());
            return;
        }
        let mut tpl = WorkspaceTemplate::new(name.clone(), tabs, active_tab);
        tpl.description = desc;
        if let Some(existing) = self
            .workspaces_cfg
            .workspaces
            .iter_mut()
            .find(|w| w.name.eq_ignore_ascii_case(&name))
        {
            tpl.id = existing.id.clone();
            *existing = tpl;
        } else {
            self.workspaces_cfg.workspaces.push(tpl);
        }
        if let Err(err) = save_workspaces(&self.paths, &self.workspaces_cfg) {
            self.workspaces_ui.error = Some(err.to_string());
        } else {
            self.workspaces_ui.draft_name.clear();
            self.workspaces_ui.draft_description.clear();
            self.workspaces_ui.error = None;
            self.last_error = None;
        }
    }

    /// Launches all tabs and nested splits defined by a saved [`WorkspaceTemplate`] by id.
    pub fn launch_workspace_template(
        &mut self,
        window_scope: Option<window::Id>,
        template_id: &str,
    ) -> Task<Message> {
        let Some(tpl) = self
            .workspaces_cfg
            .workspaces
            .iter()
            .find(|w| w.id == template_id)
            .cloned()
        else {
            self.workspaces_ui.error = Some(format!("Unknown workspace template: {template_id}"));
            return Task::none();
        };
        let mut created_tabs = Vec::new();
        for tab_layout in &tpl.tabs {
            let mut leaf_panes = Vec::new();
            match self.restore_split_node(&tab_layout.root, &mut leaf_panes) {
                Ok(root) if !leaf_panes.is_empty() => {
                    let focused_pane = leaf_panes
                        .get(tab_layout.focused_pane)
                        .copied()
                        .unwrap_or(leaf_panes[0]);
                    let zoomed_pane = tab_layout
                        .zoomed_pane
                        .and_then(|idx| leaf_panes.get(idx).copied());
                    created_tabs.push((
                        tab_layout.title.clone(),
                        tab_layout.color.clone(),
                        tab_layout.pinned,
                        root,
                        focused_pane,
                        zoomed_pane,
                    ));
                }
                Ok(_) => {}
                Err(err) => {
                    self.last_error = Some(err);
                }
            }
        }
        let mut first_tab_idx = None;
        let tab_count = created_tabs.len();
        if let Some(ws) = self.target_workspace_mut(window_scope) {
            for (title, color, pinned, root, focused, zoomed) in created_tabs {
                let idx_before = ws.tab_count();
                if first_tab_idx.is_none() {
                    first_tab_idx = Some(idx_before);
                }
                let tid = ws.restore_tab(title, root, focused, zoomed);
                ws.set_tab_metadata(tid, color, pinned);
            }
            if let Some(base) = first_tab_idx
                && tab_count > 0
            {
                let target_idx = base + tpl.active_tab.min(tab_count - 1);
                ws.set_active_tab_index(target_idx);
            }
        } else {
            for (_, _, _, root, _, _) in created_tabs {
                for sid in root.session_ids() {
                    self.remove_session(sid);
                }
            }
        }
        Task::none()
    }

    fn refresh_protocol_discovery(&mut self) {
        let runner = (self.docker_runner_factory)();
        let discovery = DockerDiscovery::new(runner);
        self.protocol_launcher.containers = discovery.list_all_containers(false);
        match (self.serial_port_lister)() {
            Ok(ports) => {
                if self.protocol_launcher.serial_port_input.trim().is_empty()
                    && let Some(first) = ports.first()
                {
                    self.protocol_launcher.serial_port_input = first.path.clone();
                }
                self.protocol_launcher.serial_ports = ports;
            }
            Err(err) => {
                self.protocol_launcher.serial_ports.clear();
                self.protocol_launcher.error = Some(err);
            }
        }
    }

    fn drain_zmodem_events(&mut self, session_id: SessionId) {
        let events: Vec<ZmodemEvent> = {
            let Some(ctrl) = self.zmodem_controllers.get(&session_id) else {
                return;
            };
            let mut guard = ctrl
                .pending_events
                .lock()
                .expect("zmodem pending_events mutex poisoned");
            if guard.is_empty() {
                return;
            }
            std::mem::take(&mut *guard)
        };
        for ev in events {
            match ev {
                ZmodemEvent::RemoteReadyToReceive => {
                    self.last_zmodem_notice =
                        Some("Zmodem remote ready (rz): open Send File to transfer".to_string());
                    self.zmodem_send_ui.open_for(session_id);
                }
                ZmodemEvent::TransferStarted {
                    direction,
                    file_name,
                    total_bytes,
                } => {
                    let dir_label = match direction {
                        ZmodemDirection::Send => "Sending",
                        ZmodemDirection::Receive => "Receiving",
                    };
                    self.last_zmodem_notice = Some(match total_bytes {
                        Some(tot) => format!("Zmodem {dir_label}: {file_name} (0/{tot} B)"),
                        None => format!("Zmodem {dir_label}: {file_name}"),
                    });
                    self.set_session_tab_progress(session_id, TerminalProgressState::Normal(0));
                }
                ZmodemEvent::TransferProgress {
                    direction,
                    file_name,
                    transferred_bytes,
                    total_bytes,
                } => {
                    let dir_label = match direction {
                        ZmodemDirection::Send => "Sending",
                        ZmodemDirection::Receive => "Receiving",
                    };
                    let pct = total_bytes
                        .filter(|&t| t > 0)
                        .map(|t| ((transferred_bytes.saturating_mul(100)) / t).min(100) as u8)
                        .unwrap_or(50);
                    self.last_zmodem_notice = Some(format!(
                        "Zmodem {dir_label}: {file_name} ({pct}%)"
                    ));
                    self.set_session_tab_progress(session_id, TerminalProgressState::Normal(pct));
                }
                ZmodemEvent::TransferCompleted {
                    direction,
                    file_name,
                    local_path,
                    bytes,
                } => {
                    let dir_label = match direction {
                        ZmodemDirection::Send => "sent",
                        ZmodemDirection::Receive => "received",
                    };
                    self.last_zmodem_notice = Some(format!(
                        "Zmodem {dir_label}: {file_name} ({bytes} B -> {})",
                        local_path.display()
                    ));
                    self.set_session_tab_progress(session_id, TerminalProgressState::Normal(100));
                }
                ZmodemEvent::SessionFinished => {
                    self.set_session_tab_progress(session_id, TerminalProgressState::Hidden);
                }
                ZmodemEvent::Error(err) => {
                    self.last_zmodem_notice = Some(format!("Zmodem error: {err}"));
                    self.set_session_tab_progress(session_id, TerminalProgressState::Error(100));
                }
            }
        }
    }

    fn set_session_tab_progress(
        &mut self,
        session_id: SessionId,
        progress: TerminalProgressState,
    ) {
        if let Some(ws) = self.workspace_for_session_mut(session_id)
            && let Some(tab_id) = ws
                .tabs()
                .iter()
                .find(|t| t.root.pane_for_session(session_id).is_some())
                .map(|t| t.id)
        {
            ws.set_tab_progress(tab_id, progress);
        }
    }

    /// Initiates a Zmodem (`sz`-compatible) multi-file send over `session_id`'s active wire transport.
    pub fn start_zmodem_send(
        &mut self,
        session_id: SessionId,
        files: Vec<PathBuf>,
    ) -> Result<(), String> {
        let Some(ctrl) = self.zmodem_controllers.get(&session_id).cloned() else {
            return Err(format!("Unknown session {}", session_id.0));
        };
        let count = files.len();
        let step = {
            let mut guard = ctrl.engine.lock().expect("zmodem engine mutex poisoned");
            guard.start_send(files).map_err(|e| e.to_string())?
        };
        self.last_zmodem_notice = Some(format!("Zmodem send initiating ({count} file(s))..."));
        if !step.events.is_empty() {
            ctrl.pending_events
                .lock()
                .expect("zmodem pending_events mutex poisoned")
                .extend(step.events);
        }
        if !step.wire_output.is_empty() {
            self.write_zmodem_wire_bytes(session_id, &step.wire_output)?;
        }
        self.drain_zmodem_events(session_id);
        Ok(())
    }

    fn cancel_zmodem_session(&mut self, session_id: SessionId) {
        let Some(ctrl) = self.zmodem_controllers.get(&session_id).cloned() else {
            return;
        };
        let step = ctrl
            .engine
            .lock()
            .expect("zmodem engine mutex poisoned")
            .cancel();
        if !step.wire_output.is_empty() {
            let _ = self.write_zmodem_wire_bytes(session_id, &step.wire_output);
        }
        self.last_zmodem_notice = Some("Zmodem transfer cancelled".to_string());
        self.set_session_tab_progress(session_id, TerminalProgressState::Hidden);
    }

    fn store_sync_token_from_draft_if_present(
        &mut self,
        sync_cfg: &mut termewnal_core::SyncConfig,
    ) -> Result<(), String> {
        let draft_token = self.settings_ui.sync_token_draft().trim().to_string();
        if draft_token.is_empty() {
            return Ok(());
        }
        if self.vault.requires_unlock() {
            return Err(
                "Termewnal Vault is locked — unlock Vault first before saving sync token"
                    .to_string(),
            );
        }
        let secret_ref = sync_cfg
            .vault_token_ref
            .get_or_insert_with(|| "termewnal/sync/token".to_string())
            .clone();
        self.vault
            .set_secret(&secret_ref, &draft_token)
            .map_err(|e| format!("Failed storing sync token in Vault: {e}"))?;
        self.settings_ui.set_sync_vault_token_ref(Some(secret_ref));
        self.settings_ui.clear_sync_token_draft();
        Ok(())
    }

    fn trigger_sync_task(&mut self, is_push: bool) -> Task<Message> {
        let mut next_cfg = match self.settings_ui.build_validated_config() {
            Ok(c) => c,
            Err(err) => {
                self.settings_ui.set_error(Some(err));
                return Task::none();
            }
        };
        if let Err(err) = self.store_sync_token_from_draft_if_present(&mut next_cfg.sync) {
            self.settings_ui.set_error(Some(err));
            return Task::none();
        }
        if let Err(err) = save_config(&self.paths, &next_cfg) {
            self.settings_ui.set_error(Some(err.to_string()));
            return Task::none();
        }
        self.config = next_cfg;
        self.settings_ui.sync_from_config(&self.config);
        self.settings_ui.set_sync_status_notice(Some(if is_push {
            "Pushing configuration sync bundle...".to_string()
        } else {
            "Pulling configuration sync bundle...".to_string()
        }));

        let passphrase = {
            let trimmed = self.settings_ui.sync_passphrase_draft().trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let service = Arc::clone(&self.sync_service);
        let paths = self.paths.clone();
        let sync_cfg = self.config.sync.clone();
        let vault = Arc::clone(&self.vault);

        let fut = async move {
            if is_push {
                service
                    .push(&paths, &sync_cfg, Some(&*vault), passphrase.as_deref())
                    .await
                    .map(|out| {
                        (
                            format!(
                                "Pushed {} bytes to {}",
                                out.bytes_uploaded, out.target_location
                            ),
                            out.created_gist_id,
                        )
                    })
                    .map_err(|e| e.to_string())
            } else {
                service
                    .pull(&paths, &sync_cfg, Some(&*vault), passphrase.as_deref())
                    .await
                    .map(|out| {
                        (
                            format!(
                                "Pulled & restored config from {} ({} files)",
                                out.source_location,
                                out.apply_report.applied_files.len()
                            ),
                            None,
                        )
                    })
                    .map_err(|e| e.to_string())
            }
        };

        if tokio::runtime::Handle::try_current().is_ok() {
            Task::perform(fut, move |result| Message::ConfigSyncFinished {
                is_push,
                result,
            })
        } else if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let result = rt.block_on(fut);
            self.apply_sync_finished(is_push, result);
            Task::none()
        } else {
            self.apply_sync_finished(
                is_push,
                Err("Failed constructing runtime for config sync".to_string()),
            );
            Task::none()
        }
    }

    /// Executes an immediate Config Sync Push and applies updated status/Gist ID/timestamp.
    pub async fn sync_push_now_async(&mut self) -> Result<String, String> {
        let mut next_cfg = self.settings_ui.build_validated_config()?;
        self.store_sync_token_from_draft_if_present(&mut next_cfg.sync)?;
        self.config = next_cfg;
        let passphrase = {
            let trimmed = self.settings_ui.sync_passphrase_draft().trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let out = self
            .sync_service
            .push(
                &self.paths,
                &self.config.sync,
                Some(&*self.vault),
                passphrase.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let summary = format!(
            "Pushed {} bytes to {}",
            out.bytes_uploaded, out.target_location
        );
        self.apply_sync_finished(true, Ok((summary.clone(), out.created_gist_id)));
        Ok(summary)
    }

    /// Executes an immediate Config Sync Pull, reloads configs cleanly, and updates status.
    pub async fn sync_pull_now_async(&mut self) -> Result<String, String> {
        let mut next_cfg = self.settings_ui.build_validated_config()?;
        self.store_sync_token_from_draft_if_present(&mut next_cfg.sync)?;
        self.config = next_cfg;
        let passphrase = {
            let trimmed = self.settings_ui.sync_passphrase_draft().trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let out = self
            .sync_service
            .pull(
                &self.paths,
                &self.config.sync,
                Some(&*self.vault),
                passphrase.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let summary = format!(
            "Pulled & restored config from {} ({} files)",
            out.source_location,
            out.apply_report.applied_files.len()
        );
        self.apply_sync_finished(false, Ok((summary.clone(), None)));
        Ok(summary)
    }

    fn apply_sync_finished(
        &mut self,
        is_push: bool,
        result: Result<(String, Option<String>), String>,
    ) {
        match result {
            Ok((msg, created_gist_id)) => {
                if !is_push {
                    if let Ok((cfg, profiles)) = ensure_config_files(&self.paths) {
                        self.config = cfg;
                        self.profiles = profiles;
                        self.rebuild_runtime_config_caches();
                        self.sync_terminal_options_to_sessions();
                        self.sync_tunnel_telemetry();
                    }
                    if let Ok(qc) = load_quick_commands(&self.paths) {
                        self.quick_commands = qc;
                    }
                    if let Ok(ws) = load_workspaces(&self.paths) {
                        self.workspaces_cfg = ws;
                    }
                }
                if let Some(gist_id) = created_gist_id {
                    self.config.sync.gist_id = Some(gist_id);
                }
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                self.config.sync.last_sync_at = Some(now_secs.to_string());
                let _ = save_config(&self.paths, &self.config);
                if self.settings_ui.is_open() {
                    self.settings_ui.sync_from_config(&self.config);
                    self.settings_origin_config = Some(self.config.clone());
                }
                self.settings_ui.set_sync_status_notice(Some(msg));
                self.settings_ui.set_error(None);
                self.last_error = None;
            }
            Err(err) => {
                self.settings_ui.set_sync_status_notice(Some(format!("Sync failed: {err}")));
                self.settings_ui.set_error(Some(err.clone()));
                self.last_error = Some(err);
            }
        }
    }

    fn apply_live_settings_preview(&mut self) {
        let draft = self.settings_ui.draft().clone();
        self.config.terminal = draft.terminal;
        if self.config.terminal.font_family.trim().is_empty() {
            self.config.terminal.font_family = "monospace".to_string();
        }
        self.config.keybindings = draft.keybindings;
        self.rebuild_runtime_config_caches();
        self.sync_terminal_options_to_sessions();
    }

    fn close_settings_canceling_preview(&mut self) {
        if let Some(original) = self.settings_origin_config.take() {
            let prev_sidebar = self.config.terminal.show_profile_sidebar;
            self.config = original;
            if self.config.terminal.show_profile_sidebar != prev_sidebar {
                self.sidebar.set_open(self.config.terminal.show_profile_sidebar);
            }
            self.rebuild_runtime_config_caches();
            self.sync_terminal_options_to_sessions();
            self.transfer_queue
                .set_transfer_config(self.config.transfer.clone());
        }
        self.settings_ui.clear_sync_token_draft();
        self.settings_ui.close();
    }

    fn apply_settings_save(&mut self) -> Task<Message> {
        let mut next_cfg = match self.settings_ui.build_validated_config() {
            Ok(cfg) => cfg,
            Err(err) => {
                self.settings_ui.set_error(Some(err));
                return Task::none();
            }
        };
        if let Err(err) = self.store_sync_token_from_draft_if_present(&mut next_cfg.sync) {
            self.settings_ui.set_error(Some(err));
            return Task::none();
        }
        if let Err(err) = save_config(&self.paths, &next_cfg) {
            warn!(error = %err, "failed to save config.toml from Settings modal");
            self.settings_ui.set_error(Some(err.to_string()));
            return Task::none();
        }
        let auto_sync = next_cfg.sync.auto_sync_on_save
            && next_cfg.sync.provider != SyncProviderKind::None
            && (!next_cfg.sync.encrypt_bundle
                || !self.settings_ui.sync_passphrase_draft().trim().is_empty());
        self.config = next_cfg;
        self.settings_origin_config = None;
        self.rebuild_runtime_config_caches();
        self.sync_terminal_options_to_sessions();
        self.transfer_queue
            .set_transfer_config(self.config.transfer.clone());
        self.vault.apply_config(&self.config.vault);
        self.last_error = None;
        if auto_sync {
            let task = self.trigger_sync_task(true);
            self.settings_ui.close();
            task
        } else {
            self.settings_ui.close();
            Task::none()
        }
    }

    fn populate_profile_editor_jump_candidates(&mut self, editing_profile_id: Option<&str>) {
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();

        for profile in &self.profiles.profiles {
            if editing_profile_id.is_some_and(|eid| eid == profile.id) {
                continue;
            }
            if let ProfileTarget::Ssh {
                host,
                port,
                username,
                proxy_jump,
                ..
            } = &profile.target
            {
                let token = profile.name.trim().to_string();
                if token.is_empty() {
                    continue;
                }
                seen.insert(token.to_ascii_lowercase());
                candidates.push(JumpHostCandidate {
                    token,
                    endpoint: format!("{username}@{host}:{port}"),
                    kind: JumpHostKind::SavedProfile,
                    transitive_hops: proxy_jump.clone(),
                });
            }
        }

        let sys_hosts: Vec<SystemSshProfile> = if !self.openssh_hosts.is_empty() {
            self.openssh_hosts
                .iter()
                .map(SystemSshProfile::from)
                .collect()
        } else {
            self.sidebar.system_ssh_profiles().to_vec()
        };
        for host in sys_hosts {
            let alias = host.alias.trim().to_string();
            if alias.is_empty() || seen.contains(&alias.to_ascii_lowercase()) {
                continue;
            }
            seen.insert(alias.to_ascii_lowercase());
            candidates.push(JumpHostCandidate {
                token: alias,
                endpoint: host.display_target(),
                kind: JumpHostKind::SystemSsh,
                transitive_hops: host.proxy_jump,
            });
        }

        self.profile_editor.set_jump_host_candidates(candidates);
    }

    fn apply_profile_save(&mut self) {
        let existing = self
            .profile_editor
            .editing_id
            .as_deref()
            .and_then(|id| self.profiles.profiles.iter().find(|p| p.id == id))
            .cloned();
        let saved = match self.profile_editor.build_profile(existing.as_ref()) {
            Ok(draft) => draft,
            Err(err) => {
                self.profile_editor.validation_error = Some(err);
                return;
            }
        };

        let mut secrets = Vec::new();
        if let Some(pair) = saved.secret_to_store {
            secrets.push(pair);
        }
        secrets.extend(saved.extra_secrets_to_store);
        if !secrets.is_empty() {
            if self.vault.requires_unlock() {
                self.profile_editor.validation_error = Some(
                    "Termewnal Vault is locked — unlock Vault first before saving secrets"
                        .to_string(),
                );
                return;
            }
            for (secret_ref, secret) in secrets {
                if let Err(err) = self.vault.set_secret(&secret_ref, secret.as_str()) {
                    self.profile_editor.validation_error =
                        Some(format!("Failed storing secret in Vault: {err}"));
                    return;
                }
            }
        }

        let mut next_profiles = self.profiles.clone();
        if let Some(pos) = next_profiles
            .profiles
            .iter()
            .position(|p| p.id == saved.profile.id)
        {
            next_profiles.profiles[pos] = saved.profile;
        } else {
            next_profiles.profiles.push(saved.profile);
        }

        if let Err(err) = save_profiles(&self.paths, &next_profiles) {
            self.profile_editor.validation_error = Some(err.to_string());
            return;
        }

        self.profiles = next_profiles;
        self.last_error = None;
        self.sync_tunnel_telemetry();
        self.profile_editor.close();
    }

    fn apply_profile_delete(&mut self, profile_id: &str) {
        let mut next_profiles = self.profiles.clone();
        let before = next_profiles.profiles.len();
        next_profiles.profiles.retain(|p| p.id != profile_id);
        if next_profiles.profiles.len() == before {
            self.profile_editor.close();
            return;
        }
        if let Err(err) = save_profiles(&self.paths, &next_profiles) {
            self.profile_editor.validation_error = Some(err.to_string());
            return;
        }
        self.profiles = next_profiles;
        if self.sftp_state.remove_profile(profile_id) {
            self.save_sftp_state_best_effort();
        }
        self.last_error = None;
        self.sync_tunnel_telemetry();
        self.profile_editor.close();
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
            | SshUiEvent::Reconnected { generation, .. }
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
                let now = Instant::now();
                let sftp_key = slot.sftp_key();
                if let Some(ref session) = ssh {
                    self.sftp_manager.register_primary_transport(
                        sftp_key.clone(),
                        Arc::clone(session.primary_transport()),
                    );
                }
                slot.active = ssh;
                slot.supervisor = supervisor;
                slot.last_error = None;
                slot.exited = None;
                let connected_status = TransportStatus::Connected { latency_ms: None };
                slot.record_transport_transition(&connected_status, now);
                slot.transport_status = connected_status;
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                slot.snapshot = Arc::new(slot.buffer.snapshot());
                if let Some(drawer) = self.sftp_drawers.get_mut(&session_id) {
                    if slot.reconnected_at.is_some() {
                        drawer.mark_reconnected_now(now);
                    } else {
                        drawer.mark_connected_or_recovered(now);
                    }
                }
                let profile_id = slot.target.profile_id.clone();
                let supervisor_handle = slot.supervisor.clone();
                self.sync_sftp_status_notice();
                if self.transfer_queue.resume_reconnected(&sftp_key) > 0 {
                    self.sync_transfer_telemetry();
                }
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
                if self
                    .sftp_drawers
                    .get(&session_id)
                    .is_some_and(SftpDrawerState::is_open)
                {
                    let path = self.sftp_drawers[&session_id].current_path().to_string();
                    tasks.push(self.sftp_refresh_task(session_id, path));
                }
                Task::batch(tasks)
            }
            SshUiEvent::TransportStatus {
                status, last_error, ..
            } => {
                let now = Instant::now();
                let sftp_key = slot.sftp_key();
                let mut resumed = 0;
                let mut refresh_path: Option<String> = None;
                if !matches!(
                    status,
                    TransportStatus::Connected { .. } | TransportStatus::Degraded { .. }
                ) {
                    self.sftp_manager.clear_primary_transport(&sftp_key);
                    slot.active = None;
                }
                slot.record_transport_transition(&status, now);
                if matches!(status, TransportStatus::Connected { .. }) {
                    slot.last_error = None;
                    slot.exited = None;
                    if let Some(sup) = &slot.supervisor
                        && let Some(transport) = sup.current_transport()
                    {
                        self.sftp_manager
                            .register_primary_transport(sftp_key.clone(), Arc::clone(&transport));
                        slot.active = Some(Arc::new(SshSession::from_supervised(transport, sup)));
                    }
                    if let Some(drawer) = self.sftp_drawers.get_mut(&session_id) {
                        if slot.reconnected_at.is_some() {
                            drawer.mark_reconnected_now(now);
                        } else {
                            drawer.mark_connected_or_recovered(now);
                        }
                        if drawer.is_open() {
                            refresh_path = Some(drawer.current_path().to_string());
                        }
                    }
                    resumed = self.transfer_queue.resume_reconnected(&sftp_key);
                } else {
                    if let Some(err) = last_error.clone() {
                        slot.last_error = Some(err.clone());
                        self.last_error = Some(err);
                    }
                    if let Some(drawer) = self.sftp_drawers.get_mut(&session_id) {
                        let reason = last_error
                            .or_else(|| slot.last_error.clone())
                            .unwrap_or_else(|| "SSH connection lost".to_string());
                        match &status {
                            TransportStatus::Reconnecting {
                                attempt,
                                next_retry_secs,
                            } => {
                                drawer.update_auto_reconnecting_progress(
                                    *attempt,
                                    Some(*next_retry_secs),
                                    reason,
                                );
                            }
                            TransportStatus::Degraded { .. } => {
                                drawer.update_auto_reconnecting_progress(0, None, reason);
                            }
                            TransportStatus::Disconnected if drawer.is_auto_reconnecting() => {
                                drawer.set_connection_state(SftpConnectionState::Disconnected {
                                    reason,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                slot.transport_status = status;
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                slot.snapshot = Arc::new(slot.buffer.snapshot());
                self.sync_sftp_status_notice();
                if resumed > 0 {
                    self.sync_transfer_telemetry();
                }
                if let Some(path) = refresh_path {
                    return self.sftp_refresh_task(session_id, path);
                }
                Task::none()
            }
            SshUiEvent::Reconnected { .. } => {
                let now = Instant::now();
                slot.is_auto_reconnecting = false;
                slot.reconnect_attempt = None;
                slot.reconnected_at = Some(now);
                slot.last_error = None;
                let has_active = slot.active.is_some();
                let refresh_path = self.sftp_drawers.get_mut(&session_id).and_then(|drawer| {
                    drawer.mark_reconnected_now(now);
                    drawer.is_open().then(|| drawer.current_path().to_string())
                });
                self.sync_sftp_status_notice();
                if has_active && let Some(path) = refresh_path {
                    return self.sftp_refresh_task(session_id, path);
                }
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
                slot.is_auto_reconnecting = false;
                slot.reconnect_attempt = None;
                slot.reconnected_at = None;
                slot.server_stats = None;
                slot.server_stats_unsupported = false;
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
                if let Some(drawer) = self.sftp_drawers.get_mut(&session_id)
                    && drawer.is_auto_reconnecting()
                {
                    drawer.set_connection_state(SftpConnectionState::Disconnected {
                        reason: msg.clone(),
                    });
                }
                self.last_error = Some(msg);
                self.ssh_ui.record_mitm_alert(alert);
                self.sync_sftp_status_notice();
                Task::none()
            }
            SshUiEvent::Disconnected { error, .. } => {
                self.sftp_manager.clear_primary_transport(&slot.sftp_key());
                slot.active = None;
                slot.supervisor = None;
                slot.is_auto_reconnecting = false;
                slot.reconnect_attempt = None;
                slot.reconnected_at = None;
                slot.server_stats = None;
                slot.server_stats_unsupported = false;
                slot.transport_status = TransportStatus::Disconnected;
                slot.status_message = format_ssh_slot_message(&slot.target, &slot.transport_status);
                self.ssh_ui.clear_session_prompts(session_id);
                if let Some(err) = error {
                    slot.last_error = Some(err.clone());
                    slot.status_message = err.clone();
                    if let Some(drawer) = self.sftp_drawers.get_mut(&session_id)
                        && drawer.is_auto_reconnecting()
                    {
                        drawer.set_connection_state(SftpConnectionState::Disconnected {
                            reason: err.clone(),
                        });
                    }
                    self.last_error = Some(err);
                }
                self.sync_sftp_status_notice();
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
                    Some(SessionEntry::Serial(s)) => {
                        s.active.is_none() || !matches!(s.status(), ProtocolStatus::Connected)
                    }
                    Some(SessionEntry::Telnet(s)) => {
                        s.active.is_none() || !matches!(s.status(), ProtocolStatus::Connected)
                    }
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
            SessionEntry::Serial(slot) => {
                if let Some(active) = &slot.active {
                    active.write_bytes(bytes).map_err(|e| e.to_string())
                } else {
                    return;
                }
            }
            SessionEntry::Telnet(slot) => {
                if let Some(active) = &slot.active {
                    active.write_bytes(bytes).map_err(|e| e.to_string())
                } else {
                    return;
                }
            }
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

    fn write_zmodem_wire_bytes(&mut self, session_id: SessionId, bytes: &[u8]) -> Result<(), String> {
        let Some(entry) = self.sessions.get_mut(&session_id) else {
            return Err(format!("Session {} not found", session_id.0));
        };
        match entry {
            SessionEntry::Local(session) => session.pty.write_bytes(bytes).map_err(|e| e.to_string()),
            SessionEntry::Ssh(slot) => {
                if let Some(sup) = &slot.supervisor {
                    sup.write_bytes(bytes).map_err(|e| e.to_string())
                } else if let Some(ssh) = &slot.active {
                    ssh.write_bytes(bytes).map_err(|e| e.to_string())
                } else {
                    Err("SSH session is disconnected".to_string())
                }
            }
            SessionEntry::Serial(slot) => {
                if let Some(active) = &slot.active {
                    active.send_raw_bytes(bytes).map_err(|e| e.to_string())
                } else {
                    Err("Serial port is disconnected".to_string())
                }
            }
            SessionEntry::Telnet(slot) => {
                if let Some(active) = &slot.active {
                    active.write_raw_bytes(bytes).map_err(|e| e.to_string())
                } else {
                    Err("Telnet session is disconnected".to_string())
                }
            }
        }
    }

    /// Renders the workspace tab strip, collapsible Left Sidebar, status/reload/Vault/SSH banners,
    /// active split panes, Status Bar, Search/Paste overlays, Command Palette, and Close Guard modal.
    pub fn view(&self, window_id: window::Id) -> Element<'_, Message, Theme, iced::Renderer> {
        let palette = ThemePalette::for_name_with_overrides(
            &self.config.terminal.theme,
            &self.config.terminal.theme_overrides,
        );
        let chrome = ChromePalette::from_palette(&palette);
        let ws = self.window_workspaces.get(&window_id).map(|w| &w.workspace);
        let is_quake = Some(window_id) == self.quake.window_id();
        let (win_w, win_h, maximized) = self
            .window_workspaces
            .get(&window_id)
            .map(|w| (w.width as f32, w.height as f32, w.maximized))
            .unwrap_or((
                self.config.window.width as f32,
                self.config.window.height as f32,
                false,
            ));

        let mut content_col = column![].width(Length::Fill).height(Length::Fill);

        if let Some(banner_text) = ws.and_then(Self::broadcast_banner_for_workspace) {
            content_col = content_col.push(render_broadcast_banner(banner_text, chrome));
        }

        if let Some(mitm) = self.ssh_ui.view_mitm_alerts(&palette) {
            content_col = content_col.push(mitm);
        }
        if let Some(tofu) = self.ssh_ui.view_tofu_modal(&palette) {
            content_col = content_col.push(tofu);
        }
        if let Some(auth_prompt) = self.ssh_ui.view_auth_prompt(&palette) {
            content_col = content_col.push(auth_prompt);
        }

        if let Some(toasts) = self.reload_ui.view_reload_toasts(&palette) {
            content_col = content_col.push(toasts);
        }
        if let Some(vault_bar) = self.reload_ui.view_vault_banner(&self.vault, &palette) {
            content_col = content_col.push(vault_bar);
        }

        let tab_pos = self.config.terminal.tab_bar_position;
        let vertical_tabs = matches!(tab_pos, TabBarPosition::Left | TabBarPosition::Right);
        let sidebar_w = sidebar_width_for_window(win_w);
        let vertical_rail_w = if vertical_tabs {
            if ViewportTier::from_width(win_w) == ViewportTier::Compact {
                148.0
            } else {
                192.0
            }
        } else {
            0.0
        };
        let active_ssh_sid = Self::active_ssh_session_in(&self.sessions, ws);
        let active_sftp_drawer = self.sftp_drawer_for_workspace(ws);
        let sftp_est_w = if active_sftp_drawer.is_open() {
            active_sftp_drawer
                .width()
                .min((win_w - sidebar_w).max(200.0) * 0.50)
        } else {
            0.0
        };
        let workspace_w = (win_w - sidebar_w - vertical_rail_w - sftp_est_w).max(200.0);
        let strip_w = (win_w - sidebar_w - sftp_est_w).max(200.0);

        let has_connected_ssh = active_ssh_sid.is_some_and(|sid| {
            self.sessions
                .get(&sid)
                .and_then(SessionEntry::as_ssh)
                .is_some_and(|slot| slot.active.is_some())
        });
        let dual_sftp_open = active_sftp_drawer.is_open()
            && active_sftp_drawer.plus.layout_mode != SftpLayoutMode::SideDock;
        let active_sftp_bookmark_key = active_ssh_sid
            .and_then(|sid| {
                self.sessions
                    .get(&sid)
                    .and_then(SessionEntry::as_ssh)
                    .and_then(|slot| slot.target.profile_id.as_deref())
            })
            .or_else(|| active_sftp_drawer.session_label());
        let body = if dual_sftp_open {
            render_sftp_plus_commander(
                active_sftp_drawer,
                &self.sftp_state,
                active_sftp_bookmark_key,
                has_connected_ssh,
                workspace_w,
                &palette,
            )
        } else {
            match ws.and_then(|w| w.active_tab()) {
                Some(tab) => self.render_tab_body(tab),
                None => self.render_empty_workspace(&palette, workspace_w, win_h),
            }
        };
        content_col = content_col.push(body);

        let tab_strip = self.render_tab_strip(ws, &palette, vertical_tabs, win_w, strip_w);

        let top_system_bar = render_top_system_title_bar(
            window_id,
            self.title(window_id),
            self.render_system_bar_controls(window_id, is_quake, maximized, chrome),
            TAB_BAR_HEIGHT,
            chrome,
        );
        let docked_workspace: Element<'_, Message, Theme, iced::Renderer> = match tab_pos {
            TabBarPosition::Top => column![tab_strip, content_col]
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            TabBarPosition::Bottom => column![content_col, tab_strip]
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            TabBarPosition::Left => row![tab_strip, content_col]
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            TabBarPosition::Right => row![content_col, tab_strip]
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        };

        let gated_workspace: Element<'_, Message, Theme, iced::Renderer> =
            CapturedEventGate::new(docked_workspace);
        let right_of_sidebar: Element<'_, Message, Theme, iced::Renderer> =
            if active_sftp_drawer.is_open()
                && active_sftp_drawer.plus.layout_mode == SftpLayoutMode::SideDock
            {
                SftpDrawerDock::new(
                    active_sftp_drawer.width(),
                    gated_workspace,
                    render_sftp_drawer(active_sftp_drawer, has_connected_ssh, &palette),
                )
            } else {
                gated_workspace
            };

        let sidebar_sessions = self.sidebar_sessions(ws);

        let workspace_row: Element<'_, Message, Theme, iced::Renderer> = row![
            render_sidebar_full(
                &self.sidebar,
                &self.profiles.profiles,
                &self.profiles.groups,
                &sidebar_sessions,
                Some(&self.drag_drop),
                !self.has_modal_open(),
                &palette,
                win_w,
            ),
            right_of_sidebar,
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

        let mut base_col = column![top_system_bar, hairline_h(chrome), workspace_row]
            .width(Length::Fill)
            .height(Length::Fill);
        if self.config.terminal.show_status_bar {
            let status_snapshot = self.status_bar_snapshot_for_workspace(ws);
            base_col = base_col.push(render_status_bar_responsive(&status_snapshot, &palette, win_w));
        }

        let base_layout: Element<'_, Message, Theme, iced::Renderer> = container(
            base_col
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(chrome.term_bg)),
            text_color: Some(chrome.text_primary),
            ..Default::default()
        })
        .into();

        let mut layers = stack![base_layout]
            .width(Length::Fill)
            .height(Length::Fill);
        if self.search.open {
            layers = layers.push(search_bar_view_responsive(&self.search, &palette, workspace_w));
        }
        if let Some(pending) = &self.pending_paste {
            layers = layers.push(paste_confirm_modal_view_responsive(pending, &palette, win_w));
        }
        if self.tabby_import.is_open() {
            layers = layers.push(self.tabby_import.view_responsive(
                &palette,
                self.vault.requires_unlock(),
                win_w,
                win_h,
            ));
        }
        if let Some(conflict_drawer) = self.active_conflict_drawer()
            && let Some(conflict_modal) =
                render_conflict_resolution_modal_responsive(conflict_drawer, &palette, win_w)
        {
            layers = layers.push(conflict_modal);
        }
        if let Some(sftp_modal) =
            render_sftp_plus_modal(active_sftp_drawer, win_w, win_h, &palette)
        {
            layers = layers.push(sftp_modal);
        }
        if let Some(sftp_menu) = render_sftp_context_menu_overlay(
            active_sftp_drawer,
            has_connected_ssh,
            win_w,
            win_h,
            &palette,
        ) {
            layers = layers.push(sftp_menu);
        }
        let adhoc_ssh_sessions: Vec<(SessionId, String)> = self
            .sessions
            .iter()
            .filter_map(|(&sid, entry)| {
                entry
                    .as_ssh()
                    .filter(|slot| slot.target.profile_id.is_none())
                    .map(|slot| (sid, slot.title.clone()))
            })
            .collect();
        if let Some(tunnel_modal) = self.tunnel_ui.view_create_modal_responsive(
            &self.profiles.profiles,
            &adhoc_ssh_sessions,
            &palette,
            win_w,
            win_h,
        ) {
            layers = layers.push(tunnel_modal);
        }
        if let Some(bind_warning) = self
            .tunnel_ui
            .view_public_bind_warning_responsive(&palette, win_w)
        {
            layers = layers.push(bind_warning);
        }
        if let Some(profile_modal) = self.profile_editor.view_responsive(&palette, win_w, win_h) {
            layers = layers.push(profile_modal);
        }
        if let Some(settings_modal) =
            self.settings_ui
                .view_responsive(&palette, &self.vault, win_w, win_h)
        {
            layers = layers.push(settings_modal);
        }
        if let Some(qc_modal) =
            self.quick_commands_ui
                .view_responsive(&self.quick_commands, &palette, win_w, win_h)
        {
            layers = layers.push(qc_modal);
        }
        let active_tab_count = ws.map(|w| w.tab_count()).unwrap_or(0);
        if let Some(ws_modal) =
            self.workspaces_ui
                .view_responsive(&self.workspaces_cfg, active_tab_count, &palette, win_w, win_h)
        {
            layers = layers.push(ws_modal);
        }
        if let Some(proto_modal) = self
            .protocol_launcher
            .view_responsive(&palette, win_w, win_h)
        {
            layers = layers.push(proto_modal);
        }
        if let Some(zmodem_modal) = self.zmodem_send_ui.view_responsive(&palette, win_w) {
            layers = layers.push(zmodem_modal);
        }
        if self.command_palette.is_open() {
            let items = self.command_palette_items();
            layers = layers.push(render_command_palette_responsive(
                &self.command_palette,
                items,
                self.last_error(),
                &palette,
                win_w,
                win_h,
            ));
        }
        if let Some(menu_overlay) =
            render_context_menu_overlay(&self.context_menu, win_w, win_h, &palette)
        {
            layers = layers.push(menu_overlay);
        }
        if let Some(drag_overlay) = render_drag_overlay(&self.drag_drop, win_w, win_h, &palette) {
            layers = layers.push(drag_overlay);
        }

        let active_tab = ws.and_then(|w| w.active_tab_id());
        let active_session = ws
            .and_then(|w| w.active_tab())
            .and_then(|t| t.focused_session_id());
        let intercepted = ShortcutInterceptor::with_keybindings(
            layers.into(),
            active_tab,
            active_session,
            self.command_palette.is_open(),
            self.settings_ui.capturing_action().is_some(),
            Arc::clone(&self.keybinding_resolver),
            self.pending_key_chords.clone(),
        );
        let coordinated = DragCoordinator::new(self.drag_drop.is_dragging(), intercepted);
        let framed = BorderlessResizeFrame::new(
            window_id,
            !is_quake && !maximized && !self.drag_drop.is_dragging(),
            coordinated,
        );
        let scoped = framed.map(move |msg| Message::InWindow {
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
            &palette,
        )
    }

    fn render_tab_strip(
        &self,
        ws: Option<&Workspace>,
        palette: &ThemePalette,
        vertical: bool,
        window_width: f32,
        strip_width: f32,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let chrome = ChromePalette::from_palette(palette);
        let tier = ViewportTier::from_width(window_width);
        let sidebar_w = sidebar_width_for_window(window_width);
        let active_tab_id = ws.and_then(|w| w.active_tab_id());
        let hide_index = self.config.terminal.hide_tab_index;
        let flex_tabs = self.config.terminal.flex_tabs;
        let tab_pos = self.config.terminal.tab_bar_position;
        let is_bottom = tab_pos == TabBarPosition::Bottom;

        let mut tab_elements: Vec<Element<'_, Message, Theme, iced::Renderer>> = Vec::new();
        if let Some(ws) = ws {
            for (idx, tab) in ws.tabs().iter().enumerate() {
                let is_active = Some(tab.id) == active_tab_id;
                let label = self.tab_display_title(tab);
                let tab_accent_opt = tab.color.as_deref().and_then(parse_hex_color);
                let index_num = idx + 1;

                let mut inner_row = row![]
                    .spacing(6)
                    .padding(Padding {
                        top: 0.0,
                        right: 4.0,
                        bottom: 0.0,
                        left: 10.0,
                    })
                    .height(Length::Fill)
                    .align_y(Alignment::Center);

                if !hide_index {
                    let index_color = Color {
                        a: if is_active { 0.55 } else { 0.38 },
                        ..chrome.text_primary
                    };
                    inner_row = inner_row.push(
                        container(
                            text(format!("{index_num}"))
                                .size(12.0)
                                .font(ui_font_semibold())
                                .color(index_color),
                        )
                        .width(Length::Fixed(18.0))
                        .center_x(Length::Fixed(18.0)),
                    );
                }

                if tab.pinned {
                    inner_row = inner_row.push(
                        text("•")
                            .size(12.0)
                            .font(ui_font_semibold())
                            .color(chrome.accent),
                    );
                }

                inner_row = inner_row.push(
                    button(
                        container(
                            text(label.clone())
                                .size(13.0)
                                .font(ui_font())
                                .wrapping(iced::widget::text::Wrapping::None)
                                .color(if is_active {
                                    chrome.text_primary
                                } else {
                                    chrome.text_muted
                                }),
                        )
                        .width(Length::Fill)
                        .clip(true),
                    )
                    .padding([0, 2])
                    .width(Length::Fill)
                    .style(tab_button_style(chrome, is_active))
                    .on_press(Message::SelectTab(tab.id)),
                );

                // Tab options ("...") button opening Tabby's tab context menu.
                let tab_w = if flex_tabs || tier == ViewportTier::Compact {
                    160.0
                } else {
                    200.0
                };
                let menu_x = if vertical {
                    if tab_pos == TabBarPosition::Right {
                        (window_width - 220.0).max(40.0)
                    } else {
                        (sidebar_w + 140.0).min((window_width - 120.0).max(40.0))
                    }
                } else {
                    (sidebar_w + idx as f32 * tab_w + (tab_w - 60.0))
                        .min((window_width - 120.0).max(40.0))
                };
                let menu_y = if is_bottom {
                    280.0
                } else if vertical {
                    TAB_BAR_HEIGHT + 1.0 + 40.0
                } else {
                    TAB_BAR_HEIGHT * 2.0 + 1.0
                };
                inner_row = inner_row.push(
                    button(text("⋯").size(13.0).font(ui_font()))
                        .padding([2, 5])
                        .style(tab_bar_button_style(chrome, false, false))
                        .on_press(Message::OpenTabContextMenu {
                            tab_id: tab.id,
                            position: Point::new(menu_x, menu_y),
                        }),
                );

                if !tab.pinned {
                    inner_row = inner_row.push(
                        button(text("×").size(14.0).font(ui_font()))
                            .padding([2, 6])
                            .style(tab_bar_button_style(chrome, false, false))
                            .on_press(Message::CloseTab(tab.id)),
                    );
                }

                let progress_bar = match tab.progress {
                    TerminalProgressState::Hidden => None,
                    TerminalProgressState::Normal(pct) => {
                        Some((chrome.accent, pct.min(100) as f32 / 100.0))
                    }
                    TerminalProgressState::Indeterminate => Some((chrome.accent, 0.45)),
                    TerminalProgressState::Error(pct) => {
                        Some((chrome.danger, pct.clamp(10, 100) as f32 / 100.0))
                    }
                    TerminalProgressState::Paused(pct) => {
                        Some((chrome.warning, pct.clamp(10, 100) as f32 / 100.0))
                    }
                };

                // Top strip: OSC 9;4 progressbar OR subtle 1.5px active tab highlight (flat on Tabby dark)
                let top_strip: Element<'_, Message, Theme, iced::Renderer> =
                    if let Some((bar_color, fraction)) = progress_bar {
                        let fill_pct = ((fraction * 100.0).round() as u16).max(1);
                        let empty_pct = 100_u16.saturating_sub(fill_pct);
                        let mut bar_row = row![
                            container(space::horizontal().height(3))
                                .width(Length::FillPortion(fill_pct))
                                .style(move |_| container::Style {
                                    background: Some(Background::Color(bar_color)),
                                    ..Default::default()
                                })
                        ]
                        .width(Length::Fill);
                        if empty_pct > 0 {
                            bar_row = bar_row
                                .push(space::horizontal().width(Length::FillPortion(empty_pct)));
                        }
                        bar_row.into()
                    } else if is_active && (tab_accent_opt.is_some() || chrome.is_light) {
                        let ind_color = tab_accent_opt.unwrap_or(chrome.active_tab_indicator);
                        container(space::horizontal().height(2))
                            .width(Length::Fill)
                            .style(move |_| container::Style {
                                background: Some(Background::Color(ind_color)),
                                ..Default::default()
                            })
                            .into()
                    } else {
                        space::horizontal().height(2).into()
                    };

                // Bottom strip: Tabby 3px `.colorbar` OR 2px `.activity-indicator`
                let bottom_strip: Element<'_, Message, Theme, iced::Renderer> =
                    if let Some(c) = tab_accent_opt {
                        container(space::horizontal().height(3))
                            .width(Length::Fill)
                            .style(move |_| container::Style {
                                background: Some(Background::Color(c)),
                                ..Default::default()
                            })
                            .into()
                    } else if !is_active && tab.bell_active {
                        let act_color = Color {
                            a: 0.42,
                            ..chrome.text_primary
                        };
                        container(
                            container(space::horizontal().height(2))
                                .width(Length::Fill)
                                .style(move |_| container::Style {
                                    background: Some(Background::Color(act_color)),
                                    ..Default::default()
                                }),
                        )
                        .padding([0, 10])
                        .width(Length::Fill)
                        .into()
                    } else {
                        space::horizontal().height(2).into()
                    };

                let tab_col = column![top_strip, inner_row, bottom_strip]
                    .width(Length::Fill)
                    .height(Length::Fixed(TAB_BAR_HEIGHT));

                let tab_width = if vertical {
                    Length::Fill
                } else if flex_tabs {
                    Length::Shrink
                } else if tier == ViewportTier::Compact {
                    Length::Fixed(160.0)
                } else {
                    Length::Fixed(200.0)
                };

                let styled_tab = container(tab_col)
                    .width(tab_width)
                    .height(Length::Fixed(TAB_BAR_HEIGHT))
                    .style(move |_theme| tab_container_style_tinted(chrome, is_active, tab_accent_opt));

                let is_dragging_this = matches!(
                    self.drag_drop.active_payload(),
                    Some(DragPayload::Tab { tab_id: dragged_id, .. }) if *dragged_id == tab.id
                );
                let accepting_tab_drag = matches!(
                    self.drag_drop.active_payload(),
                    Some(DragPayload::Tab { tab_id: dragged_id, .. }) if *dragged_id != tab.id
                );
                let hovered_slot = match self.drag_drop.hovered_target() {
                    Some(DropTarget::TabSlot { tab_id: hid, after }) => Some((*hid, *after)),
                    _ => None,
                };
                let chip = TabRightClickCapture::new(tab.id, styled_tab.into());
                let draggable_chip = DragSource::new(
                    DragPayload::Tab {
                        tab_id: tab.id,
                        label: label.clone(),
                    },
                    !self.has_modal_open(),
                    is_dragging_this,
                    chip,
                );
                tab_elements.push(DropTargetZone::new(
                    DropZoneKind::TabSlot {
                        tab_id: tab.id,
                        vertical,
                        accepting_tab_drag,
                        hovered_slot,
                        accent: chrome.accent,
                    },
                    draggable_chip,
                ));
            }
        }

        let active_zoomed = ws
            .and_then(|w| w.active_tab())
            .is_some_and(|t| t.zoomed_pane.is_some());
        let active_broadcast = ws
            .and_then(|w| w.active_tab())
            .is_some_and(|t| t.broadcast_input);

        let new_tab_btn = button(
            text("+")
                .size(17.0)
                .font(ui_font()),
        )
        .padding([0, 12])
        .height(Length::Fixed(TAB_BAR_HEIGHT))
        .style(tab_bar_button_style(chrome, false, false))
        .on_press(Message::NewTab);

        let accepting_strip_drag = self.drag_drop.accepts(&DropTarget::TabStripEnd);
        let hovered_strip = matches!(self.drag_drop.hovered_target(), Some(DropTarget::TabStripEnd));

        if vertical {
            let rail_w = if tier == ViewportTier::Compact { 148.0 } else { 192.0 };
            let mut vcol = column![].spacing(0).width(Length::Fill);
            for el in tab_elements {
                vcol = vcol.push(el);
            }
            vcol = vcol.push(
                button(text("+ New terminal").size(12.5).font(ui_font()))
                    .padding([8, 12])
                    .width(Length::Fill)
                    .style(tab_bar_button_style(chrome, false, false))
                    .on_press(Message::NewTab),
            );
            let rail_scroll = DropTargetZone::new(
                DropZoneKind::TabStripBar {
                    accepting_drag: accepting_strip_drag,
                    hovered_strip,
                    accent: chrome.accent,
                    soft_bg: chrome.accent_soft,
                },
                scrollable(vcol)
                    .height(Length::Fill)
                    .width(Length::Fill)
                    .style(tabby_scrollable_style(chrome))
                    .into(),
            );
            let rail = column![rail_scroll]
                .spacing(0)
                .width(Length::Fixed(rail_w))
                .height(Length::Fill);

            let rail_box = container(rail)
                .height(Length::Fill)
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(chrome.chrome_bg)),
                    ..Default::default()
                });

            return match self.config.terminal.tab_bar_position {
                TabBarPosition::Left => row![rail_box, hairline_v(chrome)]
                    .height(Length::Fill)
                    .into(),
                _ => row![hairline_v(chrome), rail_box]
                    .height(Length::Fill)
                    .into(),
            };
        }

        let mut tabs_row = row![].spacing(0).align_y(Alignment::Center);
        for el in tab_elements {
            tabs_row = tabs_row.push(el);
        }

        let show_zoomed = active_tab_id.is_some() && active_zoomed;
        let show_broadcast = active_tab_id.is_some() && active_broadcast;

        let mut right_toolbar = row![].spacing(0).align_y(Alignment::Center);
        if show_zoomed {
            right_toolbar = right_toolbar.push(
                button(text("Zoomed").size(11.5).font(ui_font_semibold()))
                    .padding([0, 8])
                    .height(Length::Fixed(TAB_BAR_HEIGHT))
                    .style(tab_bar_button_style(chrome, true, false))
                    .on_press(Message::TogglePaneZoom),
            );
        }
        if show_broadcast {
            right_toolbar = right_toolbar.push(
                button(text("Broadcast").size(11.5).font(ui_font_semibold()))
                    .padding([0, 8])
                    .height(Length::Fixed(TAB_BAR_HEIGHT))
                    .style(tab_bar_button_style(chrome, true, true))
                    .on_press(Message::ToggleBroadcastInput),
            );
        }

        let right_reserve = if show_zoomed { 72.0 } else { 0.0 }
            + if show_broadcast { 84.0 } else { 0.0 };
        let max_tabs_w = (strip_width - 38.0 - right_reserve - 4.0).max(60.0);

        let tabs_scroll = container(DropTargetZone::new(
            DropZoneKind::TabStripBar {
                accepting_drag: accepting_strip_drag,
                hovered_strip,
                accent: chrome.accent,
                soft_bg: chrome.accent_soft,
            },
            scrollable(tabs_row)
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new().width(2).scroller_width(2),
                ))
                .style(tabby_scrollable_style(chrome))
                .into(),
        ))
        .max_width(max_tabs_w);

        let left_toolbar = row![new_tab_btn]
            .spacing(0)
            .align_y(Alignment::Center);
        let middle_spacer: Element<'_, Message, Theme, iced::Renderer> =
            space::horizontal().width(Length::Fill).into();

        let tab_bar_row = row![
            tabs_scroll,
            left_toolbar,
            middle_spacer,
            right_toolbar,
        ]
        .spacing(0)
        .height(Length::Fixed(TAB_BAR_HEIGHT))
        .align_y(Alignment::Center);

        container(tab_bar_row)
            .width(Length::Fill)
            .height(Length::Fixed(TAB_BAR_HEIGHT))
            .style(move |_theme| container::Style {
                background: Some(Background::Color(chrome.bg_more)),
                ..Default::default()
            })
            .into()
    }

    fn render_system_bar_controls(
        &self,
        window_id: window::Id,
        is_quake: bool,
        maximized: bool,
        chrome: ChromePalette,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let settings_btn = button(
            container(text("⚙").size(15.5).font(ui_font()))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .padding(0)
        .width(Length::Fixed(44.0))
        .height(Length::Fixed(TAB_BAR_HEIGHT))
        .style(tab_bar_button_style(chrome, self.settings_ui.is_open(), false))
        .on_press(Message::OpenSettingsModal);

        let mut controls = row![settings_btn]
            .spacing(0)
            .height(Length::Fixed(TAB_BAR_HEIGHT))
            .align_y(Alignment::Center);

        if !is_quake {
            let min_btn = button(
                container(WindowControlIcon::new(WindowControlGlyph::Minimize))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .padding(0)
            .width(Length::Fixed(44.0))
            .height(Length::Fixed(TAB_BAR_HEIGHT))
            .style(tab_bar_button_style(chrome, false, false))
            .on_press(Message::WindowMinimize(window_id));

            let max_glyph = if maximized {
                WindowControlGlyph::Restore
            } else {
                WindowControlGlyph::Maximize
            };
            let max_btn = button(
                container(WindowControlIcon::new(max_glyph))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .padding(0)
            .width(Length::Fixed(44.0))
            .height(Length::Fixed(TAB_BAR_HEIGHT))
            .style(tab_bar_button_style(chrome, false, false))
            .on_press(Message::WindowToggleMaximize(window_id));

            let close_btn = button(
                container(text("✕").size(13.0).font(ui_font()))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .padding(0)
            .width(Length::Fixed(46.0))
            .height(Length::Fixed(TAB_BAR_HEIGHT))
            .style(tab_bar_close_button_style(chrome))
            .on_press(Message::CloseRequested(window_id));

            controls = controls.push(min_btn).push(max_btn).push(close_btn);
        }

        controls.into()
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
        let modal_open = self.has_modal_open();
        let terminal_focused = focused && !modal_open;
        let ime_enabled = !modal_open && !(self.search.open && self.search.input_focused);
        let options = TerminalViewOptions::from_config_with_runtime(
            &self.config.terminal,
            ime_enabled,
            Arc::clone(&self.compiled_highlights),
            self.cursor_visible,
            self.is_visual_bell_active(session_id),
        );
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
            Some(SessionEntry::Ssh(slot)) => {
                let viewport = TerminalViewport::with_focus(
                    slot.id,
                    Arc::clone(&slot.snapshot),
                    options,
                    matches,
                    active_match,
                    terminal_focused,
                );
                let palette = ThemePalette::for_name_with_overrides(
                    &self.config.terminal.theme,
                    &self.config.terminal.theme_overrides,
                );
                if slot.transport_status == TransportStatus::Disconnected {
                    let bar = SshUiState::view_disconnected_ssh_bar(
                        slot.id,
                        &slot.target.host,
                        slot.target.port,
                        self.ssh_ui.mitm_alert(slot.id).is_some(),
                        slot.last_error.as_deref(),
                        &palette,
                    );
                    column![bar, viewport]
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into()
                } else {
                    let chrome = ChromePalette::from_palette(&palette);
                    let dot_color = if slot.is_auto_reconnecting() {
                        chrome.warning
                    } else {
                        match slot.transport_status {
                            TransportStatus::Connected { .. } => chrome.success,
                            TransportStatus::Connecting
                            | TransportStatus::Degraded { .. }
                            | TransportStatus::Reconnecting { .. } => chrome.warning,
                            TransportStatus::Disconnected => chrome.danger,
                        }
                    };
                    let status_label = match &slot.transport_status {
                        TransportStatus::Connected { latency_ms: Some(ms) } => {
                            format!("{} ({ms} ms)", slot.target.display_addr())
                        }
                        TransportStatus::Connected { latency_ms: None } => slot.target.display_addr(),
                        other => format_ssh_slot_message(&slot.target, other),
                    };
                    let mut left_info = row![
                        status_dot(dot_color),
                        text(status_label)
                            .size(FONT_META)
                            .font(ui_font())
                            .color(chrome.text_secondary),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center);

                    if slot.is_auto_reconnecting() {
                        let reconnect_label = match (
                            slot.reconnect_attempt(),
                            &slot.transport_status,
                        ) {
                            (Some(attempt), TransportStatus::Reconnecting { next_retry_secs, .. }) => {
                                format!("Auto-reconnecting #{attempt} ({next_retry_secs}s)")
                            }
                            (Some(attempt), _) => format!("Auto-reconnecting #{attempt}..."),
                            _ => "Auto-reconnecting...".to_string(),
                        };
                        left_info = left_info.push(badge_pill(
                            reconnect_label,
                            chrome.warning,
                            chrome.warning_soft,
                            Color::TRANSPARENT,
                        ));
                        if let Some(err) = &slot.last_error {
                            left_info = left_info.push(
                                text(err.clone()).size(FONT_META).color(chrome.warning),
                            );
                        }
                    } else if slot.is_recently_reconnected() {
                        left_info = left_info.push(badge_pill(
                            "Reconnected successfully",
                            chrome.success,
                            chrome.success_soft,
                            Color::TRANSPARENT,
                        ));
                    }

                    if !slot.target.proxy_jump.is_empty() {
                        left_info = left_info.push(badge_pill(
                            format!("via {}", slot.target.proxy_jump.join(" → ")),
                            chrome.accent,
                            chrome.accent_soft,
                            chrome.border_subtle,
                        ));
                    }

                    let mut right_actions = row![].spacing(4).align_y(Alignment::Center);
                    if slot.is_auto_reconnecting()
                        || matches!(
                            slot.transport_status,
                            TransportStatus::Connecting | TransportStatus::Reconnecting { .. }
                        )
                    {
                        right_actions = right_actions.push(
                            button(text("Cancel").size(FONT_META))
                                .padding([2, 8])
                                .style(tab_bar_button_style(chrome, false, true))
                                .on_press(Message::StatusBarAction(
                                    StatusBarAction::CancelReconnect(slot.id),
                                )),
                        );
                    } else {
                        right_actions = right_actions.push(
                            button(text("↻ Reconnect").size(FONT_META))
                                .padding([2, 8])
                                .style(tab_bar_button_style(chrome, false, false))
                                .on_press(Message::StatusBarAction(
                                    StatusBarAction::ReconnectSession(slot.id),
                                )),
                        );
                    }
                    right_actions = right_actions
                        .push(
                            button(text("SFTP").size(FONT_META))
                                .padding([2, 8])
                                .style(tab_bar_button_style(
                                    chrome,
                                    self.is_sftp_drawer_open_for(slot.id),
                                    false,
                                ))
                                .on_press(Message::ToggleSftpDrawerForSession(slot.id)),
                        )
                        .push(
                            button(text("Ports").size(FONT_META))
                                .padding([2, 8])
                                .style(tab_bar_button_style(chrome, false, false))
                                .on_press(Message::OpenTunnelCreateModal),
                        );

                    let bar = container(
                        row![
                            container(left_info.wrap().vertical_spacing(4))
                                .width(Length::Fill)
                                .clip(true),
                            right_actions,
                        ]
                        .spacing(10)
                        .padding([5, 14])
                        .align_y(Alignment::Start),
                    )
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(chrome.term_bg)),
                        border: Border::default(),
                        text_color: Some(chrome.text_primary),
                        ..Default::default()
                    });
                    let mut col = column![bar, viewport];
                    if self.config.terminal.show_server_stats && !slot.server_stats_unsupported {
                        col = col.push(render_server_stats_bar(
                            slot.server_stats.as_ref(),
                            &slot.transport_status,
                            &palette,
                        ));
                    }
                    col.width(Length::Fill).height(Length::Fill).into()
                }
            }
            Some(SessionEntry::Serial(slot)) => {
                let viewport = TerminalViewport::with_focus(
                    slot.id,
                    Arc::clone(&slot.snapshot),
                    options,
                    matches,
                    active_match,
                    terminal_focused,
                );
                let palette = ThemePalette::for_name_with_overrides(
                    &self.config.terminal.theme,
                    &self.config.terminal.theme_overrides,
                );
                let chrome = ChromePalette::from_palette(&palette);
                let status = slot.status();
                let is_auto_reconnecting = slot.is_auto_reconnecting();
                let dot_color = if is_auto_reconnecting {
                    chrome.warning
                } else {
                    match status {
                        ProtocolStatus::Connected => chrome.success,
                        ProtocolStatus::Connecting | ProtocolStatus::Reconnecting { .. } => {
                            chrome.warning
                        }
                        ProtocolStatus::Disconnected => chrome.danger,
                    }
                };
                let conn_btn = match status {
                    ProtocolStatus::Connected
                    | ProtocolStatus::Connecting
                    | ProtocolStatus::Reconnecting { .. } => {
                        button(text("Disconnect").size(FONT_META))
                            .padding([3, 8])
                            .style(tab_bar_button_style(chrome, false, true))
                            .on_press(Message::SerialDisconnect(slot.id))
                    }
                    ProtocolStatus::Disconnected => {
                        button(text("Reconnect").size(FONT_META).font(ui_font_medium()))
                            .padding([3, 8])
                            .style(tab_bar_button_style(chrome, true, false))
                            .on_press(Message::SerialReconnect(slot.id))
                    }
                };
                let mut serial_row = row![
                    status_dot(dot_color),
                    text(format!(
                        "{} @ {} ({})",
                        slot.config.port,
                        slot.config.baud_rate,
                        status.label()
                    ))
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_secondary),
                ]
                .spacing(6)
                .padding([4, 10])
                .align_y(Alignment::Center);
                if is_auto_reconnecting {
                    let badge_label = match slot.reconnect_attempt() {
                        Some(n) if n > 0 => format!("Auto-reconnecting #{n}..."),
                        _ => "Auto-reconnecting...".to_string(),
                    };
                    serial_row = serial_row.push(badge_pill(
                        badge_label,
                        chrome.warning,
                        chrome.warning_soft,
                        Color::TRANSPARENT,
                    ));
                    if let Some(err) = slot.last_error() {
                        serial_row =
                            serial_row.push(text(err.to_string()).size(FONT_META).color(chrome.warning));
                    }
                } else if slot.is_recently_reconnected() {
                    serial_row = serial_row.push(badge_pill(
                        "Reconnected successfully",
                        chrome.success,
                        chrome.success_soft,
                        Color::TRANSPARENT,
                    ));
                }
                let bar = container(
                    serial_row
                        .push(
                            button(text("Text").size(FONT_META))
                                .padding([3, 8])
                                .style(tab_bar_button_style(
                                    chrome,
                                    slot.config.input_mode == SerialInputMode::Text,
                                    false,
                                ))
                                .on_press(Message::SerialSetInputMode {
                                    session_id: slot.id,
                                    mode: SerialInputMode::Text,
                                }),
                        )
                        .push(
                            button(text("Readline").size(FONT_META))
                                .padding([3, 8])
                                .style(tab_bar_button_style(
                                    chrome,
                                    slot.config.input_mode == SerialInputMode::Readline,
                                    false,
                                ))
                                .on_press(Message::SerialSetInputMode {
                                    session_id: slot.id,
                                    mode: SerialInputMode::Readline,
                                }),
                        )
                        .push(
                            button(text("HexOut").size(FONT_META))
                                .padding([3, 8])
                                .style(tab_bar_button_style(
                                    chrome,
                                    slot.config.output_mode == SerialOutputMode::Hexdump,
                                    false,
                                ))
                                .on_press(Message::SerialSetOutputMode {
                                    session_id: slot.id,
                                    mode: if slot.config.output_mode == SerialOutputMode::Hexdump {
                                        SerialOutputMode::Text
                                    } else {
                                        SerialOutputMode::Hexdump
                                    },
                                }),
                        )
                        .push(
                            iced::widget::text_input("AA 55 0D", &slot.hex_draft)
                                .on_input(move |v| Message::SerialHexDraftChanged {
                                    session_id: slot.id,
                                    value: v,
                                })
                                .on_submit(Message::SerialSendHex(slot.id))
                                .font(mono_font())
                                .size(FONT_META)
                                .padding([3, 7])
                                .style(styled_input_style(chrome))
                                .width(Length::Fixed(104.0)),
                        )
                        .push(
                            button(text("Send Hex").size(FONT_META))
                                .padding([3, 8])
                                .style(tab_bar_button_style(chrome, false, false))
                                .on_press(Message::SerialSendHex(slot.id)),
                        )
                        .push(space::horizontal())
                        .push(conn_btn)
                        .wrap()
                        .vertical_spacing(4),
                )
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(chrome.bg_more)),
                    border: Border {
                        color: chrome.secondary,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    text_color: Some(chrome.text_primary),
                    ..Default::default()
                });
                column![bar, viewport]
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
            Some(SessionEntry::Telnet(slot)) => {
                let viewport = TerminalViewport::with_focus(
                    slot.id,
                    Arc::clone(&slot.snapshot),
                    options,
                    matches,
                    active_match,
                    terminal_focused,
                );
                let status = slot.status();
                let is_auto_reconnecting = slot.is_auto_reconnecting();
                let palette = ThemePalette::for_name_with_overrides(
                    &self.config.terminal.theme,
                    &self.config.terminal.theme_overrides,
                );
                let chrome = ChromePalette::from_palette(&palette);
                let dot_color = if is_auto_reconnecting {
                    chrome.warning
                } else {
                    match status {
                        ProtocolStatus::Connected => chrome.success,
                        ProtocolStatus::Connecting | ProtocolStatus::Reconnecting { .. } => {
                            chrome.warning
                        }
                        ProtocolStatus::Disconnected => chrome.danger,
                    }
                };
                let mut bar_row = row![
                    status_dot(dot_color),
                    text(format!(
                        "Telnet {}:{} ({})",
                        slot.config.host,
                        slot.config.port,
                        status.label()
                    ))
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_secondary),
                ]
                .spacing(8)
                .padding([4, 10])
                .align_y(Alignment::Center);
                if is_auto_reconnecting {
                    let badge_label = match slot.reconnect_attempt() {
                        Some(n) if n > 0 => format!("Auto-reconnecting #{n}..."),
                        _ => "Auto-reconnecting...".to_string(),
                    };
                    bar_row = bar_row.push(badge_pill(
                        badge_label,
                        chrome.warning,
                        chrome.warning_soft,
                        Color::TRANSPARENT,
                    ));
                    if let Some(err) = slot.last_error() {
                        bar_row = bar_row
                            .push(text(err.to_string()).size(FONT_META).color(chrome.warning));
                    }
                } else if slot.is_recently_reconnected() {
                    bar_row = bar_row.push(badge_pill(
                        "Reconnected successfully",
                        chrome.success,
                        chrome.success_soft,
                        Color::TRANSPARENT,
                    ));
                }
                bar_row = bar_row.push(space::horizontal());
                if matches!(status, ProtocolStatus::Disconnected) {
                    bar_row = bar_row.push(
                        button(text("Reconnect").size(FONT_META).font(ui_font_medium()))
                            .padding([3, 8])
                            .style(tab_bar_button_style(chrome, true, false))
                            .on_press(Message::TelnetReconnect(slot.id)),
                    );
                }
                let bar = container(bar_row.wrap().vertical_spacing(4))
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(chrome.bg_more)),
                        border: Border {
                            color: chrome.secondary,
                            width: 1.0,
                            radius: 0.0.into(),
                        },
                        text_color: Some(chrome.text_primary),
                        ..Default::default()
                    });
                column![bar, viewport]
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
            None => container("Missing terminal session")
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        };

        let pane_palette = ThemePalette::for_name_with_overrides(
            &self.config.terminal.theme,
            &self.config.terminal.theme_overrides,
        );
        let frame = PaneFrame::new(pane_id, terminal_focused, broadcast, show_border, content);
        self.wrap_workspace_pane_drop_zone(Some(pane_id), &pane_palette, frame)
    }

    fn wrap_workspace_pane_drop_zone<'a>(
        &self,
        pane_id: Option<PaneId>,
        palette: &ThemePalette,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let chrome = ChromePalette::from_palette(palette);
        let accepting_drag = self.drag_drop.accepts(&DropTarget::WorkspacePane {
            pane_id,
            split_axis: None,
        });
        let hovered_zone = match self.drag_drop.hovered_target() {
            Some(DropTarget::WorkspacePane {
                pane_id: hid,
                split_axis,
            }) => Some((*hid, *split_axis)),
            _ => None,
        };
        DropTargetZone::new(
            DropZoneKind::WorkspacePane {
                pane_id,
                accepting_drag,
                hovered_zone,
                accent: chrome.accent,
                soft_bg: chrome.accent_soft,
            },
            content,
        )
    }

    fn render_empty_workspace(
        &self,
        palette: &ThemePalette,
        workspace_width: f32,
        window_height: f32,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let chrome = ChromePalette::from_palette(palette);
        let col_w = (workspace_width - 32.0).clamp(220.0, 400.0);

        // Matches Tabby's `start-page` (`startPage.component.pug` / `.scss`):
        // centered thin title, flat `.list-group` primary actions, recent saved profiles,
        // and a full-width subtle bottom footer bar.
        let header = column![
            text("Termewnal")
                .size(if workspace_width < 420.0 { 30.0 } else { 40.0 })
                .font(ui_font())
                .color(chrome.text_primary),
        ]
        .padding(Padding {
            top: if window_height < 580.0 { 16.0 } else { 42.0 },
            right: 0.0,
            bottom: 18.0,
            left: 0.0,
        })
        .align_x(Alignment::Center)
        .width(Length::Fill);

        let action_row = |label: &'static str, hint: Option<&'static str>, msg: Message| {
            let mut inner = row![
                text(label).size(FONT_BODY).color(chrome.text_primary),
                space::horizontal(),
            ]
            .align_y(Alignment::Center)
            .width(Length::Fill);
            if let Some(h) = hint {
                inner = inner.push(keycap_pill(h, chrome));
            }
            button(inner)
                .width(Length::Fill)
                .padding([9, 14])
                .style(list_row_button_style(chrome, false))
                .on_press(msg)
        };

        let mut list_group = column![
            action_row("Open a new terminal", Some("Ctrl+Shift+T"), Message::NewTab),
            hairline_h(chrome),
            action_row("Quick SSH connect", None, Message::OpenQuickConnectPalette),
            hairline_h(chrome),
            action_row("New profile", None, Message::OpenNewProfileModal),
            hairline_h(chrome),
            action_row("Settings", Some("Ctrl+,"), Message::OpenSettingsModal),
            hairline_h(chrome),
            action_row("Import Tabby config", None, Message::OpenTabbyImportModal),
        ]
        .width(Length::Fill);

        if !self.profiles.profiles.is_empty() {
            list_group = list_group.push(
                container(
                    text("Recent profiles")
                        .size(FONT_META)
                        .font(ui_font_medium())
                        .color(chrome.text_muted),
                )
                .padding(Padding {
                    top: 14.0,
                    right: 14.0,
                    bottom: 6.0,
                    left: 14.0,
                }),
            );
            for (idx, profile) in self.profiles.profiles.iter().take(3).enumerate() {
                if idx > 0 {
                    list_group = list_group.push(hairline_h(chrome));
                }
                let dot_color = match profile.target {
                    ProfileTarget::Ssh { .. } | ProfileTarget::Docker { .. } => chrome.accent,
                    ProfileTarget::LocalShell { .. } => chrome.success,
                    ProfileTarget::Serial { .. } | ProfileTarget::Telnet { .. } => chrome.warning,
                };
                let target = profile_target_summary(profile);
                let pid_connect = profile.id.clone();
                let pid_edit = profile.id.clone();

                let profile_btn = button(
                    row![
                        status_dot(dot_color),
                        container(
                            column![
                                text(profile.name.as_str())
                                    .size(FONT_BODY)
                                    .wrapping(iced::widget::text::Wrapping::None)
                                    .color(chrome.text_primary),
                                text(target)
                                    .size(FONT_META)
                                    .font(mono_font())
                                    .wrapping(iced::widget::text::Wrapping::None)
                                    .color(chrome.text_muted),
                            ]
                            .spacing(1),
                        )
                        .width(Length::Fill)
                        .clip(true),
                        text("Connect").size(FONT_META).color(chrome.accent),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([8, 12])
                .style(list_row_button_style(chrome, false))
                .on_press(Message::LaunchProfile {
                    profile_id: pid_connect,
                    disposition: LaunchDisposition::NewTab,
                });

                list_group = list_group.push(
                    row![
                        profile_btn,
                        button(text("⋯").size(FONT_SUBHEAD).color(chrome.text_muted))
                            .padding([8, 10])
                            .style(tab_bar_button_style(chrome, false, false))
                            .on_press(Message::OpenEditProfileModal(pid_edit)),
                    ]
                    .align_y(Alignment::Center)
                    .width(Length::Fill),
                );
            }
        }

        let center_box = container(column![header, list_group].width(Length::Fixed(col_w)))
            .width(Length::Fill)
            .center_x(Length::Fill)
            .padding(Padding {
                top: 12.0,
                right: 16.0,
                bottom: 24.0,
                left: 16.0,
            });

        let footer_bg = if chrome.is_light {
            Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.05,
            }
        } else {
            Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.45,
            }
        };

        let footer = container(
            row![
                row![
                    keycap_pill("Ctrl+K", chrome),
                    text("Commands").size(FONT_META).color(chrome.text_muted),
                    keycap_pill("Ctrl+,", chrome),
                    text("Settings").size(FONT_META).color(chrome.text_muted),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .wrap()
                .vertical_spacing(4),
                space::horizontal(),
                text(format!("Version: {}", env!("CARGO_PKG_VERSION")))
                    .size(FONT_META)
                    .color(chrome.text_muted),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding([10, 20])
        .style(move |_| container::Style {
            background: Some(Background::Color(footer_bg)),
            ..Default::default()
        });

        let empty_surface = container(
            column![
                scrollable(center_box)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(tabby_scrollable_style(chrome)),
                footer,
            ]
            .width(Length::Fill)
            .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(chrome.chrome_bg)),
            text_color: Some(chrome.text_primary),
            ..Default::default()
        })
        .into();

        self.wrap_workspace_pane_drop_zone(None, palette, empty_surface)
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
        if pane_count > 1 {
            format!("{base} ({pane_count})")
        } else {
            base.to_string()
        }
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
                SessionEntry::Ssh(slot) => format!(
                    "Termewnal — {} [{}]",
                    slot.title,
                    slot.transport_status.label()
                ),
                SessionEntry::Serial(slot) => format!(
                    "Termewnal — {} [{}]",
                    slot.title,
                    slot.status().label()
                ),
                SessionEntry::Telnet(slot) => format!(
                    "Termewnal — {} [{}]",
                    slot.title,
                    slot.status().label()
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
        iced_theme_with_overrides(&self.config.terminal.theme, &self.config.terminal.theme_overrides)
    }

    /// Subscribes to window lifecycle events, single-instance IPC commands, config hot-reload,
    /// Vault auto-lock ticks, cursor blink, and live PTY output streams.
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

        let server_stats_subs =
            self.config
                .terminal
                .show_server_stats
                .then(|| {
                    self.sessions.iter().filter_map(|(&session_id, entry)| {
                        let SessionEntry::Ssh(slot) = entry else {
                            return None;
                        };
                        if slot.server_stats_unsupported {
                            return None;
                        }
                        let sup = slot.supervisor.as_ref()?;
                        Some(Subscription::run_with(
                            ServerStatsSubKey {
                                session_id,
                                generation: slot.generation,
                                transport_rx: sup.subscribe_transport(),
                            },
                            server_stats_stream,
                        ))
                    })
                })
                .into_iter()
                .flatten();

        let ipc_sub = self.ipc_rx.as_ref().map(|rx| {
            Subscription::run_with(
                IpcSubKey {
                    rx: Arc::clone(rx),
                },
                ipc_event_stream,
            )
        });
        let tray_sub = self.tray_rx.as_ref().map(|rx| {
            Subscription::run_with(
                TraySubKey {
                    rx: Arc::clone(rx),
                },
                tray_event_stream,
            )
        });
        let hotkey_sub = self.hotkey_rx.as_ref().map(|rx| {
            Subscription::run_with(
                HotkeySubKey {
                    rx: Arc::clone(rx),
                },
                hotkey_event_stream,
            )
        });

        let vault_tick = (self.vault.backend() == VaultBackend::EncryptedFile
            && self.vault.is_unlocked())
        .then(|| iced::time::every(Duration::from_secs(15)).map(|_| Message::VaultAutoLockTick));

        let blink_tick = self
            .config
            .terminal
            .cursor_blink
            .then(|| iced::time::every(Duration::from_millis(530)).map(|_| Message::CursorBlinkTick));

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
        let sftp_status_sub = Subscription::run_with(
            SftpStatusSubKey {
                id: self.sftp_sub_id,
                manager: self.sftp_manager.clone(),
            },
            sftp_status_stream,
        );
        let serial_status_subs = self.sessions.iter().filter_map(|(&session_id, entry)| {
            let SessionEntry::Serial(slot) = entry else {
                return None;
            };
            let active = slot.active.as_ref()?;
            Some(Subscription::run_with(
                ProtocolStatusSubKey {
                    session_id,
                    kind: SessionKindLabel::Serial,
                    generation: slot.generation,
                    rx: active.subscribe_status(),
                },
                protocol_status_stream,
            ))
        });
        let telnet_status_subs = self.sessions.iter().filter_map(|(&session_id, entry)| {
            let SessionEntry::Telnet(slot) = entry else {
                return None;
            };
            let active = slot.active.as_ref()?;
            Some(Subscription::run_with(
                ProtocolStatusSubKey {
                    session_id,
                    kind: SessionKindLabel::Telnet,
                    generation: slot.generation,
                    rx: active.subscribe_status(),
                },
                protocol_status_stream,
            ))
        });
        let reconnect_notice_tick = self.has_active_reconnect_notices().then(|| {
            iced::time::every(Duration::from_secs(1)).map(|_| Message::ClearExpiredReconnectNotices)
        });

        Subscription::batch(
            [
                close_requests,
                closed,
                win_events,
                shortcuts,
                config_watch,
                transfer_sub,
                tunnel_sub,
                sftp_status_sub,
            ]
            .into_iter()
            .chain(vault_tick)
            .chain(blink_tick)
            .chain(reconnect_notice_tick)
            .chain(ipc_sub)
            .chain(tray_sub)
            .chain(hotkey_sub)
            .chain(pty_subs)
            .chain(server_stats_subs)
            .chain(serial_status_subs)
            .chain(telnet_status_subs),
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

    /// Returns the Settings modal state (`Ctrl+,`).
    pub fn settings_ui(&self) -> &SettingsUiState {
        &self.settings_ui
    }

    /// Returns the Profile Manager create/edit modal state.
    pub fn profile_editor(&self) -> &ProfileEditorState {
        &self.profile_editor
    }

    /// Returns `true` when any overlay modal blocks background terminal focus or IME.
    pub fn has_modal_open(&self) -> bool {
        self.command_palette.is_open()
            || self.tabby_import.is_open()
            || self.settings_ui.is_open()
            || self.profile_editor.is_open()
            || self.tunnel_ui.create_form().open
            || self.tunnel_ui.pending_public_bind_warning().is_some()
            || self.active_conflict_drawer().is_some()
            || self.pending_paste.is_some()
            || self.ssh_ui.active_host_key_prompt().is_some()
            || self.ssh_ui.active_auth_prompt().is_some()
            || self.context_menu.is_open()
            || self.sftp_drawer().plus.modal.is_open()
            || self.sftp_drawer().plus.context_menu.open
            || self.quick_commands_ui.open
            || self.workspaces_ui.open
            || self.protocol_launcher.open
            || self.zmodem_send_ui.session_id.is_some()
            || !matches!(self.close_guard, CloseGuardState::Idle)
    }

    /// Computes an inspectable [`StatusBarSnapshot`] from the active `Session`, `Tunnel`, and `Transfer` state.
    pub fn status_bar_snapshot(&self) -> StatusBarSnapshot {
        self.status_bar_snapshot_for_workspace(self.primary_workspace())
    }

    fn protocol_status_to_transport(status: ProtocolStatus) -> TransportStatus {
        match status {
            ProtocolStatus::Connected => TransportStatus::Connected { latency_ms: None },
            ProtocolStatus::Connecting => TransportStatus::Connecting,
            ProtocolStatus::Reconnecting { attempt, .. } => TransportStatus::Reconnecting {
                attempt,
                next_retry_secs: 1,
            },
            ProtocolStatus::Disconnected => TransportStatus::Disconnected,
        }
    }

    fn status_bar_snapshot_for_workspace(&self, ws: Option<&Workspace>) -> StatusBarSnapshot {
        let active_tab = ws.and_then(|w| w.active_tab());
        let zoomed = active_tab.is_some_and(|tab| tab.zoomed_pane.is_some());
        let broadcast = active_tab.is_some_and(|tab| tab.broadcast_input);
        let active_sid = active_tab.and_then(|tab| tab.focused_session_id());
        let mut connection_notice: Option<StatusRecoveryNotice> = None;
        let active_session = active_sid
            .and_then(|sid| self.sessions.get(&sid))
            .map(|entry| match entry {
                SessionEntry::Local(s) => {
                    let (cols, rows) = (s.snapshot.cols, s.snapshot.rows);
                    let (kind, target) = if let Some(exec) = &s.docker_exec {
                        (SessionKindLabel::Docker, exec.container.clone())
                    } else {
                        (
                            SessionKindLabel::Local,
                            s.pty.shell().display_name.clone(),
                        )
                    };
                    SessionStatusHook {
                        session_id: s.id,
                        kind,
                        title: s.title.clone(),
                        target,
                        cols,
                        rows,
                        exited: s.exited,
                        transport_status: s.transport_status.clone(),
                        is_auto_reconnecting: false,
                        zoomed,
                        broadcast,
                    }
                }
                SessionEntry::Ssh(slot) => {
                    let auto_reconnecting = slot.is_auto_reconnecting();
                    connection_notice = format_connection_recovery_notice(
                        auto_reconnecting,
                        slot.reconnect_attempt(),
                        &slot.transport_status,
                        slot.is_recently_reconnected(),
                    );
                    SessionStatusHook {
                        session_id: slot.id,
                        kind: SessionKindLabel::Ssh,
                        title: slot.title.clone(),
                        target: slot.target.display_addr(),
                        cols: slot.snapshot.cols,
                        rows: slot.snapshot.rows,
                        exited: slot.exited,
                        transport_status: slot.transport_status.clone(),
                        is_auto_reconnecting: auto_reconnecting,
                        zoomed,
                        broadcast,
                    }
                }
                SessionEntry::Serial(slot) => {
                    let transport_status = Self::protocol_status_to_transport(slot.status());
                    let auto_reconnecting = slot.is_auto_reconnecting();
                    connection_notice = format_connection_recovery_notice(
                        auto_reconnecting,
                        slot.reconnect_attempt(),
                        &transport_status,
                        slot.is_recently_reconnected(),
                    );
                    SessionStatusHook {
                        session_id: slot.id,
                        kind: SessionKindLabel::Serial,
                        title: slot.title.clone(),
                        target: format!("{} @ {} bps", slot.config.port, slot.config.baud_rate),
                        cols: slot.snapshot.cols,
                        rows: slot.snapshot.rows,
                        exited: None,
                        transport_status,
                        is_auto_reconnecting: auto_reconnecting,
                        zoomed,
                        broadcast,
                    }
                }
                SessionEntry::Telnet(slot) => {
                    let transport_status = Self::protocol_status_to_transport(slot.status());
                    let auto_reconnecting = slot.is_auto_reconnecting();
                    connection_notice = format_connection_recovery_notice(
                        auto_reconnecting,
                        slot.reconnect_attempt(),
                        &transport_status,
                        slot.is_recently_reconnected(),
                    );
                    SessionStatusHook {
                        session_id: slot.id,
                        kind: SessionKindLabel::Telnet,
                        title: slot.title.clone(),
                        target: format!("{}:{}", slot.config.host, slot.config.port),
                        cols: slot.snapshot.cols,
                        rows: slot.snapshot.rows,
                        exited: None,
                        transport_status,
                        is_auto_reconnecting: auto_reconnecting,
                        zoomed,
                        broadcast,
                    }
                }
            });

        let drawer = active_sid
            .and_then(|sid| self.sftp_drawers.get(&sid))
            .unwrap_or(&self.sftp_fallback_drawer);
        let sftp_notice = sftp_recovery_notice_for_drawer(drawer)
            .or_else(|| self.status_bar.sftp_notice().cloned());

        StatusBarSnapshot {
            active_session,
            tunnels: self.status_bar.tunnels().clone(),
            transfers: self.status_bar.transfers().clone(),
            sidebar_open: self.sidebar.is_open(),
            transfer_drawer_open: self.status_bar.transfers().drawer_open,
            vault_unlocked: self.can_lock_vault(),
            connection_notice,
            tunnel_notice: self.status_bar.tunnel_notice().cloned(),
            sftp_notice,
            last_error: self.last_error.clone(),
            zmodem_notice: self.last_zmodem_notice.clone(),
        }
    }

    fn can_lock_vault(&self) -> bool {
        self.vault.backend() == VaultBackend::EncryptedFile && self.vault.is_unlocked()
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
                                if let Some(exec) = &s.docker_exec {
                                    format!("Docker · {}", exec.container)
                                } else {
                                    format!("Local · {}", s.pty.shell().display_name)
                                }
                            }
                            SessionEntry::Ssh(slot) => {
                                format!(
                                    "SSH · {} ({})",
                                    slot.target.display_addr(),
                                    slot.transport_status.label()
                                )
                            }
                            SessionEntry::Serial(slot) => {
                                format!(
                                    "Serial · {} @ {} ({})",
                                    slot.config.port,
                                    slot.config.baud_rate,
                                    slot.status().label()
                                )
                            }
                            SessionEntry::Telnet(slot) => {
                                format!(
                                    "Telnet · {}:{} ({})",
                                    slot.config.host,
                                    slot.config.port,
                                    slot.status().label()
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
        self.command_palette.filtered_items_with_quick_commands(
            &self.profiles.profiles,
            &sessions,
            self.sidebar.system_ssh_profiles(),
            self.sidebar.active_tunnels(),
            &self.quick_commands.commands,
            self.can_lock_vault(),
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

    /// Returns a projected remote `Session` descriptor for an SSH `Session` by `id`.
    pub fn remote_session(&self, id: SessionId) -> Option<RemoteSession> {
        let slot = self.ssh_session(id)?;
        let connect_requested = slot.generation > 0
            || slot.transport_status != TransportStatus::Disconnected
            || self
                .pending_remote_connects
                .iter()
                .any(|intent| intent.session_id == id);
        Some(RemoteSession {
            id: slot.id,
            profile_id: slot.target.profile_id.clone(),
            host: slot.target.host.clone(),
            port: slot.target.port,
            username: (!slot.target.username.is_empty()).then(|| slot.target.username.clone()),
            title: slot.title.clone(),
            transport_status: slot.transport_status.clone(),
            auto_connect_on_startup: slot.target.auto_connect_on_startup,
            connect_requested,
        })
    }

    /// Returns an immutable reference to an SSH `Session` slot by `id`.
    pub fn ssh_session(&self, id: SessionId) -> Option<&SshSessionSlot> {
        match self.sessions.get(&id) {
            Some(SessionEntry::Ssh(slot)) => Some(slot),
            _ => None,
        }
    }

    fn update_ssh_slot_status(&mut self, session_id: SessionId, status: TransportStatus) {
        if let Some(SessionEntry::Ssh(slot)) = self.sessions.get_mut(&session_id) {
            let now = Instant::now();
            slot.record_transport_transition(&status, now);
            slot.status_message = format_ssh_slot_message(&slot.target, &status);
            slot.transport_status = status;
        }
    }

    /// Returns connection intents recorded for remote `Session`s on startup or manual connect.
    pub fn pending_remote_connects(&self) -> &[RemoteConnectIntent] {
        &self.pending_remote_connects
    }

    fn export_workspace_tabs(&self, ws: &Workspace) -> (Vec<TabLayout>, usize) {
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
                    color: tab.color.clone(),
                    pinned: tab.pinned,
                    focused_pane,
                    zoomed_pane,
                    root: self.export_split_node(&tab.root),
                }
            })
            .collect();
        (tabs, active_tab)
    }

    /// Exports the current multi-window workspace geometry and session descriptors to [`WorkspaceLayout`].
    pub fn to_workspace_layout(&self) -> WorkspaceLayout {
        let windows_iter: Box<dyn Iterator<Item = &WindowState> + '_> =
            if !self.window_workspaces.is_empty() {
                Box::new(
                    self.window_workspaces
                        .iter()
                        .filter(|&(id, _)| Some(*id) != self.quake.window_id())
                        .map(|(_, w)| w),
                )
            } else {
                Box::new(self.pending_windows.iter())
            };

        let windows = windows_iter
            .filter(|w| w.workspace.tab_count() > 0)
            .map(|win_state| {
                let (tabs, active_tab) = self.export_workspace_tabs(&win_state.workspace);
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
            Some(SessionEntry::Local(local)) => {
                if let Some(exec) = &local.docker_exec {
                    SessionDescriptor::Docker {
                        profile_id: local.profile_id.clone(),
                        runtime: exec.runtime,
                        container: exec.container.clone(),
                        shell: exec.shell.clone(),
                        user: exec.user.clone(),
                        working_dir: exec.working_dir.clone(),
                        title: Some(local.title.clone()),
                    }
                } else {
                    SessionDescriptor::Local {
                        profile_id: local.profile_id.clone(),
                        title: Some(local.title.clone()),
                        working_dir: local
                            .pty
                            .current_working_dir()
                            .or_else(|| local.initial_working_dir.clone()),
                        shell: Some(local.pty.shell().program.clone()),
                    }
                }
            }
            Some(SessionEntry::Ssh(slot)) => SessionDescriptor::Remote {
                profile_id: slot.target.profile_id.clone(),
                host: slot.target.host.clone(),
                port: slot.target.port,
                username: Some(slot.target.username.clone()),
                title: Some(slot.title.clone()),
                auto_connect_on_startup: slot.target.auto_connect_on_startup,
            },
            Some(SessionEntry::Serial(slot)) => SessionDescriptor::Serial {
                profile_id: slot.profile_id.clone(),
                port: slot.config.port.clone(),
                baud_rate: slot.config.baud_rate,
                data_bits: slot.config.data_bits,
                parity: slot.config.parity,
                stop_bits: slot.config.stop_bits,
                flow_control: slot.config.flow_control,
                input_mode: slot.config.input_mode,
                output_mode: slot.config.output_mode,
                newline_mode: slot.config.newline_mode,
                auto_reconnect: slot.config.auto_reconnect,
                title: Some(slot.title.clone()),
            },
            Some(SessionEntry::Telnet(slot)) => SessionDescriptor::Telnet {
                profile_id: slot.profile_id.clone(),
                host: slot.config.host.clone(),
                port: slot.config.port,
                auto_reconnect: slot.config.auto_reconnect,
                title: Some(slot.title.clone()),
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

    /// Returns the SFTP File Browser & `Transfer` Queue drawer state for the active session.
    pub fn sftp_drawer(&self) -> &SftpDrawerState {
        self.sftp_drawer_for_workspace(self.primary_workspace())
    }

    /// Returns the SFTP File Browser & `Transfer` Queue drawer state for `session_id`, if any.
    pub fn sftp_drawer_for_session(&self, session_id: SessionId) -> Option<&SftpDrawerState> {
        self.sftp_drawers.get(&session_id)
    }

    /// Returns `true` when `session_id`'s SFTP Drawer is currently open.
    pub fn is_sftp_drawer_open_for(&self, session_id: SessionId) -> bool {
        self.sftp_drawers
            .get(&session_id)
            .is_some_and(SftpDrawerState::is_open)
    }

    /// Returns the last remembered remote SFTP directory for saved `profile_id`, if any.
    pub fn remembered_profile_sftp_dir(&self, profile_id: &str) -> Option<&str> {
        self.sftp_state.last_dir(profile_id)
    }

    /// Overrides the local destination directory for SFTP downloads (useful in tests).
    pub fn set_sftp_download_dir(&mut self, dir: PathBuf) {
        self.sftp_download_dir = dir.clone();
        self.sftp_fallback_drawer.set_download_dir(dir.clone());
        for drawer in self.sftp_drawers.values_mut() {
            drawer.set_download_dir(dir.clone());
        }
        let existing_ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for sid in existing_ids {
            if let Some(buf) = self.sessions.get(&sid).and_then(SessionEntry::buffer).cloned() {
                self.attach_zmodem_controller(sid, &buf);
            }
        }
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

    /// Returns `true` when continuous session output logging is active on `session_id`.
    pub fn is_session_logging(&self, session_id: SessionId) -> bool {
        self.session_log_path(session_id).is_some()
    }

    /// Returns the active log file path when continuous logging is active on `session_id`.
    pub fn session_log_path(&self, session_id: SessionId) -> Option<PathBuf> {
        self.sessions
            .get(&session_id)
            .and_then(SessionEntry::buffer)
            .and_then(|b| b.log_path())
    }

    /// Returns the most recently exported scrollback text file path, if any.
    pub fn last_saved_scrollback_path(&self) -> Option<&Path> {
        self.last_saved_scrollback_path.as_deref()
    }

    /// Returns `true` when the brief visual bell flash highlight is active on `session_id`.
    pub fn is_visual_bell_active(&self, session_id: SessionId) -> bool {
        self.visual_bell_active.contains(&session_id)
    }

    /// Returns the active [`KeybindingResolver`].
    pub fn keybinding_resolver(&self) -> &KeybindingResolver {
        &self.keybinding_resolver
    }

    /// Returns queued prefix chords awaiting completion of a multi-chord shortcut.
    pub fn pending_key_chords(&self) -> &[KeyChord] {
        &self.pending_key_chords
    }

    /// Returns the active right-click context menu and tab/group prompt overlay state.
    pub fn context_menu(&self) -> &ContextMenuState {
        &self.context_menu
    }

    /// Returns active in-app Drag & Drop and OS file-hover feedback state.
    pub fn drag_drop(&self) -> &DragDropState {
        &self.drag_drop
    }

    /// Returns saved Quick Command snippets loaded from `quick_commands.toml`.
    pub fn quick_commands(&self) -> &[QuickCommand] {
        &self.quick_commands.commands
    }

    /// Returns saved Named Workspace templates loaded from `workspaces.toml`.
    pub fn workspace_templates(&self) -> &[WorkspaceTemplate] {
        &self.workspaces_cfg.workspaces
    }

    /// Returns the Quick Commands modal state.
    pub fn quick_commands_ui(&self) -> &QuickCommandsUiState {
        &self.quick_commands_ui
    }

    /// Returns the Named Workspaces modal state.
    pub fn workspaces_ui(&self) -> &WorkspacesUiState {
        &self.workspaces_ui
    }

    /// Returns the Containers/Serial/Telnet protocol launcher modal state.
    pub fn protocol_launcher(&self) -> &ProtocolLauncherState {
        &self.protocol_launcher
    }

    /// Returns the Zmodem send picker modal state.
    pub fn zmodem_send_ui(&self) -> &ZmodemSendUiState {
        &self.zmodem_send_ui
    }

    /// Returns the latest Zmodem transfer status notice, if any.
    pub fn zmodem_notice(&self) -> Option<&str> {
        self.last_zmodem_notice.as_deref()
    }

    /// Returns the typed Serial slot by `session_id`, if present.
    pub fn serial_session(&self, session_id: SessionId) -> Option<&SerialSessionSlot> {
        match self.sessions.get(&session_id) {
            Some(SessionEntry::Serial(s)) => Some(s),
            _ => None,
        }
    }

    /// Returns the typed Telnet slot by `session_id`, if present.
    pub fn telnet_session(&self, session_id: SessionId) -> Option<&TelnetSessionSlot> {
        match self.sessions.get(&session_id) {
            Some(SessionEntry::Telnet(s)) => Some(s),
            _ => None,
        }
    }

    /// Returns the persistent Quake drop-down console controller state.
    pub fn quake(&self) -> &QuakeController {
        &self.quake
    }

    /// Returns `true` when non-Quake desktop windows are currently hidden into the System Tray.
    pub fn is_minimized_to_tray(&self) -> bool {
        self.tray_hidden
    }

    /// Computes the current [`TrayStateSnapshot`] without mutating tray state.
    pub fn tray_snapshot(&self) -> TrayStateSnapshot {
        let non_quake_present = self
            .windows
            .iter()
            .any(|id| Some(*id) != self.quake.window_id());
        TrayStateSnapshot {
            window_visible: non_quake_present && !self.tray_hidden,
            quake_visible: self.quake.is_visible(),
            active_tunnel_count: self.tunnel_manager.snapshot().active_count,
            active_transfer_count: self.background_work_source.status().active_transfers,
            quick_profiles: self
                .profiles
                .profiles
                .iter()
                .take(8)
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect(),
        }
    }

    /// Returns `true` when an OS-level global hotkey registration guard is currently held.
    pub fn has_hotkey_guard(&self) -> bool {
        self.hotkey_guard.is_some()
    }

    /// Overrides the `known_hosts` path used by SSH connections and Standalone `Tunnel`s (for tests).
    pub fn set_known_hosts_path(&mut self, path: impl Into<PathBuf>) {
        self.known_hosts_override = Some(path.into());
    }

    fn ssh_session_sftp_label(slot: &SshSessionSlot) -> String {
        let addr = slot.target.display_addr();
        if slot.title.trim().is_empty() || slot.title == addr {
            addr
        } else {
            format!("{} ({})", slot.title, addr)
        }
    }

    fn active_ssh_session_in(
        sessions: &BTreeMap<SessionId, SessionEntry>,
        ws: Option<&Workspace>,
    ) -> Option<SessionId> {
        ws.and_then(|w| w.active_tab())
            .and_then(|t| t.focused_session_id())
            .filter(|sid| sessions.get(sid).and_then(SessionEntry::as_ssh).is_some())
    }

    fn active_ssh_session_id(&self) -> Option<SessionId> {
        Self::active_ssh_session_in(&self.sessions, self.primary_workspace())
    }

    fn sftp_drawer_for_workspace(&self, ws: Option<&Workspace>) -> &SftpDrawerState {
        Self::active_ssh_session_in(&self.sessions, ws)
            .and_then(|sid| self.sftp_drawers.get(&sid))
            .unwrap_or(&self.sftp_fallback_drawer)
    }

    fn sidebar_sessions(&self, ws: Option<&Workspace>) -> Vec<SidebarSessionEntry> {
        let Some(ws) = ws else {
            return Vec::new();
        };
        let active_tab_id = ws.active_tab_id();
        let mut out = Vec::new();
        for tab in ws.tabs() {
            let Some(sid) = tab.focused_session_id() else {
                continue;
            };
            let Some(entry) = self.sessions.get(&sid) else {
                continue;
            };
            let (profile_id, connected, connecting, auto_reconnecting, reconnected) = match entry {
                SessionEntry::Local(s) => (
                    s.profile_id().map(ToString::to_string),
                    s.exited().is_none(),
                    false,
                    false,
                    false,
                ),
                SessionEntry::Ssh(slot) => (
                    slot.target.profile_id.clone(),
                    matches!(slot.transport_status, TransportStatus::Connected { .. }),
                    matches!(
                        slot.transport_status,
                        TransportStatus::Connecting
                            | TransportStatus::Degraded { .. }
                            | TransportStatus::Reconnecting { .. }
                    ),
                    slot.is_auto_reconnecting(),
                    slot.is_recently_reconnected(),
                ),
                SessionEntry::Serial(slot) => (
                    slot.profile_id().map(ToString::to_string),
                    slot.status() == ProtocolStatus::Connected,
                    matches!(
                        slot.status(),
                        ProtocolStatus::Connecting | ProtocolStatus::Reconnecting { .. }
                    ),
                    slot.is_auto_reconnecting(),
                    slot.is_recently_reconnected(),
                ),
                SessionEntry::Telnet(slot) => (
                    slot.profile_id().map(ToString::to_string),
                    slot.status() == ProtocolStatus::Connected,
                    matches!(
                        slot.status(),
                        ProtocolStatus::Connecting | ProtocolStatus::Reconnecting { .. }
                    ),
                    slot.is_auto_reconnecting(),
                    slot.is_recently_reconnected(),
                ),
            };
            out.push(SidebarSessionEntry {
                session_id: sid,
                tab_id: tab.id,
                profile_id,
                title: self.tab_display_title(tab),
                connected,
                connecting,
                auto_reconnecting,
                reconnected,
                active_tab: Some(tab.id) == active_tab_id,
            });
        }
        out
    }

    fn build_session_sftp_drawer(&self, session_id: SessionId) -> SftpDrawerState {
        let slot = self.sessions.get(&session_id).and_then(SessionEntry::as_ssh);
        let label = slot.map(Self::ssh_session_sftp_label);
        let mut drawer = SftpDrawerState::for_session(
            session_id,
            self.sftp_download_dir.clone(),
            self.sftp_drawer_width,
            label,
        );
        if let Some(dir) = slot
            .and_then(|s| s.target.profile_id.as_deref())
            .and_then(|pid| self.sftp_state.last_dir(pid))
        {
            drawer.set_current_path(dir);
        }
        drawer
    }

    fn save_sftp_state_best_effort(&self) {
        if let Err(err) = save_sftp_state(&self.paths, &self.sftp_state) {
            warn!(error = %err, "failed to save sftp_state.toml");
        }
    }

    fn record_profile_sftp_dir(&mut self, session_id: SessionId, path: &str) {
        let Some(profile_id) = self
            .sessions
            .get(&session_id)
            .and_then(SessionEntry::as_ssh)
            .and_then(|slot| slot.target.profile_id.clone())
        else {
            return;
        };
        if self.sftp_state.record_dir(profile_id, path) {
            self.save_sftp_state_best_effort();
        }
    }

    fn ensure_session_sftp_drawer(&mut self, session_id: SessionId) -> &mut SftpDrawerState {
        if !self.sftp_drawers.contains_key(&session_id) {
            let drawer = self.build_session_sftp_drawer(session_id);
            self.sftp_drawers.insert(session_id, drawer);
        }
        let label = self
            .sessions
            .get(&session_id)
            .and_then(SessionEntry::as_ssh)
            .map(Self::ssh_session_sftp_label);
        let drawer = self
            .sftp_drawers
            .get_mut(&session_id)
            .expect("drawer inserted above");
        if let Some(lbl) = label {
            drawer.set_session_label(Some(lbl));
        }
        drawer
    }

    fn active_sftp_drawer_mut(&mut self) -> &mut SftpDrawerState {
        if let Some(sid) = self.active_ssh_session_id() {
            self.ensure_session_sftp_drawer(sid)
        } else {
            &mut self.sftp_fallback_drawer
        }
    }

    fn active_conflict_drawer(&self) -> Option<&SftpDrawerState> {
        let primary = self.sftp_drawer();
        if primary.active_conflict().is_some() {
            return Some(primary);
        }
        self.sftp_drawers
            .values()
            .find(|d| d.active_conflict().is_some())
            .or_else(|| {
                self.sftp_fallback_drawer
                    .active_conflict()
                    .map(|_| &self.sftp_fallback_drawer)
            })
    }

    fn conflict_drawer_mut(&mut self) -> &mut SftpDrawerState {
        if let Some(sid) = self.active_ssh_session_id()
            && self
                .sftp_drawers
                .get(&sid)
                .is_some_and(|d| d.active_conflict().is_some())
        {
            return self.sftp_drawers.get_mut(&sid).expect("checked");
        }
        if let Some((&sid, _)) = self
            .sftp_drawers
            .iter()
            .find(|(_, d)| d.active_conflict().is_some())
        {
            return self.sftp_drawers.get_mut(&sid).expect("checked");
        }
        if self.sftp_fallback_drawer.active_conflict().is_some() {
            return &mut self.sftp_fallback_drawer;
        }
        self.active_sftp_drawer_mut()
    }

    fn set_sftp_drawer_width(&mut self, width: f32) {
        let clamped = width.clamp(MIN_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH);
        self.sftp_drawer_width = clamped;
        self.sftp_fallback_drawer.set_width(clamped);
        for drawer in self.sftp_drawers.values_mut() {
            drawer.set_width(clamped);
        }
    }

    fn sync_status_bar_drawer_open(&mut self) {
        let open = self.sftp_drawer().is_open();
        let mut transfers = self.status_bar.transfers().clone();
        if transfers.drawer_open != open {
            transfers.drawer_open = open;
            self.status_bar.set_transfers(transfers);
        }
    }

    fn toggle_sftp_drawer(&mut self) -> Task<Message> {
        let next = !self.sftp_drawer().is_open();
        self.set_sftp_drawer_open(next)
    }

    fn toggle_sftp_drawer_for_session(&mut self, session_id: SessionId) -> Task<Message> {
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
        let next = !self.is_sftp_drawer_open_for(session_id);
        self.set_sftp_drawer_open_for_session(Some(session_id), next)
    }

    fn set_sftp_drawer_open(&mut self, open: bool) -> Task<Message> {
        let sid = self.active_ssh_session_id();
        self.set_sftp_drawer_open_for_session(sid, open)
    }

    fn set_sftp_drawer_open_for_session(
        &mut self,
        ssh_sid: Option<SessionId>,
        open: bool,
    ) -> Task<Message> {
        let home_dir = self.paths.home_dir().map(Path::to_path_buf);
        if let Some(sid) = ssh_sid {
            let drawer = self.ensure_session_sftp_drawer(sid);
            drawer.set_open(open);
            if open {
                drawer.plus.ensure_local_initialized(home_dir.as_deref());
            }
            self.sync_status_bar_drawer_open();
            if open {
                let path = self.ensure_session_sftp_drawer(sid).current_path().to_string();
                return self.sftp_refresh_task(sid, path);
            }
        } else {
            self.sftp_fallback_drawer.set_open(open);
            if open {
                self.sftp_fallback_drawer
                    .plus
                    .ensure_local_initialized(home_dir.as_deref());
            }
            self.sync_status_bar_drawer_open();
        }
        Task::none()
    }

    fn active_sftp_bookmark_key(&self) -> Option<String> {
        let profile_id = self.active_ssh_session_id().and_then(|sid| {
            self.sessions
                .get(&sid)
                .and_then(SessionEntry::as_ssh)
                .and_then(|slot| slot.target.profile_id.clone())
        });
        profile_id.or_else(|| self.sftp_drawer().session_label().map(str::to_string))
    }

    fn active_ssh_endpoint(&self) -> Option<(SessionId, SftpEndpoint)> {
        let sid = self.active_ssh_session_id()?;
        let SessionEntry::Ssh(slot) = self.sessions.get(&sid)? else {
            return None;
        };
        if slot.active.is_none() {
            return None;
        }
        Some((sid, slot.sftp_endpoint(&self.vault)))
    }

    fn refresh_open_sftp_after_transfer(&mut self) -> Task<Message> {
        let home_dir = self.paths.home_dir().map(Path::to_path_buf);
        self.active_sftp_drawer_mut()
            .plus
            .refresh_local(home_dir.as_deref());
        if let Some((sid, _)) = self.active_ssh_endpoint()
            && !self.sftp_drawer().is_loading()
        {
            let remote_path = self.sftp_drawer().current_path().to_string();
            return self.sftp_refresh_task(sid, remote_path);
        }
        Task::none()
    }

    fn enqueue_sftp_download_to_dir(
        &mut self,
        remote_path: String,
        entry_name: String,
        is_dir: bool,
        dest_dir: PathBuf,
    ) -> Task<Message> {
        if let Err(err) = validate_remote_entry_name(&entry_name) {
            self.active_sftp_drawer_mut()
                .set_browser_error(Some(err.to_string()));
            return Task::none();
        }
        let Some(session_id) = self.active_ssh_session_id() else {
            self.active_sftp_drawer_mut().set_browser_error(Some(
                "Connect an SSH Session before downloading.".to_string(),
            ));
            return Task::none();
        };
        let endpoint = {
            let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
                return Task::none();
            };
            slot.sftp_endpoint(&self.vault)
        };
        self.reset_transfer_conflict_override_if_idle();
        let local_target = dest_dir.join(&entry_name);
        if is_dir {
            let queue = self.transfer_queue.clone();
            return Task::perform(
                async move {
                    queue
                        .enqueue_download_dir(endpoint, remote_path, local_target)
                        .await
                        .map_err(|err| err.to_string())
                },
                move |result| Message::SftpSessionDirEnqueueResult { session_id, result },
            );
        }
        let id = self
            .transfer_queue
            .enqueue_download(endpoint, remote_path, local_target);
        self.transfer_sessions.insert(id, session_id);
        self.sync_transfer_telemetry();
        Task::none()
    }

    fn sftp_navigate_pane(&mut self, side: SftpPaneSide, path: String) -> Task<Message> {
        let home_dir = self.paths.home_dir().map(Path::to_path_buf);
        self.active_sftp_drawer_mut().plus.active_pane = side;
        self.active_sftp_drawer_mut().plus.context_menu.close();
        match side {
            SftpPaneSide::Local => {
                let drawer = self.active_sftp_drawer_mut();
                if path.trim().is_empty() {
                    drawer.plus.refresh_local(home_dir.as_deref());
                } else {
                    drawer.plus.navigate_local(&path);
                }
                Task::none()
            }
            SftpPaneSide::Remote => {
                let target = if path.trim().is_empty() {
                    ".".to_string()
                } else {
                    path
                };
                self.navigate_sftp_to(target)
            }
        }
    }

    fn handle_sftp_plus_message(&mut self, msg: Message) -> Task<Message> {
        let home_dir = self.paths.home_dir().map(Path::to_path_buf);
        match msg {
            Message::SftpSetLayoutMode(mode) => {
                let was_open = self.sftp_drawer().is_open();
                self.active_sftp_drawer_mut().plus.layout_mode = mode;
                self.active_sftp_drawer_mut().plus.context_menu.close();
                if !was_open {
                    return self.set_sftp_drawer_open(true);
                }
                self.active_sftp_drawer_mut()
                    .plus
                    .ensure_local_initialized(home_dir.as_deref());
                Task::none()
            }
            Message::SftpToggleTransfersPanel => {
                let drawer = self.active_sftp_drawer_mut();
                drawer.plus.transfers_panel_open = !drawer.plus.transfers_panel_open;
                Task::none()
            }
            Message::SftpSetActivePane(side) => {
                let drawer = self.active_sftp_drawer_mut();
                drawer.plus.active_pane = side;
                drawer.plus.context_menu.close();
                Task::none()
            }
            Message::SftpShortcut(action) => {
                let drawer = self.sftp_drawer();
                if !drawer.is_open()
                    || drawer.plus.layout_mode == SftpLayoutMode::SideDock
                    || self.has_modal_open()
                {
                    return Task::none();
                }
                self.execute_sftp_action(action)
            }
            Message::SftpPanePathInputChanged(side, value) => {
                pane_prefs_mut(self.active_sftp_drawer_mut(), side).path_input = value;
                Task::none()
            }
            Message::SftpPaneNavigateSubmitted(side) => {
                let target = pane_prefs(self.sftp_drawer(), side).path_input.clone();
                self.sftp_navigate_pane(side, target)
            }
            Message::SftpPaneNavigateTo { side, path } => self.sftp_navigate_pane(side, path),
            Message::SftpPaneBack(side) => {
                let Some(path) = pane_prefs_mut(self.active_sftp_drawer_mut(), side).step_back()
                else {
                    return Task::none();
                };
                self.sftp_navigate_pane(side, path)
            }
            Message::SftpPaneForward(side) => {
                let Some(path) =
                    pane_prefs_mut(self.active_sftp_drawer_mut(), side).step_forward()
                else {
                    return Task::none();
                };
                self.sftp_navigate_pane(side, path)
            }
            Message::SftpPaneUp(side) => {
                pane_prefs_mut(self.active_sftp_drawer_mut(), side).navigating_history = false;
                let current = match side {
                    SftpPaneSide::Local => self.sftp_drawer().plus.local.current_path.clone(),
                    SftpPaneSide::Remote => self.sftp_drawer().current_path().to_string(),
                };
                let parent = parent_pane_path(side, &current);
                self.sftp_navigate_pane(side, parent)
            }
            Message::SftpPaneRefresh(side) => {
                let current = match side {
                    SftpPaneSide::Local => self.sftp_drawer().plus.local.current_path.clone(),
                    SftpPaneSide::Remote => self.sftp_drawer().current_path().to_string(),
                };
                self.sftp_navigate_pane(side, current)
            }
            Message::SftpPaneHome(side) => {
                let target = match side {
                    SftpPaneSide::Local => self
                        .sftp_drawer()
                        .plus
                        .local
                        .home_path
                        .clone()
                        .or_else(|| {
                            home_dir
                                .as_ref()
                                .map(|p| p.to_string_lossy().into_owned())
                        })
                        .unwrap_or_else(|| default_local_home_dir().to_string_lossy().into_owned()),
                    SftpPaneSide::Remote => self
                        .sftp_drawer()
                        .plus
                        .remote_prefs
                        .home_path
                        .clone()
                        .unwrap_or_else(|| ".".to_string()),
                };
                self.sftp_navigate_pane(side, target)
            }
            Message::SftpPaneToggleHidden(side) => {
                let prefs = pane_prefs_mut(self.active_sftp_drawer_mut(), side);
                prefs.show_hidden = !prefs.show_hidden;
                Task::none()
            }
            Message::SftpPaneToggleFilter(side) => {
                let prefs = pane_prefs_mut(self.active_sftp_drawer_mut(), side);
                prefs.filter_open = !prefs.filter_open;
                if !prefs.filter_open {
                    prefs.filter_query.clear();
                }
                Task::none()
            }
            Message::SftpPaneFilterChanged(side, q) => {
                pane_prefs_mut(self.active_sftp_drawer_mut(), side).filter_query = q;
                Task::none()
            }
            Message::SftpPaneToggleBookmarks(side) => {
                let other = side.other();
                pane_prefs_mut(self.active_sftp_drawer_mut(), other).bookmarks_open = false;
                let prefs = pane_prefs_mut(self.active_sftp_drawer_mut(), side);
                prefs.bookmarks_open = !prefs.bookmarks_open;
                Task::none()
            }
            Message::SftpPaneDismissBookmarks(side) => {
                pane_prefs_mut(self.active_sftp_drawer_mut(), side).bookmarks_open = false;
                Task::none()
            }
            Message::SftpPaneToggleBookmarkCurrent(side) => {
                match side {
                    SftpPaneSide::Local => {
                        let current = self.sftp_drawer().plus.local.current_path.clone();
                        self.sftp_state.toggle_local_bookmark(&current);
                    }
                    SftpPaneSide::Remote => {
                        let current = self.sftp_drawer().current_path().to_string();
                        if let Some(key) = self.active_sftp_bookmark_key() {
                            self.sftp_state.toggle_remote_bookmark(key, &current);
                        }
                    }
                }
                if let Err(err) = save_sftp_state(&self.paths, &self.sftp_state) {
                    warn!(error = %err, "failed saving SFTP bookmarks");
                }
                Task::none()
            }
            Message::SftpPaneSelectBookmark { side, path } => {
                pane_prefs_mut(self.active_sftp_drawer_mut(), side).bookmarks_open = false;
                self.sftp_navigate_pane(side, path)
            }
            Message::SftpPaneRemoveBookmark { side, path } => {
                match side {
                    SftpPaneSide::Local => {
                        if self
                            .sftp_state
                            .local_bookmarks()
                            .iter()
                            .any(|b| b == &path)
                        {
                            self.sftp_state.toggle_local_bookmark(&path);
                        }
                    }
                    SftpPaneSide::Remote => {
                        if let Some(key) = self.active_sftp_bookmark_key()
                            && self
                                .sftp_state
                                .remote_bookmarks(&key)
                                .iter()
                                .any(|b| b == &path)
                        {
                            self.sftp_state.toggle_remote_bookmark(key, &path);
                        }
                    }
                }
                if let Err(err) = save_sftp_state(&self.paths, &self.sftp_state) {
                    warn!(error = %err, "failed saving SFTP bookmarks");
                }
                Task::none()
            }
            Message::SftpPaneSortBy(side, col) => {
                pane_prefs_mut(self.active_sftp_drawer_mut(), side).toggle_sort(col);
                Task::none()
            }
            Message::SftpPaneRowClicked {
                side,
                path,
                ctrl_held,
            } => {
                let double_click = self
                    .active_sftp_drawer_mut()
                    .plus
                    .handle_row_click(side, &path, ctrl_held);
                if !double_click {
                    return Task::none();
                }
                let clicked_entry = visible_pane_entries(self.sftp_drawer(), side)
                    .into_iter()
                    .find(|e| e.full_path == path);
                let Some(entry) = clicked_entry else {
                    return Task::none();
                };
                if entry.is_dir {
                    return self.sftp_navigate_pane(side, entry.full_path);
                }
                self.execute_sftp_action(SftpContextActionKind::Transfer)
            }
            Message::SftpOpenContextMenu {
                side,
                target_path,
                position,
            } => {
                let drawer = self.active_sftp_drawer_mut();
                drawer.plus.active_pane = side;
                if let Some(ref p) = target_path {
                    let prefs = pane_prefs_mut(drawer, side);
                    if !prefs.selected_paths.contains(p) {
                        prefs.selected_paths.clear();
                        prefs.selected_paths.insert(p.clone());
                    }
                }
                drawer.plus.context_menu.show(side, target_path, position);
                Task::none()
            }
            Message::SftpDismissContextMenu => {
                self.active_sftp_drawer_mut().plus.context_menu.close();
                Task::none()
            }
            Message::SftpContextAction(action) => self.execute_sftp_action(action),
            Message::SftpDismissModal => {
                self.active_sftp_drawer_mut().plus.modal = SftpModalState::None;
                Task::none()
            }
            Message::SftpModalNameChanged(val) => {
                match &mut self.active_sftp_drawer_mut().plus.modal {
                    SftpModalState::CreateEntry { name, error, .. } => {
                        *name = val;
                        *error = None;
                    }
                    SftpModalState::Rename {
                        new_name, error, ..
                    } => {
                        *new_name = val;
                        *error = None;
                    }
                    _ => {}
                }
                Task::none()
            }
            Message::SftpModalConfirmCreateOrRename => {
                let modal_snapshot = self.sftp_drawer().plus.modal.clone();
                match modal_snapshot {
                    SftpModalState::CreateEntry {
                        side,
                        parent_dir,
                        is_dir,
                        name,
                        ..
                    } => {
                        let trimmed = name.trim().to_string();
                        if let Err(err) = validate_remote_entry_name(&trimmed) {
                            if let SftpModalState::CreateEntry { error, .. } =
                                &mut self.active_sftp_drawer_mut().plus.modal
                            {
                                *error = Some(err.to_string());
                            }
                            return Task::none();
                        }
                        let target = join_pane_path(side, &parent_dir, &trimmed);
                        match side {
                            SftpPaneSide::Local => {
                                let p = PathBuf::from(&target);
                                if p.exists() {
                                    if let SftpModalState::CreateEntry { error, .. } =
                                        &mut self.active_sftp_drawer_mut().plus.modal
                                    {
                                        *error =
                                            Some("An item with that name already exists".to_string());
                                    }
                                    return Task::none();
                                }
                                let res = if is_dir {
                                    std::fs::create_dir(&p)
                                        .map_err(|e| format!("mkdir failed: {e}"))
                                } else {
                                    std::fs::OpenOptions::new()
                                        .write(true)
                                        .create_new(true)
                                        .open(&p)
                                        .map(|_| ())
                                        .map_err(|e| format!("touch failed: {e}"))
                                };
                                match res {
                                    Ok(()) => {
                                        self.active_sftp_drawer_mut().plus.modal =
                                            SftpModalState::None;
                                        self.active_sftp_drawer_mut()
                                            .plus
                                            .refresh_local(home_dir.as_deref());
                                    }
                                    Err(err) => {
                                        if let SftpModalState::CreateEntry { error, .. } =
                                            &mut self.active_sftp_drawer_mut().plus.modal
                                        {
                                            *error = Some(err);
                                        }
                                    }
                                }
                                Task::none()
                            }
                            SftpPaneSide::Remote => {
                                let Some((session_id, endpoint)) = self.active_ssh_endpoint()
                                else {
                                    if let SftpModalState::CreateEntry { error, .. } =
                                        &mut self.active_sftp_drawer_mut().plus.modal
                                    {
                                        *error = Some("SSH session is disconnected".to_string());
                                    }
                                    return Task::none();
                                };
                                self.active_sftp_drawer_mut().plus.modal = SftpModalState::None;
                                let manager = self.sftp_manager.clone();
                                Task::perform(
                                    async move {
                                        if is_dir {
                                            manager
                                                .mkdir(&endpoint, &target)
                                                .await
                                                .map_err(|e| e.to_string())
                                        } else {
                                            manager
                                                .create_empty_file(&endpoint, &target)
                                                .await
                                                .map_err(|e| e.to_string())
                                        }
                                    },
                                    move |result| Message::SftpMutationResult { session_id, result },
                                )
                            }
                        }
                    }
                    SftpModalState::Rename {
                        side,
                        old_path,
                        new_name,
                        ..
                    } => {
                        let trimmed = new_name.trim().to_string();
                        if let Err(err) = validate_remote_entry_name(&trimmed) {
                            if let SftpModalState::Rename { error, .. } =
                                &mut self.active_sftp_drawer_mut().plus.modal
                            {
                                *error = Some(err.to_string());
                            }
                            return Task::none();
                        }
                        let parent = parent_pane_path(side, &old_path);
                        let new_path = join_pane_path(side, &parent, &trimmed);
                        match side {
                            SftpPaneSide::Local => {
                                let src = PathBuf::from(&old_path);
                                let dst = PathBuf::from(&new_path);
                                if src != dst && dst.exists() {
                                    if let SftpModalState::Rename { error, .. } =
                                        &mut self.active_sftp_drawer_mut().plus.modal
                                    {
                                        *error = Some("Destination already exists".to_string());
                                    }
                                    return Task::none();
                                }
                                match std::fs::rename(&src, &dst) {
                                    Ok(()) => {
                                        self.active_sftp_drawer_mut().plus.modal =
                                            SftpModalState::None;
                                        self.active_sftp_drawer_mut()
                                            .plus
                                            .refresh_local(home_dir.as_deref());
                                    }
                                    Err(err) => {
                                        if let SftpModalState::Rename { error, .. } =
                                            &mut self.active_sftp_drawer_mut().plus.modal
                                        {
                                            *error = Some(format!("rename failed: {err}"));
                                        }
                                    }
                                }
                                Task::none()
                            }
                            SftpPaneSide::Remote => {
                                let Some((session_id, endpoint)) = self.active_ssh_endpoint()
                                else {
                                    if let SftpModalState::Rename { error, .. } =
                                        &mut self.active_sftp_drawer_mut().plus.modal
                                    {
                                        *error = Some("SSH session is disconnected".to_string());
                                    }
                                    return Task::none();
                                };
                                self.active_sftp_drawer_mut().plus.modal = SftpModalState::None;
                                let manager = self.sftp_manager.clone();
                                Task::perform(
                                    async move {
                                        manager
                                            .rename(&endpoint, &old_path, &new_path)
                                            .await
                                            .map_err(|e| e.to_string())
                                    },
                                    move |result| Message::SftpMutationResult { session_id, result },
                                )
                            }
                        }
                    }
                    _ => Task::none(),
                }
            }
            Message::SftpModalOctalChanged(val) => {
                if let SftpModalState::EditPermissions {
                    octal_input,
                    mode_bits,
                    error,
                    ..
                } = &mut self.active_sftp_drawer_mut().plus.modal
                {
                    let filtered: String =
                        val.chars().filter(|c| ('0'..='7').contains(c)).take(3).collect();
                    if let Ok(parsed) = u32::from_str_radix(&filtered, 8)
                        && !filtered.is_empty()
                    {
                        *mode_bits = parsed & 0o777;
                    }
                    *octal_input = filtered;
                    *error = None;
                }
                Task::none()
            }
            Message::SftpModalPermBitToggled { bit, enabled } => {
                if let SftpModalState::EditPermissions {
                    octal_input,
                    mode_bits,
                    error,
                    ..
                } = &mut self.active_sftp_drawer_mut().plus.modal
                {
                    if enabled {
                        *mode_bits |= bit;
                    } else {
                        *mode_bits &= !bit;
                    }
                    *mode_bits &= 0o777;
                    *octal_input = format_octal_mode(*mode_bits);
                    *error = None;
                }
                Task::none()
            }
            Message::SftpModalConfirmPermissions => {
                let modal = self.sftp_drawer().plus.modal.clone();
                let SftpModalState::EditPermissions {
                    side,
                    target_paths,
                    octal_input,
                    mode_bits,
                    ..
                } = modal
                else {
                    return Task::none();
                };
                let final_bits = if let Ok(parsed) = u32::from_str_radix(octal_input.trim(), 8)
                    && !octal_input.trim().is_empty()
                {
                    parsed & 0o777
                } else {
                    mode_bits & 0o777
                };
                match side {
                    SftpPaneSide::Local => {
                        for p in &target_paths {
                            if let Err(err) = set_local_permissions(Path::new(p), final_bits) {
                                if let SftpModalState::EditPermissions { error, .. } =
                                    &mut self.active_sftp_drawer_mut().plus.modal
                                {
                                    *error = Some(err);
                                }
                                return Task::none();
                            }
                        }
                        self.active_sftp_drawer_mut().plus.modal = SftpModalState::None;
                        self.active_sftp_drawer_mut()
                            .plus
                            .refresh_local(home_dir.as_deref());
                        Task::none()
                    }
                    SftpPaneSide::Remote => {
                        let Some((session_id, endpoint)) = self.active_ssh_endpoint() else {
                            if let SftpModalState::EditPermissions { error, .. } =
                                &mut self.active_sftp_drawer_mut().plus.modal
                            {
                                *error = Some("SSH session is disconnected".to_string());
                            }
                            return Task::none();
                        };
                        self.active_sftp_drawer_mut().plus.modal = SftpModalState::None;
                        let manager = self.sftp_manager.clone();
                        Task::perform(
                            async move {
                                for p in &target_paths {
                                    manager
                                        .set_permissions(&endpoint, p, final_bits)
                                        .await
                                        .map_err(|e| e.to_string())?;
                                }
                                Ok(())
                            },
                            move |result| Message::SftpMutationResult { session_id, result },
                        )
                    }
                }
            }
            Message::SftpModalEditorAction(action) => {
                if let SftpModalState::FileViewer {
                    editable,
                    dirty,
                    content,
                    ..
                } = &mut self.active_sftp_drawer_mut().plus.modal
                {
                    let is_edit = matches!(action, iced::widget::text_editor::Action::Edit(_));
                    if !*editable && is_edit {
                        return Task::none();
                    }
                    content.perform(action);
                    if is_edit {
                        *dirty = true;
                    }
                }
                Task::none()
            }
            Message::SftpModalSaveFile => {
                let (side, path, text_to_save, editable, loading, saving) =
                    match &self.sftp_drawer().plus.modal {
                        SftpModalState::FileViewer {
                            side,
                            path,
                            content,
                            editable,
                            loading,
                            saving,
                            ..
                        } => (*side, path.clone(), content.text(), *editable, *loading, *saving),
                        _ => return Task::none(),
                    };
                if !editable || loading || saving {
                    return Task::none();
                }
                match side {
                    SftpPaneSide::Local => {
                        match std::fs::write(&path, text_to_save.as_bytes()) {
                            Ok(()) => {
                                if let SftpModalState::FileViewer {
                                    dirty,
                                    saving,
                                    error,
                                    ..
                                } = &mut self.active_sftp_drawer_mut().plus.modal
                                {
                                    *dirty = false;
                                    *saving = false;
                                    *error = None;
                                }
                                self.active_sftp_drawer_mut()
                                    .plus
                                    .refresh_local(home_dir.as_deref());
                            }
                            Err(err) => {
                                if let SftpModalState::FileViewer { error, .. } =
                                    &mut self.active_sftp_drawer_mut().plus.modal
                                {
                                    *error = Some(format!("save failed: {err}"));
                                }
                            }
                        }
                        Task::none()
                    }
                    SftpPaneSide::Remote => {
                        let Some((session_id, endpoint)) = self.active_ssh_endpoint() else {
                            if let SftpModalState::FileViewer { error, .. } =
                                &mut self.active_sftp_drawer_mut().plus.modal
                            {
                                *error = Some("SSH session is disconnected".to_string());
                            }
                            return Task::none();
                        };
                        if let SftpModalState::FileViewer { saving, error, .. } =
                            &mut self.active_sftp_drawer_mut().plus.modal
                        {
                            *saving = true;
                            *error = None;
                        }
                        let manager = self.sftp_manager.clone();
                        let save_path = path.clone();
                        Task::perform(
                            async move {
                                manager
                                    .write_text_file(&endpoint, &save_path, &text_to_save)
                                    .await
                                    .map_err(|e| e.to_string())
                            },
                            move |result| Message::SftpRemoteFileSaveResult {
                                session_id,
                                path,
                                result,
                            },
                        )
                    }
                }
            }
            Message::SftpRemoteFileReadResult {
                session_id,
                request_path,
                editable: _,
                result,
            } => {
                let Some(drawer) = self.sftp_drawers.get_mut(&session_id) else {
                    return Task::none();
                };
                let mut reconnect_err: Option<String> = None;
                let mut recovered_ok = false;
                if let SftpModalState::FileViewer {
                    side: SftpPaneSide::Remote,
                    path,
                    loading,
                    dirty,
                    error,
                    content,
                    ..
                } = &mut drawer.plus.modal
                    && *path == request_path
                {
                    *loading = false;
                    match result {
                        Ok(text) => {
                            *content = iced::widget::text_editor::Content::with_text(&text);
                            *dirty = false;
                            *error = None;
                            recovered_ok = true;
                        }
                        Err(err) => {
                            if is_sftp_connection_error_message(&err) {
                                reconnect_err = Some(err.clone());
                            }
                            *error = Some(err);
                        }
                    }
                }
                if recovered_ok {
                    drawer.mark_connected_or_recovered(Instant::now());
                    self.sync_sftp_status_notice();
                } else if let Some(err) = reconnect_err {
                    return self.trigger_sftp_auto_reconnect(session_id, err);
                }
                Task::none()
            }
            Message::SftpRemoteFileSaveResult {
                session_id,
                path: saved_path,
                result,
            } => {
                let Some(drawer) = self.sftp_drawers.get_mut(&session_id) else {
                    return Task::none();
                };
                let mut reconnect_err: Option<String> = None;
                if let SftpModalState::FileViewer {
                    side: SftpPaneSide::Remote,
                    path,
                    saving,
                    dirty,
                    error,
                    ..
                } = &mut drawer.plus.modal
                    && *path == saved_path
                {
                    *saving = false;
                    match &result {
                        Ok(()) => {
                            *dirty = false;
                            *error = None;
                        }
                        Err(err) => {
                            if is_sftp_connection_error_message(err) {
                                reconnect_err = Some(err.clone());
                            }
                            *error = Some(err.clone());
                        }
                    }
                }
                if result.is_ok() {
                    drawer.mark_connected_or_recovered(Instant::now());
                    let remote_dir = drawer.current_path().to_string();
                    self.sync_sftp_status_notice();
                    return self.sftp_refresh_task(session_id, remote_dir);
                } else if let Some(err) = reconnect_err {
                    return self.trigger_sftp_auto_reconnect(session_id, err);
                }
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn execute_sftp_action(&mut self, action: SftpContextActionKind) -> Task<Message> {
        let home_dir = self.paths.home_dir().map(Path::to_path_buf);
        let side = self
            .sftp_drawer()
            .plus
            .context_menu
            .side
            .unwrap_or(self.sftp_drawer().plus.active_pane);
        self.active_sftp_drawer_mut().plus.context_menu.close();

        let visible = visible_pane_entries(self.sftp_drawer(), side);
        let selected_paths = pane_prefs(self.sftp_drawer(), side).selected_paths.clone();
        let selected: Vec<FileBrowserEntry> = visible
            .iter()
            .filter(|e| selected_paths.contains(&e.full_path))
            .cloned()
            .collect();

        match action {
            SftpContextActionKind::Transfer => {
                if selected.is_empty() {
                    return Task::none();
                }
                match side {
                    SftpPaneSide::Local => {
                        let Some(session_id) = self.active_ssh_session_id() else {
                            self.active_sftp_drawer_mut().set_browser_error(Some(
                                "Connect an SSH Session before uploading.".to_string(),
                            ));
                            return Task::none();
                        };
                        self.active_sftp_drawer_mut().plus.transfers_panel_open = true;
                        for entry in selected {
                            self.enqueue_sftp_upload(session_id, PathBuf::from(&entry.full_path));
                        }
                        Task::none()
                    }
                    SftpPaneSide::Remote => {
                        self.active_sftp_drawer_mut()
                            .plus
                            .ensure_local_initialized(home_dir.as_deref());
                        let local_dir = PathBuf::from(&self.sftp_drawer().plus.local.current_path);
                        self.active_sftp_drawer_mut().plus.transfers_panel_open = true;
                        let mut tasks = Vec::new();
                        for entry in selected {
                            let t = self.enqueue_sftp_download_to_dir(
                                entry.full_path,
                                entry.name,
                                entry.is_dir,
                                local_dir.clone(),
                            );
                            tasks.push(t);
                        }
                        Task::batch(tasks)
                    }
                }
            }
            SftpContextActionKind::View | SftpContextActionKind::Edit => {
                let Some(entry) = selected.into_iter().find(|e| !e.is_dir) else {
                    return Task::none();
                };
                let editable = matches!(action, SftpContextActionKind::Edit);
                self.open_sftp_file_viewer(side, entry, editable)
            }
            SftpContextActionKind::Copy | SftpContextActionKind::Cut => {
                if selected.is_empty() {
                    return Task::none();
                }
                let op = if matches!(action, SftpContextActionKind::Cut) {
                    SftpClipboardOp::Cut
                } else {
                    SftpClipboardOp::Copy
                };
                self.active_sftp_drawer_mut().plus.clipboard = Some(SftpClipboard {
                    op,
                    source_side: side,
                    entries: selected,
                });
                Task::none()
            }
            SftpContextActionKind::Paste => {
                let Some(clip) = self.sftp_drawer().plus.clipboard.clone() else {
                    return Task::none();
                };
                self.handle_sftp_paste(side, clip)
            }
            SftpContextActionKind::Rename => {
                if selected.len() != 1 {
                    return Task::none();
                }
                let entry = &selected[0];
                self.active_sftp_drawer_mut().plus.modal = SftpModalState::Rename {
                    side,
                    old_path: entry.full_path.clone(),
                    new_name: entry.name.clone(),
                    error: None,
                };
                iced::widget::operation::focus(iced::widget::Id::new("sftp-plus-modal-input"))
            }
            SftpContextActionKind::Delete => {
                if selected.is_empty() {
                    return Task::none();
                }
                match side {
                    SftpPaneSide::Local => {
                        for entry in &selected {
                            if let Err(err) =
                                remove_local_path(Path::new(&entry.full_path), entry.is_dir)
                            {
                                self.active_sftp_drawer_mut().plus.local.error = Some(err);
                                break;
                            }
                        }
                        self.active_sftp_drawer_mut()
                            .plus
                            .refresh_local(home_dir.as_deref());
                        Task::none()
                    }
                    SftpPaneSide::Remote => {
                        let Some((session_id, endpoint)) = self.active_ssh_endpoint() else {
                            self.active_sftp_drawer_mut().set_browser_error(Some(
                                "Connect an SSH Session before deleting remote items.".to_string(),
                            ));
                            return Task::none();
                        };
                        let manager = self.sftp_manager.clone();
                        Task::perform(
                            async move {
                                for entry in &selected {
                                    if entry.is_dir && !entry.is_symlink {
                                        manager
                                            .remove_dir_recursive(&endpoint, &entry.full_path)
                                            .await
                                            .map_err(|e| e.to_string())?;
                                    } else {
                                        manager
                                            .remove_file(&endpoint, &entry.full_path)
                                            .await
                                            .map_err(|e| e.to_string())?;
                                    }
                                }
                                Ok(())
                            },
                            move |result| Message::SftpMutationResult { session_id, result },
                        )
                    }
                }
            }
            SftpContextActionKind::EditPermissions => {
                if selected.is_empty() {
                    return Task::none();
                }
                let default_bits = selected[0].permissions.unwrap_or(if selected[0].is_dir {
                    0o755
                } else {
                    0o644
                }) & 0o777;
                let target_paths = selected.into_iter().map(|e| e.full_path).collect();
                self.active_sftp_drawer_mut().plus.modal = SftpModalState::EditPermissions {
                    side,
                    target_paths,
                    octal_input: format_octal_mode(default_bits),
                    mode_bits: default_bits,
                    error: None,
                };
                Task::none()
            }
            SftpContextActionKind::Properties => {
                if selected.is_empty() {
                    return Task::none();
                }
                let total_selected_bytes =
                    selected.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
                let extra_selected = selected.len().saturating_sub(1);
                let entry = selected.into_iter().next().unwrap();
                self.active_sftp_drawer_mut().plus.modal = SftpModalState::Properties {
                    side,
                    entry,
                    extra_selected,
                    total_selected_bytes,
                };
                Task::none()
            }
            SftpContextActionKind::NewFolder | SftpContextActionKind::NewFile => {
                let parent_dir = match side {
                    SftpPaneSide::Local => self.sftp_drawer().plus.local.current_path.clone(),
                    SftpPaneSide::Remote => self.sftp_drawer().current_path().to_string(),
                };
                self.active_sftp_drawer_mut().plus.modal = SftpModalState::CreateEntry {
                    side,
                    parent_dir,
                    is_dir: matches!(action, SftpContextActionKind::NewFolder),
                    name: String::new(),
                    error: None,
                };
                iced::widget::operation::focus(iced::widget::Id::new("sftp-plus-modal-input"))
            }
            SftpContextActionKind::Refresh => {
                let current = match side {
                    SftpPaneSide::Local => self.sftp_drawer().plus.local.current_path.clone(),
                    SftpPaneSide::Remote => self.sftp_drawer().current_path().to_string(),
                };
                self.sftp_navigate_pane(side, current)
            }
            SftpContextActionKind::SelectAll => {
                let all_paths: Vec<String> = visible.into_iter().map(|e| e.full_path).collect();
                let prefs = pane_prefs_mut(self.active_sftp_drawer_mut(), side);
                prefs.selected_paths.extend(all_paths);
                Task::none()
            }
            SftpContextActionKind::InvertSelection => {
                let all_paths: Vec<String> = visible.into_iter().map(|e| e.full_path).collect();
                let prefs = pane_prefs_mut(self.active_sftp_drawer_mut(), side);
                for p in all_paths {
                    if !prefs.selected_paths.insert(p.clone()) {
                        prefs.selected_paths.remove(&p);
                    }
                }
                Task::none()
            }
            SftpContextActionKind::CopyPath => {
                let text = if selected.is_empty() {
                    match side {
                        SftpPaneSide::Local => self.sftp_drawer().plus.local.current_path.clone(),
                        SftpPaneSide::Remote => self.sftp_drawer().current_path().to_string(),
                    }
                } else {
                    selected
                        .into_iter()
                        .map(|e| e.full_path)
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.copy_text_to_clipboard(text)
            }
        }
    }

    fn handle_sftp_paste(
        &mut self,
        target_side: SftpPaneSide,
        clip: SftpClipboard,
    ) -> Task<Message> {
        let home_dir = self.paths.home_dir().map(Path::to_path_buf);
        match (clip.source_side, target_side) {
            (SftpPaneSide::Local, SftpPaneSide::Local) => {
                let dst_dir = PathBuf::from(&self.sftp_drawer().plus.local.current_path);
                for entry in &clip.entries {
                    let src = Path::new(&entry.full_path);
                    let dst = if clip.op == SftpClipboardOp::Copy
                        && src.parent() == Some(dst_dir.as_path())
                    {
                        unique_local_copy_path(&dst_dir, &entry.name)
                    } else {
                        dst_dir.join(&entry.name)
                    };
                    let res = if clip.op == SftpClipboardOp::Cut {
                        if src == dst {
                            Ok(())
                        } else if std::fs::rename(src, &dst).is_ok() {
                            Ok(())
                        } else {
                            copy_local_path_recursive(src, &dst)
                                .and_then(|()| remove_local_path(src, entry.is_dir))
                        }
                    } else {
                        copy_local_path_recursive(src, &dst)
                    };
                    if let Err(err) = res {
                        self.active_sftp_drawer_mut().plus.local.error = Some(err);
                        break;
                    }
                }
                if clip.op == SftpClipboardOp::Cut {
                    self.active_sftp_drawer_mut().plus.clipboard = None;
                }
                self.active_sftp_drawer_mut()
                    .plus
                    .refresh_local(home_dir.as_deref());
                Task::none()
            }
            (SftpPaneSide::Remote, SftpPaneSide::Remote) => {
                let Some((session_id, endpoint)) = self.active_ssh_endpoint() else {
                    self.active_sftp_drawer_mut().set_browser_error(Some(
                        "Connect an SSH Session before pasting remote items.".to_string(),
                    ));
                    return Task::none();
                };
                let dst_dir = self.sftp_drawer().current_path().to_string();
                let op = clip.op;
                if op == SftpClipboardOp::Cut {
                    self.active_sftp_drawer_mut().plus.clipboard = None;
                }
                let manager = self.sftp_manager.clone();
                let entries = clip.entries;
                Task::perform(
                    async move {
                        for entry in entries {
                            let dst = join_remote_path(&dst_dir, &entry.name);
                            if entry.full_path == dst {
                                if op == SftpClipboardOp::Copy {
                                    if entry.is_dir {
                                        return Err(
                                            "Remote directory duplication in the same folder is not supported over SFTP."
                                                .to_string(),
                                        );
                                    }
                                    let copy_dst = format!("{dst}.copy");
                                    manager
                                        .copy_file(&endpoint, &entry.full_path, &copy_dst)
                                        .await
                                        .map_err(|e| e.to_string())?;
                                }
                                continue;
                            }
                            if op == SftpClipboardOp::Cut {
                                manager
                                    .rename(&endpoint, &entry.full_path, &dst)
                                    .await
                                    .map_err(|e| e.to_string())?;
                            } else if entry.is_dir {
                                return Err(
                                    "Remote directory copy over SFTP is not supported; use Cut/Move or transfer via Local."
                                        .to_string(),
                                );
                            } else {
                                manager
                                    .copy_file(&endpoint, &entry.full_path, &dst)
                                    .await
                                    .map_err(|e| e.to_string())?;
                            }
                        }
                        Ok(())
                    },
                    move |result| Message::SftpMutationResult { session_id, result },
                )
            }
            (SftpPaneSide::Local, SftpPaneSide::Remote) => {
                let Some(session_id) = self.active_ssh_session_id() else {
                    self.active_sftp_drawer_mut().set_browser_error(Some(
                        "Connect an SSH Session before uploading.".to_string(),
                    ));
                    return Task::none();
                };
                self.active_sftp_drawer_mut().plus.transfers_panel_open = true;
                for entry in clip.entries {
                    self.enqueue_sftp_upload(session_id, PathBuf::from(&entry.full_path));
                }
                if clip.op == SftpClipboardOp::Cut {
                    self.active_sftp_drawer_mut().plus.clipboard = None;
                }
                Task::none()
            }
            (SftpPaneSide::Remote, SftpPaneSide::Local) => {
                self.active_sftp_drawer_mut()
                    .plus
                    .ensure_local_initialized(home_dir.as_deref());
                let local_dir = PathBuf::from(&self.sftp_drawer().plus.local.current_path);
                self.active_sftp_drawer_mut().plus.transfers_panel_open = true;
                if clip.op == SftpClipboardOp::Cut {
                    self.active_sftp_drawer_mut().plus.clipboard = None;
                }
                let mut tasks = Vec::new();
                for entry in clip.entries {
                    tasks.push(self.enqueue_sftp_download_to_dir(
                        entry.full_path,
                        entry.name,
                        entry.is_dir,
                        local_dir.clone(),
                    ));
                }
                Task::batch(tasks)
            }
        }
    }

    fn open_sftp_file_viewer(
        &mut self,
        side: SftpPaneSide,
        entry: FileBrowserEntry,
        editable: bool,
    ) -> Task<Message> {
        match side {
            SftpPaneSide::Local => {
                const MAX_LOCAL_VIEW_BYTES: usize = 2 * 1024 * 1024;
                let bytes = match std::fs::read(&entry.full_path) {
                    Ok(b) => b,
                    Err(err) => {
                        self.active_sftp_drawer_mut().plus.modal = SftpModalState::FileViewer {
                            side,
                            path: entry.full_path,
                            file_name: entry.name,
                            editable,
                            loading: false,
                            saving: false,
                            dirty: false,
                            error: Some(format!("failed to read file: {err}")),
                            content: iced::widget::text_editor::Content::new(),
                        };
                        return Task::none();
                    }
                };
                if bytes.len() > MAX_LOCAL_VIEW_BYTES {
                    self.active_sftp_drawer_mut().plus.modal = SftpModalState::FileViewer {
                        side,
                        path: entry.full_path,
                        file_name: entry.name,
                        editable,
                        loading: false,
                        saving: false,
                        dirty: false,
                        error: Some(format!(
                            "File exceeds 2 MiB preview limit ({} bytes)",
                            bytes.len()
                        )),
                        content: iced::widget::text_editor::Content::new(),
                    };
                    return Task::none();
                }
                if bytes.contains(&0) {
                    self.active_sftp_drawer_mut().plus.modal = SftpModalState::FileViewer {
                        side,
                        path: entry.full_path,
                        file_name: entry.name,
                        editable,
                        loading: false,
                        saving: false,
                        dirty: false,
                        error: Some("Binary files cannot be opened in text view/edit".to_string()),
                        content: iced::widget::text_editor::Content::new(),
                    };
                    return Task::none();
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                self.active_sftp_drawer_mut().plus.modal = SftpModalState::FileViewer {
                    side,
                    path: entry.full_path,
                    file_name: entry.name,
                    editable,
                    loading: false,
                    saving: false,
                    dirty: false,
                    error: None,
                    content: iced::widget::text_editor::Content::with_text(&text),
                };
                Task::none()
            }
            SftpPaneSide::Remote => {
                let Some((session_id, endpoint)) = self.active_ssh_endpoint() else {
                    self.active_sftp_drawer_mut().set_browser_error(Some(
                        "Connect an SSH Session before opening remote files.".to_string(),
                    ));
                    return Task::none();
                };
                let req_path = entry.full_path.clone();
                self.active_sftp_drawer_mut().plus.modal = SftpModalState::FileViewer {
                    side,
                    path: entry.full_path,
                    file_name: entry.name,
                    editable,
                    loading: true,
                    saving: false,
                    dirty: false,
                    error: None,
                    content: iced::widget::text_editor::Content::new(),
                };
                let manager = self.sftp_manager.clone();
                let read_path = req_path.clone();
                Task::perform(
                    async move {
                        manager
                            .read_text_file(&endpoint, &read_path, 2 * 1024 * 1024)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    move |result| Message::SftpRemoteFileReadResult {
                        session_id,
                        request_path: req_path,
                        editable,
                        result,
                    },
                )
            }
        }
    }

    fn navigate_sftp_to(&mut self, path: String) -> Task<Message> {
        let Some(sid) = self.active_ssh_session_id() else {
            self.sftp_fallback_drawer.set_current_path(path);
            return Task::none();
        };
        let offline = self
            .sessions
            .get(&sid)
            .and_then(SessionEntry::as_ssh)
            .is_none_or(|slot| slot.active.is_none());
        let task = self.sftp_refresh_task(sid, path);
        if offline {
            let cur = self
                .sftp_drawers
                .get(&sid)
                .map(|d| d.current_path().to_string());
            if let Some(dir) = cur
                && dir != "."
            {
                self.record_profile_sftp_dir(sid, &dir);
            }
        }
        task
    }

    fn sftp_refresh_task(&mut self, session_id: SessionId, path: String) -> Task<Message> {
        let endpoint = {
            let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
                return Task::none();
            };
            if slot.active.is_none() {
                self.ensure_session_sftp_drawer(session_id)
                    .set_current_path(path);
                return Task::none();
            }
            slot.sftp_endpoint(&self.vault)
        };
        let (generation, target_path) = {
            let drawer = self.ensure_session_sftp_drawer(session_id);
            let generation = drawer.begin_listing(session_id, &path);
            let target_path = drawer.current_path().to_string();
            (generation, target_path)
        };
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
        let folder = self.sftp_drawer().new_folder_name().trim().to_string();
        if folder.is_empty() {
            return Task::none();
        }
        let Some(session_id) = self.active_ssh_session_id() else {
            self.active_sftp_drawer_mut().set_browser_error(Some(
                "Connect an SSH Session before creating a remote folder.".to_string(),
            ));
            return Task::none();
        };
        let endpoint = {
            let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
                return Task::none();
            };
            if slot.active.is_none() {
                self.ensure_session_sftp_drawer(session_id)
                    .set_browser_error(Some(
                        "Connect an SSH Session before creating a remote folder.".to_string(),
                    ));
                return Task::none();
            }
            slot.sftp_endpoint(&self.vault)
        };
        let target = {
            let drawer = self.ensure_session_sftp_drawer(session_id);
            let path = join_remote_path(drawer.current_path(), &folder);
            drawer.set_new_folder_name(String::new());
            path
        };
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
        let Some(session_id) = self.active_ssh_session_id() else {
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

    fn reset_transfer_conflict_override_if_idle(&self) {
        let snap = self.transfer_queue.snapshot();
        if !snap.has_incomplete_work() && snap.awaiting_conflict_count == 0 {
            self.transfer_queue.reset_conflict_policy_override();
        }
    }

    fn record_dir_transfer_notices_for(
        &mut self,
        session_id: Option<SessionId>,
        summary: &DirectoryTransferSummary,
    ) {
        let notice = if let Some(first_skipped) = summary.skipped_symlinks.first() {
            Some(
                TransferNotice::SymlinkSkipped {
                    path: first_skipped.clone(),
                    is_loop: !summary.skipped_symlink_loops.is_empty(),
                }
                .summary(),
            )
        } else {
            summary
                .permission_notices
                .first()
                .map(TransferNotice::summary)
        };
        if let Some(msg) = notice {
            if let Some(sid) = session_id {
                self.ensure_session_sftp_drawer(sid)
                    .set_last_transfer_notice(Some(msg));
            } else {
                self.active_sftp_drawer_mut()
                    .set_last_transfer_notice(Some(msg));
            }
        }
    }

    fn enqueue_sftp_upload(&mut self, session_id: SessionId, local_path: PathBuf) {
        let endpoint = {
            let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
                return;
            };
            slot.sftp_endpoint(&self.vault)
        };
        self.reset_transfer_conflict_override_if_idle();
        let file_name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload.bin".to_string());
        let remote_dir = self
            .sftp_drawers
            .get(&session_id)
            .map(|d| d.target_remote_dir_for(session_id))
            .unwrap_or_else(|| ".".to_string());
        let remote_path = if remote_dir == "." || remote_dir.is_empty() {
            file_name
        } else {
            join_remote_path(&remote_dir, &file_name)
        };
        match self
            .transfer_queue
            .enqueue_upload_path(endpoint, local_path, remote_path)
        {
            Ok(summary) => {
                for id in &summary.transfer_ids {
                    self.transfer_sessions.insert(*id, session_id);
                }
                self.record_dir_transfer_notices_for(Some(session_id), &summary);
                self.sync_transfer_telemetry();
            }
            Err(err) => {
                self.ensure_session_sftp_drawer(session_id)
                    .set_browser_error(Some(err.to_string()));
            }
        }
    }

    fn enqueue_sftp_download(&mut self, remote_path: String, file_name: String) {
        if let Err(err) = validate_remote_entry_name(&file_name) {
            self.active_sftp_drawer_mut()
                .set_browser_error(Some(err.to_string()));
            return;
        }
        let Some(session_id) = self.active_ssh_session_id() else {
            self.active_sftp_drawer_mut().set_browser_error(Some(
                "Connect an SSH Session before downloading files.".to_string(),
            ));
            return;
        };
        let endpoint = {
            let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
                return;
            };
            slot.sftp_endpoint(&self.vault)
        };
        self.reset_transfer_conflict_override_if_idle();
        let local_path = self.sftp_download_dir.join(&file_name);
        let id = self
            .transfer_queue
            .enqueue_download(endpoint, remote_path, local_path);
        self.transfer_sessions.insert(id, session_id);
        self.sync_transfer_telemetry();
    }

    fn enqueue_sftp_download_dir(&mut self, remote_path: String, dir_name: String) -> Task<Message> {
        if let Err(err) = validate_remote_entry_name(&dir_name) {
            self.active_sftp_drawer_mut()
                .set_browser_error(Some(err.to_string()));
            return Task::none();
        }
        let Some(session_id) = self.active_ssh_session_id() else {
            self.active_sftp_drawer_mut().set_browser_error(Some(
                "Connect an SSH Session before downloading folders.".to_string(),
            ));
            return Task::none();
        };
        let endpoint = {
            let Some(SessionEntry::Ssh(slot)) = self.sessions.get(&session_id) else {
                return Task::none();
            };
            slot.sftp_endpoint(&self.vault)
        };
        self.reset_transfer_conflict_override_if_idle();
        let local_dir = self.sftp_download_dir.join(&dir_name);
        let queue = self.transfer_queue.clone();
        Task::perform(
            async move {
                queue
                    .enqueue_download_dir(endpoint, remote_path, local_dir)
                    .await
                    .map_err(|err| err.to_string())
            },
            move |result| Message::SftpSessionDirEnqueueResult { session_id, result },
        )
    }

    fn handle_dir_enqueue_result(
        &mut self,
        session_id: Option<SessionId>,
        result: Result<DirectoryTransferSummary, String>,
    ) {
        match result {
            Ok(summary) => {
                if let Some(sid) = session_id {
                    for id in &summary.transfer_ids {
                        self.transfer_sessions.insert(*id, sid);
                    }
                } else if !summary.transfer_ids.is_empty() {
                    let snap = self.transfer_queue.snapshot();
                    for id in &summary.transfer_ids {
                        if !self.transfer_sessions.contains_key(id)
                            && let Some(item) = snap.transfers.iter().find(|t| t.id == *id)
                            && let Some(sid) = self.infer_transfer_session_id(item)
                        {
                            self.transfer_sessions.insert(*id, sid);
                        }
                    }
                }
                let target_sid = session_id.or_else(|| self.active_ssh_session_id());
                self.record_dir_transfer_notices_for(target_sid, &summary);
                self.sync_transfer_telemetry();
            }
            Err(err) => {
                if let Some(sid) = session_id {
                    self.ensure_session_sftp_drawer(sid)
                        .set_browser_error(Some(err));
                } else {
                    self.active_sftp_drawer_mut().set_browser_error(Some(err));
                }
            }
        }
    }

    fn infer_transfer_session_id(&self, item: &TransferSnapshot) -> Option<SessionId> {
        if let Some(&sid) = self.transfer_sessions.get(&item.id) {
            return Some(sid);
        }
        if let Some(active_sid) = self.active_ssh_session_id()
            && self
                .sessions
                .get(&active_sid)
                .and_then(SessionEntry::as_ssh)
                .is_some_and(|slot| slot.sftp_key() == item.profile_key)
        {
            return Some(active_sid);
        }
        self.sessions.iter().find_map(|(&sid, entry)| {
            entry
                .as_ssh()
                .filter(|slot| slot.sftp_key() == item.profile_key)
                .map(|_| sid)
        })
    }

    fn transfer_belongs_to_session(
        transfer_sessions: &BTreeMap<TransferId, SessionId>,
        item: &TransferSnapshot,
        sid: SessionId,
        session_key: Option<&ProfileTransportKey>,
    ) -> bool {
        if let Some(&owner_sid) = transfer_sessions.get(&item.id) {
            owner_sid == sid
        } else {
            session_key == Some(&item.profile_key)
        }
    }

    fn clear_finished_transfers_for_active_drawer(&mut self) {
        if let Some(sid) = self.active_ssh_session_id() {
            let session_key = self
                .sessions
                .get(&sid)
                .and_then(SessionEntry::as_ssh)
                .map(SshSessionSlot::sftp_key);
            let snap = self.transfer_queue.snapshot();
            let matching_ids: BTreeSet<TransferId> = snap
                .transfers
                .iter()
                .filter(|t| {
                    matches!(
                        t.status,
                        TransferStatus::Completed | TransferStatus::Skipped | TransferStatus::Cancelled
                    ) && Self::transfer_belongs_to_session(
                        &self.transfer_sessions,
                        t,
                        sid,
                        session_key.as_ref(),
                    )
                })
                .map(|t| t.id)
                .collect();
            self.transfer_queue
                .clear_finished_matching(|t| matching_ids.contains(&t.id));
            for id in &matching_ids {
                self.transfer_sessions.remove(id);
            }
        } else {
            self.transfer_queue.clear_finished();
            let remaining: BTreeSet<TransferId> = self
                .transfer_queue
                .snapshot()
                .transfers
                .into_iter()
                .map(|t| t.id)
                .collect();
            self.transfer_sessions.retain(|id, _| remaining.contains(id));
        }
        self.sync_transfer_telemetry();
    }

    fn sync_transfer_telemetry(&mut self) {
        let snap = self.transfer_queue.snapshot();
        self.sync_transfer_telemetry_from(snap);
    }

    fn sync_transfer_telemetry_from(&mut self, snap: TransferQueueSnapshot) {
        self.background_work_tracker
            .set_active_transfers(snap.active_count + snap.queued_count);
        let drawer_open = self.sftp_drawer().is_open();
        self.status_bar.set_transfers(TransferSummaryHook {
            active: snap.active_count,
            queued: snap.queued_count,
            failed: snap.failed_count,
            bytes_done: snap.bytes_done,
            bytes_total: snap.bytes_total,
            bytes_per_sec: (snap.total_bytes_per_sec > 0).then_some(snap.total_bytes_per_sec),
            drawer_open,
        });
        let session_keys: Vec<(SessionId, ProfileTransportKey)> = self
            .sftp_drawers
            .keys()
            .filter_map(|&sid| {
                self.sessions
                    .get(&sid)
                    .and_then(SessionEntry::as_ssh)
                    .map(|slot| (sid, slot.sftp_key()))
            })
            .collect();
        for (sid, key) in session_keys {
            let filtered = snap.filtered(|t| {
                Self::transfer_belongs_to_session(&self.transfer_sessions, t, sid, Some(&key))
            });
            if let Some(drawer) = self.sftp_drawers.get_mut(&sid) {
                drawer.set_queue_snapshot(filtered);
            }
        }
        self.sftp_fallback_drawer.set_queue_snapshot(snap);
        self.sync_sftp_status_notice();
    }

    fn sync_sftp_status_notice(&mut self) {
        let notice = sftp_recovery_notice_for_drawer(self.sftp_drawer());
        self.status_bar.set_sftp_notice(notice);
    }

    fn trigger_sftp_auto_reconnect(&mut self, session_id: SessionId, err: String) -> Task<Message> {
        let Some(key) = self
            .sessions
            .get(&session_id)
            .and_then(SessionEntry::as_ssh)
            .map(SshSessionSlot::sftp_key)
        else {
            return Task::none();
        };
        let Some(drawer) = self.sftp_drawers.get_mut(&session_id) else {
            return Task::none();
        };
        let prev_attempt = match drawer.connection_state() {
            SftpConnectionState::AutoReconnecting { attempt, .. } => *attempt,
            _ => 0,
        };
        let next_attempt = prev_attempt.saturating_add(1);
        if next_attempt > 5 {
            drawer.set_connection_state(SftpConnectionState::Disconnected {
                reason: err.clone(),
            });
            drawer.set_browser_error(Some(err));
            self.sync_sftp_status_notice();
            return Task::none();
        }
        let delay_secs = (1u64 << (next_attempt - 1).min(4)).min(16);
        let retry_generation =
            drawer.mark_auto_reconnecting(next_attempt, Some(delay_secs), err.clone());
        self.sftp_manager.pool().invalidate(&key);
        self.sftp_manager
            .mark_reconnecting(&key, next_attempt, err);
        self.sync_sftp_status_notice();
        Task::perform(
            async move {
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            },
            move |_| Message::SftpAutoReconnectTick {
                session_id,
                retry_generation,
            },
        )
    }

    fn has_active_reconnect_notices(&self) -> bool {
        self.tunnel_ui.has_active_reconnected_notices()
            || self
                .sftp_drawers
                .values()
                .any(SftpDrawerState::has_active_reconnected_notice)
            || self.sftp_fallback_drawer.has_active_reconnected_notice()
            || self.sessions.values().any(|entry| match entry {
                SessionEntry::Ssh(s) => s.reconnected_at.is_some(),
                SessionEntry::Serial(s) => s.reconnected_at.is_some(),
                SessionEntry::Telnet(s) => s.reconnected_at.is_some(),
                SessionEntry::Local(_) => false,
            })
    }

    fn clear_expired_reconnect_notices(&mut self, now: Instant) {
        for entry in self.sessions.values_mut() {
            match entry {
                SessionEntry::Ssh(s) => {
                    if let Some(ts) = s.reconnected_at
                        && now.saturating_duration_since(ts) >= RECONNECT_NOTICE_TTL
                    {
                        s.reconnected_at = None;
                    }
                }
                SessionEntry::Serial(s) => {
                    if let Some(ts) = s.reconnected_at
                        && now.saturating_duration_since(ts) >= RECONNECT_NOTICE_TTL
                    {
                        s.reconnected_at = None;
                    }
                }
                SessionEntry::Telnet(s) => {
                    if let Some(ts) = s.reconnected_at
                        && now.saturating_duration_since(ts) >= RECONNECT_NOTICE_TTL
                    {
                        s.reconnected_at = None;
                    }
                }
                SessionEntry::Local(_) => {}
            }
        }
        if self.tunnel_ui.expire_reconnected_notices(now) {
            self.sync_tunnel_telemetry();
        }
        for drawer in self.sftp_drawers.values_mut() {
            drawer.expire_reconnected_notice(now);
        }
        self.sftp_fallback_drawer.expire_reconnected_notice(now);
        self.sync_sftp_status_notice();
    }

    fn sync_tunnel_telemetry(&mut self) {
        let mgr_snap = self.tunnel_manager.snapshot();
        self.sync_tunnel_telemetry_from(mgr_snap);
    }

    fn sync_tunnel_telemetry_from(&mut self, mgr_snap: TunnelManagerSnapshot) {
        self.tunnel_ui
            .sync_from_manager_snapshot(&mgr_snap, Instant::now());
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
        let summary = TunnelSummaryHook::from_entries(&entries);
        let tunnel_notice =
            if summary.retrying > 0 || summary.reconnecting > 0 || summary.holding > 0 {
                let recovering = summary.retrying + summary.reconnecting + summary.holding;
                Some(StatusRecoveryNotice {
                    kind: RecoveryNoticeKind::AutoReconnecting,
                    message: if recovering == 1 {
                        "Tunnel auto-reconnecting...".to_string()
                    } else {
                        format!("{recovering} tunnels auto-reconnecting...")
                    },
                })
            } else if entries.iter().any(|e| e.reconnected_notice.is_some()) {
                Some(StatusRecoveryNotice {
                    kind: RecoveryNoticeKind::ReconnectedSuccessfully,
                    message: "Tunnel reconnected successfully".to_string(),
                })
            } else {
                None
            };
        self.status_bar.set_tunnel_notice(tunnel_notice);
        self.status_bar.set_tunnels(summary);
        self.sidebar.set_active_tunnels(entries);
        self.sync_tray_state();
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

    fn open_edit_tunnel_modal(&mut self, tunnel_id: &str) -> Task<Message> {
        self.context_menu.close();
        self.command_palette.close();
        let Some((rule, profile_id, session_id)) = self.find_tunnel_definition(tunnel_id) else {
            return Task::none();
        };
        self.tunnel_ui
            .create_form_mut()
            .open_for_edit(&rule, profile_id, session_id);
        operation::focus(tunnel_name_input_id())
    }

    fn open_tunnel_context_menu(&mut self, tunnel_id: &str, position: Point) {
        let Some(entry) = self
            .sidebar
            .active_tunnels()
            .iter()
            .find(|t| t.id == tunnel_id)
            .cloned()
        else {
            return;
        };
        let running = matches!(
            entry.state,
            crate::sidebar::TunnelRuntimeState::Starting
                | crate::sidebar::TunnelRuntimeState::Active
                | crate::sidebar::TunnelRuntimeState::Holding
                | crate::sidebar::TunnelRuntimeState::Reconnecting
                | crate::sidebar::TunnelRuntimeState::Retrying { .. }
        );
        let can_reconnect = matches!(
            entry.state,
            crate::sidebar::TunnelRuntimeState::Active
                | crate::sidebar::TunnelRuntimeState::Holding
                | crate::sidebar::TunnelRuntimeState::Reconnecting
                | crate::sidebar::TunnelRuntimeState::Retrying { .. }
                | crate::sidebar::TunnelRuntimeState::Error(_)
        );
        self.context_menu
            .open_tunnel(entry.id, entry.name, running, can_reconnect, position);
    }

    fn delete_tunnel(&mut self, tunnel_id: &str) {
        self.context_menu.close();
        if self.tunnel_ui.create_form().editing_tunnel_id.as_deref() == Some(tunnel_id) {
            self.tunnel_ui.create_form_mut().close();
        }
        self.tunnel_ui.clear_pending_public_bind_if(tunnel_id);
        self.tunnel_manager.remove_tunnel(tunnel_id);
        self.tunnel_ui.remove_ad_hoc(tunnel_id);

        let mut profiles_dirty = false;
        for profile in &mut self.profiles.profiles {
            let before = profile.tunnels.len();
            profile.tunnels.retain(|r| r.id != tunnel_id);
            if profile.tunnels.len() != before {
                profiles_dirty = true;
            }
        }
        if profiles_dirty && let Err(err) = save_profiles(&self.paths, &self.profiles) {
            warn!(error = %err, "failed removing tunnel rule from profiles.toml");
            self.last_error = Some(err.to_string());
        }
        self.sync_tunnel_telemetry();
    }

    fn submit_tunnel_create(&mut self) -> Task<Message> {
        let rule = match self.tunnel_ui.create_form().build_rule() {
            Ok(rule) => rule,
            Err(err) => {
                self.tunnel_ui.create_form_mut().validation_error = Some(err);
                return Task::none();
            }
        };

        let editing_id = self.tunnel_ui.create_form().editing_tunnel_id.clone();
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

        let was_running = if let Some(ref eid) = editing_id {
            let running = self
                .tunnel_manager
                .snapshot()
                .tunnels
                .iter()
                .any(|t| t.id == *eid && t.state.is_running());
            self.tunnel_manager.remove_tunnel(eid);
            self.tunnel_ui.remove_ad_hoc(eid);
            running
        } else {
            false
        };

        let mut profiles_dirty = false;
        if let Some(ref eid) = editing_id {
            for profile in &mut self.profiles.profiles {
                let before = profile.tunnels.len();
                profile.tunnels.retain(|r| r.id != *eid);
                if profile.tunnels.len() != before {
                    profiles_dirty = true;
                }
            }
        }

        let saved_to_profile = if let Some(pid) = profile_id.as_deref()
            && let Some(profile) = self.profiles.profiles.iter_mut().find(|p| p.id == pid)
        {
            profile.tunnels.push(rule.clone());
            profiles_dirty = true;
            true
        } else {
            false
        };

        if profiles_dirty && let Err(err) = save_profiles(&self.paths, &self.profiles) {
            warn!(error = %err, "failed saving tunnel rule to profiles.toml");
            self.last_error = Some(err.to_string());
        }
        if !saved_to_profile {
            self.tunnel_ui.add_ad_hoc(AdHocTunnelRule {
                rule: rule.clone(),
                profile_id: profile_id.clone(),
                session_id,
            });
        }

        let start_after_save = editing_id.is_none() || was_running;
        self.tunnel_ui.create_form_mut().close();

        if rule.requires_public_bind_opt_in() {
            self.tunnel_ui
                .prompt_public_bind(rule, profile_id, session_id, start_after_save);
            self.sync_tunnel_telemetry();
            return Task::none();
        }

        if start_after_save {
            self.start_tunnel_rule(rule, profile_id, session_id)
        } else {
            self.sync_tunnel_telemetry();
            Task::none()
        }
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
        } else {
            self.tunnel_ui.add_ad_hoc(AdHocTunnelRule {
                rule: pending.rule.clone(),
                profile_id: pending.profile_id.clone(),
                session_id: pending.session_id,
            });
        }

        if pending.start_after_confirm {
            self.start_tunnel_rule(pending.rule, pending.profile_id, pending.session_id)
        } else {
            self.sync_tunnel_telemetry();
            Task::none()
        }
    }

    fn toggle_tunnel(&mut self, tunnel_id: &str) -> Task<Message> {
        self.context_menu.close();
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

    fn try_reconnect_managed_tunnel(&mut self, tunnel_id: &str) {
        if let Err(err) = self.tunnel_manager.reconnect_tunnel(tunnel_id) {
            self.tunnel_ui.set_start_error(tunnel_id, Some(err.to_string()));
            self.last_error = Some(err.to_string());
        }
    }

    fn reconnect_tunnel(&mut self, tunnel_id: &str) -> Task<Message> {
        self.context_menu.close();
        let snap = self.tunnel_manager.snapshot();
        if let Some(existing) = snap.tunnels.iter().find(|t| t.id == tunnel_id) {
            if matches!(existing.state, termewnal_ssh::TunnelState::Retrying { .. }) {
                self.try_reconnect_managed_tunnel(tunnel_id);
                self.sync_tunnel_telemetry();
                return Task::none();
            }
            if existing.standalone {
                if existing.state.is_running() {
                    self.try_reconnect_managed_tunnel(tunnel_id);
                    self.sync_tunnel_telemetry();
                    return Task::none();
                }
            } else if let Some(sid) = existing.session_id {
                return self.reconnect_ssh_session(SessionId(sid));
            }
        }
        self.toggle_tunnel(tunnel_id)
    }

    fn reconnect_all_tunnels(&mut self) -> Task<Message> {
        let snap = self.tunnel_manager.snapshot();
        let mut sessions_to_reconnect = BTreeSet::new();
        let mut tasks = Vec::new();
        for t in &snap.tunnels {
            match (&t.state, t.standalone, t.session_id) {
                (termewnal_ssh::TunnelState::Retrying { .. }, _, _) => {
                    self.try_reconnect_managed_tunnel(&t.id);
                }
                (
                    termewnal_ssh::TunnelState::Starting
                    | termewnal_ssh::TunnelState::Active
                    | termewnal_ssh::TunnelState::Holding
                    | termewnal_ssh::TunnelState::Reconnecting,
                    true,
                    _,
                ) => {
                    self.try_reconnect_managed_tunnel(&t.id);
                }
                (termewnal_ssh::TunnelState::Error(_), true, _) => {
                    tasks.push(self.toggle_tunnel(&t.id));
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
            tasks.push(self.reconnect_ssh_session(sid));
        }
        self.sync_tunnel_telemetry();
        Task::batch(tasks)
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
            let task = spawn_or_task(fut, self.tunnel_manager.clone());
            self.sync_tunnel_telemetry();
            return task;
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
            && let Ok(spec) = SshTargetSpec::from_profile_with_sources(
                profile,
                &self.config,
                &self.profiles.profiles,
                &self.openssh_hosts,
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
        modifiers,
        ..
    }) = event
    else {
        return None;
    };

    if modifiers.control()
        && modifiers.shift()
        && !modifiers.alt()
        && matches!(key.as_ref(), Key::Character("e") | Key::Character("E"))
    {
        return Some(Message::ToggleSftpDrawer);
    }

    if modifiers.control() && !modifiers.shift() && !modifiers.alt() && !modifiers.logo() {
        match key.as_ref() {
            Key::Character("c") | Key::Character("C") => {
                return Some(Message::SftpShortcut(SftpContextActionKind::Copy));
            }
            Key::Character("x") | Key::Character("X") => {
                return Some(Message::SftpShortcut(SftpContextActionKind::Cut));
            }
            Key::Character("v") | Key::Character("V") => {
                return Some(Message::SftpShortcut(SftpContextActionKind::Paste));
            }
            Key::Character("a") | Key::Character("A") => {
                return Some(Message::SftpShortcut(SftpContextActionKind::SelectAll));
            }
            _ => {}
        }
    }

    match key {
        Key::Named(Named::Escape) => Some(Message::EscapePressed),
        Key::Named(Named::F3) => Some(if modifiers.shift() {
            Message::SearchPrev
        } else {
            Message::SearchNext
        }),
        Key::Named(Named::F2) if modifiers.is_empty() => {
            Some(Message::SftpShortcut(SftpContextActionKind::Rename))
        }
        Key::Named(Named::F5) if modifiers.is_empty() => {
            Some(Message::SftpShortcut(SftpContextActionKind::Refresh))
        }
        Key::Named(Named::Delete) if modifiers.is_empty() => {
            Some(Message::SftpShortcut(SftpContextActionKind::Delete))
        }
        _ => None,
    }
}

fn open_local_file_with_system(
    path: &Path,
    _line: Option<u32>,
    _col: Option<u32>,
) -> Result<(), String> {
    open::that_detached(path).map_err(|err| format!("failed to open {}: {err}", path.display()))
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
struct ServerStatsSubKey {
    session_id: SessionId,
    generation: u64,
    transport_rx: tokio::sync::watch::Receiver<Option<Arc<SshTransport>>>,
}

impl Hash for ServerStatsSubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.session_id.hash(state);
        self.generation.hash(state);
    }
}

fn server_stats_stream(
    key: &ServerStatsSubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let session_id = key.session_id;
    let generation = key.generation;
    let mut transport_rx = key.transport_rx.clone();
    Box::pin(iced::stream::channel(8, async move |mut output| {
        let mut collector = ServerStatsCollector::new();
        let mut consecutive_failures: u8 = 0;

        loop {
            let transport_opt = transport_rx.borrow_and_update().clone();
            let Some(transport) = transport_opt else {
                collector.reset();
                consecutive_failures = 0;
                if transport_rx.changed().await.is_err() {
                    break;
                }
                continue;
            };

            if transport.is_closed() {
                collector.reset();
                if transport_rx.changed().await.is_err() {
                    break;
                }
                continue;
            }

            let sample_res = tokio::select! {
                res = transport.collect_server_stats(&mut collector, Duration::from_secs(3)) => Some(res),
                changed = transport_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    None
                }
            };

            let Some(res) = sample_res else {
                collector.reset();
                consecutive_failures = 0;
                continue;
            };

            let delay = match res {
                Ok(stats) => {
                    consecutive_failures = 0;
                    let fast_followup =
                        stats.cpu_percent.is_none() || stats.net_rx_bytes_per_sec.is_none();
                    if output
                        .send(Message::ServerStatsUpdated {
                            session_id,
                            generation,
                            stats,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if fast_followup {
                        Duration::from_millis(500)
                    } else {
                        Duration::from_secs(2)
                    }
                }
                Err(_) => {
                    if transport.is_closed() {
                        collector.reset();
                        continue;
                    }
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures >= 3 {
                        let _ = output
                            .send(Message::ServerStatsUnavailable {
                                session_id,
                                generation,
                            })
                            .await;
                        break;
                    }
                    Duration::from_secs(2)
                }
            };

            tokio::select! {
                _ = tokio::time::sleep(delay) => {},
                changed = transport_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    collector.reset();
                    consecutive_failures = 0;
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

#[derive(Clone)]
struct SftpStatusSubKey {
    id: u64,
    manager: SftpManager,
}

impl Hash for SftpStatusSubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

fn sftp_status_stream(
    key: &SftpStatusSubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let manager = key.manager.clone();
    Box::pin(iced::stream::channel(32, async move |mut output| {
        let mut rx = manager.subscribe_status();
        let initial: Vec<(ProfileTransportKey, SftpTransportStatus)> = rx
            .borrow_and_update()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !initial.is_empty()
            && output
                .send(Message::SftpManagerStatusUpdated(initial))
                .await
                .is_err()
        {
            return;
        }
        while rx.changed().await.is_ok() {
            let entries: Vec<(ProfileTransportKey, SftpTransportStatus)> = rx
                .borrow_and_update()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if output
                .send(Message::SftpManagerStatusUpdated(entries))
                .await
                .is_err()
            {
                break;
            }
        }
    }))
}

#[derive(Clone)]
struct ProtocolStatusSubKey {
    session_id: SessionId,
    kind: SessionKindLabel,
    generation: u64,
    rx: tokio::sync::watch::Receiver<ProtocolStatus>,
}

impl Hash for ProtocolStatusSubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.session_id.hash(state);
        let kind_tag: u8 = match self.kind {
            SessionKindLabel::Serial => 1,
            SessionKindLabel::Telnet => 2,
            _ => 0,
        };
        kind_tag.hash(state);
        self.generation.hash(state);
    }
}

fn protocol_status_stream(
    key: &ProtocolStatusSubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let session_id = key.session_id;
    let kind = key.kind;
    let generation = key.generation;
    let mut rx = key.rx.clone();
    Box::pin(iced::stream::channel(32, async move |mut output| {
        let make_msg = |status: ProtocolStatus| match kind {
            SessionKindLabel::Serial => Message::SerialStatusChanged {
                session_id,
                generation,
                status,
            },
            SessionKindLabel::Telnet => Message::TelnetStatusChanged {
                session_id,
                generation,
                status,
            },
            _ => unreachable!("protocol_status_stream only handles Serial and Telnet"),
        };
        let initial = rx.borrow_and_update().clone();
        if output.send(make_msg(initial)).await.is_err() {
            return;
        }
        while rx.changed().await.is_ok() {
            let status = rx.borrow_and_update().clone();
            if output.send(make_msg(status)).await.is_err() {
                break;
            }
        }
    }))
}

#[derive(Clone)]
struct TraySubKey {
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<TrayAction>>>>,
}

impl Hash for TraySubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.rx) as usize).hash(state);
    }
}

fn tray_event_stream(key: &TraySubKey) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let rx_slot = Arc::clone(&key.rx);
    Box::pin(iced::stream::channel(32, async move |mut output| {
        let maybe_rx = rx_slot.lock().ok().and_then(|mut g| g.take());
        if let Some(mut rx) = maybe_rx {
            while let Some(action) = rx.recv().await {
                if output.send(Message::TrayAction(action)).await.is_err() {
                    break;
                }
            }
        }
    }))
}

#[derive(Clone)]
struct HotkeySubKey {
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<()>>>>,
}

impl Hash for HotkeySubKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.rx) as usize).hash(state);
    }
}

fn hotkey_event_stream(
    key: &HotkeySubKey,
) -> std::pin::Pin<Box<dyn Stream<Item = Message> + Send>> {
    let rx_slot = Arc::clone(&key.rx);
    Box::pin(iced::stream::channel(16, async move |mut output| {
        let maybe_rx = rx_slot.lock().ok().and_then(|mut g| g.take());
        if let Some(mut rx) = maybe_rx {
            while rx.recv().await.is_some() {
                if output.send(Message::GlobalHotkeyTriggered).await.is_err() {
                    break;
                }
            }
        }
    }))
}

fn map_window_event((id, event): (window::Id, window::Event)) -> Option<Message> {
    match event {
        window::Event::Focused => Some(Message::WindowFocused(id)),
        window::Event::Unfocused => Some(Message::WindowUnfocused(id)),
        window::Event::Resized(size) => Some(Message::WindowResized(id, size)),
        window::Event::FileHovered(path) => Some(Message::ExternalFileHovered(path)),
        window::Event::FilesHoveredLeft => Some(Message::ExternalFilesLeft),
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
            Key::Character("k") | Key::Character("K") => Some(Message::ToggleCommandPalette),
            Key::Character(",") => Some(Message::OpenSettingsModal),
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
                    | Message::ToggleSftpDrawer
                    | Message::OpenSettingsModal
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

fn ssh_target_from_profile(profile: &Profile) -> Option<SshSessionTarget> {
    let ProfileTarget::Ssh {
        host,
        port,
        username,
        auth,
        proxy_jump,
        proxy_command,
        proxy,
        forward_agent,
        forward_x11,
        auto_connect_on_startup,
        tmux_reattach,
        keepalive_interval_secs,
        keepalive_max_missed,
        auto_attach_mux,
        mux_attach_command,
        login_scripts,
    } = &profile.target
    else {
        return None;
    };
    Some(SshSessionTarget {
        profile_id: Some(profile.id.clone()),
        name: profile.name.clone(),
        host: host.clone(),
        port: *port,
        username: username.clone(),
        auth: auth.clone(),
        proxy_jump: proxy_jump.clone(),
        proxy_command: proxy_command.clone(),
        proxy: proxy.clone(),
        forward_agent: *forward_agent,
        forward_x11: *forward_x11,
        auto_connect_on_startup: *auto_connect_on_startup,
        tmux_reattach: tmux_reattach.clone(),
        keepalive_interval_secs: *keepalive_interval_secs,
        keepalive_max_missed: *keepalive_max_missed,
        auto_attach_mux: *auto_attach_mux,
        mux_attach_command: mux_attach_command.clone(),
        login_scripts: login_scripts.clone(),
    })
}

fn render_broadcast_banner(
    banner_text: String,
    chrome: ChromePalette,
) -> Element<'static, Message, Theme, iced::Renderer> {
    let content = row![
        status_dot(chrome.danger),
        text(banner_text)
            .size(FONT_META)
            .font(ui_font_medium())
            .color(chrome.text_primary),
        space::horizontal(),
        button(text("Turn Off").size(FONT_META))
            .padding([3, 10])
            .style(tab_bar_button_style(chrome, false, true))
            .on_press(Message::SetBroadcastInput(false)),
    ]
    .spacing(10)
    .padding([5, 12])
    .align_y(Alignment::Center)
    .wrap()
    .vertical_spacing(4);

    container(content)
        .width(Length::Fill)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(chrome.danger_soft)),
            border: Border {
                color: chrome.danger,
                width: 1.0,
                radius: 0.0.into(),
            },
            text_color: Some(chrome.text_primary),
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

fn format_connection_recovery_notice(
    is_auto_reconnecting: bool,
    reconnect_attempt: Option<u32>,
    transport_status: &TransportStatus,
    is_recently_reconnected: bool,
) -> Option<StatusRecoveryNotice> {
    if is_auto_reconnecting
        || matches!(
            transport_status,
            TransportStatus::Degraded { .. } | TransportStatus::Reconnecting { .. }
        )
    {
        let message = match (transport_status, reconnect_attempt) {
            (
                TransportStatus::Reconnecting {
                    attempt,
                    next_retry_secs,
                },
                _,
            ) => format!("Auto-reconnecting #{attempt} ({next_retry_secs}s)"),
            (_, Some(attempt)) if attempt > 0 => format!("Auto-reconnecting #{attempt}..."),
            _ => "Auto-reconnecting...".to_string(),
        };
        return Some(StatusRecoveryNotice {
            kind: RecoveryNoticeKind::AutoReconnecting,
            message,
        });
    }
    if is_recently_reconnected {
        return Some(StatusRecoveryNotice {
            kind: RecoveryNoticeKind::ReconnectedSuccessfully,
            message: "Reconnected successfully".to_string(),
        });
    }
    None
}

fn sftp_recovery_notice_for_drawer(drawer: &SftpDrawerState) -> Option<StatusRecoveryNotice> {
    match drawer.connection_state() {
        SftpConnectionState::AutoReconnecting {
            attempt,
            next_retry_secs,
            ..
        } => {
            let eta = next_retry_secs
                .map(|secs| format!(" ({secs}s)"))
                .unwrap_or_default();
            return Some(StatusRecoveryNotice {
                kind: RecoveryNoticeKind::AutoReconnecting,
                message: format!("SFTP auto-reconnecting #{attempt}{eta}..."),
            });
        }
        SftpConnectionState::ReconnectedSuccessfully => {
            return Some(StatusRecoveryNotice {
                kind: RecoveryNoticeKind::ReconnectedSuccessfully,
                message: "SFTP reconnected successfully".to_string(),
            });
        }
        SftpConnectionState::Idle
        | SftpConnectionState::Connected
        | SftpConnectionState::Disconnected { .. } => {}
    }

    let snap = drawer.queue_snapshot();
    if let Some(attempt) = snap.transfers.iter().find_map(|t| match t.reconnect_state {
        TransferReconnectState::AutoReconnecting { attempt, .. } => Some(attempt),
        _ => None,
    }) {
        return Some(StatusRecoveryNotice {
            kind: RecoveryNoticeKind::AutoReconnecting,
            message: format!("Transfer auto-reconnecting #{attempt}..."),
        });
    }
    if snap
        .transfers
        .iter()
        .any(|t| matches!(t.reconnect_state, TransferReconnectState::Reconnected))
    {
        return Some(StatusRecoveryNotice {
            kind: RecoveryNoticeKind::ReconnectedSuccessfully,
            message: "Transfer reconnected successfully".to_string(),
        });
    }
    None
}

fn is_sftp_connection_error_message(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("disconnected")
        || lower.contains("connection lost")
        || lower.contains("connection reset")
        || lower.contains("connection aborted")
        || lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("broken pipe")
        || lower.contains("unexpected eof")
        || lower.contains("eof")
        || lower.contains("channel closed")
        || lower.contains("transport dropped")
        || lower.contains("not connected")
        || lower.contains("ssh protocol error")
        || lower.contains("connection failed")
}
