use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{
    OpenSshHost, Profile, ProfileTarget, SecretStore, SshAuthMethod, VaultBackend, VaultConfig,
    vault::{EncryptedVault, InMemorySecretStore, VaultSession},
};
use termewnal_pty::TerminalBuffer;
use termewnal_ssh::{
    BoxFuture, ConnectionSupervisor, HostKeyDecision, HostKeyPrompt, JumpHopSpec,
    KeyboardInteractiveChallenge, OsAgentConnector, SshConnectConfig, SshError,
    SshInteractionHandler, SshSession, SupervisorConfig, SupervisorEvent, TransportStatus,
    connect_transport_chain, fingerprint_sha256, record_known_host_path, resolve_proxy_jump_hops,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

#[test]
fn resolves_proxy_jump_hops_from_saved_profiles_openssh_hosts_literals_and_rejects_cycles() {
    let temp = TempDir::new().expect("temp dir");
    let known_hosts = temp.path().join("known_hosts");

    // Saved Profile bastion with Vault password secret & custom port.
    let mut saved_bastion = Profile::ssh(
        "corp-bastion",
        "198.51.100.10",
        2222,
        "bastion-user",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/corp-bastion".to_string()),
        },
    );
    saved_bastion.id = "prof-bastion".to_string();

    // Saved Profile mid-hop referencing corp-bastion transitively.
    let mut saved_mid = Profile::ssh(
        "mid-jump",
        "10.10.0.2",
        2202,
        "mid-user",
        SshAuthMethod::PublicKey {
            key_path: Some(PathBuf::from("/keys/id_mid")),
            vault_secret_ref: Some("ssh/mid-passphrase".to_string()),
        },
    );
    if let ProfileTarget::Ssh { proxy_jump, .. } = &mut saved_mid.target {
        *proxy_jump = vec!["corp-bastion".to_string()];
    }

    // Read-only ~/.ssh/config OpenSshHost entry.
    let openssh_edge = OpenSshHost {
        alias: "edge-ro".to_string(),
        host_name: Some("203.0.113.5".to_string()),
        port: 2220,
        user: Some("edge-ops".to_string()),
        identity_files: vec![PathBuf::from("/ssh/id_edge")],
        proxy_jump: Vec::new(),
        forward_agent: None,
    };

    // Mixed chain: Saved Profile ("corp-bastion") + OpenSshHost ("edge-ro") + user-overridden alias + IPv6 literal.
    let raw = vec![
        "corp-bastion,none".to_string(),
        "override@edge-ro:2299".to_string(),
        "hop3@[2001:db8::10]:2203".to_string(),
    ];
    let resolved = resolve_proxy_jump_hops(
        &raw,
        &[saved_bastion.clone(), saved_mid.clone()],
        &[openssh_edge.clone()],
        "fallback-user",
        Some(&known_hosts),
    )
    .expect("resolve mixed ProxyJump chain");

    assert_eq!(
        resolved,
        vec![
            JumpHopSpec {
                host: "198.51.100.10".to_string(),
                port: 2222,
                username: "bastion-user".to_string(),
                auth: SshAuthMethod::Password {
                    vault_secret_ref: Some("ssh/corp-bastion".to_string()),
                },
                known_hosts_path: Some(known_hosts.clone()),
                nodelay: true,
            },
            JumpHopSpec {
                host: "203.0.113.5".to_string(),
                port: 2299,
                username: "override".to_string(),
                auth: SshAuthMethod::PublicKey {
                    key_path: Some(PathBuf::from("/ssh/id_edge")),
                    vault_secret_ref: None,
                },
                known_hosts_path: Some(known_hosts.clone()),
                nodelay: true,
            },
            JumpHopSpec {
                host: "2001:db8::10".to_string(),
                port: 2203,
                username: "hop3".to_string(),
                auth: SshAuthMethod::Agent,
                known_hosts_path: Some(known_hosts.clone()),
                nodelay: true,
            },
        ]
    );

    // Transitive expansion: referencing "mid-jump" alone expands ["corp-bastion", "mid-jump"].
    let transitive = resolve_proxy_jump_hops(
        &["mid-jump".to_string()],
        &[saved_bastion.clone(), saved_mid.clone()],
        &[],
        "fallback-user",
        None,
    )
    .expect("resolve transitive jump");
    assert_eq!(transitive.len(), 2);
    assert_eq!(transitive[0].host, "198.51.100.10");
    assert_eq!(transitive[1].host, "10.10.0.2");

    // Explicit redundant chain ["corp-bastion", "mid-jump"] deduplicates the adjacent prefix.
    let explicit_and_transitive = resolve_proxy_jump_hops(
        &["corp-bastion".to_string(), "mid-jump".to_string()],
        &[saved_bastion, saved_mid],
        &[],
        "fallback-user",
        None,
    )
    .expect("avoid duplicate prefix hop");
    assert_eq!(explicit_and_transitive.len(), 2);
    assert_eq!(explicit_and_transitive[0].host, "198.51.100.10");
    assert_eq!(explicit_and_transitive[1].host, "10.10.0.2");

    // SshConnectConfig::from_profile_with_sources wires resolved hops cleanly.
    let mut target_profile = Profile::ssh(
        "app-target",
        "10.99.0.5",
        22,
        "appuser",
        SshAuthMethod::Agent,
    );
    if let ProfileTarget::Ssh { proxy_jump, .. } = &mut target_profile.target {
        *proxy_jump = vec!["edge-ro".to_string()];
    }
    let cfg = SshConnectConfig::from_profile_with_sources(
        &target_profile,
        &[],
        &[openssh_edge],
        100,
        30,
        10_000,
    )
    .expect("build SshConnectConfig with sources");
    assert_eq!(cfg.proxy_jump.len(), 1);
    assert_eq!(cfg.proxy_jump[0].host, "203.0.113.5");
    assert_eq!(cfg.proxy_jump[0].port, 2220);
    assert_eq!(cfg.proxy_jump[0].username, "edge-ops");

    // Invalid port and cyclic jump graphs fail with explicit SshError::Connection.
    let bad_port = resolve_proxy_jump_hops(&["bad:99999".to_string()], &[], &[], "ops", None);
    assert!(matches!(bad_port, Err(SshError::Connection { .. })));

    let mut cycle_a = Profile::ssh("cyc-a", "10.0.0.1", 22, "ops", SshAuthMethod::Agent);
    let mut cycle_b = Profile::ssh("cyc-b", "10.0.0.2", 22, "ops", SshAuthMethod::Agent);
    if let ProfileTarget::Ssh { proxy_jump, .. } = &mut cycle_a.target {
        *proxy_jump = vec!["cyc-b".to_string()];
    }
    if let ProfileTarget::Ssh { proxy_jump, .. } = &mut cycle_b.target {
        *proxy_jump = vec!["cyc-a".to_string()];
    }
    let cyclic = resolve_proxy_jump_hops(
        &["cyc-a".to_string()],
        &[cycle_a, cycle_b],
        &[],
        "ops",
        None,
    );
    assert!(matches!(cyclic, Err(SshError::Connection { .. })));
}

