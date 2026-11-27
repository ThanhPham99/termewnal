# termewnal-pty

## Overview
The `termewnal-pty` crate (manifest: [`Cargo.toml`](./Cargo.toml)) provides cross-platform local shell detection, native OS PTY lifecycle management, thread-safe `alacritty_terminal::Term` state buffering, session output logging, expect/send login script automation, and terminal grid ergonomics (URL/path detection, scrollback search, shell path escaping, SGR/X10 mouse encoding, and regex highlight evaluation). It depends on `alacritty_terminal`, `portable-pty`, `regex`, `termewnal-core` (`../termewnal-core`), `thiserror`, `tokio`, `tracing`, and `zeroize` (with `tempfile` in `[dev-dependencies]`), and is verified with `cargo test -p termewnal-pty`.

## Subdirectories & Boundaries
- `src`: [src/README.md](./src/README.md) - Component implementation (`lib.rs`, `error.rs`, `buffer.rs`, `ergonomics.rs`, `logging.rs`, `login_script.rs`, `session.rs`, `shell.rs`) exporting `PtySession`, `TerminalBuffer`, `TerminalSnapshot`, `PtySpawnConfig`, `PtyEvent`, `PtyError`, `SessionLogger`, `LoginScriptRunner`, `ShellSpec`, `ShellKind`, and terminal ergonomics/re-export APIs.
