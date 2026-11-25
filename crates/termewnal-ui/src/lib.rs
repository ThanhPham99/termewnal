//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod command_palette;
pub mod sidebar;
pub mod split_view;
pub mod status_bar;
pub mod terminal_widget;
pub mod workspace;

pub use app::{
    AppState, LaunchDisposition, LocalSession, Message, SessionId, SshSessionSlot,
    SshSessionTarget, shortcut_for_key_event, shortcut_for_key_event_with_palette,
};
pub use command_palette::{
    CommandPaletteMode, CommandPaletteState, PaletteAction, PaletteCategory, PaletteItem,
    PaletteSessionEntry, QuickConnectTarget, fuzzy_score,
};
pub use sidebar::{
    ActiveTunnelEntry, ProfileGroupView, SidebarSection, SidebarState, SystemSshProfile,
    TunnelRuntimeState, UNGROUPED_LABEL,
};
pub use status_bar::{
    SessionKindLabel, SessionStatusHook, StatusBarAction, StatusBarSnapshot, StatusBarState,
    TransferSummaryHook, TunnelSummaryHook,
};
pub use terminal_widget::{TerminalViewport, key_event_to_bytes};
pub use workspace::{
    ClosePaneOutcome, CycleDirection, DEFAULT_SPLIT_RATIO, FocusDirection, MAX_SPLIT_RATIO,
    MIN_SPLIT_RATIO, PaneId, SplitAxis, SplitId, SplitNode, TabId, Workspace, WorkspaceTab,
};
