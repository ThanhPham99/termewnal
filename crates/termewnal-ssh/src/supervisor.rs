use std::io::{self, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local};
use rand::RngExt;
use russh::ChannelMsg;
use russh::client::Msg as ClientMsg;
use termewnal_core::{
    AppConfig, DEFAULT_KEEPALIVE_INTERVAL_SECS, DEFAULT_KEEPALIVE_MAX_MISSED, Profile,
    vault::SecretStore,
};
use termewnal_pty::{LoginScriptRunner, TerminalBuffer};
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tokio::time::Instant as TokioInstant;
use tracing::{debug, warn};

use crate::TransportStatus;
use crate::auth::{AgentConnector, BoxFuture, SshInteractionHandler};
use crate::client::{
    SshConnectConfig, SshTransport, TcpKeepaliveSettings, await_channel_reply, ceil_secs,
    request_channel_forwarding,
};
use crate::error::SshError;
use crate::proxy_jump::{SshTransportChain, connect_transport_chain};

/// Default timeout (`3s`) for immediate wake / manual fast keepalive probes.
pub const DEFAULT_WAKE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Default minimum reconnect backoff (`1s`).
pub const DEFAULT_BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Default maximum reconnect backoff (`30s`).
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Sample of monotonic and wall-clock time used to detect OS sleep / clock-jump wakeups.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorInstant {
    pub monotonic: Instant,
    pub wall: SystemTime,
}

impl SupervisorInstant {
    /// Captures the current OS monotonic and wall clocks.
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

/// Injectable time source boundary allowing deterministic sleep/wake testing without sleeping in real time.
pub trait TimeSource: Send + Sync {
    /// Returns the current monotonic and wall-clock sample.
    fn now(&self) -> SupervisorInstant;

    /// Optional wakeup signal triggered immediately when a test double advances sleep time.
    fn jump_notify(&self) -> Option<Arc<Notify>> {
        None
    }
}

/// Production [`TimeSource`] backed by [`Instant::now`] and [`SystemTime::now`].
#[derive(Debug, Default)]
pub struct SystemTimeSource;

impl TimeSource for SystemTimeSource {
    fn now(&self) -> SupervisorInstant {
        SupervisorInstant::now()
    }
}

/// Controllable [`TimeSource`] double for deterministic clock-jump / sleep-wake tests.
#[derive(Debug)]
pub struct ManualTimeSource {
    current: Mutex<SupervisorInstant>,
    notify: Arc<Notify>,
}

impl ManualTimeSource {
    /// Creates a [`ManualTimeSource`] anchored at the current OS instant.
    pub fn new() -> Self {
        Self {
            current: Mutex::new(SupervisorInstant::now()),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Advances both monotonic and wall clocks by `duration` (simulating steady awake time).
    pub fn advance_steady(&self, duration: Duration) {
        {
            let mut guard = self.current.lock().expect("manual clock mutex poisoned");
            guard.monotonic += duration;
            guard.wall += duration;
        }
        self.notify.notify_one();
    }

    /// Advances only wall clock by `duration` while monotonic time stays paused (OS sleep/suspend).
    pub fn advance_sleep(&self, duration: Duration) {
        {
            let mut guard = self.current.lock().expect("manual clock mutex poisoned");
            guard.wall += duration;
        }
        self.notify.notify_one();
    }
}

impl Default for ManualTimeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSource for ManualTimeSource {
    fn now(&self) -> SupervisorInstant {
        *self.current.lock().expect("manual clock mutex poisoned")
    }

    fn jump_notify(&self) -> Option<Arc<Notify>> {
        Some(Arc::clone(&self.notify))
    }
}

/// Boundary factory for opening an authenticated [`SshTransport`] (direct or multi-hop `ProxyJump`).
pub trait TransportConnector: Send + Sync {
    /// Opens and authenticates an [`SshTransport`] using `config` and optional TCP keepalive settings.
    fn connect<'a>(
        &'a self,
        config: &'a SshConnectConfig,
        keepalive: Option<TcpKeepaliveSettings>,
    ) -> BoxFuture<'a, Result<SshTransport, SshError>>;

    /// Opens and authenticates an ordered [`SshTransportChain`] (`A -> B -> C`) for `config`.
    fn connect_chain<'a>(
        &'a self,
        config: &'a SshConnectConfig,
        keepalive: Option<TcpKeepaliveSettings>,
    ) -> BoxFuture<'a, Result<SshTransportChain, SshError>> {
        Box::pin(async move {
            let transport = self.connect(config, keepalive).await?;
            Ok(SshTransportChain::direct(transport))
        })
    }

    /// Optional `Vault` store exposed to [`ConnectionSupervisor`] for `LoginScriptStep` secret lookup.
    fn vault_store(&self) -> Option<&dyn SecretStore> {
        None
    }
}

/// Default [`TransportConnector`] supporting direct TCP and ordered multi-hop `ProxyJump`
/// chains with `Vault`, interactive TOFU/auth callbacks, and SSH Agent.
pub struct DefaultTransportConnector {
    vault: Option<Arc<dyn SecretStore>>,
    interaction: Arc<dyn SshInteractionHandler>,
    agent_connector: Arc<dyn AgentConnector>,
}

impl DefaultTransportConnector {
    /// Creates a [`DefaultTransportConnector`] wired with optional `Vault`, interaction handler, and agent connector.
    pub fn new(
        vault: Option<Arc<dyn SecretStore>>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Self {
        Self {
            vault,
            interaction,
            agent_connector,
        }
    }
}

impl TransportConnector for DefaultTransportConnector {
    fn connect<'a>(
        &'a self,
        config: &'a SshConnectConfig,
        keepalive: Option<TcpKeepaliveSettings>,
    ) -> BoxFuture<'a, Result<SshTransport, SshError>> {
        Box::pin(async move {
            SshTransport::connect_with_keepalive(
                config,
                keepalive,
                self.vault.as_deref(),
                Arc::clone(&self.interaction),
                Arc::clone(&self.agent_connector),
            )
            .await
        })
    }

    fn connect_chain<'a>(
        &'a self,
        config: &'a SshConnectConfig,
        keepalive: Option<TcpKeepaliveSettings>,
    ) -> BoxFuture<'a, Result<SshTransportChain, SshError>> {
        Box::pin(async move {
            connect_transport_chain(
                config,
                keepalive,
                self.vault.as_deref(),
                Arc::clone(&self.interaction),
                Arc::clone(&self.agent_connector),
            )
            .await
        })
    }

    fn vault_store(&self) -> Option<&dyn SecretStore> {
        self.vault.as_deref()
    }
}

