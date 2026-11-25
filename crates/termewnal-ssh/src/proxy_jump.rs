use std::path::{Path, PathBuf};
use std::sync::Arc;

use termewnal_core::{OpenSshHost, Profile, ProfileTarget, SshAuthMethod, vault::SecretStore};
use tokio::task::JoinSet;

use crate::auth::{AgentConnector, SshInteractionHandler};
use crate::client::{SshConnectConfig, SshTransport, TcpKeepaliveSettings};
use crate::error::SshError;

/// Connection target specification for a single intermediate `ProxyJump` hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpHopSpec {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuthMethod,
    pub known_hosts_path: Option<PathBuf>,
    pub nodelay: bool,
}

impl JumpHopSpec {
    /// Creates a new [`JumpHopSpec`] with `nodelay = true` and default `known_hosts` fallback.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth: SshAuthMethod,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            auth,
            known_hosts_path: None,
            nodelay: true,
        }
    }

    /// Sets an explicit `known_hosts` file path for verifying this hop's host key.
    pub fn with_known_hosts_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts_path = Some(path.into());
        self
    }

    /// Formats this hop as `username@host:port`.
    pub fn display_addr(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("{}@[{}]:{}", self.username, self.host, self.port)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port)
        }
    }

    /// Converts this hop into an [`SshConnectConfig`] inheriting `fallback_known_hosts` if unset.
    pub fn to_connect_config(&self, fallback_known_hosts: Option<&Path>) -> SshConnectConfig {
        SshConnectConfig {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            auth: self.auth.clone(),
            known_hosts_path: self
                .known_hosts_path
                .clone()
                .or_else(|| fallback_known_hosts.map(Path::to_path_buf)),
            proxy_jump: Vec::new(),
            nodelay: self.nodelay,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 0,
        }
    }
}

/// Ordered active SSH `Transport` chain from outermost jump hop inward (`[A, B]` -> `C`).
#[derive(Clone)]
pub struct SshTransportChain {
    jump_hops: Vec<Arc<SshTransport>>,
    target: Arc<SshTransport>,
}

impl SshTransportChain {
    /// Wraps a direct single-hop target [`SshTransport`] with no jump hops.
    pub fn direct(transport: SshTransport) -> Self {
        Self {
            jump_hops: Vec::new(),
            target: Arc::new(transport),
        }
    }

    /// Constructs an [`SshTransportChain`] from ordered outer-to-inner jump hops and final target.
    pub fn from_parts(jump_hops: Vec<SshTransport>, mut target: SshTransport) -> Self {
        if jump_hops.iter().any(|h| h.used_interactive_auth()) {
            target.mark_used_interactive_auth();
        }
        Self {
            jump_hops: jump_hops.into_iter().map(Arc::new).collect(),
            target: Arc::new(target),
        }
    }

    /// Returns the final target `Primary Transport` (`C`).
    pub fn target(&self) -> &Arc<SshTransport> {
        &self.target
    }

    /// Returns intermediate `ProxyJump` transports ordered outermost to innermost (`[A, B, ...]`).
    pub fn jump_hops(&self) -> &[Arc<SshTransport>] {
        &self.jump_hops
    }

    /// Returns `true` if this chain traverses one or more intermediate `ProxyJump` hops.
    pub fn has_jump_hops(&self) -> bool {
        !self.jump_hops.is_empty()
    }

    /// Returns total hop count (`jump_hops.len() + 1`).
    pub fn hop_count(&self) -> usize {
        self.jump_hops.len() + 1
    }

    /// Iterates every [`SshTransport`] in the chain from outermost jump hop inward (`A -> B -> C`).
    pub fn all_hops_outer_to_inner(&self) -> impl Iterator<Item = &Arc<SshTransport>> {
        self.jump_hops.iter().chain(std::iter::once(&self.target))
    }

    /// Returns `true` if any hop (intermediate or target) used interactive authentication.
    pub fn used_interactive_auth(&self) -> bool {
        self.all_hops_outer_to_inner()
            .any(|hop| hop.used_interactive_auth())
    }

