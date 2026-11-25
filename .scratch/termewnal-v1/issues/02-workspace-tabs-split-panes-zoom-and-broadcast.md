# 02: Workspace Tabs, Split Panes, Pane Zoom & Broadcast Input

**What to build:** Extend the workspace UI so users can open multiple workspace tabs, split any pane horizontally or vertically into an arbitrary binary split tree, temporarily zoom/maximize a single pane, and toggle Broadcast Input across panes in the active tab behind a clear red safety banner.

**Blocked by:** 01: Local Shell App & Atomic TOML Config

**Status:** resolved

- [x] Users can create, switch, reorder, and close multiple workspace tabs, each hosting one or more `Session`s.
- [x] Users can split the focused pane horizontally or vertically, resize dividers, move focus between panes, and close individual panes cleanly within a binary split tree.
- [x] Toggling pane zoom maximizes the active pane to fill the tab viewport and restores the prior split geometry on toggle-off.
- [x] Enabling Broadcast Input mirrors keystrokes to all panes in the active workspace tab and renders a prominent red warning banner while active.
- [x] Top-level `iced` Elm `update()` tests verify tab switching, binary split-tree insertion/removal, zoom toggle, and Broadcast Input routing/banner state.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `4d8f465`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
