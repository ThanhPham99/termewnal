use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::RngExt;
use russh::{Channel, ChannelOpenFailure};
use russh::client::Msg as ClientMsg;
use termewnal_core::{
    DEFAULT_TUNNEL_HOLD_GRACE_SECS, TunnelDirection, TunnelRule, vault::SecretStore,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant as TokioInstant;
use tracing::{debug, warn};

use crate::TransportStatus;
use crate::auth::{AgentConnector, SshInteractionHandler};
use crate::client::{
    ForwardedTcpIpRequest, RemoteForwardRegistration, SshConnectConfig, SshTransport,
};
use crate::error::SshError;
use crate::supervisor::{
    DEFAULT_BACKOFF_MAX, DEFAULT_BACKOFF_MIN, DefaultTransportConnector, SupervisorConfig,
    SystemTimeSource, TimeSource, TransportConnector, TransportReadySnapshot,
    compute_reconnect_backoff,
};

/// Default grace timeout (`10s`) while holding local TCP client sockets during `Transport` recovery.
pub const DEFAULT_TUNNEL_HOLD_GRACE: Duration =
    Duration::from_secs(DEFAULT_TUNNEL_HOLD_GRACE_SECS);

const SOCKS5_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const STANDALONE_LIVENESS_POLL: Duration = Duration::from_millis(50);
const CANCEL_TCPIP_FORWARD_TIMEOUT: Duration = Duration::from_secs(3);

static NEXT_TUNNEL_MANAGER_ID: AtomicU64 = AtomicU64::new(1);

/// Exponential backoff bounds for Remote (`-R`) `tcpip_forward` `RequestDenied` retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteForwardBackoffConfig {
    pub min: Duration,
    pub max: Duration,
}

impl Default for RemoteForwardBackoffConfig {
    fn default() -> Self {
        Self {
            min: DEFAULT_BACKOFF_MIN,
            max: DEFAULT_BACKOFF_MAX,
        }
    }
}

/// Runtime lifecycle state of a managed SSH `Tunnel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Stopped,
    Starting,
    Active,
    Holding,
    Reconnecting,
    Retrying {
        attempt: u32,
        next_retry_secs: u64,
        reason: String,
    },
    Error(String),
}

impl TunnelState {
    /// Returns `true` when the `Tunnel` is actively running or recovering in the background.
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            Self::Starting
                | Self::Active
                | Self::Holding
                | Self::Reconnecting
                | Self::Retrying { .. }
        )
    }

    /// Returns a concise human-readable label for this `TunnelState`.
    pub fn label(&self) -> &str {
        match self {
            Self::Stopped => "Stopped",
            Self::Starting => "Starting",
            Self::Active => "Active",
            Self::Holding => "Holding",
            Self::Reconnecting => "Reconnecting",
            Self::Retrying { .. } => "Retrying",
            Self::Error(_) => "Error",
        }
    }
}

/// Telemetry snapshot of a single managed `Tunnel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSnapshot {
    pub id: String,
    pub name: String,
    pub profile_id: Option<String>,
    pub session_id: Option<u64>,
    pub direction: TunnelDirection,
    pub bind_host: String,
    pub bind_port: u16,
    pub bound_addr: Option<SocketAddr>,
    pub remote_bound_port: Option<u16>,
    pub target_host: Option<String>,
    pub target_port: Option<u16>,
    pub standalone: bool,
    pub allow_public_bind: bool,
    pub hold_grace_secs: u64,
    pub state: TunnelState,
    pub held_clients: usize,
    pub active_connections: usize,
    pub last_error: Option<String>,
}

/// Aggregated telemetry snapshot published by [`TunnelManager`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TunnelManagerSnapshot {
    pub tunnels: Vec<TunnelSnapshot>,
    pub active_count: usize,
    pub holding_count: usize,
    pub reconnecting_count: usize,
    pub retrying_count: usize,
    pub error_count: usize,
    pub active_standalone_count: usize,
}

/// Connection configuration and boundary factories for a `Standalone` background `Tunnel`.
#[derive(Clone)]
pub struct StandaloneTransportSpec {
    pub connect_config: SshConnectConfig,
    pub supervisor_config: SupervisorConfig,
    pub connector: Arc<dyn TransportConnector>,
    pub time_source: Arc<dyn TimeSource>,
}

impl StandaloneTransportSpec {
    /// Builds a [`StandaloneTransportSpec`] backed by [`DefaultTransportConnector`] and OS clock.
    pub fn new(
        connect_config: SshConnectConfig,
        supervisor_config: SupervisorConfig,
        vault: Option<Arc<dyn SecretStore>>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Self {
        Self {
            connect_config,
            supervisor_config,
            connector: Arc::new(DefaultTransportConnector::new(
                vault,
                interaction,
                agent_connector,
            )),
            time_source: Arc::new(SystemTimeSource),
        }
    }

    /// Builds a [`StandaloneTransportSpec`] with injectable [`TransportConnector`] and [`TimeSource`] boundaries.
    pub fn with_connector(
        connect_config: SshConnectConfig,
        supervisor_config: SupervisorConfig,
        connector: Arc<dyn TransportConnector>,
        time_source: Arc<dyn TimeSource>,
    ) -> Self {
        Self {
            connect_config,
            supervisor_config,
            connector,
            time_source,
        }
    }
}

#[derive(Clone)]
enum TunnelForwardMode {
    Local {
        target_host: String,
        target_port: u16,
    },
    Remote {
        remote_bind_addr: String,
        remote_bind_port: u16,
        target_host: String,
        target_port: u16,
    },
    DynamicSocks5,
}

struct ValidatedRule {
    bind_host_raw: String,
    bind_host_socket: String,
    mode: TunnelForwardMode,
    hold_grace: Duration,
    hold_grace_secs: u64,
}

fn validate_tunnel_rule(rule: &TunnelRule) -> Result<ValidatedRule, SshError> {
    let bind_host_raw = rule.effective_bind_host().to_string();
    if rule.requires_public_bind_opt_in() {
        return Err(SshError::TunnelPublicBindDenied {
            tunnel_name: rule.name.clone(),
            bind_host: bind_host_raw,
            bind_port: rule.bind_port,
        });
    }

    let bind_host_socket = bind_host_raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&bind_host_raw)
        .to_string();

    let mode = match rule.direction {
        TunnelDirection::Local => {
            let (target_host, target_port) = validate_target_host_port(rule, "local tunnel")?;
            TunnelForwardMode::Local {
                target_host,
                target_port,
            }
        }
        TunnelDirection::Remote => {
            let (target_host, target_port) = validate_target_host_port(rule, "remote tunnel")?;
            TunnelForwardMode::Remote {
                remote_bind_addr: bind_host_socket.clone(),
                remote_bind_port: rule.bind_port,
                target_host,
                target_port,
            }
        }
        TunnelDirection::Dynamic => TunnelForwardMode::DynamicSocks5,
    };

    let hold_grace = rule.effective_hold_grace();
    let hold_grace_secs = rule
        .hold_grace_secs
        .unwrap_or(DEFAULT_TUNNEL_HOLD_GRACE_SECS);

    Ok(ValidatedRule {
        bind_host_raw,
        bind_host_socket,
        mode,
        hold_grace,
        hold_grace_secs,
    })
}

fn validate_target_host_port(rule: &TunnelRule, kind_label: &str) -> Result<(String, u16), SshError> {
    let target_host = rule
        .target_host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SshError::TunnelConfig {
            tunnel_name: rule.name.clone(),
            reason: format!("{kind_label} requires a non-empty target_host"),
        })?
        .to_string();
    let target_port = rule.target_port.filter(|&p| p > 0).ok_or_else(|| {
        SshError::TunnelConfig {
            tunnel_name: rule.name.clone(),
            reason: format!("{kind_label} requires a non-zero target_port"),
        }
    })?;
    Ok((target_host, target_port))
}

#[derive(Debug, Clone)]
struct RetryTelemetry {
    attempt: u32,
    next_retry_secs: u64,
    reason: String,
}

enum StandaloneCmd {
    ReconnectNow,
    Stop,
}

struct ManagedTunnel {
    snapshot: TunnelSnapshot,
    transport_status: TransportStatus,
    remote_bound: bool,
    retrying: Option<RetryTelemetry>,
    fatal_error: Option<String>,
    task_gen: u64,
    stop_tx: Option<watch::Sender<bool>>,
    retry_now_tx: Option<mpsc::UnboundedSender<()>>,
    standalone_cmd_tx: Option<mpsc::UnboundedSender<StandaloneCmd>>,
    tunnel_done_rx: Option<watch::Receiver<bool>>,
    driver_done_rx: Option<watch::Receiver<bool>>,
}