#[derive(Clone)]
enum HopAuthRule {
    PublicKey { user: String, key: PublicKey },
    Password { user: String, password: String },
}

#[derive(Clone)]
enum HopRole {
    JumpOnly { allowed_next: SocketAddr },
    TargetShell,
}

struct ChainNodeState {
    name: &'static str,
    auth: HopAuthRule,
    role: HopRole,
    events: Arc<Mutex<Vec<String>>>,
    connections: AtomicUsize,
    active_sessions: AtomicUsize,
}

struct ChainNodeHandler {
    state: Arc<ChainNodeState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    line_buf: String,
    authenticated: bool,
}

impl server::Handler for ChainNodeHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        match &self.state.auth {
            HopAuthRule::Password {
                user: expected_user,
                password: expected_pass,
            } if user == expected_user && password == expected_pass => {
                self.authenticated = true;
                self.state
                    .events
                    .lock()
                    .unwrap()
                    .push(format!("connect:{}", self.state.name));
                Ok(server::Auth::Accept)
            }
            _ => Ok(server::Auth::reject()),
        }
    }

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        _public_key: server::PublicKeyOffer<'_>,
    ) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: server::VerifiedPublicKey<'_>,
    ) -> Result<server::Auth, Self::Error> {
        match &self.state.auth {
            HopAuthRule::PublicKey {
                user: expected_user,
                key: expected_key,
            } if user == expected_user && key.public_key == expected_key => {
                self.authenticated = true;
                self.state
                    .events
                    .lock()
                    .unwrap()
                    .push(format!("connect:{}", self.state.name));
                Ok(server::Auth::Accept)
            }
            _ => Ok(server::Auth::reject()),
        }
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let HopRole::JumpOnly { allowed_next } = self.state.role else {
            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        };
        if host_to_connect != "127.0.0.1" || port_to_connect != u32::from(allowed_next.port()) {
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        }

        match TcpStream::connect(allowed_next).await {
            Ok(mut upstream_tcp) => {
                self.state
                    .events
                    .lock()
                    .unwrap()
                    .push(format!("forward:{}:{port_to_connect}", self.state.name));
                reply.accept().await;
                tokio::spawn(async move {
                    let mut ch_stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut ch_stream, &mut upstream_tcp).await;
                });
            }
            Err(_) => {
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
            }
        }
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if !matches!(self.state.role, HopRole::TargetShell) {
            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        }
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let n = self.state.connections.fetch_add(1, Ordering::SeqCst) + 1;
        session.channel_success(channel)?;
        session.data(channel, format!("TARGET_{}_READY_{n}\r\n", self.state.name).into_bytes())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if !matches!(self.state.role, HopRole::TargetShell)
            || !self.channels.contains_key(&channel)
        {
            return Ok(());
        }
        let mut echo = Vec::new();
        for &b in data {
            if b == b'\r' || b == b'\n' {
                if !self.line_buf.is_empty() {
                    let cmd = std::mem::take(&mut self.line_buf);
                    echo.extend_from_slice(format!("\r\nOUT:{}:{cmd}\r\n", self.state.name).as_bytes());
                } else {
                    echo.extend_from_slice(b"\r\n");
                }
            } else {
                self.line_buf.push(b as char);
                echo.push(b);
            }
        }
        session.data(channel, echo)?;
        Ok(())
    }
}

