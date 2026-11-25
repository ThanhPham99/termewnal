//! Cross-platform local shell detection, PTY lifecycle, and terminal grid state.

pub mod ergonomics;
pub mod error;
pub mod session;
pub mod shell;

pub use alacritty_terminal::grid::Scroll;
pub use alacritty_terminal::index::Side;
pub use alacritty_terminal::selection::SelectionType;
pub use alacritty_terminal::term::cell::Flags as CellFlags;
pub use alacritty_terminal::vte::ansi::{Color as TermColor, CursorShape, NamedColor, Rgb};
pub use ergonomics::{
    DetectedUrl, GridCoord, GridSearchMatch, UrlSpan, detect_urls_in_snapshot, shell_escape_path,
    shell_escape_paths, url_at_cell,
};
pub use error::PtyError;
pub use session::{
    CursorSnapshot, GridRange, PtyEvent, PtySession, PtySpawnConfig, TermDimensions,
    TerminalCell, TerminalSnapshot,
};
pub use shell::{
    ShellKind, ShellSpec, detect_available_shells, detect_default_shell, resolve_unix_shell,
    resolve_windows_shells,
};
