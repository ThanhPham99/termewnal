# Termewnal

A native Rust terminal emulator and SSH/SFTP workspace designed for resilient connections, reliable file transfers, and streamlined UX.

## Language

**Profile**:
A saved connection definition specifying the target (local shell or SSH host), authentication settings, and attached tunnel rules.
_Avoid_: Server config, host entry, bookmark

**Session**:
An active runtime workspace opened from a Profile, owning a terminal view, connection lifecycle state, and associated channels.
_Avoid_: Tab, connection, window

**Tunnel**:
A port forwarding rule (`Local`, `Remote`, or `Dynamic` SOCKS5) and its managed lifecycle bound to an SSH connection.
_Avoid_: Forward rule, port forward, proxy rule

**Transfer**:
An individual SFTP file or recursive directory upload/download unit tracked in the transfer queue.
_Avoid_: Upload job, file task, copy

**Transport**:
An active network connection to a target host (direct or via jump hosts) whose keepalive and reconnection lifecycle are managed by the supervisor; a Session owns a primary Transport, while bulk Transfers may use a separate Transfer Transport.
_Avoid_: Socket, SSH pipe, channel

**Vault**:
An encrypted secret store holding credentials and private-key passphrases, kept strictly separate from Profile configuration files.
_Avoid_: Keystore, secret config, credentials file
