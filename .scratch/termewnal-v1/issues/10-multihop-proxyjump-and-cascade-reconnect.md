# 10: Multi-Hop ProxyJump & Cascade Chain Reconnect

**What to build:** Support multi-hop `ProxyJump` chains (`A -> B -> C`) defined on `Profile`s or parsed from `~/.ssh/config`, integrated with Connection Supervisor so an intermediate hop failure tears down dependent inner `Transport`s cleanly and rebuilds the entire jump chain in order.

**Blocked by:** 08: Read-Only ~/.ssh/config & Tabby config.yaml Importer, 09: Connection Supervisor (Keepalive, Wake Probe, Backoff & Mux Recovery)

**Status:** ready-for-agent

- [ ] `Profile`s (including cloned/parsed `~/.ssh/config` hosts) support ordered multi-hop `ProxyJump` chains over nested `russh` `direct-tcpip` streams (`connect_stream`).
- [ ] Each hop authenticates using its own `Profile`/OpenSSH credentials (`Vault`, SSH Agent, or interactive prompt) and enforces strict `known_hosts` verification per hop.
- [ ] When any upstream hop in an active chain disconnects or fails keepalive, Connection Supervisor tears down all dependent inner hops cleanly before rebuilding the chain from the outermost hop inward (`A -> B -> C`).
- [ ] Multi-hop in-process `russh` chain tests verify initial end-to-end connection and clean cascade recovery when the bastion hop drops mid-session.
