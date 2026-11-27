# termewnal-protocols

## Overview
Native non-SSH protocol crate for Termewnal (`Cargo.toml` package `termewnal-protocols`), implementing container discovery/exec builders (`docker`), supervised serial port sessions (`serial`), async TCP Telnet sessions with IAC negotiation and login scripts (`telnet`), and a sans-IO Zmodem transfer state machine (`zmodem`). Depends on `termewnal-core`, `termewnal-pty`, `serial2`, `thiserror`, `tokio`, and `tracing` (`tempfile` for dev-dependencies), verified via `cargo test -p termewnal-protocols`.

## Subdirectories & Boundaries
- `src`: [src/README.md](./src/README.md) - Component package containing the full protocol implementations (`lib.rs`, `error.rs`, `docker.rs`, `serial.rs`, `telnet.rs`, `zmodem.rs`). Exposes `ProtocolStatus`, `ProtocolError`, `DockerDiscovery`/`build_exec_shell_spec`, `SerialSession`/`SerialConnectConfig`/`SerialReadlineBuffer`/`HexdumpFormatter`, `TelnetSession`/`TelnetConnectConfig`/`TelnetParser`, and `ZmodemEngine`/`ZmodemHeader`/path-sanitization helpers, along with their actor concurrency, protocol framing, input/path validation, and I/O side-effect boundaries (OS subprocesses, serial devices, TCP sockets, Tokio tasks, `TerminalBuffer` mutations, and download/upload filesystem access).
