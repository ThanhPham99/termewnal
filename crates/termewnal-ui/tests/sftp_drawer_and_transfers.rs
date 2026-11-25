use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Seek, SeekFrom, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::keyboard::{Key, Modifiers};
use iced::window;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use russh_sftp::protocol::{
    Attrs, Data, File as SftpProtoFile, FileAttributes, Handle, Name, OpenFlags, SftpHandle,
    SftpString, Status, StatusCode, Version,
};
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigPaths, Profile, SshAuthMethod, VaultBackend, VaultConfig, save_config,
    vault::{EncryptedVault, InMemorySecretStore, VaultSession},
};
use termewnal_ssh::{
    OsAgentConnector, RemoteEntryKind, SftpTransportMode, SshConnectConfig, TransferDirection,
    TransferStatus, record_known_host_path,
};
use termewnal_ui::command_palette::PaletteAction;
use termewnal_ui::sftp_drawer::{
    DEFAULT_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH, MIN_SFTP_DRAWER_WIDTH,
};
use termewnal_ui::status_bar::StatusBarAction;
use termewnal_ui::{
    AppState, CloseGuardState, Message, SshTargetSpec, SshUiEvent, TabId, drive_ssh_connect,
    shortcut_for_key_event,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

fn boot_test_state() -> (TempDir, AppState) {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    #[cfg(unix)]
    if Path::new("/bin/sh").exists() {
        let mut cfg = AppConfig::default();
        cfg.terminal.shell = Some("/bin/sh".to_string());
        save_config(&paths, &cfg).expect("write deterministic test shell config");
    }
    let state = AppState::boot_with_paths(paths).expect("boot AppState");
    (temp, state)
}

#[test]
fn ctrl_shift_e_and_status_bar_toggle_and_resize_sftp_drawer() {
    let tab = Some(TabId(1));
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;

    assert!(matches!(
        shortcut_for_key_event(&Key::Character("e".into()), ctrl_shift, tab),
        Some(Message::ToggleSftpDrawer)
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("E".into()), ctrl_shift, tab),
        Some(Message::ToggleSftpDrawer)
    ));

    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    assert!(!state.sftp_drawer().is_open());
    assert!(!state.status_bar_snapshot().transfer_drawer_open);
    assert_eq!(state.sftp_drawer().width(), DEFAULT_SFTP_DRAWER_WIDTH);

    // Toggle via Ctrl+Shift+E shortcut message.
    let _ = state.update(Message::ToggleSftpDrawer);
    assert!(state.sftp_drawer().is_open());
    assert!(state.status_bar_snapshot().transfer_drawer_open);

    // Resize clamps to supported bounds and renders cleanly in Iced view().
    let _ = state.update(Message::ResizeSftpDrawer(100.0));
    assert_eq!(state.sftp_drawer().width(), MIN_SFTP_DRAWER_WIDTH);
    let _ = state.update(Message::ResizeSftpDrawer(480.0));
    assert_eq!(state.sftp_drawer().width(), 480.0);
    let _ = state.update(Message::ResizeSftpDrawer(1500.0));
    assert_eq!(state.sftp_drawer().width(), MAX_SFTP_DRAWER_WIDTH);
    drop(state.view(win));

    // Status Bar toggle closes the drawer cleanly.
    let _ = state.update(Message::StatusBarAction(
        StatusBarAction::ToggleTransferDrawer,
    ));
    assert!(!state.sftp_drawer().is_open());
    assert!(!state.status_bar_snapshot().transfer_drawer_open);

    // Command Palette exposes SFTP Drawer toggle command.
    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("sftp".to_string()));
    let items = state.command_palette_items();
    assert!(
        items
            .iter()
            .any(|item| matches!(item.action, PaletteAction::ToggleSftpDrawer))
    );
    let _ = state.update(Message::CommandPaletteExecuteItem(
        PaletteAction::ToggleSftpDrawer,
    ));
    assert!(state.sftp_drawer().is_open());
}

