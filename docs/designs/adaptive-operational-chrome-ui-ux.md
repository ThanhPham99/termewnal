# Termewnal Comprehensive UI/UX Redesign ("Adaptive Operational Chrome")

* **Author:** Moevuive
* **Date:** 2026-11-27

## Overview

Termewnal is a pure-Rust native terminal emulator and multi-protocol workspace (`iced 0.14` + `alacritty_terminal` + `russh`) serving SSH and DevOps engineers who run multiple concurrent servers, SSH tunnels (`-L`, `-R`, `-D`), and SFTP transfers, including in edge-docked Quake drop-down windows.

This brief synthesizes a full codebase inspection and seven parallel Goldfish reviews to define **"Adaptive Operational Chrome"**: a single, implementable UI/UX architecture that keeps Termewnal's 28-theme dynamic palette identity (`ChromePalette`), collapses duplicated chrome from 76px+ down to a unified ~40px header, unlocks per-window sidebar collapsing, fixes terminal starvation when `SFTP+` auto-opens in split panes or Quake mode, eliminates overlapping telemetry and cryptic labels for newcomers, and uses Google Stitch plus Hallmark (calibrated to `iced 0.14`) to lock the target screens before updating `DESIGN.md` and `crates/termewnal-ui`.

## Analysis

### Reviewer Completion Status

| Reviewer Lens | Status | Primary Focus |
| :--- | :--- | :--- |
| `egm-design` | `completed` | Window geometry, split/Quake economics, security UX, polling cost |
| `egm-codebase-details` | `completed` | Exact code invariants, test contracts, hit-testing, theme wiring |
| `egm-zoom-out` | `completed` | Scope minimization, YAGNI, single-launcher consolidation |
| `egm-comprehension` | `completed` | First-time user onboarding, terminology drift, hidden interactions |
| `egm-implementation-readiness` | `completed` | `iced 0.14` limits, schema/config migration safety, execution order |
| `egm-structural` | `completed` | 7-screen information architecture, surface ladder, Hallmark rules |
| `egm-inventive-catalyst` | `completed` | TRIZ contradiction elimination & Ideal Final Results |

### Consolidated Findings (Blockers & Majors)

1. **Governance & State Drift Between `DESIGN.md`, Stitch, and Code (`Blocker`)**
   * `DESIGN.md` specifies Tabby defaults (`show_profile_sidebar = false`, `Ctrl+B`, tab-bar `Profiles`/`Sidebar` buttons, no status bar), while Stitch project `4811973832648448562` uses fixed Apple HIG tokens (`#131315`, `#0A84FF`, `SF Pro`, `14px` sheets) that discard the 28 terminal themes.
   * Meanwhile, live code locks the Left Sidebar permanently open across seven layers: `config.rs:390` (`default_true`), `sidebar.rs:253-267` (`set_open`/`toggle` force `true`), `sidebar.rs:986-1064` (unconditional `SidebarDock`), `settings_ui.rs:309-312` (forces `true` and omits UI toggle), `keybindings.rs:63-110` + `keybinding_bridge.rs:136` (`KeyAction::ToggleSidebar => None`), `command_palette.rs` (omits command item), and `app.rs:9320` (unconditional `SidebarDock::new`). Five integration tests (`navigation_chrome.rs`, `tabby_ui_ux.rs`) explicitly assert this forced-open lock.
   * `AppState.sidebar` is a global singleton across windows; collapsing it for Quake or narrow viewports without per-window runtime overrides in `WindowState` (`app.rs:1655`) would corrupt desktop windows and persisted `config.toml`.

