# 10: Quake Console Mode, System Tray & Final Workspace Verification

**What to build:** Implement Quake drop-down window positioning & global hotkey / IPC `--toggle-quake` integration, cross-platform System Tray (`org.kde.StatusNotifierItem` via `zbus` on Linux) synced with `DaemonLifecycle::BackgroundRunning`, and run full workspace tests & clippy/build verification across all crates.

**Blocked by:** 08, 09

**Status:** resolved

- [x] Quake mode supports configurable edge (`Top`/`Bottom`/`Left`/`Right`), size percentage, global hotkey registration (X11 `XGrabKey` via `x11rb` + Wayland/fallback IPC `--toggle-quake`), and hide/show toggle without closing active sessions.
- [x] System Tray exposes Show/Hide Window, Toggle Quake, Quick Launch Profiles, Active Tunnel count, and Quit, keeping the daemon alive when `BackgroundRunning` or tray-minimized.
- [x] `main.rs` CLI flags support `--toggle-quake`, `--profile`, `--new-tab`, and `--cwd`.
- [x] Integration tests verify Quake window toggle state, tray action routing, CLI launch commands, and full `cargo test --workspace` passes with 0 failures.
