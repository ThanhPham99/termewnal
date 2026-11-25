use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::ChannelMsg;
use russh::client::Msg as ClientMsg;
use termewnal_core::vault::SecretStore;
use termewnal_pty::{PtyEvent, TerminalBuffer, TerminalSnapshot};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::TransportStatus;
use crate::auth::{AgentConnector, SshInteractionHandler};
use crate::client::{SshConnectConfig, SshTransport};
use crate::error::SshError;

enum SessionCmd {
    WriteBytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Disconnect,
}

struct ChannelWriteSink {
    tx: mpsc::UnboundedSender<SessionCmd>,
}

impl Write for ChannelWriteSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.tx.send(SessionCmd::WriteBytes(buf.to_vec()));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Interactive PTY-backed SSH `Session` streaming terminal I/O into [`TerminalBuffer`].
pub struct SshSession {
    transport: Arc<SshTransport>,
    buffer: Arc<TerminalBuffer>,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    status: Arc<Mutex<TransportStatus>>,
    used_interactive_auth: bool,
}

impl SshSession {
    /// Connects to an SSH server, performs strict `known_hosts` verification and authentication,
    /// allocates a PTY shell channel, and starts streaming I/O into a new [`TerminalBuffer`].
    pub async fn connect(
        config: SshConnectConfig,
        vault: Option<&dyn SecretStore>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Result<Self, SshError> {
        let transport =
            SshTransport::connect(&config, vault, interaction, agent_connector).await?;
        Self::open_on_transport(transport, config).await
    }

    /// Opens an interactive PTY shell channel on an already authenticated [`SshTransport`].
    pub async fn open_on_transport(
        transport: SshTransport,
        config: SshConnectConfig,
    ) -> Result<Self, SshError> {
        let buffer = Arc::new(TerminalBuffer::new(
            config.cols,
            config.rows,
            config.scrollback_lines,
        ));
        Self::open_on_buffer(
            transport,
            buffer,
            config.cols,
            config.rows,
            &config.term_name,
        )
        .await
    }

    /// Attaches a fresh PTY shell channel on `transport` to an existing [`TerminalBuffer`]
    /// (preserving scrollback across reconnects).
    pub async fn open_on_buffer(
        transport: SshTransport,
        buffer: Arc<TerminalBuffer>,
        cols: u16,
        rows: u16,
        term_name: &str,
    ) -> Result<Self, SshError> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        buffer.resize(cols, rows);

        let transport = Arc::new(transport);
        let channel = transport.open_session_channel().await?;
        let (mut rx, tx) = channel.split();

        tx.request_pty(true, term_name, u32::from(cols), u32::from(rows), 0, 0, &[])
            .await
            .map_err(|err| SshError::Channel(err.to_string()))?;
        await_channel_reply(&mut rx, &buffer, "PTY request").await?;

        tx.request_shell(true)
            .await
            .map_err(|err| SshError::Channel(err.to_string()))?;
        await_channel_reply(&mut rx, &buffer, "shell request").await?;

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        buffer.set_write_sink(Some(Box::new(ChannelWriteSink { tx: cmd_tx.clone() })));

        let status = Arc::new(Mutex::new(TransportStatus::Connected { latency_ms: None }));
        let used_interactive_auth = transport.used_interactive_auth();

        spawn_channel_pump(
            Arc::clone(&transport),
            rx,
            tx,
            cmd_rx,
            Arc::clone(&buffer),
            Arc::clone(&status),
        );

        Ok(Self {
            transport,
            buffer,
            cmd_tx,
            status,
            used_interactive_auth,
        })
    }

    /// Returns the active SSH `Primary Transport` backing this `Session`.
    pub fn primary_transport(&self) -> &Arc<SshTransport> {
        &self.transport
    }

