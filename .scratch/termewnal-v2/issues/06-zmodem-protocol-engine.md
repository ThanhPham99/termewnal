# 06: Pure-Rust Zmodem (`rz`/`sz`) Transfer Engine (`termewnal-protocols`)

**What to build:** Implement a pure-Rust Zmodem protocol state machine and stream detector (`ZmodemEngine`) inside `termewnal-protocols` capable of detecting `rz`/`sz` start sequences on any session byte stream and executing CRC-16/CRC-32 file sends and receives cleanly.

**Blocked by:** 04

**Status:** resolved

- [x] Zmodem header encoder/decoder supports `ZPAD`, `ZBIN`, `ZBIN32`, `ZHEX`, `ZDLE` escaping/unescaping, and CRC-16 / CRC-32 verification.
- [x] Stream detector recognizes remote `sz` (`ZRQINIT` `**\x18B00...`) and `rz` prompts without corrupting normal terminal rendering once active.
- [x] `ZmodemSession` handles receiving files (`ZFILE` + `ZDATA` + `ZEOF` + `ZFIN`) into the download directory with progress events and conflict-safe naming, and sending local files upon user selection.
- [x] Unit and loopback stream tests verify full Zmodem send and receive transfers end-to-end between two `ZmodemEngine` peers.

## Comments

- Verified via `cargo test -p termewnal-protocols`: all 9 integration tests (including CRC-16/CRC-32, path sanitization, conflict renaming, abort handling, and multi-file fragmented peer-to-peer Zmodem transfer) passed cleanly in 0.10s.