impl Drop for ChainNodeHandler {
    fn drop(&mut self) {
        if self.authenticated {
            self.state
                .events
                .lock()
                .unwrap()
                .push(format!("disconnect:{}", self.state.name));
        }
    }
}

struct ChainNodeServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<ChainNodeState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for ChainNodeServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_chain_node(
    name: &'static str,
    auth: HopAuthRule,
    role: HopRole,
    events: Arc<Mutex<Vec<String>>>,
) -> ChainNodeServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();

    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = match &auth {
        HopAuthRule::PublicKey { .. } => MethodSet::from(&[MethodKind::PublicKey][..]),
        HopAuthRule::Password { .. } => MethodSet::from(&[MethodKind::Password][..]),
    };
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

    let state = Arc::new(ChainNodeState {
        name,
        auth,
        role,
        events,
        connections: AtomicUsize::new(0),
        active_sessions: AtomicUsize::new(0),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let srv_state = Arc::clone(&state);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    let cfg = Arc::clone(&cfg);
                    let st = Arc::clone(&srv_state);
                    tokio::spawn(async move {
                        st.active_sessions.fetch_add(1, Ordering::SeqCst);
                        let handler = ChainNodeHandler {
                            state: Arc::clone(&st),
                            channels: HashMap::new(),
                            line_buf: String::new(),
                            authenticated: false,
                        };
                        if let Ok(running) = server::run_stream(cfg, stream, handler).await {
                            let _ = running.await;
                        }
                        st.active_sessions.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }
        }
    });

    ChainNodeServer {
        addr,
        host_pubkey,
        state,
        shutdown_tx: Some(shutdown_tx),
    }
}

struct RecordingTofuHandler {
    accept: bool,
    seen_prompts: Mutex<Vec<(String, u16, String)>>,
}

impl SshInteractionHandler for RecordingTofuHandler {
    fn verify_unknown_host(&self, prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
        self.seen_prompts.lock().unwrap().push((
            prompt.host,
            prompt.port,
            prompt.fingerprint_sha256,
        ));
        let decision = if self.accept {
            HostKeyDecision::AcceptAndSave
        } else {
            HostKeyDecision::Reject
        };
        Box::pin(async move { decision })
    }

    fn prompt_key_passphrase(&self, _key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        Box::pin(async { None })
    }

    fn prompt_password(
        &self,
        _username: &str,
        _host: &str,
    ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        Box::pin(async { None })
    }

    fn prompt_keyboard_interactive(
        &self,
        _challenge: KeyboardInteractiveChallenge,
    ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
        Box::pin(async { None })
    }
}