#[test]
fn file_drop_routes_to_sftp_upload_on_drawer_or_shift_ssh_and_pastes_path_otherwise() {
    let (temp, mut state) = boot_test_state();
    let upload_file = temp.path().join("deploy_artifact.tar.gz");
    std::fs::write(&upload_file, vec![7u8; 4096]).unwrap();

    let local_sid = state.active_session_id().expect("local session");

    // 1. Shift+drop onto LOCAL terminal pane still pastes shell-escaped path, never enqueues SFTP.
    let _ = state.update(Message::TerminalInput {
        session_id: local_sid,
        bytes: b"printf 'L:%s\\n' ".to_vec(),
    });
    let _ = state.update(Message::FileDropped {
        session_id: local_sid,
        path: PathBuf::from("/tmp/local file.txt"),
        shift_held: true,
    });
    let _ = state.update(Message::TerminalInput {
        session_id: local_sid,
        bytes: b"\r".to_vec(),
    });
    assert!(state.sftp_drawer().transfers().is_empty());
    let local_pty = state.session(local_sid).unwrap().pty().clone();
    assert!(
        local_pty.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("L:/tmp/local file.txt")
        }),
        "expected dropped shell path output, got:\n{}",
        local_pty.snapshot().visible_text()
    );

    // 2. Open an SSH Session slot.
    let profile = Profile::ssh("Prod Box", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let ssh_sid = state.active_session_id().expect("ssh session");
    let _ = state.update(Message::SftpNavigateTo("/var/www/releases".to_string()));

    // Plain drop (shift_held = false) onto SSH pane does NOT enqueue an upload Transfer.
    let _ = state.update(Message::FileDropped {
        session_id: ssh_sid,
        path: upload_file.clone(),
        shift_held: false,
    });
    assert!(state.sftp_drawer().transfers().is_empty());

    // Shift + drop onto SSH pane enqueues an Upload Transfer targeting active remote directory.
    let _ = state.update(Message::FileDropped {
        session_id: ssh_sid,
        path: upload_file.clone(),
        shift_held: true,
    });
    assert_eq!(state.sftp_drawer().transfers().len(), 1);
    let first = &state.sftp_drawer().transfers()[0];
    assert_eq!(first.direction, TransferDirection::Upload);
    assert_eq!(first.local_path, upload_file);
    assert_eq!(first.remote_path, "/var/www/releases/deploy_artifact.tar.gz");
    assert_eq!(first.status, TransferStatus::Queued);

    // 3. Dropping directly onto the SFTP Drawer (without Shift) enqueues an Upload Transfer too.
    let second_file = temp.path().join("schema.sql");
    std::fs::write(&second_file, b"SELECT 1;").unwrap();
    let _ = state.update(Message::ToggleSftpDrawer);
    let _ = state.update(Message::SftpDrawerFileDropped {
        path: second_file.clone(),
    });
    assert_eq!(state.sftp_drawer().transfers().len(), 2);
    let second = &state.sftp_drawer().transfers()[1];
    assert_eq!(second.direction, TransferDirection::Upload);
    assert_eq!(second.local_path, second_file);
    assert_eq!(second.remote_path, "/var/www/releases/schema.sql");
    assert_eq!(second.status, TransferStatus::Queued);
}

