# Termewnal v1 Specification

Status: resolved

## Problem Statement

Terminal and SSH power users migrating from Electron-based clients like Tabby frequently suffer from four interconnected reliability and UX failures:

1. **Fragile SFTP uploads**: Uploading large files (>1 MB) or nested directories freezes the UI or drops the active terminal session because SFTP shares the same transport stream and waits synchronously on chunk acknowledgments.
2. **Broken tunnel recovery on network drops**: When Wi-Fi switches, laptops sleep/wake, or connections drop silently, remote port forwarding fails on reconnect with `RequestDenied`, local port forwarding collides with stale sockets (`EADDRINUSE`), and tunnels bound to terminal tabs die when the tab closes.
3. **Configuration corruption and secret coupling**: Storing settings and encrypted secrets inside a single YAML file risks corrupting profiles on crash or multi-window writes, while manual edits or migrations lack safe recovery.
4. **Heavy Electron resource footprint and noisy UX**: High memory usage, sluggish large-scrollback rendering, and opaque connection/tunnel states make day-to-day multi-host operations slower than a native tool should be.

## Solution

Termewnal is a cross-platform, pure-Rust native desktop terminal emulator and SSH/SFTP workspace built for resilient networking, isolated non-blocking file transfers, crash-safe configuration, and crisp GPU-accelerated UX:

- **Native GPU Desktop Workspace**: Built on `iced` (`iced_wgpu`), `alacritty_terminal`, and `portable-pty` inside a single-instance multi-window process, delivering fast startup, split panes, a command palette, and a non-blocking SFTP/Transfer drawer.
- **Supervised Networking & Resilient Tunnels**: Powered by pure-Rust `russh` on `tokio`, a Connection Supervisor actively monitors `Transport` health via keepalives and sleep/wake fast-probes, preserves terminal scrollback across reconnects, keeps local `Tunnel` listeners bound across transient drops with a 10-second hold window, and backs off cleanly on `RequestDenied` for remote `Tunnel`s.
- **Isolated Pipelined SFTP Transfers**: Bulk `Transfer`s run on a dedicated `Transfer Transport` with pipelined chunk writes, `.termewnal.part` atomic uploads, byte-offset resume, and recursive directory support without starving interactive terminal input.
- **Atomic TOML Config & Separated Vault**: Non-secret configuration lives in XDG-standard TOML files with atomic writes, rotating backups, safe hot-reload, read-only `~/.ssh/config` inspection, and a Tabby `config.yaml` importer, while secrets live strictly isolated in OS Keyring or an optional Argon2id/AES-256-GCM encrypted `Vault`.

## User Stories

### Workspace, Windows & Local Shell

1. As a user, I want Termewnal to run as a single-instance process across multiple OS windows, so that opening a new window shares my unlocked `Vault`, active Standalone `Tunnel`s, and configuration state without concurrent file corruption.
2. As a user, I want Termewnal to auto-detect available local shells for my OS (`$SHELL` on Linux/macOS; PowerShell, `pwsh`, CMD, Git Bash, and WSL on Windows), so that I can open local terminal `Session`s immediately without manual setup.
3. As a user, I want to organize `Session`s across top workspace tabs and arbitrary horizontal/vertical split panes inside each tab, so that I can monitor and operate multiple hosts or local shells side by side.
4. As a user, I want to temporarily zoom/maximize any split pane and restore the split layout with one shortcut, so that I can inspect dense output comfortably.
5. As a user, I want an optional Broadcast Input mode across panes in the active workspace tab accompanied by a prominent red warning banner, so that I can send commands to multiple panes intentionally without accidentally typing destructive commands unawares.
6. As a user, I want a collapsible Left Sidebar (`Ctrl+B`) listing grouped/tagged Saved `Profile`s, read-only System SSH `Profile`s from `~/.ssh/config`, and Active `Tunnel`s, so that I can launch or inspect anything with minimal screen clutter.
7. As a user, I want a keyboard-first Command Palette (`Ctrl+K` / `Ctrl+Shift+P`), so that I can launch `Profile`s, quick-connect to hosts, switch `Session`s, toggle `Tunnel`s, and trigger workspace actions without reaching for the mouse.
8. As a user, I want responsive GPU-accelerated terminal ergonomics including 10,000-line configurable scrollback, scrollback search (`Ctrl+F`), `Ctrl+Click` URL opening, optional copy-on-select, multi-line paste confirmation, Vietnamese/CJK IME input support, font/ligature/cursor settings, and built-in themes (Catppuccin, Tokyo Night, Dracula, Nord, Dark/Light), so that daily terminal work feels fast and comfortable.
9. As a user, I want my workspace tab and split-pane layout restored when restarting Termewnal—automatically respawning local `Session`s at their last working directory and restoring SSH `Session`s in a ready-to-reconnect state (unless `auto_connect_on_startup` is enabled on the `Profile`), so that restarting does not trigger a storm of interactive auth prompts.
10. As a user, I want a confirmation dialog when closing the last application window while Standalone `Tunnel`s or active `Transfer`s are running—letting me either keep running in the background/tray or stop cleanly—so that I never silently kill active tunnels or leave corrupt transfers behind.

