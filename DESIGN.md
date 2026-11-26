# Design System — Termewnal (Eugeny/Tabby UI & UX)

## 1. Visual Direction & Philosophy
Termewnal follows the exact visual architecture and UX of [`Eugeny/tabby`](https://github.com/Eugeny/tabby) (`tabby-core` / Standard theme / **Follow the color scheme** chrome):
- **Terminal-first, edge-to-edge workspace**: When sessions are running, chrome steps back cleanly; there is no persistent bottom status bar and the profile sidebar starts collapsed (`show_profile_sidebar = false`, toggleable via the tab bar button, `Ctrl+B`, or Settings).
- **Flush 38px tab strip (`tab-bar` + `tab-header`)**: Rectangular top/bottom/side tabs (`height: 38px`) with no wasted outer capsule padding; active tabs merge smoothly into the terminal background (`--body-bg`) and carry a subtle `2px` active indicator bar.
- **Follow the color scheme**: UI surfaces (`--theme-bg`, `--theme-bg-more`, `--theme-bg-less`, `--theme-secondary`, `--theme-primary`, `--theme-fg-more`) derive dynamically from the active terminal color scheme so any of the 28 built-in schemes (starting with **Tabby Default** dark and **Tabby Default Light**) drives a unified theme across chrome and terminal cells.

## 2. Color & Surface Tokens (`ui_theme::ChromePalette` + `termewnal-core::theme`)
- **Tabby Default (Dark)**:
  - Terminal canvas (`--body-bg`): `#171717`
  - Foreground: `#cacaca` (`term_fg`), emphasized `#eeeeee` (`--theme-fg-more`)
  - Accent (`--theme-primary`): `#5da9f6`
  - Surfaces derived dynamically in HSL around `--body-bg` and `--theme-primary`:
    - `chrome_bg` (`--theme-bg`): deep cool-tinted surface shifted from `#171717` + accent hue
    - `bg_more` (`--theme-bg-more`): darker tab strip & sidebar well
    - `secondary` (`--theme-secondary`): flat button & divider surface (`1px` crisp hairlines, zero noisy outer frames)
- **Tabby Default Light**:
  - Terminal canvas (`--body-bg`): `#f8f9fa`
  - Foreground: `#4d4d4c`, emphasized `#2a2a2a`
  - Accent (`--theme-primary`): `#4271ae`
- **Adaptive contrast**: `ensure_contrast` guarantees readable button/highlight foregrounds across both dark and light schemes.

## 3. Core Tabby Surfaces & Controls
- **Tab Bar (`app.rs::render_tab_strip`)**:
  - `38px` bar on `--theme-bg-more` (`bg_more`) containing 1-based numbered rectangular `tab-header`s (toggleable with `hide_tab_index`), dynamic/fixed width toggle (`flex_tabs`), `3px` bottom color tags (`TAB_COLOR_PRESETS`: `#0275d8`, `#5cb85c`, `#f0ad4e`, `#613d7c`, `#d9534f`, `#ffd500`), `2px` activity pulse bar, `⋯` context menu trigger, and `×` close button.
  - Flat `.btn-tab-bar` actions (`+`, `Profiles`, `Sidebar`) directly follow tabs, with `Settings` (`⚙`) anchored cleanly at the trailing edge.
- **Start Page (`app.rs::render_empty_workspace`)**:
  - Rendered on `--theme-bg` when zero tabs exist: centered light `Termewnal` heading, `.list-group` quick actions (`Open a new terminal`, `Profiles & connections`, `Quick SSH connect`, `New profile`, `Settings`, `Import Tabby config`), up to 3 recent saved profiles, and a full-width dark bottom footer with shortcuts & version.
- **Profile Tree Sidebar (`sidebar.rs`)**:
  - Borderless `.tree-item` rows on `chrome_bg` with `1px` right hairline, compact `+` / `SSH` / `Import` / `×` header controls, `Filter profiles...` box, flat tag/group toggles, drag-and-drop group sorting, and `~/.ssh/config` + Tunnel sections.
- **In-Pane Terminal Toolbars (`app.rs::render_pane`, `ssh_ui.rs`) & Search (`terminal_ergonomics.rs`)**:
  - Sleek `terminal-toolbar` (`bg_more` + `1px` bottom border) on SSH, Serial, and Telnet tabs with status dot, transport latency/state, `Reconnect`/`Cancel`, `SFTP`, and `Ports` actions.
  - Top-right docked `search-panel` (`border-radius: 0 0 5px 5px`) with `Search`, `▲`, `▼`, and `×`.
- **Settings (`settings_ui.rs`), Selector Modal (`command_palette.rs`), & SFTP Panel (`sftp_drawer.rs`)**:
  - Settings uses Tabby's left `.nav-pills` rail + flush `.form-line` rows, a live `john@doe-pc$ ls` color scheme preview, `[ █ | | | ▁ ]` cursor buttons, window chrome toggles (`flex_tabs`, `hide_tab_index`, `show_profile_sidebar`, `show_status_bar`), and bordered primary hotkey chips.
  - Command Palette matches Tabby's `selector-modal` (top borderless search box, `.list-group-light` items, `ENTER` hint on selected row).
  - SFTP Drawer features clickable path breadcrumbs, flat action buttons, and thin `3px` transfer progress bars.
