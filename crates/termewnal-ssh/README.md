# termewnal-ssh

## Overview

The `termewnal-ssh` crate provides Termewnal's SSH transport, authentication, strict `known_hosts` verification, proxy traversal (`ProxyCommand`, `ProxyJump`, SOCKS5/HTTP `CONNECT`), X11/agent forwarding, supervised interactive PTY sessions, server telemetry parsing, local/remote/dynamic port-forwarding tunnels, and pooled SFTP browsing plus resilient file/directory transfer queues. Its crate manifest ([Cargo.toml](./Cargo.toml)) declares package `termewnal-ssh` depending on `chrono`, `data-encoding`, `directories`, `hmac`, `rand`, `russh`, `russh-sftp`, `sha1`, `socket2`, `termewnal-core`, `termewnal-pty`, `thiserror`, `tokio`, `tracing`, and `zeroize` (with `futures` and `tempfile` in `[dev-dependencies]`; verified via `cargo test -p termewnal-ssh`).

## Subdirectories & Boundaries

- src: [./src/README.md](./src/README.md) - Core SSH client implementation (`SshTransport`, `SshConnectConfig`, `SshClientHandler`, `SshError`, `TransportStatus`), authentication & agent/X11 integration (`authenticate_session`, `SshInteractionHandler`, `OsAgentConnector`, `x11`), `known_hosts` TOFU/MITM verification, proxy connectors (`proxy_cmd`, `proxy_jump`, `proxy_net`), interactive session & keepalive/clock-jump supervisor (`SshSession`, `ConnectionSupervisor`, `ServerStatsCollector`), port-forwarding runtime (`TunnelManager`), and pooled SFTP/resumable transfer engine (`TransferTransportPool`, `SftpManager`, `TransferQueue`, `DirectoryTransferSummary`).