struct TunnelManagerInner {
    instance_id: u64,
    next_gen: AtomicU64,
    remote_backoff: Mutex<RemoteForwardBackoffConfig>,
    registry: Mutex<BTreeMap<String, ManagedTunnel>>,
    snapshot_tx: watch::Sender<TunnelManagerSnapshot>,
}

impl TunnelManagerInner {
    fn remote_backoff(&self) -> RemoteForwardBackoffConfig {
        *self
            .remote_backoff
            .lock()
            .expect("remote backoff mutex poisoned")
    }

    fn publish(&self, registry: &BTreeMap<String, ManagedTunnel>) {
        let mut snap = TunnelManagerSnapshot::default();
        for entry in registry.values() {
            let item = &entry.snapshot;
            match item.state {
                TunnelState::Active => snap.active_count += 1,
                TunnelState::Holding => snap.holding_count += 1,
                TunnelState::Reconnecting => snap.reconnecting_count += 1,
                TunnelState::Retrying { .. } => snap.retrying_count += 1,
                TunnelState::Error(_) => snap.error_count += 1,
                TunnelState::Stopped | TunnelState::Starting => {}
            }
            if item.standalone && item.state.is_running() {
                snap.active_standalone_count += 1;
            }
            snap.tunnels.push(item.clone());
        }
        self.snapshot_tx.send_replace(snap);
    }

    fn update_entry<F>(&self, id: &str, task_gen: u64, f: F)
    where
        F: FnOnce(&mut ManagedTunnel),
    {
        let mut guard = self.registry.lock().expect("tunnel registry poisoned");
        if let Some(entry) = guard.get_mut(id)
            && entry.task_gen == task_gen
        {
            f(entry);
            self.publish(&guard);
        }
    }
}

/// Manages Local (`-L`), Remote (`-R`), and Dynamic SOCKS5 (`-D`) `Tunnel` lifecycles (both
/// `Session`-bound and `Standalone`), preserving local TCP listeners across transient `Transport`
/// drops and retrying Remote `RequestDenied` failures with exponential backoff.
#[derive(Clone)]
pub struct TunnelManager {
    inner: Arc<TunnelManagerInner>,
}

impl Default for TunnelManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelManager {
    /// Creates an empty [`TunnelManager`] with default Remote (`-R`) `RequestDenied` backoff (`1s..=30s`).
    pub fn new() -> Self {
        Self::with_remote_backoff(RemoteForwardBackoffConfig::default())
    }

    /// Creates an empty [`TunnelManager`] with explicit Remote (`-R`) `RequestDenied` backoff bounds.
    pub fn with_remote_backoff(remote_backoff: RemoteForwardBackoffConfig) -> Self {
        let (snapshot_tx, _) = watch::channel(TunnelManagerSnapshot::default());
        Self {
            inner: Arc::new(TunnelManagerInner {
                instance_id: NEXT_TUNNEL_MANAGER_ID.fetch_add(1, Ordering::Relaxed),
                next_gen: AtomicU64::new(1),
                remote_backoff: Mutex::new(remote_backoff),
                registry: Mutex::new(BTreeMap::new()),
                snapshot_tx,
            }),
        }
    }

    /// Updates Remote (`-R`) `RequestDenied` exponential backoff bounds (useful in tests and config reloads).
    pub fn set_remote_backoff(&self, remote_backoff: RemoteForwardBackoffConfig) {
        *self
            .inner
            .remote_backoff
            .lock()
            .expect("remote backoff mutex poisoned") = remote_backoff;
    }

    /// Returns a process-unique instance identifier for subscription stream keys.
    pub fn instance_id(&self) -> u64 {
        self.inner.instance_id
    }

    /// Subscribes to live [`TunnelManagerSnapshot`] updates.
    pub fn subscribe(&self) -> watch::Receiver<TunnelManagerSnapshot> {
        self.inner.snapshot_tx.subscribe()
    }

    /// Returns the latest [`TunnelManagerSnapshot`].
    pub fn snapshot(&self) -> TunnelManagerSnapshot {
        self.inner.snapshot_tx.borrow().clone()
    }

    /// Starts a `Session`-bound `Local` (`-L`), `Remote` (`-R`), or `Dynamic` (`-D`) `Tunnel`
    /// attached to `ready_rx` (typically [`ConnectionSupervisor::subscribe_ready`]).
    ///
    /// Keeps local `TcpListener`s bound across transient `Transport` drops (with `10s` default hold
    /// grace) and handles Remote (`-R`) `tcpip_forward`, `forwarded-tcpip` proxying,
    /// `RequestDenied` exponential backoff, and graceful `cancel_tcpip_forward` teardown.
    pub async fn start_session_tunnel(
        &self,
        session_id: u64,
        profile_id: Option<String>,
        rule: TunnelRule,
        ready_rx: watch::Receiver<TransportReadySnapshot>,
    ) -> Result<SocketAddr, SshError> {
        let validated = validate_tunnel_rule(&rule)?;
        let task_gen = self.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        self.spawn_validated_tunnel(
            task_gen,
            Some(session_id),
            profile_id,
            rule,
            validated,
            ready_rx,
            None,
            None,
            None,
        )
        .await
    }

