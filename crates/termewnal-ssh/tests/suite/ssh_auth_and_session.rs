use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream;
use russh::keys::agent::client::AgentClient;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, EcdsaCurve, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{
    SshAuthMethod, VaultBackend, VaultConfig,
    vault::{EncryptedVault, InMemorySecretStore, SecretStore, VaultSession},
};
use termewnal_pty::PtyEvent;
use termewnal_ssh::{
    AgentConnector, BoxFuture, HostKeyDecision, HostKeyPrompt, KeyboardInteractiveChallenge,
    OsAgentConnector, SshConnectConfig, SshError, SshInteractionHandler, SshSession,
    TransportStatus, fingerprint_sha256, record_known_host_path,
};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::oneshot;
use zeroize::Zeroizing;

#[derive(Clone)]
enum ExpectedAuth {
    Password {
        username: String,
        password: String,
    },
    PublicKey {
        username: String,
        allowed_key: PublicKey,
    },
    KeyboardInteractive {
        username: String,
        otp_code: String,
        pin: String,
    },
}

struct TestServerState {
    expected_auth: ExpectedAuth,
    auth_attempted: AtomicBool,
    pty_requests: Mutex<Vec<(String, u32, u32)>>,
    window_changes: Mutex<Vec<(u32, u32)>>,
}

struct TestSshHandler {
    state: Arc<TestServerState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    shell_channel: Option<ChannelId>,
    kbi_stage: u8,
}

impl server::Handler for TestSshHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.state.auth_attempted.store(true, Ordering::SeqCst);
        match &self.state.expected_auth {
            ExpectedAuth::Password {
                username,
                password: expected,
            } if user == username && password == expected => Ok(server::Auth::Accept),
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
        self.state.auth_attempted.store(true, Ordering::SeqCst);
        match &self.state.expected_auth {
            ExpectedAuth::PublicKey {
                username,
                allowed_key,
            } if user == username && key.public_key == allowed_key => Ok(server::Auth::Accept),
            _ => Ok(server::Auth::reject()),
        }
    }

    async fn auth_keyboard_interactive(
        &mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'_>>,
    ) -> Result<server::Auth, Self::Error> {
        self.state.auth_attempted.store(true, Ordering::SeqCst);
        let ExpectedAuth::KeyboardInteractive {
            username,
            otp_code,
            pin,
        } = &self.state.expected_auth
        else {
            return Ok(server::Auth::reject());
        };
        if user != username {
            return Ok(server::Auth::reject());
        }

        match (self.kbi_stage, response) {
            (0, None) => {
                self.kbi_stage = 1;
                Ok(server::Auth::Partial {
                    name: "2FA Verification".into(),
                    instructions: "Enter PIN and authenticator token".into(),
                    prompts: vec![
                        ("PIN: ".into(), false),
                        ("OTP Code: ".into(), true),
                    ]
                    .into(),
                })
            }
            (1, Some(mut resp)) => {
                let first = resp.next().map(|b| String::from_utf8_lossy(&b).into_owned());
                let second = resp.next().map(|b| String::from_utf8_lossy(&b).into_owned());
                if first.as_deref() == Some(pin.as_str())
                    && second.as_deref() == Some(otp_code.as_str())
                {
                    Ok(server::Auth::Accept)
                } else {
                    Ok(server::Auth::reject())
                }
            }
            _ => Ok(server::Auth::reject()),
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
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.state
            .pty_requests
            .lock()
            .unwrap()
            .push((term.to_string(), col_width, row_height));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.shell_channel = Some(channel);
        session.channel_success(channel)?;
        session.data(channel, b"WELCOME_SSH\r\n".to_vec())?;
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
        self.state
            .window_changes
            .lock()
            .unwrap()
            .push((col_width, row_height));
        let msg = format!("RESIZE:{col_width}x{row_height}\r\n");
        session.data(channel, msg.into_bytes())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let mut out = Vec::new();
        for &b in data {
            if b == b'\r' {
                out.extend_from_slice(b"\r\n");
            } else {
                out.push(b);
            }
        }
        session.data(channel, out)?;
        if data.windows(4).any(|w| w == b"exit") {
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }
}

struct SpawnedTestServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<TestServerState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for SpawnedTestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_test_server(expected_auth: ExpectedAuth) -> SpawnedTestServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();

    let mut config = server::Config::default();
    config.auth_rejection_time = Duration::from_millis(1);
    config.auth_rejection_time_initial = Some(Duration::from_millis(1));
    config.inactivity_timeout = None;
    config.methods = MethodSet::from(
        &[
            MethodKind::Password,
            MethodKind::PublicKey,
            MethodKind::KeyboardInteractive,
        ][..],
    );
    config.keys.push(host_key);
    let config = Arc::new(config);

    let state = Arc::new(TestServerState {
        expected_auth,
        auth_attempted: AtomicBool::new(false),
        pty_requests: Mutex::new(Vec::new()),
        window_changes: Mutex::new(Vec::new()),
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
                    let cfg = Arc::clone(&config);
                    let st = Arc::clone(&srv_state);
                    tokio::spawn(async move {
                        let handler = TestSshHandler {
                            state: st,
                            channels: HashMap::new(),
                            shell_channel: None,
                            kbi_stage: 0,
                        };
                        let _ = server::run_stream(cfg, stream, handler).await;
                    });
                }
            }
        }
    });

    SpawnedTestServer {
        addr,
        host_pubkey,
        state,
        shutdown_tx: Some(shutdown_tx),
    }
}