#[tokio::test]
async fn multihop_proxy_jump_chain_authenticates_per_hop_enforces_known_hosts_and_cleans_up_on_failure() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");

    let key_a = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_a_path = temp.path().join("id_bastion");
    let encrypted_a = key_a.encrypt(&mut rand::rng(), "bastion-pass").unwrap();
    encrypted_a.write_openssh_file(&key_a_path, LineEnding::LF).unwrap();

    let key_c = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_c_path = temp.path().join("id_target");
    key_c.write_openssh_file(&key_c_path, LineEnding::LF).unwrap();

    let vault = VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(temp.path().join("vault.enc")),
    );
    vault.set_secret("ssh/hop-a-passphrase", "bastion-pass").unwrap();
    vault.set_secret("ssh/hop-b-password", "jump-b-secret").unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let srv_c = spawn_chain_node(
        "C",
        HopAuthRule::PublicKey {
            user: "app".to_string(),
            key: key_c.public_key().clone(),
        },
        HopRole::TargetShell,
        Arc::clone(&events),
    )
    .await;
    let srv_b = spawn_chain_node(
        "B",
        HopAuthRule::Password {
            user: "jump2".to_string(),
            password: "jump-b-secret".to_string(),
        },
        HopRole::JumpOnly {
            allowed_next: srv_c.addr,
        },
        Arc::clone(&events),
    )
    .await;
    let srv_a = spawn_chain_node(
        "A",
        HopAuthRule::PublicKey {
            user: "bastion".to_string(),
            key: key_a.public_key().clone(),
        },
        HopRole::JumpOnly {
            allowed_next: srv_b.addr,
        },
        Arc::clone(&events),
    )
    .await;

    let hop_a = JumpHopSpec::new(
        "127.0.0.1",
        srv_a.addr.port(),
        "bastion",
        SshAuthMethod::PublicKey {
            key_path: Some(key_a_path),
            vault_secret_ref: Some("ssh/hop-a-passphrase".to_string()),
        },
    );
    let hop_b = JumpHopSpec::new(
        "127.0.0.1",
        srv_b.addr.port(),
        "jump2",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/hop-b-password".to_string()),
        },
    );

    let cfg = SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port: srv_c.addr.port(),
        username: "app".to_string(),
        auth: SshAuthMethod::PublicKey {
            key_path: Some(key_c_path),
            vault_secret_ref: None,
        },
        known_hosts_path: Some(known_hosts.clone()),
        proxy_jump: vec![hop_a.clone(), hop_b.clone()],
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 10_000,
    };

    // 1. Unknown hosts on A -> B -> C trigger per-hop TOFU prompts in chain order and persist keys.
    let handler = Arc::new(RecordingTofuHandler {
        accept: true,
        seen_prompts: Mutex::new(Vec::new()),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> = Arc::new(OsAgentConnector::default());
    let chain = match connect_transport_chain(&cfg, None, Some(&vault), handler.clone(), agent.clone()).await {
        Ok(c) => c,
        Err(e) => {
            let ev = events.lock().unwrap().clone();
            let prompts = handler.seen_prompts.lock().unwrap().clone();
            panic!("3-hop chain failed: {e:?}; events={ev:?}; prompts={prompts:?}");
        }
    };
    assert_eq!(chain.hop_count(), 3);
    assert_eq!(chain.jump_hops().len(), 2);

    let prompts = handler.seen_prompts.lock().unwrap().clone();
    assert_eq!(
        prompts,
        vec![
            (
                "127.0.0.1".to_string(),
                srv_a.addr.port(),
                fingerprint_sha256(&srv_a.host_pubkey)
            ),
            (
                "127.0.0.1".to_string(),
                srv_b.addr.port(),
                fingerprint_sha256(&srv_b.host_pubkey)
            ),
            (
                "127.0.0.1".to_string(),
                srv_c.addr.port(),
                fingerprint_sha256(&srv_c.host_pubkey)
            ),
        ]
    );

    // Open interactive session over Target C and run a command end-to-end.
    let session = SshSession::connect(cfg.clone(), Some(&vault), handler.clone(), agent.clone())
        .await
        .expect("SshSession over 3-hop ProxyJump");
    assert!(
        session
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("TARGET_C_READY")
            })
            .await
    );
    session.write_bytes(b"chain_ping\r").unwrap();
    assert!(
        session
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("OUT:C:chain_ping")
            })
            .await
    );

    // Explicit reverse teardown closes C -> B -> A cleanly.
    chain.teardown_reverse().await;
    drop(session);
    wait_for_zero(&srv_a.state.active_sessions, "srv_a teardown").await;
    wait_for_zero(&srv_b.state.active_sessions, "srv_b teardown").await;
    wait_for_zero(&srv_c.state.active_sessions, "srv_c teardown").await;

    let logged = events.lock().unwrap().clone();
    assert!(logged.contains(&"connect:A".to_string()));
    assert!(logged.contains(&"connect:B".to_string()));
    assert!(logged.contains(&"connect:C".to_string()));
    assert!(logged.contains(&format!("forward:A:{}", srv_b.addr.port())));
    assert!(logged.contains(&format!("forward:B:{}", srv_c.addr.port())));

    // 2. Partial auth failure on Target C cleanly tears down Bastion A and Jump B.
    events.lock().unwrap().clear();
    let bad_c_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let bad_c_path = temp.path().join("id_bad_c");
    bad_c_key.write_openssh_file(&bad_c_path, LineEnding::LF).unwrap();
    let mut bad_auth_cfg = cfg.clone();
    bad_auth_cfg.auth = SshAuthMethod::PublicKey {
        key_path: Some(bad_c_path),
        vault_secret_ref: None,
    };
    let err = connect_transport_chain(&bad_auth_cfg, None, Some(&vault), handler.clone(), agent.clone())
        .await
        .err()
        .expect("wrong Target C key must fail");
    assert!(matches!(err, SshError::AuthenticationFailed { .. }));
    wait_for_zero(&srv_a.state.active_sessions, "srv_a auth failure").await;
    wait_for_zero(&srv_b.state.active_sessions, "srv_b auth failure").await;

    // 3. MITM mismatch on intermediate Jump B is strictly blocked and tears down Bastion A.
    let rogue_b = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let rogue_kh = temp.path().join("rogue_known_hosts");
    record_known_host_path("127.0.0.1", srv_a.addr.port(), &srv_a.host_pubkey, &rogue_kh).unwrap();
    record_known_host_path(
        "127.0.0.1",
        srv_b.addr.port(),
        rogue_b.public_key(),
        &rogue_kh,
    )
    .unwrap();
    let mut mitm_cfg = cfg.clone();
    mitm_cfg.known_hosts_path = Some(rogue_kh);
    let mitm_err = connect_transport_chain(&mitm_cfg, None, Some(&vault), handler, agent)
        .await
        .err()
        .expect("MITM key on Jump B must abort");
    assert!(
        matches!(
            mitm_err,
            SshError::HostKeyMismatch { port, .. } if port == srv_b.addr.port()
        ),
        "expected HostKeyMismatch for Jump B, got {mitm_err:?}"
    );
    wait_for_zero(&srv_a.state.active_sessions, "srv_a MITM abort").await;
}

