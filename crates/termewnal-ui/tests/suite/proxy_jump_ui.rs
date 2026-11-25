use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigPaths, Profile, ProfileTarget, ProfilesConfig, SecretStore, SshAuthMethod,
    VaultBackend, VaultConfig,
    vault::{EncryptedVault, InMemorySecretStore, VaultSession},
    save_profiles,
};
use termewnal_ssh::{
    JumpHopSpec, SupervisorConfig, TransportStatus, record_known_host_path,
};
use termewnal_ui::{
    LaunchDisposition, Message, SshTargetSpec, SshUiEvent, drive_ssh_connect,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};

#[test]
fn launch_profile_and_system_ssh_resolve_proxy_jump_chains_into_ssh_target_spec() {
    let temp = TempDir::new().expect("temp root");
    let ssh_dir = temp.path().join(".ssh");
    fs::create_dir_all(&ssh_dir).expect("create .ssh dir");

    let ssh_config = "\
Host bastion-ro
    HostName 203.0.113.20
    User jump-ro
    Port 2201
    IdentityFile ~/.ssh/id_bastion_ro

Host db-ro
    HostName 10.30.0.8
    User dbuser
    ProxyJump bastion-ro
";
    fs::write(ssh_dir.join("config"), ssh_config).expect("write ~/.ssh/config");

    let mut bastion_prof = Profile::ssh(
        "bastion-prof",
        "198.51.100.10",
        2222,
        "jump1",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/bastion".to_string()),
        },
    );
    bastion_prof.id = "prof-bastion".to_string();

    let mut app_prof = Profile::ssh(
        "app-prof",
        "10.20.0.5",
        22,
        "appuser",
        SshAuthMethod::Agent,
    );
    app_prof.id = "prof-app".to_string();
    if let ProfileTarget::Ssh { proxy_jump, .. } = &mut app_prof.target {
        *proxy_jump = vec![
            "bastion-prof".to_string(),
            "ops@10.1.0.2:2202".to_string(),
        ];
    }

    let paths = ConfigPaths::from_root(temp.path());
    let mut cfg = ProfilesConfig::default();
    cfg.profiles = vec![bastion_prof, app_prof.clone()];
    save_profiles(&paths, &cfg).expect("save profiles.toml");

    let mut state = super::common::boot_test_app(paths).expect("boot AppState");

    // 1. LaunchProfile resolves Saved Profile jump alias + literal user@host:port into SshConnectConfig.
    let _ = state.update(Message::LaunchProfile {
        profile_id: "prof-app".to_string(),
        disposition: LaunchDisposition::NewTab,
    });
    let app_sid = state.active_session_id().expect("app ssh session");
    let app_slot = state.ssh_session(app_sid).expect("app ssh slot");
    assert_eq!(
        app_slot.spec().connect_config.proxy_jump,
        vec![
            JumpHopSpec::new(
                "198.51.100.10",
                2222,
                "jump1",
                SshAuthMethod::Password {
                    vault_secret_ref: Some("ssh/bastion".to_string()),
                },
            ),
            JumpHopSpec::new("10.1.0.2", 2202, "ops", SshAuthMethod::Agent),
        ]
    );

    // 2. LaunchSystemSsh resolves ~/.ssh/config ProxyJump alias into JumpHopSpec with IdentityFile.
    let _ = state.update(Message::LaunchSystemSsh {
        alias: "db-ro".to_string(),
        disposition: LaunchDisposition::NewTab,
    });
    let db_sid = state.active_session_id().expect("db ssh session");
    let db_slot = state.ssh_session(db_sid).expect("db ssh slot");
    assert_eq!(
        db_slot.spec().connect_config.proxy_jump,
        vec![JumpHopSpec::new(
            "203.0.113.20",
            2201,
            "jump-ro",
            SshAuthMethod::PublicKey {
                key_path: Some(ssh_dir.join("id_bastion_ro")),
                vault_secret_ref: None,
            },
        )]
    );

    // 3. OpenSshProfile resolves proxy_jump against loaded Profiles/OpenSshHosts too.
    let _ = state.update(Message::OpenSshProfile(app_prof));
    let open_sid = state.active_session_id().expect("open profile ssh session");
    let open_slot = state.ssh_session(open_sid).expect("open slot");
    assert_eq!(open_slot.spec().connect_config.proxy_jump.len(), 2);
    assert_eq!(open_slot.spec().connect_config.proxy_jump[0].host, "198.51.100.10");
    assert_eq!(open_slot.target().proxy_jump.len(), 2);

    // 4. SshTargetSpec::from_profile_with_sources directly supports explicit source resolution.
    let direct_spec = SshTargetSpec::from_profile_with_sources(
        &state.profiles().profiles[1],
        &AppConfig::default(),
        &state.profiles().profiles,
        &[],
        80,
        24,
        10_000,
    )
    .expect("build SshTargetSpec with sources");
    assert_eq!(direct_spec.connect_config.proxy_jump.len(), 2);
}