#[test]
fn transfer_queue_ui_updates_pause_resume_cancel_retry_status_bar_and_close_guard() {
    let temp = TempDir::new().expect("create temp XDG root");
    let file_a = temp.path().join("alpha.bin");
    let file_b = temp.path().join("beta.bin");
    std::fs::write(&file_a, vec![1u8; 2048]).unwrap();
    std::fs::write(&file_b, vec![2u8; 4096]).unwrap();

    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let profile = Profile::ssh("App", "10.0.0.20", 22, "deploy", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let ssh_sid = state.active_session_id().expect("ssh session");
    let _ = state.update(Message::SftpNavigateTo("/srv/app".to_string()));
    let _ = state.update(Message::ToggleSftpDrawer);

    let _ = state.update(Message::FileDropped {
        session_id: ssh_sid,
        path: file_a,
        shift_held: true,
    });
    let _ = state.update(Message::FileDropped {
        session_id: ssh_sid,
        path: file_b,
        shift_held: true,
    });

    assert_eq!(state.background_work_tracker().active_transfers(), 2);
    assert_eq!(state.status_bar_snapshot().transfers.queued, 2);
    assert_eq!(state.status_bar_snapshot().transfers_text(), "Transfers: 2 queued");

    let id_a = state.sftp_drawer().transfers()[0].id;
    let id_b = state.sftp_drawer().transfers()[1].id;

    // Pause one transfer -> active/queued drops from 2 to 1.
    let _ = state.update(Message::PauseTransfer(id_a));
    assert_eq!(state.sftp_drawer().transfers()[0].status, TransferStatus::Paused);
    assert_eq!(state.background_work_tracker().active_transfers(), 1);
    assert_eq!(state.status_bar_snapshot().transfers.queued, 1);

    // Resume paused transfer -> back to 2 queued.
    let _ = state.update(Message::ResumeTransfer(id_a));
    assert_eq!(state.sftp_drawer().transfers()[0].status, TransferStatus::Queued);
    assert_eq!(state.background_work_tracker().active_transfers(), 2);

    // Cancel second transfer -> stays Cancelled, retry puts it back to Queued.
    let _ = state.update(Message::CancelTransfer(id_b));
    assert_eq!(
        state.sftp_drawer().transfers()[1].status,
        TransferStatus::Cancelled
    );
    assert_eq!(state.background_work_tracker().active_transfers(), 1);
    let _ = state.update(Message::RetryTransfer(id_b));
    assert_eq!(state.sftp_drawer().transfers()[1].status, TransferStatus::Queued);
    assert_eq!(state.background_work_tracker().active_transfers(), 2);

    // Closing last window with active Transfers triggers Close Guard confirmation!
    let _ = state.update(Message::CloseRequested(win));
    assert_eq!(
        state.close_guard(),
        CloseGuardState::Confirming {
            window_id: win,
            active_standalone_tunnels: 0,
            active_transfers: 2,
        }
    );
    drop(state.view(win));

    // Stop & Quit cancels active Transfers and clears tracker.
    let _ = state.update(Message::CloseGuardStopAndQuit);
    assert_eq!(state.background_work_tracker().active_transfers(), 0);
    assert!(
        state
            .sftp_drawer()
            .transfers()
            .iter()
            .all(|t| t.status == TransferStatus::Cancelled)
    );
}

#[derive(Clone)]
enum UiTestAuth {
    Kbi { user: String, otp: String },
}

struct UiSftpState {
    root_dir: PathBuf,
    auth: UiTestAuth,
}

impl UiSftpState {
    fn resolve(&self, raw: &str) -> PathBuf {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "." || trimmed == "/" {
            return self.root_dir.clone();
        }
        let mut clean = PathBuf::new();
        for comp in Path::new(trimmed.trim_start_matches('/')).components() {
            if let std::path::Component::Normal(p) = comp {
                clean.push(p);
            }
        }
        self.root_dir.join(clean)
    }

    fn virt(&self, abs: &Path) -> String {
        match abs.strip_prefix(&self.root_dir) {
            Ok(rel) if rel.as_os_str().is_empty() => "/".to_string(),
            Ok(rel) => format!(
                "/{}",
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            Err(_) => "/".to_string(),
        }
    }
}

struct UiOpenHandle {
    file: Option<std::fs::File>,
    dir_entries: Option<Vec<SftpProtoFile>>,
}

struct UiSftpSubsystem {
    state: Arc<UiSftpState>,
    next_id: u64,
    handles: HashMap<Vec<u8>, UiOpenHandle>,
}

impl UiSftpSubsystem {
    fn new(state: Arc<UiSftpState>) -> Self {
        Self {
            state,
            next_id: 1,
            handles: HashMap::new(),
        }
    }

    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        }
    }
}

