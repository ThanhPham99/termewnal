# termewnal-ui

## Overview
Native Rust desktop UI crate (`termewnal-ui`) providing the `termewnal` Iced GUI binary, the `termewnal_ui` multi-protocol workspace library, and a Windows build script (`build.rs`) for embedding application icon and version metadata.

## Interface
- `[Cargo.toml:package]`: Package `termewnal-ui` inheriting workspace `version`, `edition`, and `rust-version`; defines library target `termewnal_ui` (`src/lib.rs`) and binary target `termewnal` (`src/main.rs`).
- `[Cargo.toml:dependencies]`: Links workspace crates `termewnal-core`, `termewnal-protocols`, `termewnal-pty`, and `termewnal-ssh` alongside `arboard`, `chrono`, `iced`, `interprocess`, `notify`, `notify-debouncer-mini`, `open`, `regex`, `rfd`, `serde`, `thiserror`, `tokio`, `toml`, `tracing`, and `zeroize`; adds `x11rb` and `zbus` under `[target.'cfg(target_os = "linux")'.dependencies]`, `winresource` under `[build-dependencies]`, and `rand`, `russh`, `russh-sftp`, and `tempfile` under `[dev-dependencies]`.
- `[build.rs:main]`: Cargo build script `fn main()` checking `CARGO_CFG_TARGET_OS` and configuring `winresource::WindowsResource` when targeting `"windows"`.
- Subdirectory contract: [src](./src/README.md) — `termewnal` binary entrypoint (`src/main.rs`), `termewnal_ui` crate root (`src/lib.rs`), and 29 UI/workspace/protocol/editor/IPC/theme submodules.

## Invariants
- `[build.rs:main]`: Windows resource embedding (`../../assets/termewnal-icon.ico`, `ProductName` = `"Termewnal"`, `FileDescription` = `"Termewnal Terminal & Multi-Protocol Workspace"`) runs strictly when `CARGO_CFG_TARGET_OS == "windows"` (missing env var defaults to `""`).
- `[build.rs:main]`: Resource compilation failure (`Err(err)` from `res.compile()`) is non-fatal and degrades to a `cargo:warning=` Cargo directive rather than aborting the build.
- `[Cargo.toml:target.'cfg(target_os = "linux")'.dependencies]`: Linux platform dependencies (`x11rb`, `zbus`) are restricted strictly to `target_os = "linux"`.
- `[src/README.md:Invariants]`: Child module invariants cover single-instance IPC framing (`<= 64 KiB`, 2s probe timeout, `BIND_ATTEMPTS = 3` cleanup retries + 1 final bind), borderless window and close-guard lifecycle (closing directly when `!should_minimize_to_tray_on_close()` and `!has_active_work()`), monotonic workspace IDs and focus/input routing, two-pass terminal rendering with selection contrast/clamping and drag auto-scroll, SSH generation guards and fail-closed host-key rejection, SFTP/Remote File Editor (`Ctrl+F`/Find & Replace/Undo-Redo/Go to Line/Unsaved Guard) and tunnel safety limits, secret zeroization/handling, and sidebar/contrast/Quake bounds.

## Side Effects
- `[build.rs:main]`: Reads `CARGO_CFG_TARGET_OS` from the process environment; when targeting `"windows"`, emits `cargo:rerun-if-changed=../../assets/termewnal-icon.ico` on stdout, reads `../../assets/termewnal-icon.ico` and invokes `winresource::WindowsResource::compile()` to emit Windows resource artifacts, and emits `cargo:warning=...` on stdout if compilation fails.
- `[src/README.md:Side Effects]`: Child `src/` runtime side effects include TOML/YAML/SSH config/scrollback/session-log disk I/O, PTY/SSH/SFTP/tunnel/serial/telnet/Docker/ZMODEM/sync I/O, single-instance IPC sockets/pipes, X11 global hotkeys, D-Bus tray/notifications, filesystem watchers, clipboard reads/writes, native file dialogs, OS window operations, vault mutations, font-name interning, and Iced GPU draw calls.

## Verification
- Run: `cargo test -p termewnal-ui`