    /// Returns the shared [`TerminalBuffer`] fed by this SSH session.
    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
    }

    /// Takes the async [`PtyEvent`] receiver once for driving an external event loop.
    pub fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<PtyEvent>> {
        self.buffer.take_event_receiver()
    }

    /// Writes keyboard or paste bytes to the remote SSH shell channel.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), SshError> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.cmd_tx
            .send(SessionCmd::WriteBytes(bytes.to_vec()))
            .map_err(|_| SshError::Disconnected)
    }

    /// Resizes the local [`TerminalBuffer`] and notifies the remote SSH PTY via `window_change`.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), SshError> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        self.buffer.resize(cols, rows);
        self.cmd_tx
            .send(SessionCmd::Resize { cols, rows })
            .map_err(|_| SshError::Disconnected)
    }

    /// Returns the current `(cols, rows)` terminal dimensions.
    pub fn dimensions(&self) -> (u16, u16) {
        self.buffer.dimensions()
    }

    /// Captures an immutable snapshot of the current terminal viewport.
    pub fn snapshot(&self) -> TerminalSnapshot {
        self.buffer.snapshot()
    }

    /// Blocks up to `timeout` until `predicate(&snapshot)` evaluates to `true`.
    pub fn wait_for_output<F>(&self, timeout: Duration, predicate: F) -> bool
    where
        F: FnMut(&TerminalSnapshot) -> bool,
    {
        self.buffer.wait_for_output(timeout, predicate)
    }

    /// Asynchronously awaits up to `timeout` until `predicate(&snapshot)` evaluates to `true`.
    pub async fn wait_for_output_async<F>(&self, timeout: Duration, predicate: F) -> bool
    where
        F: FnMut(&TerminalSnapshot) -> bool,
    {
        self.buffer.wait_for_output_async(timeout, predicate).await
    }

    /// Returns the current [`TransportStatus`] snapshot for this SSH session.
    pub fn transport_status(&self) -> TransportStatus {
        self.status
            .lock()
            .expect("ssh status mutex poisoned")
            .clone()
    }

    /// Returns `true` if this connection used interactive auth (2FA/OTP or prompt).
    pub fn used_interactive_auth(&self) -> bool {
        self.used_interactive_auth
    }

    /// Gracefully closes the SSH session channel and underlying transport.
    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(SessionCmd::Disconnect);
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        self.disconnect();
    }
}

async fn await_channel_reply(
    rx: &mut russh::ChannelReadHalf,
    buffer: &TerminalBuffer,
    stage: &str,
) -> Result<(), SshError> {
    loop {
        match rx.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => {
                return Err(SshError::Channel(format!("{stage} rejected by remote server")));
            }
            Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                buffer.feed_bytes(&data);
            }
            Some(ChannelMsg::WindowAdjusted { .. }) => {}
            Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                return Err(SshError::Channel(format!(
                    "channel closed while awaiting {stage} reply"
                )));
            }
            Some(_) => {}
        }
    }
}

fn spawn_channel_pump(
    transport: Arc<SshTransport>,
    mut rx: russh::ChannelReadHalf,
    tx: russh::ChannelWriteHalf<ClientMsg>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    buffer: Arc<TerminalBuffer>,
    status: Arc<Mutex<TransportStatus>>,
) {
    tokio::spawn(async move {
        let mut exit_code: Option<i32> = None;
        let mut cmd_open = true;

        loop {
            tokio::select! {
                msg = rx.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) => {
                            buffer.feed_bytes(&data);
                        }
                        Some(ChannelMsg::ExtendedData { data, .. }) => {
                            buffer.feed_bytes(&data);
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            exit_code = Some(exit_status as i32);
                        }
                        Some(ChannelMsg::Eof) => {
                            debug!("SSH channel received EOF");
                        }
                        Some(ChannelMsg::Close) | None => {
                            debug!("SSH channel closed");
                            break;
                        }
                        Some(_) => {}
                    }
                }
                cmd = cmd_rx.recv(), if cmd_open => {
                    match cmd {
                        Some(SessionCmd::WriteBytes(bytes)) => {
                            if let Err(err) = tx.data_bytes(bytes).await {
                                warn!(error = %err, "failed writing bytes to SSH channel");
                                break;
                            }
                        }
                        Some(SessionCmd::Resize { cols, rows }) => {
                            if let Err(err) = tx
                                .window_change(u32::from(cols), u32::from(rows), 0, 0)
                                .await
                            {
                                warn!(error = %err, "failed sending SSH window_change");
                            }
                        }
                        Some(SessionCmd::Disconnect) | None => {
                            let _ = tx.close().await;
                            break;
                        }
                    }
                    if cmd_rx.is_closed() {
                        cmd_open = false;
                    }
                }
            }
        }

        if let Ok(mut guard) = status.lock() {
            *guard = TransportStatus::Disconnected;
        }
        buffer.mark_exited(exit_code);
        let _ = transport.disconnect_shared().await;
    });
}
