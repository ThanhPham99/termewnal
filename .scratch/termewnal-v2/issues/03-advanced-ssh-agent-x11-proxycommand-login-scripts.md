# 03: Advanced SSH (Agent Forwarding, X11 Forwarding, ProxyCommand & Login Scripts)

**What to build:** Extend `termewnal-ssh` to support SSH Agent Forwarding (`ForwardAgent`), X11 Forwarding (`ForwardX11`), `ProxyCommand` external transport piping, and `LoginScript` expect/send automation on connect and reconnect.

**Blocked by:** 01

**Status:** resolved

- [x] `AgentConnector` supports opening raw agent streams so `SshClientHandler::server_channel_open_agent_forward` bridges remote `auth-agent@openssh.com` requests bidirectionally to the OS/test SSH agent when `forward_agent` is enabled.
- [x] `SshSession` requests `x11-req` when `forward_x11` is enabled and `SshClientHandler::server_channel_open_x11` bridges remote X11 channels to the local X11 display socket/TCP endpoint.
- [x] `SshConnectConfig::proxy_command` spawns a local proxy process (`%h`, `%p`, `%r` expanded) and pipes stdio directly into `russh::client::connect_stream`.
- [x] `LoginScriptRunner` watches incoming PTY output against configured `expect` patterns and sends scripted responses (with optional Vault secret expansion and delays) on session open and reconnect.
- [x] In-process `russh` integration tests verify agent forwarding round-trip, X11 channel forwarding, `ProxyCommand` transport connection, and login script automation.

## Comments

- Verified via `cargo test -p termewnal-ssh`: 4/4 tests in `v2_ssh_forwarding_and_scripts` plus all 51 integration tests and 1 unit test passed with 0 failures (alongside 15 `termewnal-pty` tests).
