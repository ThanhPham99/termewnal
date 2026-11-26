use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::PublicKeyOrCertificate;
use russh::{Channel, ChannelMsg, ChannelOpenFailure, Disconnect};
use socket2::{SockRef, TcpKeepalive};
use termewnal_core::{
    DEFAULT_KEEPALIVE_INTERVAL_SECS, LoginScriptStep, OpenSshHost, Profile, ProfileTarget,
    SshAuthMethod, SshProxyConfig, vault::SecretStore,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::auth::{AgentConnector, SshInteractionHandler, authenticate_session};
use crate::error::SshError;
use crate::known_hosts::{
    KnownHostStatus, default_known_hosts_path, record_known_host_path, verify_known_host_path,
};
use crate::proxy_cmd::ProxyCommandStream;
use crate::proxy_jump::{JumpHopSpec, connect_chain_parts, resolve_proxy_jump_hops};
use crate::proxy_net::dial_via_proxy;
use crate::server_stats::{
    POSIX_SERVER_STATS_COMMAND, ServerStatsCollector, ServerStatsSnapshot, parse_raw_server_stats,
};
use crate::x11::{X11AuthInfo, X11DisplayTarget, connect_x11_target, resolve_x11_auth};

/// Connection and PTY configuration for establishing an SSH `Transport` and `Session`.
#[derive(Debug, Clone)]
pub struct SshConnectConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuthMethod,
    pub known_hosts_path: Option<PathBuf>,
    pub proxy_jump: Vec<JumpHopSpec>,
    pub proxy_command: Option<String>,
    pub proxy: Option<SshProxyConfig>,
    pub forward_agent: bool,
    pub forward_x11: bool,
    pub x11_display: Option<String>,
    pub login_scripts: Vec<LoginScriptStep>,
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
            proxy_command: None,
            proxy: None,
            forward_agent: false,
            forward_x11: false,
            x11_display: None,
            login_scripts: Vec::new(),
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
                proxy_command,
                proxy,
                forward_agent,
                forward_x11,
                login_scripts,
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
                    proxy_command: proxy_command.clone(),
                    proxy: proxy.clone(),
                    forward_agent: *forward_agent,
                    forward_x11: *forward_x11,
                    x11_display: None,
                    login_scripts: login_scripts.clone(),
                    nodelay: true,
                    term_name: "xterm-256color".to_string(),
                    cols: cols.max(1),
                    rows: rows.max(1),
                    scrollback_lines,
                })
            }
            _ => Err(SshError::Connection {
                host: profile.name.clone(),
                port: 0,
                reason: "profile target is not SSH".to_string(),
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
    forward_agent: bool,
    agent_connector: Option<Arc<dyn AgentConnector>>,
    x11_target: Option<X11DisplayTarget>,
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
        Self::with_options(
            host,
            port,
            known_hosts_path,
            interaction,
            Arc::new(Mutex::new(RemoteForwardRegistry::default())),
            false,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_options(
        host: impl Into<String>,
        port: u16,
        known_hosts_path: PathBuf,
        interaction: Arc<dyn SshInteractionHandler>,
        remote_forwards: Arc<Mutex<RemoteForwardRegistry>>,
        forward_agent: bool,
        agent_connector: Option<Arc<dyn AgentConnector>>,
        x11_target: Option<X11DisplayTarget>,
    ) -> Self {
        let (closed_tx, _) = watch::channel(false);
        Self {
            host: host.into(),
            port,
            known_hosts_path,
            interaction,
            banner: Arc::new(Mutex::new(None)),
            remote_forwards,
            forward_agent,
            agent_connector,
            x11_target,
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

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let Some(connector) = self
            .agent_connector
            .as_ref()
            .filter(|_| self.forward_agent)
            .cloned()
        else {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };
        reply.accept().await;
        tokio::spawn(async move {
            match connector.connect().await {
                Ok(agent) => {
                    let mut agent_stream = agent.into_inner();
                    let mut ch_stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut ch_stream, &mut agent_stream).await;
                }
                Err(err) => {
                    warn!(error = %err, "SSH agent forwarding connection failed");
                }
            }
        });
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        channel: Channel<client::Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let Some(target) = self.x11_target.clone() else {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };
        reply.accept().await;
        tokio::spawn(async move {
            match connect_x11_target(&target).await {
                Ok(mut x11_stream) => {
                    let mut ch_stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut ch_stream, &mut x11_stream).await;
                }
                Err(err) => {
                    warn!(error = %err, "SSH X11 forwarding local display connect failed");
                }
            }
        });
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
    x11_auth: Option<X11AuthInfo>,
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
        if let Some(proxy_cmd) = config
            .proxy_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let stream = ProxyCommandStream::spawn(
                proxy_cmd,
                &config.host,
                config.port,
                &config.username,
            )?;
            return Self::connect_over_stream(stream, config, vault, interaction, agent_connector)
                .await;
        }

        if let Some(proxy_cfg) = config
            .proxy
            .as_ref()
            .filter(|p| !p.host.trim().is_empty())
        {
            let proxy_secret = match (vault, proxy_cfg.vault_secret_ref.as_deref()) {
                (Some(store), Some(secret_ref)) => store.get_secret(secret_ref)?,
                (None, Some(_)) => {
                    return Err(SshError::Connection {
                        host: config.host.clone(),
                        port: config.port,
                        reason: "proxy password stored in Vault, but Vault is unavailable"
                            .to_string(),
                    });
                }
                _ => None,
            };
            let stream = dial_via_proxy(
                proxy_cfg,
                proxy_secret.as_deref().map(String::as_str),
                &config.host,
                config.port,
                config.nodelay,
                keepalive,
            )
            .await?;
            return Self::connect_over_stream(stream, config, vault, interaction, agent_connector)
                .await;
        }

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
    /// (direct `TcpStream`, `ProxyCommand`, or nested `ProxyJump` `direct-tcpip` channel stream).
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

        let x11_auth = if config.forward_x11 {
            resolve_x11_auth(config.x11_display.as_deref())
        } else {
            None
        };

        let russh_cfg = Arc::new(client::Config {
            nodelay: config.nodelay,
            ..client::Config::default()
        });
        let remote_forwards = Arc::new(Mutex::new(RemoteForwardRegistry::default()));
        let handler = SshClientHandler::with_options(
            config.host.clone(),
            config.port,
            known_hosts_path,
            Arc::clone(&interaction),
            Arc::clone(&remote_forwards),
            config.forward_agent,
            config.forward_agent.then(|| Arc::clone(&agent_connector)),
            x11_auth.as_ref().map(|a| a.target.clone()),
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
            x11_auth,
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

    /// Returns resolved X11 forwarding parameters if enabled and display was detected.
    pub fn x11_auth(&self) -> Option<&X11AuthInfo> {
        self.x11_auth.as_ref()
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

    /// Executes a non-interactive command on a multiplexed SSH `exec` channel within `timeout`.
    pub async fn exec_command(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<(Option<u32>, Vec<u8>, Vec<u8>), SshError> {
        let fut = async {
            let mut channel = self.open_session_channel().await?;
            channel
                .exec(true, command)
                .await
                .map_err(|err| SshError::Channel(err.to_string()))?;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut exit_status: Option<u32> = None;
            await_channel_reply(&mut channel, "exec request", |data| {
                stdout.extend_from_slice(data);
            })
            .await?;

            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, ext } => {
                        if ext == 1 {
                            stderr.extend_from_slice(&data);
                        } else {
                            stdout.extend_from_slice(&data);
                        }
                    }
                    ChannelMsg::ExitStatus { exit_status: code } => exit_status = Some(code),
                    ChannelMsg::Eof => {
                        if exit_status.is_some() {
                            break;
                        }
                    }
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            let _ = channel.close().await;
            Ok((exit_status, stdout, stderr))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(inner) => inner,
            Err(_) => Err(SshError::Channel("exec command timed out".to_string())),
        }
    }

    /// Collects live remote server stats (`CPU`, `RAM`, `DISK`, `NET`) over an `exec` channel.
    pub async fn collect_server_stats(
        &self,
        collector: &mut ServerStatsCollector,
        timeout: Duration,
    ) -> Result<ServerStatsSnapshot, SshError> {
        let (_status, stdout, _stderr) =
            self.exec_command(POSIX_SERVER_STATS_COMMAND, timeout).await?;
        let text = String::from_utf8_lossy(&stdout);
        let raw = parse_raw_server_stats(&text).map_err(SshError::Channel)?;
        Ok(collector.update(raw, std::time::Instant::now()))
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

pub(crate) fn apply_tcp_keepalive(tcp: &TcpStream, settings: TcpKeepaliveSettings) {
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

pub(crate) fn ceil_secs(duration: Duration) -> u64 {
    if duration.is_zero() {
        0
    } else {
        duration.as_millis().div_ceil(1000) as u64
    }
}

pub(crate) trait ChannelWait: Send {
    fn wait_msg(&mut self) -> impl std::future::Future<Output = Option<ChannelMsg>> + Send;
}

impl ChannelWait for Channel<client::Msg> {
    fn wait_msg(&mut self) -> impl std::future::Future<Output = Option<ChannelMsg>> + Send {
        self.wait()
    }
}

impl ChannelWait for russh::ChannelReadHalf {
    fn wait_msg(&mut self) -> impl std::future::Future<Output = Option<ChannelMsg>> + Send {
        self.wait()
    }
}

pub(crate) async fn await_channel_reply<C, F>(
    channel: &mut C,
    stage: &str,
    mut on_data: F,
) -> Result<(), SshError>
where
    C: ChannelWait + ?Sized,
    F: FnMut(&[u8]),
{
    loop {
        match channel.wait_msg().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => {
                return Err(SshError::Channel(format!(
                    "{stage} rejected by remote server"
                )));
            }
            Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                on_data(&data);
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

pub(crate) async fn request_channel_forwarding(
    tx: &russh::ChannelWriteHalf<client::Msg>,
    rx: &mut russh::ChannelReadHalf,
    forward_agent: bool,
    x11_auth: Option<&X11AuthInfo>,
    on_data: &mut (dyn FnMut(&[u8]) + Send),
) {
    if forward_agent {
        match tx.agent_forward(true).await {
            Ok(()) => {
                if let Err(err) = await_channel_reply(rx, "agent forwarding", |d| on_data(d)).await
                {
                    warn!(error = %err, "remote SSH server declined agent forwarding");
                }
            }
            Err(err) => {
                warn!(error = %err, "failed sending agent forwarding request");
            }
        }
    }

    if let Some(x11) = x11_auth {
        match tx
            .request_x11(true, false, &x11.protocol, &x11.cookie_hex, x11.screen)
            .await
        {
            Ok(()) => {
                if let Err(err) = await_channel_reply(rx, "X11 forwarding", |d| on_data(d)).await {
                    warn!(error = %err, "remote SSH server declined X11 forwarding");
                }
            }
            Err(err) => {
                warn!(error = %err, "failed sending X11 forwarding request");
            }
        }
    }
}
