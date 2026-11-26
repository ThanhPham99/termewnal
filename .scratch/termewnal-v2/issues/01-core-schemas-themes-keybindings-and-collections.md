# 01: Core Schemas, Color Schemes, Keybindings, Quick Commands & Workspace Templates

**What to build:** Extend `termewnal-core` with 25+ Tabby-inspired color schemes + custom palette overrides, `KeybindingMap` (single & multi-key chords), `TabBarPosition`, `RightClickAction`, `BellBehavior`, `HighlightRule`, `QuickCommand` (`quick_commands.toml`), `WorkspaceTemplate` (`workspaces.toml`), extended `ProfileTarget` (`Serial`, `Telnet`, `Docker`, SSH `forward_agent`/`forward_x11`/`proxy_command`/`login_scripts`), `QuakeConfig`, `TrayConfig`, `SyncConfig`, and corresponding Tabby `config.yaml` importer enhancements.

**Blocked by:** None

**Status:** resolved

- [x] `BuiltinTheme` includes 25+ Tabby community themes with distinct ANSI + UI palettes, plus optional custom theme overrides in `TerminalConfig`.
- [x] `AppConfig` stores keybindings, tab bar position, right-click/middle-click mouse behavior, bell behavior, cursor blink, opacity, highlight rules, quake, tray, and sync settings with backward-compatible TOML deserialization.
- [x] `quick_commands.toml` and `workspaces.toml` load/save atomically with `schema_version` and 3 rotating backups.
- [x] `ProfileTarget` supports `Serial`, `Telnet`, `Docker`, and SSH `forward_agent`, `forward_x11`, `proxy_command`, and `login_scripts` without storing secrets in TOML.
- [x] `tabby_importer` imports Tabby `serial`, `telnet`, `loginScripts`, `x11`/`agentForwarding`, and quick commands cleanly.
- [x] Integration tests verify backward compatibility, atomic persistence, color palettes, and Tabby v2 import paths.

## Comments

- Resolved and verified with `cargo test --workspace` (all workspace tests pass including 30 `termewnal-core` integration tests and 2 unit tests).
