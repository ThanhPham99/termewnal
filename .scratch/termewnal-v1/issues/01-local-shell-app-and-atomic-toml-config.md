# 01: Local Shell App & Atomic TOML Config

**What to build:** Scaffold the multi-crate Cargo workspace (`termewnal-core`, `termewnal-pty`, `termewnal-ssh` stub, `termewnal-ui`) and launch a working pure-Rust `iced` desktop window hosting a live local shell `Session` backed by `portable-pty` and `alacritty_terminal`, with XDG TOML configuration (`config.toml`, `profiles.toml`) persisted via atomic writes and 3 rotating backups.

**Blocked by:** None (can start immediately)

**Status:** resolved

- [x] Cargo workspace compiles cleanly across `termewnal-core`, `termewnal-pty`, `termewnal-ssh` (stub crate), and `termewnal-ui`.
- [x] Launching the application auto-detects the OS default local shell (`$SHELL` on Unix; PowerShell, `pwsh`, CMD, Git Bash, or WSL on Windows) and renders an interactive terminal `Session` in a GPU-accelerated `iced` window.
- [x] Resizing the window updates PTY dimensions and `alacritty_terminal` grid layout cleanly without corrupting output.
- [x] `config.toml` and `profiles.toml` include a `schema_version` field, live under XDG standard directories, never serialize secret fields, and write atomically (`tempfile` + `fsync` + `rename`) while keeping 3 rotating backups (`.bak.1`..`.bak.3`).
- [x] Integration tests verify real OS PTY spawn/input/output/resize and crash-safe atomic TOML write + backup rotation against a temp XDG directory.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `a780a61`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