2. **77px Dual Top Chrome & Borderless Hit-Test Collisions (`Blocker`)**
   * `app.rs:9335` stacks `render_top_system_title_bar` (`38px`) + `hairline_h` (`1px`) + `render_tab_strip` (`38px`) = **77px** even when `TabBarPosition::Top`, contradicting `window_chrome.rs:583`.
   * Merging them into one ~40px bar breaks three coupled mechanisms unless redesigned together:
     * **Drag-zone starvation & hijacking**: Under 15+ tabs, `middle_spacer` shrinks to `0px`, making the window undraggable; conversely, wrapping `DragSource::Tab` inside `TitleBarDragZone` triggers `WindowDragStart` instead of tab dragging because `DragSource` does not capture `ButtonPressed(Left)` until `6px` movement.
     * **`BorderlessResizeFrame` collision**: `window_chrome.rs:414-433` intercepts the top `5px` (`EDGE_RESIZE_HIT_PX`) across `width - 190.0` (a magic number tied to the 4 existing system buttons) and intercepts the rightmost `5px` of the top-right Close button.
     * **Non-Top tab edges & hardcoded menu coordinates**: `TabBarPosition::{Bottom, Left, Right}` still need client-side drag and `─ □ ✕` controls (`decorations: false`). Tab `⋯` menus hardcode `menu_y = TAB_BAR_HEIGHT * 2.0 + 1.0` (`app.rs:9623`) instead of anchoring via `anchored_menu`.

3. **`SFTP+` Auto-Open Starvation in Multi-Pane Splits & Quake Mode (`Blocker`)**
   * `AppState::handle_ssh_event` (`app.rs:8809-8814`) calls `auto_open_if_allowed()` unconditionally on SSH connect regardless of pane width, split count, or Quake mode.
   * `SftpDrawerDock::clamped_panel_width` (`sftp_drawer.rs:634-645`) can shrink `TerminalViewport` down to `140px` (~17 columns; or even fewer when `pane_w < 281px` because `max_allowed` floors `drawer_w` at `140px`), while `sftp_plus.rs:760-803` wraps 10 toolbar controls across multiple rows and eats vertical space in `405px`-tall `QuakeEdge::Top`/`Bottom` windows.
   * `self.sftp_drawer_width` (`app.rs:12176`) is a single global scalar that resizes drawers across unrelated panes, and suppressing auto-open via `set_open(false)` currently marks `manually_closed = true`, blocking auto-open permanently.

4. **Security & Data-Loss Defects Discovered in Current UI State (`Blocker`)**
   * **`DismissMitmAlert` bypasses host-key protection**: Clicking `Dismiss` on a MITM banner (`app.rs:5599`, `ssh_ui.rs:440`) removes the entry from `mitm_alerts`, clearing `mitm_alert(session_id).is_none()` (`app.rs:4734`) and re-enabling `Enter`-to-reconnect despite the mismatch remaining in `known_hosts`.
   * **Cross-pane `FileViewer` edit loss & inverted backdrop rule**: `Message::SftpDismissModal` (`app.rs:13118`) clears `SftpModalState::None` across *all* drawers, so dismissing a modal in Pane B drops unsaved remote file edits in Pane A. Additionally, `sftp_plus.rs:2552` inverts backdrop dismissal (`if *dirty || !*editable`), dismissing dirty editors on outside clicks while locking clean ones. `FileViewer` + search/replace bars also overflow vertically in `405px` Quake windows.
   * **Quake `hide_on_blur` kills native file dialogs**: `Message::WindowUnfocused` (`app.rs:4482`) hides Quake whenever an OS `rfd` upload/download picker or passphrase prompt opens.
   * **App-global `SshUiState` head-of-line blocking**: Background tab TOFU/2FA prompts block active foreground tabs, and `has_modal_open()` locks terminal input without `Escape` handling on inline auth banners.
   * **`ProfileEditor` `Cancel` styled as `PillTone::Danger`**: Bright red `Cancel` button sits beside `Delete` (`profile_ui.rs:1341-1343`).

5. **28-Theme Contrast Drift & `extended_palette()` Divergence (`Blocker`)**
   * `ChromePalette::from_palette` (`ui_theme.rs:244-289`) uses multiplicative HSL darkening (`hsl_darken`), collapsing `term_bg`, `bg_more`, and `chrome_bg` on dark/OLED schemes (`#000000`, `AyuDark`), flipping light-theme direction around `luminance > 0.80` (`TokyoNightLight`), ignoring `palette.ui_surface`/`ui_border`, and leaving `text_muted` at `2.6:1` and `ansi[1..3]` badges unchecked on light backgrounds (`SolarizedLight`, `Cobalt2`, `Synthwave84`).
   * `PaneFrame` (`split_view.rs:507`) and `draw_interactive_divider` (`ui_theme.rs:149`) read Iced's `theme.extended_palette()` instead of `ChromePalette`, and multiple files (`ui_theme.rs:960,1530`, `sftp_plus.rs:1100,1104,3376`, `server_stats_bar.rs:241`) hardcode literal RGB colors.

