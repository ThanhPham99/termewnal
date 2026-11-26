# 08: UI Keybindings, Tab Customization, Bell/Notifications, Context Menu, Highlight & Logging

**What to build:** Wire customizable `KeybindingMap` (including multi-key chords + font zoom `Ctrl+=/- /0` + `Ctrl+1..9` + clear buffer), configurable tab bar positioning (`Top`/`Bottom`/`Left`/`Right`), tab rename/color/pin/duplicate/close-others/context-menu, Bell badge + visual flash + desktop notification, `OSC 9;4` progress indicators, terminal right-click context menu & mouse forwarding, `Ctrl+Click` file paths, `HighlightRule` cell coloring, and output logging/export into `termewnal-ui`.

**Blocked by:** 01, 02

**Status:** resolved

- [x] `KeybindingResolver` replaces hardcoded global/terminal shortcuts, supports two-step chords, and edits bindings in Settings UI.
- [x] Tab bar renders cleanly at `Top`, `Bottom`, `Left`, or `Right` per `config.toml`, and tabs support rename, accent color, pin, duplicate, close others/to-right, Bell indicator, and `OSC 9;4` progress bars.
- [x] Right-click behavior obeys `RightClickAction` (`Menu`, `Paste`, `Off`), middle-click pastes when enabled, terminal context menu exposes Copy/Paste/Clear/Split/Search/Save Output/Logging/Quick Commands/SFTP/Close, and `mouse_mode` forwards PTY mouse sequences.
- [x] `TerminalViewport` applies `HighlightRule` colors, cursor blink, opacity, and `Ctrl+Click` local file paths.
- [x] Desktop notification adapter sends `org.freedesktop.Notifications` alerts via `zbus` on background tab bell (behind a testable trait).
- [x] `termewnal-ui` tests verify keybinding resolution, chord matching, tab positioning/actions, bell/progress state, context menu routing, and highlight/logging actions.
