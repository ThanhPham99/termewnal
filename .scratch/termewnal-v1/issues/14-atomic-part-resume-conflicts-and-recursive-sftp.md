# 14: Atomic .part Upload, Byte-Offset Resume, Conflicts & Recursive Directories

**What to build:** Harden SFTP `Transfer`s against disconnects and batch edge cases with `.termewnal.part` atomic uploads + exact byte-offset resume for files `>1 MB` (falling back cleanly to direct writes when `.part`/rename is denied), a remote conflict dialog (`Overwrite` / `Resume` / `Skip` / `Rename` + `Apply to all` + configurable default), and recursive directory transfers preserving permissions while safely skipping symlink loops.

**Blocked by:** 09: Connection Supervisor (Keepalive, Wake Probe, Backoff & Mux Recovery), 13: Non-Blocking SFTP Browser, Isolated Transfer Transport & Pipelined Transfers

**Status:** resolved

- [x] Uploads `>1 MB` write to `<filename>.termewnal.part` and atomically rename on completion; on `Transport` drop and reconnect, transfers resume from the verified remote byte offset without re-uploading completed bytes.
- [x] When a remote server or directory permissions deny `.part` creation or atomic rename, the `Transfer` automatically falls back to direct file writes with a non-blocking status notice.
- [x] Colliding target files trigger a conflict resolution dialog offering `Overwrite`, `Resume`, `Skip`, and `Rename` with an `Apply to all` checkbox and a configurable default policy in `config.toml`.
- [x] Recursive directory uploads and downloads preserve POSIX permissions (`chmod`) where supported and detect/skip symlink loops safely by default (configurable).
- [x] In-process `russh-sftp` fault-injection tests verify mid-upload TCP drop + byte-offset `.part` resume, `.part`-rename-denied fallback, conflict policies, and symlink-loop protection.

## Comments

- Resolved on integration branch `feat/termewnal-v1` (merged at `b8f779d`, post-review fixes at `66ecbb3`, verified with `cargo check --workspace --all-targets` and `cargo test --workspace` — 154 passed, 0 failed, 0 ignored).
