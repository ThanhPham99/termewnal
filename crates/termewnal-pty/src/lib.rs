//! Cross-platform local shell detection, PTY lifecycle, and terminal grid state.

pub mod buffer;
pub mod ergonomics;
pub mod error;
pub mod logging;
pub mod login_script;
pub mod session;
pub mod shell;

pub use alacritty_terminal::grid::Scroll;
pub use alacritty_terminal::index::Side;
pub use alacritty_terminal::selection::SelectionType;
pub use alacritty_terminal::term::cell::Flags as CellFlags;
pub use alacritty_terminal::vte::ansi::{Color as TermColor, CursorShape, NamedColor, Rgb};
pub use buffer::{OutputInterceptor, TerminalBuffer, snapshot_from_term};
pub use ergonomics::{
    CompiledHighlightRule, DetectedLink, DetectedLinkKind, DetectedUrl, GridCoord, GridSearchMatch,
    HighlightStyle, PtyMouseButton, PtyMouseEventKind, UrlSpan, compile_highlight_rules,
    compute_highlight_grid, detect_links_in_snapshot, detect_urls_in_snapshot, encode_mouse_event,
    link_at_cell, shell_escape_path, shell_escape_paths, url_at_cell,
};
pub use error::PtyError;
pub use logging::{SessionLogger, default_session_log_path};
pub use login_script::{LoginScriptDispatch, LoginScriptRunner};
pub use session::{
    CursorSnapshot, GridRange, PtyEvent, PtySession, PtySpawnConfig, TermDimensions,
    TerminalCell, TerminalProgressState, TerminalSnapshot,
};
pub use shell::{
    ShellKind, ShellSpec, detect_available_shells, detect_default_shell, resolve_unix_shell,
    resolve_windows_shells,
};
