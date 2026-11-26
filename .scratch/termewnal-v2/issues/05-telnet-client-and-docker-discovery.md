# 05: Telnet Client & Docker/Podman Container Discovery (`termewnal-protocols`)

**What to build:** Implement an async pure-Rust `TelnetSession` (TCP + NVT/IAC option negotiation for `NAWS`, `TTYPE`, `ECHO`, `SGA`, and auto-reconnect) and `DockerDiscovery` (`docker` / `podman` container listing and exec `ShellSpec` builder) inside `termewnal-protocols`.

**Blocked by:** 02, 04

**Status:** resolved

- [x] `TelnetSession` connects over `tokio::net::TcpStream`, parses IAC commands/subnegotiations (`WILL`/`WONT`/`DO`/`DONT`/`SB`/`SE`), negotiates `NAWS` on resize, `TTYPE`, `ECHO`, and `SGA`, and streams cleaned payloads into `TerminalBuffer`.
- [x] `TelnetSession` supports configurable auto-reconnect while retaining scrollback.
- [x] `DockerDiscovery` parses running Docker and Podman containers via an injectable command runner and generates `ShellSpec` commands for interactive container attachment.
- [x] Integration tests verify IAC negotiation + resize `NAWS` against a real loopback Rust Telnet test server and Docker/Podman discovery parsing.

## Comments

- Verified via `cargo test -p termewnal-protocols`: all 7 tests (`serial` + `telnet_and_docker`) passed cleanly in 0.10s with 0 warnings.