### SSH Authentication, Host Verification & Jump Hosts

11. As an SSH user, I want to authenticate using private keys (`ed25519`, `rsa`, `ecdsa` with optional passphrases), SSH Agent (`SSH_AUTH_SOCK` on Unix; OpenSSH named pipe and Pageant on Windows), passwords, or keyboard-interactive 2FA/OTP prompts, so that Termewnal works across standard and hardened SSH servers.
12. As an SSH user, I want multi-hop `ProxyJump` support configured in `Profile`s or imported from `~/.ssh/config`, so that I can reach hosts behind bastion servers seamlessly.
13. As a security-conscious user, I want strict `~/.ssh/known_hosts` verification with interactive Trust On First Use (TOFU) SHA256 fingerprint prompts for unknown hosts and a hard connection block with a clear MITM warning if a host key changes, so that connections are never silently trusted blindly.

### Resilient Networking & Connection Supervision

14. As a remote engineer, I want active TCP and SSH keepalives (default every 15 seconds, max 3 missed probes) configurable globally and per `Profile`, so that half-open connections across NATs and firewalls are detected promptly instead of hanging on keystrokes.
15. As a laptop user waking from sleep or switching Wi-Fi/VPN networks, I want Termewnal to detect system clock jumps (or respond to a manual Probe/Reconnect trigger) and immediately send a short 3-second keepalive probe, so that dead `Transport`s start reconnecting right away rather than waiting 45 seconds.
16. As an SSH user experiencing a network drop, I want the `Session` to stay open with its scrollback intact, show a non-modal status bar countdown with `Reconnect now` and `Cancel` buttons using exponential backoff with jitter (`1s..30s`), and clearly mark reconnection with a `--- Reconnected at HH:MM:SS ---` divider, so that I never lose terminal context or get interrupted by modal popups.
17. As a user running persistent remote work inside `tmux` or `screen`, I want a per-`Profile` option to automatically re-attach my multiplexer command upon `Session` reconnection, so that remote shell state resumes automatically after a drop.
18. As a user connected through multi-hop `ProxyJump`, I want a dropped intermediate hop to tear down the dependent tunnel chain cleanly and rebuild all hops in order (`A -> B -> C`), so that reconnection never leaves orphaned inner channels stuck.
19. As a user, I want a live Status Bar displaying real-time `Transport` health (`Connected` with latency, `Degraded`, `Reconnecting`), active `Tunnel` states, and 1-click reconnect/toggle controls, so that I always know the true network state at a glance.

### Resilient Port Forwarding (`Tunnel`)