/// Keepalive, wake-probe, backoff, and multiplexer auto-attach settings for [`ConnectionSupervisor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorConfig {
    pub keepalive_interval: Duration,
    pub keepalive_max_missed: u32,
    pub probe_timeout: Duration,
    pub wake_probe_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub mux_attach_command: Option<String>,
    pub tcp_keepalive: bool,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            keepalive_interval: Duration::from_secs(DEFAULT_KEEPALIVE_INTERVAL_SECS),
            keepalive_max_missed: DEFAULT_KEEPALIVE_MAX_MISSED,
            probe_timeout: DEFAULT_WAKE_PROBE_TIMEOUT,
            wake_probe_timeout: DEFAULT_WAKE_PROBE_TIMEOUT,
            backoff_min: DEFAULT_BACKOFF_MIN,
            backoff_max: DEFAULT_BACKOFF_MAX,
            mux_attach_command: None,
            tcp_keepalive: true,
        }
    }
}

impl SupervisorConfig {
    /// Derives [`SupervisorConfig`] from global [`AppConfig`] defaults.
    pub fn from_app_config(app: &AppConfig) -> Self {
        Self {
            keepalive_interval: Duration::from_secs(app.keepalive_interval_secs),
            keepalive_max_missed: app.keepalive_max_missed.max(1),
            ..Self::default()
        }
    }

    /// Derives [`SupervisorConfig`] by merging per-[`Profile`] overrides over global [`AppConfig`].
    pub fn from_configs(app: &AppConfig, profile: &Profile) -> Self {
        let (interval_secs, max_missed) = profile.resolve_keepalive(app);
        Self {
            keepalive_interval: Duration::from_secs(interval_secs),
            keepalive_max_missed: max_missed.max(1),
            mux_attach_command: profile.effective_mux_attach_command(),
            ..Self::default()
        }
    }

    fn tcp_keepalive_settings(&self) -> Option<TcpKeepaliveSettings> {
        if self.tcp_keepalive && !self.keepalive_interval.is_zero() {
            Some(TcpKeepaliveSettings {
                interval: self.keepalive_interval,
            })
        } else {
            None
        }
    }

    fn effective_probe_timeout(&self) -> Duration {
        if self.keepalive_interval.is_zero() {
            self.probe_timeout.max(Duration::from_millis(1))
        } else {
            self.probe_timeout
                .min(self.keepalive_interval)
                .max(Duration::from_millis(1))
        }
    }
}

/// Real-time status snapshot published by [`ConnectionSupervisor`] on status transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportStatusEvent {
    pub epoch: u64,
    pub status: TransportStatus,
    pub last_error: Option<String>,
}

/// Watch snapshot exposing the active [`TransportStatus`] and current [`SshTransport`] handle for
/// dependent `Tunnel`s and SFTP channels.
#[derive(Clone)]
pub struct TransportReadySnapshot {
    pub epoch: u64,
    pub status: TransportStatus,
    pub transport: Option<Arc<SshTransport>>,
}

impl TransportReadySnapshot {
    /// Returns `true` when the supervised `Transport` is currently `Connected`.
    pub fn is_connected(&self) -> bool {
        matches!(self.status, TransportStatus::Connected { .. }) && self.transport.is_some()
    }
}

/// Lifecycle events emitted by [`ConnectionSupervisor`] for UI Status Bar and alert integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorEvent {
    StatusChanged(TransportStatusEvent),
    Reconnected {
        epoch: u64,
        divider: String,
        mux_command_sent: Option<String>,
    },
    MitmBlocked {
        host: String,
        port: u16,
        line: usize,
        key_type: String,
        expected_fingerprint: String,
        received_fingerprint: String,
    },
    Disconnected {
        error: Option<String>,
    },
}

/// Computes exponential backoff with ±20% jitter clamped to `[min, max]`.
///
/// `jitter_unit` is expected in `[0.0, 1.0)`.
pub fn compute_reconnect_backoff(
    attempt: u32,
    min: Duration,
    max: Duration,
    jitter_unit: f64,
) -> Duration {
    let min = min.max(Duration::from_millis(1));
    let max = max.max(min);
    let exp = attempt.saturating_sub(1).min(16);
    let factor = 1_u128 << exp;
    let base_ms = min
        .as_millis()
        .saturating_mul(factor)
        .min(max.as_millis());
    let scale = 0.8 + 0.4 * jitter_unit.clamp(0.0, 0.999_999);
    let jittered_ms = ((base_ms as f64) * scale).round() as u128;
    let clamped_ms = jittered_ms.clamp(min.as_millis(), max.as_millis());
    Duration::from_millis(clamped_ms as u64)
}

/// Formats the scrollback recovery divider `"\r\n--- Reconnected at HH:MM:SS ---\r\n"`.
pub fn format_reconnected_divider(wall_time: SystemTime) -> String {
    let local: DateTime<Local> = DateTime::from(wall_time);
    format!("\r\n--- Reconnected at {} ---\r\n", local.format("%H:%M:%S"))
}

enum SupervisorCmd {
    WriteBytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    ProbeNow,
    ReconnectNow,
    CancelReconnect,
    Disconnect,
    UpdateKeepalive {
        interval: Duration,
        max_missed: u32,
    },
}

struct SupervisorWriteSink {
    tx: mpsc::UnboundedSender<SupervisorCmd>,
}

impl Write for SupervisorWriteSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.tx.send(SupervisorCmd::WriteBytes(buf.to_vec()));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Supervised SSH `Transport` + interactive PTY `Session` handle.
///
/// Actively monitors `Transport` health via TCP + SSH keepalives, detects clock jumps / sleep wake,
/// drives non-modal exponential backoff reconnects while preserving [`TerminalBuffer`] scrollback,
/// inserts `--- Reconnected at HH:MM:SS ---` dividers, and optionally auto-attaches `tmux`/`screen`.
#[derive(Clone)]
pub struct ConnectionSupervisor {
    cmd_tx: mpsc::UnboundedSender<SupervisorCmd>,
    status_rx: watch::Receiver<TransportStatus>,
    transport_rx: watch::Receiver<Option<Arc<SshTransport>>>,
    ready_rx: watch::Receiver<TransportReadySnapshot>,
    events_tx: broadcast::Sender<SupervisorEvent>,
    buffer: Arc<TerminalBuffer>,
    used_interactive_auth: Arc<AtomicBool>,
    dimensions: Arc<Mutex<(u16, u16)>>,
}

