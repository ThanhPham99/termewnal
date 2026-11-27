# termewnal-core

## Overview
Crate root for `termewnal-core` (declared in `./Cargo.toml` with workspace dependencies covering `serde`/`toml`/`noyalib`/`serde_json` serialization, `aes-gcm`/`argon2`/`zeroize`/`rand`/`base64` cryptography & encoding, `keyring`/`keyring-core` credential storage, `notify`/`notify-debouncer-mini`/`tokio` config watching, `reqwest` sync transport, `ssh2-config` OpenSSH parsing, `directories`, `tempfile`, `thiserror`, and `uuid`). It groups Termewnal's core domain schemas, XDG path resolution, atomic TOML persistence, encrypted secret vault, config hot-reload, remote sync, keybinding resolution, workspace layout, SFTP state, OpenSSH/Tabby importers, and built-in themes under `src/`. Crate verification runs via `cargo test -p termewnal-core`.

## Subdirectories & Boundaries
- `src`: [./src/README.md](./src/README.md) - Component package housing all 15 `termewnal-core` modules (`lib.rs`, `error.rs`, `collections.rs`, `config.rs`, `hot_reload.rs`, `keybindings.rs`, `layout.rs`, `openssh_config.rs`, `profile.rs`, `sftp_state.rs`, private `storage.rs`, `sync.rs`, `tabby_importer.rs`, `theme.rs`, and `vault.rs`), enforcing schema-versioned atomic persistence with 3-slot `.bak` rotation, credential isolation via `SecretStore`/`EncryptedVault`, hot-reload snapshot recovery, and encrypted/plaintext config sync.
