# 11: Resilient Local & Dynamic SOCKS5 Tunnels (Session & Standalone)

**What to build:** Implement Local (`-L`) and Dynamic SOCKS5 (`-D`) `Tunnel`s running either bound to a `Session` or Standalone in the background, keeping local `TcpListener`s alive across `Transport` drops (`127.0.0.1` default bind, explicit opt-in + warning for `0.0.0.0`) with a 10-second Hold-until-reconnected grace window and 1-click Sidebar/Status Bar controls.

**Blocked by:** 04: Command Palette, Collapsible Sidebar & Status Bar, 05: Single-Instance IPC, Multi-Window, Workspace Restore & Close Guard, 09: Connection Supervisor (Keepalive, Wake Probe, Backoff & Mux Recovery)

**Status:** ready-for-agent

- [ ] Users can define and toggle Local (`-L`) and Dynamic SOCKS5 (`-D`) `Tunnel`s attached to a `Profile`/`Session` or running Standalone in the background independently of open terminal tabs.
- [ ] Local listeners bind `127.0.0.1` by default and require explicit opt-in accompanied by a clear security warning before binding `0.0.0.0`.
- [ ] During transient `Transport` disconnects and reconnects, the `Tunnel` keeps its local `TcpListener` bound (preventing `EADDRINUSE`) and holds incoming local client sockets up to a configurable 10-second grace timeout before resetting cleanly if recovery fails or is cancelled.
- [ ] Active Standalone `Tunnel`s report to the Close Guard from ticket 05 and expose live state and 1-click toggle/reconnect controls in the Status Bar and Left Sidebar.
- [ ] In-process `russh` integration tests verify Local and SOCKS5 traffic forwarding, zero `EADDRINUSE` across simulated disconnects, and seamless hold-and-flush for a local TCP client connecting mid-reconnect.
