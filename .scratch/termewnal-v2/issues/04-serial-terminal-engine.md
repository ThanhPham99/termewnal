# 04: Serial Terminal Engine (`termewnal-protocols`)

**What to build:** Create the `crates/termewnal-protocols` workspace crate and implement `SerialSession` backed by `serial2` (with injectable transport trait for deterministic tests), port discovery, RS-232 parameter configuration, `Text`/`Readline`/`HexBytes` input modes, `Text`/`Hexdump` output rendering, newline conversion, and automatic reconnection.

**Blocked by:** 01, 02

**Status:** resolved

- [x] `crates/termewnal-protocols` is added to `[workspace].members`.
- [x] `SerialSession` discovers available serial ports via `serial2` and opens ports with baud rate, data bits, parity, stop bits, and flow control.
- [x] Input formatter supports raw byte-by-byte, local-echo readline editing, and hex byte parsing (`AA 55 0D`).
- [x] Output formatter supports standard terminal text, `LF`/`CRLF`/`CR` newline conversion, and live ASCII+hex hexdump view into `TerminalBuffer`.
- [x] Auto-reconnect monitor retries disconnected serial ports cleanly and preserves `TerminalBuffer` scrollback.
- [x] Tests verify readline/hex/newline conversion, hexdump formatting, and disconnect/reconnect recovery against a virtual in-memory serial transport.

## Comments

- Verified via `cargo test -p termewnal-protocols`: 4/4 integration tests in `serial` passed cleanly with 0 warnings in 0.10s.