impl ConnectionSupervisor {
    /// Starts supervising an SSH connection on `buffer` using the default direct-TCP connector
    /// and system clock.
    pub fn connect_on_buffer(
        connect_config: SshConnectConfig,
        supervisor_config: SupervisorConfig,
        buffer: Arc<TerminalBuffer>,
        vault: Option<Arc<dyn SecretStore>>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Self {
        let (supervisor, _events_rx) = Self::connect_on_buffer_with_events(
            connect_config,
            supervisor_config,
            buffer,
            vault,
            interaction,
            agent_connector,
        );
        supervisor
    }

    /// Starts supervising an SSH connection on `buffer` and returns a race-free ordered event
    /// stream attached before the actor spawns.
    pub fn connect_on_buffer_with_events(
        connect_config: SshConnectConfig,
        supervisor_config: SupervisorConfig,
        buffer: Arc<TerminalBuffer>,
        vault: Option<Arc<dyn SecretStore>>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> (Self, mpsc::UnboundedReceiver<SupervisorEvent>) {
        let connector: Arc<dyn TransportConnector> = Arc::new(DefaultTransportConnector::new(
            vault,
            interaction,
            agent_connector,
        ));
        Self::connect_with_connector_and_events(
            connect_config,
            supervisor_config,
            buffer,
            connector,
            Arc::new(SystemTimeSource),
        )
    }

    /// Starts supervising an SSH connection with injectable [`TransportConnector`] and [`TimeSource`]
    /// boundaries (used by `ProxyJump` composition and deterministic clock-jump tests).
    pub fn connect_with_connector(
        connect_config: SshConnectConfig,
        supervisor_config: SupervisorConfig,
        buffer: Arc<TerminalBuffer>,
        connector: Arc<dyn TransportConnector>,
        time_source: Arc<dyn TimeSource>,
    ) -> Self {
        let (supervisor, _events_rx) = Self::connect_with_connector_and_events(
            connect_config,
            supervisor_config,
            buffer,
            connector,
            time_source,
        );
        supervisor
    }

    /// Starts supervising an SSH connection with injectable boundaries and a race-free ordered
    /// event receiver attached before actor spawn.
    pub fn connect_with_connector_and_events(
        connect_config: SshConnectConfig,
        supervisor_config: SupervisorConfig,
        buffer: Arc<TerminalBuffer>,
        connector: Arc<dyn TransportConnector>,
        time_source: Arc<dyn TimeSource>,
    ) -> (Self, mpsc::UnboundedReceiver<SupervisorEvent>) {
        let cols = connect_config.cols.max(1);
        let rows = connect_config.rows.max(1);
        buffer.resize(cols, rows);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        buffer.set_write_sink(Some(Box::new(SupervisorWriteSink {
            tx: cmd_tx.clone(),
        })));

        let (status_tx, status_rx) = watch::channel(TransportStatus::Connecting);
        let (transport_tx, transport_rx) = watch::channel(None);
        let (ready_tx, ready_rx) = watch::channel(TransportReadySnapshot {
            epoch: 0,
            status: TransportStatus::Connecting,
            transport: None,
        });
        let (events_tx, _) = broadcast::channel(128);
        let (ordered_events_tx, ordered_events_rx) = mpsc::unbounded_channel();
        let used_interactive_auth = Arc::new(AtomicBool::new(false));
        let dimensions = Arc::new(Mutex::new((cols, rows)));

        let actor = SupervisorActor {
            connect_config,
            config: supervisor_config,
            buffer: Arc::clone(&buffer),
            connector,
            time_source,
            cmd_tx: cmd_tx.clone(),
            cmd_rx,
            status_tx,
            transport_tx,
            ready_tx,
            events_tx: events_tx.clone(),
            ordered_events_tx: Some(ordered_events_tx),
            used_interactive_auth: Arc::clone(&used_interactive_auth),
            epoch: 0,
            has_connected_once: false,
            cols,
            rows,
            last_error: None,
            current_status: TransportStatus::Connecting,
        };
        tokio::spawn(actor.run());

        let supervisor = Self {
            cmd_tx,
            status_rx,
            transport_rx,
            ready_rx,
            events_tx,
            buffer,
            used_interactive_auth,
            dimensions,
        };
        (supervisor, ordered_events_rx)
    }

    /// Returns the shared [`TerminalBuffer`] preserved across disconnects and reconnects.
    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
    }

    /// Returns the latest [`TransportStatus`] snapshot.
    pub fn status(&self) -> TransportStatus {
        self.status_rx.borrow().clone()
    }

    /// Subscribes to live [`TransportStatus`] updates.
    pub fn subscribe_status(&self) -> watch::Receiver<TransportStatus> {
        self.status_rx.clone()
    }

    /// Subscribes to the active [`SshTransport`] watch channel (`Some` when connected/degraded, `None` otherwise).
    pub fn subscribe_transport(&self) -> watch::Receiver<Option<Arc<SshTransport>>> {
        self.transport_rx.clone()
    }

    /// Returns the currently active [`SshTransport`], if `Connected` or `Degraded`.
    pub fn current_transport(&self) -> Option<Arc<SshTransport>> {
        self.transport_rx.borrow().clone()
    }

    /// Subscribes to the combined [`TransportReadySnapshot`] (epoch + status + transport handle) for `Tunnel`s.
    pub fn subscribe_ready(&self) -> watch::Receiver<TransportReadySnapshot> {
        self.ready_rx.clone()
    }

    /// Subscribes to [`SupervisorEvent`] lifecycle broadcasts.
    pub fn subscribe_events(&self) -> broadcast::Receiver<SupervisorEvent> {
        self.events_tx.subscribe()
    }

    /// Returns `true` if the most recent authentication required interactive input (2FA/OTP).
    pub fn used_interactive_auth(&self) -> bool {
        self.used_interactive_auth.load(Ordering::SeqCst)
    }

    /// Returns the current terminal grid `(cols, rows)`.
    pub fn dimensions(&self) -> (u16, u16) {
        *self.dimensions.lock().expect("dimensions mutex poisoned")
    }

    /// Writes terminal input bytes to the active SSH PTY shell channel.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), SshError> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.cmd_tx
            .send(SupervisorCmd::WriteBytes(bytes.to_vec()))
            .map_err(|_| SshError::Disconnected)
    }

