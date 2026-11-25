# 03: Terminal Ergonomics, Themes, IME & Scrollback Search

**What to build:** Equip the terminal view with 10,000-line configurable scrollback, live scrollback search (`Ctrl+F`), `Ctrl+Click` URL opening, optional copy-on-select, right-click / `Ctrl+Shift+V` paste with a multi-line paste confirmation guard, Vietnamese/CJK IME input, font/ligature/cursor settings, built-in color themes, and terminal-pane file-drop path pasting.

**Blocked by:** 01: Local Shell App & Atomic TOML Config

**Status:** resolved

- [x] Terminal buffers default to 10,000 scrollback lines (configurable in `config.toml`) and support interactive `Ctrl+F` incremental search with match highlighting and next/previous navigation.
- [x] `Ctrl+Click` detects and opens URLs in the OS browser; copy-on-select is configurable; pasting multi-line text triggers a confirmation guard before writing to the `Session`.
- [x] Vietnamese/CJK IME pre-edit and commit events render and dispatch accurately into the active terminal `Session` on Linux X11/Wayland (with cross-platform `iced` event wiring).
- [x] Built-in themes (Catppuccin, Tokyo Night, Dracula, Nord, Dark, Light) plus font family, size, ligature, and cursor style settings apply immediately from configuration.
- [x] Dropping files onto a Terminal pane inserts shell-escaped file paths into the active `Session`.
- [x] Automated tests verify scrollback retention, grid search matches, URL detection, multi-line paste guarding, and shell-escaped path formatting.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `80fe8e3`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
