# 06: Secrets Vault & Safe TOML Hot-Reload

**What to build:** Implement `Vault` for strict secret isolation—defaulting to OS Keyring with an optional Argon2id + AES-256-GCM Master-Password encrypted local `Vault` (unlock once per process session, 30-minute idle/sleep auto-lock, manual lock, `zeroize` in RAM)—alongside safe external TOML hot-reload that retains active config on parse errors and offers 1-click `.bak.1` recovery.

**Blocked by:** 01: Local Shell App & Atomic TOML Config

**Status:** resolved

- [x] `Vault` stores passwords and key passphrases via OS Keyring (`Secret Service` / `Keychain` / `Credential Manager`) behind a trait boundary with an optional local encrypted `Vault` using Argon2id + AES-256-GCM.
- [x] Local encrypted `Vault` unlocks once per running process session, supports manual locking, auto-locks after configurable idle/sleep timeout (default 30 minutes), and zeroizes decrypted key material in memory on lock/exit.
- [x] Saving edits to `config.toml` or `profiles.toml` externally triggers safe hot-reload without interrupting running `Session`s.
- [x] Malformed external TOML edits keep the last valid in-memory configuration active, display an error toast citing the parse failure line/message, offer 1-click restore from `.bak.1`, and never overwrite the user's edited file.
- [x] Integration tests using a temp XDG dir and an in-memory keyring double verify `Vault` encryption/auto-lock/zeroize and hot-reload retention across invalid TOML edits.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `9260653`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
