# Cross-Platform Packaging and Distribution (v1.0.0)

Status: accepted

We will distribute Termewnal as the `termewnal` binary at version `1.0.0`, isolate Linux-only desktop integrations (`x11rb` X11 global hotkey and `zbus` D-Bus StatusNotifierItem/Notifications) behind `#[cfg(target_os = "linux")]` with `Null*` platform stubs on non-Linux targets, and build native Linux (`.deb` + `.tar.gz`) and Windows (`.exe` NSIS installer + `.zip`) packages through `scripts/build-linux.sh`, `scripts/termewnal.nsi`, and GitHub Actions (`ubuntu-latest` + `windows-latest`) rather than cross-compiling Windows C crypto dependencies (`aws-lc-sys` / `ring`) from Linux.

## Context

Prior to `1.0.0`, `termewnal-ui` depended unconditionally on `x11rb` and `zbus` in `Cargo.toml` and emitted a binary named `termewnal-ui` at `0.1.0`. Meanwhile, `russh` and `reqwest` pull in `aws-lc-sys` and `ring`, which require native C/Windows toolchains (`MSVC` / `clang-cl` + Windows SDK / NSIS) to compile cleanly for `x86_64-pc-windows-msvc`.

## Considered Options

- **OS-gated Linux adapters + native GitHub Actions runners**: Keeps `termewnal-core`, `termewnal-pty`, `termewnal-ssh`, `termewnal-protocols`, and all workspace/terminal/SFTP/tunnel UI features identical on Linux and Windows while gracefully stubbing Linux D-Bus tray/notifications and X11 global hotkeys via the existing `SystemTrayAdapter`, `DesktopNotifier`, and `GlobalHotkeyRegistrar` traits, embeds the Windows `.ico` and `windows_subsystem = "windows"` via `winresource`, and produces native `.deb`, `.tar.gz`, NSIS `.exe`, and `.zip` artifacts on native CI runners (while allowing immediate local Linux `.deb` and `.tar.gz` builds via `scripts/build-linux.sh`).
- **Local Linux-to-Windows cross-compilation (`cargo-xwin` / `mingw-w64`)**: Avoids CI wait times, but requires installing `clang`/`llvm`/`mingw-w64`/`nsis` on developer machines and introduces fragile C-ABI and `aws-lc-sys` build matrix issues compared to `windows-latest`.
- **Full Win32 native tray, `RegisterHotKey`, and toast notification backend in `1.0.0`**: Adds Windows-only runtime complexity and new Win32 dependencies that cannot be execution-tested on Linux hosts; deferred past `1.0.0` behind the existing trait boundaries.
