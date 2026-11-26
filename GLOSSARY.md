# Termewnal

A native Rust terminal emulator and multi-protocol workspace designed for resilient connections, reliable file transfers, and streamlined UX.

## Language

**Profile**:
A saved connection definition specifying the target (local shell, SSH host, Serial port, Telnet host, or Docker container), authentication settings, login scripts, and attached tunnel rules.
_Avoid_: Server config, host entry, bookmark

**Session**:
An active runtime workspace opened from a Profile or quick-connect, owning a terminal view, connection lifecycle state, and associated channels.
_Avoid_: Tab, connection, window

**Pane**:
A rectangular region within a workspace tab that hosts one **Session** and, for SSH **Session**s, an optional **SFTP Panel** docked on its right side.
_Avoid_: Split frame, terminal box

**SFTP Panel**:
A remote-only SFTP file browser and **Transfer** queue view (labeled `SFTP+` in the UI) docked to the right of an SSH **Session**'s **Pane**.
_Avoid_: SFTP drawer, SFTP tab, dual-pane commander

**Tunnel**:
A port forwarding rule (`Local`, `Remote`, or `Dynamic` SOCKS5) and its managed lifecycle bound to an SSH connection.
_Avoid_: Forward rule, port forward, proxy rule

**Transfer**:
An individual SFTP file or recursive directory upload/download unit tracked in the transfer queue, or a Zmodem file transfer stream.
_Avoid_: Upload job, file task, copy

**Transport**:
An active connection to a target host or device (direct, via `ProxyJump`, via `ProxyCommand`, TCP Telnet, or Serial device) whose keepalive and reconnection lifecycle are managed by the supervisor; a Session owns a primary Transport, while bulk SFTP Transfers may use a separate Transfer Transport.
_Avoid_: Socket, SSH pipe, channel

**Vault**:
An encrypted secret store holding credentials, private-key passphrases, and sync tokens, kept strictly separate from Profile configuration files.
_Avoid_: Keystore, secret config, credentials file

**Quick Command**:
A named reusable command snippet or macro persisted in `quick_commands.toml` that can be dispatched to the focused pane or broadcast across all panes in the active tab.
_Avoid_: Macro button, shortcut command

**Workspace Template**:
A named multi-tab, split-pane layout snapshot saved in `workspaces.toml` that users can launch or switch to on demand.
_Avoid_: Saved session bundle, layout preset

**Login Script**:
An ordered list of expect/send automation steps executed after an interactive SSH, Telnet, or Serial Session connects.
_Avoid_: Auto-typer, expect macro

**Highlight Rule**:
A regex pattern and ANSI/RGB foreground-background style applied dynamically when rendering matching terminal output.
_Avoid_: Color filter, output rule
