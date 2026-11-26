use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use termewnal_core::{
    ConfigPaths, ConfigSyncService, DockerRuntime, EncryptedVault, InMemorySecretStore,
    ProfileTarget, QuickCommand, SecretStore as _, SerialInputMode, SerialOutputMode,
    SyncProviderKind, VaultBackend, VaultConfig, VaultSession, load_quick_commands,
    load_workspaces,
};
use termewnal_protocols::{
    CommandOutputRunner, ContainerInfo, ProtocolError, ProtocolStatus, SerialByteStream,
    SerialConnectConfig, SerialPortInfo, SerialPortOpener, ZmodemEngine,
};
use termewnal_pty::PtyEvent;
use termewnal_ui::{
    Message, ProfileAuthChoice, ProfileKindChoice, ProtocolLauncherTab, SessionKindLabel,
    SplitAxis,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::common::{boot_test_app, boot_test_app_with_vault};

struct LoopbackSerialStream {
    rx_from_peer: Arc<Mutex<VecDeque<u8>>>,
    tx_to_peer: Arc<Mutex<VecDeque<u8>>>,
}

impl LoopbackSerialStream {
    fn new_pair() -> (Arc<Self>, Arc<Mutex<VecDeque<u8>>>, Arc<Mutex<VecDeque<u8>>>) {
        let rx_from_peer = Arc::new(Mutex::new(VecDeque::new()));
        let tx_to_peer = Arc::new(Mutex::new(VecDeque::new()));
        let stream = Arc::new(Self {
            rx_from_peer: Arc::clone(&rx_from_peer),
            tx_to_peer: Arc::clone(&tx_to_peer),
        });
        (stream, rx_from_peer, tx_to_peer)
    }
}

impl SerialByteStream for LoopbackSerialStream {
    fn read_chunk(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut guard = self.rx_from_peer.lock().unwrap();
        if guard.is_empty() {
            drop(guard);
            std::thread::sleep(Duration::from_millis(5));
            return Err(io::Error::new(ErrorKind::WouldBlock, "empty loopback"));
        }
        let mut n = 0;
        while n < buf.len() {
            if let Some(b) = guard.pop_front() {
                buf[n] = b;
                n += 1;
            } else {
                break;
            }
        }
        Ok(n)
    }

    fn write_all_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.tx_to_peer.lock().unwrap().extend(bytes.iter().copied());
        Ok(())
    }

    fn flush_bytes(&self) -> io::Result<()> {
        Ok(())
    }
}

struct MockSerialOpener {
    stream: Arc<LoopbackSerialStream>,
}

impl SerialPortOpener for MockSerialOpener {
    fn list_ports(&self) -> Result<Vec<SerialPortInfo>, ProtocolError> {
        Ok(vec![SerialPortInfo {
            path: "/dev/ttyUSB9".to_string(),
            display_name: "USB UART Bridge".to_string(),
        }])
    }

    fn open(
        &self,
        _config: &SerialConnectConfig,
    ) -> Result<Arc<dyn SerialByteStream>, ProtocolError> {
        Ok(Arc::clone(&self.stream) as Arc<dyn SerialByteStream>)
    }
}

struct MockDockerRunner;

impl CommandOutputRunner for MockDockerRunner {
    fn run_stdout(&self, program: &str, _args: &[&str]) -> Result<String, ProtocolError> {
        match program {
            "docker" => Ok("c11111111111\tapi-prod\tnginx:1.25\tUp 2 hours\n".to_string()),
            "podman" => Ok("p22222222222\tworker-dev\tubuntu:24.04\tUp 5 mins\n".to_string()),
            other => Err(ProtocolError::Docker(format!("unknown runtime {other}"))),
        }
    }
}

