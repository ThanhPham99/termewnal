# scripts

## Overview
Packaging and developer-tooling scripts for Termewnal. This directory contains the Linux release build and packaging pipeline ([`build-linux.sh`](./build-linux.sh)), the Windows x86_64 NSIS per-user installer definition ([`termewnal.nsi`](./termewnal.nsi)), and a Git-worktree Cargo wrapper that shares a single `target/` directory across worktrees ([`wt-cargo.sh`](./wt-cargo.sh)).

## Interface
- **Subdirectories**: None.
- **Linux Packaging ([`./build-linux.sh`](./build-linux.sh))**:
  - `[build-linux.sh:CLI]` Accepts no CLI flags or positional arguments (extra arguments are silently ignored); builds the `termewnal` binary from `termewnal-ui` and outputs `.deb`, `.tar.gz`, and `SHA256SUMS-linux.txt` artifacts under `dist/`.
  - `[build-linux.sh:BASH_SOURCE]` Resolves `ROOT_DIR` as `$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)`.
  - `[build-linux.sh:VERSION]` Optional input environment variable overriding the package version; defaults to the first column-0 `^version = "..."` entry parsed from `Cargo.toml`.
  - `[build-linux.sh:CARGO_TARGET_DIR]` Optional input environment variable overriding the Cargo target directory when locating `BIN_PATH` (defaults to `$ROOT_DIR/target`; resolved relative to `$ROOT_DIR` after `cd "$ROOT_DIR"` and honored by `cargo build`, while `PKG_ROOT` stays `$ROOT_DIR/target/pkg`).
  - `[build-linux.sh:install-user.sh]` Embedded user-local installer script generated inside the `.tar.gz` archive; ignores CLI flags, defines `HERE`, `BIN_DIR`, `APP_DIR`, and `ICON_BASE`, and reads `[build-linux.sh:HOME]` (failing under `set -u` if unset; ignores `XDG_DATA_HOME`) to target `$HOME/.local/bin`, `$HOME/.local/share/applications`, and `$HOME/.local/share/icons/hicolor`.
  - `[build-linux.sh:functions]` None defined.
- **Windows NSIS Installer ([`./termewnal.nsi`](./termewnal.nsi))**:
  - `[termewnal.nsi:Unicode True]` Enables Unicode NSIS installer compilation and includes `[termewnal.nsi:MUI2.nsh]`.
  - `[termewnal.nsi:VERSION]` Overridable preprocessor define (`!ifndef VERSION`, statically defaulted to `"1.0.3"` rather than parsed from `Cargo.toml`).
  - `[termewnal.nsi:BIN_PATH]` Overridable preprocessor define (`!ifndef BIN_PATH`, defaults to `"..\target\release\termewnal.exe"`).
  - `[termewnal.nsi:OUT_FILE]` Overridable preprocessor define (`!ifndef OUT_FILE`, defaults to `"..\dist\termewnal-${VERSION}-windows-x86_64-setup.exe"`).
  - `[termewnal.nsi:APP_NAME]`, `[termewnal.nsi:APP_PUBLISHER]`, `[termewnal.nsi:APP_URL]`, `[termewnal.nsi:UNINSTALL_KEY]` Preprocessor constants (`"Termewnal"`, `"ThanhPham99"`, `"https://thanhpham99.github.io/apps/termewnal/"`, `"Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_NAME}"`).
  - `[termewnal.nsi:Name]`, `[termewnal.nsi:OutFile]`, `[termewnal.nsi:InstallDir]`, `[termewnal.nsi:InstallDirRegKey]`, `[termewnal.nsi:RequestExecutionLevel]`, `[termewnal.nsi:SetCompressor]` Installer configuration directives (`${APP_NAME} ${VERSION}`, `${OUT_FILE}`, `$LOCALAPPDATA\Programs\${APP_NAME}`, `HKCU "${UNINSTALL_KEY}" "InstallLocation"` reusing prior install dir on upgrade, `user`, `/SOLID lzma`).
  - `[termewnal.nsi:MUI_ABORTWARNING]`, `[termewnal.nsi:MUI_ICON]`, `[termewnal.nsi:MUI_UNICON]`, `[termewnal.nsi:MUI_FINISHPAGE_RUN]`, `[termewnal.nsi:MUI_PAGE_WELCOME]`, `[termewnal.nsi:MUI_PAGE_DIRECTORY]`, `[termewnal.nsi:MUI_PAGE_INSTFILES]`, `[termewnal.nsi:MUI_PAGE_FINISH]`, `[termewnal.nsi:MUI_UNPAGE_CONFIRM]`, `[termewnal.nsi:MUI_UNPAGE_INSTFILES]`, `[termewnal.nsi:MUI_LANGUAGE]` Modern UI 2 page macros (no `MUI_PAGE_COMPONENTS`), icon defines (`..\assets\termewnal-icon.ico`), finish-page launch target (`$INSTDIR\termewnal.exe`), and `"English"` language macro.
  - `[termewnal.nsi:VIProductVersion]`, `[termewnal.nsi:VIAddVersionKey]` Windows version resource metadata (`"${VERSION}.0"`, `ProductName`, `CompanyName`, `FileDescription` = `"${APP_NAME} Installer"`, `FileVersion`, `ProductVersion`).
  - `[termewnal.nsi:Section "Termewnal (required)" SecMain]` Required main installation section (`SectionIn RO`).
  - `[termewnal.nsi:Section "Desktop Shortcut" SecDesktop]` Desktop shortcut section (selected by default without `/o` or `MUI_PAGE_COMPONENTS`).
  - `[termewnal.nsi:Section "Uninstall"]` Uninstaller section.
  - `[termewnal.nsi:functions]` None defined.
