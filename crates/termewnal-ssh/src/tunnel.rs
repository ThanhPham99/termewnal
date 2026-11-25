use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::RngExt;
use russh::Channel;
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
use crate::client::{SshConnectConfig, SshTransport};
use crate::error::SshError;
use crate::supervisor::{
    DefaultTransportConnector, SupervisorConfig, SystemTimeSource, TimeSource, TransportConnector,
    TransportReadySnapshot, compute_reconnect_backoff,
};

/// Default grace timeout (`10s`) while holding local TCP client sockets during `Transport` recovery.
pub const DEFAULT_TUNNEL_HOLD_GRACE: Duration =
    Duration::from_secs(DEFAULT_TUNNEL_HOLD_GRACE_SECS);

const SOCKS5_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const STANDALONE_LIVENESS_POLL: Duration = Duration::from_millis(50);

static NEXT_TUNNEL_MANAGER_ID: AtomicU64 = AtomicU64::new(1);

/// Runtime lifecycle state of a managed SSH `Tunnel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Stopped,
    Starting,
    Active,
    Holding,
    Reconnecting,
    Error(String),
}

impl TunnelState {
    /// Returns `true` when the `Tunnel` is actively running or recovering in the background.
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Active | Self::Holding | Self::Reconnecting
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
            let target_host = rule
                .target_host
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| SshError::TunnelConfig {
                    tunnel_name: rule.name.clone(),
                    reason: "local tunnel requires a non-empty target_host".to_string(),
                })?
                .to_string();
            let target_port = rule.target_port.filter(|&p| p > 0).ok_or_else(|| {
                SshError::TunnelConfig {
                    tunnel_name: rule.name.clone(),
                    reason: "local tunnel requires a non-zero target_port".to_string(),
                }
            })?;
            TunnelForwardMode::Local {
                target_host,
                target_port,
            }
        }
        TunnelDirection::Dynamic => TunnelForwardMode::DynamicSocks5,
        TunnelDirection::Remote => {
            return Err(SshError::NotImplemented(
                "remote port forwarding (-R) is scheduled for Ticket 12",
            ));
        }
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

enum StandaloneCmd {
    ReconnectNow,
    Stop,
}

struct ManagedTunnel {
    snapshot: TunnelSnapshot,
    transport_status: TransportStatus,
    fatal_error: Option<String>,
    task_gen: u64,
    stop_tx: Option<watch::Sender<bool>>,
    standalone_cmd_tx: Option<mpsc::UnboundedSender<StandaloneCmd>>,
}

struct TunnelManagerInner {
    instance_id: u64,
    next_gen: AtomicU64,
    registry: Mutex<BTreeMap<String, ManagedTunnel>>,
    snapshot_tx: watch::Sender<TunnelManagerSnapshot>,
}