    /// Resizes the local [`TerminalBuffer`] and forwards `window_change` to the active SSH channel.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), SshError> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        self.buffer.resize(cols, rows);
        *self.dimensions.lock().expect("dimensions mutex poisoned") = (cols, rows);
        self.cmd_tx
            .send(SupervisorCmd::Resize { cols, rows })
            .map_err(|_| SshError::Disconnected)
    }

    /// Dispatches an immediate fast keepalive probe (`3s` timeout by default) when connected/degraded,
    /// or wakes reconnect backoff immediately if reconnecting.
    pub fn probe_now(&self) {
        let _ = self.cmd_tx.send(SupervisorCmd::ProbeNow);
    }

    /// Cancels any active backoff countdown and starts a connection attempt immediately.
    pub fn reconnect_now(&self) {
        let _ = self.cmd_tx.send(SupervisorCmd::ReconnectNow);
    }

    /// Cancels pending reconnect attempts and transitions to [`TransportStatus::Disconnected`] while
    /// keeping terminal scrollback intact.
    pub fn cancel_reconnect(&self) {
        let _ = self.cmd_tx.send(SupervisorCmd::CancelReconnect);
    }

    /// Gracefully disconnects the supervised transport and stops the supervisor actor.
    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(SupervisorCmd::Disconnect);
    }

    /// Updates keepalive interval and max-missed thresholds dynamically (e.g. after `config.toml` hot-reload).
    pub fn update_keepalive(&self, interval: Duration, max_missed: u32) {
        let _ = self.cmd_tx.send(SupervisorCmd::UpdateKeepalive {
            interval,
            max_missed: max_missed.max(1),
        });
    }
}

enum ProbeOutcome {
    Alive { rtt_ms: u64 },
    TimedOut,
    Dead(SshError),
}

enum ConnectedExit {
    Drop(String),
    CleanShellExit(Option<i32>),
    CancelToDisconnected,
    Shutdown,
}

enum ConnectOutcome {
    Connected {
        chain: SshTransportChain,
        rx: russh::ChannelReadHalf,
        tx: russh::ChannelWriteHalf<ClientMsg>,
        initial_output: Vec<u8>,
    },
    RetryableError(String),
    FatalMitm(SshError),
    FatalStop(Option<String>),
    CancelledToDisconnected,
    Shutdown,
}

struct ChainDropGuard(Option<SshTransportChain>);

impl ChainDropGuard {
    fn chain(&self) -> &SshTransportChain {
        self.0.as_ref().expect("chain present until taken or torn down")
    }

    async fn teardown(&mut self) {
        if let Some(chain) = self.0.take() {
            chain.teardown_reverse().await;
        }
    }

    fn take(&mut self) -> SshTransportChain {
        self.0.take().expect("chain already taken")
    }
}

impl Drop for ChainDropGuard {
    fn drop(&mut self) {
        if let Some(chain) = self.0.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                chain.teardown_reverse().await;
            });
        }
    }
}

enum BackoffWaitOutcome {
    RetryNow,
    CancelledToDisconnected,
    Shutdown,
}

enum IdleOutcome {
    ReconnectNow,
    Shutdown,
}

struct SupervisorActor {
    connect_config: SshConnectConfig,
    config: SupervisorConfig,
    buffer: Arc<TerminalBuffer>,
    connector: Arc<dyn TransportConnector>,
    time_source: Arc<dyn TimeSource>,
    cmd_tx: mpsc::UnboundedSender<SupervisorCmd>,
    cmd_rx: mpsc::UnboundedReceiver<SupervisorCmd>,
    status_tx: watch::Sender<TransportStatus>,
    transport_tx: watch::Sender<Option<Arc<SshTransport>>>,
    ready_tx: watch::Sender<TransportReadySnapshot>,
    events_tx: broadcast::Sender<SupervisorEvent>,
    ordered_events_tx: Option<mpsc::UnboundedSender<SupervisorEvent>>,
    used_interactive_auth: Arc<AtomicBool>,
    epoch: u64,
    has_connected_once: bool,
    cols: u16,
    rows: u16,
    last_error: Option<String>,
    current_status: TransportStatus,
}

