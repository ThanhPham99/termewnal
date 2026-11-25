use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::PublicKeyOrCertificate;
use russh::{Channel, ChannelOpenFailure, Disconnect};
use socket2::{SockRef, TcpKeepalive};
use termewnal_core::{
    DEFAULT_KEEPALIVE_INTERVAL_SECS, OpenSshHost, Profile, ProfileTarget, SshAuthMethod,
    vault::SecretStore,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::auth::{AgentConnector, SshInteractionHandler, authenticate_session};
use crate::error::SshError;
use crate::known_hosts::{
    KnownHostStatus, default_known_hosts_path, record_known_host_path, verify_known_host_path,
};
use crate::proxy_jump::{JumpHopSpec, connect_chain_parts, resolve_proxy_jump_hops};

/// Connection and PTY configuration for establishing an SSH `Transport` and `Session`.
#[derive(Debug, Clone)]
pub struct SshConnectConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuthMethod,
    pub known_hosts_path: Option<PathBuf>,
    pub proxy_jump: Vec<JumpHopSpec>,
    pub nodelay: bool,
    pub term_name: String,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: usize,
}

impl Default for SshConnectConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            username: String::new(),
            auth: SshAuthMethod::Agent,
            known_hosts_path: None,
            proxy_jump: Vec::new(),
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
        }
    }
}

impl SshConnectConfig {
    /// Builds an [`SshConnectConfig`] from an SSH [`Profile`] resolving literal `ProxyJump` hops.
    pub fn from_profile(
        profile: &Profile,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> Result<Self, SshError> {
        Self::from_profile_with_sources(profile, &[], &[], cols, rows, scrollback_lines)
    }

    /// Builds an [`SshConnectConfig`] from an SSH [`Profile`], resolving `ProxyJump` aliases
    /// against Saved [`Profile`]s and `~/.ssh/config` [`OpenSshHost`] entries.
    pub fn from_profile_with_sources(
        profile: &Profile,
        saved_profiles: &[Profile],
        openssh_hosts: &[OpenSshHost],
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> Result<Self, SshError> {
        match &profile.target {
            ProfileTarget::Ssh {
                host,
                port,
                username,
                auth,
                proxy_jump,
                ..
            } => {
                let resolved_hops = resolve_proxy_jump_hops(
                    proxy_jump,
                    saved_profiles,
                    openssh_hosts,
                    username,
                    None,
                )?;
                Ok(Self {
                    host: host.clone(),
                    port: *port,
                    username: username.clone(),
                    auth: auth.clone(),
                    known_hosts_path: None,
                    proxy_jump: resolved_hops,
                    nodelay: true,
                    term_name: "xterm-256color".to_string(),
                    cols: cols.max(1),
                    rows: rows.max(1),
                    scrollback_lines,
                })
            }
            ProfileTarget::LocalShell { .. } => Err(SshError::Connection {
                host: profile.name.clone(),
                port: 0,
                reason: "profile target is a local shell, not SSH".to_string(),
            }),
        }
    }

    /// Overrides the ordered `ProxyJump` hop list on this configuration.
    pub fn with_proxy_jump(mut self, proxy_jump: Vec<JumpHopSpec>) -> Self {
        self.proxy_jump = proxy_jump;
        self
    }
}

/// Incoming `forwarded-tcpip` channel dispatched from [`SshClientHandler`] to a Remote (`-R`) `Tunnel`.
pub struct ForwardedTcpIpRequest {
    pub channel: Channel<client::Msg>,
    pub connected_address: String,
    pub connected_port: u32,
    pub originator_address: String,
    pub originator_port: u32,
    pub reply: client::ChannelOpenHandle,
}

#[derive(Debug)]
struct RegisteredRemoteForward {
    id: u64,
    bind_address: String,
    port: Option<u32>,
    tx: mpsc::UnboundedSender<ForwardedTcpIpRequest>,
}

#[derive(Debug, Default)]
struct RemoteForwardRegistry {
    next_id: u64,
    entries: Vec<RegisteredRemoteForward>,
}

impl RemoteForwardRegistry {
    fn register(
        &mut self,
        bind_address: String,
        requested_port: u32,
        tx: mpsc::UnboundedSender<ForwardedTcpIpRequest>,
    ) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.entries.push(RegisteredRemoteForward {
            id,
            bind_address,
            port: (requested_port > 0).then_some(requested_port),
            tx,
        });
        id
    }

    fn set_port(&mut self, id: u64, port: u32) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.port = Some(port);
        }
    }

    fn unregister(&mut self, id: u64) {
        self.entries.retain(|e| e.id != id);
    }

    fn find_sender(
        &self,
        connected_address: &str,
        connected_port: u32,
    ) -> Option<mpsc::UnboundedSender<ForwardedTcpIpRequest>> {
        let conn_norm = normalize_forward_addr(connected_address);

        // 1. Exact port + exact normalized address match.
        if let Some(entry) = self.entries.iter().find(|e| {
            e.port == Some(connected_port) && normalize_forward_addr(&e.bind_address) == conn_norm
        }) {
            return Some(entry.tx.clone());
        }

        // 2. Exact port + wildcard/loopback address equivalence.
        if let Some(entry) = self.entries.iter().find(|e| {
            e.port == Some(connected_port)
                && forward_addr_matches(&e.bind_address, connected_address)
        }) {
            return Some(entry.tx.clone());
        }

        // 3. Port-only fallback when server rewrote bind host unexpectedly.
        let mut by_port = self
            .entries
            .iter()
            .filter(|e| e.port == Some(connected_port));
        if let (Some(first), None) = (by_port.next(), by_port.next()) {
            return Some(first.tx.clone());
        }

        // 4. Pending ephemeral port (`port: None`) match while `tcpip_forward(..., 0)` reply races.
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.port.is_none() && forward_addr_matches(&e.bind_address, connected_address))
        {
            return Some(entry.tx.clone());
        }

        let mut pending = self.entries.iter().filter(|e| e.port.is_none());
        if let (Some(first), None) = (pending.next(), pending.next()) {
            return Some(first.tx.clone());
        }

        None
    }
}