6. **Discoverability, Navigation Fragmentation & Telemetry Duplication (`Major`)**
   * Five overlapping navigation/launch surfaces (`Start Page`, `Left Sidebar`, `Command Palette`, `ProtocolLauncher`, `StatusBar`) confuse newcomers while hiding `Docker/Serial/Telnet` (`ProtocolLauncherState` is only inside `Ctrl+K`), exposing cryptic sidebar footer abbreviations (`"SSH"`, `"Import"`, `"Cmds"`, `"Spaces"`), and renaming `"Tunnels"` as `"Ports"` in SSH toolbars and Profile Editor.
   * Sidebar UX conflicts: `[Active | All]` pills sit directly under `ACTIVE SESSIONS` with no `"Saved profiles"` header; clicking a Saved Profile selects once and requires an unmarked 400ms double-click to launch; `"Imported from ~/.ssh/config"` says `"Clone"` instead of `"Save as profile"`.
   * SFTP & ZMODEM gaps: unlabeled custom-drawn toolbar icons, no Download toolbar button, silent `Shift+Drop` upload missing from the drag-hover banner, `0.62` asymmetric split drop zone, raw `"Mux fallback on Primary Transport"` message, and no file-picker `Browse...` button in `ZmodemSendUiState`.
   * Four incompatible tab/switch controls (`tabby_underline_tab`, `tabby_auth_tile_style`, `segmented_button_style`, and solid-accent `settings_nav_pill_style` used for both navigation and 4-6 inline form selectors).
   * Operational telemetry repeats 3-4 times across Sidebar, Pane Toolbar, `ServerStatsBar` (whose `.wrap()` inflates vertical height in splits while polling every 2s even on hidden tabs), and `StatusBar` (which must keep `StatusBarSnapshot` / `StatusBarAction` signatures for 6 integration test suites).

### Inventive Contradictions & Ideal Final Results (`egm-inventive-catalyst`)

1. **SFTP+ Auto-Open vs. Split & Quake Geometry**: Separate logical SFTP readiness (channel warm-up and directory caching on connect) from layout column reservation (`SftpDrawerDock`). Keep side-by-side docking on wide panes (`>= 700px`, `!is_quake`), and resolve narrow splits / Quake without burying SFTP+ behind manual discovery by using either an `opaque(...)` `stack!` edge-peek strip (`28px` live badge + instant slide/overlay or drag-hover reveal, zero terminal column loss) or a focused-pane singleton viewport paired with a `32px` single-row SFTP path/action bar.
2. **40px Top Bar vs. Drag Handle & 15+ Tabs & 4-Edge Docking**: Use role-morphing headers (`TabBarPosition::Top` merges bar + tabs with a guaranteed `>= 72px` `TitleBarDragZone` floor and `32px` inset buttons; `Bottom`/`Left`/`Right` keep a slim `34px` system/context header) plus elastic tab compression (`92..200px`) hiding inactive `×` below `140px` and adding a `[▾ N]` overflow jump pill.
3. **Newcomer Onboarding vs. High-Density Power Console**: Separate by operational phase: when zero tabs exist, suppress the redundant sidebar and promote `CommandPalette`'s Omni-Search (`user@host`, detected `~/.ssh/config`, Tabby import, `New Shell`, `Protocols`) onto the Start Page; once a session opens, transition cleanly into dense Console Mode with `Ctrl+B`-free sidebar toggling.
4. **28 Themes vs. Guaranteed Contrast & Legibility**: Replace multiplicative HSL with bounded **OKLCH perceptual surface steps** (chroma-capped structural neutrals `C <= 0.024`, monotonic `ΔL` stepping outward from `term_bg` on ultra-dark `#000` and ultra-light themes) plus a closed-loop contrast solver and 28-theme unit test gate, validated visually in Stitch across 5 boundary archetypes (`Ultra-Dark`, `Neutral Dark`, `High-Chroma Dark`, `Warm Muted`, `High-Luminance Light`).
5. **Single-Owner Telemetry Scope**: Assign one canonical owner per scope (`Header`: global launcher, Vault lock, Settings, window controls, tab `Zoom`/`Broadcast`; `Pane Toolbar`: `24px` single-row host latency, conditional `Reconnect`/`Cancel` only when unhealthy, `SFTP+`, `Tunnels`, inline tabular stats fold; `Bottom Activity Strip`: `0px` when idle, surfacing only active transfers/degraded tunnels unless pinned).

