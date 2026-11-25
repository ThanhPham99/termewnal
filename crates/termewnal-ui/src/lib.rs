//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod terminal_widget;

pub use app::{AppState, LocalSession, Message, SessionId};
pub use terminal_widget::{TerminalViewport, key_event_to_bytes};