struct ScriptedInteraction {
    host_decision: HostKeyDecision,
    tofu_prompts: Mutex<Vec<HostKeyPrompt>>,
    passphrase: Option<String>,
    passphrase_calls: AtomicUsize,
    password: Option<String>,
    password_calls: AtomicUsize,
    kbi_challenges: Mutex<Vec<KeyboardInteractiveChallenge>>,
    kbi_answers: Vec<String>,
}

impl ScriptedInteraction {
    fn accept_tofu() -> Arc<Self> {
        Arc::new(Self {
            host_decision: HostKeyDecision::AcceptAndSave,
            tofu_prompts: Mutex::new(Vec::new()),
            passphrase: None,
            passphrase_calls: AtomicUsize::new(0),
            password: None,
            password_calls: AtomicUsize::new(0),
            kbi_challenges: Mutex::new(Vec::new()),
            kbi_answers: Vec::new(),
        })
    }
}

impl SshInteractionHandler for ScriptedInteraction {
    fn verify_unknown_host(&self, prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
        self.tofu_prompts.lock().unwrap().push(prompt);
        let decision = self.host_decision;
        Box::pin(async move { decision })
    }

    fn prompt_key_passphrase(&self, _key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        self.passphrase_calls.fetch_add(1, Ordering::SeqCst);
        let pass = self.passphrase.clone().map(Zeroizing::new);
        Box::pin(async move { pass })
    }

    fn prompt_password(
        &self,
        _username: &str,
        _host: &str,
    ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        self.password_calls.fetch_add(1, Ordering::SeqCst);
        let pw = self.password.clone().map(Zeroizing::new);
        Box::pin(async move { pw })
    }

    fn prompt_keyboard_interactive(
        &self,
        challenge: KeyboardInteractiveChallenge,
    ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
        self.kbi_challenges.lock().unwrap().push(challenge);
        let answers = Some(
            self.kbi_answers
                .iter()
                .cloned()
                .map(Zeroizing::new)
                .collect(),
        );
        Box::pin(async move { answers })
    }
}

fn make_vault(temp: &TempDir) -> VaultSession {
    VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(temp.path().join("vault.enc")),
    )
}

fn connect_cfg(addr: SocketAddr, user: &str, auth: SshAuthMethod, kh: &Path) -> SshConnectConfig {
    SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        username: user.to_string(),
        auth,
        known_hosts_path: Some(kh.to_path_buf()),
        proxy_jump: Vec::new(),
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 10_000,
    }
}