    /// Returns `true` if any hop in the chain (intermediate or target) is closed.
    pub fn any_hop_closed(&self) -> bool {
        self.all_hops_outer_to_inner().any(|hop| hop.is_closed())
    }

    /// Resolves immediately once any intermediate upstream hop (`[A, B, ...]`) disconnects or closes.
    ///
    /// Returns `(hop_index, host, port)` for the failed upstream hop. Pends forever when direct.
    pub async fn wait_upstream_closed(&self) -> (usize, String, u16) {
        if self.jump_hops.is_empty() {
            return std::future::pending().await;
        }
        if let Some((idx, hop)) = self
            .jump_hops
            .iter()
            .enumerate()
            .find(|(_, hop)| hop.is_closed())
        {
            return (idx, hop.host().to_string(), hop.port());
        }

        let mut waiters = JoinSet::new();
        for (idx, hop) in self.jump_hops.iter().enumerate() {
            let hop = Arc::clone(hop);
            waiters.spawn(async move {
                hop.wait_closed().await;
                (idx, hop.host().to_string(), hop.port())
            });
        }
        match waiters.join_next().await {
            Some(Ok(info)) => info,
            _ => (0, String::new(), 0),
        }
    }

    /// Tears down the entire chain cleanly in strict reverse order (`C -> B -> A`).
    pub async fn teardown_reverse(&self) {
        let target_was_open = !self.target.is_closed();
        self.target.disconnect_best_effort().await;
        if target_was_open && !self.jump_hops.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        for (idx, hop) in self.jump_hops.iter().rev().enumerate() {
            let was_open = !hop.is_closed();
            hop.disconnect_best_effort().await;
            if was_open && idx + 1 < self.jump_hops.len() {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }
    }
}

struct ChainBuildGuard {
    hops: Vec<SshTransport>,
}

impl ChainBuildGuard {
    fn new() -> Self {
        Self { hops: Vec::new() }
    }

    async fn teardown_reverse(&mut self) {
        while let Some(hop) = self.hops.pop() {
            hop.disconnect_best_effort().await;
        }
    }

    fn take(&mut self) -> Vec<SshTransport> {
        std::mem::take(&mut self.hops)
    }
}

impl Drop for ChainBuildGuard {
    fn drop(&mut self) {
        if self.hops.is_empty() {
            return;
        }
        let leftover = std::mem::take(&mut self.hops);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for hop in leftover.into_iter().rev() {
                    hop.disconnect_best_effort().await;
                }
            });
        }
    }
}

/// Connects and authenticates an ordered multi-hop `ProxyJump` [`SshTransportChain`]
/// (`A -> B -> C`) with per-hop credentials and strict per-hop `known_hosts` verification.
pub async fn connect_transport_chain(
    config: &SshConnectConfig,
    keepalive: Option<TcpKeepaliveSettings>,
    vault: Option<&dyn SecretStore>,
    interaction: Arc<dyn SshInteractionHandler>,
    agent_connector: Arc<dyn AgentConnector>,
) -> Result<SshTransportChain, SshError> {
    if config.proxy_jump.is_empty() {
        let transport = SshTransport::connect_direct_with_keepalive(
            config,
            keepalive,
            vault,
            interaction,
            agent_connector,
        )
        .await?;
        return Ok(SshTransportChain::direct(transport));
    }

    let (jump_hops, target) =
        connect_chain_parts(config, keepalive, vault, interaction, agent_connector).await?;
    Ok(SshTransportChain::from_parts(jump_hops, target))
}

