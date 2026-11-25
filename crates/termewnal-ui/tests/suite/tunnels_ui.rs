use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use iced::window;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{
    ConfigPaths, Profile, ProfilesConfig, SshAuthMethod, TunnelDirection, TunnelRule, VaultBackend,
    VaultConfig, load_profiles, save_profiles,
    vault::{EncryptedVault, InMemorySecretStore, SecretStore, VaultSession},
};
use termewnal_ssh::{record_known_host_path};
use termewnal_ui::command_palette::PaletteAction;
use termewnal_ui::sidebar::{SidebarSection, TunnelRuntimeState};
use termewnal_ui::status_bar::StatusBarAction;
use termewnal_ui::{
    CloseGuardState, DaemonLifecycle, Message, SshTargetSpec, SshUiEvent, drive_ssh_connect,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

struct UiTunnelSrvState {
    shell_opens: AtomicUsize,
}

struct UiTunnelSrvHandler {
    state: Arc<UiTunnelSrvState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
}

impl server::Handler for UiTunnelSrvHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if user == "ops" && password == "tunnel-pw" {
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.state.shell_opens.fetch_add(1, Ordering::SeqCst);
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
        session.channel_success(channel)?;
        session.data(channel, b"TUNNEL_UI_SHELL\r\n".to_vec())?;
        Ok(())
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
        let host = host_to_connect.to_string();
        let port = u16::try_from(port_to_connect).unwrap_or(0);
        tokio::spawn(async move {
            match TcpStream::connect((host.as_str(), port)).await {
                Ok(mut target) => {
                    reply.accept().await;
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
                }
                Err(_) => {
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                }
            }
        });
        Ok(())
    }
}

struct TestSshServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<UiTunnelSrvState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for TestSshServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_ssh_server() -> TestSshServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();

    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

    let state = Arc::new(UiTunnelSrvState {
        shell_opens: AtomicUsize::new(0),
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
                        let handler = UiTunnelSrvHandler {
                            state: st,
                            channels: HashMap::new(),
                        };
                        let _ = server::run_stream(cfg, stream, handler).await;
                    });
                }
            }
        }
    });

    TestSshServer {
        addr,
        host_pubkey,
        state,
        shutdown_tx: Some(shutdown_tx),
    }
}

async fn spawn_echo_server() -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { break; };
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        loop {
                            match stream.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if stream.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
    });

    (addr, shutdown_tx)
}

fn seed_ssh_profile_with_vault(
    paths: &ConfigPaths,
    addr: SocketAddr,
    host_pubkey: &PublicKey,
    tunnels: Vec<TunnelRule>,
) -> (Profile, Arc<VaultSession>, std::path::PathBuf) {
    let secret_ref = "ssh/ui-tunnel-test";
    let mut profile = Profile::ssh(
        "Tunnel Bastion",
        addr.ip().to_string(),
        addr.port(),
        "ops",
        SshAuthMethod::Password {
            vault_secret_ref: Some(secret_ref.to_string()),
        },
    );
    profile.id = "prof-tunnel-bastion".to_string();
    profile.tunnels = tunnels;

    let mut cfg = ProfilesConfig::default();
    cfg.profiles.push(profile.clone());
    save_profiles(paths, &cfg).expect("save profiles");

    let store = Arc::new(InMemorySecretStore::new());
    store.set_secret(secret_ref, "tunnel-pw").unwrap();
    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        store,
        EncryptedVault::open(paths.vault_file()),
    ));

    let known_hosts = paths.data_local_dir().join("known_hosts");
    record_known_host_path(&addr.ip().to_string(), addr.port(), host_pubkey, &known_hosts).unwrap();

    (profile, vault, known_hosts)
}

