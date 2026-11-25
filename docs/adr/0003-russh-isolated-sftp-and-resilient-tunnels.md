# Russh with Isolated SFTP Transport and Listener-Preserving Tunnels

Status: accepted

We will implement SSH, SFTP, and port forwarding in-process on `russh` + `russh-sftp` (`tokio`), isolate bulk SFTP transfers onto a dedicated Transfer Transport (falling back to a multiplexed channel only when one-time interactive auth prevents silent secondary connections), and preserve local tunnel listeners across transient SSH drops.

## Considered Options

- **`russh` + isolated Transfer Transport + listener preservation**: Eliminates C-library (`libssh2`) mutex contention and cross-compile friction, prevents bulk pipelined SFTP uploads from starving interactive terminal input or keepalives, avoids `EADDRINUSE` on local tunnel reconnects, and allows backoff retries for `RequestDenied` on remote forwards.
- **Single shared SSH transport (Tabby's model)**: Simpler connection setup, but large SFTP transfers contend with terminal traffic and keepalives on the same TCP stream, causing freezes and cascading disconnects.
- **`ssh2` (`libssh2`) or wrapping external `openssh` CLI**: `ssh2` suffers from synchronous locking across concurrent channels and C/OpenSSL build friction on Windows; wrapping external `ssh` loses fine-grained in-process control over pipelined SFTP queues and tunnel lifecycle states.
