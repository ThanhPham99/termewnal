//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod config_reload_ui;
pub mod split_view;
pub mod terminal_widget;
pub mod workspace;

pub use app::{AppState, LocalSession, Message, SessionId, shortcut_for_key_event};
pub use config_reload_ui::ConfigReloadUiState;
pub use terminal_widget::{TerminalViewport, key_event_to_bytes};
pub use workspace::{
    ClosePaneOutcome, CycleDirection, DEFAULT_SPLIT_RATIO, FocusDirection, MAX_SPLIT_RATIO,
    MIN_SPLIT_RATIO, PaneId, SplitAxis, SplitId, SplitNode, TabId, Workspace, WorkspaceTab,
};
