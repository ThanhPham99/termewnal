# 13: Non-Blocking SFTP Browser, Isolated Transfer Transport & Pipelined Transfers

**What to build:** Deliver the toggleable, resizable SFTP Browser & `Transfer` Queue Drawer (`Ctrl+Shift+E`) powered by an isolated `Transfer Transport` (with graceful multiplexed fallback when 2FA/interactive auth blocks silent secondary connections), pipelined `128–256KB` SFTP chunk streaming capped at 3 concurrent `Transfer`s, pause/cancel/retry controls, Close Guard integration, and drag-and-drop upload routing.

**Blocked by:** 04: Command Palette, Collapsible Sidebar & Status Bar, 05: Single-Instance IPC, Multi-Window, Workspace Restore & Close Guard, 07: SSH Session, Auth Methods & Strict known_hosts TOFU

**Status:** resolved

- [x] Pressing `Ctrl+Shift+E` opens a non-blocking SFTP File Browser and `Transfer` Queue drawer showing remote directory contents, upload/download progress, throughput, and errors without interrupting terminal I/O.
- [x] Bulk SFTP `Transfer`s open and reuse a dedicated `Transfer Transport` per `Profile`, automatically falling back to an SFTP channel on `Primary Transport` when one-time 2FA/interactive auth prevents silent secondary connections.
- [x] The `Transfer` queue dispatches pipelined `128–256KB` SFTP chunk writes/reads with concurrency capped at 3 active `Transfer`s by default and supports independent pause, cancel, and retry.
- [x] Dropping files onto the SFTP Drawer (or holding `Shift` while dropping onto an SSH Terminal pane) enqueues an upload `Transfer` to the active remote directory, while active `Transfer`s register with the Close Guard from ticket 05.
- [x] In-process `russh-sftp` integration tests verify concurrent pipelined upload throughput, zero terminal-channel blocking during heavy transfers, and automatic 2FA mux fallback.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `e60e399`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
