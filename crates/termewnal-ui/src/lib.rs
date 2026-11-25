//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod close_guard;
pub mod ipc;
pub mod split_view;
pub mod terminal_widget;
pub mod workspace;

pub use app::{
    AppState, LocalSession, Message, RemoteConnectIntent, RemoteSession, SessionId,
    shortcut_for_key_event,
};
pub use close_guard::{
    BackgroundWorkSource, BackgroundWorkStatus, BackgroundWorkTracker, CloseGuardState,
    DaemonLifecycle,
};
pub use ipc::{IpcCommand, IpcError, IpcServerHandle, LaunchOutcome, acquire_or_forward, send_command};
pub use terminal_widget::{TerminalViewport, key_event_to_bytes};
pub use workspace::{
    ClosePaneOutcome, CycleDirection, DEFAULT_SPLIT_RATIO, FocusDirection, MAX_SPLIT_RATIO,
    MIN_SPLIT_RATIO, PaneId, SplitAxis, SplitId, SplitNode, TabId, Workspace, WorkspaceIds,
    WorkspaceTab,
};
