# Native Protocols and Pure-Rust Built-In Extensibility

Status: accepted

We will implement Tabby's additional connection protocols (`Serial`, `Telnet`, `Zmodem`, `Docker` container exec) and popular Tabby plugin capabilities (`Quick Commands`, `Output Highlighting`, `Workspace Templates`, `Config Sync`, `Quake Mode`, `System Tray`) as native pure-Rust modules inside the Termewnal Cargo workspace, rather than embedding a JavaScript, Lua, or WebAssembly plugin runtime.

## Context

`termewnal-v1` (ADR-0001 through ADR-0004) intentionally kept Serial, Telnet, Zmodem, runtime plugin loading, and Cloud Sync out of scope to deliver a reliable core terminal and SSH/SFTP workspace first. Users migrating from `Eugeny/tabby` rely on those protocols and built-in workflows for embedded hardware debugging, legacy network appliances, container shells, command snippets, and multi-device settings synchronization.

## Considered Options

- **Pure-Rust native modules (`termewnal-protocols` + core/UI modules)**: Preserves ADR-0001's low memory footprint, zero WebView/JS runtime overhead, deterministic static typing, and full end-to-end testability with in-process Rust test doubles, while delivering feature parity with Tabby's bundled packages and top plugins.
- **Embedded JS/WASM/Lua plugin runtime**: Supports arbitrary third-party Tabby JS packages or external scripts, rejected because Tabby JS plugins depend on Electron/DOM/Node APIs (`xterm.js`, Angular, Node `fs`) that cannot run cleanly over `iced` without re-introducing an Electron-like runtime, and WASM/Lua sandboxing adds large API surface without solving protocol parity directly.

## Consequences

- Reopens and supersedes the `Out of Scope` protocol and sync boundaries from `.scratch/termewnal-v1/spec.md` for `termewnal-v2`.
- Keeps ADR-0001 (`iced` pure-Rust GUI), ADR-0003 (`russh` isolated SFTP & resilient tunnels), and ADR-0004 (atomic TOML config, read-only `~/.ssh/config`, separated `Vault` with no secrets in TOML or unencrypted sync payloads) intact.
- Adds `crates/termewnal-protocols` to isolate `Serial`, `Telnet`, `Zmodem`, and `Docker` discovery/session engines from SSH (`termewnal-ssh`) and local PTY (`termewnal-pty`).