pub(crate) async fn connect_chain_parts(
    config: &SshConnectConfig,
    keepalive: Option<TcpKeepaliveSettings>,
    vault: Option<&dyn SecretStore>,
    interaction: Arc<dyn SshInteractionHandler>,
    agent_connector: Arc<dyn AgentConnector>,
) -> Result<(Vec<SshTransport>, SshTransport), SshError> {
    let mut guard = ChainBuildGuard::new();
    let fallback_kh = config.known_hosts_path.as_deref();

    let first_cfg = config.proxy_jump[0].to_connect_config(fallback_kh);
    let first_hop = SshTransport::connect_direct_with_keepalive(
        &first_cfg,
        keepalive,
        vault,
        Arc::clone(&interaction),
        Arc::clone(&agent_connector),
    )
    .await?;
    guard.hops.push(first_hop);

    for hop in &config.proxy_jump[1..] {
        let hop_cfg = hop.to_connect_config(fallback_kh);
        let next = match connect_hop_over_prev(
            guard.hops.last().expect("at least one hop exists"),
            &hop_cfg,
            vault,
            Arc::clone(&interaction),
            Arc::clone(&agent_connector),
        )
        .await
        {
            Ok(t) => t,
            Err(err) => {
                guard.teardown_reverse().await;
                return Err(err);
            }
        };
        guard.hops.push(next);
    }

    let target = match connect_hop_over_prev(
        guard.hops.last().expect("at least one hop exists"),
        config,
        vault,
        interaction,
        agent_connector,
    )
    .await
    {
        Ok(t) => t,
        Err(err) => {
            guard.teardown_reverse().await;
            return Err(err);
        }
    };

    Ok((guard.take(), target))
}

async fn connect_hop_over_prev(
    prev: &SshTransport,
    next_cfg: &SshConnectConfig,
    vault: Option<&dyn SecretStore>,
    interaction: Arc<dyn SshInteractionHandler>,
    agent_connector: Arc<dyn AgentConnector>,
) -> Result<SshTransport, SshError> {
    let channel = prev
        .handle()
        .channel_open_direct_tcpip(
            next_cfg.host.as_str(),
            u32::from(next_cfg.port),
            "127.0.0.1",
            0,
        )
        .await
        .map_err(|err| SshError::Connection {
            host: next_cfg.host.clone(),
            port: next_cfg.port,
            reason: format!(
                "ProxyJump forward via {}:{} failed: {err}",
                prev.host(),
                prev.port()
            ),
        })?;

    SshTransport::connect_over_stream(
        channel.into_stream(),
        next_cfg,
        vault,
        interaction,
        agent_connector,
    )
    .await
}

struct ParsedHopToken {
    username: Option<String>,
    host_or_alias: String,
    port: Option<u16>,
    bracketed_ipv6: bool,
}

/// Resolves ordered `ProxyJump` specifications (`alias` or `[user@]host[:port]`, optionally
/// comma-separated) against Saved [`Profile`]s, parsed `~/.ssh/config` [`OpenSshHost`] entries,
/// and literal endpoints with default credentials.
///
/// Transitive `proxy_jump` declarations on referenced [`Profile`]s / [`OpenSshHost`]s are
/// expanded outermost-first with cycle detection.
pub fn resolve_proxy_jump_hops(
    raw_hops: &[String],
    saved_profiles: &[Profile],
    openssh_hosts: &[OpenSshHost],
    default_username: &str,
    known_hosts_path: Option<&Path>,
) -> Result<Vec<JumpHopSpec>, SshError> {
    let mut visiting = Vec::new();
    let mut resolved = Vec::new();
    resolve_hop_list(
        raw_hops,
        saved_profiles,
        openssh_hosts,
        default_username,
        known_hosts_path,
        &mut visiting,
        &mut resolved,
    )?;
    Ok(resolved)
}

fn resolve_hop_list(
    raw_hops: &[String],
    saved_profiles: &[Profile],
    openssh_hosts: &[OpenSshHost],
    default_username: &str,
    known_hosts_path: Option<&Path>,
    visiting: &mut Vec<String>,
    out: &mut Vec<JumpHopSpec>,
) -> Result<(), SshError> {
    for raw_entry in raw_hops {
        for segment in raw_entry.split(',') {
            let token = segment.trim();
            if token.is_empty() || token.eq_ignore_ascii_case("none") {
                continue;
            }
            resolve_single_hop_token(
                token,
                saved_profiles,
                openssh_hosts,
                default_username,
                known_hosts_path,
                visiting,
                out,
            )?;
        }
    }
    Ok(())
}

