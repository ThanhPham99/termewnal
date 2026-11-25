//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod config_reload_ui;
pub mod terminal_widget;

pub use app::{AppState, LocalSession, Message, SessionId};
pub use config_reload_ui::ConfigReloadUiState;
pub use terminal_widget::{TerminalViewport, key_event_to_bytes};
