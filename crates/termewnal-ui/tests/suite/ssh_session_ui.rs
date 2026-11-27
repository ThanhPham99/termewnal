use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::window;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigPaths, Profile, SshAuthMethod, VaultBackend, VaultConfig,
    vault::{EncryptedVault, InMemorySecretStore, SecretStore, VaultSession},
};
use termewnal_pty::PtyEvent;
use termewnal_ssh::{
    HostKeyDecision, HostKeyPrompt, InteractivePromptItem, KeyboardInteractiveChallenge,
    SshConnectConfig, SupervisorConfig, TransportStatus, fingerprint_sha256,
    record_known_host_path,
};
use termewnal_ui::{
    Message, MitmAlert, SshAuthPrompt, SshTargetSpec, SshUiEvent, StatusBarAction,
    drive_ssh_connect,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};

#[test]
fn restored_ssh_sessions_respect_auto_connect_on_startup_and_trigger_on_enter_or_click() {
    let temp = TempDir::new().unwrap();
    let mut state = super::common::boot_test_app(ConfigPaths::from_root(temp.path())).unwrap();
    let win = window::Id::unique();

    // 1. Default auto_connect_on_startup = false -> restored session stays Disconnected.
    let profile_manual = Profile::ssh(
        "Manual Host",
        "127.0.0.1",
        2222,
        "ops",
        SshAuthMethod::Agent,
    );
    let _ = state.update(Message::RestoreSshProfile(profile_manual.clone()));
    let manual_sid = state.active_session_id().unwrap();
    assert!(state.session(manual_sid).is_none());
    assert_eq!(
        state.ssh_session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    drop(state.view(win));

    // Typing ordinary character while Disconnected does not start connecting.
    let _ = state.update(Message::TerminalInput {
        session_id: manual_sid,
        bytes: b"a".to_vec(),
    });
    assert_eq!(
        state.ssh_session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );

    // Pressing Enter initiates Connecting.
    let _ = state.update(Message::TerminalInput {
        session_id: manual_sid,
        bytes: b"\r".to_vec(),
    });
    assert_eq!(
        state.ssh_session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );

    // 2. Second manual session triggered via explicit click (ConnectSshSession).
    let _ = state.update(Message::RestoreSshProfile(profile_manual));
    let click_sid = state.active_session_id().unwrap();
    assert_eq!(
        state.ssh_session(click_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    let _ = state.update(Message::ConnectSshSession(click_sid));
    assert_eq!(
        state.ssh_session(click_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );

    // 3. auto_connect_on_startup = true -> starts Connecting immediately on restore.
    let mut profile_auto = Profile::ssh(
        "Auto Host",
        "127.0.0.1",
        2223,
        "ops",
        SshAuthMethod::Agent,
    );
    profile_auto.set_auto_connect_on_startup(true);
    let _ = state.update(Message::RestoreSshProfile(profile_auto));
    let auto_sid = state.active_session_id().unwrap();
    assert_eq!(
        state.ssh_session(auto_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );
}

#[test]
fn tofu_prompt_accept_and_reject_resolve_host_key_decisions_and_clear_modal() {
    let temp = TempDir::new().unwrap();
    let mut state = super::common::boot_test_app(ConfigPaths::from_root(temp.path())).unwrap();
    let win = window::Id::unique();

    let profile = Profile::ssh("TOFU Host", "db.internal", 22, "admin", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();

    // First prompt -> Reject.
    let (tx1, mut rx1) = oneshot::channel();
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::HostKeyPrompt {
            generation: gen_id,
            prompt: HostKeyPrompt {
                host: "db.internal".to_string(),
                port: 22,
                key_type: "ssh-ed25519".to_string(),
                fingerprint_sha256: "SHA256:testfingerprint1".to_string(),
            },
            responder: Arc::new(Mutex::new(Some(tx1))),
        },
    });
    assert_eq!(
        state
            .ssh_ui()
            .active_host_key_prompt()
            .map(|p| p.fingerprint_sha256.as_str()),
        Some("SHA256:testfingerprint1")
    );
    drop(state.view(win));

    let _ = state.update(Message::RespondHostKeyPrompt {
        session_id: sid,
        accept: false,
    });
    assert_eq!(rx1.try_recv().unwrap(), HostKeyDecision::Reject);
    assert!(state.ssh_ui().active_host_key_prompt().is_none());

    // Second prompt -> AcceptAndSave.
    let (tx2, mut rx2) = oneshot::channel();
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::HostKeyPrompt {
            generation: gen_id,
            prompt: HostKeyPrompt {
                host: "db.internal".to_string(),
                port: 22,
                key_type: "ssh-ed25519".to_string(),
                fingerprint_sha256: "SHA256:testfingerprint2".to_string(),
            },
            responder: Arc::new(Mutex::new(Some(tx2))),
        },
    });
    let _ = state.update(Message::RespondHostKeyPrompt {
        session_id: sid,
        accept: true,
    });
    assert_eq!(rx2.try_recv().unwrap(), HostKeyDecision::AcceptAndSave);
    assert!(state.ssh_ui().active_host_key_prompt().is_none());
}

#[test]
fn mitm_alert_blocks_session_displays_fingerprints_and_can_be_dismissed() {
    let temp = TempDir::new().unwrap();
    let mut state = super::common::boot_test_app(ConfigPaths::from_root(temp.path())).unwrap();
    let win = window::Id::unique();

    let profile = Profile::ssh("Prod", "prod.example.com", 22, "root", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();

    let alert = MitmAlert {
        session_id: sid,
        host: "prod.example.com".to_string(),
        port: 22,
        line: 7,
        key_type: "ssh-ed25519".to_string(),
        expected_fingerprint: "SHA256:expectedAAA".to_string(),
        received_fingerprint: "SHA256:evilBBB".to_string(),
    };
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::MitmBlocked {
            generation: gen_id,
            alert: alert.clone(),
        },
    });

    assert_eq!(
        state.ssh_session(sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    assert_eq!(state.ssh_ui().mitm_alert(sid), Some(&alert));
    assert!(state.last_error().is_some_and(|e| e.contains("HOST KEY VERIFICATION FAILED")));
    drop(state.view(win));

    let _ = state.update(Message::DismissMitmAlert(sid));
    assert!(state.ssh_ui().mitm_alert(sid).is_none());
}

#[test]
fn interactive_2fa_prompt_updates_drafts_submits_answers_and_supports_cancel() {
    let temp = TempDir::new().unwrap();
    let mut state = super::common::boot_test_app(ConfigPaths::from_root(temp.path())).unwrap();
    let win = window::Id::unique();

    let profile = Profile::ssh(
        "2FA Host",
        "vault.internal",
        22,
        "secops",
        SshAuthMethod::KeyboardInteractive,
    );
    let _ = state.update(Message::OpenSshProfile(profile));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();

    let (tx1, mut rx1) = oneshot::channel();
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::AuthPrompt {
            generation: gen_id,
            prompt: SshAuthPrompt::KeyboardInteractive(KeyboardInteractiveChallenge {
                name: "Duo 2FA".to_string(),
                instructions: "Enter PIN and OTP".to_string(),
                prompts: vec![
                    InteractivePromptItem {
                        prompt: "PIN: ".to_string(),
                        echo: false,
                    },
                    InteractivePromptItem {
                        prompt: "OTP: ".to_string(),
                        echo: true,
                    },
                ],
            }),
            responder: Arc::new(Mutex::new(Some(tx1))),
        },
    });
    assert!(state.ssh_ui().active_auth_prompt().is_some());
    drop(state.view(win));

    let _ = state.update(Message::SshAuthInputChanged {
        session_id: sid,
        index: 0,
        value: "1234".to_string(),
    });
    let _ = state.update(Message::SshAuthInputChanged {
        session_id: sid,
        index: 1,
        value: "654321".to_string(),
    });
    let _ = state.update(Message::SubmitSshAuthPrompt(sid));

    let answers = rx1.try_recv().unwrap().expect("submitted answers");
    let plain: Vec<String> = answers.into_iter().map(|z| (*z).clone()).collect();
    assert_eq!(plain, vec!["1234".to_string(), "654321".to_string()]);
    assert!(state.ssh_ui().active_auth_prompt().is_none());

    // Verify cancel sends None.
    let (tx2, mut rx2) = oneshot::channel();
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::AuthPrompt {
            generation: gen_id,
            prompt: SshAuthPrompt::Password {
                username: "secops".to_string(),
                host: "vault.internal".to_string(),
            },
            responder: Arc::new(Mutex::new(Some(tx2))),
        },
    });
    let _ = state.update(Message::CancelSshAuthPrompt(sid));
    assert!(rx2.try_recv().unwrap().is_none());
    assert!(state.ssh_ui().active_auth_prompt().is_none());
}

