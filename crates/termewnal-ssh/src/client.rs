use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use russh::client;
use russh::keys::PublicKeyOrCertificate;
use russh::{Channel, Disconnect};
use termewnal_core::{Profile, ProfileTarget, SshAuthMethod, vault::SecretStore};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::auth::{AgentConnector, SshInteractionHandler, authenticate_session};
use crate::error::SshError;
use crate::known_hosts::{
    KnownHostStatus, default_known_hosts_path, record_known_host_path, verify_known_host_path,
};

/// Connection and PTY configuration for establishing an SSH `Transport` and `Session`.
#[derive(Debug, Clone)]
pub struct SshConnectConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuthMethod,
    pub known_hosts_path: Option<PathBuf>,
    pub nodelay: bool,
    pub term_name: String,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: usize,
}

impl SshConnectConfig {
    /// Builds an [`SshConnectConfig`] from an SSH [`Profile`].
    pub fn from_profile(profile: &Profile, cols: u16, rows: u16, scrollback_lines: usize) -> Result<Self, SshError> {
        match &profile.target {
            ProfileTarget::Ssh {
                host,
                port,
                username,
                auth,
                ..
            } => Ok(Self {
                host: host.clone(),
                port: *port,
                username: username.clone(),
                auth: auth.clone(),
                known_hosts_path: None,
                nodelay: true,
                term_name: "xterm-256color".to_string(),
                cols: cols.max(1),
                rows: rows.max(1),
                scrollback_lines,
            }),
            ProfileTarget::LocalShell { .. } => Err(SshError::Connection {
                host: profile.name.clone(),
                port: 0,
                reason: "profile target is a local shell, not SSH".to_string(),
            }),
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
}

impl SshClientHandler {
    /// Creates a new [`SshClientHandler`] for `(host, port)`.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        known_hosts_path: PathBuf,
        interaction: Arc<dyn SshInteractionHandler>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            known_hosts_path,
            interaction,
            banner: Arc::new(Mutex::new(None)),
        }
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
}

/// Authenticated SSH `Primary Transport` wrapping `russh::client::Handle`.
pub struct SshTransport {
    host: String,
    port: u16,
    handle: client::Handle<SshClientHandler>,
    used_interactive_auth: bool,
}

impl SshTransport {
    /// Opens a TCP connection to `config.host:config.port`, verifies host key, and authenticates.
    pub async fn connect(
        config: &SshConnectConfig,
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
        let handler = SshClientHandler::new(
            config.host.clone(),
            config.port,
            known_hosts_path,
            Arc::clone(&interaction),
        );

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
            used_interactive_auth: outcome.used_interactive_auth,
        })
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

    /// Returns `true` if the underlying SSH transport channel has closed.
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Borrows the underlying `russh::client::Handle` for supervisor, tunnel, or SFTP operations.
    pub fn handle(&self) -> &client::Handle<SshClientHandler> {
        &self.handle
    }

    /// Opens an additional SSH session channel (used for PTY shell or multiplexed SFTP fallback).
    pub async fn open_session_channel(&self) -> Result<Channel<client::Msg>, SshError> {
        Ok(self.handle.channel_open_session().await?)
    }

    /// Sends an SSH `SSH_MSG_DISCONNECT` cleanly and shuts down the transport.
    pub async fn disconnect(self) -> Result<(), SshError> {
        self.handle
            .disconnect(Disconnect::ByApplication, "closed by user", "en")
            .await?;
        Ok(())
    }
}
