//! `ProxyCommand` child process stdio stream adapter for SSH transport tunneling.

use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::error::SshError;

/// Expands OpenSSH `ProxyCommand` tokens (`%h` host, `%p` port, `%r` user, `%%` literal `%`).
pub fn expand_proxy_command(template: &str, host: &str, port: u16, username: &str) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.peek().copied() {
                Some('h') => {
                    chars.next();
                    out.push_str(host);
                }
                Some('p') => {
                    chars.next();
                    out.push_str(&port.to_string());
                }
                Some('r') => {
                    chars.next();
                    out.push_str(username);
                }
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                _ => out.push('%'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Bidirectional `AsyncRead + AsyncWrite` wrapper around a spawned `ProxyCommand` child process.
pub struct ProxyCommandStream {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl ProxyCommandStream {
    /// Spawns `template` (expanded for `host`, `port`, `username`) and connects its piped stdio.
    pub fn spawn(
        template: &str,
        host: &str,
        port: u16,
        username: &str,
    ) -> Result<Self, SshError> {
        let expanded = expand_proxy_command(template.trim(), host, port, username);
        if expanded.trim().is_empty() {
            return Err(SshError::Connection {
                host: host.to_string(),
                port,
                reason: "ProxyCommand is empty".to_string(),
            });
        }

        let mut cmd = build_shell_command(&expanded);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|err| SshError::Connection {
            host: host.to_string(),
            port,
            reason: format!("failed to spawn ProxyCommand `{expanded}`: {err}"),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| SshError::Connection {
            host: host.to_string(),
            port,
            reason: "ProxyCommand missing stdin pipe".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| SshError::Connection {
            host: host.to_string(),
            port,
            reason: "ProxyCommand missing stdout pipe".to_string(),
        })?;

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }
}

impl Drop for ProxyCommandStream {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl AsyncRead for ProxyCommandStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProxyCommandStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

#[cfg(unix)]
fn build_shell_command(expanded: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(expanded);
    cmd
}

#[cfg(windows)]
fn build_shell_command(expanded: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(expanded);
    cmd
}
