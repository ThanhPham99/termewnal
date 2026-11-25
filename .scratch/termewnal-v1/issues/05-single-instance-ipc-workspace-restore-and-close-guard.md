# 05: Single-Instance IPC, Multi-Window, Workspace Restore & Close Guard

**What to build:** Enforce a single-instance OS process over a local IPC socket to coordinate multiple application windows sharing runtime state, persist and restore tab/split-pane layouts across restarts (respawning local `Session`s at their last working directory and respecting `auto_connect_on_startup` metadata for remote `Session`s), and guard closing the last window when background tasks are active.

**Blocked by:** 02: Workspace Tabs, Split Panes, Pane Zoom & Broadcast Input

**Status:** resolved

- [x] Launching a second Termewnal instance connects over local IPC to the primary process and opens a coordinated window or tab instead of spawning an isolated competing process.
- [x] Closing and restarting Termewnal restores workspace tab and split-pane geometry, automatically respawning local shell `Session`s at their captured working directory where available.
- [x] Remote `Session` descriptors persisted in the workspace layout restore cleanly into a `Disconnected` state (waiting for user trigger or honoring `Profile.auto_connect_on_startup` once SSH is wired).
- [x] Attempting to close the last window queries an active-background-work predicate (covering Standalone `Tunnel`s and active `Transfer`s); when active work exists, a confirmation dialog offers Minimize/Keep Running in Background vs Stop & Quit cleanly.
- [x] Automated tests verify local IPC single-instance handoff, workspace layout serialize/restore, and close-guard state transitions when background tasks are active vs idle.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `d98a6d6`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