#[tokio::test]
async fn creating_and_toggling_standalone_socks5_tunnel_updates_sidebar_status_bar_and_close_guard() {
    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let (echo_addr, _echo_stop) = spawn_echo_server().await;
    let ssh_srv = spawn_ssh_server().await;

    let (_, vault, known_hosts) =
        seed_ssh_profile_with_vault(&paths, ssh_srv.addr, &ssh_srv.host_pubkey, Vec::new());

    let mut state = super::common::boot_test_app_with_vault(paths.clone(), vault).unwrap();
    state.set_known_hosts_path(known_hosts);
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // Open Tunnel creation modal -> defaults to 127.0.0.1.
    let _ = state.update(Message::OpenTunnelCreateModal);
    assert!(state.tunnel_ui().create_form().open);
    assert_eq!(state.tunnel_ui().create_form().bind_host, "127.0.0.1");

    // Define a Standalone Dynamic SOCKS5 tunnel via form inputs.
    let _ = state.update(Message::TunnelFormNameChanged("Prod SOCKS".to_string()));
    let _ = state.update(Message::TunnelFormDirectionChanged(TunnelDirection::Dynamic));
    let _ = state.update(Message::TunnelFormProfileSelected(Some(
        "prof-tunnel-bastion".to_string(),
    )));
    let _ = state.update(Message::TunnelFormBindPortChanged("0".to_string()));
    let _ = state.update(Message::TunnelFormStandaloneToggled(true));
    let _ = state.update(Message::SubmitTunnelCreate);

    assert!(!state.tunnel_ui().create_form().open);
    assert!(state.tunnel_ui().pending_public_bind_warning().is_none());

    // Wait for TunnelManager to bind and reach Active state without opening any terminal SSH tab.
    let mut sub = state.tunnel_manager().subscribe();
    let snap = tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| s.active_standalone_count == 1 && s.active_count == 1)
            .await
            .unwrap()
            .clone()
    })
    .await
    .expect("standalone SOCKS5 tunnel reaches Active");

    let tunnel_id = snap.tunnels[0].id.clone();
    let socks_addr = snap.tunnels[0].bound_addr.expect("bound socks addr");
    assert!(socks_addr.ip().is_loopback());
    assert_eq!(ssh_srv.state.shell_opens.load(Ordering::SeqCst), 0);

    // Sync snapshot into AppState (simulating Iced subscription delivery).
    let _ = state.update(Message::TunnelManagerUpdated(snap));

    // Verify Sidebar, Status Bar, Command Palette, and Close Guard reporting.
    assert_eq!(state.sidebar().active_tunnels().len(), 1);
    assert_eq!(state.sidebar().active_tunnels()[0].direction, TunnelDirection::Dynamic);
    assert_eq!(state.sidebar().active_tunnels()[0].state, TunnelRuntimeState::Active);
    assert!(state.sidebar().active_tunnels()[0].standalone);
    assert_eq!(state.status_bar_snapshot().tunnels_text(), "Tunnels: 1 active");
    assert_eq!(state.background_work_tracker().active_standalone_tunnels(), 1);

    let palette_items = state.command_palette_items();
    assert!(palette_items.iter().any(|item| {
        matches!(&item.action, PaletteAction::ToggleTunnel(id) if id == &tunnel_id)
    }));

    // Verify real SOCKS5 traffic forwards through the UI-created Standalone tunnel.
    {
        let mut client = TcpStream::connect(socks_addr).await.unwrap();
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut greet = [0u8; 2];
        client.read_exact(&mut greet).await.unwrap();
        assert_eq!(greet, [0x05, 0x00]);
        let mut req = vec![0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1];
        req.extend_from_slice(&echo_addr.port().to_be_bytes());
        client.write_all(&req).await.unwrap();
        let mut rep = [0u8; 10];
        client.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[0..2], [0x05, 0x00]);
        client.write_all(b"ui-socks-ok").await.unwrap();
        let mut echo = [0u8; 11];
        client.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"ui-socks-ok");
    }

    // Status Bar ToggleTunnelsView opens Sidebar ActiveTunnels; View builds cleanly.
    let _ = state.update(Message::StatusBarAction(StatusBarAction::ToggleTunnelsView));
    assert!(state.sidebar().is_open());
    assert!(!state.sidebar().is_section_collapsed(SidebarSection::ActiveTunnels));
    drop(state.view(win));

    // Close Guard triggers when closing last window with active Standalone tunnel;
    // Keep Running in Background keeps the Standalone tunnel alive!
    let _ = state.update(Message::CloseRequested(win));
    assert_eq!(
        state.close_guard(),
        CloseGuardState::Confirming {
            window_id: win,
            active_standalone_tunnels: 1,
            active_transfers: 0,
        }
    );
    let _ = state.update(Message::CloseGuardKeepRunningInBackground);
    let _ = state.update(Message::WindowClosed(win));
    assert_eq!(state.lifecycle(), DaemonLifecycle::BackgroundRunning);
    assert_eq!(state.tunnel_manager().snapshot().active_standalone_count, 1);

    // Command Palette ToggleTunnel stops the tunnel -> Close Guard count drops to 0.
    let _ = state.update(Message::CommandPaletteExecuteItem(
        PaletteAction::ToggleTunnel(tunnel_id.clone()),
    ));
    let _ = state.update(Message::TunnelManagerUpdated(
        state.tunnel_manager().snapshot(),
    ));
    assert_eq!(state.background_work_tracker().active_standalone_tunnels(), 0);
    assert_eq!(
        state.sidebar().active_tunnels()[0].state,
        TunnelRuntimeState::Stopped
    );
    assert!(TcpStream::connect(socks_addr).await.is_err());

    // Re-toggle via Sidebar 1-click ToggleTunnel -> starts back up, then Stop & Quit cleans up!
    let _ = state.update(Message::ToggleTunnel(tunnel_id));
    tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| s.active_standalone_count == 1).await.unwrap();
    })
    .await
    .unwrap();
    let _ = state.update(Message::TunnelManagerUpdated(
        state.tunnel_manager().snapshot(),
    ));
    assert_eq!(state.background_work_tracker().active_standalone_tunnels(), 1);

    let _ = state.update(Message::CloseGuardStopAndQuit);
    assert_eq!(state.tunnel_manager().snapshot().active_standalone_count, 0);
    assert_eq!(state.background_work_tracker().active_standalone_tunnels(), 0);

    // Persisted on profile in profiles.toml.
    let saved = load_profiles(&paths).unwrap();
    assert_eq!(saved.profiles[0].tunnels.len(), 1);
    assert!(saved.profiles[0].tunnels[0].standalone);
}