20. As a developer, I want Local (`-L`), Remote (`-R`), and Dynamic SOCKS5 (`-D`) `Tunnel`s that can either attach to a `Profile`/`Session` or run Standalone in the background even when terminal tabs are closed, so that database, web, and proxy tunnels outlive individual terminal views.
21. As a developer using Local or Dynamic `Tunnel`s, I want Termewnal to keep the local TCP listener bound across transient SSH `Transport` drops and hold incoming local client sockets for a configurable grace window (default 10 seconds) until the `Transport` recovers, so that local clients neither hit `EADDRINUSE` on reconnect nor immediately fail during brief network blips.
22. As a developer using Remote `Tunnel`s, I want Termewnal to send `cancel_tcpip_forward` cleanly on manual stop/exit and automatically retry with backoff when hitting `RequestDenied` after an ungraceful disconnect, so that remote forwards recover smoothly once the remote SSH daemon releases the stale socket.
23. As a security-conscious user, I want Local and Dynamic `Tunnel`s to bind `127.0.0.1` by default and require explicit opt-in with a clear security warning before binding `0.0.0.0`, so that local ports are never accidentally exposed to a LAN.

### Reliable SFTP File Browser & `Transfer` Queue

24. As a user transferring files, I want a resizable, toggleable SFTP Browser and `Transfer` Queue drawer (`Ctrl+Shift+E`) that never blocks terminal interaction, so that I can browse remote directories and track upload/download speed, progress, and errors without interrupting shell work.
25. As a user uploading large files, I want bulk SFTP `Transfer`s isolated on a dedicated `Transfer Transport` (falling back automatically to a multiplexed channel on the `Primary Transport` if one-time interactive/2FA auth blocks silent secondary connections), using pipelined SFTP chunk writes (`128–256KB` chunks in flight), so that large uploads reach full throughput without freezing or dropping the terminal `Session`.
26. As a user uploading over unstable networks, I want files larger than 1 MB written first to `<filename>.termewnal.part` and atomically renamed on completion—enabling exact byte-offset resume on reconnect, with automatic fallback to direct writes if the server denies `.part` creation or rename—so that failed uploads do not waste bandwidth or leave corrupted destination files.
27. As a user transferring multiple files and folders, I want recursive directory transfers capped at 3 concurrent `Transfer`s by default, preserving file permissions, skipping symlink loops safely by default (configurable), and supporting pause/cancel/retry plus a conflict prompt (`Overwrite`, `Resume`, `Skip`, `Rename` with `Apply to all` and configurable default policy), so that batch transfers remain predictable and recoverable.
28. As a desktop user, I want dragging a file onto a Terminal pane to paste its shell-escaped path, while dragging onto the SFTP Drawer (or holding `Shift` while dropping onto an SSH Terminal pane) enqueues an upload `Transfer` to the current remote directory, so that both workflows are fast and unambiguous.

### Configuration Persistence, Secrets `Vault` & Migration

29. As a user, I want non-secret settings (`config.toml`) and `Profile`s (`profiles.toml`) stored as XDG-standard TOML files written atomically (`tempfile` + `fsync` + `rename`) with `schema_version` and 3 rotating backups (`.bak.1..3`), so that crashes or power loss never corrupt my configuration.
30. As a power user editing TOML files externally, I want safe file-watch hot-reload that keeps the active valid configuration running if a syntax error is saved, surfaces an actionable error toast with line details, and offers 1-click restore from `.bak.1` without overwriting my edited file, so that manual typos never destroy my work.
31. As an existing OpenSSH user, I want Termewnal to surface hosts from `~/.ssh/config` in read-only mode with a 1-click "Clone to Profile" action, so that I can use existing SSH setups immediately without risking accidental edits to `~/.ssh/config`.
32. As a user migrating from Tabby, I want a 1-click importer for Tabby's `config.yaml` that brings over `Profile`s, groups/tags, and `Tunnel` definitions (ignoring plugin settings and prompting cleanly for credentials if Tabby's vault is locked), so that switching to Termewnal is effortless.
33. As a security-conscious user, I want credentials and passphrases stored strictly outside TOML config inside `Vault`—defaulting to the native OS Keyring with an optional Master-Password encrypted local `Vault` (Argon2id + AES-256-GCM) that unlocks once per process session, auto-locks after configurable idle/sleep (default 30 minutes) or manual lock, and zeroizes key material in memory—so that secrets remain protected across both desktop and portable setups.