impl SupervisorActor {
    async fn run(mut self) {
        let mut reconnect_attempt: u32 = 0;

        loop {
            let retry_err = if reconnect_attempt > 0 || self.has_connected_once {
                self.last_error.clone()
            } else {
                None
            };
            self.publish_status(TransportStatus::Connecting, None, retry_err);

            match self.attempt_connect_and_open_pty().await {
                ConnectOutcome::Connected {
                    chain,
                    rx,
                    tx,
                    initial_output,
                } => {
                    let transport = Arc::clone(chain.target());
                    let is_reconnect = self.has_connected_once;
                    self.has_connected_once = true;
                    self.epoch = self.epoch.wrapping_add(1);
                    self.used_interactive_auth
                        .store(chain.used_interactive_auth(), Ordering::SeqCst);

                    let mut login_runner = LoginScriptRunner::compile(
                        &self.connect_config.login_scripts,
                        self.connector.vault_store(),
                    );
                    let mut mux_command_sent = None;
                    if is_reconnect {
                        let divider = format_reconnected_divider(self.time_source.now().wall);
                        self.buffer.feed_bytes(divider.as_bytes());
                        if !initial_output.is_empty() {
                            self.buffer.feed_bytes(&initial_output);
                        }
                        if let Some(cmd) = self
                            .config
                            .mux_attach_command
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            let line = format!("{cmd}\r");
                            if let Err(err) = tx.data_bytes(line.into_bytes()).await {
                                warn!(error = %err, "failed sending mux attach command after reconnect");
                            } else {
                                mux_command_sent = Some(cmd.to_string());
                            }
                        }
                        self.emit_event(SupervisorEvent::Reconnected {
                            epoch: self.epoch,
                            divider,
                            mux_command_sent,
                        });
                    } else if !initial_output.is_empty() {
                        self.buffer.feed_bytes(&initial_output);
                    }
                    if !login_runner.is_finished() {
                        let dispatches = login_runner.advance(&initial_output);
                        schedule_supervisor_login_dispatches(&self.cmd_tx, dispatches);
                    }

                    self.last_error = None;
                    self.publish_status(
                        TransportStatus::Connected { latency_ms: None },
                        Some(Arc::clone(&transport)),
                        None,
                    );

                    match self.monitor_connected(chain, rx, tx, &mut login_runner).await {
                        ConnectedExit::Drop(reason) => {
                            self.last_error = Some(reason);
                            self.clear_transport();
                            reconnect_attempt = 1;
                            match self.wait_reconnect_backoff(reconnect_attempt).await {
                                BackoffWaitOutcome::RetryNow => continue,
                                BackoffWaitOutcome::CancelledToDisconnected => {
                                    if !self.park_disconnected(None).await {
                                        break;
                                    }
                                    reconnect_attempt = 1;
                                }
                                BackoffWaitOutcome::Shutdown => break,
                            }
                        }
                        ConnectedExit::CleanShellExit(code) => {
                            self.clear_transport();
                            self.buffer.mark_exited(code);
                            self.enter_disconnected(None);
                            break;
                        }
                        ConnectedExit::CancelToDisconnected => {
                            self.clear_transport();
                            if !self.park_disconnected(None).await {
                                break;
                            }
                            reconnect_attempt = 1;
                        }
                        ConnectedExit::Shutdown => {
                            self.clear_transport();
                            self.enter_disconnected(None);
                            break;
                        }
                    }
                }
                ConnectOutcome::RetryableError(err_msg) => {
                    self.clear_transport();
                    self.last_error = Some(err_msg.clone());
                    reconnect_attempt = reconnect_attempt.saturating_add(1).max(1);
                    match self.wait_reconnect_backoff(reconnect_attempt).await {
                        BackoffWaitOutcome::RetryNow => continue,
                        BackoffWaitOutcome::CancelledToDisconnected => {
                            if !self.park_disconnected(Some(err_msg)).await {
                                break;
                            }
                            reconnect_attempt = 1;
                        }
                        BackoffWaitOutcome::Shutdown => break,
                    }
                }
                ConnectOutcome::FatalMitm(err) => {
                    self.clear_transport();
                    let msg = err.to_string();
                    if let SshError::HostKeyMismatch {
                        host,
                        port,
                        line,
                        key_type,
                        expected_fingerprint,
                        received_fingerprint,
                    } = err
                    {
                        self.emit_event(SupervisorEvent::MitmBlocked {
                            host,
                            port,
                            line,
                            key_type,
                            expected_fingerprint,
                            received_fingerprint,
                        });
                    }
                    self.enter_disconnected(Some(msg));
                    break;
                }
                ConnectOutcome::FatalStop(err_msg) => {
                    self.clear_transport();
                    if !self.park_disconnected(err_msg).await {
                        break;
                    }
                    reconnect_attempt = 1;
                }
                ConnectOutcome::CancelledToDisconnected => {
                    self.clear_transport();
                    if !self.park_disconnected(None).await {
                        break;
                    }
                    reconnect_attempt = 1;
                }
                ConnectOutcome::Shutdown => {
                    self.clear_transport();
                    self.enter_disconnected(None);
                    break;
                }
            }
        }
    }

    async fn park_disconnected(&mut self, error: Option<String>) -> bool {
        self.enter_disconnected(error);
        matches!(
            self.wait_while_disconnected().await,
            IdleOutcome::ReconnectNow
        )
    }

    async fn attempt_connect_and_open_pty(&mut self) -> ConnectOutcome {
        let mut cfg = self.connect_config.clone();
        cfg.cols = self.cols;
        cfg.rows = self.rows;
        let ka = self.config.tcp_keepalive_settings();
        let connector = Arc::clone(&self.connector);
        let connect_fut = async move {
            let chain = connector.connect_chain(&cfg, ka).await?;
            let mut guard = ChainDropGuard(Some(chain));
            let channel = match guard.chain().target().open_session_channel().await {
                Ok(ch) => ch,
                Err(err) => {
                    guard.teardown().await;
                    return Err(err);
                }
            };
            let (mut rx, tx) = channel.split();
            let mut initial_output = Vec::new();

            if let Err(err) = tx
                .request_pty(
                    true,
                    &cfg.term_name,
                    u32::from(cfg.cols),
                    u32::from(cfg.rows),
                    0,
                    0,
                    &[],
                )
                .await
            {
                guard.teardown().await;
                return Err(SshError::Channel(err.to_string()));
            }
            if let Err(err) = await_channel_reply(&mut rx, "PTY request", |data| {
                initial_output.extend_from_slice(data);
            })
            .await
            {
                guard.teardown().await;
                return Err(err);
            }

            let x11_auth = guard.chain().target().x11_auth().cloned();
            request_channel_forwarding(
                &tx,
                &mut rx,
                cfg.forward_agent,
                x11_auth.as_ref(),
                &mut |data| {
                    initial_output.extend_from_slice(data);
                },
            )
            .await;

            if let Err(err) = tx.request_shell(true).await {
                guard.teardown().await;
                return Err(SshError::Channel(err.to_string()));
            }
            if let Err(err) = await_channel_reply(&mut rx, "shell request", |data| {
                initial_output.extend_from_slice(data);
            })
            .await
            {
                guard.teardown().await;
                return Err(err);
            }

            Ok::<_, SshError>((guard.take(), rx, tx, initial_output))
        };
        tokio::pin!(connect_fut);

        loop {
            tokio::select! {
                res = &mut connect_fut => {
                    return match res {
                        Ok((chain, rx, tx, initial_output)) => ConnectOutcome::Connected {
                            chain,
                            rx,
                            tx,
                            initial_output,
                        },
                        Err(err @ SshError::HostKeyMismatch { .. }) => ConnectOutcome::FatalMitm(err),
                        Err(err @ (SshError::HostKeyRejected { .. } | SshError::InteractiveCancelled)) => {
                            ConnectOutcome::FatalStop(Some(err.to_string()))
                        }
                        Err(err @ (SshError::AuthenticationFailed { .. } | SshError::KeyPassphraseRequired { .. } | SshError::KeyLoad { .. }))
                            if !self.has_connected_once =>
                        {
                            ConnectOutcome::FatalStop(Some(err.to_string()))
                        }
                        Err(err) => ConnectOutcome::RetryableError(err.to_string()),
                    };
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(SupervisorCmd::Resize { cols, rows }) => {
                            self.cols = cols;
                            self.rows = rows;
                        }
                        Some(SupervisorCmd::CancelReconnect) => {
                            return ConnectOutcome::CancelledToDisconnected;
                        }
                        Some(SupervisorCmd::Disconnect) | None => {
                            return ConnectOutcome::Shutdown;
                        }
                        Some(SupervisorCmd::UpdateKeepalive { interval, max_missed }) => {
                            self.config.keepalive_interval = interval;
                            self.config.keepalive_max_missed = max_missed;
                        }
                        Some(SupervisorCmd::WriteBytes(_))
                        | Some(SupervisorCmd::ProbeNow)
                        | Some(SupervisorCmd::ReconnectNow) => {}
                    }
                }
            }
        }
    }

    async fn monitor_connected(
        &mut self,
        chain: SshTransportChain,
        mut rx: russh::ChannelReadHalf,
        tx: russh::ChannelWriteHalf<ClientMsg>,
        login_runner: &mut LoginScriptRunner,
    ) -> ConnectedExit {
        let transport = Arc::clone(chain.target());
        let mut exit_status: Option<i32> = None;
        let mut missed_probes: u32 = 0;
        let mut last_clock = self.time_source.now();
        let jump_notify = self.time_source.jump_notify();

        // Fire one immediate RTT measurement probe on connect so live latency appears immediately.
        let mut probe_in_flight: Option<(bool, Pin<Box<oneshot::Receiver<ProbeOutcome>>>)> =
            Some((
                false,
                Box::pin(spawn_chain_probe(
                    chain.clone(),
                    self.config.effective_probe_timeout(),
                )),
            ));

        let mut keepalive_deadline = if self.config.keepalive_interval.is_zero() {
            None
        } else {
            Some(TokioInstant::now() + self.config.keepalive_interval)
        };
        let mut wake_check_deadline = TokioInstant::now() + wake_check_period(&self.config);

        loop {
            let ka_sleep = sleep_until_opt(keepalive_deadline);
            let wake_sleep = tokio::time::sleep_until(wake_check_deadline);
            let jump_fut = wait_jump_notify(jump_notify.as_ref());

            tokio::select! {
                (hop_idx, hop_host, hop_port) = chain.wait_upstream_closed(), if chain.has_jump_hops() => {
                    let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                    chain.teardown_reverse().await;
                    return ConnectedExit::Drop(format!(
                        "upstream ProxyJump hop #{} ({hop_host}:{hop_port}) disconnected",
                        hop_idx + 1,
                    ));
                }
                msg = rx.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                            self.buffer.feed_bytes(&data);
                            if !login_runner.is_finished() {
                                let dispatches = login_runner.advance(&data);
                                schedule_supervisor_login_dispatches(&self.cmd_tx, dispatches);
                            }
                            if missed_probes > 0 {
                                missed_probes = 0;
                                self.publish_status(
                                    TransportStatus::Connected { latency_ms: None },
                                    Some(Arc::clone(&transport)),
                                    None,
                                );
                            }
                        }
                        Some(ChannelMsg::ExitStatus { exit_status: code }) => {
                            exit_status = Some(code as i32);
                        }
                        Some(ChannelMsg::Eof) => {
                            debug!("SSH channel received EOF");
                        }
                        Some(ChannelMsg::Close) | None => {
                            let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                            let clean_exit = exit_status.is_some()
                                && !chain.any_hop_closed()
                                && missed_probes == 0;
                            chain.teardown_reverse().await;
                            if clean_exit {
                                return ConnectedExit::CleanShellExit(exit_status);
                            }
                            return ConnectedExit::Drop("SSH transport closed".to_string());
                        }
                        Some(_) => {}
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(SupervisorCmd::WriteBytes(bytes)) => {
                            if let Err(err) = tx.data_bytes(bytes).await {
                                warn!(error = %err, "failed writing bytes to SSH channel");
                                chain.teardown_reverse().await;
                                return ConnectedExit::Drop(err.to_string());
                            }
                        }
                        Some(SupervisorCmd::Resize { cols, rows }) => {
                            self.cols = cols;
                            self.rows = rows;
                            if let Err(err) = tx
                                .window_change(u32::from(cols), u32::from(rows), 0, 0)
                                .await
                            {
                                warn!(error = %err, "failed sending SSH window_change");
                            }
                        }
                        Some(SupervisorCmd::ProbeNow) => {
                            probe_in_flight = Some((
                                true,
                                Box::pin(spawn_chain_probe(
                                    chain.clone(),
                                    self.config.wake_probe_timeout,
                                )),
                            ));
                        }
                        Some(SupervisorCmd::ReconnectNow) => {
                            let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                            chain.teardown_reverse().await;
                            return ConnectedExit::Drop("manual reconnect requested".to_string());
                        }
                        Some(SupervisorCmd::CancelReconnect) => {
                            let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                            chain.teardown_reverse().await;
                            return ConnectedExit::CancelToDisconnected;
                        }
                        Some(SupervisorCmd::Disconnect) | None => {
                            let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                            chain.teardown_reverse().await;
                            return ConnectedExit::Shutdown;
                        }
                        Some(SupervisorCmd::UpdateKeepalive { interval, max_missed }) => {
                            self.config.keepalive_interval = interval;
                            self.config.keepalive_max_missed = max_missed;
                            keepalive_deadline = if interval.is_zero() {
                                None
                            } else {
                                Some(TokioInstant::now() + interval)
                            };
                            wake_check_deadline = TokioInstant::now() + wake_check_period(&self.config);
                        }
                    }
                }
                probe_res = poll_probe(&mut probe_in_flight), if probe_in_flight.is_some() => {
                    let (is_fast_probe, outcome) = probe_res;
                    probe_in_flight = None;
                    match outcome {
                        ProbeOutcome::Alive { rtt_ms } => {
                            missed_probes = 0;
                            if !self.config.keepalive_interval.is_zero() {
                                keepalive_deadline = Some(TokioInstant::now() + self.config.keepalive_interval);
                            }
                            self.publish_status(
                                TransportStatus::Connected {
                                    latency_ms: Some(rtt_ms),
                                },
                                Some(Arc::clone(&transport)),
                                None,
                            );
                        }
                        ProbeOutcome::Dead(err) => {
                            let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                            chain.teardown_reverse().await;
                            return ConnectedExit::Drop(err.to_string());
                        }
                        ProbeOutcome::TimedOut => {
                            if is_fast_probe {
                                let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                                chain.teardown_reverse().await;
                                return ConnectedExit::Drop("fast keepalive probe timed out".to_string());
                            }
                            missed_probes = missed_probes.saturating_add(1);
                            if missed_probes >= self.config.keepalive_max_missed.max(1) {
                                let _ = tokio::time::timeout(Duration::from_millis(150), tx.close()).await;
                                chain.teardown_reverse().await;
                                return ConnectedExit::Drop(format!(
                                    "keepalive timed out after {missed_probes} missed probes"
                                ));
                            }
                            self.publish_status(
                                TransportStatus::Degraded {
                                    missed_probes: missed_probes.min(u32::from(u8::MAX)) as u8,
                                },
                                Some(Arc::clone(&transport)),
                                None,
                            );
                        }
                    }
                }
                () = ka_sleep, if keepalive_deadline.is_some() => {
                    if !self.config.keepalive_interval.is_zero() {
                        let next = TokioInstant::now() + self.config.keepalive_interval;
                        keepalive_deadline = Some(next);
                        if probe_in_flight.is_none() {
                            probe_in_flight = Some((
                                false,
                                Box::pin(spawn_chain_probe(
                                    chain.clone(),
                                    self.config.effective_probe_timeout(),
                                )),
                            ));
                        }
                    } else {
                        keepalive_deadline = None;
                    }
                }
                () = wake_sleep => {
                    wake_check_deadline = TokioInstant::now() + wake_check_period(&self.config);
                    self.maybe_trigger_wake_probe(&chain, &mut last_clock, &mut probe_in_flight);
                }
                () = jump_fut, if jump_notify.is_some() => {
                    self.maybe_trigger_wake_probe(&chain, &mut last_clock, &mut probe_in_flight);
                }
            }
        }
    }

    fn maybe_trigger_wake_probe(
        &self,
        chain: &SshTransportChain,
        last_clock: &mut SupervisorInstant,
        probe_in_flight: &mut Option<(bool, Pin<Box<oneshot::Receiver<ProbeOutcome>>>)>,
    ) {
        let now = self.time_source.now();
        let jumped = is_clock_jump(*last_clock, now, self.config.keepalive_interval);
        *last_clock = now;
        if jumped {
            *probe_in_flight = Some((
                true,
                Box::pin(spawn_chain_probe(
                    chain.clone(),
                    self.config.wake_probe_timeout,
                )),
            ));
        }
    }

    async fn wait_reconnect_backoff(&mut self, attempt: u32) -> BackoffWaitOutcome {
        let jitter_unit: f64 = rand::rng().random();
        let delay = compute_reconnect_backoff(
            attempt,
            self.config.backoff_min,
            self.config.backoff_max,
            jitter_unit,
        );
        let deadline = TokioInstant::now() + delay;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;

        let initial_secs = ceil_secs(delay);
        self.publish_status(
            TransportStatus::Reconnecting {
                attempt,
                next_retry_secs: initial_secs,
            },
            None,
            self.last_error.clone(),
        );

        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    return BackoffWaitOutcome::RetryNow;
                }
                _ = tick.tick() => {
                    let remaining = deadline.saturating_duration_since(TokioInstant::now());
                    let secs = ceil_secs(remaining);
                    self.publish_status(
                        TransportStatus::Reconnecting {
                            attempt,
                            next_retry_secs: secs,
                        },
                        None,
                        self.last_error.clone(),
                    );
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(SupervisorCmd::ReconnectNow) | Some(SupervisorCmd::ProbeNow) => {
                            return BackoffWaitOutcome::RetryNow;
                        }
                        Some(SupervisorCmd::CancelReconnect) => {
                            return BackoffWaitOutcome::CancelledToDisconnected;
                        }
                        Some(SupervisorCmd::Disconnect) | None => {
                            return BackoffWaitOutcome::Shutdown;
                        }
                        Some(SupervisorCmd::Resize { cols, rows }) => {
                            self.cols = cols;
                            self.rows = rows;
                        }
                        Some(SupervisorCmd::UpdateKeepalive { interval, max_missed }) => {
                            self.config.keepalive_interval = interval;
                            self.config.keepalive_max_missed = max_missed;
                        }
                        Some(SupervisorCmd::WriteBytes(_)) => {}
                    }
                }
            }
        }
    }

    async fn wait_while_disconnected(&mut self) -> IdleOutcome {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                SupervisorCmd::ReconnectNow => return IdleOutcome::ReconnectNow,
                SupervisorCmd::Resize { cols, rows } => {
                    self.cols = cols;
                    self.rows = rows;
                }
                SupervisorCmd::UpdateKeepalive {
                    interval,
                    max_missed,
                } => {
                    self.config.keepalive_interval = interval;
                    self.config.keepalive_max_missed = max_missed;
                }
                SupervisorCmd::Disconnect => return IdleOutcome::Shutdown,
                SupervisorCmd::WriteBytes(_)
                | SupervisorCmd::ProbeNow
                | SupervisorCmd::CancelReconnect => {}
            }
        }
        IdleOutcome::Shutdown
    }

    fn clear_transport(&self) {
        self.transport_tx.send_replace(None);
    }

    fn emit_event(&self, event: SupervisorEvent) {
        let _ = self.events_tx.send(event.clone());
        if let Some(tx) = &self.ordered_events_tx {
            let _ = tx.send(event);
        }
    }

    fn enter_disconnected(&mut self, error: Option<String>) {
        self.last_error = error.clone();
        self.publish_status(TransportStatus::Disconnected, None, error.clone());
        self.emit_event(SupervisorEvent::Disconnected { error });
    }

    fn publish_status(
        &mut self,
        status: TransportStatus,
        transport: Option<Arc<SshTransport>>,
        last_error: Option<String>,
    ) {
        let changed = self.current_status != status;
        self.current_status = status.clone();
        self.status_tx.send_replace(status.clone());
        self.transport_tx.send_replace(transport.clone());
        self.ready_tx.send_replace(TransportReadySnapshot {
            epoch: self.epoch,
            status: status.clone(),
            transport,
        });
        if changed {
            self.emit_event(SupervisorEvent::StatusChanged(TransportStatusEvent {
                epoch: self.epoch,
                status,
                last_error,
            }));
        }
    }
}

