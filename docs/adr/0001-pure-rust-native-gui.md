# Pure Rust Native Desktop GUI

Status: accepted

We will build Termewnal as a pure Rust native desktop GUI application rather than using Tauri v2 (Rust backend + WebView/`xterm.js`) or a terminal-only TUI (`ratatui`).

## Considered Options

- **Pure Rust native GUI**: 100% Rust stack with minimal memory footprint, fast cold start, and direct GPU terminal rendering, at the cost of building rich desktop widgets (split panes, SFTP file tree, drag-and-drop) natively in Rust.
- **Tauri v2 (Rust + WebView/`xterm.js`)**: Faster rich UI iteration with HTML/CSS/`xterm.js`, rejected to avoid WebView/JS runtime overhead and keep the entire stack in Rust.
- **TUI (`ratatui`)**: Runs inside an existing terminal and cannot provide a standalone desktop terminal emulator or native drag-and-drop SFTP UX.
