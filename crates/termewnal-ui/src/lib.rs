//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod close_guard;
pub mod command_palette;
pub mod config_reload_ui;
pub mod ipc;
pub mod sftp_drawer;
pub mod sidebar;
pub mod split_view;
pub mod ssh_ui;
pub mod status_bar;
pub mod terminal_ergonomics;
pub mod terminal_widget;
pub mod workspace;

pub use app::{
    AppState, LaunchDisposition, LocalSession, Message, RemoteConnectIntent, RemoteSession,
    SessionId, SshSessionSlot, SshSessionTarget, shortcut_for_key_event,
    shortcut_for_key_event_with_palette,
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
pub use ipc::{IpcCommand, IpcError, IpcServerHandle, LaunchOutcome, acquire_or_forward, send_command};
pub use sftp_drawer::{
    DEFAULT_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH, MIN_SFTP_DRAWER_WIDTH, SftpDrawerDock,
    SftpDrawerState, render_sftp_drawer,
};
pub use sidebar::{
    ActiveTunnelEntry, ProfileGroupView, SidebarSection, SidebarState, SystemSshProfile,
    TunnelRuntimeState, UNGROUPED_LABEL,
};
pub use ssh_ui::{
    AuthPromptResponder, HostKeyResponder, MitmAlert, SshAuthPrompt, SshTargetSpec, SshUiEvent,
    SshUiState, drive_ssh_connect,
};
pub use status_bar::{
    SessionKindLabel, SessionStatusHook, StatusBarAction, StatusBarSnapshot, StatusBarState,
    TransferSummaryHook, TunnelSummaryHook, render_status_bar,
};
pub use terminal_ergonomics::{
    FileDropAction, PendingPaste, SearchBarState, classify_file_drop, format_paste_bytes,
    is_multiline_paste,
};
pub use terminal_widget::{
    ImeComposition, ImeUpdate, TerminalPalette, TerminalViewOptions, TerminalViewport,
    ime_cursor_anchor_rect, key_event_to_bytes,
};
pub use workspace::{
    ClosePaneOutcome, CycleDirection, DEFAULT_SPLIT_RATIO, FocusDirection, MAX_SPLIT_RATIO,
    MIN_SPLIT_RATIO, PaneId, SplitAxis, SplitId, SplitNode, TabId, Workspace, WorkspaceIds,
    WorkspaceTab,
};