impl russh_sftp::server::Handler for UiSftpSubsystem {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: SftpString) -> Result<Name, Self::Error> {
        let abs = self.state.resolve(&path);
        Ok(Name {
            id,
            files: vec![SftpProtoFile::dummy(self.state.virt(&abs))],
        })
    }

    async fn opendir(&mut self, id: u32, path: SftpString) -> Result<Handle, Self::Error> {
        let abs = self.state.resolve(&path);
        let mut files = Vec::new();
        for entry in fs::read_dir(&abs).map_err(|_| StatusCode::NoSuchFile)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(meta) = entry.metadata() {
                files.push(SftpProtoFile::new(name, FileAttributes::from(&meta)));
            }
        }
        let h = self.next_id.to_be_bytes().to_vec();
        self.next_id += 1;
        self.handles.insert(
            h.clone(),
            UiOpenHandle {
                file: None,
                dir_entries: Some(files),
            },
        );
        Ok(Handle {
            id,
            handle: h.into(),
        })
    }

    async fn readdir(&mut self, id: u32, handle: SftpHandle) -> Result<Name, Self::Error> {
        let entry = self
            .handles
            .get_mut(handle.as_bytes())
            .ok_or(StatusCode::Failure)?;
        let files = entry.dir_entries.take().unwrap_or_default();
        if files.is_empty() {
            Err(StatusCode::Eof)
        } else {
            Ok(Name { id, files })
        }
    }

    async fn stat(&mut self, id: u32, path: SftpString) -> Result<Attrs, Self::Error> {
        let abs = self.state.resolve(&path);
        let meta = fs::metadata(abs).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn open(
        &mut self,
        id: u32,
        filename: SftpString,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let abs = self.state.resolve(&filename);
        let mut opts = OpenOptions::new();
        opts.read(pflags.contains(OpenFlags::READ));
        opts.write(pflags.contains(OpenFlags::WRITE));
        opts.create(pflags.contains(OpenFlags::CREATE));
        opts.truncate(pflags.contains(OpenFlags::TRUNCATE));
        let file = opts.open(abs).map_err(|_| StatusCode::NoSuchFile)?;
        let h = self.next_id.to_be_bytes().to_vec();
        self.next_id += 1;
        self.handles.insert(
            h.clone(),
            UiOpenHandle {
                file: Some(file),
                dir_entries: None,
            },
        );
        Ok(Handle {
            id,
            handle: h.into(),
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: SftpHandle,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let entry = self
            .handles
            .get_mut(handle.as_bytes())
            .ok_or(StatusCode::Failure)?;
        let file = entry.file.as_mut().ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        let mut buf = vec![0u8; len as usize];
        let n = file.read(&mut buf).map_err(|_| StatusCode::Failure)?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: SftpHandle,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let entry = self
            .handles
            .get_mut(handle.as_bytes())
            .ok_or(StatusCode::Failure)?;
        let file = entry.file.as_mut().ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        file.write_all(&data).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok(id))
    }

    async fn close(&mut self, id: u32, handle: SftpHandle) -> Result<Status, Self::Error> {
        self.handles.remove(handle.as_bytes());
        Ok(Self::ok(id))
    }
}

struct UiSshHandler {
    state: Arc<UiSftpState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    shells: BTreeSet<ChannelId>,
    kbi_stage: u8,
}

impl server::Handler for UiSshHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_keyboard_interactive(
        &mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'_>>,
    ) -> Result<server::Auth, Self::Error> {
        let UiTestAuth::Kbi { user: u, otp } = &self.state.auth;
        if user != u {
            return Ok(server::Auth::reject());
        }
        match (self.kbi_stage, response) {
            (0, None) => {
                self.kbi_stage = 1;
                Ok(server::Auth::Partial {
                    name: "2FA".into(),
                    instructions: "Enter OTP".into(),
                    prompts: vec![("OTP: ".into(), true)].into(),
                })
            }
            (1, Some(mut resp)) => {
                let ans = resp.next().map(|b| String::from_utf8_lossy(&b).into_owned());
                if ans.as_deref() == Some(otp.as_str()) {
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
        _term: &str,
        _cols: u32,
        _rows: u32,
        _px_w: u32,
        _px_h: u32,
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
        self.shells.insert(channel);
        session.channel_success(channel)?;
        session.data(channel, b"UI_SHELL_READY\r\n".to_vec())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.shells.contains(&channel) {
            session.data(channel, data.to_vec())?;
        }
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp"
            && let Some(ch) = self.channels.remove(&channel_id)
        {
            session.channel_success(channel_id)?;
            russh_sftp::server::run(
                ch.into_stream(),
                UiSftpSubsystem::new(Arc::clone(&self.state)),
            )
            .await;
            return Ok(());
        }
        session.channel_failure(channel_id)?;
        Ok(())
    }
}

async fn spawn_ui_sftp_server(
    root_dir: PathBuf,
    auth: UiTestAuth,
) -> (SocketAddr, PublicKey, oneshot::Sender<()>) {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();
    let mut config = server::Config::default();
    config.auth_rejection_time = Duration::from_millis(1);
    config.auth_rejection_time_initial = Some(Duration::from_millis(1));
    config.inactivity_timeout = None;
    config.methods = MethodSet::from(&[MethodKind::Password, MethodKind::KeyboardInteractive][..]);
    config.keys.push(host_key);
    let config = Arc::new(config);
    let state = Arc::new(UiSftpState { root_dir, auth });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    let cfg = Arc::clone(&config);
                    let st = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _ = server::run_stream(
                            cfg,
                            stream,
                            UiSshHandler {
                                state: st,
                                channels: HashMap::new(),
                                shells: BTreeSet::new(),
                                kbi_stage: 0,
                            },
                        )
                        .await;
                    });
                }
            }
        }
    });
    (addr, host_pubkey, tx)
}

