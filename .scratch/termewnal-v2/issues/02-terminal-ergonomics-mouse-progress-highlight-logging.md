# 02: Terminal Ergonomics, Mouse Reporting, Bell, OSC 9;4 Progress, Highlight & Logging

**What to build:** Upgrade `termewnal-pty` to parse `OSC 9;4` progress events, support cursor blink & clear-scrollback, encode PTY mouse events (`SGR 1006` / normal X10), detect clickable local file paths (`path:line:col` & `file://`), match `HighlightRule` regex spans on terminal rows, export scrollback text, and stream session output via `SessionLogger`.

**Blocked by:** 01

**Status:** resolved

- [x] `TerminalBuffer` parses `OSC 9;4` sequences into `TerminalProgressState` (`Normal`, `Indeterminate`, `Error`, `Paused`, `Hidden`) and emits `PtyEvent::Progress`.
- [x] `TerminalBuffer` supports `clear_screen_and_scrollback()`, cursor blink configuration, and `export_scrollback_text()`.
- [x] Mouse event encoder translates press/release/drag/scroll into SGR 1006 or normal X10 byte sequences when `mouse_mode` is enabled.
- [x] URL/link detector recognizes local file paths (`path/to/file.rs:42:10`, `file://...`) alongside `http(s)://` URLs.
- [x] `HighlightRule` matcher produces styled cell spans for visible rows, and `SessionLogger` writes timestamped/raw logs safely to disk.
- [x] Integration tests verify OSC 9;4 parsing, mouse byte encoding, file-path detection, highlight matching, clear/export, and logging.

## Comments

- Resolved and verified with `cargo test -p termewnal-pty` (15 passed, 0 failed).
