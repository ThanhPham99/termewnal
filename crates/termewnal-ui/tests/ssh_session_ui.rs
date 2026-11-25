use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::window;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{
    ConfigPaths, Profile, SshAuthMethod, VaultBackend, VaultConfig,
    vault::{EncryptedVault, InMemorySecretStore, SecretStore, VaultSession},
};
use termewnal_pty::PtyEvent;
use termewnal_ssh::{
    HostKeyDecision, HostKeyPrompt, InteractivePromptItem, KeyboardInteractiveChallenge,
    SshConnectConfig, TransportStatus, fingerprint_sha256, record_known_host_path,
};
use termewnal_ui::{
    AppState, Message, MitmAlert, SshAuthPrompt, SshTargetSpec, SshUiEvent, drive_ssh_connect,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

#[test]
fn restored_ssh_sessions_respect_auto_connect_on_startup_and_trigger_on_enter_or_click() {
    let temp = TempDir::new().unwrap();
    let mut state = AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).unwrap();
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
    assert_eq!(
        state.session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    assert!(state.session(manual_sid).unwrap().is_ssh());
    drop(state.view(win));

    // Typing ordinary character while Disconnected does not start connecting.
    let _ = state.update(Message::TerminalInput {
        session_id: manual_sid,
        bytes: b"a".to_vec(),
    });
    assert_eq!(
        state.session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );

    // Pressing Enter initiates Connecting.
    let _ = state.update(Message::TerminalInput {
        session_id: manual_sid,
        bytes: b"\r".to_vec(),
    });
    assert_eq!(
        state.session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );

    // 2. Second manual session triggered via explicit click (ConnectSshSession).
    let _ = state.update(Message::RestoreSshProfile(profile_manual));
    let click_sid = state.active_session_id().unwrap();
    assert_eq!(
        state.session(click_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    let _ = state.update(Message::ConnectSshSession(click_sid));
    assert_eq!(
        state.session(click_sid).unwrap().transport_status(),
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
        state.session(auto_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );
}

#[test]
fn tofu_prompt_accept_and_reject_resolve_host_key_decisions_and_clear_modal() {
    let temp = TempDir::new().unwrap();
    let mut state = AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).unwrap();
    let win = window::Id::unique();

    let profile = Profile::ssh("TOFU Host", "db.internal", 22, "admin", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.session(sid).unwrap().ssh_generation().unwrap();

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
    let mut state = AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).unwrap();
    let win = window::Id::unique();

    let profile = Profile::ssh("Prod", "prod.example.com", 22, "root", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.session(sid).unwrap().ssh_generation().unwrap();

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
        state.session(sid).unwrap().transport_status(),
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
    let mut state = AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).unwrap();
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
    let gen_id = state.session(sid).unwrap().ssh_generation().unwrap();

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

    let mut state = AppState::boot_with_paths_and_vault(paths, Arc::clone(&vault)).unwrap();
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
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
        },
    )
    .with_known_hosts_path(&known_hosts);

    let (sid, _task) = state.open_ssh_session(spec.clone(), true);
    let gen_id = state.session(sid).unwrap().ssh_generation().unwrap();
    let buffer = Arc::clone(state.session(sid).unwrap().buffer());

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
        state.session(sid).unwrap().transport_status(),
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
            .session(sid)
            .unwrap()
            .snapshot()
            .visible_text()
            .contains("UI_RESIZE:100x35")
    );
    let _ = state.update(Message::DisconnectSshSession(sid));
    assert_eq!(
        state.session(sid).unwrap().transport_status(),
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
    let mitm_gen = state.session(mitm_sid).unwrap().ssh_generation().unwrap();
    let mitm_buf = Arc::clone(state.session(mitm_sid).unwrap().buffer());
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
        state.session(mitm_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
}