    /// Starts a `Standalone` background `Local` (`-L`), `Remote` (`-R`), or `Dynamic` (`-D`)
    /// `Tunnel` owning its own headless `Transport` supervisor loop without allocating a PTY shell.
    pub async fn start_standalone_tunnel(
        &self,
        profile_id: Option<String>,
        mut rule: TunnelRule,
        spec: StandaloneTransportSpec,
    ) -> Result<SocketAddr, SshError> {
        rule.standalone = true;
        let validated = validate_tunnel_rule(&rule)?;

        let (ready_tx, ready_rx) = watch::channel(TransportReadySnapshot {
            epoch: 0,
            status: TransportStatus::Connecting,
            transport: None,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        let (tunnel_done_tx, tunnel_done_rx) = watch::channel(false);
        let (driver_done_tx, driver_done_rx) = watch::channel(false);

        let task_gen = self.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        let tunnel_id = rule.id.clone();

        let driver = StandaloneTransportDriver {
            tunnel_id: tunnel_id.clone(),
            task_gen,
            spec,
            ready_tx,
            cmd_rx,
            stop_tx: stop_tx.clone(),
            tunnel_done_rx: tunnel_done_rx.clone(),
            driver_done_tx,
            inner: Arc::clone(&self.inner),
            epoch: 0,
            has_connected_once: false,
            last_error: None,
        };

        let addr = self
            .spawn_validated_tunnel(
                task_gen,
                None,
                profile_id,
                rule,
                validated,
                ready_rx,
                Some(cmd_tx),
                Some((stop_tx, stop_rx, tunnel_done_tx, tunnel_done_rx)),
                Some(driver_done_rx),
            )
            .await?;

        tokio::spawn(driver.run());
        Ok(addr)
    }

    /// Requests an immediate reconnect (or immediate `tcpip_forward` retry when `Retrying`) on a `Tunnel`.
    pub fn reconnect_tunnel(&self, tunnel_id: &str) -> Result<(), SshError> {
        let guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        let Some(entry) = guard.get(tunnel_id) else {
            return Err(SshError::TunnelConfig {
                tunnel_name: tunnel_id.to_string(),
                reason: "tunnel is not active".to_string(),
            });
        };
        if matches!(entry.snapshot.state, TunnelState::Retrying { .. })
            && let Some(retry_tx) = &entry.retry_now_tx
        {
            let _ = retry_tx.send(());
            return Ok(());
        }
        if let Some(tx) = &entry.standalone_cmd_tx {
            let _ = tx.send(StandaloneCmd::ReconnectNow);
            Ok(())
        } else {
            Err(SshError::TunnelConfig {
                tunnel_name: entry.snapshot.name.clone(),
                reason: "session-bound tunnel reconnects via its owning Session".to_string(),
            })
        }
    }

    /// Stops a single running `Tunnel` synchronously in registry state and initiates async teardown.
    pub fn stop_tunnel(&self, tunnel_id: &str) -> bool {
        let (found, wait_rx) = self.stop_tunnel_internal(tunnel_id);
        drop(wait_rx);
        found
    }

    /// Stops a single running `Tunnel` and awaits graceful `cancel_tcpip_forward` and transport teardown.
    pub async fn stop_tunnel_async(&self, tunnel_id: &str) -> bool {
        let (found, wait_rx) = self.stop_tunnel_internal(tunnel_id);
        await_done_receivers(wait_rx).await;
        found
    }

    fn stop_tunnel_internal(&self, tunnel_id: &str) -> (bool, Vec<watch::Receiver<bool>>) {
        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        let Some(entry) = guard.get_mut(tunnel_id) else {
            return (false, Vec::new());
        };
        let receivers = stop_managed_entry(entry);
        self.inner.publish(&guard);
        (true, receivers)
    }

    /// Stops and removes all `Session`-bound (non-`Standalone`) `Tunnel`s attached to `session_id`.
    pub fn stop_session_tunnels(&self, session_id: u64) -> usize {
        let (count, receivers) = self.stop_session_tunnels_internal(session_id);
        drop(receivers);
        count
    }

    /// Stops and removes all `Session`-bound `Tunnel`s for `session_id` and returns a future that
    /// resolves once all `cancel_tcpip_forward` calls have completed.
    pub fn stop_session_tunnels_with_barrier(
        &self,
        session_id: u64,
    ) -> (usize, Pin<Box<dyn Future<Output = ()> + Send>>) {
        let (count, receivers) = self.stop_session_tunnels_internal(session_id);
        (count, Box::pin(await_done_receivers(receivers)))
    }

    /// Stops and removes all `Session`-bound `Tunnel`s for `session_id` and awaits graceful
    /// `cancel_tcpip_forward` teardown before returning.
    pub async fn stop_session_tunnels_async(&self, session_id: u64) -> usize {
        let (count, receivers) = self.stop_session_tunnels_internal(session_id);
        await_done_receivers(receivers).await;
        count
    }

    fn stop_session_tunnels_internal(&self, session_id: u64) -> (usize, Vec<watch::Receiver<bool>>) {
        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        let matching_ids: Vec<String> = guard
            .iter()
            .filter_map(|(id, entry)| {
                (!entry.snapshot.standalone && entry.snapshot.session_id == Some(session_id))
                    .then(|| id.clone())
            })
            .collect();
        let count = matching_ids.len();
        let mut receivers = Vec::new();
        for id in matching_ids {
            if let Some(mut entry) = guard.remove(&id) {
                receivers.extend(stop_managed_entry(&mut entry));
            }
        }
        if count > 0 {
            self.inner.publish(&guard);
        }
        (count, receivers)
    }

    /// Stops every active `Tunnel` (both `Session`-bound and `Standalone`) synchronously in registry state.
    pub fn stop_all(&self) {
        let receivers = self.stop_all_internal();
        drop(receivers);
    }

    /// Stops every active `Tunnel` and returns a future resolving after all `cancel_tcpip_forward`
    /// and standalone `Transport` teardowns complete.
    pub fn stop_all_with_barrier(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let receivers = self.stop_all_internal();
        Box::pin(await_done_receivers(receivers))
    }

    /// Stops every active `Tunnel` and awaits all `cancel_tcpip_forward` and transport teardowns.
    pub async fn stop_all_async(&self) {
        let receivers = self.stop_all_internal();
        await_done_receivers(receivers).await;
    }

    fn stop_all_internal(&self) -> Vec<watch::Receiver<bool>> {
        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        let mut receivers = Vec::new();
        for entry in guard.values_mut() {
            receivers.extend(stop_managed_entry(entry));
        }
        self.inner.publish(&guard);
        receivers
    }

    /// Records a failed `Tunnel` start attempt (e.g. `EADDRINUSE` or rejected bind) in snapshot telemetry.
    pub fn record_start_error(
        &self,
        session_id: Option<u64>,
        profile_id: Option<String>,
        rule: &TunnelRule,
        error: String,
    ) {
        let task_gen = self.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        let hold_grace_secs = rule
            .hold_grace_secs
            .unwrap_or(DEFAULT_TUNNEL_HOLD_GRACE_SECS);
        let snapshot = TunnelSnapshot {
            id: rule.id.clone(),
            name: rule.name.clone(),
            profile_id,
            session_id,
            direction: rule.direction,
            bind_host: rule.effective_bind_host().to_string(),
            bind_port: rule.bind_port,
            bound_addr: None,
            remote_bound_port: None,
            target_host: rule.target_host.clone(),
            target_port: rule.target_port,
            standalone: rule.standalone,
            allow_public_bind: rule.allow_public_bind,
            hold_grace_secs,
            state: TunnelState::Error(error.clone()),
            held_clients: 0,
            active_connections: 0,
            last_error: Some(error.clone()),
        };

        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        if let Some(prev) = guard.get_mut(&rule.id) {
            let _ = stop_managed_entry(prev);
        }
        guard.insert(
            rule.id.clone(),
            ManagedTunnel {
                snapshot,
                transport_status: TransportStatus::Disconnected,
                remote_bound: false,
                retrying: None,
                fatal_error: Some(error),
                task_gen,
                stop_tx: None,
                retry_now_tx: None,
                standalone_cmd_tx: None,
                tunnel_done_rx: None,
                driver_done_rx: None,
            },
        );
        self.inner.publish(&guard);
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    async fn spawn_validated_tunnel(
        &self,
        task_gen: u64,
        session_id: Option<u64>,
        profile_id: Option<String>,
        rule: TunnelRule,
        validated: ValidatedRule,
        ready_rx: watch::Receiver<TransportReadySnapshot>,
        standalone_cmd_tx: Option<mpsc::UnboundedSender<StandaloneCmd>>,
        stop_pair: Option<(
            watch::Sender<bool>,
            watch::Receiver<bool>,
            watch::Sender<bool>,
            watch::Receiver<bool>,
        )>,
        driver_done_rx: Option<watch::Receiver<bool>>,
    ) -> Result<SocketAddr, SshError> {
        let (stop_tx, stop_rx, tunnel_done_tx, tunnel_done_rx) = stop_pair.unwrap_or_else(|| {
            let (stx, srx) = watch::channel(false);
            let (dtx, drx) = watch::channel(false);
            (stx, srx, dtx, drx)
        });
        let initial_status = ready_rx.borrow().status.clone();
        let initial_state = compute_tunnel_state(
            rule.direction,
            &initial_status,
            0,
            false,
            None,
            None,
        );
        let tunnel_id = rule.id.clone();

        match validated.mode {
            TunnelForwardMode::Local { .. } | TunnelForwardMode::DynamicSocks5 => {
                let listener =
                    TcpListener::bind((validated.bind_host_socket.as_str(), rule.bind_port))
                        .await?;
                let bound_addr = listener.local_addr()?;

                let snapshot = TunnelSnapshot {
                    id: tunnel_id.clone(),
                    name: rule.name.clone(),
                    profile_id,
                    session_id,
                    direction: rule.direction,
                    bind_host: validated.bind_host_raw,
                    bind_port: rule.bind_port,
                    bound_addr: Some(bound_addr),
                    remote_bound_port: None,
                    target_host: rule.target_host.clone(),
                    target_port: rule.target_port,
                    standalone: rule.standalone,
                    allow_public_bind: rule.allow_public_bind,
                    hold_grace_secs: validated.hold_grace_secs,
                    state: initial_state,
                    held_clients: 0,
                    active_connections: 0,
                    last_error: None,
                };

                self.insert_managed_tunnel(
                    tunnel_id.clone(),
                    ManagedTunnel {
                        snapshot,
                        transport_status: initial_status,
                        remote_bound: false,
                        retrying: None,
                        fatal_error: None,
                        task_gen,
                        stop_tx: Some(stop_tx),
                        retry_now_tx: None,
                        standalone_cmd_tx,
                        tunnel_done_rx: Some(tunnel_done_rx),
                        driver_done_rx,
                    },
                );

                tokio::spawn(run_listener_tunnel(
                    Arc::clone(&self.inner),
                    tunnel_id,
                    task_gen,
                    validated.mode,
                    validated.hold_grace,
                    listener,
                    ready_rx,
                    stop_rx,
                    tunnel_done_tx,
                ));

                Ok(bound_addr)
            }
            TunnelForwardMode::Remote {
                remote_bind_addr,
                remote_bind_port,
                target_host,
                target_port,
            } => {
                let placeholder_addr = parse_remote_socket_addr(&remote_bind_addr, remote_bind_port)
                    .unwrap_or_else(|| {
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), remote_bind_port)
                    });
                let (retry_now_tx, retry_now_rx) = mpsc::unbounded_channel();

                let snapshot = TunnelSnapshot {
                    id: tunnel_id.clone(),
                    name: rule.name.clone(),
                    profile_id,
                    session_id,
                    direction: rule.direction,
                    bind_host: validated.bind_host_raw,
                    bind_port: rule.bind_port,
                    bound_addr: None,
                    remote_bound_port: None,
                    target_host: Some(target_host.clone()),
                    target_port: Some(target_port),
                    standalone: rule.standalone,
                    allow_public_bind: rule.allow_public_bind,
                    hold_grace_secs: validated.hold_grace_secs,
                    state: initial_state,
                    held_clients: 0,
                    active_connections: 0,
                    last_error: None,
                };

                self.insert_managed_tunnel(
                    tunnel_id.clone(),
                    ManagedTunnel {
                        snapshot,
                        transport_status: initial_status,
                        remote_bound: false,
                        retrying: None,
                        fatal_error: None,
                        task_gen,
                        stop_tx: Some(stop_tx),
                        retry_now_tx: Some(retry_now_tx),
                        standalone_cmd_tx,
                        tunnel_done_rx: Some(tunnel_done_rx),
                        driver_done_rx,
                    },
                );

                tokio::spawn(run_remote_tunnel(
                    Arc::clone(&self.inner),
                    tunnel_id,
                    task_gen,
                    remote_bind_addr,
                    remote_bind_port,
                    target_host,
                    target_port,
                    ready_rx,
                    stop_rx,
                    retry_now_rx,
                    tunnel_done_tx,
                ));

                Ok(placeholder_addr)
            }
        }
    }

    fn insert_managed_tunnel(&self, tunnel_id: String, entry: ManagedTunnel) {
        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        if let Some(prev) = guard.get_mut(&tunnel_id) {
            let _ = stop_managed_entry(prev);
        }
        guard.insert(tunnel_id, entry);
        self.inner.publish(&guard);
    }
}

async fn await_done_receivers(receivers: Vec<watch::Receiver<bool>>) {
    for mut rx in receivers {
        let _ = tokio::time::timeout(
            CANCEL_TCPIP_FORWARD_TIMEOUT + Duration::from_secs(1),
            rx.wait_for(|&done| done),
        )
        .await;
    }
}

fn stop_managed_entry(entry: &mut ManagedTunnel) -> Vec<watch::Receiver<bool>> {
    if let Some(stop_tx) = entry.stop_tx.take() {
        let _ = stop_tx.send(true);
    }
    if let Some(cmd_tx) = entry.standalone_cmd_tx.take() {
        let _ = cmd_tx.send(StandaloneCmd::Stop);
    }
    entry.retry_now_tx.take();
    entry.remote_bound = false;
    entry.retrying = None;
    entry.snapshot.bound_addr = None;
    entry.snapshot.remote_bound_port = None;
    entry.snapshot.held_clients = 0;
    entry.snapshot.active_connections = 0;
    if entry.fatal_error.is_none() {
        entry.snapshot.state = TunnelState::Stopped;
    }
    let mut waiters = Vec::new();
    if let Some(rx) = entry.tunnel_done_rx.take() {
        waiters.push(rx);
    }
    if let Some(rx) = entry.driver_done_rx.take() {
        waiters.push(rx);
    }
    waiters
}

fn compute_tunnel_state(
    direction: TunnelDirection,
    transport_status: &TransportStatus,
    held_clients: usize,
    remote_bound: bool,
    retrying: Option<&RetryTelemetry>,
    fatal_error: Option<&str>,
) -> TunnelState {
    if let Some(err) = fatal_error {
        return TunnelState::Error(err.to_string());
    }
    if let Some(retry) = retrying {
        return TunnelState::Retrying {
            attempt: retry.attempt,
            next_retry_secs: retry.next_retry_secs,
            reason: retry.reason.clone(),
        };
    }
    if held_clients > 0 {
        return TunnelState::Holding;
    }
    match transport_status {
        TransportStatus::Connected { .. } => {
            if direction == TunnelDirection::Remote && !remote_bound {
                TunnelState::Starting
            } else {
                TunnelState::Active
            }
        }
        TransportStatus::Degraded { .. } | TransportStatus::Reconnecting { .. } => {
            TunnelState::Reconnecting
        }
        TransportStatus::Connecting => TunnelState::Starting,
        TransportStatus::Disconnected => TunnelState::Stopped,
    }
}

fn refresh_entry_state(entry: &mut ManagedTunnel) {
    if !matches!(entry.transport_status, TransportStatus::Connected { .. }) {
        entry.remote_bound = false;
        entry.retrying = None;
        if entry.snapshot.direction == TunnelDirection::Remote {
            entry.snapshot.bound_addr = None;
            entry.snapshot.remote_bound_port = None;
        }
    }
    entry.snapshot.state = compute_tunnel_state(
        entry.snapshot.direction,
        &entry.transport_status,
        entry.snapshot.held_clients,
        entry.remote_bound,
        entry.retrying.as_ref(),
        entry.fatal_error.as_deref(),
    );
}

struct DoneSignal(watch::Sender<bool>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send_replace(true);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_listener_tunnel(
    inner: Arc<TunnelManagerInner>,
    tunnel_id: String,
    task_gen: u64,
    mode: TunnelForwardMode,
    hold_grace: Duration,
    listener: TcpListener,
    mut ready_rx: watch::Receiver<TransportReadySnapshot>,
    mut stop_rx: watch::Receiver<bool>,
    tunnel_done_tx: watch::Sender<bool>,
) {
    let _done = DoneSignal(tunnel_done_tx);
    loop {
        if *stop_rx.borrow_and_update() {
            break;
        }

        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow_and_update() {
                    break;
                }
            }
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let snap = ready_rx.borrow_and_update().clone();
                let status = snap.status.clone();
                inner.update_entry(&tunnel_id, task_gen, |entry| {
                    entry.transport_status = status.clone();
                    refresh_entry_state(entry);
                });
                if matches!(status, TransportStatus::Disconnected) {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((client_stream, peer_addr)) => {
                        let _ = client_stream.set_nodelay(true);
                        let client_inner = Arc::clone(&inner);
                        let client_id = tunnel_id.clone();
                        let client_mode = mode.clone();
                        let client_ready = ready_rx.clone();
                        let client_stop = stop_rx.clone();
                        tokio::spawn(async move {
                            handle_local_client(
                                client_inner,
                                client_id,
                                task_gen,
                                client_mode,
                                hold_grace,
                                client_stream,
                                peer_addr,
                                client_ready,
                                client_stop,
                            )
                            .await;
                        });
                    }
                    Err(err) => {
                        warn!(tunnel_id = %tunnel_id, error = %err, "tunnel accept error");
                        inner.update_entry(&tunnel_id, task_gen, |entry| {
                            entry.fatal_error = Some(err.to_string());
                            entry.snapshot.last_error = Some(err.to_string());
                            entry.snapshot.bound_addr = None;
                            refresh_entry_state(entry);
                        });
                        return;
                    }
                }
            }
        }
    }

    inner.update_entry(&tunnel_id, task_gen, |entry| {
        if let Some(cmd_tx) = entry.standalone_cmd_tx.take() {
            let _ = cmd_tx.send(StandaloneCmd::Stop);
        }
        entry.stop_tx.take();
        entry.snapshot.bound_addr = None;
        entry.snapshot.held_clients = 0;
        entry.snapshot.active_connections = 0;
        entry.snapshot.state = if entry.fatal_error.is_none() {
            TunnelState::Stopped
        } else {
            entry.snapshot.state.clone()
        };
    });
}