#[derive(Clone)]
enum UiNodeRole {
    BastionForward { next: SocketAddr },
    TargetShell,
}

struct UiChainNodeState {
    user: String,
    password: String,
    role: UiNodeRole,
    connections: AtomicUsize,
}

struct UiChainNodeHandler {
    state: Arc<UiChainNodeState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    line_buf: String,
}

impl server::Handler for UiChainNodeHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if user == self.state.user && password == self.state.password {
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::reject())
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
        let UiNodeRole::BastionForward { next } = self.state.role else {
            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        };
        if host_to_connect != "127.0.0.1" || port_to_connect != u32::from(next.port()) {
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        }
        match TcpStream::connect(next).await {
            Ok(mut upstream) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut ch_stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut ch_stream, &mut upstream).await;
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
        if !matches!(self.state.role, UiNodeRole::TargetShell) {
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
        session.data(channel, format!("UI_TARGET_READY_{n}\r\n").into_bytes())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if !matches!(self.state.role, UiNodeRole::TargetShell)
            || !self.channels.contains_key(&channel)
        {
            return Ok(());
        }
        let mut echo = Vec::new();
        for &b in data {
            if b == b'\r' || b == b'\n' {
                if !self.line_buf.is_empty() {
                    let cmd = std::mem::take(&mut self.line_buf);
                    echo.extend_from_slice(format!("\r\nUI_ECHO:{cmd}\r\n").as_bytes());
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

struct UiNodeServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for UiNodeServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_ui_node(user: &str, password: &str, role: UiNodeRole) -> UiNodeServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();

    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

    let state = Arc::new(UiChainNodeState {
        user: user.to_string(),
        password: password.to_string(),
        role,
        connections: AtomicUsize::new(0),
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
                        let handler = UiChainNodeHandler {
                            state: st,
                            channels: HashMap::new(),
                            line_buf: String::new(),
                        };
                        if let Ok(running) = server::run_stream(cfg, stream, handler).await {
                            let _ = running.await;
                        }
                    });
                }
            }
        }
    });

    UiNodeServer {
        addr,
        host_pubkey,
        shutdown_tx: Some(shutdown_tx),
    }
}

struct UiDropProxy {
    addr: SocketAddr,
    accept_new: Arc<AtomicBool>,
    drop_tx: watch::Sender<u64>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl UiDropProxy {
    fn drop_active(&self) {
        let next = self.drop_tx.borrow().wrapping_add(1);
        let _ = self.drop_tx.send(next);
    }
}

impl Drop for UiDropProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_ui_drop_proxy(upstream: SocketAddr) -> UiDropProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_new = Arc::new(AtomicBool::new(true));
    let (drop_tx, drop_rx) = watch::channel(0_u64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let accept_flag = Arc::clone(&accept_new);

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
                    let epoch = *my_drop_rx.borrow();
                    tokio::spawn(async move {
                        let (mut cr, mut cw) = client_tcp.into_split();
                        let (mut sr, mut sw) = server_tcp.into_split();
                        let c2s = async move {
                            let mut buf = [0u8; 4096];
                            while let Ok(n) = cr.read(&mut buf).await {
                                if n == 0 || sw.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        };
                        let s2c = async move {
                            let mut buf = [0u8; 4096];
                            while let Ok(n) = sr.read(&mut buf).await {
                                if n == 0 || cw.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        };
                        let _ = tokio::select! {
                            _ = my_drop_rx.wait_for(|v| *v != epoch) => {},
                            _ = c2s => {},
                            _ = s2c => {},
                        };
                    });
                }
            }
        }
    });

    UiDropProxy {
        addr,
        accept_new,
        drop_tx,
        shutdown_tx: Some(shutdown_tx),
    }
}

