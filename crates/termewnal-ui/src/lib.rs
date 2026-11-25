//! Termewnal desktop UI application state and terminal viewport widget.

pub mod app;
pub mod terminal_ergonomics;
pub mod terminal_widget;

pub use app::{AppState, LocalSession, Message, SessionId};
pub use terminal_ergonomics::{
    FileDropAction, PendingPaste, SearchBarState, classify_file_drop, format_paste_bytes,
    is_multiline_paste,
};
pub use terminal_widget::{
    ImeComposition, ImeUpdate, TerminalPalette, TerminalViewOptions, TerminalViewport,
    ime_cursor_anchor_rect, key_event_to_bytes,
};