fn parse_remote_socket_addr(bind_addr: &str, port: u16) -> Option<SocketAddr> {
    let trimmed = bind_addr
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(bind_addr.trim());
    let ip = match trimmed {
        "" | "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        s if s.eq_ignore_ascii_case("localhost") => IpAddr::V4(Ipv4Addr::LOCALHOST),
        s => s.parse::<IpAddr>().ok()?,
    };
    Some(SocketAddr::new(ip, port))
}

fn ceil_secs(duration: Duration) -> u64 {
    if duration.is_zero() {
        0
    } else {
        duration.as_millis().div_ceil(1000) as u64
    }
}

struct ActiveRemoteForward {
    transport: Arc<SshTransport>,
    bind_address: String,
    bound_port: u32,
    registration: Option<RemoteForwardRegistration>,
    inbound_rx: mpsc::UnboundedReceiver<ForwardedTcpIpRequest>,
}

enum RemoteBindOutcome {
    Bound(ActiveRemoteForward),
    RequestDenied,
    TransportError(String),
    StopRequested,
}

enum ActivePumpExit {
    Continue(ActiveRemoteForward),
    Rebind,
    Stop(ActiveRemoteForward),
}

#[allow(clippy::too_many_arguments)]
async fn pump_active_remote_forward(
    inner: &Arc<TunnelManagerInner>,
    tunnel_id: &str,
    task_gen: u64,
    target_host: &str,
    target_port: u16,
    mut act: ActiveRemoteForward,
    ready_rx: &mut watch::Receiver<TransportReadySnapshot>,
    stop_rx: &mut watch::Receiver<bool>,
    retry_now_rx: &mut mpsc::UnboundedReceiver<()>,
) -> ActivePumpExit {
    tokio::select! {
        _ = stop_rx.changed() => {
            if *stop_rx.borrow_and_update() {
                return ActivePumpExit::Stop(act);
            }
        }
        changed = ready_rx.changed() => {
            if changed.is_err() {
                return ActivePumpExit::Stop(act);
            }
        }
        _ = retry_now_rx.recv() => {}
        maybe_req = act.inbound_rx.recv() => {
            match maybe_req {
                Some(req) => {
                    let proxy_inner = Arc::clone(inner);
                    let proxy_id = tunnel_id.to_string();
                    let proxy_host = target_host.to_string();
                    let proxy_stop = stop_rx.clone();
                    tokio::spawn(async move {
                        handle_remote_forwarded_stream(
                            proxy_inner,
                            proxy_id,
                            task_gen,
                            proxy_host,
                            target_port,
                            req,
                            proxy_stop,
                        )
                        .await;
                    });
                }
                None => {
                    drop(act.registration.take());
                    inner.update_entry(tunnel_id, task_gen, |entry| {
                        entry.remote_bound = false;
                        entry.snapshot.bound_addr = None;
                        entry.snapshot.remote_bound_port = None;
                        refresh_entry_state(entry);
                    });
                    return ActivePumpExit::Rebind;
                }
            }
        }
    }
    ActivePumpExit::Continue(act)
}