fn normalize_forward_addr(raw: &str) -> String {
    raw.trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw.trim())
        .to_ascii_lowercase()
}

fn is_wildcard_addr(norm: &str) -> bool {
    matches!(norm, "" | "*" | "0.0.0.0" | "::")
}

fn is_loopback_addr(norm: &str) -> bool {
    matches!(norm, "localhost" | "127.0.0.1" | "::1")
}

fn forward_addr_matches(registered: &str, connected: &str) -> bool {
    let reg = normalize_forward_addr(registered);
    let conn = normalize_forward_addr(connected);
    reg == conn
        || is_wildcard_addr(&reg)
        || (is_loopback_addr(&reg) && is_loopback_addr(&conn))
}

/// RAII handle unregistering a Remote (`-R`) `Tunnel` dispatcher from [`SshTransport`].
pub struct RemoteForwardRegistration {
    id: u64,
    registry: Arc<Mutex<RemoteForwardRegistry>>,
}

impl RemoteForwardRegistration {
    /// Records the server-allocated remote port once `tcpip_forward` succeeds.
    pub fn set_port(&self, port: u32) {
        if let Ok(mut guard) = self.registry.lock() {
            guard.set_port(self.id, port);
        }
    }
}

impl Drop for RemoteForwardRegistration {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.registry.lock() {
            guard.unregister(self.id);
        }
    }
}

/// `russh::client::Handler` enforcing strict `known_hosts` TOFU/MITM verification.
pub struct SshClientHandler {
    host: String,
    port: u16,
    known_hosts_path: PathBuf,
    interaction: Arc<dyn SshInteractionHandler>,
    banner: Arc<Mutex<Option<String>>>,
    remote_forwards: Arc<Mutex<RemoteForwardRegistry>>,
    closed_tx: watch::Sender<bool>,
}