struct UiTestServer {
    expected_user: String,
    expected_password: String,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
}

impl server::Handler for UiTestServer {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if user == self.expected_user && password == self.expected_password {
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
        session.data(channel, b"UI_WELCOME_SSH\r\n".to_vec())?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, format!("UI_RESIZE:{col_width}x{row_height}\r\n").into_bytes())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let mut echoed = Vec::new();
        for &b in data {
            if b == b'\r' {
                echoed.extend_from_slice(b"\r\n");
            } else {
                echoed.push(b);
            }
        }
        session.data(channel, echoed)?;
        Ok(())
    }
}

async fn spawn_ui_ssh_server(user: &str, password: &str) -> (SocketAddr, PublicKey) {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let pubkey = key.public_key().clone();
    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(key);
    let cfg = Arc::new(cfg);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let u = user.to_string();
    let p = password.to_string();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let cfg = Arc::clone(&cfg);
            let handler = UiTestServer {
                expected_user: u.clone(),
                expected_password: p.clone(),
                channels: HashMap::new(),
            };
            tokio::spawn(async move {
                let _ = server::run_stream(cfg, stream, handler).await;
            });
        }
    });

    (addr, pubkey)
}

#[tokio::test]
async fn end_to_end_app_state_drives_tofu_vault_auth_terminal_io_resize_and_mitm_block() {
    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let known_hosts = temp.path().join("known_hosts");

    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    vault.set_secret("ssh/ui-test", "ui-pass").unwrap();

    let mut state = super::common::boot_test_app_with_vault(paths, Arc::clone(&vault)).unwrap();
    let (addr, server_pubkey) = spawn_ui_ssh_server("deploy", "ui-pass").await;

    let spec = SshTargetSpec::quick_connect(
        "UI Live SSH",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            username: "deploy".to_string(),
            auth: SshAuthMethod::Password {
                vault_secret_ref: Some("ssh/ui-test".to_string()),
            },
            known_hosts_path: Some(known_hosts.clone()),
            proxy_jump: Vec::new(),
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            ..SshConnectConfig::default()
        },
    )
    .with_known_hosts_path(&known_hosts);

    let (sid, _task) = state.open_ssh_session(spec.clone(), true);
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
    let buffer = Arc::clone(state.ssh_session(sid).unwrap().buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        sid,
        gen_id,
        spec.clone(),
        Arc::clone(&buffer),
        Arc::clone(&vault),
        ev_tx,
    ));

    // 1. Unknown host triggers TOFU prompt; accept & save.
    let tofu_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: tofu_ev,
    });
    assert_eq!(
        state
            .ssh_ui()
            .active_host_key_prompt()
            .map(|p| p.fingerprint_sha256.clone()),
        Some(fingerprint_sha256(&server_pubkey))
    );
    let _ = state.update(Message::RespondHostKeyPrompt {
        session_id: sid,
        accept: true,
    });

    // 2. Connection succeeds using Vault password.
    let conn_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: conn_ev,
    });
    assert_eq!(
        state.ssh_session(sid).unwrap().transport_status(),
        &TransportStatus::Connected { latency_ms: None }
    );
    assert!(known_hosts.exists());

    // 3. Verify shell output, interactive TerminalInput, and ViewportResized propagate through AppState.
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("UI_WELCOME_SSH")
            })
            .await
    );
    let _ = state.update(Message::TerminalInput {
        session_id: sid,
        bytes: b"ui_echo\r".to_vec(),
    });
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("ui_echo")
            })
            .await
    );
    let _ = state.update(Message::ViewportResized {
        session_id: sid,
        cols: 100,
        rows: 35,
    });
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("UI_RESIZE:100x35")
            })
            .await
    );
    let _ = state.update(Message::PtyEvent {
        session_id: sid,
        event: PtyEvent::Output,
    });
    assert!(
        state
            .ssh_session(sid)
            .unwrap()
            .snapshot()
            .visible_text()
            .contains("UI_RESIZE:100x35")
    );
    let _ = state.update(Message::DisconnectSshSession(sid));
    assert_eq!(
        state.ssh_session(sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );

    // 4. Replace known_hosts with a wrong key -> MitmBlocked hard block.
    std::fs::remove_file(&known_hosts).unwrap();
    let wrong_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .public_key()
        .clone();
    record_known_host_path("127.0.0.1", addr.port(), &wrong_key, &known_hosts).unwrap();

    let (mitm_sid, _task2) = state.open_ssh_session(spec.clone(), true);
    let mitm_gen = state.ssh_session(mitm_sid).unwrap().ssh_generation();
    let mitm_buf = Arc::clone(state.ssh_session(mitm_sid).unwrap().buffer());
    let (ev_tx2, mut ev_rx2) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        mitm_sid,
        mitm_gen,
        spec,
        mitm_buf,
        vault,
        ev_tx2,
    ));

    let mitm_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx2.recv())
        .await
        .unwrap()
        .unwrap();
    let _ = state.update(Message::SshEvent {
        session_id: mitm_sid,
        event: mitm_ev,
    });
    assert!(state.ssh_ui().mitm_alert(mitm_sid).is_some());
    assert_eq!(
        state.ssh_session(mitm_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
}

struct SupervisedUiProxy {
    addr: SocketAddr,
    blackhole_active: Arc<AtomicBool>,
    accept_new: Arc<AtomicBool>,
    drop_tx: watch::Sender<u64>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl SupervisedUiProxy {
    fn drop_active_connections(&self) {
        let next = *self.drop_tx.borrow() + 1;
        let _ = self.drop_tx.send(next);
    }
}

impl Drop for SupervisedUiProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_supervised_ui_proxy(target: SocketAddr) -> SupervisedUiProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let blackhole_active = Arc::new(AtomicBool::new(false));
    let accept_new = Arc::new(AtomicBool::new(true));
    let (drop_tx, drop_rx) = watch::channel(0_u64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    let bh = Arc::clone(&blackhole_active);
    let accept_flag = Arc::clone(&accept_new);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = listener.accept() => {
                    let Ok((client, _)) = res else { break; };
                    if !accept_flag.load(Ordering::SeqCst) {
                        drop(client);
                        continue;
                    }
                    let Ok(upstream) = TcpStream::connect(target).await else {
                        drop(client);
                        continue;
                    };
                    let bh = Arc::clone(&bh);
                    let mut drop_rx = drop_rx.clone();
                    let conn_gen = *drop_rx.borrow_and_update();
                    tokio::spawn(async move {
                        let (mut c_rx, mut c_tx) = client.into_split();
                        let (mut u_rx, mut u_tx) = upstream.into_split();
                        let mut c_buf = [0u8; 8192];
                        let mut u_buf = [0u8; 8192];
                        loop {
                            if *drop_rx.borrow_and_update() != conn_gen {
                                break;
                            }
                            tokio::select! {
                                _ = drop_rx.changed() => break,
                                n = c_rx.read(&mut c_buf) => {
                                    match n {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if !bh.load(Ordering::SeqCst)
                                                && u_tx.write_all(&c_buf[..n]).await.is_err()
                                            {
                                                break;
                                            }
                                        }
                                    }
                                }
                                n = u_rx.read(&mut u_buf) => {
                                    match n {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if !bh.load(Ordering::SeqCst)
                                                && c_tx.write_all(&u_buf[..n]).await.is_err()
                                            {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
    });

    SupervisedUiProxy {
        addr,
        blackhole_active,
        accept_new,
        drop_tx,
        shutdown_tx: Some(shutdown_tx),
    }
}

#[tokio::test]
async fn supervised_ui_lifecycle_drives_latency_degraded_cancel_reconnect_scrollback_and_mux() {
    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let known_hosts = temp.path().join("known_hosts");

    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    vault.set_secret("ssh/sup-ui", "sup-pass").unwrap();

    let mut state = super::common::boot_test_app_with_vault(paths, Arc::clone(&vault)).unwrap();
    let (srv_addr, server_pubkey) = spawn_ui_ssh_server("deploy", "sup-pass").await;
    let proxy = spawn_supervised_ui_proxy(srv_addr).await;
    record_known_host_path("127.0.0.1", proxy.addr.port(), &server_pubkey, &known_hosts).unwrap();

    let mut profile = Profile::ssh(
        "Supervised UI",
        "127.0.0.1",
        proxy.addr.port(),
        "deploy",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/sup-ui".to_string()),
        },
    );
    profile.set_mux_attach(true, Some("tmux attach -t ui-work".to_string()));

    let mut sup_cfg = SupervisorConfig::from_configs(&AppConfig::default(), &profile);
    sup_cfg.keepalive_interval = Duration::from_millis(60);
    sup_cfg.keepalive_max_missed = 3;
    sup_cfg.probe_timeout = Duration::from_millis(50);
    sup_cfg.backoff_min = Duration::from_millis(50);
    sup_cfg.backoff_max = Duration::from_millis(150);

    let spec = SshTargetSpec::from_profile_with_config(
        &profile,
        &AppConfig::default(),
        80,
        24,
        10_000,
    )
    .unwrap()
    .with_known_hosts_path(&known_hosts)
    .with_supervisor_config(sup_cfg);

    let (sid, _task) = state.open_ssh_session(spec.clone(), true);
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
    let buffer = Arc::clone(state.ssh_session(sid).unwrap().buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        sid,
        gen_id,
        spec,
        Arc::clone(&buffer),
        Arc::clone(&vault),
        ev_tx,
    ));

    // Wait for initial Connected + RTT latency update in Status Bar.
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = ev_rx.recv().await {
            let _ = state.update(Message::SshEvent {
                session_id: sid,
                event: ev,
            });
            if matches!(
                state.ssh_session(sid).unwrap().transport_status(),
                TransportStatus::Connected {
                    latency_ms: Some(_)
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("must reach Connected with RTT latency");
    assert!(
        state
            .status_bar_snapshot()
            .transport_text()
            .starts_with("Transport: Connected (")
    );
    assert!(state.ssh_session(sid).unwrap().supervisor().is_some());

    // Produce terminal output before drop.
    let _ = state.update(Message::TerminalInput {
        session_id: sid,
        bytes: b"UI_SCROLLBACK_KEEP\r".to_vec(),
    });
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("UI_SCROLLBACK_KEEP")
            })
            .await
    );

    // Inject blackhole -> Status Bar transitions through Degraded -> Reconnecting with controls.
    proxy.blackhole_active.store(true, Ordering::SeqCst);
    let mut saw_degraded = false;
    let mut saw_reconnecting = false;
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = ev_rx.recv().await {
            let _ = state.update(Message::SshEvent {
                session_id: sid,
                event: ev,
            });
            let snap = state.status_bar_snapshot();
            match state.ssh_session(sid).unwrap().transport_status() {
                TransportStatus::Degraded { .. } => {
                    saw_degraded = true;
                    assert!(snap.shows_reconnect_controls());
                }
                TransportStatus::Reconnecting { .. } => {
                    saw_reconnecting = true;
                    assert!(snap.shows_reconnect_controls());
                    assert!(snap.transport_text().contains("Reconnecting #"));
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("must observe Degraded and Reconnecting in Status Bar");
    assert!(saw_degraded && saw_reconnecting);

    // Clear blackhole and observe recovery + divider + mux attach + preserved scrollback.
    proxy.blackhole_active.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), async {
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
    .expect("must recover after blackhole cleared");

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                let txt = s.visible_text();
                txt.contains("UI_SCROLLBACK_KEEP")
                    && txt.contains("--- Reconnected at ")
                    && txt.contains("tmux attach -t ui-work")
            })
            .await,
        "expected preserved scrollback + divider + mux command, got:\n{}",
        buffer.snapshot().visible_text()
    );

    // Drop again with accept_new=false, cancel via Status Bar Cancel button -> Disconnected,
    // then trigger StatusBarAction::ReconnectSession -> recovers!
    proxy.accept_new.store(false, Ordering::SeqCst);
    proxy.drop_active_connections();
    tokio::time::timeout(Duration::from_secs(3), async {
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
    .expect("must enter Reconnecting after drop");

    let _ = state.update(Message::StatusBarAction(StatusBarAction::CancelReconnect(
        sid,
    )));
    assert_eq!(
        state.ssh_session(sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    assert!(buffer.snapshot().visible_text().contains("UI_SCROLLBACK_KEEP"));

    proxy.accept_new.store(true, Ordering::SeqCst);
    let _ = state.update(Message::StatusBarAction(StatusBarAction::ReconnectSession(
        sid,
    )));
    assert_eq!(
        state.ssh_session(sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );

    tokio::time::timeout(Duration::from_secs(3), async {
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
    .expect("ReconnectSession must recover supervised session");

    let _ = state.update(Message::DisconnectSshSession(sid));
    assert_eq!(
        state.ssh_session(sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
}


#[tokio::test]
async fn ssh_connection_ui_displays_auto_reconnecting_and_reconnected_successfully() {
    use crate::common::boot_test_app;
    use termewnal_ui::{RecoveryNoticeKind, SshUiEvent};

    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = boot_test_app(paths).unwrap();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let spec = SshTargetSpec::quick_connect(
        "Reconnect Banner Host",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: 2222,
            username: "ops".to_string(),
            auth: SshAuthMethod::Password {
                vault_secret_ref: None,
            },
            known_hosts_path: None,
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 1000,
            proxy_jump: Vec::new(),
            ..SshConnectConfig::default()
        },
    );

    let _ = state.update(Message::QuickConnectSsh(spec));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();

    // 1. Initial connection establishes session.
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::Connected {
            generation: gen_id,
            session: None,
            supervisor: None,
        },
    });
    assert!(state.status_bar_snapshot().connection_notice.is_none());

    // 2. Connection drop -> Auto-reconnecting status must persist across Connecting retries.
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::TransportStatus {
            generation: gen_id,
            status: TransportStatus::Reconnecting {
                attempt: 1,
                next_retry_secs: 2,
            },
            last_error: Some("connection reset by peer".to_string()),
        },
    });
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::TransportStatus {
            generation: gen_id,
            status: TransportStatus::Connecting,
            last_error: Some("connection reset by peer".to_string()),
        },
    });

    let snap_reconn = state.status_bar_snapshot();
    assert!(snap_reconn.shows_reconnect_controls());
    let conn_notice = snap_reconn
        .connection_notice
        .as_ref()
        .expect("connection_notice must show auto-reconnecting");
    assert_eq!(conn_notice.kind, RecoveryNoticeKind::AutoReconnecting);
    assert!(conn_notice.message.contains("Auto-reconnecting"));
    let _ = state.update(Message::ToggleSidebar);
    let _ = state.view(win);

    // 3. Recovery -> Reconnected successfully badge & Status Bar notice.
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::Reconnected {
            generation: gen_id,
            epoch: 1,
        },
    });
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::TransportStatus {
            generation: gen_id,
            status: TransportStatus::Connected {
                latency_ms: Some(12),
            },
            last_error: None,
        },
    });

    let snap_done = state.status_bar_snapshot();
    assert_eq!(snap_done.transport_text(), "Transport: Connected (12 ms)");
    let done_notice = snap_done
        .connection_notice
        .as_ref()
        .expect("connection_notice must report Reconnected successfully");
    assert_eq!(done_notice.kind, RecoveryNoticeKind::ReconnectedSuccessfully);
    assert_eq!(done_notice.message, "Reconnected successfully");
    let _ = state.view(win);
}

#[test]
fn mitm_blocked_persists_after_dismiss_and_prompts_scope_per_pane_and_window() {
    use termewnal_core::ThemePalette;
    use termewnal_ssh::HostKeyPrompt;
    use termewnal_ui::status_bar::StatusBarAction;
    use termewnal_ui::{MitmAlert, SshAuthPrompt};
    use tokio::sync::oneshot;

    let temp = TempDir::new().unwrap();
    let mut state = super::common::boot_test_app(ConfigPaths::from_root(temp.path())).unwrap();
    let win_a = window::Id::unique();
    let win_b = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win_a));
    let _ = state.update(Message::WindowOpened(win_b));

    let profile_a = Profile::ssh(
        "Prod A",
        "10.10.0.1",
        22,
        "ops",
        SshAuthMethod::Password {
            vault_secret_ref: None,
        },
    );
    let profile_b = Profile::ssh(
        "Prod B",
        "10.10.0.2",
        22,
        "ops",
        SshAuthMethod::Password {
            vault_secret_ref: None,
        },
    );
    let _ = state.update(Message::InWindow {
        window_id: win_a,
        inner: Box::new(Message::OpenSshProfile(profile_a)),
    });
    let sid_a = state.active_session_id().expect("sid_a");
    let gen_a = state.ssh_session(sid_a).unwrap().ssh_generation();

    let _ = state.update(Message::InWindow {
        window_id: win_b,
        inner: Box::new(Message::OpenSshProfile(profile_b)),
    });
    let sid_b = state.active_session_id().expect("sid_b");
    let gen_b = state.ssh_session(sid_b).unwrap().ssh_generation();
    assert_ne!(sid_a, sid_b);

    // 1. MITM detection sets persistent mitm_blocked=true that survives DismissMitmAlert
    // and keeps all reconnect triggers blocked.
    let alert = MitmAlert {
        session_id: sid_a,
        host: "10.10.0.1".to_string(),
        port: 22,
        line: 3,
        key_type: "ssh-ed25519".to_string(),
        expected_fingerprint: "SHA256:goodAAA".to_string(),
        received_fingerprint: "SHA256:evilZZZ".to_string(),
    };
    let _ = state.update(Message::SshEvent {
        session_id: sid_a,
        event: SshUiEvent::MitmBlocked {
            generation: gen_a,
            alert,
        },
    });
    assert!(state.ssh_session(sid_a).unwrap().is_mitm_blocked());
    let _ = state.update(Message::DismissMitmAlert(sid_a));
    assert!(state.ssh_ui().mitm_alert(sid_a).is_none());
    assert!(state.ssh_session(sid_a).unwrap().is_mitm_blocked());

    let gen_before_retry = state.ssh_session(sid_a).unwrap().ssh_generation();
    let _ = state.update(Message::TerminalInput {
        session_id: sid_a,
        bytes: b"\r".to_vec(),
    });
    let _ = state.update(Message::StatusBarAction(StatusBarAction::ReconnectSession(
        sid_a,
    )));
    let _ = state.update(Message::StatusBarAction(StatusBarAction::ProbeAll));
    assert_eq!(
        state.ssh_session(sid_a).unwrap().ssh_generation(),
        gen_before_retry
    );
    assert_eq!(
        state.ssh_session(sid_a).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );

    // 2. Host-key TOFU & Auth prompts stay isolated per SessionId and render in constrained pane widths.
    let (tofu_tx, _tofu_rx) = oneshot::channel();
    let _ = state.update(Message::SshEvent {
        session_id: sid_a,
        event: SshUiEvent::HostKeyPrompt {
            generation: gen_a,
            prompt: HostKeyPrompt {
                host: "10.10.0.1".to_string(),
                port: 22,
                key_type: "ssh-ed25519".to_string(),
                fingerprint_sha256: "SHA256:hostA".to_string(),
            },
            responder: Arc::new(Mutex::new(Some(tofu_tx))),
        },
    });
    let (auth_tx, _auth_rx) = oneshot::channel();
    let _ = state.update(Message::SshEvent {
        session_id: sid_b,
        event: SshUiEvent::AuthPrompt {
            generation: gen_b,
            prompt: SshAuthPrompt::Password {
                username: "ops".to_string(),
                host: "10.10.0.2".to_string(),
            },
            responder: Arc::new(Mutex::new(Some(auth_tx))),
        },
    });

    assert!(state.ssh_ui().host_key_prompt_for_session(sid_a).is_some());
    assert!(state.ssh_ui().host_key_prompt_for_session(sid_b).is_none());
    assert!(state.ssh_ui().auth_prompt_for_session(sid_b).is_some());
    assert!(state.ssh_ui().auth_prompt_for_session(sid_a).is_none());
    assert!(state.ssh_ui().session_has_blocking_prompt(sid_a));
    assert!(state.ssh_ui().session_has_blocking_prompt(sid_b));
    assert!(!state.has_modal_open_in_window(win_a));
    assert!(!state.has_modal_open_in_window(win_b));

    let palette = ThemePalette::for_name("Tomorrow Night");
    assert!(
        state
            .ssh_ui()
            .view_tofu_modal_for_session(sid_a, 340.0, &palette)
            .is_some()
    );
    assert!(
        state
            .ssh_ui()
            .view_auth_prompt_for_session(sid_b, 340.0, &palette)
            .is_some()
    );
    drop(state.view(win_a));
    drop(state.view(win_b));
}
