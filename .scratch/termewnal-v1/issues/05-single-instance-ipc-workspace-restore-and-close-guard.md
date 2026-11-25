# 05: Single-Instance IPC, Multi-Window, Workspace Restore & Close Guard

**What to build:** Enforce a single-instance OS process over a local IPC socket to coordinate multiple application windows sharing runtime state, persist and restore tab/split-pane layouts across restarts (respawning local `Session`s at their last working directory and respecting `auto_connect_on_startup` metadata for remote `Session`s), and guard closing the last window when background tasks are active.

**Blocked by:** 02: Workspace Tabs, Split Panes, Pane Zoom & Broadcast Input

**Status:** ready-for-agent

- [ ] Launching a second Termewnal instance connects over local IPC to the primary process and opens a coordinated window or tab instead of spawning an isolated competing process.
- [ ] Closing and restarting Termewnal restores workspace tab and split-pane geometry, automatically respawning local shell `Session`s at their captured working directory where available.
- [ ] Remote `Session` descriptors persisted in the workspace layout restore cleanly into a `Disconnected` state (waiting for user trigger or honoring `Profile.auto_connect_on_startup` once SSH is wired).
- [ ] Attempting to close the last window queries an active-background-work predicate (covering Standalone `Tunnel`s and active `Transfer`s); when active work exists, a confirmation dialog offers Minimize/Keep Running in Background vs Stop & Quit cleanly.
- [ ] Automated tests verify local IPC single-instance handoff, workspace layout serialize/restore, and close-guard state transitions when background tasks are active vs idle.