impl SshClientHandler {
    /// Creates a new [`SshClientHandler`] for `(host, port)`.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        known_hosts_path: PathBuf,
        interaction: Arc<dyn SshInteractionHandler>,
    ) -> Self {
        Self::with_remote_forwards(
            host,
            port,
            known_hosts_path,
            interaction,
            Arc::new(Mutex::new(RemoteForwardRegistry::default())),
        )
    }

    fn with_remote_forwards(
        host: impl Into<String>,
        port: u16,
        known_hosts_path: PathBuf,
        interaction: Arc<dyn SshInteractionHandler>,
        remote_forwards: Arc<Mutex<RemoteForwardRegistry>>,
    ) -> Self {
        let (closed_tx, _) = watch::channel(false);
        Self {
            host: host.into(),
            port,
            known_hosts_path,
            interaction,
            banner: Arc::new(Mutex::new(None)),
            remote_forwards,
            closed_tx,
        }
    }

    fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }
}

impl client::Handler for SshClientHandler {
    type Error = SshError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let pubkey = server_public_key.public_key();
        match verify_known_host_path(&self.host, self.port, &pubkey, &self.known_hosts_path)? {
            KnownHostStatus::Trusted => Ok(true),
            KnownHostStatus::Mismatch {
                host,
                port,
                line,
                recorded_fingerprint_sha256,
                offered_fingerprint_sha256,
                key_type,
            } => Err(SshError::HostKeyMismatch {
                host,
                port,
                line,
                key_type,
                expected_fingerprint: recorded_fingerprint_sha256,
                received_fingerprint: offered_fingerprint_sha256,
            }),
            KnownHostStatus::Unknown {
                host,
                port,
                key_type,
                fingerprint_sha256,
            } => {
                let decision = self
                    .interaction
                    .verify_unknown_host(crate::auth::HostKeyPrompt {
                        host: host.clone(),
                        port,
                        key_type,
                        fingerprint_sha256: fingerprint_sha256.clone(),
                    })
                    .await;

                match decision {
                    crate::auth::HostKeyDecision::AcceptAndSave => {
                        record_known_host_path(&host, port, &pubkey, &self.known_hosts_path)?;
                        Ok(true)
                    }
                    crate::auth::HostKeyDecision::Reject => Err(SshError::HostKeyRejected {
                        host,
                        port,
                        fingerprint: fingerprint_sha256,
                    }),
                }
            }
        }
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if let Ok(mut guard) = self.banner.lock() {
            *guard = Some(banner.to_string());
        }
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let sender = self
            .remote_forwards
            .lock()
            .ok()
            .and_then(|reg| reg.find_sender(connected_address, connected_port));

        let Some(tx) = sender else {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };

        let req = ForwardedTcpIpRequest {
            channel,
            connected_address: connected_address.to_string(),
            connected_port,
            originator_address: originator_address.to_string(),
            originator_port,
            reply,
        };
        if let Err(err) = tx.send(req) {
            err.0.reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
        }
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let _ = self.closed_tx.send(true);
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(err) => Err(err),
        }
    }
}

/// TCP keepalive settings applied to the underlying `TcpStream` via `socket2` before SSH handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpKeepaliveSettings {
    /// Idle duration before the OS starts sending TCP keepalive probes.
    pub interval: Duration,
}

impl Default for TcpKeepaliveSettings {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_KEEPALIVE_INTERVAL_SECS),
        }
    }
}

/// Authenticated SSH `Primary Transport` wrapping `russh::client::Handle`.
pub struct SshTransport {
    host: String,
    port: u16,
    handle: client::Handle<SshClientHandler>,
    closed_rx: watch::Receiver<bool>,
    used_interactive_auth: bool,
    remote_forwards: Arc<Mutex<RemoteForwardRegistry>>,
    upstream_hops: Vec<Arc<SshTransport>>,
}