fn spawn_chain_probe(
    chain: SshTransportChain,
    timeout: Duration,
) -> oneshot::Receiver<ProbeOutcome> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let outcome = run_chain_probe(&chain, timeout).await;
        let _ = tx.send(outcome);
    });
    rx
}

async fn run_chain_probe(chain: &SshTransportChain, timeout: Duration) -> ProbeOutcome {
    if let Some(dead_hop) = chain
        .all_hops_outer_to_inner()
        .find(|hop| hop.is_closed())
    {
        return ProbeOutcome::Dead(SshError::Connection {
            host: dead_hop.host().to_string(),
            port: dead_hop.port(),
            reason: "SSH transport closed".to_string(),
        });
    }

    if !chain.has_jump_hops() {
        return run_probe(chain.target(), timeout).await;
    }

    let target_idx = chain.hop_count() - 1;
    let mut set = tokio::task::JoinSet::new();
    for (idx, hop) in chain.all_hops_outer_to_inner().enumerate() {
        let hop = Arc::clone(hop);
        set.spawn(async move { (idx, run_probe(&hop, timeout).await) });
    }

    let mut timed_out = false;
    let mut target_rtt_ms = 0;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((idx, ProbeOutcome::Alive { rtt_ms })) => {
                if idx == target_idx {
                    target_rtt_ms = rtt_ms;
                }
            }
            Ok((_idx, ProbeOutcome::TimedOut)) => {
                timed_out = true;
            }
            Ok((_idx, ProbeOutcome::Dead(err))) => {
                set.abort_all();
                return ProbeOutcome::Dead(err);
            }
            Err(_) => {
                set.abort_all();
                return ProbeOutcome::Dead(SshError::Disconnected);
            }
        }
    }

    if timed_out {
        ProbeOutcome::TimedOut
    } else {
        ProbeOutcome::Alive {
            rtt_ms: target_rtt_ms,
        }
    }
}