## Implementation Decisions

- **Workspace Module Boundaries**: Organize the codebase as a multi-crate Cargo workspace separating headless domain and protocol engines from the GUI shell:
  - `termewnal-core`: `Profile` and application config TOML schemas with `schema_version`, atomic writer with 3-slot `.bak` rotation, safe hot-reload watcher, read-only `~/.ssh/config` parser, Tabby `config.yaml` importer, `Vault` abstraction (OS Keyring adapter + Argon2id/AES-256-GCM encrypted store with `zeroize`), and workspace layout persistence models.
  - `termewnal-pty`: Cross-platform local shell auto-detection and PTY lifecycle wrapper on `portable-pty`, paired with `alacritty_terminal` grid state, search, URL detection, and working-directory tracking.
  - `termewnal-ssh`: `russh` + `russh-sftp` async engine on `tokio`, implementing auth flows, `ProxyJump` nested stream chaining, strict `known_hosts` TOFU/mismatch verification, the Connection Supervisor (keepalive, clock-jump fast-probe, backoff + jitter, ordered `ProxyJump` cascade rebuild), the `Tunnel` manager (Local/Remote/Dynamic, Standalone vs Session-bound, preserved local listener with 10-second hold grace timeout, `cancel_tcpip_forward` + `RequestDenied` backoff), and the pipelined SFTP `Transfer` queue (`Transfer Transport` isolation with 2FA mux fallback, `.termewnal.part` atomic rename + byte-offset resume, conflict resolution callbacks, concurrency semaphore, symlink-loop protection).
  - `termewnal-ui`: `iced` (`iced_wgpu`) single-instance application shell (local IPC listener/client for multi-window coordination), Elm-style top-level state and `update()` loop, custom terminal grid renderer with IME and drag-and-drop routing, split-pane binary tree, Command Palette, Left Sidebar, Status Bar, SFTP/Transfer Drawer, and window-close guard for active Standalone `Tunnel`s/`Transfer`s.
- **UI & Terminal Stack (ADR-0001, ADR-0002)**: Pure Rust native GUI via `iced` + `alacritty_terminal` + `portable-pty`, targeting Linux (X11/Wayland), macOS (Metal), and Windows (DX12/Vulkan), with Linux prioritized for initial verification.
- **Connection Supervisor State Machine**: Each `Transport` transitions explicitly through `Disconnected -> Connecting -> Connected -> Degraded -> Reconnecting -> Disconnected`, publishing events to bound `Session`s, `Tunnel`s, and the Status Bar. Keepalive defaults (`15s` interval, `3` missed probes) and clock-jump detection (`delta > keepalive_interval` triggering an immediate `3s` probe) live at the supervisor boundary so UI code only observes status snapshots and invokes high-level commands (`connect`, `probe_now`, `reconnect_now`, `cancel_reconnect`, `disconnect`).
- **Isolated `Transfer Transport` & SFTP Pipelining**: Bulk `Transfer` operations open a dedicated `Transfer Transport` authenticated silently from `Vault`/Agent; if authentication requires interactive user input (2FA/OTP) that cannot be replayed silently, the queue falls back gracefully to opening an SFTP subsystem channel over the active `Primary Transport`. Large writes (`>1 MB`) issue concurrent in-flight SFTP write requests (`128–256 KB` chunks) against `.termewnal.part` and atomically rename upon completion, querying remote file size for byte-offset resume and falling back to direct-path writes if `.part` or rename fails with permission/unsupported errors.
- **Tunnel Listener Preservation**: Local (`-L`) and Dynamic SOCKS5 (`-D`) listeners stay bound to `127.0.0.1` (unless explicitly opted into `0.0.0.0`) inside the `Tunnel` task across `Transport` drops; accepted local streams during `Reconnecting` wait up to a 10-second grace timeout on a transport-ready watch channel before resetting cleanly. Remote (`-R`) `Tunnel`s invoke `cancel_tcpip_forward` on intentional teardown and apply exponential backoff on `RequestDenied` during recovery.
- **Configuration, Migration & `Vault` Boundary (ADR-0004)**: `termewnal-core` enforces strict separation so secret fields never serialize into TOML files. Atomic writes flush to a sibling temporary file with `fsync`, rotate `.bak.1..3`, and atomically rename. File-watcher updates parse into a candidate snapshot first and reject malformed edits without overwriting disk state.

