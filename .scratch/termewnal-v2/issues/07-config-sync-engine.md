# 07: Cloud & Local Config Sync Engine (`termewnal-core`)

**What to build:** Implement `ConfigSyncService` in `termewnal-core` supporting pull/push synchronization of `config.toml`, `profiles.toml`, `quick_commands.toml`, and `workspaces.toml` across GitHub Gist, WebDAV, and local directory sync targets, with optional Argon2id/AES-256-GCM payload encryption and `Vault`-isolated auth tokens.

**Blocked by:** 01

**Status:** resolved

- [x] `SyncBundle` packages non-secret configuration files (`config.toml`, `profiles.toml`, `quick_commands.toml`, `workspaces.toml`) with version metadata and timestamps, and optionally encrypts the bundle using Termewnal's Argon2id + AES-256-GCM envelope.
- [x] Sync providers support `LocalDirectory`, `WebDav`, and `GitHubGist` behind an injectable HTTP/transport boundary (`reqwest` production adapter).
- [x] Applying a pulled bundle writes atomically with `.bak` backups and never leaks or overwrites `Vault` secret entries.
- [x] Integration tests verify GitHub Gist and WebDAV sync against an in-process HTTP test server, encrypted bundle round-trip, and local bundle sync.

## Comments

- Verified via `cargo test -p termewnal-core --offline`: 33 integration tests (including encrypted envelope tampering/wrong-passphrase tests and loopback HTTP tests for WebDAV and GitHub Gist via `ReqwestSyncHttpClient`) plus 2 unit tests passed cleanly in 0.17s.
