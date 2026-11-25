# 01: Local Shell App & Atomic TOML Config

**What to build:** Scaffold the multi-crate Cargo workspace (`termewnal-core`, `termewnal-pty`, `termewnal-ssh` stub, `termewnal-ui`) and launch a working pure-Rust `iced` desktop window hosting a live local shell `Session` backed by `portable-pty` and `alacritty_terminal`, with XDG TOML configuration (`config.toml`, `profiles.toml`) persisted via atomic writes and 3 rotating backups.

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] Cargo workspace compiles cleanly across `termewnal-core`, `termewnal-pty`, `termewnal-ssh` (stub crate), and `termewnal-ui`.
- [ ] Launching the application auto-detects the OS default local shell (`$SHELL` on Unix; PowerShell, `pwsh`, CMD, Git Bash, or WSL on Windows) and renders an interactive terminal `Session` in a GPU-accelerated `iced` window.
- [ ] Resizing the window updates PTY dimensions and `alacritty_terminal` grid layout cleanly without corrupting output.
- [ ] `config.toml` and `profiles.toml` include a `schema_version` field, live under XDG standard directories, never serialize secret fields, and write atomically (`tempfile` + `fsync` + `rename`) while keeping 3 rotating backups (`.bak.1`..`.bak.3`).
- [ ] Integration tests verify real OS PTY spawn/input/output/resize and crash-safe atomic TOML write + backup rotation against a temp XDG directory.
