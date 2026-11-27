# SFTP+ Remote File Editor: Find, Find & Replace, and Ergonomics

## Overview

Upgrades the **Remote File Editor** (`SftpModalState::FileViewer` in **SFTP Panel** / `SFTP+`) with in-editor Find (`Ctrl+F`), Find & Replace (`Ctrl+H`), atomic Undo/Redo (`Ctrl+Z` / `Ctrl+Y` / `Ctrl+Shift+Z`), unsaved-changes protection, modal shortcut isolation, and editor ergonomics (`Line Numbers`, `Word Wrap`, `Go to Line`, `Expand`, `Status Bar`, `Switch to Edit`, `Reload`, `Ctrl+S`, `Tab`/`Shift+Tab`, `LF`/`CRLF` preservation).

## Requirements

1. **Modal Shortcut Isolation & Precedence**
   - While **Remote File Editor** is open, `ShortcutInterceptor` and `window_shortcut_event` must not forward modal editor shortcuts (`Ctrl+F`, `Ctrl+H`, `Ctrl+G`, `Ctrl+S`, `Ctrl+Z`, `Ctrl+Y`, `Ctrl+Shift+Z`, `F3`, `Shift+F3`, `Ctrl+Tab`, font zoom, pane actions) to background terminal panes or workspace tabs.
   - `Esc` follows layered dismissal: close `Go to Line` if open -> else close Find/Replace bar if open (returning focus to the editor) -> else trigger modal close (checking the unsaved-changes guard).

2. **Find (`Ctrl+F`)**
   - Available in both `View` (read-only) and `Edit` modes.
   - Docked as a compact inline bar directly above the editor viewport (never overlapping the first lines of text).
   - Pressing `Ctrl+F` focuses the Find input and selects all query text; if the editor has a non-empty single-line selection, that text auto-populates the Find query (multi-line or empty selections keep the previous query).
   - Supports three match toggles: Case sensitive (`Aa`), Whole word (`ab`), and Regex (`.*`, backed by the workspace `regex` crate with inline pattern error display).
   - Highlights all visible matches across the buffer, selects and scrolls the active match into view, shows `current / total` counter (or `No matches`), and navigates via `Enter` / `F3` / `▼` (next) and `Shift+Enter` / `Shift+F3` / `▲` (previous) with wrap-around.
   - Live re-search updates match offsets and counts whenever buffer content, search query, or match toggles change.

3. **Find & Replace (`Ctrl+H`)**
   - Available in `Edit` mode (`Ctrl+H` or `▸` expand chevron on the Find bar; ignored/hidden in `View` mode).
   - `Replace` replaces the currently active match (expanding `$1`, `$2`, `${name}` capture groups when Regex mode is enabled; literal replacement otherwise) and jumps to the next match.
   - `Replace All` replaces every match across the whole file as a single atomic Undo step and reports the replaced count (e.g. `"Replaced 14 occurrences"`).

4. **Undo / Redo & Indentation**
   - Maintains an in-modal Undo/Redo stack (`Ctrl+Z`, `Ctrl+Y` / `Ctrl+Shift+Z`) covering manual edits, single `Replace`, atomic `Replace All`, and `LF` ↔ `CRLF` conversion, recomputing `dirty` state relative to the saved baseline.
   - Binds `Tab` and `Shift+Tab` for single-line and multi-line block indent/unindent, configurable via status-bar selector between `Spaces: 4` (default), `Spaces: 2`, and `Tab (\t)`.

5. **Data Safety & Unsaved Changes Guard**
   - When `dirty == true`, closing (`Close`, `×`, `Esc`, backdrop click) or triggering `Reload` opens an inline/overlay confirmation (`Save & Close` / `Discard` / `Cancel`, or `Reload & Discard` / `Cancel`) rather than discarding edits silently.
   - When `dirty == false`, backdrop click dismisses in `View` mode, while backdrop click is disabled in `Edit` mode (closing only via `Esc`, `×`, or `Close`).
   - `Ctrl+S` saves immediately in `Edit` mode, keeps the editor open, clears `dirty`, shows `"Saved"` in the status bar, and refreshes the **SFTP Panel** directory listing.

6. **Editor Navigation, Workflow & Layout Utilities**
   - **Line Numbers Gutter & Word Wrap**: `Word Wrap` defaults to `OFF` (with horizontal scrolling and a toolbar toggle); the left line-number gutter stays aligned with logical lines in both modes (blank on wrapped continuation rows when `Word Wrap` is `ON`).
   - **Go to Line (`Ctrl+G`)**: Opens a compact inline input accepting `line` or `line:col`; `Enter` scrolls and moves cursor to the target position and refocuses the editor; `Esc` closes.
   - **Expand Toggle**: Expands the modal from `760×460` up to ~90% of window width/height.
   - **Status Bar**: Displays 1-based `Ln X, Col Y`, total line count, file byte size, `UTF-8`, detected/active line ending (`LF` / `CRLF`, clickable to convert and preserved on save), indent mode (`Spaces: 4` / `Spaces: 2` / `Tab (\t)`), and `Saved` / `Unsaved` indicator.
   - **In-Place Workflow**: `Switch to Edit` promotes an open `View` buffer directly to `Edit` mode without re-fetching, preserving scroll and search state; `Reload` re-fetches remote contents while preserving the nearest valid line position.

7. **Persistence & Backwards Compatibility**
   - Persist `Word Wrap`, `Expand`, and default indent preference in `TerminalConfig` (`config.toml`) using `#[serde(default)]` so legacy configs remain compatible.