#[tokio::test]
async fn tofu_reject_aborts_before_auth_and_tofu_accept_persists_known_hosts() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    vault.set_secret("ssh/pw", "hunter2").unwrap();

    let srv = spawn_test_server(ExpectedAuth::Password {
        username: "alice".to_string(),
        password: "hunter2".to_string(),
    })
    .await;

    let cfg = connect_cfg(
        srv.addr,
        "alice",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/pw".to_string()),
        },
        &known_hosts,
    );

    // 1. Reject TOFU -> fails with HostKeyRejected, never attempts auth, never writes known_hosts.
    let reject_handler = Arc::new(ScriptedInteraction {
        host_decision: HostKeyDecision::Reject,
        ..Arc::try_unwrap(ScriptedInteraction::accept_tofu()).unwrap_or_else(|_| unreachable!())
    });
    let err = SshSession::connect(
        cfg.clone(),
        Some(&vault),
        reject_handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .err()
    .expect("rejecting TOFU must abort connection");

    assert!(matches!(err, SshError::HostKeyRejected { .. }), "got: {err:?}");
    assert!(!srv.state.auth_attempted.load(Ordering::SeqCst));
    assert!(!known_hosts.exists());
    let prompts = reject_handler.tofu_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompts[0].fingerprint_sha256,
        fingerprint_sha256(&srv.host_pubkey)
    );

    // 2. Accept TOFU -> saves key, connects, and second connection does not re-prompt.
    let accept_handler = ScriptedInteraction::accept_tofu();
    let session1 = SshSession::connect(
        cfg.clone(),
        Some(&vault),
        accept_handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .expect("accepting TOFU connects");
    assert_eq!(accept_handler.tofu_prompts.lock().unwrap().len(), 1);
    assert!(known_hosts.exists());
    session1.disconnect();

    let second_handler = ScriptedInteraction::accept_tofu();
    let session2 = SshSession::connect(
        cfg,
        Some(&vault),
        second_handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .expect("known host connects without TOFU prompt");
    assert!(second_handler.tofu_prompts.lock().unwrap().is_empty());
    session2.disconnect();
}

#[tokio::test]
async fn host_key_mismatch_hard_blocks_mitm_before_auth() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    vault.set_secret("ssh/pw", "hunter2").unwrap();

    let srv = spawn_test_server(ExpectedAuth::Password {
        username: "alice".to_string(),
        password: "hunter2".to_string(),
    })
    .await;

    let wrong_host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .public_key()
        .clone();
    record_known_host_path("127.0.0.1", srv.addr.port(), &wrong_host_key, &known_hosts).unwrap();
    let before = std::fs::read_to_string(&known_hosts).unwrap();

    let handler = ScriptedInteraction::accept_tofu();
    let err = SshSession::connect(
        connect_cfg(
            srv.addr,
            "alice",
            SshAuthMethod::Password {
                vault_secret_ref: Some("ssh/pw".to_string()),
            },
            &known_hosts,
        ),
        Some(&vault),
        handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .err()
    .expect("key change must hard block");

    assert!(
        matches!(
            err,
            SshError::HostKeyMismatch { line: 1, ref expected_fingerprint, ref received_fingerprint, .. }
            if expected_fingerprint == &fingerprint_sha256(&wrong_host_key)
                && received_fingerprint == &fingerprint_sha256(&srv.host_pubkey)
        ),
        "unexpected error: {err:?}"
    );
    assert!(!srv.state.auth_attempted.load(Ordering::SeqCst));
    assert!(handler.tofu_prompts.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), before);
}