#[tokio::test]
async fn public_bind_zero_addr_requires_explicit_security_warning_opt_in_before_binding() {
    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let (echo_addr, _echo_stop) = spawn_echo_server().await;
    let ssh_srv = spawn_ssh_server().await;

    let mut rule = TunnelRule::local("Exposed DB", 0, "127.0.0.1", echo_addr.port());
    rule.id = "tun-public-1".to_string();
    rule.bind_host = "0.0.0.0".to_string();
    rule.standalone = true;
    rule.allow_public_bind = false;

    let (_, vault, known_hosts) =
        seed_ssh_profile_with_vault(&paths, ssh_srv.addr, &ssh_srv.host_pubkey, vec![rule]);

    let mut state = super::common::boot_test_app_with_vault(paths.clone(), vault).unwrap();
    state.set_known_hosts_path(known_hosts);
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // Attempting to start 0.0.0.0 tunnel without opt-in triggers the security warning modal and does NOT bind!
    let _ = state.update(Message::ToggleTunnel("tun-public-1".to_string()));
    let warning = state
        .tunnel_ui()
        .pending_public_bind_warning()
        .expect("0.0.0.0 must require explicit security warning confirmation");
    assert!(warning.warning_text().contains("0.0.0.0"));
    assert!(warning.warning_text().contains("127.0.0.1"));
    assert!(state.tunnel_manager().snapshot().tunnels.is_empty());
    drop(state.view(win));

    // Cancelling keeps allow_public_bind = false and refuses to bind.
    let _ = state.update(Message::TunnelPublicBindCancel);
    assert!(state.tunnel_ui().pending_public_bind_warning().is_none());
    assert!(!state.profiles().profiles[0].tunnels[0].allow_public_bind);
    assert!(state.tunnel_manager().snapshot().tunnels.is_empty());

    // Confirming explicit opt-in sets allow_public_bind = true, persists, and starts/binds 0.0.0.0!
    let _ = state.update(Message::ToggleTunnel("tun-public-1".to_string()));
    assert!(state.tunnel_ui().pending_public_bind_warning().is_some());
    let _ = state.update(Message::TunnelPublicBindConfirm);
    assert!(state.tunnel_ui().pending_public_bind_warning().is_none());
    assert!(state.profiles().profiles[0].tunnels[0].allow_public_bind);

    let mut sub = state.tunnel_manager().subscribe();
    let snap = tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| s.active_count == 1).await.unwrap().clone()
    })
    .await
    .expect("opted-in 0.0.0.0 tunnel starts");
    assert!(snap.tunnels[0].bound_addr.unwrap().ip().is_unspecified());

    let saved = load_profiles(&paths).unwrap();
    assert!(saved.profiles[0].tunnels[0].allow_public_bind);

    state.tunnel_manager().stop_all();
}

