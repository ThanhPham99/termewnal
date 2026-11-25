# 09: Connection Supervisor (Keepalive, Wake Probe, Backoff & Mux Recovery)

**What to build:** Build the Connection Supervisor in `termewnal-ssh` to actively monitor `Transport` health via TCP + SSH keepalives (`15s × 3` configurable globally and per `Profile`) and clock-jump/wake `3s` fast-probes, drive non-modal exponential backoff + jitter reconnects (`1s..30s`) with live Status Bar latency/controls, preserve scrollback across drops with a `--- Reconnected ---` divider, and optionally auto-attach `tmux`/`screen`.

**Blocked by:** 04: Command Palette, Collapsible Sidebar & Status Bar, 07: SSH Session, Auth Methods & Strict known_hosts TOFU

**Status:** ready-for-agent

- [ ] `Transport` lifecycle transitions cleanly through `Disconnected -> Connecting -> Connected -> Degraded -> Reconnecting -> Disconnected`, publishing real-time status and keepalive RTT latency to the Status Bar.
- [ ] TCP and SSH keepalives default to 15-second intervals with 3 missed probes (~45s timeout), configurable globally and per `Profile`; system clock-jump detection or a manual `Probe / Reconnect All` action dispatches an immediate 3-second probe.
- [ ] On drop, `Session`s remain open with full scrollback intact, show a non-modal Status Bar countdown (`1s..30s` exponential backoff + jitter) with `Reconnect now` and `Cancel` buttons, and insert `--- Reconnected at HH:MM:SS ---` upon recovery.
- [ ] Upon reconnection, `Session`s open a fresh remote shell or automatically invoke the `Profile`'s configured `tmux`/`screen` attach command when enabled.
- [ ] In-process `russh` server fault-injection tests verify keepalive timeout detection, simulated clock-jump fast-probe recovery, backoff cancellation, scrollback preservation, and `tmux` attach command execution.