#[tokio::test]
async fn publickey_auth_supports_ed25519_rsa_ecdsa_and_encrypted_keys_via_vault_or_prompt() {
    let temp = TempDir::new().unwrap();
    let vault = make_vault(&temp);

    for (label, key) in [
        (
            "ed25519",
            PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap(),
        ),
        (
            "ecdsa",
            PrivateKey::random(
                &mut rand::rng(),
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP256,
                },
            )
            .unwrap(),
        ),
        (
            "rsa",
            PrivateKey::from(
                russh::keys::ssh_key::private::RsaKeypair::random(&mut rand::rng(), 2048).unwrap(),
            ),
        ),
    ] {
        let key_path = temp.path().join(format!("id_{label}"));
        std::fs::write(&key_path, key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();

        let kh = temp.path().join(format!("kh_{label}"));
        let srv = spawn_test_server(ExpectedAuth::PublicKey {
            username: "deploy".to_string(),
            allowed_key: key.public_key().clone(),
        })
        .await;
        record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &kh).unwrap();

        let session = SshSession::connect(
            connect_cfg(
                srv.addr,
                "deploy",
                SshAuthMethod::PublicKey {
                    key_path: Some(key_path),
                    vault_secret_ref: None,
                },
                &kh,
            ),
            Some(&vault),
            ScriptedInteraction::accept_tofu(),
            Arc::new(OsAgentConnector::default()),
        )
        .await
        .unwrap_or_else(|e| panic!("{label} publickey auth failed: {e}"));

        assert!(!session.used_interactive_auth());
        assert!(
            session
                .wait_for_output_async(Duration::from_secs(3), |s| {
                    s.visible_text().contains("WELCOME_SSH")
                })
                .await
        );
        session.disconnect();
    }

    // Encrypted OpenSSH key backed by VaultSession passphrase (no interactive prompt).
    let enc_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let enc_pem = enc_key
        .encrypt(&mut rand::rng(), "vault-key-pass")
        .unwrap()
        .to_openssh(LineEnding::LF)
        .unwrap();
    let enc_path = temp.path().join("id_enc_openssh");
    std::fs::write(&enc_path, enc_pem.as_bytes()).unwrap();
    vault.set_secret("keys/enc-openssh", "vault-key-pass").unwrap();

    let kh_enc = temp.path().join("kh_enc");
    let srv_enc = spawn_test_server(ExpectedAuth::PublicKey {
        username: "deploy".to_string(),
        allowed_key: enc_key.public_key().clone(),
    })
    .await;
    record_known_host_path("127.0.0.1", srv_enc.addr.port(), &srv_enc.host_pubkey, &kh_enc)
        .unwrap();

    let vault_handler = ScriptedInteraction::accept_tofu();
    let session_vault = SshSession::connect(
        connect_cfg(
            srv_enc.addr,
            "deploy",
            SshAuthMethod::PublicKey {
                key_path: Some(enc_path.clone()),
                vault_secret_ref: Some("keys/enc-openssh".to_string()),
            },
            &kh_enc,
        ),
        Some(&vault),
        vault_handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .expect("encrypted key via VaultSession should authenticate");
    assert_eq!(vault_handler.passphrase_calls.load(Ordering::SeqCst), 0);
    assert!(!session_vault.used_interactive_auth());
    session_vault.disconnect();

    // Encrypted PKCS#8 key without Vault entry -> interactive passphrase callback.
    let mut pkcs8_buf = Vec::new();
    russh::keys::encode_pkcs8_pem_encrypted(&enc_key, b"prompt-pass", 8, &mut pkcs8_buf).unwrap();
    let pkcs8_path = temp.path().join("id_enc_pkcs8");
    std::fs::write(&pkcs8_path, &pkcs8_buf).unwrap();

    let prompt_handler = Arc::new(ScriptedInteraction {
        host_decision: HostKeyDecision::AcceptAndSave,
        tofu_prompts: Mutex::new(Vec::new()),
        passphrase: Some("prompt-pass".to_string()),
        passphrase_calls: AtomicUsize::new(0),
        password: None,
        password_calls: AtomicUsize::new(0),
        kbi_challenges: Mutex::new(Vec::new()),
        kbi_answers: Vec::new(),
    });
    let session_prompt = SshSession::connect(
        connect_cfg(
            srv_enc.addr,
            "deploy",
            SshAuthMethod::PublicKey {
                key_path: Some(pkcs8_path),
                vault_secret_ref: None,
            },
            &kh_enc,
        ),
        Some(&vault),
        prompt_handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .expect("encrypted PKCS#8 key via interactive passphrase should authenticate");
    assert_eq!(prompt_handler.passphrase_calls.load(Ordering::SeqCst), 1);
    assert!(session_prompt.used_interactive_auth());
    session_prompt.disconnect();
}

#[cfg(unix)]
#[tokio::test]
async fn ssh_agent_auth_connects_over_unix_agent_socket_and_authenticates() {
    let temp = TempDir::new().unwrap();
    let agent_sock = temp.path().join("agent.sock");
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
    let stream = tokio::net::UnixStream::connect(&agent_sock).await.unwrap();
    let mut setup_client = AgentClient::connect(stream);
    setup_client.add_identity(&agent_key, &[]).await.unwrap();

    let kh = temp.path().join("known_hosts");
    let srv = spawn_test_server(ExpectedAuth::PublicKey {
        username: "agentuser".to_string(),
        allowed_key: agent_key.public_key().clone(),
    })
    .await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &kh).unwrap();

    let agent_connector: Arc<dyn AgentConnector> =
        Arc::new(OsAgentConnector::from_unix_socket(&agent_sock));
    let session = SshSession::connect(
        connect_cfg(srv.addr, "agentuser", SshAuthMethod::Agent, &kh),
        None,
        ScriptedInteraction::accept_tofu(),
        agent_connector,
    )
    .await
    .expect("SSH Agent auth should succeed");

    assert!(!session.used_interactive_auth());
    assert!(
        session
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("WELCOME_SSH")
            })
            .await
    );
    session.disconnect();
}