fn resolve_single_hop_token(
    token: &str,
    saved_profiles: &[Profile],
    openssh_hosts: &[OpenSshHost],
    default_username: &str,
    known_hosts_path: Option<&Path>,
    visiting: &mut Vec<String>,
    out: &mut Vec<JumpHopSpec>,
) -> Result<(), SshError> {
    let parsed = parse_hop_token(token)?;

    if !parsed.bracketed_ipv6
        && let Some((profile, target)) = find_ssh_profile(saved_profiles, &parsed.host_or_alias)
    {
        let cycle_key = format!("profile:{}", profile.id);
        if visiting.iter().any(|k| k == &cycle_key) {
            return Err(SshError::Connection {
                host: target.host.clone(),
                port: target.port,
                reason: format!("cyclic ProxyJump reference detected at profile '{}'", profile.name),
            });
        }

        visiting.push(cycle_key);
        let mut parent_hops = Vec::new();
        let expand_res = resolve_hop_list(
            &target.proxy_jump,
            saved_profiles,
            openssh_hosts,
            &target.username,
            known_hosts_path,
            visiting,
            &mut parent_hops,
        );
        visiting.pop();
        expand_res?;

        if !parent_hops.is_empty() && !out.ends_with(&parent_hops) {
            out.extend(parent_hops);
        }

        out.push(JumpHopSpec {
            host: target.host,
            port: parsed.port.unwrap_or(target.port),
            username: parsed.username.unwrap_or(target.username),
            auth: target.auth,
            known_hosts_path: known_hosts_path.map(Path::to_path_buf),
            nodelay: true,
        });
        return Ok(());
    }

    if !parsed.bracketed_ipv6
        && let Some(ssh_host) = openssh_hosts.iter().find(|h| h.alias == parsed.host_or_alias)
    {
        let cycle_key = format!("openssh:{}", ssh_host.alias);
        let resolved_host = ssh_host.resolved_host().to_string();
        if visiting.iter().any(|k| k == &cycle_key) {
            return Err(SshError::Connection {
                host: resolved_host,
                port: ssh_host.port,
                reason: format!(
                    "cyclic ProxyJump reference detected at OpenSSH host '{}'",
                    ssh_host.alias
                ),
            });
        }

        let effective_user = parsed
            .username
            .or_else(|| ssh_host.user.clone())
            .unwrap_or_else(|| fallback_username(default_username));

        visiting.push(cycle_key);
        let mut parent_hops = Vec::new();
        let expand_res = resolve_hop_list(
            &ssh_host.proxy_jump,
            saved_profiles,
            openssh_hosts,
            &effective_user,
            known_hosts_path,
            visiting,
            &mut parent_hops,
        );
        visiting.pop();
        expand_res?;

        if !parent_hops.is_empty() && !out.ends_with(&parent_hops) {
            out.extend(parent_hops);
        }

        let auth = match ssh_host.primary_identity_file() {
            Some(path) => SshAuthMethod::PublicKey {
                key_path: Some(path.clone()),
                vault_secret_ref: None,
            },
            None => SshAuthMethod::Agent,
        };

        out.push(JumpHopSpec {
            host: resolved_host,
            port: parsed.port.unwrap_or(ssh_host.port),
            username: effective_user,
            auth,
            known_hosts_path: known_hosts_path.map(Path::to_path_buf),
            nodelay: true,
        });
        return Ok(());
    }

    out.push(JumpHopSpec {
        host: parsed.host_or_alias,
        port: parsed.port.unwrap_or(22),
        username: parsed
            .username
            .unwrap_or_else(|| fallback_username(default_username)),
        auth: SshAuthMethod::Agent,
        known_hosts_path: known_hosts_path.map(Path::to_path_buf),
        nodelay: true,
    });
    Ok(())
}

struct MatchedProfileTarget {
    host: String,
    port: u16,
    username: String,
    auth: SshAuthMethod,
    proxy_jump: Vec<String>,
}

