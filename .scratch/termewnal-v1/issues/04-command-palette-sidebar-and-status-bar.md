# 04: Command Palette, Collapsible Sidebar & Status Bar

**What to build:** Add the primary workspace navigation and status chrome: a collapsible Left Sidebar (`Ctrl+B`) showing grouped/tagged Saved `Profile`s, a keyboard-first Command Palette (`Ctrl+K` / `Ctrl+Shift+P`) to launch `Profile`s, quick-connect, switch `Session`s, or invoke actions, and a bottom Status Bar shell ready to display live `Transport`, `Tunnel`, and `Transfer` indicators.

**Blocked by:** 02: Workspace Tabs, Split Panes, Pane Zoom & Broadcast Input

**Status:** ready-for-agent

- [ ] Pressing `Ctrl+B` toggles the Left Sidebar, displaying Saved `Profile`s organized by group/tag alongside placeholders/sections for System SSH `Profile`s and Active `Tunnel`s.
- [ ] Pressing `Ctrl+K` or `Ctrl+Shift+P` opens a searchable Command Palette supporting `Profile` launching, Quick Connect triggering, active `Session` switching, and workspace commands.
- [ ] Clicking or selecting a Saved `Profile` opens a new `Session` in the active tab or split pane using its configured settings.
- [ ] A persistent bottom Status Bar renders active `Session` status slots and exposes clean state hooks for future `Transport`, `Tunnel`, and `Transfer` controls.
- [ ] Top-level `iced` `update()` tests verify sidebar toggling, Command Palette fuzzy filtering/execution, and `Profile` launch actions.