- **Git Worktree Cargo Wrapper ([`./wt-cargo.sh`](./wt-cargo.sh))**:
  - `[wt-cargo.sh:$@]` Accepts arbitrary Cargo subcommands, flags, and arguments and forwards them verbatim to `exec cargo "$@"`.
  - `[wt-cargo.sh:CARGO_TARGET_DIR]` Optional input environment variable; when unset or empty inside a Git repository or worktree, exported as `"$(dirname "$common_dir")/target"`.
  - `[wt-cargo.sh:functions]` None defined.

## Invariants
- `[build-linux.sh:set -euo pipefail]` Strict Bash error handling (`set -euo pipefail`) is active in both `build-linux.sh` and the generated `install-user.sh`.
- `[build-linux.sh:ROOT_DIR]` Assumes `build-linux.sh` resides one directory beneath repository root (`$ROOT_DIR`) and `Cargo.toml` exists at `$ROOT_DIR/Cargo.toml`.
- `[build-linux.sh:VERSION]` Exits with status `1` if `VERSION` is empty (or `""`) after reading `$VERSION` and parsing `Cargo.toml`; performs no Debian version syntax validation.
- `[build-linux.sh:ARCH]` Hardcodes Debian package architecture to `amd64` and `[build-linux.sh:TAR_ARCH]` tarball architecture to `x86_64`, assuming a native `x86_64` Linux host (`cargo build` passes no `--target` and does not check `uname -m`).
- `[build-linux.sh:BIN_PATH]` Expects `cargo build --release -p termewnal-ui --bin termewnal` to produce an executable (`-x`) at `${CARGO_TARGET_DIR:-$ROOT_DIR/target}/release/termewnal`; exits `1` otherwise.
- `[build-linux.sh:cp]` Requires `$ROOT_DIR/README.md`, `$ROOT_DIR/assets/termewnal-icon-{32,64,128,256}.png`, and `$ROOT_DIR/assets/termewnal-icon.png` (staged as `512x512` / `termewnal-512.png`).
- `[build-linux.sh:strip]` Runs `strip --strip-unneeded` on the `.deb` staged binary only when `strip` is present in `PATH`, ignoring non-zero exits (`|| true`).
- `[build-linux.sh:dpkg-shlibdeps]` Resolves shared library dependencies via `dpkg-shlibdeps -O` when `dpkg-shlibdeps` is available and emits non-empty output; falls back to `RUNTIME_SHLIBS="libc6, libgcc-s1"` otherwise.
- `[build-linux.sh:DEB_DEPENDS]` Always appends `, libxkbcommon0, libfontconfig1` to `RUNTIME_SHLIBS` for `DEBIAN/control` `Depends` (`Section: utils`, `Priority: optional`, `Maintainer: Thanh Pham <thanhpham110699@gmail.com>`, `Homepage: https://thanhpham99.github.io/apps/termewnal/`), and sets `Recommends: libwayland-client0, libx11-6, libdbus-1-3, libvulkan1`.
- `[build-linux.sh:chmod]` Enforces mode `0755` on staged binaries (`$DEB_STAGE/usr/bin/termewnal`, `$TAR_STAGE/termewnal`) and `install-user.sh`, and mode `0644` on `$DEB_STAGE/usr/share/applications/termewnal.desktop`, staged `.deb` icon PNGs, and `DEBIAN/control` (`$TAR_STAGE/termewnal.desktop` and `install-user.sh`-rewritten `$APP_DIR/termewnal.desktop` retain default umask; `install-user.sh` enforces `0755` on `$BIN_DIR/termewnal` and `0644` on installed icon PNGs).
- `[build-linux.sh:DESKTOP_ENTRY]` Emits `[Desktop Entry]` (`Type=Application`, `Name=Termewnal`, `GenericName=Terminal Emulator`, `Icon=termewnal`, `Terminal=false`, `Categories=System;TerminalEmulator;Network;`, `Keywords=terminal;ssh;sftp;serial;telnet;zmodem;docker;`, `StartupNotify=true`, `StartupWMClass=termewnal`) with `Exec=/usr/bin/termewnal %F` for `.deb` vs `Exec=termewnal %F` in the tarball (`install-user.sh` rewrites via `sed "s|^Exec=termewnal|Exec=${BIN_DIR}/termewnal|"`, assuming `$HOME` contains no unescaped `|`, `&`, or `\`).
- `[build-linux.sh:dpkg-deb]` Requires `cargo`, `sed`, `head`, `du`, `awk`, `dpkg-deb` (with `--root-owner-group`), `tar`, `sha256sum`, `ls`, and POSIX/coreutils (`dirname`, `basename`, `mkdir`, `rm`, `cp`, `chmod`, `cat`, `printf`, plus `install` at `install-user.sh` runtime) in `PATH`.
- `[termewnal.nsi:VIProductVersion]` Formats `"${VERSION}.0"`, requiring `VERSION` to be a 3-component dotted numeric string (`X.Y.Z`) to produce a valid 4-part Windows version.
- `[termewnal.nsi:BIN_PATH]` Default relative paths (`..\target\release\termewnal.exe`, `..\dist\...`, `..\README.md`, `..\assets\termewnal-icon.ico`) assume `makensis` evaluates paths relative to `scripts/` and that `..\dist\` exists when `OUT_FILE` is not overridden.
- `[termewnal.nsi:RequestExecutionLevel]` Enforces per-user execution (`user`), `$LOCALAPPDATA\Programs\Termewnal` (`[termewnal.nsi:InstallDir]`), and `HKCU` registry storage (`[termewnal.nsi:UNINSTALL_KEY]`) with no elevation or `HKLM` writes.
- `[termewnal.nsi:SectionIn RO]` Keeps `SecMain` mandatory; because `!insertmacro MUI_PAGE_COMPONENTS` is absent and `SecDesktop` lacks `/o`, interactive GUI installs cannot deselect `SecDesktop` and create `$DESKTOP\Termewnal.lnk` unconditionally (while relying on `SecMain`'s `SetOutPath "$INSTDIR"`).
- `[termewnal.nsi:Section "Uninstall"]` Uses non-recursive `RMDir` on `$SMPROGRAMS\${APP_NAME}` and `$INSTDIR`, removing directories only when empty.
- `[wt-cargo.sh:set -euo pipefail]` Active prior to `exec cargo "$@"`.
- `[wt-cargo.sh:CARGO_TARGET_DIR]` Preserves any pre-existing non-empty `CARGO_TARGET_DIR` without overwriting it.
- `[wt-cargo.sh:git rev-parse]` Requires `git` (`>= 2.31` for `--path-format=absolute --git-common-dir`), `dirname`, and `cargo` in `PATH`; suppresses stderr (`2>/dev/null`), falls back silently to Cargo's default target directory if `git rev-parse` fails, and in a bare repository resolves `$(dirname "$common_dir")/target` to the bare repo's parent directory.

## Side Effects
- `[build-linux.sh:cd]` Changes working directory to `$ROOT_DIR` (and in subshells to `$SHLIB_TMP` and `$DIST_DIR`).
- `[build-linux.sh:rm]` Deletes existing `$ROOT_DIR/target/pkg/termewnal_${VERSION}_amd64` and `$ROOT_DIR/target/pkg/termewnal-${VERSION}-linux-x86_64`, and deletes `$ROOT_DIR/target/pkg/.shlibdeps` only when `dpkg-shlibdeps` is present in `PATH`.
- `[build-linux.sh:cargo build --release -p termewnal-ui --bin termewnal]` Invokes Cargo release build, writing compilation outputs into `${CARGO_TARGET_DIR:-$ROOT_DIR/target}/release/`.
- `[build-linux.sh:mkdir]` Creates `$ROOT_DIR/dist` and staging directories under `$ROOT_DIR/target/pkg/`.
- `[build-linux.sh:cp]` Stages `.deb` files (`usr/bin/termewnal`, `usr/share/applications/termewnal.desktop`, `usr/share/doc/termewnal/README.md`, `usr/share/icons/hicolor/{32x32,64x64,128x128,256x256,512x512}/apps/termewnal.png`, `DEBIAN/control` with `du -sk` `Installed-Size`) and tarball files (`termewnal`, `README.md`, `icons/termewnal-{32,64,128,256,512}.png`, `termewnal.desktop`, `install-user.sh`).
- `[build-linux.sh:strip]` Optionally strips `$DEB_STAGE/usr/bin/termewnal` in place prior to copying into `TAR_STAGE`.
- `[build-linux.sh:dpkg-shlibdeps]` Optionally creates `$PKG_ROOT/.shlibdeps/debian/control` (`Source: termewnal`), runs `dpkg-shlibdeps -O`, and removes `$PKG_ROOT/.shlibdeps`.
- `[build-linux.sh:dpkg-deb]` Builds `$ROOT_DIR/dist/termewnal_${VERSION}_amd64.deb`.
- `[build-linux.sh:tar]` Writes `$ROOT_DIR/dist/termewnal-${VERSION}-linux-x86_64.tar.gz`.
- `[build-linux.sh:sha256sum]` Writes `$ROOT_DIR/dist/SHA256SUMS-linux.txt` in `$ROOT_DIR/dist`.
- `[build-linux.sh:ls]` Writes progress banners and `ls -lh` output to stdout (errors to stderr).
- `[build-linux.sh:install-user.sh]` When executed by an end user, creates `$HOME/.local/bin`, `$HOME/.local/share/applications`, and `$HOME/.local/share/icons/hicolor/{32,64,128,256,512}x.../apps`, installs `termewnal` (`0755`), rewritten `termewnal.desktop`, and 5 PNG icons (`0644`), and prints `Installed Termewnal to ${BIN_DIR}/termewnal`.
- `[termewnal.nsi:OutFile]` At `makensis` compile time, packages `BIN_PATH`, `..\README.md`, and `..\assets\termewnal-icon.ico` into `${OUT_FILE}`.
- `[termewnal.nsi:Section "Termewnal (required)" SecMain]` At install time, writes `$INSTDIR\termewnal.exe`, `$INSTDIR\README.md`, and `$INSTDIR\Uninstall.exe`; creates `$SMPROGRAMS\Termewnal\` containing `Termewnal.lnk` and `Uninstall Termewnal.lnk`; writes 7 `REG_SZ` values (`DisplayName`, `DisplayVersion`, `Publisher`, `URLInfoAbout`, `DisplayIcon`, `InstallLocation`, `UninstallString`) and 2 `REG_DWORD` values (`NoModify=1`, `NoRepair=1`) under `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Termewnal`.
- `[termewnal.nsi:MUI_FINISHPAGE_RUN]` Allows launching `"$INSTDIR\termewnal.exe"` from the installer finish page.
- `[termewnal.nsi:Section "Desktop Shortcut" SecDesktop]` At install time, creates `$DESKTOP\Termewnal.lnk` unconditionally in interactive GUI installs (unless skipped programmatically).
- `[termewnal.nsi:Section "Uninstall"]` At uninstall time, deletes `$DESKTOP\Termewnal.lnk`, `$SMPROGRAMS\Termewnal\Termewnal.lnk`, `$SMPROGRAMS\Termewnal\Uninstall Termewnal.lnk`, `$SMPROGRAMS\Termewnal`, `$INSTDIR\termewnal.exe`, `$INSTDIR\README.md`, `$INSTDIR\Uninstall.exe`, `$INSTDIR`, and registry key `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Termewnal`.
- `[wt-cargo.sh:git rev-parse]` Spawns `git rev-parse --path-format=absolute --git-common-dir` when `CARGO_TARGET_DIR` is unset or empty.
- `[wt-cargo.sh:CARGO_TARGET_DIR]` Exports `CARGO_TARGET_DIR` in the wrapper environment when `CARGO_TARGET_DIR` was initially unset/empty and `git rev-parse` succeeds.
- `[wt-cargo.sh:exec cargo "$@"]` Replaces the Bash process with `cargo "$@"`, delegating all Cargo filesystem, network, and subprocess side effects.

## Verification
- Run: `cargo test --workspace`
- `.github/workflows/release.yml` runs inside an `ubuntu:22.04` container, resolves `VERSION` in order `inputs.version` -> `GITHUB_REF` (`refs/tags/v*` stripped) -> `Cargo.toml`, runs `cargo check --workspace --all-targets`, `cargo test --workspace`, `./scripts/build-linux.sh`, and a `readelf --dyn-syms -W target/release/termewnal` check failing on `GLIBC > 2.35`, then uploads the `.deb` and `.tar.gz` artifacts (`publish-release` generates `SHA256SUMS.txt` rather than uploading `dist/SHA256SUMS-linux.txt`). Neither `termewnal.nsi`, `wt-cargo.sh`, nor `install-user.sh` has an automated test target in `scripts/` or `.github/workflows/release.yml`.
