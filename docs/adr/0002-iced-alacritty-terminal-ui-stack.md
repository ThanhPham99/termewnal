# Iced, alacritty_terminal, and portable-pty UI Stack

Status: accepted

We will build Termewnal's desktop frontend and terminal emulation layer on `iced` (`iced_wgpu`) paired with `alacritty_terminal` and `portable-pty`, rather than `gpui` or `egui`.

## Considered Options

- **`iced` + `alacritty_terminal` + `portable-pty`**: Provides mature cross-platform windowing and GPU backends across Linux (X11/Wayland), macOS (Metal), and Windows (DX12/Vulkan), with an Elm-inspired reactive architecture suited for complex state (transfer queues, tunnel drawers, split panes) and proven integration with `alacritty_terminal`.
- **`gpui`**: Offers exceptional text-layout performance on macOS and Linux, but standalone Windows support still has stability and rendering gaps, API churn is high, and terminal grid widgets require building from scratch.
- **`egui`**: Fast to prototype for developer tools, but immediate-mode layout and typography make achieving a polished Tabby-grade multi-pane desktop workspace significantly harder.