#[tokio::test]
async fn session_bound_local_tunnel_starts_with_session_and_stops_when_tab_closes() {
    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let (echo_addr, _echo_stop) = spawn_echo_server().await;
    let ssh_srv = spawn_ssh_server().await;

    let mut rule = TunnelRule::local("Session DB", 0, "127.0.0.1", echo_addr.port());
    rule.id = "tun-sess-1".to_string();
    rule.standalone = false;

    let (profile, vault, known_hosts) =
        seed_ssh_profile_with_vault(&paths, ssh_srv.addr, &ssh_srv.host_pubkey, vec![rule]);

    let mut state = super::common::boot_test_app_with_vault(paths, Arc::clone(&vault)).unwrap();
    state.set_known_hosts_path(known_hosts.clone());
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let _ = state.update(Message::OpenSshProfile(profile.clone()));
    let ssh_sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(ssh_sid).unwrap().ssh_generation();
    let buffer = Arc::clone(state.ssh_session(ssh_sid).unwrap().buffer());

    let spec = SshTargetSpec::from_profile(&profile, 80, 24, 1000)
        .unwrap()
        .with_known_hosts_path(known_hosts);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(ssh_sid, gen_id, spec, buffer, vault, ev_tx));

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let connected = matches!(ev, SshUiEvent::Connected { .. });
        let _ = state.update(Message::SshEvent {
            session_id: ssh_sid,
            event: ev,
        });
        if connected {
            break;
        }
    }

    let mut sub = state.tunnel_manager().subscribe();
    let snap = tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| s.active_count == 1).await.unwrap().clone()
    })
    .await
    .expect("session-bound tunnel auto-starts on SSH connect");

    let bound_addr = snap.tunnels[0].bound_addr.expect("bound local addr");
    assert_eq!(snap.active_standalone_count, 0);

    let _ = state.update(Message::TunnelManagerUpdated(snap));
    assert_eq!(state.background_work_tracker().active_standalone_tunnels(), 0);
    assert_eq!(state.status_bar_snapshot().tunnels_text(), "Tunnels: 1 active");

    // Verify local tunnel forwards TCP traffic.
    {
        let mut c = TcpStream::connect(bound_addr).await.unwrap();
        c.write_all(b"session-tunnel-ok").await.unwrap();
        let mut buf = [0u8; 17];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"session-tunnel-ok");
    }

    // Closing the SSH tab/pane stops the session-bound tunnel and unbinds its port.
    let _ = state.update(Message::CloseFocusedPane);
    assert!(state.tunnel_manager().snapshot().tunnels.is_empty());
    assert!(TcpStream::connect(bound_addr).await.is_err());
}

#[tokio::test]
async fn tunnel_form_validates_inputs_prompts_zero_bind_and_reconnect_tunnels_skips_stopped() {
    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let (echo_addr, _echo_stop) = spawn_echo_server().await;
    let ssh_srv = spawn_ssh_server().await;

    let (_, vault, known_hosts) =
        seed_ssh_profile_with_vault(&paths, ssh_srv.addr, &ssh_srv.host_pubkey, Vec::new());

    let mut state = super::common::boot_test_app_with_vault(paths, vault).unwrap();
    state.set_known_hosts_path(known_hosts);
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // 1. Invalid port keeps form open with validation error.
    let _ = state.update(Message::OpenTunnelCreateModal);
    let _ = state.update(Message::TunnelFormBindPortChanged("not-a-port".to_string()));
    let _ = state.update(Message::SubmitTunnelCreate);
    assert!(state.tunnel_ui().create_form().open);
    assert!(state.tunnel_ui().create_form().validation_error.is_some());

    // 2. Submitting 0.0.0.0 from the creation form surfaces the public-bind security warning before binding.
    let _ = state.update(Message::TunnelFormNameChanged("LAN Forward".to_string()));
    let _ = state.update(Message::TunnelFormBindHostChanged("0.0.0.0".to_string()));
    let _ = state.update(Message::TunnelFormBindPortChanged("0".to_string()));
    let _ = state.update(Message::TunnelFormTargetHostChanged("127.0.0.1".to_string()));
    let _ = state.update(Message::TunnelFormTargetPortChanged(
        echo_addr.port().to_string(),
    ));
    let _ = state.update(Message::TunnelFormStandaloneToggled(true));
    let _ = state.update(Message::SubmitTunnelCreate);

    assert!(!state.tunnel_ui().create_form().open);
    assert!(state.tunnel_ui().pending_public_bind_warning().is_some());
    assert!(state.tunnel_manager().snapshot().tunnels.is_empty());

    // Confirm opt-in -> binds 0.0.0.0, then stop it and verify StatusBarAction::ReconnectTunnels does NOT revive Stopped tunnels.
    let _ = state.update(Message::TunnelPublicBindConfirm);
    let mut sub = state.tunnel_manager().subscribe();
    let snap = tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| s.active_count == 1).await.unwrap().clone()
    })
    .await
    .unwrap();
    let tid = snap.tunnels[0].id.clone();

    let _ = state.update(Message::ToggleTunnel(tid));
    let _ = state.update(Message::TunnelManagerUpdated(
        state.tunnel_manager().snapshot(),
    ));
    assert_eq!(
        state.sidebar().active_tunnels()[0].state,
        TunnelRuntimeState::Stopped
    );

    let _ = state.update(Message::StatusBarAction(StatusBarAction::ReconnectTunnels));
    let _ = state.update(Message::TunnelManagerUpdated(
        state.tunnel_manager().snapshot(),
    ));
    assert_eq!(
        state.sidebar().active_tunnels()[0].state,
        TunnelRuntimeState::Stopped
    );
}