#[tokio::test]
async fn keyboard_interactive_2fa_pty_io_and_resize_stream_into_terminal_buffer() {
    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    let srv = spawn_test_server(ExpectedAuth::KeyboardInteractive {
        username: "secops".to_string(),
        pin: "4242".to_string(),
        otp_code: "987654".to_string(),
    })
    .await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &kh).unwrap();

    let handler = Arc::new(ScriptedInteraction {
        host_decision: HostKeyDecision::AcceptAndSave,
        tofu_prompts: Mutex::new(Vec::new()),
        passphrase: None,
        passphrase_calls: AtomicUsize::new(0),
        password: None,
        password_calls: AtomicUsize::new(0),
        kbi_challenges: Mutex::new(Vec::new()),
        kbi_answers: vec!["4242".to_string(), "987654".to_string()],
    });

    let session = SshSession::connect(
        connect_cfg(srv.addr, "secops", SshAuthMethod::KeyboardInteractive, &kh),
        None,
        handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .expect("keyboard-interactive 2FA auth should succeed");

    assert!(session.used_interactive_auth());
    assert!(matches!(
        session.transport_status(),
        TransportStatus::Connected { .. }
    ));

    let challenges = handler.kbi_challenges.lock().unwrap();
    assert_eq!(challenges.len(), 1);
    assert_eq!(challenges[0].name, "2FA Verification");
    assert_eq!(challenges[0].prompts.len(), 2);
    assert!(!challenges[0].prompts[0].echo);
    assert!(challenges[0].prompts[1].echo);

    let mut rx = session.take_event_receiver().expect("ssh event receiver");

    // Verify initial PTY output rendered in TerminalBuffer.
    assert!(
        session
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("WELCOME_SSH")
            })
            .await
    );
    assert_eq!(
        srv.state.pty_requests.lock().unwrap().as_slice(),
        &[("xterm-256color".to_string(), 80, 24)]
    );

    // Send interactive shell input & verify echoed output.
    session.write_bytes(b"hello_ssh\r").unwrap();
    assert!(
        session
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("hello_ssh")
            })
            .await
    );

    // Resize terminal & verify window_change propagated to server + buffer dimensions updated.
    session.resize(120, 42).unwrap();
    assert_eq!(session.dimensions(), (120, 42));
    assert!(
        session
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("RESIZE:120x42")
            })
            .await
    );
    assert_eq!(
        srv.state.window_changes.lock().unwrap().as_slice(),
        &[(120, 42)]
    );

    // Clean exit command delivers PtyEvent::Exited(Some(0)).
    session.write_bytes(b"exit\r").unwrap();
    let exited = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = rx.recv().await {
            if let PtyEvent::Exited(code) = ev {
                return Some(code);
            }
        }
        None
    })
    .await
    .expect("timeout waiting for SSH PtyEvent::Exited");
    assert_eq!(exited, Some(Some(0)));
}

#[tokio::test]
async fn password_auth_falls_back_to_interactive_prompt_when_absent_from_vault_and_rejects_bad_password() {
    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    let srv = spawn_test_server(ExpectedAuth::Password {
        username: "bob".to_string(),
        password: "correct-password".to_string(),
    })
    .await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &kh).unwrap();

    // Missing Vault ref -> prompts interactively and succeeds.
    let ok_handler = Arc::new(ScriptedInteraction {
        host_decision: HostKeyDecision::AcceptAndSave,
        tofu_prompts: Mutex::new(Vec::new()),
        passphrase: None,
        passphrase_calls: AtomicUsize::new(0),
        password: Some("correct-password".to_string()),
        password_calls: AtomicUsize::new(0),
        kbi_challenges: Mutex::new(Vec::new()),
        kbi_answers: Vec::new(),
    });
    let session = SshSession::connect(
        connect_cfg(
            srv.addr,
            "bob",
            SshAuthMethod::Password {
                vault_secret_ref: Some("ssh/missing".to_string()),
            },
            &kh,
        ),
        Some(&vault),
        ok_handler.clone(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .expect("interactive password fallback should authenticate");
    assert_eq!(ok_handler.password_calls.load(Ordering::SeqCst), 1);
    assert!(session.used_interactive_auth());
    session.disconnect();

    // Wrong password in Vault -> AuthenticationFailed.
    vault.set_secret("ssh/bad", "wrong-password").unwrap();
    let err = SshSession::connect(
        connect_cfg(
            srv.addr,
            "bob",
            SshAuthMethod::Password {
                vault_secret_ref: Some("ssh/bad".to_string()),
            },
            &kh,
        ),
        Some(&vault),
        ScriptedInteraction::accept_tofu(),
        Arc::new(OsAgentConnector::default()),
    )
    .await
    .err()
    .expect("wrong password must fail");
    assert!(matches!(
        err,
        SshError::AuthenticationFailed {
            method: "password",
            ..
        }
    ));
}