async fn run_probe(transport: &SshTransport, timeout: Duration) -> ProbeOutcome {
    if transport.is_closed() {
        return ProbeOutcome::Dead(SshError::Disconnected);
    }
    let started = Instant::now();
    let req = transport
        .handle()
        .send_global_request("keepalive@openssh.com", &[], true);
    match tokio::time::timeout(timeout, req).await {
        Ok(Ok(_)) | Ok(Err(russh::Error::RequestDenied)) => {
            if transport.is_closed() {
                ProbeOutcome::Dead(SshError::Disconnected)
            } else {
                ProbeOutcome::Alive {
                    rtt_ms: started.elapsed().as_millis() as u64,
                }
            }
        }
        Ok(Err(err)) => ProbeOutcome::Dead(SshError::Russh(err)),
        Err(_) => ProbeOutcome::TimedOut,
    }
}

async fn poll_probe(
    slot: &mut Option<(bool, Pin<Box<oneshot::Receiver<ProbeOutcome>>>)>,
) -> (bool, ProbeOutcome) {
    let (is_fast, rx) = slot.as_mut().expect("probe slot guarded by is_some");
    let fast = *is_fast;
    let outcome = match rx.await {
        Ok(outcome) => outcome,
        Err(_) => ProbeOutcome::Dead(SshError::Disconnected),
    };
    (fast, outcome)
}