fn find_ssh_profile<'a>(
    profiles: &'a [Profile],
    name_or_id: &str,
) -> Option<(&'a Profile, MatchedProfileTarget)> {
    let profile = profiles
        .iter()
        .find(|p| {
            matches!(p.target, ProfileTarget::Ssh { .. })
                && (p.name == name_or_id || p.id == name_or_id)
        })?;
    let ProfileTarget::Ssh {
        host,
        port,
        username,
        auth,
        proxy_jump,
        ..
    } = &profile.target
    else {
        return None;
    };
    Some((
        profile,
        MatchedProfileTarget {
            host: host.clone(),
            port: *port,
            username: username.clone(),
            auth: auth.clone(),
            proxy_jump: proxy_jump.clone(),
        },
    ))
}

fn parse_hop_token(token: &str) -> Result<ParsedHopToken, SshError> {
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return Err(SshError::Connection {
            host: token.to_string(),
            port: 22,
            reason: format!("invalid ProxyJump hop specification: '{token}'"),
        });
    }

    let (username, host_port) = if let Some((user, rest)) = token.split_once('@') {
        if user.is_empty() || rest.is_empty() || rest.contains('@') {
            return Err(SshError::Connection {
                host: token.to_string(),
                port: 22,
                reason: format!("invalid ProxyJump user@host specification: '{token}'"),
            });
        }
        (Some(user.to_string()), rest)
    } else {
        (None, token)
    };

    if let Some(after_open) = host_port.strip_prefix('[') {
        let Some((addr, rest)) = after_open.split_once(']') else {
            return Err(SshError::Connection {
                host: token.to_string(),
                port: 22,
                reason: format!("unclosed IPv6 bracket in ProxyJump hop: '{token}'"),
            });
        };
        if addr.is_empty() {
            return Err(SshError::Connection {
                host: token.to_string(),
                port: 22,
                reason: format!("empty host in ProxyJump hop: '{token}'"),
            });
        }
        let port = if rest.is_empty() {
            None
        } else if let Some(port_str) = rest.strip_prefix(':') {
            Some(parse_valid_port(token, port_str)?)
        } else {
            return Err(SshError::Connection {
                host: token.to_string(),
                port: 22,
                reason: format!("invalid trailing characters in ProxyJump hop: '{token}'"),
            });
        };
        return Ok(ParsedHopToken {
            username,
            host_or_alias: addr.to_string(),
            port,
            bracketed_ipv6: true,
        });
    }

    let colon_count = host_port.bytes().filter(|&b| b == b':').count();
    if colon_count == 1 {
        let (host, port_str) = host_port
            .split_once(':')
            .expect("single colon checked above");
        if host.is_empty() {
            return Err(SshError::Connection {
                host: token.to_string(),
                port: 22,
                reason: format!("missing host before port in ProxyJump hop: '{token}'"),
            });
        }
        let port = parse_valid_port(token, port_str)?;
        return Ok(ParsedHopToken {
            username,
            host_or_alias: host.to_string(),
            port: Some(port),
            bracketed_ipv6: false,
        });
    }

    if host_port.is_empty() {
        return Err(SshError::Connection {
            host: token.to_string(),
            port: 22,
            reason: format!("empty host in ProxyJump hop: '{token}'"),
        });
    }

    Ok(ParsedHopToken {
        username,
        host_or_alias: host_port.to_string(),
        port: None,
        bracketed_ipv6: colon_count > 1,
    })
}

fn parse_valid_port(token: &str, raw: &str) -> Result<u16, SshError> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SshError::Connection {
            host: token.to_string(),
            port: 22,
            reason: format!("invalid port '{raw}' in ProxyJump hop '{token}'"),
        });
    }
    let port: u16 = raw.parse().map_err(|_| SshError::Connection {
        host: token.to_string(),
        port: 22,
        reason: format!("out-of-range port '{raw}' in ProxyJump hop '{token}'"),
    })?;
    if port == 0 {
        return Err(SshError::Connection {
            host: token.to_string(),
            port: 0,
            reason: format!("port 0 is invalid in ProxyJump hop '{token}'"),
        });
    }
    Ok(port)
}

fn fallback_username(default_username: &str) -> String {
    let trimmed = default_username.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string())
}