async fn wait_for_zero(counter: &AtomicUsize, label: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{label}: expected counter to reach 0, still {}",
            counter.load(Ordering::SeqCst)
        )
    });
}

struct ChainFaultProxy {
    addr: SocketAddr,
    accept_new: Arc<std::sync::atomic::AtomicBool>,
    blackhole: Arc<std::sync::atomic::AtomicBool>,
    drop_tx: watch::Sender<u64>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl ChainFaultProxy {
    fn drop_active_connections(&self) {
        let next = self.drop_tx.borrow().wrapping_add(1);
        let _ = self.drop_tx.send(next);
    }
}

impl Drop for ChainFaultProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_chain_fault_proxy(upstream: SocketAddr) -> ChainFaultProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_new = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let blackhole = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (drop_tx, drop_rx) = watch::channel(0_u64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    let accept_flag = Arc::clone(&accept_new);
    let blackhole_flag = Arc::clone(&blackhole);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((client_tcp, _)) = accepted else { break; };
                    if !accept_flag.load(Ordering::SeqCst) {
                        drop(client_tcp);
                        continue;
                    }
                    let Ok(server_tcp) = TcpStream::connect(upstream).await else {
                        drop(client_tcp);
                        continue;
                    };
                    let mut my_drop_rx = drop_rx.clone();
                    let initial_epoch = *my_drop_rx.borrow();
                    let bh = Arc::clone(&blackhole_flag);

                    tokio::spawn(async move {
                        let (mut cr, mut cw) = client_tcp.into_split();
                        let (mut sr, mut sw) = server_tcp.into_split();
                        let bh_c2s = Arc::clone(&bh);
                        let bh_s2c = Arc::clone(&bh);

                        let c2s = async move {
                            let mut buf = [0u8; 4096];
                            loop {
                                match cr.read(&mut buf).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        if bh_c2s.load(Ordering::SeqCst) {
                                            continue;
                                        }
                                        if sw.write_all(&buf[..n]).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        };
                        let s2c = async move {
                            let mut buf = [0u8; 4096];
                            loop {
                                match sr.read(&mut buf).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        if bh_s2c.load(Ordering::SeqCst) {
                                            continue;
                                        }
                                        if cw.write_all(&buf[..n]).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        };

                        let _ = tokio::select! {
                            _ = my_drop_rx.wait_for(|v| *v != initial_epoch) => {},
                            _ = c2s => {},
                            _ = s2c => {},
                        };
                    });
                }
            }
        }
    });

    ChainFaultProxy {
        addr,
        accept_new,
        blackhole,
        drop_tx,
        shutdown_tx: Some(shutdown_tx),
    }
}

