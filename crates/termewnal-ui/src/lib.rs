//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod close_guard;
pub mod command_palette;
pub mod config_reload_ui;
pub mod context_menu;
pub mod drag_drop;
pub mod ipc;
pub mod keybinding_bridge;
pub mod notifications;
pub mod profile_ui;
pub mod quake_and_tray;
pub mod server_stats_bar;
pub mod settings_ui;
pub mod sftp_drawer;
pub mod sftp_plus;
pub mod sidebar;
pub mod split_view;
pub mod ssh_ui;
pub mod status_bar;
pub mod tabby_import_ui;
pub mod terminal_ergonomics;
pub mod terminal_widget;
pub mod tunnel_ui;
pub mod ui_theme;
pub mod v2_modals;
pub mod window_chrome;
pub mod workspace;

pub use app::{
    AppState, LaunchDisposition, LocalSession, Message, RemoteConnectIntent, RemoteSession,
    SerialSessionSlot, SessionId, SshSessionSlot, SshSessionTarget, TelnetSessionSlot,
    shortcut_for_key_event, shortcut_for_key_event_with_palette,
};
pub use close_guard::{
    BackgroundWorkSource, BackgroundWorkStatus, BackgroundWorkTracker, CloseGuardState,
    DaemonLifecycle,
};
pub use command_palette::{
    CommandPaletteMode, CommandPaletteState, PaletteAction, PaletteCategory, PaletteItem,
    PaletteSessionEntry, QuickConnectTarget, fuzzy_score,
};
pub use config_reload_ui::ConfigReloadUiState;
pub use context_menu::{ContextMenuKind, ContextMenuState};
pub use drag_drop::{DragDropState, DragPayload, DropTarget};
pub use ipc::{
    IpcCommand, IpcError, IpcServerHandle, LaunchOutcome, acquire_or_forward,
    parse_cli_launch_command, send_command,
};
#[cfg(target_os = "linux")]
pub use notifications::ZbusDesktopNotifier;
pub use notifications::{
    DesktopNotifier, NullDesktopNotifier, RecordingDesktopNotifier, default_desktop_notifier,
};
pub use profile_ui::{
    JumpHostCandidate, JumpHostKind, ProfileAuthChoice, ProfileEditorState, ProfileEditorTab,
    ProfileKindChoice, ProxyEndpointDraft, SavedProfileDraft, SshConnectionMode,
};
#[cfg(target_os = "linux")]
pub use quake_and_tray::{X11GlobalHotkeyRegistrar, ZbusStatusNotifierItem};
pub use quake_and_tray::{
    GlobalHotkeyRegistrar, HotkeyRegistrationGuard, NullGlobalHotkeyRegistrar, NullSystemTray,
    ParsedGlobalHotkey, QuakeBounds, QuakeController, RecordingSystemTray, SystemTrayAdapter,
    TrayAction, TrayMenuItem, TrayStateSnapshot, compute_quake_bounds,
    default_global_hotkey_registrar, default_system_tray, parse_global_hotkey,
    quake_window_settings,
};
pub use settings_ui::{SettingsTab, SettingsUiState, ThemeOverrideField};
pub use sftp_drawer::{
    DEFAULT_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH, MIN_SFTP_DRAWER_WIDTH, SftpConnectionState,
    SftpDrawerDock, SftpDrawerState, render_conflict_resolution_modal, render_sftp_drawer,
};
pub use sftp_plus::{
    FileBrowserEntry, SftpClipboard, SftpClipboardOp, SftpContextActionKind,
    SftpContextMenuState, SftpLayoutMode, SftpModalState, SftpPaneBrowseState, SftpPaneSide,
    SftpPlusState, SftpSortColumn, render_sftp_context_menu_overlay, render_sftp_plus_commander,
    render_sftp_plus_modal,
};
pub use sidebar::{
    ActiveTunnelEntry, ProfileGroupView, SidebarFilterScope, SidebarSection, SidebarSessionEntry,
    SidebarState, SystemSshProfile, TunnelRuntimeState, UNGROUPED_LABEL,
};
pub use ssh_ui::{
    AuthPromptResponder, HostKeyResponder, MitmAlert, SshAuthPrompt, SshTargetSpec, SshUiEvent,
    SshUiState, drive_ssh_connect,
};
pub use status_bar::{
    RecoveryNoticeKind, SessionKindLabel, SessionStatusHook, StatusBarAction, StatusBarSnapshot,
    StatusBarState, StatusRecoveryNotice, TransferSummaryHook, TunnelSummaryHook, render_status_bar,
};
pub use tabby_import_ui::{
    TabbyCredentialPromptRow, TabbyImportSummary, TabbyImportUiState, tabby_import_path_input_id,
};
pub use terminal_ergonomics::{
    FileDropAction, PendingPaste, SearchBarState, classify_file_drop, format_paste_bytes,
    is_multiline_paste,
};
pub use terminal_widget::{
    ImeComposition, ImeUpdate, TerminalPalette, TerminalViewOptions, TerminalViewport,
    ime_cursor_anchor_rect, key_event_to_bytes,
};
pub use tunnel_ui::{
    AdHocTunnelRule, PendingPublicBindWarning, TunnelCreateForm, TunnelUiState,
    build_active_tunnel_entries, tunnel_bind_host_input_id, tunnel_bind_port_input_id,
    tunnel_name_input_id, tunnel_target_host_input_id, tunnel_target_port_input_id,
};
pub use ui_theme::{ChromePalette, PillTone};
pub use v2_modals::{
    ProtocolLauncherState, ProtocolLauncherTab, QuickCommandsUiState, WorkspacesUiState,
    ZmodemSendUiState,
};
pub use workspace::{
    ClosePaneOutcome, CycleDirection, DEFAULT_SPLIT_RATIO, FocusDirection, MAX_SPLIT_RATIO,
    MIN_SPLIT_RATIO, PaneId, SplitAxis, SplitId, SplitNode, TabId, Workspace, WorkspaceIds,
    WorkspaceTab,
};