### `iced 0.14` Translation & Hallmark Calibration Contract

* `Cargo.toml:26` uses `iced 0.14` with `["tokio", "advanced"]` only — no `svg`, no CSS blur/backdrop-filter, no animated drawer widths (frame-by-frame width interpolation causes `SIGWINCH` PTY reflow storms), and `ui_font_medium()` aliases `Font::DEFAULT` (`Normal`) because `cosmic-text` drops missing `Weight::Medium` glyphs.
* `iced::widget::button::Status` has no `Focused` state (`Active`, `Hovered`, `Pressed`, `Disabled` only), so Hallmark Floor #6 `:focus-visible` applies to inputs (`text_input::Status::Focused`), lists, and custom focusable pane widgets rather than raw buttons.
* Stitch + Hallmark prototypes (`--scope app`, `Console`-inspired restraint) must enforce `iced`-realizable primitives only: `Normal` and `Bold` weights, opaque 4-tier OKLCH surfaces with `1px` hairlines (no border+shadow card nesting per Hallmark Floor #11 / `craft.md:36`), sentence-case labels (Hallmark Gate 67, preserving acronyms like `SSH`, `SFTP`, `TCP`, `CPU`), and zero middle-dot chains (Gate 63).

### Key Open Trade-Offs (Resolved & Locked by Author)

1. **Google Stitch + Hallmark Validation Scope (`Resolved`)**: Keep Google Stitch and Hallmark strictly bounded by the `iced 0.14` Calibration Contract and `ChromePalette` 4-tier OKLCH tokens as the visual alignment blueprint before updating `DESIGN.md` and implementing in Rust.
2. **Narrow/Quake `SFTP+` Auto-Open Behavior (`Resolved`)**: Auto-open `SFTP+` per SSH pane only when pane geometry sustains a usable terminal (`pane_avail_w >= ~640–700px && pane_avail_h >= 360px && !is_quake`). Below threshold or in Quake drop-down mode, record `auto_suppressed` (without setting `manually_closed = true`) and surface a pane-header `SFTP+ (N)` pill with live transfer count that opens an `opaque` `stack!` overlay on click or drag-hover.
3. **Default Sidebar Startup State & Keybinding (`Resolved`)**: Unlock per-window collapsing in `WindowState`, keep desktop default `show_profile_sidebar = true` on wide screens (`>= 1180px`) with automatic per-window collapse on `< 860px` (`Compact`) and Quake windows, and map `KeyAction::ToggleSidebar` to `Ctrl+Shift+S` (avoiding `Ctrl+B` bash/tmux and `Ctrl+Shift+B` Broadcast Input collisions).

## Goals vs. Non-Goals

* **In-Scope Goals:**
  * Unify top system title bar and horizontal tab strip into a single ~40px header for `TabBarPosition::Top`, with guaranteed drag space, non-colliding `BorderlessResizeFrame`, and slim system headers for `Bottom`/`Left`/`Right`.
  * Unlock per-window Left Sidebar collapsing with automatic Compact (`< 860px`) and Quake auto-collapse, sentence-case headings, unified launch click behavior, and non-cryptic actions.
  * Make per-pane `SFTP+` width/height-aware (no terminal starvation in splits or Quake, separate `auto_suppressed` from `manually_closed`, per-pane drawer width) and slim down its toolbar.
  * Replace multiplicative HSL chrome math with a 4-tier perceptual OKLCH `ChromePalette` + 28-theme contrast test gate, eliminating literal RGB/extended_palette leaks.
  * Fix critical security/data-loss UX bugs (`DismissMitmAlert` re-arming reconnect, cross-pane `SftpDismissModal` wiping dirty edits, inverted `backdrop_dismiss`, Quake `hide_on_blur` during file dialogs, `ProfileEditor` red `Cancel` button).
  * Deduplicate controls and telemetry across Start Page, Sidebar, Pane Toolbar, `ServerStatsBar`, `StatusBar`, and `CommandPalette` while preserving `StatusBarSnapshot`/`StatusBarAction` APIs.
  * Produce and Hallmark-audit Google Stitch reference screens before locking `DESIGN.md`.

* **Out-of-Scope Non-Goals:**
  * Rewriting `crates/termewnal-core`, `termewnal-pty`, `termewnal-ssh`, or `termewnal-protocols` protocol engines.
  * Adding CSS/HTML/WebView rendering or Electron dependencies (`iced 0.14` native Rust stays fixed).
  * Breaking `CURRENT_SCHEMA_VERSION = 1` TOML compatibility or `StatusBarSnapshot`/`StatusBarAction` contracts.
  * Implementing animated sliding drawers (`window::frames()` width transitions that trigger PTY `SIGWINCH` storms).

## Alternatives Weighed

1. **Keep Existing Tabby Flat Chrome (`DESIGN.md` as written)**
   * *Rejected*: Leaves `77px` dual top chrome, permanent `250px` sidebar lock-in, broken dark-theme tier collapse, 4-way telemetry duplication, hidden protocols/commands, and unusable split/Quake SFTP+ geometry unresolved.
2. **Adopt Static Apple HIG Design from Existing Stitch Project (`4811973832648448562`)**
   * *Rejected*: Hardcodes fixed `#131315`/`#0A84FF` colors and macOS-style shadows/blur assumptions, erasing Termewnal's 28-theme dynamic palette engine and ignoring split-pane `SFTP+`, `Tunnels`, `Remote File Editor`, and `Quake` edge constraints.
3. **Minimal Pruning-Only Pass in Rust Without Stitch/Hallmark (`egm-zoom-out` baseline)**
   * *Partially incorporated, rejected as sole approach*: Merging `top_system_bar` + `tab_strip`, unlocking sidebar, and gating `SFTP+` fixes immediate geometry issues cheaply, but misses the broken HSL contrast math, `SshUiState`/`SftpModalState`/`DismissMitmAlert` safety bugs, inconsistent 4-tab modal primitives, and visual validation requested by the user via Google Stitch + Hallmark.
4. **Adaptive Operational Chrome (`iced 0.14` Calibrated Stitch + Hallmark + Structural Rust Refactor — Proposed)**
   * *Selected*: Combines minimal structural consolidation, perceptual 4-tier OKLCH `ChromePalette`, security/state isolation fixes, and Hallmark-audited Stitch prototypes mapped strictly to `iced 0.14` capabilities.

## Key Decisions & Staging for Next Steps

* **Locked Visual Direction**: Adopt `"Adaptive Operational Chrome"` — 28-theme dynamic OKLCH 4-tier `ChromePalette` (`Tier 0 Canvas`, `Tier 1 Recessed Chrome`, `Tier 2 Interactive Control`, `Tier 3 Floating Overlay`) with `Normal (400)` / `Bold (700)` typography, `1px` hairlines, sentence-case labels, and zero blur or card-in-card borders, verified in Stitch (`projects/15108367123833185129`) and Hallmark App Screen (`scratch/hallmark-app/`, `0 FAIL / 0 REFLEX / 0 WARN` static sweep).
* **Locked Chrome & SFTP+ Behavior**: Merge `top_system_bar` + `tab_strip` into a unified `~40px` header for `TabBarPosition::Top` (retaining slim drag/control headers for `Bottom`/`Left`/`Right`); unlock per-window Left Sidebar collapsing (`Ctrl+Shift+S`, auto-collapsed in `< 860px` and Quake); auto-open `SFTP+` per SSH pane when `>= ~640–700px` wide and `>= 360px` tall outside Quake, falling back to a pane-header `SFTP+ (N)` overlay toggle below threshold.
* **Design System Update**: Rewrite root `DESIGN.md` to match the locked `iced 0.14` spec (replacing stale Tabby `show_profile_sidebar = false` and `Ctrl+B` claims) before modifying `crates/termewnal-ui`.
* **Schema & Migration Contract**: Retain `CURRENT_SCHEMA_VERSION = 1` across all TOML files using additive `#[serde(default)]` fields (`sidebar_auto_collapse`, per-window layout overrides) and separate `auto_suppressed` from `manually_closed` on `SftpDrawerState`.
* **Test Migration Plan**: Explicitly update the 5 forced-open sidebar assertions in `crates/termewnal-ui/tests/suite/navigation_chrome.rs` and `tabby_ui_ux.rs`, while preserving `StatusBarSnapshot` / `StatusBarAction` behavioral tests and adding a 28-theme contrast invariant test.
* **Locked Execution Sequence**:
  1. Update `DESIGN.md` with the locked `iced 0.14` Adaptive Operational Chrome contract.
  2. Fix critical security/state-isolation defects (`MitmAlert` persistence on dismiss, scoped `SftpModalState`, `FileViewer` backdrop guard, Quake modal blur lock, `ProfileEditor` `Cancel` tone).
  3. Ship Unified `40px` Header + Per-Window Collapsible Sidebar + `Ctrl+Shift+S` (`KeyAction::ToggleSidebar`).
  4. Ship 4-Tier OKLCH `ChromePalette` + Unified `PaneToolbar` / `ModalShell` / `SegmentedControl` + Responsive `SFTP+` auto-open & single-home telemetry.

## Google Stitch & Hallmark Prototype Verification Log

* **Stitch Project Created**: `projects/15108367123833185129` (*"Termewnal — Adaptive Operational Chrome (iced 0.14)"*, Design System `assets/8369308359466938333`), covering 4 core screens:
  1. `Termewnal — Wide SSH + SFTP+ Workspace` (`06915c16ab64486e8b9e5f19c0d4fd26`, edited via `edit_screens` to remove an extra web nav rail and header avatar)
  2. `Termewnal — 2-Pane Split SSH with Adaptive SFTP+ Edge-Peek` (`e66dda7fa21d43b597411fc47128d40b`, edited via `edit_screens` to remove fake header tabs and rotated vertical text incompatible with `iced 0.14`)
  3. `Termewnal — Quake Top Drop-Down Mode` (`4697ba9ef12b41d2824af130ca6b8e91`)
  4. `Termewnal — Onboarding Start Page & Omni-Launcher` (`155f455fe3b748efb3415770f8f422a9`)
* **Empirical Hallmark Audit on Raw Stitch HTML** (`sloplint.mjs --scope app --genre modern-minimal`): Raw Stitch exports hit `78 FAIL / 21 REFLEX` (`wide-ssh.html`), `45 FAIL / 14 REFLEX` (`start-page.html`), `27 FAIL / 10 REFLEX` (`quake.html`), and `7 FAIL / 14 REFLEX` (`split-ssh.html`) — flagging web hallucinations (`font-medium` weight dropped by `cosmic-text`, `writing-mode: vertical-rl`, `transition-all`, middle-dot chains, literal hexes, missing `:focus-visible`, and `uppercase` tracked tags).
* **Calibrated Hallmark App Screen Prototype**: Built an `iced 0.14`-constrained interactive 5-view / 5-theme OKLCH reference (`scratch/hallmark-app/index.html`, `app.css`, `tokens.css`, `base.css`) covering `Wide SSH + SFTP+`, `Split SSH`, `Quake Mode`, `Start Page`, and `Modals & Operational States` across `Default Dark`, `Default Light`, `Tokyo Night`, `Cobalt2`, and `Solarized Light`. Static `sloplint` run verified after contrast calibration (note: render tier skipped because global `puppeteer-core` is absent in this environment).
