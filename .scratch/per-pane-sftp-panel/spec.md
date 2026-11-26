# Per-Pane Remote-Only SFTP Panel & Resizable Layout

## Overview

Unifies Termewnal's dual-pane SFTP+ commander (`DualHorizontal` / `DualVertical`) and basic right-docked `SftpDrawer` into a single remote-only **SFTP Panel** (`SFTP+`) docked on the right side of each SSH **Pane**, automatically opened and connected when an SSH **Session** connects, alongside persistent resizable Left Sidebar and SFTP Panel widths.

## Requirements

1. **Unified Remote-Only SFTP Panel (`SFTP+`)**
   - Remove `Local` file browser pane, `DualHorizontal` / `DualVertical` overlay modes, and the legacy basic `render_sftp_drawer` UI.
   - Retain all SFTP+ remote features inside a compact right-docked panel: navigation history (`Back`/`Forward`/`Up`/`Refresh`/`Home`), editable path bar, hidden-file toggle, inline substring filter, remote bookmarks, sortable columns (`Name`, `Size`, width-gated `Modified` >= 370px and `Permission` >= 500px), multi-select (`Ctrl+click`), right-click context menu (`Download`, `Download to...`, `View`, `Edit`, `Copy`, `Cut`, `Paste`, `Rename`, `Delete`, `Edit Permissions`, `Properties`, `New Folder`, `New File`, `Refresh`, `Select All`, `Invert Selection`, `Copy Path`), modals, and a collapsible `Transfers` tray at the bottom (collapsed by default, auto-opened when a transfer is queued).
2. **Per-Pane Placement & Auto-Open Lifecycle**
   - Render SFTP Panel inside each SSH **Pane** (`render_pane`) on the right of the terminal viewport, below the SSH session toolbar.
   - Automatically open and begin listing the remote directory when an SSH **Session** receives `SshUiEvent::Connected`, provided `auto_open_sftp` is enabled (`true` by default) and the user has not manually closed SFTP Panel for that session (`manually_closed` persists across auto-reconnects).
   - Non-SSH sessions (`LocalShell`, `Serial`, `Telnet`, `Docker`) and empty workspaces do not show an SFTP Panel (`Ctrl+Shift+E` / Command Palette toggle is a no-op).
3. **Focus & Keyboard Shortcut Isolation**
   - Terminal keeps keyboard focus by default on SSH connect.
   - Track per-pane focus (`Terminal` vs `SftpPanel`): SFTP shortcuts (`Ctrl+C`/`X`/`V`/`A`, `F2`, `F5`, `Delete`) fire only when that pane's SFTP Panel has focus; clicking back into Terminal restores all terminal input.
   - Route per-pane SFTP actions and OS drops to the owning `SessionId`.
4. **Native Upload & Configurable Download**
   - Keep OS drag-and-drop onto SFTP Panel and `Shift+Drop` onto SSH terminal.
   - Add `Upload Files...` and `Upload Folder...` actions using native OS pickers (`rfd`).
   - Default downloads to OS Downloads directory (fallback `<data_local_dir>/downloads`), configurable in `Settings -> SSH & SFTP`; add `Download to...` folder picker in context menu; double-click on a remote file downloads to the default directory.
5. **Resizable Sidebar, SFTP Panel & Split Dividers**
   - Add a draggable right-edge divider (`SidebarDock`) to Left Sidebar (`[180, 480]` px, clamped by window width), double-click resets to `250px`, persisted in `config.toml` (`sidebar_width`).
   - SFTP Panel divider resizes within `[220px, 70% pane width]` (preserving >= 240px terminal when wide enough), double-click resets to `360px`, persisted in `config.toml` (`sftp_panel_width`).
   - Add double-click reset to `0.5` on `SplitView` dividers.
6. **Backwards Compatibility**
   - Tolerate legacy `config.toml` and `sftp_state.toml` (`local_bookmarks`) cleanly via `#[serde(default)]`.
