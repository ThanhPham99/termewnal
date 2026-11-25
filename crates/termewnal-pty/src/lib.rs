//! Cross-platform local shell detection, PTY lifecycle, and terminal grid state.

pub mod buffer;
pub mod error;
pub mod session;
pub mod shell;

pub use alacritty_terminal::term::cell::Flags as CellFlags;
pub use alacritty_terminal::vte::ansi::{Color as TermColor, CursorShape, NamedColor, Rgb};
pub use buffer::{TerminalBuffer, snapshot_from_term};
pub use error::PtyError;
pub use session::{
    CursorSnapshot, PtyEvent, PtySession, PtySpawnConfig, TermDimensions, TerminalCell,
    TerminalSnapshot,
};
pub use shell::{
    ShellKind, ShellSpec, detect_available_shells, detect_default_shell, resolve_unix_shell,
    resolve_windows_shells,
};
