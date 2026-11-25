# 12: Resilient Remote Tunnels (-R) & RequestDenied Backoff

**What to build:** Add Remote (`-R`) `Tunnel`s (`tcpip_forward`) to the `Tunnel` manager with clean `cancel_tcpip_forward` teardown on manual stop/exit and automatic exponential backoff retries when encountering `RequestDenied` during recovery after ungraceful network drops.

**Blocked by:** 11: Resilient Local & Dynamic SOCKS5 Tunnels (Session & Standalone)

**Status:** resolved

- [x] Users can configure and toggle Remote (`-R`) `Tunnel`s (both `Session`-bound and Standalone) forwarding remote `forwarded-tcpip` streams to local targets.
- [x] Stopping a Remote `Tunnel` manually or exiting Termewnal cleanly awaits `cancel_tcpip_forward` before tearing down the `Transport`.
- [x] When `tcpip_forward` fails with `RequestDenied` immediately after an ungraceful disconnect, the `Tunnel` transitions into a retrying state with exponential backoff and surfaces retry status in the UI rather than failing permanently on the first attempt.
- [x] Integration tests against an in-process `russh` server simulating a stale remote port binding verify `cancel_tcpip_forward` on clean stop and eventual recovery once the mock server releases the port after initial `RequestDenied` responses.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `5533f5c`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