async fn wait_for_condition<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn supervisor_cascade_tears_down_and_rebuilds_chain_when_bastion_drops_mid_session() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");

    let key_a = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_a_path = temp.path().join("id_bastion");
    key_a.write_openssh_file(&key_a_path, LineEnding::LF).unwrap();

    let key_c = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_c_path = temp.path().join("id_target");
    key_c.write_openssh_file(&key_c_path, LineEnding::LF).unwrap();

    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(temp.path().join("vault.enc")),
    ));
    vault.set_secret("ssh/jump-b-pass", "jump-b-pw").unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let srv_c = spawn_chain_node(
        "C",
        HopAuthRule::PublicKey {
            user: "app".to_string(),
            key: key_c.public_key().clone(),
        },
        HopRole::TargetShell,
        Arc::clone(&events),
    )
    .await;
    let srv_b = spawn_chain_node(
        "B",
        HopAuthRule::Password {
            user: "jump2".to_string(),
            password: "jump-b-pw".to_string(),
        },
        HopRole::JumpOnly {
            allowed_next: srv_c.addr,
        },
        Arc::clone(&events),
    )
    .await;
    let srv_a = spawn_chain_node(
        "A",
        HopAuthRule::PublicKey {
            user: "bastion".to_string(),
            key: key_a.public_key().clone(),
        },
        HopRole::JumpOnly {
            allowed_next: srv_b.addr,
        },
        Arc::clone(&events),
    )
    .await;
    let proxy_a = spawn_chain_fault_proxy(srv_a.addr).await;

    record_known_host_path("127.0.0.1", proxy_a.addr.port(), &srv_a.host_pubkey, &known_hosts).unwrap();
    record_known_host_path("127.0.0.1", srv_b.addr.port(), &srv_b.host_pubkey, &known_hosts).unwrap();
    record_known_host_path("127.0.0.1", srv_c.addr.port(), &srv_c.host_pubkey, &known_hosts).unwrap();

    let cfg = SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port: srv_c.addr.port(),
        username: "app".to_string(),
        auth: SshAuthMethod::PublicKey {
            key_path: Some(key_c_path),
            vault_secret_ref: None,
        },
        known_hosts_path: Some(known_hosts),
        proxy_jump: vec![
            JumpHopSpec::new(
                "127.0.0.1",
                proxy_a.addr.port(),
                "bastion",
                SshAuthMethod::PublicKey {
                    key_path: Some(key_a_path),
                    vault_secret_ref: None,
                },
            ),
            JumpHopSpec::new(
                "127.0.0.1",
                srv_b.addr.port(),
                "jump2",
                SshAuthMethod::Password {
                    vault_secret_ref: Some("ssh/jump-b-pass".to_string()),
                },
            ),
        ],
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 10_000,
    };

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    sup_cfg.backoff_min = Duration::from_millis(40);
    sup_cfg.backoff_max = Duration::from_millis(120);

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
    let handler: Arc<dyn SshInteractionHandler> = Arc::new(RecordingTofuHandler {
        accept: false,
        seen_prompts: Mutex::new(Vec::new()),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> = Arc::new(OsAgentConnector::default());

    let (supervisor, mut sup_events) = ConnectionSupervisor::connect_on_buffer_with_events(
        cfg,
        sup_cfg,
        Arc::clone(&buffer),
        Some(vault),
        handler,
        agent,
    );

    // Wait for initial Connected state and shell ready.
    let mut status_rx = supervisor.subscribe_status();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                *status_rx.borrow(),
                TransportStatus::Connected { .. }
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("initial multi-hop chain connects");

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("TARGET_C_READY_1")
            })
            .await
    );
    supervisor.write_bytes(b"BEFORE_DROP\r").unwrap();
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("OUT:C:BEFORE_DROP")
            })
            .await
    );

    // Drop Bastion A mid-session while blocking new connections briefly so we observe clean teardown.
    events.lock().unwrap().clear();
    proxy_a.accept_new.store(false, Ordering::SeqCst);
    proxy_a.drop_active_connections();

    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = sup_events.recv().await {
            if matches!(
                ev,
                SupervisorEvent::StatusChanged(ref st)
                    if matches!(st.status, TransportStatus::Reconnecting { .. })
            ) {
                break;
            }
        }
    })
    .await
    .expect("supervisor enters Reconnecting when Bastion A drops");

    assert!(
        wait_for_condition(Duration::from_secs(3), || {
            srv_a.state.active_sessions.load(Ordering::SeqCst) == 0
                && srv_b.state.active_sessions.load(Ordering::SeqCst) == 0
                && srv_c.state.active_sessions.load(Ordering::SeqCst) == 0
        })
        .await,
        "all inner hops must tear down cleanly before rebuild"
    );

    // Allow Bastion A to accept again and verify ordered rebuild A -> B -> C.
    events.lock().unwrap().clear();
    proxy_a.accept_new.store(true, Ordering::SeqCst);

    let mut saw_reconnected_event = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = sup_events.recv().await {
            if matches!(ev, SupervisorEvent::Reconnected { .. }) {
                saw_reconnected_event = true;
                break;
            }
        }
    })
    .await
    .expect("must receive Reconnected event after cascade rebuild");
    assert!(saw_reconnected_event);

    let rebuild_events = events.lock().unwrap().clone();
    let idx_a = rebuild_events
        .iter()
        .position(|e| e == "connect:A")
        .expect("connect:A recorded");
    let idx_b = rebuild_events
        .iter()
        .position(|e| e == "connect:B")
        .expect("connect:B recorded");
    let idx_c = rebuild_events
        .iter()
        .position(|e| e == "connect:C")
        .expect("connect:C recorded");
    assert!(
        idx_a < idx_b && idx_b < idx_c,
        "expected ordered rebuild A -> B -> C, got {rebuild_events:?}"
    );

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                let txt = s.visible_text();
                txt.contains("OUT:C:BEFORE_DROP")
                    && txt.contains("--- Reconnected at ")
                    && txt.contains("TARGET_C_READY_2")
            })
            .await,
        "scrollback, divider, and new shell prompt expected, got:\n{}",
        buffer.snapshot().visible_text()
    );

    supervisor.write_bytes(b"AFTER_REBUILD\r").unwrap();
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("OUT:C:AFTER_REBUILD")
            })
            .await
    );

    // Explicit disconnect tears down C -> B -> A cleanly in reverse order.
    events.lock().unwrap().clear();
    supervisor.disconnect();
    assert!(
        wait_for_condition(Duration::from_secs(3), || {
            let log = events.lock().unwrap();
            log.iter().any(|e| e == "disconnect:C")
                && log.iter().any(|e| e == "disconnect:B")
                && log.iter().any(|e| e == "disconnect:A")
        })
        .await
    );
    let teardown_log = events.lock().unwrap().clone();
    let d_c = teardown_log.iter().position(|e| e == "disconnect:C").unwrap();
    let d_b = teardown_log.iter().position(|e| e == "disconnect:B").unwrap();
    let d_a = teardown_log.iter().position(|e| e == "disconnect:A").unwrap();
    assert!(
        d_c < d_b && d_b < d_a,
        "expected reverse teardown C -> B -> A, got {teardown_log:?}"
    );
}