enum RetryWaitExit {
    RetryNow,
    Reevaluate,
    Stop,
}

#[allow(clippy::too_many_arguments)]
async fn wait_remote_retry_backoff(
    inner: &TunnelManagerInner,
    tunnel_id: &str,
    task_gen: u64,
    retry_attempt: u32,
    reason_str: &str,
    deadline: TokioInstant,
    ready_rx: &mut watch::Receiver<TransportReadySnapshot>,
    stop_rx: &mut watch::Receiver<bool>,
    retry_now_rx: &mut mpsc::UnboundedReceiver<()>,
) -> RetryWaitExit {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return RetryWaitExit::RetryNow,
            _ = tick.tick() => {
                let rem = deadline.saturating_duration_since(TokioInstant::now());
                let rem_secs = ceil_secs(rem);
                inner.update_entry(tunnel_id, task_gen, |entry| {
                    entry.retrying = Some(RetryTelemetry {
                        attempt: retry_attempt,
                        next_retry_secs: rem_secs,
                        reason: reason_str.to_string(),
                    });
                    refresh_entry_state(entry);
                });
            }
            _ = stop_rx.changed() => {
                if *stop_rx.borrow_and_update() {
                    return RetryWaitExit::Stop;
                }
            }
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return RetryWaitExit::Stop;
                }
                return RetryWaitExit::Reevaluate;
            }
            cmd = retry_now_rx.recv() => {
                if cmd.is_some() {
                    return RetryWaitExit::RetryNow;
                }
                return RetryWaitExit::Reevaluate;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_remote_tunnel(
    inner: Arc<TunnelManagerInner>,
    tunnel_id: String,
    task_gen: u64,
    remote_bind_addr: String,
    remote_bind_port: u16,
    target_host: String,
    target_port: u16,
    mut ready_rx: watch::Receiver<TransportReadySnapshot>,
    mut stop_rx: watch::Receiver<bool>,
    mut retry_now_rx: mpsc::UnboundedReceiver<()>,
    tunnel_done_tx: watch::Sender<bool>,
) {
    let _done = DoneSignal(tunnel_done_tx);
    let mut active: Option<ActiveRemoteForward> = None;
    let mut failed_transport: Option<Arc<SshTransport>> = None;
    let mut retry_attempt: u32 = 0;
    let mut next_retry_deadline: Option<TokioInstant> = None;
    let mut retry_reason: Option<String> = None;

    loop {
        if *stop_rx.borrow_and_update() {
            break;
        }

        let snap = ready_rx.borrow_and_update().clone();
        if matches!(snap.status, TransportStatus::Disconnected) {
            break;
        }

        if let Some(mut act) = active.take() {
            let still_same_connected = snap.is_connected()
                && snap
                    .transport
                    .as_ref()
                    .is_some_and(|t| Arc::ptr_eq(t, &act.transport) && !t.is_closed());

            if !still_same_connected {
                drop(act.registration.take());
                retry_attempt = 0;
                next_retry_deadline = None;
                retry_reason = None;
                inner.update_entry(&tunnel_id, task_gen, |entry| {
                    entry.transport_status = snap.status.clone();
                    entry.remote_bound = false;
                    entry.retrying = None;
                    entry.snapshot.bound_addr = None;
                    entry.snapshot.remote_bound_port = None;
                    refresh_entry_state(entry);
                });
                continue;
            }

            match pump_active_remote_forward(
                &inner,
                &tunnel_id,
                task_gen,
                &target_host,
                target_port,
                act,
                &mut ready_rx,
                &mut stop_rx,
                &mut retry_now_rx,
            )
            .await
            {
                ActivePumpExit::Continue(act) => active = Some(act),
                ActivePumpExit::Rebind => {}
                ActivePumpExit::Stop(act) => {
                    active = Some(act);
                    break;
                }
            }
            continue;
        }

        let connected_transport = if snap.is_connected() {
            snap.transport.as_ref().filter(|tr| {
                !tr.is_closed() && failed_transport.as_ref().is_none_or(|ft| !Arc::ptr_eq(ft, tr))
            })
        } else {
            None
        };

        let Some(transport) = connected_transport.cloned() else {
            retry_attempt = 0;
            next_retry_deadline = None;
            retry_reason = None;
            inner.update_entry(&tunnel_id, task_gen, |entry| {
                entry.transport_status = snap.status.clone();
                entry.remote_bound = false;
                entry.retrying = None;
                entry.snapshot.bound_addr = None;
                entry.snapshot.remote_bound_port = None;
                refresh_entry_state(entry);
            });

            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow_and_update() {
                        break;
                    }
                }
                changed = ready_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = retry_now_rx.recv() => {}
            }
            continue;
        };

        if let Some(deadline) = next_retry_deadline
            && TokioInstant::now() < deadline
        {
            let reason_str = retry_reason
                .as_deref()
                .unwrap_or("remote port forward denied (RequestDenied)");
            match wait_remote_retry_backoff(
                &inner,
                &tunnel_id,
                task_gen,
                retry_attempt,
                reason_str,
                deadline,
                &mut ready_rx,
                &mut stop_rx,
                &mut retry_now_rx,
            )
            .await
            {
                RetryWaitExit::RetryNow => {
                    next_retry_deadline = None;
                    continue;
                }
                RetryWaitExit::Reevaluate => continue,
                RetryWaitExit::Stop => break,
            }
        }

        match try_bind_remote_forward(
            transport,
            &remote_bind_addr,
            remote_bind_port,
            &mut stop_rx,
        )
        .await
        {
            RemoteBindOutcome::Bound(bound) => {
                retry_attempt = 0;
                next_retry_deadline = None;
                retry_reason = None;
                failed_transport = None;
                let port_u16 = u16::try_from(bound.bound_port).ok();
                let parsed_addr =
                    port_u16.and_then(|p| parse_remote_socket_addr(&bound.bind_address, p));
                inner.update_entry(&tunnel_id, task_gen, |entry| {
                    entry.transport_status = snap.status.clone();
                    entry.remote_bound = true;
                    entry.retrying = None;
                    entry.fatal_error = None;
                    entry.snapshot.last_error = None;
                    entry.snapshot.remote_bound_port = port_u16;
                    entry.snapshot.bound_addr = parsed_addr;
                    refresh_entry_state(entry);
                });
                active = Some(bound);
            }
            RemoteBindOutcome::RequestDenied => {
                retry_attempt = retry_attempt.saturating_add(1).max(1);
                let backoff = inner.remote_backoff();
                let jitter: f64 = rand::rng().random();
                let delay =
                    compute_reconnect_backoff(retry_attempt, backoff.min, backoff.max, jitter);
                next_retry_deadline = Some(TokioInstant::now() + delay);
                let reason = format!(
                    "remote port {remote_bind_addr}:{remote_bind_port} forward denied (RequestDenied)"
                );
                retry_reason = Some(reason.clone());
                let secs = ceil_secs(delay);
                inner.update_entry(&tunnel_id, task_gen, |entry| {
                    entry.transport_status = snap.status.clone();
                    entry.remote_bound = false;
                    entry.snapshot.remote_bound_port = None;
                    entry.snapshot.bound_addr = None;
                    entry.snapshot.last_error = Some(reason.clone());
                    entry.retrying = Some(RetryTelemetry {
                        attempt: retry_attempt,
                        next_retry_secs: secs,
                        reason,
                    });
                    refresh_entry_state(entry);
                });
            }
            RemoteBindOutcome::TransportError(err_msg) => {
                failed_transport = snap.transport.clone();
                retry_attempt = 0;
                next_retry_deadline = None;
                retry_reason = None;
                inner.update_entry(&tunnel_id, task_gen, |entry| {
                    entry.transport_status = snap.status.clone();
                    entry.remote_bound = false;
                    entry.retrying = None;
                    entry.snapshot.remote_bound_port = None;
                    entry.snapshot.bound_addr = None;
                    entry.snapshot.last_error = Some(err_msg);
                    refresh_entry_state(entry);
                });
            }
            RemoteBindOutcome::StopRequested => {
                break;
            }
        }
    }

    if let Some(mut act) = active.take() {
        drop(act.registration.take());
        if !act.transport.is_closed() {
            let _ = tokio::time::timeout(
                CANCEL_TCPIP_FORWARD_TIMEOUT,
                act.transport
                    .handle()
                    .cancel_tcpip_forward(act.bind_address.as_str(), act.bound_port),
            )
            .await;
        }
    }

    inner.update_entry(&tunnel_id, task_gen, |entry| {
        if let Some(cmd_tx) = entry.standalone_cmd_tx.take() {
            let _ = cmd_tx.send(StandaloneCmd::Stop);
        }
        entry.stop_tx.take();
        entry.retry_now_tx.take();
        entry.remote_bound = false;
        entry.retrying = None;
        entry.snapshot.bound_addr = None;
        entry.snapshot.remote_bound_port = None;
        entry.snapshot.held_clients = 0;
        entry.snapshot.active_connections = 0;
        if entry.fatal_error.is_none() {
            entry.snapshot.state = TunnelState::Stopped;
        }
    });
}