#[test]
fn quick_commands_crud_dispatch_broadcast_and_confirm_flow() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());
    let mut state = boot_test_app(paths.clone()).unwrap();

    // Open Quick Commands modal and save a snippet.
    let _ = state.update(Message::OpenQuickCommandsModal);
    assert!(state.quick_commands_ui().open);
    assert!(state.has_modal_open());
    let _ = state.update(Message::QuickCommandNameChanged("Say Focused".to_string()));
    let _ = state.update(Message::QuickCommandGroupChanged("Ops".to_string()));
    let _ = state.update(Message::QuickCommandBodyChanged(
        "echo QC_FOCUSED_OK".to_string(),
    ));
    let _ = state.update(Message::QuickCommandSave);

    assert_eq!(state.quick_commands().len(), 1);
    let saved_id = state.quick_commands()[0].id.clone();
    let on_disk = load_quick_commands(&paths).unwrap();
    assert_eq!(on_disk.commands.len(), 1);
    assert_eq!(on_disk.commands[0].group.as_deref(), Some("Ops"));

    // Quick command shows up in Command Palette items.
    assert!(state
        .command_palette_items()
        .iter()
        .any(|i| i.title == "Run Quick Command: Say Focused"));

    // Close modal & split active tab into 2 local shell panes.
    let _ = state.update(Message::EscapePressed);
    assert!(!state.quick_commands_ui().open);

    let first_sid = state.active_session_id().unwrap();
    let _ = state.update(Message::SplitFocusedPane(SplitAxis::Horizontal));
    let panes = state.workspace().active_tab().unwrap().root.session_ids();
    assert_eq!(panes.len(), 2);
    let second_sid = panes.into_iter().find(|&id| id != first_sid).unwrap();
    assert_eq!(state.active_session_id(), Some(second_sid));

    // Wait briefly so both shells settle past initial prompt, then run non-broadcast quick command.
    let buf1 = Arc::clone(state.session(first_sid).unwrap().buffer());
    let buf2 = Arc::clone(state.session(second_sid).unwrap().buffer());
    let _ = state.update(Message::ExecuteQuickCommand {
        id: saved_id.clone(),
        broadcast_override: None,
    });
    assert!(buf2.wait_for_output(Duration::from_secs(3), |s| s
        .visible_text()
        .contains("QC_FOCUSED_OK")));
    assert!(!buf1.snapshot().visible_text().contains("QC_FOCUSED_OK"));

    // Edit snippet with broadcast=true & confirm_before_run=true, then run.
    let _ = state.update(Message::QuickCommandEdit(saved_id.clone()));
    let _ = state.update(Message::QuickCommandBodyChanged(
        "echo QC_BCAST_OK".to_string(),
    ));
    let _ = state.update(Message::QuickCommandBroadcastToggled(true));
    let _ = state.update(Message::QuickCommandConfirmToggled(true));
    let _ = state.update(Message::QuickCommandSave);

    let _ = state.update(Message::ExecuteQuickCommand {
        id: saved_id.clone(),
        broadcast_override: None,
    });
    assert_eq!(
        state.quick_commands_ui().confirm_pending_id.as_deref(),
        Some(saved_id.as_str())
    );
    assert!(!buf1.snapshot().visible_text().contains("QC_BCAST_OK"));

    let _ = state.update(Message::ConfirmExecuteQuickCommand(saved_id.clone()));
    assert!(state.quick_commands_ui().confirm_pending_id.is_none());
    assert!(buf1.wait_for_output(Duration::from_secs(3), |s| s
        .visible_text()
        .contains("QC_BCAST_OK")));
    assert!(buf2.wait_for_output(Duration::from_secs(3), |s| s
        .visible_text()
        .contains("QC_BCAST_OK")));

    // Delete snippet and verify persisted removal.
    let _ = state.update(Message::QuickCommandDelete(saved_id));
    assert!(state.quick_commands().is_empty());
    assert!(load_quick_commands(&paths).unwrap().commands.is_empty());
}