## Testing Decisions

- **Testing Philosophy**: Tests verify external observable behavior across high-level module boundaries rather than mocking internal helper functions or testing private state.
- **Prior Art**: None exists yet in this greenfield repository; this specification establishes the initial test suite and harness conventions.
- **Primary Seam — Headless Core Engine + Real Boundary Doubles**:
  - **In-Process `russh` SSH/SFTP Test Server (`termewnal-ssh`)**: Integration tests spin up a real loopback `russh` server capable of fault injection—dropping TCP sockets mid-upload, holding remote forward port bindings to force `RequestDenied`, chaining multi-hop `ProxyJump` servers, enforcing `known_hosts` key changes, requiring interactive 2FA auth to verify `Transfer Transport` mux fallback, and denying `.part` rename permissions—to verify end-to-end `Transport` recovery, `Tunnel` continuity, and SFTP resume.
  - **Temp XDG Filesystem & In-Memory Keyring Adapter (`termewnal-core`)**: Tests exercise real file I/O for atomic writes, backup rotation, corrupt-TOML hot-reload retention, `~/.ssh/config` parsing/cloning, Tabby `config.yaml` importing, and Argon2id/AES-256-GCM `Vault` lock/unlock/zeroize behavior without requiring a live desktop Secret Service daemon.
  - **Real Local PTY (`termewnal-pty`)**: Integration tests spawn real OS shells via `portable-pty` and verify `alacritty_terminal` grid output, resize events, scrollback search, and working-directory capture.
- **Secondary Seam — Top-Level `iced` State Machine & IPC (`termewnal-ui`)**:
  - Deterministic unit/integration tests drive the top-level Elm `update(state, message)` handler and local IPC protocol directly (Command Palette filtering, split-pane binary tree operations, Broadcast Input safety toggle, drag-and-drop Terminal vs SFTP routing, non-modal reconnect state, and quit confirmation when Standalone `Tunnel`s or `Transfer`s are active). Visual rendering, GPU frame pacing, and desktop IME/drag-drop integration are verified via manual smoke testing on Linux X11/Wayland.

## Out of Scope

- Serial port, Telnet, and ZModem protocols.
- Third-party runtime plugin API (JavaScript, Lua, or WebAssembly extension runtime).
- Cloud configuration/vault synchronization service.
- Modifying or writing back to `~/.ssh/config` directly.
- Unattended silent acceptance of changed SSH host keys or binding tunnels to `0.0.0.0` without explicit opt-in.
- Following recursive directory symlinks during SFTP folder transfers by default.
- Electron, Tauri/WebView, or TUI-only (`ratatui`) UI targets.

## Further Notes

- Architecture Decision Records governing this specification: `0001-pure-rust-native-gui.md`, `0002-iced-alacritty-terminal-ui-stack.md`, `0003-russh-isolated-sftp-and-resilient-tunnels.md`, and `0004-toml-config-readonly-ssh-and-dual-vault.md` under `docs/adr/`.
- All terminology conforms to `GLOSSARY.md`.
- Note on current development environment: the workspace host runs Linux (Ubuntu X11) with Rust toolchain installed; cross-platform macOS and Windows targets are architected from day one and validated following Linux core/UI stability.
- Recommended next workflow step: run `/to-tickets` to decompose this specification into ordered, self-contained implementation issues under `.scratch/termewnal-v1/issues/`.
