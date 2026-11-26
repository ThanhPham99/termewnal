# Per-Pane Remote-Only SFTP Panel Auto-Opened on SSH Connect

Status: accepted

We will unify Termewnal's dual-pane SFTP+ commander (`DualHorizontal` / `DualVertical`) and basic right-docked `SftpDrawer` into a single remote-only **SFTP Panel** (`SFTP+`) docked to the right of each SSH **Pane**, automatically opened and connected when an SSH **Session** connects (unless manually closed for that session or disabled in settings).

## Considered Options

- **Unified per-Pane remote-only SFTP Panel (`SFTP+`)**: Keeps the terminal visible side-by-side with full SFTP+ remote features (navigation history, bookmarks, filter, sort, context menu, text viewer/editor, permissions, collapsible `Transfers` tray), eliminates the redundant `Local` file browser to reclaim horizontal space, uses OS native file/folder pickers plus drag-and-drop for uploads, and isolates SFTP state per split SSH **Pane**.
- **Dual-pane commander (`Local | Remote`) covering the workspace**: Matches Tabby SFTP+ closely, but hides the active terminal session behind a full-tab overlay and duplicates OS file manager functionality at high screen-space cost.
- **Workspace-wide basic `SftpDrawer` (`SideDock`)**: Kept the terminal visible, but stripped away SFTP+ capabilities (context menu, sorting, filtering, bookmarks, inline viewer/editor) and applied one global right dock across split tabs instead of binding to the active SSH **Pane**.

## Consequences

- `SftpLayoutMode`, `SftpPaneSide::Local`, `local_bookmarks` UI, and `render_sftp_drawer` are removed in favor of one remote-only SFTP Panel widget inside `render_pane`; legacy `local_bookmarks` entries in `sftp_state.toml` remain ignored-on-read for backwards compatibility.
- Because Terminal and SFTP Panel are rendered side-by-side inside the same **Pane**, keyboard focus is explicitly tracked between Terminal and SFTP Panel so file shortcuts (`Ctrl+C`/`X`/`V`/`A`, `F2`, `F5`, `Delete`) never hijack shell input while the Terminal is focused.
- Both Left Sidebar width and SFTP Panel default width become user-resizable via draggable dividers (with double-click reset) and persist in `config.toml`.