#[tokio::test]
async fn supervisor_cascade_tears_down_healthy_outer_hop_and_rebuilds_on_intermediate_keepalive_failure() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");

    let key_a = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_a_path = temp.path().join("id_bastion");
    key_a.write_openssh_file(&key_a_path, LineEnding::LF).unwrap();

    let key_c = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_c_path = temp.path().join("id_target");
    key_c.write_openssh_file(&key_c_path, LineEnding::LF).unwrap();

    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(temp.path().join("vault.enc")),
    ));
    vault.set_secret("ssh/jump-b-pass", "jump-b-pw").unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let srv_c = spawn_chain_node(
        "C",
        HopAuthRule::PublicKey {
            user: "app".to_string(),
            key: key_c.public_key().clone(),
        },
        HopRole::TargetShell,
        Arc::clone(&events),
    )
    .await;
    let srv_b = spawn_chain_node(
        "B",
        HopAuthRule::Password {
            user: "jump2".to_string(),
            password: "jump-b-pw".to_string(),
        },
        HopRole::JumpOnly {
            allowed_next: srv_c.addr,
        },
        Arc::clone(&events),
    )
    .await;
    // Place fault-injectable blackhole proxy between Bastion A and Jump B!
    let proxy_b = spawn_chain_fault_proxy(srv_b.addr).await;
    let srv_a = spawn_chain_node(
        "A",
        HopAuthRule::PublicKey {
            user: "bastion".to_string(),
            key: key_a.public_key().clone(),
        },
        HopRole::JumpOnly {
            allowed_next: proxy_b.addr,
        },
        Arc::clone(&events),
    )
    .await;

    record_known_host_path("127.0.0.1", srv_a.addr.port(), &srv_a.host_pubkey, &known_hosts).unwrap();
    record_known_host_path("127.0.0.1", proxy_b.addr.port(), &srv_b.host_pubkey, &known_hosts).unwrap();
    record_known_host_path("127.0.0.1", srv_c.addr.port(), &srv_c.host_pubkey, &known_hosts).unwrap();

    let cfg = SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port: srv_c.addr.port(),
        username: "app".to_string(),
        auth: SshAuthMethod::PublicKey {
            key_path: Some(key_c_path),
            vault_secret_ref: None,
        },
        known_hosts_path: Some(known_hosts),
        proxy_jump: vec![
            JumpHopSpec::new(
                "127.0.0.1",
                srv_a.addr.port(),
                "bastion",
                SshAuthMethod::PublicKey {
                    key_path: Some(key_a_path),
                    vault_secret_ref: None,
                },
            ),
            JumpHopSpec::new(
                "127.0.0.1",
                proxy_b.addr.port(),
                "jump2",
                SshAuthMethod::Password {
                    vault_secret_ref: Some("ssh/jump-b-pass".to_string()),
                },
            ),
        ],
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 10_000,
    };

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_millis(60);
    sup_cfg.keepalive_max_missed = 2;
    sup_cfg.probe_timeout = Duration::from_millis(50);
    sup_cfg.backoff_min = Duration::from_millis(40);
    sup_cfg.backoff_max = Duration::from_millis(120);

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
    let handler: Arc<dyn SshInteractionHandler> = Arc::new(RecordingTofuHandler {
        accept: false,
        seen_prompts: Mutex::new(Vec::new()),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> = Arc::new(OsAgentConnector::default());

    let (supervisor, mut sup_events) = ConnectionSupervisor::connect_on_buffer_with_events(
        cfg,
        sup_cfg,
        Arc::clone(&buffer),
        Some(vault),
        handler,
        agent,
    );

    let mut status_rx = supervisor.subscribe_status();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = sup_events.recv().await {
            if matches!(
                ev,
                SupervisorEvent::StatusChanged(ref st)
                    if matches!(st.status, TransportStatus::Connected { latency_ms: Some(_) })
            ) {
                break;
            }
        }
    })
    .await
    .expect("initial chain connects with RTT");

    // Blackhole link between Bastion A and Jump B while Bastion A stays responsive.
    proxy_b.blackhole.store(true, Ordering::SeqCst);
    proxy_b.accept_new.store(false, Ordering::SeqCst);

    let mut saw_degraded = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = sup_events.recv().await {
            if let SupervisorEvent::StatusChanged(st) = ev {
                match st.status {
                    TransportStatus::Degraded { .. } => saw_degraded = true,
                    TransportStatus::Reconnecting { .. } => break,
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("keepalive timeout on Jump B must transition through Degraded to Reconnecting");
    assert!(saw_degraded);

    // Even healthy Bastion A must have been torn down cleanly by reverse cascade teardown!
    assert!(
        wait_for_condition(Duration::from_secs(3), || {
            srv_a.state.active_sessions.load(Ordering::SeqCst) == 0
        })
        .await,
        "healthy Bastion A must tear down cleanly when intermediate hop B fails keepalive"
    );

    // Restore Jump B reachability and verify full A -> B -> C recovery.
    events.lock().unwrap().clear();
    proxy_b.blackhole.store(false, Ordering::SeqCst);
    proxy_b.accept_new.store(true, Ordering::SeqCst);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Connected { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("chain recovers cleanly after intermediate keepalive failure clears");

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("TARGET_C_READY_2")
            })
            .await
    );
    supervisor.disconnect();
}

