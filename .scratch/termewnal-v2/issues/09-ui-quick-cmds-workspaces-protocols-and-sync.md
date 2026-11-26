# 09: UI Quick Commands, Named Workspaces, Protocol Sessions, Profile Tunnels/SSH UI & Config Sync

**What to build:** Integrate `QuickCommand` panel/palette dispatch (focused or tab-broadcast), `WorkspaceTemplate` manager, `Serial`/`Telnet`/`Docker` session runtime & launch UI, Zmodem transfer progress/send picker, `ProfileEditorState` editing for tunnels/keepalive/forwarding/login scripts/serial/telnet/docker, and Settings UI tabs for Keybindings, Highlight, and Config Sync.

**Blocked by:** 03, 04, 05, 06, 07, 08

**Status:** resolved

- [x] Quick Commands drawer/modal and Command Palette entries create, edit, delete, and execute snippets to focused pane or broadcast across active tab panes.
- [x] Named Workspace Manager saves current tabs/splits as reusable `WorkspaceTemplate`s and restores or deletes them.
- [x] Sidebar, Command Palette, Quick Connect, and Profile Editor support `Serial` (port discovery + hex/readline controls), `Telnet`, and `Docker` container exec sessions, plus SSH `ForwardAgent`, `ForwardX11`, `ProxyCommand`, `LoginScript`, per-profile keepalive, and attached `TunnelRule` editing.
- [x] Zmodem auto-receive (`sz`) and send picker (`rz`) integrate into session stream handling and Status Bar notices.
- [x] Settings modal exposes Color Schemes (25+), Appearance (opacity, cursor blink, tab position, right-click mode), Keybindings, Highlight Rules, and Config Sync (Gist/WebDAV/Local).
- [x] UI integration tests verify Quick Commands, Named Workspaces, Serial/Telnet/Docker session flows, Profile Editor extensions, and Sync UI state.