async fn try_bind_remote_forward(
    transport: Arc<SshTransport>,
    remote_bind_addr: &str,
    remote_bind_port: u16,
    stop_rx: &mut watch::Receiver<bool>,
) -> RemoteBindOutcome {
    let req_port = u32::from(remote_bind_port);
    let (reg, inbound_rx) = transport.register_remote_forward(remote_bind_addr, req_port);
    let fwd_transport = Arc::clone(&transport);
    let fwd_fut = fwd_transport
        .handle()
        .tcpip_forward(remote_bind_addr, req_port);
    tokio::pin!(fwd_fut);

    let fwd_res = loop {
        tokio::select! {
            res = &mut fwd_fut => break Some(res),
            _ = stop_rx.changed() => {
                if *stop_rx.borrow_and_update() {
                    break None;
                }
            }
        }
    };

    let Some(res) = fwd_res else {
        // Stopped while `tcpip_forward` was in flight: if it succeeded on the server, cancel cleanly!
        drop(reg);
        if !transport.is_closed()
            && let Ok(Ok(returned_port)) =
                tokio::time::timeout(Duration::from_millis(500), fwd_fut).await
        {
            let effective_port = resolve_bound_remote_port(req_port, returned_port);
            if effective_port > 0 {
                let _ = tokio::time::timeout(
                    CANCEL_TCPIP_FORWARD_TIMEOUT,
                    transport
                        .handle()
                        .cancel_tcpip_forward(remote_bind_addr, effective_port),
                )
                .await;
            }
        }
        return RemoteBindOutcome::StopRequested;
    };

    match res {
        Ok(returned_port) => {
            let effective_port = resolve_bound_remote_port(req_port, returned_port);
            if effective_port == 0 {
                return RemoteBindOutcome::TransportError(
                    "SSH server returned port 0 for remote forward".to_string(),
                );
            }
            reg.set_port(effective_port);
            RemoteBindOutcome::Bound(ActiveRemoteForward {
                transport,
                bind_address: remote_bind_addr.to_string(),
                bound_port: effective_port,
                registration: Some(reg),
                inbound_rx,
            })
        }
        Err(russh::Error::RequestDenied) => RemoteBindOutcome::RequestDenied,
        Err(err) => RemoteBindOutcome::TransportError(err.to_string()),
    }
}

fn resolve_bound_remote_port(requested_port: u32, returned_port: u32) -> u32 {
    if requested_port == 0 {
        returned_port
    } else if returned_port == 0 {
        requested_port
    } else {
        returned_port
    }
}

async fn handle_remote_forwarded_stream(
    inner: Arc<TunnelManagerInner>,
    tunnel_id: String,
    task_gen: u64,
    target_host: String,
    target_port: u16,
    req: ForwardedTcpIpRequest,
    mut stop_rx: watch::Receiver<bool>,
) {
    if *stop_rx.borrow_and_update() {
        req.reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        return;
    }

    let mut local_tcp = match TcpStream::connect((target_host.as_str(), target_port)).await {
        Ok(stream) => stream,
        Err(err) => {
            debug!(
                tunnel_id = %tunnel_id,
                target = %format!("{target_host}:{target_port}"),
                error = %err,
                "remote tunnel failed connecting to local target"
            );
            req.reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return;
        }
    };
    let _ = local_tcp.set_nodelay(true);

    req.reply.accept().await;
    let _conn_guard = ActiveConnGuard::acquire(inner, tunnel_id, task_gen);
    let mut ssh_stream = req.channel.into_stream();

    tokio::select! {
        res = tokio::io::copy_bidirectional(&mut local_tcp, &mut ssh_stream) => {
            if let Err(err) = res {
                debug!(error = %err, "remote tunnel stream closed");
            }
        }
        _ = stop_rx.changed() => {
            if *stop_rx.borrow_and_update() {
                drop(ssh_stream);
                reset_local_client(local_tcp).await;
            }
        }
    }
}

struct HeldGuard {
    inner: Arc<TunnelManagerInner>,
    tunnel_id: String,
    task_gen: u64,
    active: bool,
}

impl HeldGuard {
    fn new(inner: Arc<TunnelManagerInner>, tunnel_id: String, task_gen: u64) -> Self {
        Self {
            inner,
            tunnel_id,
            task_gen,
            active: false,
        }
    }

    fn set_held(&mut self, held: bool) {
        if self.active == held {
            return;
        }
        self.active = held;
        self.inner.update_entry(&self.tunnel_id, self.task_gen, |entry| {
            if held {
                entry.snapshot.held_clients = entry.snapshot.held_clients.saturating_add(1);
            } else {
                entry.snapshot.held_clients = entry.snapshot.held_clients.saturating_sub(1);
            }
            refresh_entry_state(entry);
        });
    }
}

impl Drop for HeldGuard {
    fn drop(&mut self) {
        if self.active {
            self.set_held(false);
        }
    }
}

struct ActiveConnGuard {
    inner: Arc<TunnelManagerInner>,
    tunnel_id: String,
    task_gen: u64,
}

impl ActiveConnGuard {
    fn acquire(inner: Arc<TunnelManagerInner>, tunnel_id: String, task_gen: u64) -> Self {
        inner.update_entry(&tunnel_id, task_gen, |entry| {
            entry.snapshot.active_connections = entry.snapshot.active_connections.saturating_add(1);
        });
        Self {
            inner,
            tunnel_id,
            task_gen,
        }
    }
}

