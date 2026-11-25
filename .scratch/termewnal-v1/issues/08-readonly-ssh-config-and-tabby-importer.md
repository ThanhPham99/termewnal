# 08: Read-Only ~/.ssh/config & Tabby config.yaml Importer

**What to build:** Surface OpenSSH hosts parsed from `~/.ssh/config` in read-only mode inside the Left Sidebar with 1-click "Clone to Profile", and build a 1-click Tabby `config.yaml` migration importer converting `Profile`s, groups/tags, and `Tunnel` definitions into `profiles.toml`.

**Blocked by:** 04: Command Palette, Collapsible Sidebar & Status Bar, 07: SSH Session, Auth Methods & Strict known_hosts TOFU

**Status:** resolved

- [x] `termewnal-core` parses `~/.ssh/config` (hosts, user, port, identity files, `ProxyJump` directives) strictly read-only without ever mutating `~/.ssh/config`.
- [x] Parsed System SSH `Profile`s appear in their own Left Sidebar section, can launch SSH `Session`s directly, and expose a 1-click `Clone to Profile` action saving an editable `Profile` to `profiles.toml`.
- [x] The Tabby `config.yaml` importer migrates SSH/local `Profile`s, group/tag organization, and `Tunnel` rules into `profiles.toml`, ignores Tabby plugin blocks, and prompts cleanly when Tabby's encrypted vault holds locked credentials.
- [x] Automated tests verify `~/.ssh/config` parsing/cloning and Tabby `config.yaml` migration edge cases against temp fixtures.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `25fbbb0`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
