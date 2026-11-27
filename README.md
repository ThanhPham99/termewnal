<p align="center">
  <img src="assets/termewnal-icon-128.png" alt="Termewnal Icon" width="128" height="128" />
</p>

<h1 align="center">Termewnal</h1>

<p align="center">
  A pure-Rust native terminal emulator and multi-protocol workspace for SSH, resilient tunnels, SFTP+, Serial, Telnet, and containers — inspired by <a href="https://github.com/Eugeny/tabby">Tabby</a>.
</p>

---

## Based on Tabby

> **Termewnal is inspired by and based on the design, workflows, and user experience of [Tabby](https://github.com/Eugeny/tabby) by [Eugeny](https://github.com/Eugeny).**
>
> Special thanks to **[Eugeny](https://github.com/Eugeny)** and all contributors of [`Eugeny/tabby`](https://github.com/Eugeny/tabby) for pioneering an ergonomic terminal & SSH workspace—including its adaptive color-scheme UI system, integrated tab strip, dual-pane SFTP experience, profile manager, quick commands, and community themes. Termewnal reimplements that UX natively in Rust (`iced` + `alacritty_terminal` + `russh`) with zero Electron/WebView overhead, plus built-in Tabby `config.yaml` import support.

---

## Key Features

### Native GPU Workspace & Ergonomics
- **Pure-Rust Desktop Shell**: Single-instance daemon (`iced` with `iced_wgpu`) supporting multiple OS windows coordinated over local IPC, borderless Tabby-style chrome, and configurable tab bar placement (`Top`, `Bottom`, `Left`, `Right`).
- **Tabs & Split Panes**: Arbitrary horizontal/vertical binary split panes, pane zoom, intentional **Broadcast Input** mode, tab pinning/renaming/coloring, Quick Commands, Named Workspaces, and Quake drop-down mode with global hotkey + Linux DBus System Tray (`StatusNotifierItem`).
- **Terminal Engine (`alacritty_terminal` + `portable-pty`)**: Configurable scrollback, in-buffer search (`Ctrl+F`), `OSC 9;4` progress tracking, PTY mouse forwarding, regex highlight rules, session logging, Vietnamese/CJK IME support, clickable HTTP(S) & local `file://` links, and **28 built-in color schemes** (starting with **Tabby Default** dark and **Tabby Default Light**) with custom palette overrides.

### Supervised SSH & Auto-Reconnect
- **Authentication & Forwarding**: Pure-Rust `russh` engine supporting `ed25519`/`rsa`/`ecdsa` keys, OS SSH Agent, passwords, keyboard-interactive 2FA/OTP, multi-hop `ProxyJump`, `ProxyCommand`, Agent Forwarding, X11 Forwarding, and post-connect `LoginScript` automation.
- **Host Verification**: Strict `~/.ssh/known_hosts` checks with interactive SHA-256 Trust On First Use (TOFU) prompts and hard MITM blocks on fingerprint mismatch.
- **Proactive Health & Clear Recovery State**: Active keepalive probes, sleep/wake clock-jump fast-probing, exponential backoff with jitter (`1s..30s`), `tmux`/`screen` auto re-attach, and live **`Auto-reconnecting`** / **`Reconnected successfully`** indicators surfaced in the pane toolbar, Left Sidebar, and Status Bar.

### Resilient Tunnels (`-L`, `-R`, `-D`)
- **Session-Bound & Standalone Tunnels**: Create Local, Remote, and Dynamic SOCKS5 forwards bound to `127.0.0.1` by default (with explicit opt-in prompts for `0.0.0.0`).
- **Seamless Recovery**: Preserves local listeners across transient SSH drops with a 10-second client hold grace window (avoiding `EADDRINUSE`), handles Remote `-R` `RequestDenied` backoff recovery, and displays live **`Auto-reconnecting`** / **`Reconnected successfully`** status in both the Sidebar and Status Bar.

### Per-Pane SFTP+ Panel & Resilient Transfers
- **Remote-Only SFTP+ Panel**: Automatically docks alongside newly connected SSH sessions on eligible wide panes (`>= 680x360`, non-Quake) and collapses smoothly into a pane-header `SFTP+` pill opening an in-pane overlay on narrow splits or Quake windows (`Ctrl+Shift+E` to toggle), supporting per-session resizable widths, bookmark/filter/sort workflows, inline viewer/editor, permissions editor, and **Open with...** OS application chooser with local edit monitoring & re-upload.
- **Isolated Pipelined Transfers**: Runs bulk uploads/downloads on a dedicated `Transfer Transport` with 16-chunk SFTP pipelining (falling back automatically to separate browser/transfer multiplexed channels on 2FA hosts), writes uploads and downloads via `.termewnal.part` with atomic rename and source integrity-verified resume, exponential backoff + jitter recovery, and live **`Auto-reconnecting`** / **`Reconnected successfully`** indicators.

### Multi-Protocol Connections
- **Serial**: Port discovery, configurable baud/framing/flow control, `Text` / `Readline` / `HexBytes` input modes, `Hexdump` output, and automatic reconnect.
- **Telnet**: Native async TCP Telnet client with IAC/NVT negotiation (`NAWS`, `TTYPE`, `ECHO`, `SGA`) and auto-reconnect.
- **ZMODEM & Containers**: Pure-Rust `rz`/`sz` ZMODEM file transfer engine and Docker/Podman container discovery + shell attachment.

### Safe Configuration, Vault & Migration
- **Atomic TOML Storage**: XDG-compliant configuration (`config.toml`, `profiles.toml`, `quick_commands.toml`, `workspaces.toml`) with schema versioning, `fsync` + atomic rename, 3 rotating `.bak` backups, and safe hot-reload.
- **Isolated `Vault`**: Passwords, key passphrases, and sync tokens never touch TOML files—stored in OS Keyring or an optional Argon2id + AES-256-GCM encrypted `Vault` with idle auto-lock and memory zeroization.
- **OpenSSH & Tabby Import**: Read-only `~/.ssh/config` integration (with 1-click clone to editable Profile), 1-click Tabby `config.yaml` importer, and optional encrypted Config Sync (GitHub Gist, WebDAV, or local directory).

---

## System Overview

Termewnal is organized as a Rust **1.91+** (edition `2024`) Cargo workspace (`Cargo.toml`) separating headless protocol/domain engines from the desktop shell:

```text
termewnal/
├── assets/                   # Embedded app icons (PNG 32..512, RGBA 64x64, Windows ICO)
├── crates/
│   ├── termewnal-core/       # Config schemas, atomic TOML persistence, Vault, OpenSSH/Tabby importers, sync
│   ├── termewnal-pty/        # PTY lifecycle, alacritty_terminal grid, IME, mouse, URL/file links, logging
│   ├── termewnal-protocols/  # Serial terminal, Telnet client, ZMODEM engine, Docker/Podman discovery
│   ├── termewnal-ssh/        # russh supervisor, ProxyJump/ProxyCommand/X11/Agent, resilient tunnels & SFTP
│   └── termewnal-ui/         # iced desktop application, Tabby-inspired UI, SFTP+, Status Bar, Quake/Tray
├── docs/adr/                 # Architecture Decision Records (ADR-0001..0008)
├── docs/designs/             # Active design specifications (Adaptive Operational Chrome)
├── scripts/                  # Linux .deb/.tar.gz packaging, Windows NSIS installer, worktree Cargo wrapper
├── DESIGN.md                 # Tabby-aligned visual & UX design system tokens
└── GLOSSARY.md               # Domain terminology
```

For architectural rationale and design tokens, see [DESIGN.md](./DESIGN.md), [GLOSSARY.md](./GLOSSARY.md), [AGENTS.md](./AGENTS.md), and `docs/adr/`.

## Subdirectories & System Boundaries
- `crates`: [`./crates/README.md`](./crates/README.md) — Intermediate workspace blueprint indexing all five Rust crates (`termewnal-core`, `termewnal-protocols`, `termewnal-pty`, `termewnal-ssh`, `termewnal-ui`) and their package/source contracts.
- `scripts`: [`./scripts/README.md`](./scripts/README.md) — Component package containing `build-linux.sh` (`.deb`, `.tar.gz`, `SHA256SUMS-linux.txt` release pipeline), `termewnal.nsi` (Windows `x86_64` per-user NSIS installer), and `wt-cargo.sh` (Git-worktree shared `target/` Cargo wrapper).
- `assets/`: Static binary icon assets (`termewnal-icon-{32,64,128,256}.png`, `termewnal-icon.png`, `termewnal-icon-64.rgba`, `termewnal-icon.ico`) embedded by `termewnal-ui` and packaged by `scripts/`.
- `docs/`: Documentation directory containing Architecture Decision Records (`docs/adr/0001`–`0008`), design specifications (`docs/designs/`), and agent instructions (`docs/agents/`).

## Global Invariants
- **Crate Dependency Boundaries**: `termewnal-core` provides core schemas, persistence, and secret storage without depending on PTY, network protocol, or UI crates; `termewnal-pty` depends on `termewnal-core`; `termewnal-protocols` and `termewnal-ssh` depend on `termewnal-core` and `termewnal-pty`; `termewnal-ui` composes all four workspace crates while gating `x11rb`/`zbus` to `target_os = "linux"` and Windows icon/version resource compilation (`build.rs`) to `CARGO_CFG_TARGET_OS == "windows"`.
- **Schema-Versioned Atomic Storage & Backup Recovery**: All persisted TOML stores (`config.toml`, `profiles.toml`, `layout.toml`, `sftp_state.toml`, `quick_commands.toml`, `workspaces.toml`) enforce `schema_version == 1` (`CURRENT_SCHEMA_VERSION`) and write via sibling temp staging + `sync_all` + atomic rename with `MAX_BACKUPS = 3` `.bak` slots; hot reload retains the prior valid snapshot on error and validates `.bak.1` before restoring without rotating backups.
- **Credential Isolation & Memory Zeroization**: Passwords, passphrases, and sync tokens never serialize into TOML files or `SyncBundle` (`ALLOWED_BUNDLE_FILES`); secrets stay in `SecretStore` (`KeyringSecretStore`, `InMemorySecretStore`, or `EncryptedVault` using `"TMNVLT01"` + Argon2id + AES-256-GCM AAD at `0o600` Unix mode with `max(monotonic, wall)` idle auto-lock), while UI/auth/login-script secret buffers remain wrapped in `Zeroizing` and refuse persistence while `vault.requires_unlock()` is true.
- **Fail-Closed Host Verification, Proxy & Input Safety**: `known_hosts` verification aborts immediately on `@revoked` or fingerprint mismatch (`HostKeyMismatch`, blocking auto-reconnect via `MitmAlert`), `UiSshInteractionHandler::verify_unknown_host` fails closed to `HostKeyDecision::Reject`, new public tunnel binds (`Local`/`Dynamic`) or `bind_host` changes require explicit confirmation (`allow_public_bind = true`, while edits keeping the same public `bind_host` preserve approval), HTTP `CONNECT` proxy dialing rejects header control characters and over-reads (`16 KiB` cap), and SFTP/Zmodem filenames reject path traversal (`validate_remote_entry_name`, `sanitize_remote_filename`).
- **Concurrency, Lock Ordering & Lifecycle Barriers**: `TerminalBuffer` releases internal state mutexes before acquiring `update_signal` (never locking `term` under `update_signal` in feed paths) and clamps dimensions to `>= 1x1`; async SSH/SFTP/Serial/Telnet/transfer state updates check session or task generations (`reader_gen`, `task_gen`, `(id, generation)`), `AppState::remove_session` stops session tunnels with a barrier (`stop_session_tunnels_with_barrier`) before disconnecting transports, and single-instance IPC enforces `64 KiB` frames, 2s probe timeouts, and `BIND_ATTEMPTS = 3` stale-socket cleanup iterations plus one final bind attempt (up to 4 bind attempts).
- **Packaging Preconditions**: `scripts/build-linux.sh` enforces `set -euo pipefail`, non-empty `VERSION` (`Cargo.toml` fallback), executable `termewnal` binary, and `libxkbcommon0, libfontconfig1` `.deb` dependencies; `scripts/termewnal.nsi` enforces per-user `HKCU`/`$LOCALAPPDATA` scope and 3-part `X.Y.Z` `VERSION`.

## Global Side Effects
- **Filesystem Persistence & Staging**: Reads/writes XDG configuration and `.bak` files, `vault.enc` (`0o600`), `~/.ssh/known_hosts` (`0o700`/`0o600`), read-only `~/.ssh/config` and Tabby `config.yaml` imports, scrollback exports, continuous session logs, staged Open-With/Zmodem/SFTP `.termewnal.part` files, and release packages under `dist/` and `target/pkg/`.
- **Subprocesses, Devices & Network I/O**: Spawns OS PTY shells, `ProxyCommand` child processes, `docker`/`podman` CLI commands, and external URL/app launchers; opens serial devices (`serial2`, 40 ms read timeout), TCP connections (`SSH`, `Telnet`, `SOCKS5`/`HTTP` proxies, X11, WebDAV/GitHub Gist `ConfigSyncService`), local tunnel TCP listeners, and pipelined SFTP sessions (256 KiB / 16 concurrent channels).
- **Background Threads, Desktop IPC & Rendering**: Runs Tokio worker/blocking tasks, PTY reader/waiter threads, filesystem watchers (`notify_debouncer_mini`), `"termewnal-ipc"` socket/pipe server threads, Linux X11 global hotkey threads (`x11rb`), D-Bus `StatusNotifierItem` tray and notification clients (`zbus`), OS keyring/clipboard/file-dialog calls (`keyring`, `arboard`, `rfd`), and Iced GPU draw calls (`iced_wgpu`) with process-global font-family interning.

---

## Default Keyboard Shortcuts

| Shortcut | Action |
| --- | --- |
| `Ctrl+K` / `Ctrl+Shift+P` | Open Command Palette |
| `Ctrl+Shift+S` | Toggle Profiles Sidebar (Per-Window) |
| `Ctrl+Shift+E` | Toggle SFTP+ Panel (Docked or Pane Overlay) |
| `Ctrl+F` | Search Terminal Scrollback |
| `Ctrl+,` | Open Settings |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Font Zoom In / Out / Reset |
| `Ctrl+Shift+L` | Clear Terminal |
| `Ctrl+1`..`Ctrl+9` | Switch Workspace Tabs |

*(Sidebar visibility can be toggled per window with `Ctrl+Shift+S`, while `Ctrl+B` passes directly through to `tmux`/bash and `Ctrl+Shift+B` triggers Broadcast Input; shortcuts and multi-key chords can be customized in Settings.)*

---

## Building & Running

Requires Rust **1.91+** (edition `2024`).

```bash
# Run the desktop application
cargo run -p termewnal-ui --bin termewnal

# Build release binary
cargo build --release -p termewnal-ui --bin termewnal

# Build Linux .deb and .tar.gz installers (dist/)
./scripts/build-linux.sh
```

## System Verification

- Run: `cargo test --workspace`

```bash
# Type-check all workspace crates and integration tests
cargo check --workspace --all-targets

# Run the full test suite
cargo test --workspace

# Run per-crate tests
cargo test -p termewnal-core
cargo test -p termewnal-protocols
cargo test -p termewnal-pty
cargo test -p termewnal-ssh
cargo test -p termewnal-ui
```

---

## Acknowledgments

- **[Tabby (`Eugeny/tabby`)](https://github.com/Eugeny/tabby)** by **[Eugeny](https://github.com/Eugeny)** — Termewnal is deeply grateful for and directly inspired by Tabby's visual design language, workflow ergonomics, SFTP & tunnel UX concepts, and theme palette ecosystem.
- Built on the Rust ecosystem (`iced`, `alacritty_terminal`, `russh`, `russh-sftp`, `portable-pty`, and `tokio`).