impl Drop for ActiveConnGuard {
    fn drop(&mut self) {
        self.inner.update_entry(&self.tunnel_id, self.task_gen, |entry| {
            entry.snapshot.active_connections = entry.snapshot.active_connections.saturating_sub(1);
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_local_client(
    inner: Arc<TunnelManagerInner>,
    tunnel_id: String,
    task_gen: u64,
    mode: TunnelForwardMode,
    hold_grace: Duration,
    mut local_tcp: TcpStream,
    peer_addr: SocketAddr,
    mut ready_rx: watch::Receiver<TransportReadySnapshot>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let hold_deadline = TokioInstant::now() + hold_grace;
    let mut held_guard = HeldGuard::new(Arc::clone(&inner), tunnel_id.clone(), task_gen);

    let Some(first_transport) = wait_for_connected_transport(
        &mut ready_rx,
        &mut stop_rx,
        None,
        hold_deadline,
        &mut held_guard,
    )
    .await
    else {
        reset_local_client(local_tcp).await;
        return;
    };

    held_guard.set_held(false);

    let peer_ip = peer_addr.ip().to_string();
    let peer_port = u32::from(peer_addr.port());

    let ssh_channel = match mode {
        TunnelForwardMode::Local {
            target_host,
            target_port,
        } => {
            open_direct_tcpip_with_hold_retry(
                first_transport,
                &target_host,
                target_port,
                &peer_ip,
                peer_port,
                &mut ready_rx,
                &mut stop_rx,
                hold_deadline,
                &mut held_guard,
            )
            .await
        }
        TunnelForwardMode::DynamicSocks5 => {
            let handshake_res = tokio::time::timeout(
                SOCKS5_HANDSHAKE_TIMEOUT,
                negotiate_socks5_connect(&mut local_tcp),
            )
            .await;
            let Ok(Ok((dst_host, dst_port))) = handshake_res else {
                reset_local_client(local_tcp).await;
                return;
            };

            let ch = open_direct_tcpip_with_hold_retry(
                first_transport,
                &dst_host,
                dst_port,
                &peer_ip,
                peer_port,
                &mut ready_rx,
                &mut stop_rx,
                hold_deadline,
                &mut held_guard,
            )
            .await;

            match ch {
                Ok(channel) => {
                    if send_socks5_reply(&mut local_tcp, 0x00).await.is_err() {
                        return;
                    }
                    Ok(channel)
                }
                Err(DirectOpenError::RemoteRejected) => {
                    let _ = send_socks5_reply(&mut local_tcp, 0x05).await;
                    reset_local_client(local_tcp).await;
                    return;
                }
                Err(DirectOpenError::TransportReset) => Err(DirectOpenError::TransportReset),
            }
        }
        TunnelForwardMode::Remote { .. } => {
            reset_local_client(local_tcp).await;
            return;
        }
    };

    held_guard.set_held(false);

    let Ok(channel) = ssh_channel else {
        reset_local_client(local_tcp).await;
        return;
    };

    let _conn_guard = ActiveConnGuard::acquire(inner, tunnel_id, task_gen);
    let mut ssh_stream = channel.into_stream();

    tokio::select! {
        res = tokio::io::copy_bidirectional(&mut local_tcp, &mut ssh_stream) => {
            if let Err(err) = res {
                debug!(error = %err, "tunnel stream closed");
            }
        }
        _ = stop_rx.changed() => {
            if *stop_rx.borrow_and_update() {
                drop(ssh_stream);
                reset_local_client(local_tcp).await;
            }
        }
    }
}

async fn reset_local_client(mut stream: TcpStream) {
    let _ = stream.shutdown().await;
    drop(stream);
}

enum DirectOpenError {
    RemoteRejected,
    TransportReset,
}

#[allow(clippy::too_many_arguments)]
async fn open_direct_tcpip_with_hold_retry(
    mut transport: Arc<SshTransport>,
    target_host: &str,
    target_port: u16,
    peer_ip: &str,
    peer_port: u32,
    ready_rx: &mut watch::Receiver<TransportReadySnapshot>,
    stop_rx: &mut watch::Receiver<bool>,
    hold_deadline: TokioInstant,
    held_guard: &mut HeldGuard,
) -> Result<Channel<ClientMsg>, DirectOpenError> {
    loop {
        if *stop_rx.borrow_and_update() {
            return Err(DirectOpenError::TransportReset);
        }
        let open_res = transport
            .handle()
            .channel_open_direct_tcpip(target_host, u32::from(target_port), peer_ip, peer_port)
            .await;

        match open_res {
            Ok(channel) => {
                held_guard.set_held(false);
                return Ok(channel);
            }
            Err(russh::Error::ChannelOpenFailure(_)) if !transport.is_closed() => {
                return Err(DirectOpenError::RemoteRejected);
            }
            Err(err) => {
                debug!(error = %err, "direct-tcpip failed during transport transition; waiting in hold window");
                let Some(next) = wait_for_connected_transport(
                    ready_rx,
                    stop_rx,
                    Some(&transport),
                    hold_deadline,
                    held_guard,
                )
                .await
                else {
                    return Err(DirectOpenError::TransportReset);
                };
                transport = next;
            }
        }
    }
}

async fn wait_for_connected_transport(
    ready_rx: &mut watch::Receiver<TransportReadySnapshot>,
    stop_rx: &mut watch::Receiver<bool>,
    avoid_transport: Option<&Arc<SshTransport>>,
    hold_deadline: TokioInstant,
    held_guard: &mut HeldGuard,
) -> Option<Arc<SshTransport>> {
    {
        if *stop_rx.borrow_and_update() {
            return None;
        }
        let snap = ready_rx.borrow_and_update().clone();
        if let Some(tr) = usable_transport(&snap, avoid_transport) {
            return Some(tr);
        }
        if matches!(snap.status, TransportStatus::Disconnected) && avoid_transport.is_none() {
            return None;
        }
    }

    if TokioInstant::now() >= hold_deadline {
        return None;
    }

    held_guard.set_held(true);

    loop {
        tokio::select! {
            () = tokio::time::sleep_until(hold_deadline) => {
                return None;
            }
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow_and_update() {
                    return None;
                }
            }
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return None;
                }
                let snap = ready_rx.borrow_and_update().clone();
                if let Some(tr) = usable_transport(&snap, avoid_transport) {
                    return Some(tr);
                }
                if matches!(snap.status, TransportStatus::Disconnected) {
                    return None;
                }
            }
        }
    }
}

fn usable_transport(
    snap: &TransportReadySnapshot,
    avoid_transport: Option<&Arc<SshTransport>>,
) -> Option<Arc<SshTransport>> {
    if !snap.is_connected() {
        return None;
    }
    let tr = snap.transport.as_ref()?;
    if tr.is_closed() {
        return None;
    }
    if avoid_transport.is_some_and(|prev| Arc::ptr_eq(prev, tr)) {
        return None;
    }
    Some(Arc::clone(tr))
}

async fn negotiate_socks5_connect(stream: &mut TcpStream) -> Result<(String, u16), ()> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.map_err(|_| ())?;
    if header[0] != 0x05 || header[1] == 0 {
        return Err(());
    }

    let mut methods = vec![0u8; usize::from(header[1])];
    stream.read_exact(&mut methods).await.map_err(|_| ())?;
    if !methods.contains(&0x00) {
        let _ = stream.write_all(&[0x05, 0xFF]).await;
        return Err(());
    }
    stream.write_all(&[0x05, 0x00]).await.map_err(|_| ())?;

    let mut req_head = [0u8; 4];
    stream.read_exact(&mut req_head).await.map_err(|_| ())?;
    if req_head[0] != 0x05 || req_head[2] != 0x00 {
        let _ = send_socks5_reply(stream, 0x01).await;
        return Err(());
    }
    if req_head[1] != 0x01 {
        let _ = send_socks5_reply(stream, 0x07).await;
        return Err(());
    }

    let dst_host = match req_head[3] {
        0x01 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await.map_err(|_| ())?;
            IpAddr::V4(Ipv4Addr::from(addr)).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(|_| ())?;
            if len[0] == 0 {
                let _ = send_socks5_reply(stream, 0x01).await;
                return Err(());
            }
            let mut domain = vec![0u8; usize::from(len[0])];
            stream.read_exact(&mut domain).await.map_err(|_| ())?;
            match String::from_utf8(domain) {
                Ok(host) if !host.trim().is_empty() => host,
                _ => {
                    let _ = send_socks5_reply(stream, 0x08).await;
                    return Err(());
                }
            }
        }
        0x04 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await.map_err(|_| ())?;
            IpAddr::V6(Ipv6Addr::from(addr)).to_string()
        }
        _ => {
            let _ = send_socks5_reply(stream, 0x08).await;
            return Err(());
        }
    };

    let mut port_bytes = [0u8; 2];
    stream.read_exact(&mut port_bytes).await.map_err(|_| ())?;
    let dst_port = u16::from_be_bytes(port_bytes);
    if dst_port == 0 {
        let _ = send_socks5_reply(stream, 0x01).await;
        return Err(());
    }

    Ok((dst_host, dst_port))
}