async fn sleep_until_opt(deadline: Option<TokioInstant>) {
    match deadline {
        Some(dl) => tokio::time::sleep_until(dl).await,
        None => std::future::pending::<()>().await,
    }
}

async fn wait_jump_notify(notify: Option<&Arc<Notify>>) {
    match notify {
        Some(n) => n.notified().await,
        None => std::future::pending::<()>().await,
    }
}

fn wake_check_period(config: &SupervisorConfig) -> Duration {
    if config.keepalive_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        (config.keepalive_interval / 4).clamp(Duration::from_millis(15), Duration::from_secs(1))
    }
}

fn is_clock_jump(
    earlier: SupervisorInstant,
    later: SupervisorInstant,
    keepalive_interval: Duration,
) -> bool {
    let threshold = if keepalive_interval.is_zero() {
        Duration::from_secs(DEFAULT_KEEPALIVE_INTERVAL_SECS)
    } else {
        keepalive_interval
    };
    let wall_delta = later
        .wall
        .duration_since(earlier.wall)
        .unwrap_or(Duration::ZERO);
    let mono_delta = later.monotonic.saturating_duration_since(earlier.monotonic);
    if wall_delta <= threshold {
        return false;
    }
    wall_delta.saturating_sub(mono_delta) > threshold / 2
}

fn schedule_supervisor_login_dispatches(
    cmd_tx: &mpsc::UnboundedSender<SupervisorCmd>,
    dispatches: Vec<termewnal_pty::LoginScriptDispatch>,
) {
    if dispatches.is_empty() {
        return;
    }
    let mut iter = dispatches.into_iter().peekable();
    while let Some(next) = iter.peek() {
        if !next.delay.is_zero() {
            break;
        }
        let dispatch = iter.next().expect("peeked dispatch");
        let _ = cmd_tx.send(SupervisorCmd::WriteBytes(dispatch.bytes.to_vec()));
    }
    let remaining: Vec<_> = iter.collect();
    if remaining.is_empty() {
        return;
    }
    let cmd_tx = cmd_tx.clone();
    tokio::spawn(async move {
        for dispatch in remaining {
            if !dispatch.delay.is_zero() {
                tokio::time::sleep(dispatch.delay).await;
            }
            if cmd_tx
                .send(SupervisorCmd::WriteBytes(dispatch.bytes.to_vec()))
                .is_err()
            {
                break;
            }
        }
    });
}