#[tokio::test]
async fn live_app_state_browses_remote_sftp_uploads_downloads_and_surfaces_mux_fallback() {
    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    let local_uploads = temp.path().join("uploads");
    let local_downloads = temp.path().join("downloads");
    fs::create_dir_all(remote_root.join("releases")).unwrap();
    fs::write(remote_root.join("releases/readme.txt"), b"remote-readme-payload").unwrap();
    fs::create_dir_all(&local_uploads).unwrap();
    fs::create_dir_all(&local_downloads).unwrap();

    let upload_payload = vec![0x5Au8; 600 * 1024];
    let upload_src = local_uploads.join("bundle.bin");
    fs::write(&upload_src, &upload_payload).unwrap();

    let (addr, host_pubkey, _shutdown) = spawn_ui_sftp_server(
        remote_root.clone(),
        UiTestAuth::Kbi {
            user: "secops".to_string(),
            otp: "424242".to_string(),
        },
    )
    .await;

    let paths = ConfigPaths::from_root(temp.path());
    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    let mut state = AppState::boot_with_paths_and_vault(paths, Arc::clone(&vault)).unwrap();
    state.set_sftp_download_dir(local_downloads.clone());
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let known_hosts = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", addr.port(), &host_pubkey, &known_hosts).unwrap();

    let spec = SshTargetSpec::quick_connect(
        "2FA SFTP Host",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            username: "secops".to_string(),
            auth: SshAuthMethod::KeyboardInteractive,
            known_hosts_path: Some(known_hosts),
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
        },
    )
    .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let _ = state.update(Message::QuickConnectSsh(spec.clone()));
    let ssh_sid = state.active_session_id().unwrap();
    let slot = state.ssh_session(ssh_sid).unwrap();
    let gen_id = slot.ssh_generation();
    let buffer = Arc::clone(slot.buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        ssh_sid,
        gen_id,
        spec,
        buffer,
        vault,
        ev_tx,
    ));

    // Answer 2FA prompt and wait for Connected event.
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("SSH connect timeout")
            .expect("event channel open");
        let is_connected = matches!(ev, SshUiEvent::Connected { .. });
        let is_auth_prompt = matches!(ev, SshUiEvent::AuthPrompt { .. });
        let _ = state.update(Message::SshEvent {
            session_id: ssh_sid,
            event: ev,
        });
        if is_auth_prompt {
            let _ = state.update(Message::SshAuthInputChanged {
                session_id: ssh_sid,
                index: 0,
                value: "424242".to_string(),
            });
            let _ = state.update(Message::SubmitSshAuthPrompt(ssh_sid));
        }
        if is_connected {
            break;
        }
    }

    // Open SFTP Drawer via ToggleSftpDrawer (Ctrl+Shift+E) and list "/releases".
    let _ = state.update(Message::ToggleSftpDrawer);
    assert!(state.sftp_drawer().is_open());

    let ssh_slot = state.ssh_session(ssh_sid).unwrap();
    let endpoint = termewnal_ssh::SftpEndpoint::new(
        termewnal_ssh::ProfileTransportKey::from_connect_config(
            None,
            &ssh_slot.spec().connect_config,
        ),
        ssh_slot.spec().connect_config.clone(),
    )
    .with_primary_transport(Arc::clone(ssh_slot.ssh_session().unwrap().primary_transport()));

    let listing = state
        .sftp_manager()
        .list_dir(&endpoint, "/releases")
        .await
        .unwrap();
    assert_eq!(listing.transport_mode, SftpTransportMode::MuxFallback);

    // Feed browse result into AppState and verify drawer entries & MuxFallback notice.
    let _ = state.update(Message::SftpNavigateTo("/releases".to_string()));
    let _ = state.update(Message::SftpBrowseResult {
        session_id: ssh_sid,
        generation: 2,
        result: Ok(listing),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/releases");
    assert_eq!(state.sftp_drawer().entries().len(), 1);
    assert_eq!(state.sftp_drawer().entries()[0].name, "readme.txt");
    assert_eq!(state.sftp_drawer().entries()[0].kind, RemoteEntryKind::File);
    assert_eq!(
        state.sftp_drawer().transport_notice().map(|n| n.mode),
        Some(SftpTransportMode::MuxFallback)
    );

    // Enqueue upload via drawer drop + download via SftpDownloadFile.
    let _ = state.update(Message::SftpDrawerFileDropped {
        path: upload_src,
    });
    let _ = state.update(Message::SftpDownloadFile {
        remote_path: "/releases/readme.txt".to_string(),
        file_name: "readme.txt".to_string(),
    });
    assert!(state.background_work_tracker().active_transfers() > 0);

    // Wait for async TransferQueue workers to complete, then sync snapshot into AppState.
    assert!(
        state
            .transfer_queue()
            .wait_until_idle(Duration::from_secs(10))
            .await
    );
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    assert_eq!(state.background_work_tracker().active_transfers(), 0);
    assert_eq!(state.sftp_drawer().transfers().len(), 2);
    assert_eq!(
        state.sftp_drawer().transfers()[0].direction,
        TransferDirection::Upload
    );
    assert_eq!(
        state.sftp_drawer().transfers()[0].status,
        TransferStatus::Completed
    );
    assert_eq!(
        state.sftp_drawer().transfers()[1].direction,
        TransferDirection::Download
    );
    assert_eq!(
        state.sftp_drawer().transfers()[1].status,
        TransferStatus::Completed
    );
    assert_eq!(
        fs::read(remote_root.join("releases/bundle.bin")).unwrap(),
        upload_payload
    );
    assert_eq!(
        fs::read(local_downloads.join("readme.txt")).unwrap(),
        b"remote-readme-payload"
    );

    // Interactive SSH terminal session still responds immediately after SFTP transfers.
    let ssh = state.ssh_session(ssh_sid).unwrap().ssh_session().unwrap().clone();
    ssh.write_bytes(b"AFTER_SFTP_OK\r").unwrap();
    assert!(
        ssh.wait_for_output_async(Duration::from_secs(3), |s| {
            s.visible_text().contains("AFTER_SFTP_OK")
        })
        .await
    );

    drop(state.view(win));

    // Navigating up resolves parent directory and ClearFinishedTransfers removes completed entries.
    let _ = state.update(Message::SftpNavigateUp);
    assert_eq!(state.sftp_drawer().current_path(), "/");
    let _ = state.update(Message::ClearFinishedTransfers);
    assert!(state.sftp_drawer().transfers().is_empty());
}