async fn send_socks5_reply(stream: &mut TcpStream, reply_code: u8) -> std::io::Result<()> {
    let reply = [0x05, reply_code, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    stream.write_all(&reply).await
}

struct StandaloneTransportDriver {
    tunnel_id: String,
    task_gen: u64,
    spec: StandaloneTransportSpec,
    ready_tx: watch::Sender<TransportReadySnapshot>,
    cmd_rx: mpsc::UnboundedReceiver<StandaloneCmd>,
    stop_tx: watch::Sender<bool>,
    tunnel_done_rx: watch::Receiver<bool>,
    driver_done_tx: watch::Sender<bool>,
    inner: Arc<TunnelManagerInner>,
    epoch: u64,
    has_connected_once: bool,
    last_error: Option<String>,
}

impl StandaloneTransportDriver {
    async fn run(mut self) {
        let _done = DoneSignal(self.driver_done_tx.clone());
        let mut attempt: u32 = 0;

        loop {
            self.publish_ready(TransportStatus::Connecting, None, None);

            let connect_res = self.connect_with_cancel().await;
            match connect_res {
                StandaloneConnectOutcome::Connected(transport) => {
                    self.has_connected_once = true;
                    self.epoch = self.epoch.wrapping_add(1);
                    self.last_error = None;
                    self.publish_ready(
                        TransportStatus::Connected { latency_ms: None },
                        Some(Arc::clone(&transport)),
                        None,
                    );

                    match self.monitor_connected(transport).await {
                        StandaloneMonitorExit::Dropped(reason) => {
                            self.last_error = Some(reason);
                            attempt = 1;
                            if !self.wait_backoff(attempt).await {
                                break;
                            }
                        }
                        StandaloneMonitorExit::Stop => break,
                    }
                }
                StandaloneConnectOutcome::Retryable(err) => {
                    self.last_error = Some(err.clone());
                    attempt = attempt.saturating_add(1).max(1);
                    if !self.wait_backoff(attempt).await {
                        break;
                    }
                }
                StandaloneConnectOutcome::Fatal(err) => {
                    self.last_error = Some(err.clone());
                    self.publish_fatal(err);
                    break;
                }
                StandaloneConnectOutcome::Stop => break,
            }
        }

        self.publish_ready(TransportStatus::Disconnected, None, self.last_error.clone());
    }

    async fn shutdown_tunnel_then_transport(&mut self, transport: Option<&Arc<SshTransport>>) {
        let _ = self.stop_tx.send(true);
        let _ = tokio::time::timeout(
            CANCEL_TCPIP_FORWARD_TIMEOUT + Duration::from_secs(1),
            self.tunnel_done_rx.wait_for(|&done| done),
        )
        .await;
        if let Some(tr) = transport {
            let _ = tr.close().await;
        }
    }

    async fn connect_with_cancel(&mut self) -> StandaloneConnectOutcome {
        let ka = if self.spec.supervisor_config.tcp_keepalive
            && !self.spec.supervisor_config.keepalive_interval.is_zero()
        {
            Some(crate::client::TcpKeepaliveSettings {
                interval: self.spec.supervisor_config.keepalive_interval,
            })
        } else {
            None
        };
        let cfg = self.spec.connect_config.clone();
        let connector = Arc::clone(&self.spec.connector);
        let fut = async move { connector.connect(&cfg, ka).await.map(Arc::new) };
        tokio::pin!(fut);

        loop {
            tokio::select! {
                res = &mut fut => {
                    return match res {
                        Ok(tr) => StandaloneConnectOutcome::Connected(tr),
                        Err(err @ (SshError::HostKeyMismatch { .. } | SshError::HostKeyRejected { .. } | SshError::InteractiveCancelled)) => {
                            StandaloneConnectOutcome::Fatal(err.to_string())
                        }
                        Err(err @ (SshError::AuthenticationFailed { .. } | SshError::KeyPassphraseRequired { .. } | SshError::KeyLoad { .. })) if !self.has_connected_once => {
                            StandaloneConnectOutcome::Fatal(err.to_string())
                        }
                        Err(err) => StandaloneConnectOutcome::Retryable(err.to_string()),
                    };
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(StandaloneCmd::Stop) | None => {
                            self.shutdown_tunnel_then_transport(None).await;
                            return StandaloneConnectOutcome::Stop;
                        }
                        Some(StandaloneCmd::ReconnectNow) => {}
                    }
                }
            }
        }
    }

    async fn monitor_connected(&mut self, transport: Arc<SshTransport>) -> StandaloneMonitorExit {
        let mut liveness_tick = tokio::time::interval(STANDALONE_LIVENESS_POLL);
        liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let ka_interval = self.spec.supervisor_config.keepalive_interval;
        let max_missed = self.spec.supervisor_config.keepalive_max_missed.max(1);
        let probe_timeout = self
            .spec
            .supervisor_config
            .probe_timeout
            .max(Duration::from_millis(1));
        let mut missed_probes: u32 = 0;
        let mut next_probe = if ka_interval.is_zero() {
            None
        } else {
            Some(TokioInstant::now() + ka_interval)
        };

        loop {
            let probe_sleep = async {
                match next_probe {
                    Some(dl) => tokio::time::sleep_until(dl).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                _ = liveness_tick.tick() => {
                    if transport.is_closed() {
                        return StandaloneMonitorExit::Dropped("SSH transport closed".to_string());
                    }
                }
                () = probe_sleep, if next_probe.is_some() => {
                    if transport.is_closed() {
                        return StandaloneMonitorExit::Dropped("SSH transport closed".to_string());
                    }
                    let req = transport
                        .handle()
                        .send_global_request("keepalive@openssh.com", &[], true);
                    match tokio::time::timeout(probe_timeout, req).await {
                        Ok(Ok(_)) | Ok(Err(russh::Error::RequestDenied)) if !transport.is_closed() => {
                            missed_probes = 0;
                            next_probe = (!ka_interval.is_zero()).then(|| TokioInstant::now() + ka_interval);
                            self.publish_ready(
                                TransportStatus::Connected { latency_ms: None },
                                Some(Arc::clone(&transport)),
                                None,
                            );
                        }
                        Ok(Err(err)) => {
                            let _ = transport.close().await;
                            return StandaloneMonitorExit::Dropped(err.to_string());
                        }
                        Ok(_) => {
                            return StandaloneMonitorExit::Dropped("SSH transport closed".to_string());
                        }
                        Err(_) => {
                            missed_probes = missed_probes.saturating_add(1);
                            if missed_probes >= max_missed {
                                let _ = transport.close().await;
                                return StandaloneMonitorExit::Dropped(format!(
                                    "keepalive timed out after {missed_probes} missed probes"
                                ));
                            }
                            next_probe = (!ka_interval.is_zero()).then(|| TokioInstant::now() + ka_interval);
                            self.publish_ready(
                                TransportStatus::Degraded {
                                    missed_probes: missed_probes.min(u32::from(u8::MAX)) as u8,
                                },
                                Some(Arc::clone(&transport)),
                                None,
                            );
                        }
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(StandaloneCmd::Stop) | None => {
                            self.shutdown_tunnel_then_transport(Some(&transport)).await;
                            return StandaloneMonitorExit::Stop;
                        }
                        Some(StandaloneCmd::ReconnectNow) => {
                            let _ = transport.close().await;
                            return StandaloneMonitorExit::Dropped("manual tunnel reconnect requested".to_string());
                        }
                    }
                }
            }
        }
    }

    async fn wait_backoff(&mut self, attempt: u32) -> bool {
        let jitter: f64 = rand::rng().random();
        let delay = compute_reconnect_backoff(
            attempt,
            self.spec.supervisor_config.backoff_min,
            self.spec.supervisor_config.backoff_max,
            jitter,
        );
        let deadline = TokioInstant::now() + delay;
        let secs = delay.as_millis().div_ceil(1000) as u64;
        self.publish_ready(
            TransportStatus::Reconnecting {
                attempt,
                next_retry_secs: secs,
            },
            None,
            self.last_error.clone(),
        );

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;

        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => return true,
                _ = tick.tick() => {
                    let rem = deadline.saturating_duration_since(TokioInstant::now());
                    let rem_secs = rem.as_millis().div_ceil(1000) as u64;
                    self.publish_ready(
                        TransportStatus::Reconnecting {
                            attempt,
                            next_retry_secs: rem_secs,
                        },
                        None,
                        self.last_error.clone(),
                    );
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(StandaloneCmd::ReconnectNow) => return true,
                        Some(StandaloneCmd::Stop) | None => {
                            self.shutdown_tunnel_then_transport(None).await;
                            return false;
                        }
                    }
                }
            }
        }
    }

    fn publish_ready(
        &self,
        status: TransportStatus,
        transport: Option<Arc<SshTransport>>,
        last_error: Option<String>,
    ) {
        self.ready_tx.send_replace(TransportReadySnapshot {
            epoch: self.epoch,
            status: status.clone(),
            transport,
        });
        self.inner
            .update_entry(&self.tunnel_id, self.task_gen, |entry| {
                entry.transport_status = status;
                if let Some(err) = last_error {
                    entry.snapshot.last_error = Some(err);
                } else if matches!(entry.transport_status, TransportStatus::Connected { .. }) {
                    entry.snapshot.last_error = None;
                }
                refresh_entry_state(entry);
            });
    }

    fn publish_fatal(&self, error: String) {
        self.inner
            .update_entry(&self.tunnel_id, self.task_gen, |entry| {
                entry.fatal_error = Some(error.clone());
                entry.snapshot.last_error = Some(error);
                refresh_entry_state(entry);
            });
    }
}

enum StandaloneConnectOutcome {
    Connected(Arc<SshTransport>),
    Retryable(String),
    Fatal(String),
    Stop,
}

enum StandaloneMonitorExit {
    Dropped(String),
    Stop,
}
