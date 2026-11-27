# Terminal Text Selection UI/UX Improvements

## Overview

Fixes per-word background rendering gaps during terminal text selection, guarantees readable selected-text contrast, clamps multi-line selection highlights cleanly to line content bounds, adds ergonomic mouse selection behaviors (`Shift+Click` extend, `Alt+Drag` block mode, out-of-bounds drag auto-scroll, pixel-distance multi-click), wires `word_separators` into `alacritty_terminal`, clears selection on typing/paste, and adds a transient `"Copied"` feedback badge.

## Requirements

1. **Two-Pass Cell Rendering & Contrast**
   - Split `TerminalViewport::draw()` into two per-row passes:
     - **Pass 1 (Backgrounds):** Coalesce contiguous horizontal spans with identical resolved background color `!= palette.background` (including spaces, wide-character cells/spacers, selection highlights, search highlights, and highlight rules) and draw single `fill_quad` spans so coalesced text runs never leave per-word gaps.
     - **Pass 2 (Foreground Text):** Coalesce contiguous printable glyph runs with matching foreground style/flags (`bold`, `italic`, `underline`, `strikethrough`, `DIM`, `HIDDEN`) and render glyphs cleanly over the background quads.
   - Wire `ThemePalette::selection_fg` into `TerminalPalette`; when a cell is selected:
     - Use `selection_fg` if provided by the theme.
     - Otherwise ensure readable foreground contrast against `selection_bg` (including `INVERSE` cells whose `fg`/`bg` would otherwise blend into `selection_bg`).

2. **Visual Line-End Highlight Clamping**
   - Single-line `Simple` selection highlights exact dragged columns (`start_col..=end_col`) so intentional trailing spaces remain visible.
   - Multi-line `Simple` and `Lines` (triple-click) selections clamp visual background highlighting per row:
     - Non-empty rows highlight up to `last_non_space_col + 1` (representing the newline cell, clamped to `cols - 1`).
     - Completely blank rows highlight `1` cell at column `0`.
   - `Block` selection highlights exact rectangular column bounds (`start_col..=end_col`) across all selected rows.

3. **Out-of-Bounds Drag & Auto-Scroll**
   - When `dragging_selection` is active, handle `CursorMoved` and `ButtonReleased` even outside `bounds`, clamping `(col, row, side)` to viewport `inner` edges.
   - Track vertical overshoot (`above` or `below` `inner`); while overshoot is non-zero, a ~50ms UI tick scrolls viewport scrollback by 1–5 lines scaled by overshoot distance (clamped at scrollback top / live bottom) and updates the selection endpoint at the clamped viewport edge.

4. **Selection Gestures & Word Separators**
   - `Shift+Click`: if the session has an active non-empty selection, extend that selection to the clicked `(col, row, side)`; if no selection exists, start a new `SelectionType::Simple` selection at the clicked cell.
   - `Alt+Drag`: start `SelectionType::Block` when `!mouse_mode`, or `Shift+Alt+Drag` when `mouse_mode` is active (consistent with `Shift` bypassing PTY mouse reporting).
   - Multi-click detection (`Double`/`Triple`): match consecutive left presses within `400ms` and pointer distance `<= 6.0px` rather than requiring identical `(col, row)`.
   - Plumb `TerminalConfig::word_separators` into `alacritty_terminal::term::Config::semantic_escape_chars` on local/remote buffer initialization and live `apply_options` sync.

5. **Clear on Input & Transient `"Copied"` Feedback**
   - Automatically clear any active selection when user sends terminal input (`TerminalInput`, IME commit, paste dispatch), without intercepting `Escape`.
   - Display a lightweight `"Copied"` pill badge in the bottom-right corner of the active pane for ~1.2 seconds whenever text is copied via `Ctrl+Shift+C`, context menu `Copy Selection`, or `copy_on_select`, avoiding overlap with the top-right `SearchBar`.