#[cfg(unix)]
#[tokio::test]
async fn multihop_chain_supports_ssh_agent_and_interactive_prompt_per_hop_and_propagates_interactive_flag() {
    use futures::stream;
    use russh::keys::agent::client::AgentClient;
    use tokio::net::UnixListener;

    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");

    // Start real in-process UDS SSH Agent holding Bastion A's key.
    let agent_sock = temp.path().join("jump-agent.sock");
    let listener = UnixListener::bind(&agent_sock).unwrap();
    let accept_stream = stream::unfold(listener, |l| async move {
        match l.accept().await {
            Ok((stream, _)) => Some((Ok(stream), l)),
            Err(err) => Some((Err(err), l)),
        }
    });
    tokio::spawn(async move {
        let _ = russh::keys::agent::server::serve(Box::pin(accept_stream), ()).await;
    });
    let agent_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let agent_stream = tokio::net::UnixStream::connect(&agent_sock).await.unwrap();
    let mut setup_agent = AgentClient::connect(agent_stream);
    setup_agent.add_identity(&agent_key, &[]).await.unwrap();

    let key_c = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_c_path = temp.path().join("id_c_non_interactive");
    key_c.write_openssh_file(&key_c_path, LineEnding::LF).unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let srv_c = spawn_chain_node(
        "C",
        HopAuthRule::PublicKey {
            user: "app".to_string(),
            key: key_c.public_key().clone(),
        },
        HopRole::TargetShell,
        Arc::clone(&events),
    )
    .await;
    let srv_b = spawn_chain_node(
        "B",
        HopAuthRule::Password {
            user: "otpjump".to_string(),
            password: "prompted-hop-password".to_string(),
        },
        HopRole::JumpOnly {
            allowed_next: srv_c.addr,
        },
        Arc::clone(&events),
    )
    .await;
    let srv_a = spawn_chain_node(
        "A",
        HopAuthRule::PublicKey {
            user: "agentjump".to_string(),
            key: agent_key.public_key().clone(),
        },
        HopRole::JumpOnly {
            allowed_next: srv_b.addr,
        },
        Arc::clone(&events),
    )
    .await;

    record_known_host_path("127.0.0.1", srv_a.addr.port(), &srv_a.host_pubkey, &known_hosts).unwrap();
    record_known_host_path("127.0.0.1", srv_b.addr.port(), &srv_b.host_pubkey, &known_hosts).unwrap();
    record_known_host_path("127.0.0.1", srv_c.addr.port(), &srv_c.host_pubkey, &known_hosts).unwrap();

    struct InteractiveJumpPromptHandler {
        prompted_hops: Mutex<Vec<(String, String)>>,
    }

    impl SshInteractionHandler for InteractiveJumpPromptHandler {
        fn verify_unknown_host(&self, _prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async { HostKeyDecision::Reject })
        }

        fn prompt_key_passphrase(&self, _key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>> {
            Box::pin(async { None })
        }

        fn prompt_password(
            &self,
            username: &str,
            host: &str,
        ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
            self.prompted_hops
                .lock()
                .unwrap()
                .push((username.to_string(), host.to_string()));
            Box::pin(async { Some(Zeroizing::new("prompted-hop-password".to_string())) })
        }

        fn prompt_keyboard_interactive(
            &self,
            _challenge: KeyboardInteractiveChallenge,
        ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
            Box::pin(async { None })
        }
    }

    let handler = Arc::new(InteractiveJumpPromptHandler {
        prompted_hops: Mutex::new(Vec::new()),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> =
        Arc::new(OsAgentConnector::from_unix_socket(agent_sock));

    let cfg = SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port: srv_c.addr.port(),
        username: "app".to_string(),
        auth: SshAuthMethod::PublicKey {
            key_path: Some(key_c_path),
            vault_secret_ref: None,
        },
        known_hosts_path: Some(known_hosts),
        proxy_jump: vec![
            JumpHopSpec::new("127.0.0.1", srv_a.addr.port(), "agentjump", SshAuthMethod::Agent),
            JumpHopSpec::new(
                "127.0.0.1",
                srv_b.addr.port(),
                "otpjump",
                SshAuthMethod::Password {
                    vault_secret_ref: None,
                },
            ),
        ],
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 10_000,
    };

    let chain = connect_transport_chain(&cfg, None, None, handler.clone(), agent)
        .await
        .expect("Agent + interactive prompt multi-hop chain connects");
    assert!(chain.used_interactive_auth());
    assert!(chain.target().used_interactive_auth());
    assert_eq!(
        *handler.prompted_hops.lock().unwrap(),
        vec![("otpjump".to_string(), "127.0.0.1".to_string())]
    );
    chain.teardown_reverse().await;
}
