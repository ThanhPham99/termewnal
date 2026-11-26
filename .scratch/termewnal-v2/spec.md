# Termewnal v2 Specification: Tabby Feature Parity

Status: active

## Problem Statement

`termewnal-v1` delivered the pure-Rust core terminal emulator, resilient SSH/SFTP engine, and Tabby-inspired desktop chrome, while deferring several daily-driver `Eugeny/tabby` capabilities—including customizable keybindings, Quick Commands, tab customization, context menus, PTY mouse forwarding, Bell/OSC 9;4 progress feedback, advanced SSH forwarding (`ForwardAgent`, `ForwardX11`, `ProxyCommand`, Login Scripts), Serial/Telnet/Zmodem/Docker protocols, Quake mode, System Tray, Named Workspaces, and Config Sync.

## Solution

Extend Termewnal in pure Rust (`ADR-0005`) across `termewnal-core`, `termewnal-pty`, `termewnal-ssh`, a new `termewnal-protocols` crate, and `termewnal-ui` while preserving `ADR-0001`..`ADR-0004` guarantees (zero WebView/JS overhead, isolated SFTP `Transfer Transport`, strict `known_hosts`, and strict `Vault` separation):

1. **Core Schemas, Themes & Customization (`termewnal-core`)**:
   - Expand `BuiltinTheme` / color schemes to 25+ Tabby community themes (`Solarized Dark/Light`, `Gruvbox Dark/Light`, `One Dark`, `Monokai`, `Everforest`, `Rose Pine`, `Kanagawa`, `Ayu Dark/Mirage`, `Tomorrow Night`, `Material`, `Synthwave`, `Cobalt2`, `Horizon`, `Oceanic Next`, `PaperColor`, `Catppuccin Latte`, etc.) plus optional custom palette overrides.
   - Add `KeybindingMap` (single shortcuts + multi-key chords), `TabBarPosition` (`Top`, `Bottom`, `Left`, `Right`), `RightClickAction` (`Menu`, `Paste`, `Off`), cursor blink, window opacity, bell behavior (`Visual`, `Notification`, `Off`), `HighlightRule`s, `QuakeConfig`, `TrayConfig`, and `SyncConfig` to `config.toml`.
   - Persist `QuickCommand`s in `quick_commands.toml` and named `WorkspaceTemplate`s in `workspaces.toml` with atomic writes and rotating backups.
   - Extend `ProfileTarget` with `Serial`, `Telnet`, and `Docker` variants, plus SSH `forward_agent`, `forward_x11`, `proxy_command`, and `login_scripts`.
   - Extend `tabby_importer.rs` to import Tabby `serial`, `telnet`, `loginScripts`, `x11`/`agentForwarding`, and `quick-cmds` snippets cleanly.

2. **Terminal Ergonomics, Mouse Reporting, Bell, Progress & Logging (`termewnal-pty`)**:
   - Parse `OSC 9;4` progress states (`Normal(0..100)`, `Indeterminate`, `Error`, `Paused`, `Hidden`) and cursor blink options inside `TerminalBuffer`.
   - Encode PTY mouse reporting (`SGR 1006` / normal X10) when `mouse_mode` is active for apps like `vim`, `htop`, and `tmux`.
   - Extend URL detection to detect clickable local file paths (`path/to/file:line:col`, `file://...`) alongside `http(s)://` URLs.
   - Add `HighlightRule` regex matching over terminal rows, `clear_screen_and_scrollback`, scrollback export, and live `SessionLogger` file output.

3. **Advanced SSH (`termewnal-ssh`)**:
   - Implement SSH Agent Forwarding (`agent_forward` channel request + `server_channel_open_agent_forward` bridged to `AgentConnector`).
   - Implement SSH X11 Forwarding (`request_x11` + `server_channel_open_x11` bridged to local X11 display socket/TCP with `.Xauthority` cookie handling).
   - Implement `ProxyCommand` stream transport (`%h`, `%p`, `%r` substitution) as an `AsyncRead + AsyncWrite` feed into `russh`.
   - Implement `LoginScript` expect/send automation upon SSH session open and reconnect.

4. **Additional Protocols Crate (`termewnal-protocols`)**:
   - **Serial Terminal**: Port discovery via `serial2`, configurable baud/data bits/parity/stop bits/flow control, `Text`/`Readline`/`HexBytes` input modes, `Text`/`Hexdump` output modes, newline conversion (`LF`, `CRLF`, `CR`), and auto-reconnect.
   - **Telnet Client**: Async TCP `TelnetSession` with IAC/NVT option negotiation (`NAWS`, `TTYPE`, `ECHO`, `SGA`) and auto-reconnect.
   - **Zmodem (`rz`/`sz`)**: Pure-Rust ZMODEM frame engine + stream scanner detecting `ZPAD`/`ZBIN`/`ZHEX` sequences to send or receive files over any active `Session` stream.
   - **Docker / Podman**: Container discovery and `exec -it` integration for launching shell `Session`s directly inside running containers.

5. **Config Sync Engine (`termewnal-core`)**:
   - Synchronize `config.toml`, `profiles.toml`, `quick_commands.toml`, and `workspaces.toml` via GitHub Gist, WebDAV, or a local sync folder (with optional Argon2id + AES-256-GCM bundle encryption; credentials/tokens stay in `Vault`).

6. **Desktop UI, Quake Mode & System Tray (`termewnal-ui`)**:
   - Customizable `KeybindingMap` resolution (including multi-key chords) and Keybindings tab in Settings; `Ctrl+=`/`Ctrl+-`/`Ctrl+0` font zoom, `Ctrl+1..9` tab switching, `Ctrl+Shift+L` clear terminal.
   - Configurable tab bar position (`Top`/`Bottom`/`Left`/`Right`), tab rename, tab color accent, pin, duplicate, close others/to-right, drag-reorder/context menu, bell/activity indicators, desktop notifications, and `OSC 9;4` progress bar.
   - Right-click context menu (`Menu`/`Paste`/`Off`) + middle-click paste + mouse forwarding to PTY + `Ctrl+Click` file paths.
   - Quick Commands drawer/palette integration (send to focused pane or broadcast), Highlight rules UI, Session Logging & Save Output, Named Workspace Manager, Profile Editor full support for Serial/Telnet/Docker + SSH Forwarding/Login Scripts/Tunnels/Keepalive.
   - Zmodem transfer notifications & queue integration, Quake drop-down mode + global hotkey (X11 `XGrabKey` + IPC toggle), Linux DBus `StatusNotifierItem` System Tray, and Sync Settings UI.

## Implementation Issues

Decomposed under `.scratch/termewnal-v2/issues/01`..`10`.