impl SshTransport {
    /// Opens an SSH connection to `config.host:config.port` (direct or via `config.proxy_jump`),
    /// applies TCP keepalives, verifies host keys, and authenticates.
    pub async fn connect(
        config: &SshConnectConfig,
        vault: Option<&dyn SecretStore>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Result<Self, SshError> {
        Self::connect_with_keepalive(
            config,
            Some(TcpKeepaliveSettings::default()),
            vault,
            interaction,
            agent_connector,
        )
        .await
    }

    /// Opens an SSH connection to `config.host:config.port` (direct or via `config.proxy_jump`)
    /// with explicit TCP keepalive settings, verifies host keys, and authenticates.
    pub async fn connect_with_keepalive(
        config: &SshConnectConfig,
        keepalive: Option<TcpKeepaliveSettings>,
        vault: Option<&dyn SecretStore>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Result<Self, SshError> {
        if config.proxy_jump.is_empty() {
            return Self::connect_direct_with_keepalive(
                config,
                keepalive,
                vault,
                interaction,
                agent_connector,
            )
            .await;
        }

        let (jump_hops, mut target) =
            connect_chain_parts(config, keepalive, vault, interaction, agent_connector).await?;
        let any_interactive = jump_hops.iter().any(|h| h.used_interactive_auth());
        if any_interactive {
            target.mark_used_interactive_auth();
        }
        target.attach_upstream_hops(jump_hops.into_iter().map(Arc::new).collect());
        Ok(target)
    }

    pub(crate) async fn connect_direct_with_keepalive(
        config: &SshConnectConfig,
        keepalive: Option<TcpKeepaliveSettings>,
        vault: Option<&dyn SecretStore>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Result<Self, SshError> {
        let tcp = TcpStream::connect((config.host.as_str(), config.port))
            .await
            .map_err(|err| SshError::Connection {
                host: config.host.clone(),
                port: config.port,
                reason: err.to_string(),
            })?;
        let _ = tcp.set_nodelay(config.nodelay);
        if let Some(ka) = keepalive
            && !ka.interval.is_zero()
        {
            apply_tcp_keepalive(&tcp, ka);
        }

        Self::connect_over_stream(tcp, config, vault, interaction, agent_connector).await
    }

    /// Performs SSH handshake, host key verification, and authentication over an existing stream
    /// (direct `TcpStream` or nested `ProxyJump` `direct-tcpip` channel stream).
    pub async fn connect_over_stream<S>(
        stream: S,
        config: &SshConnectConfig,
        vault: Option<&dyn SecretStore>,
        interaction: Arc<dyn SshInteractionHandler>,
        agent_connector: Arc<dyn AgentConnector>,
    ) -> Result<Self, SshError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let known_hosts_path = match &config.known_hosts_path {
            Some(path) => path.clone(),
            None => default_known_hosts_path()?,
        };

        let russh_cfg = Arc::new(client::Config {
            nodelay: config.nodelay,
            ..client::Config::default()
        });
        let remote_forwards = Arc::new(Mutex::new(RemoteForwardRegistry::default()));
        let handler = SshClientHandler::with_remote_forwards(
            config.host.clone(),
            config.port,
            known_hosts_path,
            Arc::clone(&interaction),
            Arc::clone(&remote_forwards),
        );
        let closed_rx = handler.subscribe_closed();

        let mut handle = client::connect_stream(russh_cfg, stream, handler).await?;

        let auth_res = authenticate_session(
            &mut handle,
            &config.host,
            &config.username,
            &config.auth,
            vault,
            interaction.as_ref(),
            agent_connector.as_ref(),
        )
        .await;

        let outcome = match auth_res {
            Ok(outcome) => outcome,
            Err(err) => {
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "", "en")
                    .await;
                return Err(err);
            }
        };

        Ok(Self {
            host: config.host.clone(),
            port: config.port,
            handle,
            closed_rx,
            used_interactive_auth: outcome.used_interactive_auth,
            remote_forwards,
            upstream_hops: Vec::new(),
        })
    }

    pub(crate) fn mark_used_interactive_auth(&mut self) {
        self.used_interactive_auth = true;
    }

    pub(crate) fn attach_upstream_hops(&mut self, hops: Vec<Arc<SshTransport>>) {
        self.upstream_hops = hops;
    }

    /// Target SSH hostname.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Target SSH TCP port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns `true` if authentication required interactive user input (2FA/OTP or prompt).
    pub fn used_interactive_auth(&self) -> bool {
        self.used_interactive_auth
    }

    /// Returns `true` if the underlying SSH transport channel (or any attached upstream hop) has closed.
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
            || *self.closed_rx.borrow()
            || self.upstream_hops.iter().any(|hop| hop.is_closed())
    }

