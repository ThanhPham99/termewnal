# Atomic TOML Config, Read-Only OpenSSH Import, and Dual Vault

Status: accepted

We will store non-secret configuration (`config.toml`, `profiles.toml`) in XDG-standard TOML files with atomic writes and three rotating backups, treat `~/.ssh/config` as strictly read-only with one-click cloning into Termewnal Profiles, and isolate secrets in OS Keyring with an optional Argon2id/AES-256-GCM local encrypted Vault.

## Considered Options

- **Separated TOML + read-only `~/.ssh/config` + Keyring/Vault**: Prevents config corruption on crashes or concurrent edits, never mutates the user's existing OpenSSH config, and avoids forcing a master password on desktop OSes while still supporting headless or portable Vault workflows.
- **Single YAML config + embedded vault (Tabby's model)**: Risks corrupting profiles and secrets together on failed writes and mixes shareable configuration with sensitive credentials.
- **Bidirectional read/write sync with `~/.ssh/config`**: Cannot cleanly store Termewnal-specific Profile metadata (groups, colors, reconnect policies, multiplexer auto-attach) and risks breaking user-maintained OpenSSH configurations.