#[tokio::test]
async fn ui_drive_ssh_connect_executes_multihop_tofu_and_cascade_recovery_through_app_state() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let paths = ConfigPaths::from_root(temp.path());

    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    vault.set_secret("ssh/ui-bastion", "bastion-pw").unwrap();
    vault.set_secret("ssh/ui-target", "target-pw").unwrap();

    let srv_target = spawn_ui_node("app", "target-pw", UiNodeRole::TargetShell).await;
    let srv_bastion = spawn_ui_node(
        "jump",
        "bastion-pw",
        UiNodeRole::BastionForward {
            next: srv_target.addr,
        },
    )
    .await;
    let proxy_bastion = spawn_ui_drop_proxy(srv_bastion.addr).await;

    // Pre-trust Target C only; Bastion A triggers interactive TOFU modal through AppState!
    record_known_host_path(
        "127.0.0.1",
        srv_target.addr.port(),
        &srv_target.host_pubkey,
        &known_hosts,
    )
    .unwrap();

    let mut bastion_prof = Profile::ssh(
        "ui-bastion",
        "127.0.0.1",
        proxy_bastion.addr.port(),
        "jump",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/ui-bastion".to_string()),
        },
    );
    bastion_prof.id = "ui-bastion-id".to_string();

    let mut target_prof = Profile::ssh(
        "ui-target",
        "127.0.0.1",
        srv_target.addr.port(),
        "app",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/ui-target".to_string()),
        },
    );
    target_prof.id = "ui-target-id".to_string();
    if let ProfileTarget::Ssh { proxy_jump, .. } = &mut target_prof.target {
        *proxy_jump = vec!["ui-bastion".to_string()];
    }

    let mut profiles_cfg = ProfilesConfig::default();
    profiles_cfg.profiles = vec![bastion_prof, target_prof];
    save_profiles(&paths, &profiles_cfg).unwrap();

    let mut state = super::common::boot_test_app_with_vault(paths, Arc::clone(&vault)).unwrap();

    let _ = state.update(Message::LaunchProfile {
        profile_id: "ui-target-id".to_string(),
        disposition: LaunchDisposition::NewTab,
    });
    let sid = state.active_session_id().expect("launched SSH session");
    let slot = state.ssh_session(sid).expect("SSH slot");
    assert_eq!(slot.spec().connect_config.proxy_jump.len(), 1);
    assert_eq!(
        slot.spec().connect_config.proxy_jump[0].port,
        proxy_bastion.addr.port()
    );

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    sup_cfg.backoff_min = Duration::from_millis(40);
    sup_cfg.backoff_max = Duration::from_millis(120);

    let spec = slot
        .spec()
        .clone()
        .with_known_hosts_path(&known_hosts)
        .with_supervisor_config(sup_cfg);
    let gen_id = slot.ssh_generation();
    let buffer = Arc::clone(slot.buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        sid,
        gen_id,
        spec,
        Arc::clone(&buffer),
        Arc::clone(&vault),
        ev_tx,
    ));

    // Handle Bastion TOFU prompt through AppState and reach Connected.
    let mut saw_bastion_tofu = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = ev_rx.recv().await {
            let is_tofu = matches!(
                ev,
                SshUiEvent::HostKeyPrompt { ref prompt, .. }
                    if prompt.port == proxy_bastion.addr.port()
            );
            let _ = state.update(Message::SshEvent {
                session_id: sid,
                event: ev,
            });
            if is_tofu {
                saw_bastion_tofu = true;
                let _ = state.update(Message::RespondHostKeyPrompt {
                    session_id: sid,
                    accept: true,
                });
            }
            if matches!(
                state.ssh_session(sid).unwrap().transport_status(),
                TransportStatus::Connected { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("multi-hop UI session connects after Bastion TOFU");
    assert!(saw_bastion_tofu);

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("UI_TARGET_READY_1")
            })
            .await
    );
    let _ = state.update(Message::TerminalInput {
        session_id: sid,
        bytes: b"UI_KEEP_LINE\r".to_vec(),
    });
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("UI_ECHO:UI_KEEP_LINE")
            })
            .await
    );

    // Drop Bastion mid-session -> Status Bar enters Reconnecting -> rebuilds chain and preserves scrollback.
    proxy_bastion.accept_new.store(false, Ordering::SeqCst);
    proxy_bastion.drop_active();

    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = ev_rx.recv().await {
            let _ = state.update(Message::SshEvent {
                session_id: sid,
                event: ev,
            });
            if matches!(
                state.ssh_session(sid).unwrap().transport_status(),
                TransportStatus::Reconnecting { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("UI Status Bar observes Reconnecting on Bastion drop");

    proxy_bastion.accept_new.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = ev_rx.recv().await {
            let _ = state.update(Message::SshEvent {
                session_id: sid,
                event: ev,
            });
            if matches!(
                state.ssh_session(sid).unwrap().transport_status(),
                TransportStatus::Connected { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("UI multi-hop session recovers after Bastion returns");

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                let txt = s.visible_text();
                txt.contains("UI_ECHO:UI_KEEP_LINE")
                    && txt.contains("--- Reconnected at ")
                    && txt.contains("UI_TARGET_READY_2")
            })
            .await
    );

    let _ = state.update(Message::DisconnectSshSession(sid));
    assert_eq!(
        state.ssh_session(sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
}