impl TunnelManagerInner {
    fn publish(&self, registry: &BTreeMap<String, ManagedTunnel>) {
        let mut snap = TunnelManagerSnapshot::default();
        for entry in registry.values() {
            let item = &entry.snapshot;
            match item.state {
                TunnelState::Active => snap.active_count += 1,
                TunnelState::Holding => snap.holding_count += 1,
                TunnelState::Reconnecting => snap.reconnecting_count += 1,
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

/// Manages Local (`-L`) and Dynamic SOCKS5 (`-D`) `Tunnel` lifecycles (both `Session`-bound
/// and `Standalone`), preserving local TCP listeners across transient `Transport` drops.
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
    /// Creates an empty [`TunnelManager`].
    pub fn new() -> Self {
        let (snapshot_tx, _) = watch::channel(TunnelManagerSnapshot::default());
        Self {
            inner: Arc::new(TunnelManagerInner {
                instance_id: NEXT_TUNNEL_MANAGER_ID.fetch_add(1, Ordering::Relaxed),
                next_gen: AtomicU64::new(1),
                registry: Mutex::new(BTreeMap::new()),
                snapshot_tx,
            }),
        }
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

    /// Starts a `Session`-bound `Local` (`-L`) or `Dynamic` (`-D`) `Tunnel` attached to
    /// `ready_rx` (typically [`ConnectionSupervisor::subscribe_ready`]).
    ///
    /// Keeps the local `TcpListener` bound across transient `Transport` drops and holds incoming
    /// client sockets up to `rule.effective_hold_grace()` (`10s` by default).
    pub async fn start_session_tunnel(
        &self,
        session_id: u64,
        profile_id: Option<String>,
        rule: TunnelRule,
        ready_rx: watch::Receiver<TransportReadySnapshot>,
    ) -> Result<SocketAddr, SshError> {
        let validated = validate_tunnel_rule(&rule)?;
        let listener =
            TcpListener::bind((validated.bind_host_socket.as_str(), rule.bind_port)).await?;
        let bound_addr = listener.local_addr()?;

        self.spawn_listener_tunnel(
            Some(session_id),
            profile_id,
            rule,
            validated,
            listener,
            bound_addr,
            ready_rx,
            None,
        )
    }

    /// Starts a `Standalone` background `Local` (`-L`) or `Dynamic` (`-D`) `Tunnel` owning its
    /// own headless `Transport` supervisor loop without allocating a remote PTY shell.
    pub async fn start_standalone_tunnel(
        &self,
        profile_id: Option<String>,
        mut rule: TunnelRule,
        spec: StandaloneTransportSpec,
    ) -> Result<SocketAddr, SshError> {
        rule.standalone = true;
        let validated = validate_tunnel_rule(&rule)?;
        let listener =
            TcpListener::bind((validated.bind_host_socket.as_str(), rule.bind_port)).await?;
        let bound_addr = listener.local_addr()?;

        let (ready_tx, ready_rx) = watch::channel(TransportReadySnapshot {
            epoch: 0,
            status: TransportStatus::Connecting,
            transport: None,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let task_gen = self.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        let tunnel_id = rule.id.clone();

        let driver = StandaloneTransportDriver {
            tunnel_id: tunnel_id.clone(),
            task_gen,
            spec,
            ready_tx,
            cmd_rx,
            inner: Arc::clone(&self.inner),
            epoch: 0,
            has_connected_once: false,
            last_error: None,
        };

        let addr = self.spawn_listener_tunnel_with_gen(
            task_gen,
            None,
            profile_id,
            rule,
            validated,
            listener,
            bound_addr,
            ready_rx,
            Some(cmd_tx),
        )?;

        tokio::spawn(driver.run());
        Ok(addr)
    }

    /// Requests an immediate reconnect on a `Standalone` `Tunnel`.
    pub fn reconnect_tunnel(&self, tunnel_id: &str) -> Result<(), SshError> {
        let guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        let Some(entry) = guard.get(tunnel_id) else {
            return Err(SshError::TunnelConfig {
                tunnel_name: tunnel_id.to_string(),
                reason: "tunnel is not active".to_string(),
            });
        };
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

    /// Stops a single running `Tunnel`, immediately closing its local listener and held clients.
    pub fn stop_tunnel(&self, tunnel_id: &str) -> bool {
        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        let Some(entry) = guard.get_mut(tunnel_id) else {
            return false;
        };
        stop_managed_entry(entry);
        self.inner.publish(&guard);
        true
    }

    /// Stops and removes all `Session`-bound (non-`Standalone`) `Tunnel`s attached to `session_id`.
    pub fn stop_session_tunnels(&self, session_id: u64) -> usize {
        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        let matching_ids: Vec<String> = guard
            .iter()
            .filter_map(|(id, entry)| {
                (!entry.snapshot.standalone && entry.snapshot.session_id == Some(session_id))
                    .then(|| id.clone())
            })
            .collect();
        let count = matching_ids.len();
        for id in matching_ids {
            if let Some(mut entry) = guard.remove(&id) {
                stop_managed_entry(&mut entry);
            }
        }
        if count > 0 {
            self.inner.publish(&guard);
        }
        count
    }

    /// Stops every active `Tunnel` (both `Session`-bound and `Standalone`).
    pub fn stop_all(&self) {
        let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
        for entry in guard.values_mut() {
            stop_managed_entry(entry);
        }
        self.inner.publish(&guard);
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
            stop_managed_entry(prev);
        }
        guard.insert(
            rule.id.clone(),
            ManagedTunnel {
                snapshot,
                transport_status: TransportStatus::Disconnected,
                fatal_error: Some(error),
                task_gen,
                stop_tx: None,
                standalone_cmd_tx: None,
            },
        );
        self.inner.publish(&guard);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_listener_tunnel(
        &self,
        session_id: Option<u64>,
        profile_id: Option<String>,
        rule: TunnelRule,
        validated: ValidatedRule,
        listener: TcpListener,
        bound_addr: SocketAddr,
        ready_rx: watch::Receiver<TransportReadySnapshot>,
        standalone_cmd_tx: Option<mpsc::UnboundedSender<StandaloneCmd>>,
    ) -> Result<SocketAddr, SshError> {
        let task_gen = self.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        self.spawn_listener_tunnel_with_gen(
            task_gen,
            session_id,
            profile_id,
            rule,
            validated,
            listener,
            bound_addr,
            ready_rx,
            standalone_cmd_tx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_listener_tunnel_with_gen(
        &self,
        task_gen: u64,
        session_id: Option<u64>,
        profile_id: Option<String>,
        rule: TunnelRule,
        validated: ValidatedRule,
        listener: TcpListener,
        bound_addr: SocketAddr,
        ready_rx: watch::Receiver<TransportReadySnapshot>,
        standalone_cmd_tx: Option<mpsc::UnboundedSender<StandaloneCmd>>,
    ) -> Result<SocketAddr, SshError> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let initial_status = ready_rx.borrow().status.clone();
        let initial_state = compute_tunnel_state(&initial_status, 0, None);
        let tunnel_id = rule.id.clone();

        let snapshot = TunnelSnapshot {
            id: tunnel_id.clone(),
            name: rule.name.clone(),
            profile_id,
            session_id,
            direction: rule.direction,
            bind_host: validated.bind_host_raw,
            bind_port: rule.bind_port,
            bound_addr: Some(bound_addr),
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

        {
            let mut guard = self.inner.registry.lock().expect("tunnel registry poisoned");
            if let Some(prev) = guard.get_mut(&tunnel_id) {
                stop_managed_entry(prev);
            }
            guard.insert(
                tunnel_id.clone(),
                ManagedTunnel {
                    snapshot,
                    transport_status: initial_status,
                    fatal_error: None,
                    task_gen,
                    stop_tx: Some(stop_tx),
                    standalone_cmd_tx,
                },
            );
            self.inner.publish(&guard);
        }

        tokio::spawn(run_listener_tunnel(
            Arc::clone(&self.inner),
            tunnel_id,
            task_gen,
            validated.mode,
            validated.hold_grace,
            listener,
            ready_rx,
            stop_rx,
        ));

        Ok(bound_addr)
    }
}

fn stop_managed_entry(entry: &mut ManagedTunnel) {
    if let Some(stop_tx) = entry.stop_tx.take() {
        let _ = stop_tx.send(true);
    }
    if let Some(cmd_tx) = entry.standalone_cmd_tx.take() {
        let _ = cmd_tx.send(StandaloneCmd::Stop);
    }
    entry.snapshot.bound_addr = None;
    entry.snapshot.held_clients = 0;
    entry.snapshot.active_connections = 0;
    if entry.fatal_error.is_none() {
        entry.snapshot.state = TunnelState::Stopped;
    }
}

fn compute_tunnel_state(
    transport_status: &TransportStatus,
    held_clients: usize,
    fatal_error: Option<&str>,
) -> TunnelState {
    if let Some(err) = fatal_error {
        return TunnelState::Error(err.to_string());
    }
    if held_clients > 0 {
        return TunnelState::Holding;
    }
    match transport_status {
        TransportStatus::Connected { .. } => TunnelState::Active,
        TransportStatus::Degraded { .. } | TransportStatus::Reconnecting { .. } => {
            TunnelState::Reconnecting
        }
        TransportStatus::Connecting => TunnelState::Starting,
        TransportStatus::Disconnected => TunnelState::Stopped,
    }
}

fn refresh_entry_state(entry: &mut ManagedTunnel) {
    entry.snapshot.state = compute_tunnel_state(
        &entry.transport_status,
        entry.snapshot.held_clients,
        entry.fatal_error.as_deref(),
    );
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
) {
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
        if entry.fatal_error.is_none() {
            entry.snapshot.state = TunnelState::Stopped;
        }
    });
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
    inner: Arc<TunnelManagerInner>,
    epoch: u64,
    has_connected_once: bool,
    last_error: Option<String>,
}

impl StandaloneTransportDriver {
    async fn run(mut self) {
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
                        Some(StandaloneCmd::Stop) | None => return StandaloneConnectOutcome::Stop,
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
                            let _ = transport.close().await;
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
                        Some(StandaloneCmd::Stop) | None => return false,
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
