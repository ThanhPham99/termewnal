# Design System — Termewnal (Adaptive Operational Chrome)

## 1. Visual Direction & Philosophy
Termewnal combines [`Eugeny/tabby`](https://github.com/Eugeny/tabby)-inspired multi-protocol ergonomics with a perceptual OKLCH surface hierarchy and `iced 0.14` native chrome:
- **Terminal-first, per-window adaptive chrome**: There is no persistent bottom status bar (`show_status_bar = false`), and the Profiles sidebar defaults open on desktop windows (`show_profile_sidebar = true`, `>= 1180px`) while auto-collapsing per window on `Compact` (`< 860px`) viewports and `Quake` drop-down windows (`Ctrl+Shift+S`, header toggle, Command Palette, or Settings; `Ctrl+B` passes through to `tmux`/bash while `Ctrl+Shift+B` controls Broadcast Input).
- **Flush 40px full-width top header (`tab-bar` + `tab-header`)**: Rectangular top/bottom/side tabs (`height: 40px` on `Top` headers) with a guaranteed drag spacer (`>= 72px` when space allows); active tabs merge cleanly into the terminal background (`--body-bg`) and carry a subtle `2px` active indicator bar.
- **Follow the color scheme**: UI surfaces (`chrome_bg`, `bg_more`, `panel_bg`, `card_bg`, `well_bg`, `secondary`, `input_bg`, `hairline`) derive dynamically via a 4-tier OKLCH lightness ladder from the active terminal color scheme so all 28 built-in schemes (starting with **Tabby Default** dark and **Tabby Default Light**) and `ThemeOverrides` drive a unified hierarchy across chrome and terminal cells.

## 2. Color & Surface Tokens (`ui_theme::ChromePalette` + `termewnal-core::theme`)
- **Tabby Default (Dark)** (`#171717` canvas, `#cacaca` / `#eeeeee` text, `#5da9f6` accent) and **Tabby Default Light** (`#f8f9fa` canvas, `#4d4d4c` / `#2a2a2a` text, `#4271ae` accent) anchor an ordered OKLCH surface ladder converted with binary-search sRGB gamut chroma clamping (`c_base.min(0.024)`, plus an ultra-dark anchor when `l_base < 0.165`):
  - **Surface 0** (`bg_more`, `chrome_bg`): top header, `SidebarDock`, inset chrome (`l_base - 0.050` / `-0.034` dark; `l_base - 0.060` / `-0.044` light).
  - **Terminal Canvas** (`term_bg`): `alacritty_terminal` grid background (`l_base`).
  - **Input Well** (`input_bg`): recessed input field surface (`l_base - 0.018` dark; `l_base + 0.012` light).
  - **Elevated Surfaces** (`well_bg` / `bg_less`, `panel_bg`, `card_bg` / `secondary`): inset wells, `SFTP+` overlays/modals, and active controls/cards stepping outward from canvas (`+0.028` / `+0.050` / `+0.078` dark; `-0.026` / `+0.022` / `-0.086` light).
  - **Hairlines** (`hairline`, `border_subtle`, `border`): crisp `1px` opaque borders and split dividers.
- **Adaptive contrast & `iced 0.14` discipline**: `ensure_contrast_against` enforces readable ink against surface sets (`text_primary >= 4.5`, `text_secondary >= 4.5`, `text_muted >= 3.1`, `text_emphasis >= 7.0`, `accent >= 3.1`, `accent_fg >= 4.5`), opaque surfaces without backdrop blur, zero animated width transitions, and `Normal (400)` / `Bold (700)` weights only.

## 3. Core Surfaces & Controls
- **Top Header & Tab Strip (`app.rs::render_tab_strip`, `window_chrome.rs`)**:
  - `40px` header (`TAB_BAR_HEIGHT = 40.0`) leading with a per-window Sidebar toggle button (`☰`), followed by 1-based numbered rectangular `tab-header`s (`hide_tab_index`, `flex_tabs`, `3px` bottom color tags from `TAB_COLOR_PRESETS`, `2px` activity pulse bar, `⋯` context menu trigger, `×` close button), `+` new-tab button, dynamic drag spacer (`TitleBarDragZone::with_resize_guard`, `>= 72px` reserved when `strip_width` allows), operational controls (`Quake`, `Settings`), and window caption buttons. (`Commands` and `Templates` live in the Sidebar footer.)
- **Start Page (`app.rs::render_empty_workspace`)**:
  - Rendered inside empty tabs: `New SSH connection`, `Import ~/.ssh/config`, direct protocol chips (`Local Shell`, `SSH`, `Serial`, `Telnet`, `Docker`), recent saved Profiles, and a compact shortcut legend (`Ctrl+Shift+S`, `Ctrl+K`, `Ctrl+Shift+D`, `Ctrl+Shift+E`).
- **Profile Tree Sidebar (`sidebar.rs`)**:
  - Sentence-case sections (`Active sessions`, `Profiles`, `Active tunnels`, imported OpenSSH hosts), borderless `.tree-item` rows on `chrome_bg` with `1px` hairline, `Filter profiles...` box, drag-and-drop group sorting, and a footer row (`SSH`, `Import`, `Commands`, `Templates`, `Settings`) plus `Hide sidebar` (`Ctrl+Shift+S`).
- **In-Pane Terminal Toolbars, Scoped Security Prompts, & Search (`app.rs::render_pane`, `ssh_ui.rs`, `terminal_ergonomics.rs`)**:
  - Sleek per-pane `terminal-toolbar` on SSH/Serial/Telnet/Docker tabs with status dot, transport state, `Reconnect`/`Cancel`, `SFTP+` toggle pill (`SFTP+` or `SFTP+ (N)`), and `Ports`; SSH `MitmAlert` (backed by persistent `SshSessionSlot::mitm_blocked`), `TOFU`, and interactive `AuthPrompt` overlays render strictly inside their owning pane.
  - Top-right docked `search-panel` (`border-radius: 0 0 5px 5px`) with `Search`, `▲`, `▼`, and `×`.
- **Settings (`settings_ui.rs`), Selector Modal (`command_palette.rs`), & SFTP+ Panel (`sftp_plus.rs` / `sftp_drawer.rs`)**:
  - Settings uses left `.nav-pills` + flush `.form-line` rows, live `john@doe-pc$ ls` color scheme preview, `[ █ | | | ▁ ]` cursor buttons, window chrome toggles (`flex_tabs`, `hide_tab_index`, `Show Profiles sidebar by default`, `show_status_bar`), and shortcut capture.
  - Command Palette (`selector-modal`) includes `Toggle Profiles Sidebar` (`Ctrl+Shift+S`), normalized sentence-case category badges (`Connect`, `Command`, `Session`, `Profile`, `SSH Config`, `Tunnel`), and `Enter` keycap hints.
  - SFTP+ (`sftp_plus.rs` / `sftp_drawer.rs`) docks as a per-session resizable right panel on eligible SSH panes (`!is_quake && pane_avail_w >= 680px && pane_avail_h >= 360px`), suppresses non-destructively into `auto_suppressed` on compact splits or `Quake` windows (`SFTP+` header pill opening an opaque in-pane `stack!` overlay), and scopes modals per pane and window (`SftpDismissModal(Option<SessionId>)`, `SftpModalRequestClose(Option<SessionId>)`).
