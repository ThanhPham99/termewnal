# 07: SSH Session, Auth Methods & Strict known_hosts TOFU

**What to build:** Connect Saved `Profile`s and Quick Connect targets to live SSH `Session`s over `russh` (`Primary Transport`), supporting private keys (`ed25519`, `rsa`, `ecdsa` + `Vault` passphrases), SSH Agent, password, and keyboard-interactive 2FA/OTP UI prompts, enforcing strict `~/.ssh/known_hosts` TOFU/MITM verification, and wiring SSH `Session` restart states (`Disconnected` / `auto_connect_on_startup`).

**Blocked by:** 06: Secrets Vault & Safe TOML Hot-Reload

**Status:** resolved

- [x] `termewnal-ssh` establishes interactive PTY-backed SSH `Session`s over `russh` and streams terminal I/O and window-size events into `alacritty_terminal`.
- [x] Authentication supports `ed25519`/`rsa`/`ecdsa` private keys (with `Vault`-backed passphrases), OS SSH Agent (`SSH_AUTH_SOCK`, Windows OpenSSH named pipe, Pageant), `Vault`-backed passwords, and interactive 2FA/OTP prompts.
- [x] Host key verification parses and updates `~/.ssh/known_hosts`, displays an interactive TOFU prompt with SHA256 fingerprint for unknown hosts, and hard-blocks connections with an explicit MITM alert when a recorded key changes.
- [x] Saved `Profile`s support an `auto_connect_on_startup` flag; restored SSH `Session`s stay `Disconnected` until user action (Enter/click) unless that flag is enabled.
- [x] Integration tests against an in-process `russh` test server verify all auth methods, unknown-host TOFU acceptance/rejection, and key-mismatch blocking.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `a8804fc`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