#[test]
fn named_workspaces_save_restore_multi_tab_splits_and_delete() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());
    let mut state = boot_test_app(paths.clone()).unwrap();

    // Configure tab 1 (split + colored + renamed) and tab 2.
    let t1 = state.active_tab_id().unwrap();
    let _ = state.update(Message::BeginTabRename(t1));
    let _ = state.update(Message::TabRenameDraftChanged("Backend API".to_string()));
    let _ = state.update(Message::SubmitTabRename);
    let _ = state.update(Message::SetTabColor {
        tab_id: t1,
        color: Some("#88C0D0".to_string()),
    });
    let _ = state.update(Message::SplitFocusedPane(SplitAxis::Vertical));
    let _ = state.update(Message::NewTab);
    let tabs_before = state.workspace().tab_count();
    assert_eq!(tabs_before, 2);

    // Save Named Workspace template.
    let _ = state.update(Message::OpenWorkspacesModal);
    assert!(state.workspaces_ui().open);
    let _ = state.update(Message::WorkspaceNameChanged("Prod Stack".to_string()));
    let _ = state.update(Message::WorkspaceDescChanged("API + Logs".to_string()));
    let _ = state.update(Message::SaveCurrentWorkspaceTemplate);

    assert_eq!(state.workspace_templates().len(), 1);
    let tpl_id = state.workspace_templates()[0].id.clone();
    assert_eq!(load_workspaces(&paths).unwrap().workspaces.len(), 1);

    // Launch saved workspace in a fresh AppState sharing the same config dir.
    let mut state2 = boot_test_app(paths.clone()).unwrap();
    assert_eq!(state2.workspace_templates().len(), 1);
    let initial_tabs = state2.workspace().tab_count();
    let _ = state2.update(Message::OpenWorkspacesModal);
    let _ = state2.update(Message::LaunchWorkspaceTemplate(tpl_id.clone()));
    assert!(!state2.workspaces_ui().open);
    assert_eq!(state2.workspace().tab_count(), initial_tabs + 2);

    let restored_tab = &state2.workspace().tabs()[initial_tabs];
    assert_eq!(restored_tab.title.as_deref(), Some("Backend API"));
    assert_eq!(restored_tab.color.as_deref(), Some("#88C0D0"));
    assert_eq!(restored_tab.root.pane_count(), 2);

    // Delete workspace template.
    let _ = state2.update(Message::DeleteWorkspaceTemplate(tpl_id));
    assert!(state2.workspace_templates().is_empty());
    assert!(load_workspaces(&paths).unwrap().workspaces.is_empty());
}