    /// Resolves once this SSH transport session disconnects or closes.
    pub async fn wait_closed(&self) {
        if self.handle.is_closed() || *self.closed_rx.borrow() {
            return;
        }
        let mut rx = self.closed_rx.clone();
        loop {
            if *rx.borrow_and_update() || self.handle.is_closed() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Borrows the underlying `russh::client::Handle` for supervisor, tunnel, or SFTP operations.
    pub fn handle(&self) -> &client::Handle<SshClientHandler> {
        &self.handle
    }

    /// Registers a Remote (`-R`) `Tunnel` listener dispatcher for `forwarded-tcpip` channels.
    ///
    /// Dropping the returned [`RemoteForwardRegistration`] unregisters the dispatcher automatically.
    pub fn register_remote_forward(
        &self,
        bind_address: impl Into<String>,
        requested_port: u32,
    ) -> (
        RemoteForwardRegistration,
        mpsc::UnboundedReceiver<ForwardedTcpIpRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self
            .remote_forwards
            .lock()
            .expect("remote forward registry poisoned")
            .register(bind_address.into(), requested_port, tx);
        let reg = RemoteForwardRegistration {
            id,
            registry: Arc::clone(&self.remote_forwards),
        };
        (reg, rx)
    }

    /// Opens an additional SSH session channel (used for PTY shell or multiplexed SFTP fallback).
    pub async fn open_session_channel(&self) -> Result<Channel<client::Msg>, SshError> {
        Ok(self.handle.channel_open_session().await?)
    }

    /// Sends an SSH `SSH_MSG_DISCONNECT` cleanly without consuming the shared transport handle,
    /// tearing down any attached upstream `ProxyJump` hops in reverse order afterwards.
    pub async fn disconnect_shared(&self) -> Result<(), SshError> {
        let res = tokio::time::timeout(
            Duration::from_millis(250),
            self.handle
                .disconnect(Disconnect::ByApplication, "closed by user", "en"),
        )
        .await;
        if !self.upstream_hops.is_empty() {
            let _ = tokio::time::timeout(Duration::from_millis(50), self.wait_closed()).await;
            for hop in self.upstream_hops.iter().rev() {
                hop.disconnect_best_effort().await;
            }
        }
        match res {
            Ok(inner) => Ok(inner?),
            Err(_) => Ok(()),
        }
    }

    pub(crate) async fn disconnect_best_effort(&self) {
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            self.handle
                .disconnect(Disconnect::ByApplication, "closed by user", "en"),
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_millis(50), self.wait_closed()).await;
    }

    /// Alias for [`Self::disconnect_shared`] used by supervisor teardown paths.
    pub async fn close(&self) -> Result<(), SshError> {
        self.disconnect_shared().await
    }

    /// Sends an SSH `SSH_MSG_DISCONNECT` cleanly and shuts down the transport.
    pub async fn disconnect(self) -> Result<(), SshError> {
        self.disconnect_shared().await
    }
}

fn apply_tcp_keepalive(tcp: &TcpStream, settings: TcpKeepaliveSettings) {
    let mut keepalive = TcpKeepalive::new().with_time(settings.interval);
    #[cfg(not(any(
        target_os = "openbsd",
        target_os = "redox",
        target_os = "solaris",
        target_os = "nto",
        target_os = "espidf",
        target_os = "vita",
        target_os = "haiku",
        target_os = "horizon"
    )))]
    {
        keepalive = keepalive.with_interval(settings.interval);
    }

    if let Err(err) = SockRef::from(tcp).set_tcp_keepalive(&keepalive) {
        debug!(error = %err, "failed to apply SO_KEEPALIVE/TCP_KEEPALIVE parameters");
    }
}
