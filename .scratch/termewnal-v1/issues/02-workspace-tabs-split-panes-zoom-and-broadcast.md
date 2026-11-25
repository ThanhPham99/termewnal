# 02: Workspace Tabs, Split Panes, Pane Zoom & Broadcast Input

**What to build:** Extend the workspace UI so users can open multiple workspace tabs, split any pane horizontally or vertically into an arbitrary binary split tree, temporarily zoom/maximize a single pane, and toggle Broadcast Input across panes in the active tab behind a clear red safety banner.

**Blocked by:** 01: Local Shell App & Atomic TOML Config

**Status:** ready-for-agent

- [ ] Users can create, switch, reorder, and close multiple workspace tabs, each hosting one or more `Session`s.
- [ ] Users can split the focused pane horizontally or vertically, resize dividers, move focus between panes, and close individual panes cleanly within a binary split tree.
- [ ] Toggling pane zoom maximizes the active pane to fill the tab viewport and restores the prior split geometry on toggle-off.
- [ ] Enabling Broadcast Input mirrors keystrokes to all panes in the active workspace tab and renders a prominent red warning banner while active.
- [ ] Top-level `iced` Elm `update()` tests verify tab switching, binary split-tree insertion/removal, zoom toggle, and Broadcast Input routing/banner state.