#[tokio::test]
async fn serial_discovery_live_modes_hex_send_and_layout_persistence() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());
    let (stream, rx_from_peer, tx_to_peer) = LoopbackSerialStream::new_pair();
    let opener = Arc::new(MockSerialOpener { stream });

    let mut state = boot_test_app(paths.clone())
        .unwrap()
        .with_serial_opener(opener)
        .with_serial_port_lister(|| {
            Ok(vec![SerialPortInfo {
                path: "/dev/ttyUSB9".to_string(),
                display_name: "USB UART Bridge".to_string(),
            }])
        });

    let _ = state.update(Message::OpenProtocolLauncherModal(ProtocolLauncherTab::Serial));
    assert!(state.protocol_launcher().open);
    assert_eq!(state.protocol_launcher().serial_ports.len(), 1);
    assert_eq!(state.protocol_launcher().serial_port_input, "/dev/ttyUSB9");

    let _ = state.update(Message::LaunchQuickSerial);
    assert!(!state.protocol_launcher().open);

    let sid = state.active_session_id().unwrap();
    let serial_slot = state.serial_session(sid).expect("serial slot created");
    assert!(serial_slot.serial_session().is_some());
    assert_eq!(
        state.status_bar_snapshot().active_session.as_ref().unwrap().kind,
        SessionKindLabel::Serial
    );

    // Remote device writes RX text into terminal.
    rx_from_peer
        .lock()
        .unwrap()
        .extend(b"BOOT READY\r\n".iter().copied());
    assert!(serial_slot
        .buffer()
        .wait_for_output_async(Duration::from_secs(2), |s| s
            .visible_text()
            .contains("BOOT READY"))
        .await);

    // Toggle Hexdump output mode & verify hex output renders cleanly.
    let _ = state.update(Message::SerialSetOutputMode {
        session_id: sid,
        mode: SerialOutputMode::Hexdump,
    });
    rx_from_peer.lock().unwrap().extend([0xAB, 0xCD]);
    assert!(state
        .serial_session(sid)
        .unwrap()
        .buffer()
        .wait_for_output_async(Duration::from_secs(2), |s| s
            .visible_text()
            .contains("ab cd"))
        .await);

    // Toggle Readline mode and send hex frame explicitly.
    let _ = state.update(Message::SerialSetInputMode {
        session_id: sid,
        mode: SerialInputMode::Readline,
    });
    let _ = state.update(Message::SerialHexDraftChanged {
        session_id: sid,
        value: "AA 55 0D".to_string(),
    });
    let _ = state.update(Message::SerialSendHex(sid));
    let mut got_hex = false;
    for _ in 0..40 {
        {
            let tx = tx_to_peer.lock().unwrap();
            if tx.iter().copied().collect::<Vec<u8>>() == vec![0xAA, 0x55, 0x0D] {
                got_hex = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(got_hex, "expected raw hex bytes on serial wire");

    // Layout persistence saves SessionDescriptor::Serial & restores it cleanly.
    state.save_workspace_layout().unwrap();
    let restored = boot_test_app(paths).unwrap();
    let restored_sid = restored.active_session_id().unwrap();
    let r_slot = restored.serial_session(restored_sid).expect("restored serial tab");
    assert_eq!(r_slot.config().port, "/dev/ttyUSB9");
    assert_eq!(r_slot.config().baud_rate, 115_200);
    assert_eq!(r_slot.config().output_mode, SerialOutputMode::Hexdump);

    // Clean shutdown.
    let _ = state.update(Message::SerialDisconnect(sid));
    let mut disconnected = false;
    for _ in 0..40 {
        if state.serial_session(sid).unwrap().status() == ProtocolStatus::Disconnected {
            disconnected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(disconnected);
}

#[tokio::test]
async fn telnet_loopback_and_docker_discovery_and_zmodem_end_to_end() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());

    // Start local Telnet loopback server.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = socket.write_all(b"Welcome Telnet\r\n").await;
        let mut buf = [0u8; 256];
        loop {
            let n = socket.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            let _ = socket.write_all(&buf[..n]).await;
            if buf[..n].windows(b"ping_telnet".len()).any(|w| w == b"ping_telnet") {
                break;
            }
        }
    });

    let download_dir = dir.path().join("z_downloads");
    std::fs::create_dir_all(&download_dir).unwrap();

    let (stream, rx_from_peer, tx_to_peer) = LoopbackSerialStream::new_pair();
    let mut state = boot_test_app(paths.clone())
        .unwrap()
        .with_serial_opener(Arc::new(MockSerialOpener { stream }))
        .with_docker_runner_factory(|| Box::new(MockDockerRunner));
    state.set_sftp_download_dir(download_dir.clone());

    // Verify Docker/Podman discovery in ProtocolLauncher modal.
    let _ = state.update(Message::OpenProtocolLauncherModal(
        ProtocolLauncherTab::Containers,
    ));
    assert_eq!(
        state.protocol_launcher().containers,
        vec![
            ContainerInfo {
                runtime: DockerRuntime::Docker,
                id: "c11111111111".to_string(),
                name: "api-prod".to_string(),
                image: "nginx:1.25".to_string(),
                status: "Up 2 hours".to_string(),
            },
            ContainerInfo {
                runtime: DockerRuntime::Podman,
                id: "p22222222222".to_string(),
                name: "worker-dev".to_string(),
                image: "ubuntu:24.04".to_string(),
                status: "Up 5 mins".to_string(),
            },
        ]
    );

    // Launch Telnet session from ProtocolLauncher.
    let _ = state.update(Message::ProtocolLauncherSelectTab(ProtocolLauncherTab::Telnet));
    let _ = state.update(Message::ProtocolTelnetHostChanged("127.0.0.1".to_string()));
    let _ = state.update(Message::ProtocolTelnetPortChanged(port.to_string()));
    let _ = state.update(Message::LaunchQuickTelnet);

    let telnet_sid = state.active_session_id().unwrap();
    let telnet_buf = Arc::clone(
        state
            .telnet_session(telnet_sid)
            .expect("telnet slot created")
            .buffer(),
    );
    assert!(telnet_buf
        .wait_for_output_async(Duration::from_secs(2), |s| s
            .visible_text()
            .contains("Welcome Telnet"))
        .await);
    assert_eq!(
        state.status_bar_snapshot().active_session.as_ref().unwrap().kind,
        SessionKindLabel::Telnet
    );
    let _ = state.update(Message::TerminalInput {
        session_id: telnet_sid,
        bytes: b"ping_telnet\r\n".to_vec(),
    });
    assert!(telnet_buf
        .wait_for_output_async(Duration::from_secs(2), |s| s
            .visible_text()
            .contains("ping_telnet"))
        .await);
    let _ = server_task.await;

    // Live Zmodem `sz` auto-receive over loopback Serial session.
    let _ = state.update(Message::OpenProtocolLauncherModal(ProtocolLauncherTab::Serial));
    let _ = state.update(Message::ProtocolSerialPortChanged("/dev/ttyZ0".to_string()));
    let _ = state.update(Message::LaunchQuickSerial);
    let serial_sid = state.active_session_id().unwrap();

    let src_dir = dir.path().join("z_src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let payload_file = src_dir.join("firmware.bin");
    std::fs::write(&payload_file, b"FIRMWARE_V2_PAYLOAD").unwrap();

    let mut peer = ZmodemEngine::new(src_dir.clone());
    let init = peer.start_send(vec![payload_file]).unwrap();
    rx_from_peer
        .lock()
        .unwrap()
        .extend(init.wire_output.iter().copied());

    // Pump peer Zmodem engine against UI session's Serial wire queues until received on disk.
    let expected_dl = download_dir.join("firmware.bin");
    let mut received_ok = false;
    for _ in 0..100 {
        let outgoing: Vec<u8> = {
            let mut tx = tx_to_peer.lock().unwrap();
            tx.drain(..).collect()
        };
        if !outgoing.is_empty() {
            let step = peer.feed_wire(&outgoing).unwrap();
            if !step.wire_output.is_empty() {
                rx_from_peer
                    .lock()
                    .unwrap()
                    .extend(step.wire_output.iter().copied());
            }
        }
        // Deliver PtyEvent::Output to Elm state as Subscription would.
        let _ = state.update(Message::PtyEvent {
            session_id: serial_sid,
            event: PtyEvent::Output,
        });
        if expected_dl.exists() && std::fs::read(&expected_dl).unwrap_or_default() == b"FIRMWARE_V2_PAYLOAD" {
            received_ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(received_ok, "expected Zmodem auto-received file in download_dir");
    assert!(state
        .status_bar_snapshot()
        .zmodem_notice
        .as_deref()
        .is_some_and(|n| n.contains("firmware.bin")));

    // Zmodem Send picker UI (`TriggerZmodemSend` -> `SubmitZmodemSend`) initiates `start_send`
    // and transfers `upload.txt` end-to-end to the peer acting as `rz`.
    let peer_rx_dir = dir.path().join("z_peer_rx");
    std::fs::create_dir_all(&peer_rx_dir).unwrap();
    let mut rx_peer = ZmodemEngine::new(peer_rx_dir.clone());
    let _ = state.update(Message::TriggerZmodemSend(serial_sid));
    assert!(state.zmodem_send_ui().session_id == Some(serial_sid));
    let send_file = src_dir.join("upload.txt");
    std::fs::write(&send_file, b"HELLO_RZ").unwrap();
    let _ = state.update(Message::ZmodemPathInputChanged(
        send_file.to_string_lossy().into_owned(),
    ));
    let _ = state.update(Message::SubmitZmodemSend);
    assert!(state.zmodem_send_ui().session_id.is_none());

    let expected_up = peer_rx_dir.join("upload.txt");
    let mut sent_ok = false;
    for _ in 0..100 {
        let outgoing: Vec<u8> = {
            let mut tx = tx_to_peer.lock().unwrap();
            tx.drain(..).collect()
        };
        if !outgoing.is_empty() {
            let step = rx_peer.feed_wire(&outgoing).unwrap();
            if !step.wire_output.is_empty() {
                rx_from_peer
                    .lock()
                    .unwrap()
                    .extend(step.wire_output.iter().copied());
            }
        }
        let _ = state.update(Message::PtyEvent {
            session_id: serial_sid,
            event: PtyEvent::Output,
        });
        if expected_up.exists() && std::fs::read(&expected_up).unwrap_or_default() == b"HELLO_RZ" {
            sent_ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(sent_ok, "expected Zmodem send upload.txt to arrive at peer rz");
    assert!(state
        .zmodem_notice()
        .is_some_and(|n| n.contains("upload.txt")));
}

#[tokio::test]
async fn profile_editor_v2_and_settings_sync_ui_roundtrip() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());
    let sync_dir = dir.path().join("sync_remote");
    std::fs::create_dir_all(&sync_dir).unwrap();

    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    let mut state = boot_test_app_with_vault(paths.clone(), Arc::clone(&vault))
        .unwrap()
        .with_sync_service(ConfigSyncService::new().with_kdf_params(16 * 1024, 1, 1));

    // 1. Create advanced SSH profile with ProxyCommand, ForwardAgent, ForwardX11, script, keepalive & tunnel.
    let _ = state.update(Message::OpenNewProfileModal);
    let _ = state.update(Message::ProfileFormNameChanged("Edge Bastion".to_string()));
    let _ = state.update(Message::ProfileFormHostChanged("edge.internal".to_string()));
    let _ = state.update(Message::ProfileFormUsernameChanged("ops".to_string()));
    let _ = state.update(Message::ProfileFormAuthChanged(ProfileAuthChoice::Password));
    let _ = state.update(Message::ProfileFormSecretChanged("s3cret_pw".to_string()));
    let _ = state.update(Message::ProfileFormProxyCommandChanged(
        "nc -X connect -x 127.0.0.1:8080 %h %p".to_string(),
    ));
    let _ = state.update(Message::ProfileFormForwardAgentToggled(true));
    let _ = state.update(Message::ProfileFormForwardX11Toggled(true));
    let _ = state.update(Message::ProfileFormKeepaliveIntervalChanged("45".to_string()));
    let _ = state.update(Message::ProfileFormKeepaliveMissedChanged("4".to_string()));
    let _ = state.update(Message::ProfileFormScriptExpectChanged("login:".to_string()));
    let _ = state.update(Message::ProfileFormScriptSendChanged("ops".to_string()));
    let _ = state.update(Message::ProfileFormAddScriptStep);
    let _ = state.update(Message::ProfileFormTunnelNameChanged("Metrics".to_string()));
    let _ = state.update(Message::ProfileFormTunnelBindPortChanged("9100".to_string()));
    let _ = state.update(Message::ProfileFormTunnelTargetHostChanged("127.0.0.1".to_string()));
    let _ = state.update(Message::ProfileFormTunnelTargetPortChanged("9100".to_string()));
    let _ = state.update(Message::ProfileFormAddTunnel);
    let _ = state.update(Message::ProfileSave);
    assert!(!state.profile_editor().is_open());

    let ssh_prof = state
        .profiles()
        .profiles
        .iter()
        .find(|p| p.name == "Edge Bastion")
        .unwrap()
        .clone();
    match &ssh_prof.target {
        ProfileTarget::Ssh {
            proxy_command,
            forward_agent,
            forward_x11,
            keepalive_interval_secs,
            keepalive_max_missed,
            login_scripts,
            ..
        } => {
            assert_eq!(
                proxy_command.as_deref(),
                Some("nc -X connect -x 127.0.0.1:8080 %h %p")
            );
            assert!(*forward_agent);
            assert!(*forward_x11);
            assert_eq!(*keepalive_interval_secs, Some(45));
            assert_eq!(*keepalive_max_missed, Some(4));
            assert_eq!(login_scripts.len(), 1);
            assert_eq!(login_scripts[0].expect, "login:");
        }
        _ => panic!("expected SSH target"),
    }
    assert_eq!(ssh_prof.tunnels.len(), 1);
    let profiles_raw = std::fs::read_to_string(paths.profiles_file()).unwrap();
    assert!(!profiles_raw.contains("s3cret_pw"));

    // 2. Create a Docker profile through ProfileEditor.
    let _ = state.update(Message::OpenNewProfileModal);
    let _ = state.update(Message::ProfileFormKindChanged(ProfileKindChoice::Docker));
    let _ = state.update(Message::ProfileFormNameChanged("Web Container".to_string()));
    let _ = state.update(Message::ProfileFormDockerRuntimeChanged(DockerRuntime::Podman));
    let _ = state.update(Message::ProfileFormDockerContainerChanged("web_app".to_string()));
    let _ = state.update(Message::ProfileFormDockerShellChanged("/bin/bash".to_string()));
    let _ = state.update(Message::ProfileSave);
    assert!(state.profiles().profiles.iter().any(|p| matches!(
        &p.target,
        ProfileTarget::Docker {
            runtime: DockerRuntime::Podman,
            container,
            ..
        } if container == "web_app"
    )));

    // 3. Configure QuickCommand & Settings Sync (encrypted LocalDirectory) and Push -> Wipe -> Pull.
    let mut qc_cfg = load_quick_commands(&paths).unwrap();
    qc_cfg
        .commands
        .push(QuickCommand::new("Synced Cmd", "uname -a"));
    termewnal_core::save_quick_commands(&paths, &qc_cfg).unwrap();

    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsSyncProviderSelected(
        SyncProviderKind::LocalDirectory,
    ));
    let _ = state.update(Message::SettingsSyncLocalDirChanged(
        sync_dir.to_string_lossy().into_owned(),
    ));
    let _ = state.update(Message::SettingsSyncTokenChanged("ghp_dummy_sync_secret".to_string()));
    let _ = state.update(Message::SettingsSyncEncryptToggled(true));
    let _ = state.update(Message::SettingsSyncPassphraseChanged(
        "sync-master-pass".to_string(),
    ));
    let _ = state.update(Message::SettingsSyncAutoToggled(true));

    let push_msg = state.sync_push_now_async().await.unwrap();
    assert!(push_msg.contains("Pushed"));
    assert_eq!(
        vault
            .get_secret("termewnal/sync/token")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("ghp_dummy_sync_secret")
    );
    let cfg_raw = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(!cfg_raw.contains("ghp_dummy_sync_secret"));
    assert!(!cfg_raw.contains("sync-master-pass"));

    // Wipe profiles & quick_commands on disk, then pull via sync_pull_now_async.
    termewnal_core::save_profiles(&paths, &Default::default()).unwrap();
    termewnal_core::save_quick_commands(&paths, &Default::default()).unwrap();
    let pull_msg = state.sync_pull_now_async().await.unwrap();
    assert!(pull_msg.contains("Pulled"));
    assert!(state.profiles().profiles.iter().any(|p| p.name == "Edge Bastion"));
    assert_eq!(state.quick_commands().len(), 1);
    assert_eq!(state.quick_commands()[0].name, "Synced Cmd");
    assert!(state.config().sync.last_sync_at.is_some());
    assert!(state.settings_ui().sync_status_notice().is_some());
}


#[test]
fn serial_and_telnet_status_updates_show_auto_reconnecting_and_reconnected_successfully() {
    use iced::window;
    use termewnal_protocols::TelnetConnectConfig;
    use termewnal_ui::RecoveryNoticeKind;

    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = boot_test_app(paths).unwrap();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // Launch quick Serial session slot (disconnected without Tokio runtime, then drive status events).
    let _ = state.update(Message::OpenProtocolLauncherModal(ProtocolLauncherTab::Serial));
    let _ = state.update(Message::ProtocolSerialPortChanged("/dev/ttyUSB0".to_string()));
    let _ = state.update(Message::LaunchQuickSerial);
    let serial_sid = state.active_session_id().unwrap();

    let _ = state.update(Message::SerialStatusChanged {
        session_id: serial_sid,
        generation: 1,
        status: ProtocolStatus::Reconnecting {
            attempt: 1,
            last_error: Some("serial cable disconnected".to_string()),
        },
    });
    let _ = state.update(Message::SerialStatusChanged {
        session_id: serial_sid,
        generation: 1,
        status: ProtocolStatus::Connecting,
    });
    assert!(state.serial_session(serial_sid).unwrap().is_auto_reconnecting());
    let serial_reconn = state.status_bar_snapshot();
    assert_eq!(
        serial_reconn.connection_notice.as_ref().unwrap().kind,
        RecoveryNoticeKind::AutoReconnecting
    );
    let _ = state.view(win);

    let _ = state.update(Message::SerialStatusChanged {
        session_id: serial_sid,
        generation: 1,
        status: ProtocolStatus::Connected,
    });
    assert!(state.serial_session(serial_sid).unwrap().is_recently_reconnected());
    let serial_done = state.status_bar_snapshot();
    assert_eq!(
        serial_done.connection_notice.as_ref().unwrap().kind,
        RecoveryNoticeKind::ReconnectedSuccessfully
    );
    assert_eq!(
        serial_done.connection_notice.as_ref().unwrap().message,
        "Reconnected successfully"
    );
    let _ = state.view(win);

    // Verify Telnet slot transitions identically.
    let _ = state.update(Message::OpenProtocolLauncherModal(ProtocolLauncherTab::Telnet));
    let _ = state.update(Message::ProtocolTelnetHostChanged("127.0.0.1".to_string()));
    let _ = state.update(Message::ProtocolTelnetPortChanged("2323".to_string()));
    let _ = state.update(Message::LaunchQuickTelnet);
    let telnet_sid = state.active_session_id().unwrap();
    let _: fn(&TelnetConnectConfig) = |_| {};

    let _ = state.update(Message::TelnetStatusChanged {
        session_id: telnet_sid,
        generation: 1,
        status: ProtocolStatus::Reconnecting {
            attempt: 2,
            last_error: Some("TCP reset".to_string()),
        },
    });
    assert!(state.telnet_session(telnet_sid).unwrap().is_auto_reconnecting());
    assert_eq!(
        state
            .status_bar_snapshot()
            .connection_notice
            .as_ref()
            .unwrap()
            .kind,
        RecoveryNoticeKind::AutoReconnecting
    );
    let _ = state.view(win);

    let _ = state.update(Message::TelnetStatusChanged {
        session_id: telnet_sid,
        generation: 1,
        status: ProtocolStatus::Connected,
    });
    assert!(state.telnet_session(telnet_sid).unwrap().is_recently_reconnected());
    assert_eq!(
        state
            .status_bar_snapshot()
            .connection_notice
            .as_ref()
            .unwrap()
            .kind,
        RecoveryNoticeKind::ReconnectedSuccessfully
    );
    let _ = state.view(win);
}
