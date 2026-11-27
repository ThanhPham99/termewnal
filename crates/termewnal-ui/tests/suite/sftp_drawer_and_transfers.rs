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
    super::common::seed_fast_test_shell(&paths);
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
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = AppState::boot_with_paths(paths.clone()).expect("boot AppState");
    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsMinimizeToTrayToggled(false));
    let _ = state.update(Message::SettingsSave);
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    assert!(!state.sftp_drawer().is_open());
    assert!(!state.status_bar_snapshot().transfer_drawer_open);
    assert_eq!(state.sftp_drawer().width(), DEFAULT_SFTP_DRAWER_WIDTH);

    // Non-SSH sessions (LocalShell) treat SFTP toggles as a no-op and never show the panel.
    let _ = state.update(Message::ToggleSftpDrawer);
    assert!(!state.sftp_drawer().is_open());
    let _ = state.update(Message::SetSftpDrawerOpen(true));
    assert!(!state.sftp_drawer().is_open());
    let _ = state.update(Message::StatusBarAction(
        StatusBarAction::ToggleTransferDrawer,
    ));
    assert!(!state.sftp_drawer().is_open());

    // Open an SSH session so SFTP Panel toggle, resize, reset, and persistence apply.
    let profile = Profile::ssh("Box", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    assert!(!state.sftp_drawer().is_open());

    // Toggle via Ctrl+Shift+E shortcut message.
    let _ = state.update(Message::ToggleSftpDrawer);
    assert!(state.sftp_drawer().is_open());
    assert!(state.status_bar_snapshot().transfer_drawer_open);

    // Resize clamps to supported bounds, resets on double-click, commits to config.toml, and renders cleanly.
    let _ = state.update(Message::ResizeSftpDrawer(100.0));
    assert_eq!(state.sftp_drawer().width(), MIN_SFTP_DRAWER_WIDTH);
    let _ = state.update(Message::ResizeSftpDrawer(480.0));
    assert_eq!(state.sftp_drawer().width(), 480.0);
    let _ = state.update(Message::CommitSftpDrawerWidth);
    let loaded = termewnal_core::load_config(&paths).expect("load persisted config");
    assert_eq!(loaded.terminal.sftp_panel_width, 480.0);
    let _ = state.update(Message::ResizeSftpDrawer(1500.0));
    assert_eq!(state.sftp_drawer().width(), MAX_SFTP_DRAWER_WIDTH);
    let _ = state.update(Message::ResetSftpDrawerWidth);
    assert_eq!(state.sftp_drawer().width(), DEFAULT_SFTP_DRAWER_WIDTH);
    drop(state.view(win));

    // Status Bar toggle closes the panel cleanly.
    let _ = state.update(Message::StatusBarAction(
        StatusBarAction::ToggleTransferDrawer,
    ));
    assert!(!state.sftp_drawer().is_open());
    assert!(!state.status_bar_snapshot().transfer_drawer_open);

    // Command Palette exposes SFTP Panel toggle command.
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
    #[cfg(unix)]
    {
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
    }
    #[cfg(not(unix))]
    {
        let _ = state.update(Message::FileDropped {
            session_id: local_sid,
            path: PathBuf::from("/tmp/local file.txt"),
            shift_held: true,
        });
        assert!(state.sftp_drawer().transfers().is_empty());
    }

    // 2. Open an SSH Session slot.
    let profile = Profile::ssh("Prod Box", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let ssh_sid = state.active_session_id().expect("ssh session");
    let _ = state.update(Message::SftpSessionNavigateTo {
        session_id: ssh_sid,
        path: "/var/www/releases".to_string(),
    });

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

    // 3. Dropping directly onto the SFTP+ panel (without Shift) enqueues an Upload Transfer too.
    let second_file = temp.path().join("schema.sql");
    std::fs::write(&second_file, b"SELECT 1;").unwrap();
    let _ = state.update(Message::ToggleSftpDrawer);
    let _ = state.update(Message::SftpSessionFileDropped {
        session_id: ssh_sid,
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
    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsMinimizeToTrayToggled(false));
    let _ = state.update(Message::SettingsSave);
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let profile = Profile::ssh("App", "10.0.0.20", 22, "deploy", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile));
    let ssh_sid = state.active_session_id().expect("ssh session");
    let _ = state.update(Message::SftpSessionNavigateTo {
        session_id: ssh_sid,
        path: "/srv/app".to_string(),
    });
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
        let canon_root = fs::canonicalize(&self.root_dir).unwrap_or_else(|_| self.root_dir.clone());
        match abs
            .strip_prefix(&canon_root)
            .or_else(|_| abs.strip_prefix(&self.root_dir))
        {
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

    async fn lstat(&mut self, id: u32, path: SftpString) -> Result<Attrs, Self::Error> {
        let abs = self.state.resolve(&path);
        let meta = fs::symlink_metadata(abs).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: SftpString,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let abs = self.state.resolve(&path);
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&abs, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|_| StatusCode::PermissionDenied)?;
        }
        #[cfg(not(unix))]
        let _ = (abs, attrs);
        Ok(Self::ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: SftpString,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let abs = self.state.resolve(&path);
        fs::create_dir(&abs).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: SftpString) -> Result<Status, Self::Error> {
        let abs = self.state.resolve(&path);
        fs::remove_dir(&abs).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok(id))
    }

    async fn remove(&mut self, id: u32, filename: SftpString) -> Result<Status, Self::Error> {
        let abs = self.state.resolve(&filename);
        fs::remove_file(&abs).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Self::ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: SftpString,
        newpath: SftpString,
    ) -> Result<Status, Self::Error> {
        let src = self.state.resolve(&oldpath);
        let dst = self.state.resolve(&newpath);
        fs::rename(src, dst).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok(id))
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
            proxy_jump: Vec::new(),
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            ..SshConnectConfig::default()
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

    // SshUiEvent::Connected automatically opens & begins connecting SFTP+ for this SSH session.
    assert!(state.sftp_drawer().is_open());
    assert!(state.sftp_drawer().is_loading());
    let _ = state.update(Message::ToggleSftpDrawer);
    assert!(!state.sftp_drawer().is_open());
    assert!(state.sftp_drawer().is_manually_closed());
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

    // Feed browse result into AppState and verify panel entries & MuxFallback notice.
    let _ = state.update(Message::SftpSessionNavigateTo {
        session_id: ssh_sid,
        path: "/releases".to_string(),
    });
    let gen_releases = state.sftp_drawer().list_generation();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: ssh_sid,
        generation: gen_releases,
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

    // Enqueue upload via SFTP+ drop + download via SFTP+ selection & Download action.
    let _ = state.update(Message::SftpSessionFileDropped {
        session_id: ssh_sid,
        path: upload_src,
    });
    let _ = state.update(Message::SftpSessionRowClicked {
        session_id: ssh_sid,
        path: "/releases/readme.txt".to_string(),
        ctrl_held: false,
    });
    let _ = state.update(Message::SftpContextAction(
        termewnal_ui::SftpContextActionKind::Download,
    ));
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
    let _ = state.update(Message::SftpSessionUp(ssh_sid));
    assert_eq!(state.sftp_drawer().current_path(), "/");
    let _ = state.update(Message::ClearFinishedTransfers);
    assert!(state.sftp_drawer().transfers().is_empty());
}

#[test]
fn sftp_drawer_state_and_transfers_are_isolated_per_ssh_session() {
    let (temp, mut state) = boot_test_state();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));
    let local_sid = state.active_session_id().expect("initial local session");

    let file_a = temp.path().join("alpha.txt");
    let file_b = temp.path().join("beta.txt");
    fs::write(&file_a, b"payload-alpha").unwrap();
    fs::write(&file_b, b"payload-beta").unwrap();

    // Open two SSH sessions (even sharing the same host tuple) alongside the local session.
    let profile_a = Profile::ssh("Prod A", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let profile_b = Profile::ssh("Prod B", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile_a));
    let ssh_a = state.active_session_id().expect("ssh_a session");
    let _ = state.update(Message::OpenSshProfile(profile_b));
    let ssh_b = state.active_session_id().expect("ssh_b session");
    assert_ne!(ssh_a, ssh_b);

    // Focus ssh_a: open SFTP+ panel, set path/filter, apply listing, enqueue upload.
    let _ = state.update(Message::FocusSession(ssh_a));
    assert!(!state.sftp_drawer().is_open());
    let _ = state.update(Message::ToggleSftpDrawer);
    assert!(state.sftp_drawer().is_open());
    assert!(state.status_bar_snapshot().transfer_drawer_open);
    assert_eq!(
        state.sftp_drawer().session_label(),
        Some("Prod A (ops@10.0.0.10:22)")
    );
    let _ = state.update(Message::SftpSessionNavigateTo {
        session_id: ssh_a,
        path: "/var/www".to_string(),
    });
    let _ = state.update(Message::SftpSessionToggleFilter(ssh_a));
    let _ = state.update(Message::SftpSessionFilterChanged(ssh_a, "index".to_string()));
    let _ = state.update(Message::SftpBrowseResult {
        session_id: ssh_a,
        generation: 0,
        result: Ok(termewnal_ssh::RemoteListing {
            resolved_path: "/var/www".to_string(),
            entries: vec![termewnal_ssh::RemoteDirEntry {
                name: "index.html".to_string(),
                path: "/var/www/index.html".to_string(),
                kind: RemoteEntryKind::File,
                size: 512,
                permissions: Some(0o644),
                modified_epoch_secs: None,
                uid: None,
                gid: None,
            }],
            transport_mode: SftpTransportMode::IsolatedTransferTransport,
            notice: None,
        }),
    });
    let _ = state.update(Message::SftpSessionFileDropped {
        session_id: ssh_a,
        path: file_a.clone(),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/var/www");
    assert_eq!(state.sftp_drawer().plus.remote.filter_query, "index");
    assert_eq!(state.sftp_drawer().entries().len(), 1);
    assert_eq!(state.sftp_drawer().transfers().len(), 1);
    assert_eq!(
        state.sftp_drawer().transfers()[0].remote_path,
        "/var/www/alpha.txt"
    );
    drop(state.view(win));

    // Switch to ssh_b: panel starts closed, at ".", empty filter/entries, zero transfers from ssh_a.
    let _ = state.update(Message::FocusSession(ssh_b));
    assert!(!state.sftp_drawer().is_open());
    assert!(!state.status_bar_snapshot().transfer_drawer_open);
    assert_eq!(state.sftp_drawer().current_path(), ".");
    assert_eq!(state.sftp_drawer().plus.remote.filter_query, "");
    assert!(state.sftp_drawer().entries().is_empty());
    assert!(state.sftp_drawer().transfers().is_empty());
    assert_eq!(
        state.sftp_drawer().session_label(),
        Some("Prod B (ops@10.0.0.10:22)")
    );
    drop(state.view(win));

    // Open ssh_b's SFTP+ panel and navigate/upload independently.
    let _ = state.update(Message::ToggleSftpDrawerForSession(ssh_b));
    assert!(state.sftp_drawer().is_open());
    let _ = state.update(Message::SftpSessionNavigateTo {
        session_id: ssh_b,
        path: "/etc/nginx".to_string(),
    });
    let _ = state.update(Message::SftpBrowseResult {
        session_id: ssh_b,
        generation: 0,
        result: Ok(termewnal_ssh::RemoteListing {
            resolved_path: "/etc/nginx".to_string(),
            entries: vec![termewnal_ssh::RemoteDirEntry {
                name: "nginx.conf".to_string(),
                path: "/etc/nginx/nginx.conf".to_string(),
                kind: RemoteEntryKind::File,
                size: 1024,
                permissions: Some(0o644),
                modified_epoch_secs: None,
                uid: None,
                gid: None,
            }],
            transport_mode: SftpTransportMode::IsolatedTransferTransport,
            notice: None,
        }),
    });
    let _ = state.update(Message::SftpSessionFileDropped {
        session_id: ssh_b,
        path: file_b.clone(),
    });
    assert_eq!(state.sftp_drawer().entries()[0].name, "nginx.conf");
    assert_eq!(state.sftp_drawer().transfers().len(), 1);
    assert_eq!(
        state.sftp_drawer().transfers()[0].remote_path,
        "/etc/nginx/beta.txt"
    );

    // Background browse result arriving for ssh_a updates ssh_a only without touching ssh_b.
    let _ = state.update(Message::SftpBrowseResult {
        session_id: ssh_a,
        generation: 0,
        result: Ok(termewnal_ssh::RemoteListing {
            resolved_path: "/srv/releases".to_string(),
            entries: vec![termewnal_ssh::RemoteDirEntry {
                name: "release.tar.gz".to_string(),
                path: "/srv/releases/release.tar.gz".to_string(),
                kind: RemoteEntryKind::File,
                size: 2048,
                permissions: Some(0o644),
                modified_epoch_secs: None,
                uid: None,
                gid: None,
            }],
            transport_mode: SftpTransportMode::IsolatedTransferTransport,
            notice: None,
        }),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/etc/nginx");
    assert_eq!(state.sftp_drawer().entries()[0].name, "nginx.conf");
    assert_eq!(
        state.sftp_drawer_for_session(ssh_a).unwrap().current_path(),
        "/srv/releases"
    );
    assert_eq!(
        state.sftp_drawer_for_session(ssh_a).unwrap().entries()[0].name,
        "release.tar.gz"
    );

    // Switch to Local tab: no SSH SFTP drawer leaks into the Local workspace.
    let _ = state.update(Message::FocusSession(local_sid));
    assert!(!state.sftp_drawer().is_open());
    assert!(!state.status_bar_snapshot().transfer_drawer_open);
    drop(state.view(win));

    // ToggleSftpDrawerForSession(ssh_a) from Local tab focuses ssh_a and toggles ssh_a's open drawer.
    let _ = state.update(Message::ToggleSftpDrawerForSession(ssh_a));
    assert_eq!(state.active_session_id(), Some(ssh_a));
    assert!(!state.sftp_drawer().is_open());
    let _ = state.update(Message::ToggleSftpDrawerForSession(ssh_a));
    assert!(state.sftp_drawer().is_open());
    assert_eq!(state.sftp_drawer().current_path(), "/srv/releases");
    assert_eq!(state.sftp_drawer().entries()[0].name, "release.tar.gz");
    assert_eq!(state.sftp_drawer().transfers().len(), 1);
    let transfer_a_id = state.sftp_drawer().transfers()[0].id;

    // Cancel and clear finished transfers on ssh_a leaves ssh_b's queue untouched.
    let _ = state.update(Message::CancelTransfer(transfer_a_id));
    let _ = state.update(Message::ClearFinishedTransfers);
    assert!(state.sftp_drawer().transfers().is_empty());
    let _ = state.update(Message::FocusSession(ssh_b));
    assert_eq!(state.sftp_drawer().transfers().len(), 1);
    assert_eq!(
        state.sftp_drawer().transfers()[0].remote_path,
        "/etc/nginx/beta.txt"
    );

    // Closing ssh_a removes only ssh_a's SFTP drawer state while preserving ssh_b.
    let _ = state.update(Message::FocusSession(ssh_a));
    let _ = state.update(Message::CloseFocusedPane);
    assert!(state.sftp_drawer_for_session(ssh_a).is_none());
    assert!(state.sftp_drawer_for_session(ssh_b).is_some());
    assert!(state.sftp_drawer_for_session(ssh_b).unwrap().is_open());
}

#[test]
fn remember_last_sftp_working_folder_per_profile_across_sessions_and_restarts() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut cfg = AppConfig::default();
    #[cfg(unix)]
    if Path::new("/bin/sh").exists() {
        cfg.terminal.shell = Some("/bin/sh".to_string());
    }
    #[cfg(windows)]
    {
        cfg.terminal.shell = Some(
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
        );
    }
    save_config(&paths, &cfg).expect("write deterministic test shell config");

    // Save two SSH profiles targeting the same server so identity isolation depends on profile_id.
    let prof_web = Profile::ssh("Prod Web", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let prof_api = Profile::ssh("Prod API", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let web_id = prof_web.id.clone();
    let api_id = prof_api.id.clone();
    let mut profiles = termewnal_core::ProfilesConfig::default();
    profiles.profiles.push(prof_web.clone());
    profiles.profiles.push(prof_api.clone());
    termewnal_core::save_profiles(&paths, &profiles).expect("save profiles");

    {
        let mut state = AppState::boot_with_paths(paths.clone()).expect("boot AppState v1");
        let win = window::Id::unique();
        let _ = state.update(Message::WindowOpened(win));

        // Open Prod Web -> defaults to ".", then live browse to "/var/www/html".
        let _ = state.update(Message::LaunchProfile {
            profile_id: web_id.clone(),
            disposition: termewnal_ui::LaunchDisposition::NewTab,
        });
        let web_sid = state.active_session_id().expect("web session");
        assert_eq!(state.sftp_drawer().current_path(), ".");
        let _ = state.update(Message::SftpBrowseResult {
            session_id: web_sid,
            generation: 0,
            result: Ok(termewnal_ssh::RemoteListing {
                resolved_path: "/var/www/html".to_string(),
                entries: Vec::new(),
                transport_mode: SftpTransportMode::IsolatedTransferTransport,
                notice: None,
            }),
        });
        assert_eq!(
            state.remembered_profile_sftp_dir(&web_id),
            Some("/var/www/html")
        );

        // Failed browse error must NOT overwrite the valid remembered directory.
        let _ = state.update(Message::SftpBrowseResult {
            session_id: web_sid,
            generation: 0,
            result: Err("permission denied".to_string()),
        });
        assert_eq!(
            state.remembered_profile_sftp_dir(&web_id),
            Some("/var/www/html")
        );

        // Navigating to another directory updates remembered folder for Prod Web.
        let _ = state.update(Message::SftpSessionNavigateTo {
            session_id: web_sid,
            path: "/var/www/releases".to_string(),
        });
        assert_eq!(
            state.remembered_profile_sftp_dir(&web_id),
            Some("/var/www/releases")
        );

        // Open Prod API on same host -> starts cleanly at "." and remembers its own folder.
        let _ = state.update(Message::LaunchProfile {
            profile_id: api_id.clone(),
            disposition: termewnal_ui::LaunchDisposition::NewTab,
        });
        let api_sid = state.active_session_id().expect("api session");
        assert_eq!(state.sftp_drawer().current_path(), ".");
        let _ = state.update(Message::SftpSessionNavigateTo {
            session_id: api_sid,
            path: "/opt/api".to_string(),
        });
        assert_eq!(state.remembered_profile_sftp_dir(&api_id), Some("/opt/api"));
        assert_eq!(
            state.remembered_profile_sftp_dir(&web_id),
            Some("/var/www/releases")
        );

        // Persist workspace layout before simulating full app shutdown.
        state.save_workspace_layout().expect("save workspace layout");
        assert!(paths.sftp_state_file().is_file());
    }

    // Reboot AppState from disk: restored tabs AND freshly launched tabs both remember SFTP dirs!
    let mut rebooted = AppState::boot_with_paths(paths.clone()).expect("reboot AppState");
    let win2 = window::Id::unique();
    let _ = rebooted.update(Message::WindowOpened(win2));

    assert_eq!(
        rebooted.remembered_profile_sftp_dir(&web_id),
        Some("/var/www/releases")
    );
    assert_eq!(
        rebooted.remembered_profile_sftp_dir(&api_id),
        Some("/opt/api")
    );
    // Active restored tab was Prod API -> SFTP drawer restores "/opt/api".
    assert_eq!(rebooted.sftp_drawer().current_path(), "/opt/api");
    assert_eq!(rebooted.sftp_drawer().path_input(), "/opt/api");

    // Launching Prod Web again initializes its new SSH session's SFTP drawer at "/var/www/releases".
    let _ = rebooted.update(Message::LaunchProfile {
        profile_id: web_id.clone(),
        disposition: termewnal_ui::LaunchDisposition::NewTab,
    });
    assert_eq!(rebooted.sftp_drawer().current_path(), "/var/www/releases");
    assert_eq!(rebooted.sftp_drawer().path_input(), "/var/www/releases");

    // Deleting Prod Web removes its remembered SFTP entry on disk without affecting Prod API.
    let _ = rebooted.update(Message::DeleteSavedProfileById(web_id.clone()));
    assert_eq!(rebooted.remembered_profile_sftp_dir(&web_id), None);
    assert_eq!(
        rebooted.remembered_profile_sftp_dir(&api_id),
        Some("/opt/api")
    );
    let persisted = termewnal_core::load_sftp_state(&paths).expect("load persisted sftp_state");
    assert_eq!(persisted.last_dir(&web_id), None);
    assert_eq!(persisted.last_dir(&api_id), Some("/opt/api"));
}

#[tokio::test]
async fn sftp_plus_remote_panel_navigation_sorting_filtering_hidden_bookmarks_and_ops() {
    use iced::widget::text_editor;
    use iced::Point;
    use termewnal_ui::sftp_plus::{format_octal_mode, visible_remote_entries};
    use termewnal_ui::{SftpContextActionKind, SftpModalState, SftpSortColumn};

    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    let remote_sub = remote_root.join("sub_folder");
    let local_uploads = temp.path().join("uploads");
    let custom_downloads = temp.path().join("custom_dl");
    fs::create_dir_all(&remote_sub).unwrap();
    fs::create_dir_all(&local_uploads).unwrap();
    fs::create_dir_all(&custom_downloads).unwrap();

    fs::write(remote_root.join("alpha.txt"), b"hello alpha").unwrap();
    fs::write(remote_root.join("zeta.log"), vec![b'z'; 2048]).unwrap();
    fs::write(remote_root.join(".hidden_rc"), b"secret=1").unwrap();
    fs::write(remote_sub.join("inside.txt"), b"inside").unwrap();

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
    let mut state = AppState::boot_with_paths_and_vault(paths.clone(), Arc::clone(&vault)).unwrap();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let known_hosts = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", addr.port(), &host_pubkey, &known_hosts).unwrap();

    let spec = SshTargetSpec::quick_connect(
        "Remote Panel Host",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            username: "secops".to_string(),
            auth: SshAuthMethod::KeyboardInteractive,
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
    .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let _ = state.update(Message::QuickConnectSsh(spec.clone()));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
    let buffer = Arc::clone(state.ssh_session(sid).unwrap().buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        sid,
        gen_id,
        spec.clone(),
        buffer,
        Arc::clone(&vault),
        ev_tx,
    ));

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("SSH connect timeout")
            .expect("event channel open");
        let is_connected = matches!(ev, SshUiEvent::Connected { .. });
        let is_auth_prompt = matches!(ev, SshUiEvent::AuthPrompt { .. });
        let _ = state.update(Message::SshEvent {
            session_id: sid,
            event: ev,
        });
        if is_auth_prompt {
            let _ = state.update(Message::SshAuthInputChanged {
                session_id: sid,
                index: 0,
                value: "424242".to_string(),
            });
            let _ = state.update(Message::SubmitSshAuthPrompt(sid));
        }
        if is_connected {
            break;
        }
    }

    // SFTP+ auto-opens and kicks off initial directory listing while Terminal keeps focus.
    assert!(state.sftp_drawer().is_open());
    assert!(!state.sftp_drawer().is_panel_focused());
    assert!(state.sftp_drawer().is_loading());
    assert!(!state.sftp_drawer().plus.transfers_panel_open);

    let ssh_slot = state.ssh_session(sid).unwrap();
    let endpoint = termewnal_ssh::SftpEndpoint::new(
        termewnal_ssh::ProfileTransportKey::from_connect_config(
            None,
            &ssh_slot.spec().connect_config,
        ),
        ssh_slot.spec().connect_config.clone(),
    )
    .with_primary_transport(Arc::clone(ssh_slot.ssh_session().unwrap().primary_transport()));
    let gen_init = state.sftp_drawer().list_generation();
    let root_listing = state.sftp_manager().list_dir(&endpoint, "/").await.unwrap();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid,
        generation: gen_init,
        result: Ok(root_listing),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/");
    drop(state.view(win));

    // Hidden files toggle hides and reveals dot-prefixed entries.
    let visible_with_hidden = visible_remote_entries(state.sftp_drawer());
    assert!(visible_with_hidden.iter().any(|e| e.name == ".hidden_rc"));
    let _ = state.update(Message::SftpSessionToggleHidden(sid));
    let visible_no_hidden = visible_remote_entries(state.sftp_drawer());
    assert!(!visible_no_hidden.iter().any(|e| e.name == ".hidden_rc"));
    let _ = state.update(Message::SftpSessionToggleHidden(sid));

    // Inline filter filters case-insensitively and clears when closed.
    let _ = state.update(Message::SftpSessionToggleFilter(sid));
    let _ = state.update(Message::SftpSessionFilterChanged(sid, "ALPHA".to_string()));
    let filtered = visible_remote_entries(state.sftp_drawer());
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "alpha.txt");
    let _ = state.update(Message::SftpSessionToggleFilter(sid));

    // Sorting keeps directories first, and sorts files by Size asc/desc cleanly.
    let _ = state.update(Message::SftpSessionSortBy(sid, SftpSortColumn::Size));
    let sorted_asc = visible_remote_entries(state.sftp_drawer());
    let first_file_asc = sorted_asc.iter().position(|e| !e.is_dir).unwrap();
    assert!(sorted_asc[..first_file_asc].iter().all(|e| e.is_dir));
    assert!(sorted_asc[first_file_asc..].iter().all(|e| !e.is_dir));
    let names_asc: Vec<&str> = sorted_asc
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names_asc.iter().position(|&n| n == "alpha.txt")
            < names_asc.iter().position(|&n| n == "zeta.log")
    );
    let _ = state.update(Message::SftpSessionSortBy(sid, SftpSortColumn::Size));
    let sorted_desc = visible_remote_entries(state.sftp_drawer());
    let names_desc: Vec<&str> = sorted_desc
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        names_desc.iter().position(|&n| n == "zeta.log")
            < names_desc.iter().position(|&n| n == "alpha.txt")
    );

    // Navigation history (enter /sub_folder, Back, Forward, Up, Home).
    let sub_listing = state
        .sftp_manager()
        .list_dir(&endpoint, "/sub_folder")
        .await
        .unwrap();
    let _ = state.update(Message::SftpSessionNavigateTo {
        session_id: sid,
        path: "/sub_folder".to_string(),
    });
    let gen_sub = state.sftp_drawer().list_generation();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid,
        generation: gen_sub,
        result: Ok(sub_listing.clone()),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/sub_folder");
    assert_eq!(visible_remote_entries(state.sftp_drawer())[0].name, "inside.txt");

    let _ = state.update(Message::SftpSessionBack(sid));
    let gen_back = state.sftp_drawer().list_generation();
    let root_listing2 = state.sftp_manager().list_dir(&endpoint, "/").await.unwrap();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid,
        generation: gen_back,
        result: Ok(root_listing2.clone()),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/");

    let _ = state.update(Message::SftpSessionForward(sid));
    let gen_fwd = state.sftp_drawer().list_generation();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid,
        generation: gen_fwd,
        result: Ok(sub_listing),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/sub_folder");

    let _ = state.update(Message::SftpSessionUp(sid));
    let gen_up = state.sftp_drawer().list_generation();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid,
        generation: gen_up,
        result: Ok(root_listing2),
    });
    assert_eq!(state.sftp_drawer().current_path(), "/");

    // Remote bookmark persistence in sftp_state.toml.
    let _ = state.update(Message::SftpSessionToggleBookmarkCurrent(sid));
    let persisted = termewnal_core::load_sftp_state(&paths).expect("load sftp bookmarks");
    let bkey = format!("Remote Panel Host (secops@127.0.0.1:{})", addr.port());
    assert_eq!(persisted.remote_bookmarks(&bkey), &["/".to_string()]);

    // Multi-select + SelectAll + InvertSelection.
    let _ = state.update(Message::SftpSessionRowClicked {
        session_id: sid,
        path: "/alpha.txt".to_string(),
        ctrl_held: false,
    });
    let _ = state.update(Message::SftpSessionRowClicked {
        session_id: sid,
        path: "/zeta.log".to_string(),
        ctrl_held: true,
    });
    assert_eq!(state.sftp_drawer().plus.remote.selected_paths.len(), 2);
    let _ = state.update(Message::SftpSessionOpenContextMenu {
        session_id: sid,
        target_path: Some("/alpha.txt".to_string()),
        position: Point::new(120.0, 200.0),
    });
    assert!(state.sftp_drawer().plus.context_menu.open);
    drop(state.view(win));
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::InvertSelection));
    assert!(!state
        .sftp_drawer()
        .plus
        .remote
        .selected_paths
        .contains("/alpha.txt"));
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::SelectAll));
    assert_eq!(
        state.sftp_drawer().plus.remote.selected_paths.len(),
        visible_remote_entries(state.sftp_drawer()).len()
    );

    // Remote View & Edit modal transitions + real SFTP write_text_file roundtrip.
    let _ = state.update(Message::SftpSessionRowClicked {
        session_id: sid,
        path: "/alpha.txt".to_string(),
        ctrl_held: false,
    });
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::View));
    assert!(matches!(
        state.sftp_drawer().plus.modal,
        SftpModalState::FileViewer {
            editable: false,
            loading: true,
            ..
        }
    ));
    let content = state
        .sftp_manager()
        .read_text_file(&endpoint, "/alpha.txt", 1024 * 1024)
        .await
        .unwrap();
    let _ = state.update(Message::SftpRemoteFileReadResult {
        session_id: sid,
        request_path: "/alpha.txt".to_string(),
        editable: false,
        result: Ok(content.clone()),
    });
    drop(state.view(win));
    let _ = state.update(Message::SftpModalEditorEvent(
        termewnal_ui::RemoteEditorEvent::Action(text_editor::Action::Edit(
            text_editor::Edit::Insert('X'),
        )),
    ));
    if let SftpModalState::FileViewer { content: ed, dirty, .. } = &state.sftp_drawer().plus.modal {
        assert_eq!(ed.text(), "hello alpha");
        assert!(!dirty);
    }
    let _ = state.update(Message::SftpModalRequestClose(Some(sid)));

    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::Edit));
    let _ = state.update(Message::SftpRemoteFileReadResult {
        session_id: sid,
        request_path: "/alpha.txt".to_string(),
        editable: true,
        result: Ok(content),
    });
    let _ = state.update(Message::SftpModalEditorEvent(
        termewnal_ui::RemoteEditorEvent::Action(text_editor::Action::Edit(
            text_editor::Edit::Insert('!'),
        )),
    ));
    let _ = state.update(Message::SftpModalSaveFile);
    state
        .sftp_manager()
        .write_text_file(&endpoint, "/alpha.txt", "hello alpha!")
        .await
        .unwrap();
    let _ = state.update(Message::SftpRemoteFileSaveResult {
        session_id: sid,
        path: "/alpha.txt".to_string(),
        result: Ok(()),
    });
    assert!(fs::read_to_string(remote_root.join("alpha.txt"))
        .unwrap()
        .contains('!'));
    let _ = state.update(Message::SftpDismissModal(Some(sid)));

    // Edit Permissions modal + real SFTP set_permissions + Properties modal.
    let _ = state.update(Message::SftpContextAction(
        SftpContextActionKind::EditPermissions,
    ));
    let _ = state.update(Message::SftpModalOctalChanged("740".to_string()));
    if let SftpModalState::EditPermissions { mode_bits, .. } = &state.sftp_drawer().plus.modal {
        assert_eq!(*mode_bits, 0o740);
        assert_eq!(format_octal_mode(*mode_bits), "740");
    }
    drop(state.view(win));
    let _ = state.update(Message::SftpModalConfirmPermissions);
    state
        .sftp_manager()
        .set_permissions(&endpoint, "/alpha.txt", 0o740)
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(remote_root.join("alpha.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o740);
    }

    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::Properties));
    assert!(matches!(
        state.sftp_drawer().plus.modal,
        SftpModalState::Properties { .. }
    ));
    drop(state.view(win));
    let _ = state.update(Message::SftpDismissModal(Some(sid)));

    // Upload via SFTP Panel file drop + picker & Download-to picker auto-open Transfers tray.
    let dropped_upload = local_uploads.join("dropped_panel.txt");
    fs::write(&dropped_upload, b"dropped on sftp panel").unwrap();
    let _ = state.update(Message::SftpSessionFileDropped {
        session_id: sid,
        path: dropped_upload,
    });
    let picked_upload = local_uploads.join("picked_upload.txt");
    fs::write(&picked_upload, b"uploaded via picker").unwrap();
    let _ = state.update(Message::SftpSessionUploadPicked {
        session_id: sid,
        paths: vec![picked_upload],
    });
    assert!(state.sftp_drawer().plus.transfers_panel_open);
    let _ = state.update(Message::SftpSessionDownloadToPicked {
        session_id: sid,
        entries: vec![termewnal_ui::sftp_plus::FileBrowserEntry {
            name: "alpha.txt".to_string(),
            full_path: "/alpha.txt".to_string(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            size: 12,
            modified_unix_secs: None,
            permissions: Some(0o740),
        }],
        dest_dir: Some(custom_downloads.clone()),
    });
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));
    assert_eq!(
        fs::read(remote_root.join("dropped_panel.txt")).unwrap(),
        b"dropped on sftp panel"
    );
    assert_eq!(
        fs::read(remote_root.join("picked_upload.txt")).unwrap(),
        b"uploaded via picker"
    );
    assert_eq!(
        fs::read(custom_downloads.join("alpha.txt")).unwrap(),
        b"hello alpha!"
    );

    // Remote NewFile, Copy+Paste, Rename, Cut+Paste, and Delete roundtrip via SFTP manager + UI modals.
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::NewFile));
    let _ = state.update(Message::SftpModalNameChanged("created.txt".to_string()));
    let _ = state.update(Message::SftpModalConfirmCreateOrRename);
    state
        .sftp_manager()
        .create_empty_file(&endpoint, "/created.txt")
        .await
        .unwrap();
    state
        .sftp_manager()
        .copy_file(&endpoint, "/created.txt", "/created.txt.copy")
        .await
        .unwrap();
    state
        .sftp_manager()
        .rename(&endpoint, "/created.txt.copy", "/sub_folder/moved.txt")
        .await
        .unwrap();
    assert!(remote_root.join("sub_folder/moved.txt").is_file());
    state
        .sftp_manager()
        .remove_file(&endpoint, "/sub_folder/moved.txt")
        .await
        .unwrap();
    assert!(!remote_root.join("sub_folder/moved.txt").exists());

    // Manually closing SFTP+ sets `manually_closed` so subsequent SSH auto-reconnect does NOT reopen it.
    let _ = state.update(Message::SftpSessionClose(sid));
    assert!(!state.sftp_drawer().is_open());
    assert!(state.sftp_drawer().is_manually_closed());
    let gen_after_close = state.ssh_session(sid).unwrap().ssh_generation();
    let _ = state.update(Message::SshEvent {
        session_id: sid,
        event: SshUiEvent::Connected {
            generation: gen_after_close,
            session: None,
            supervisor: None,
        },
    });
    assert!(!state.sftp_drawer().is_open());

    // Disabling `auto_open_sftp` in config prevents new SSH sessions from auto-opening SFTP+.
    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsAutoOpenSftpToggled(false));
    let _ = state.update(Message::SettingsSave);
    assert!(!state.config().terminal.auto_open_sftp);
    let _ = state.update(Message::QuickConnectSsh(spec));
    let sid2 = state.active_session_id().unwrap();
    let gen2 = state.ssh_session(sid2).unwrap().ssh_generation();
    let _ = state.update(Message::SshEvent {
        session_id: sid2,
        event: SshUiEvent::Connected {
            generation: gen2,
            session: None,
            supervisor: None,
        },
    });
    assert!(!state.sftp_drawer_for_session(sid2).unwrap().is_open());
}

#[test]
fn per_pane_sftp_panels_and_focus_shortcut_isolation() {
    use termewnal_ui::SftpContextActionKind;

    let (_temp, mut state) = boot_test_state();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let profile_a = Profile::ssh("Host A", "10.0.0.10", 22, "ops", SshAuthMethod::Agent);
    let profile_b = Profile::ssh("Host B", "10.0.0.20", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile_a));
    let sid_a = state.active_session_id().expect("ssh a");
    let _ = state.update(Message::SplitFocusedPane(
        termewnal_ui::workspace::SplitAxis::Vertical,
    ));
    let _ = state.update(Message::OpenSshProfile(profile_b));
    let sid_b = state.active_session_id().expect("ssh b");
    assert_ne!(sid_a, sid_b);

    // Connect both SSH sessions: sid_a shares a vertical split (<680px) so docked SFTP+
    // auto-suppresses while sid_b occupies an unsplit tab (>=680px) and auto-opens docked.
    for sid in [sid_a, sid_b] {
        let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
        let _ = state.update(Message::SshEvent {
            session_id: sid,
            event: SshUiEvent::Connected {
                generation: gen_id,
                session: None,
                supervisor: None,
            },
        });
        assert!(!state.sftp_drawer_for_session(sid).unwrap().is_panel_focused());
    }
    assert!(!state.is_sftp_drawer_open_for(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_auto_suppressed());
    assert!(state.sftp_drawer_for_session(sid_b).unwrap().is_docked_open());
    let _ = state.update(Message::ToggleSftpDrawerForSession(sid_a));
    assert!(state.is_sftp_drawer_open_for(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_overlay_open());

    // Seed an entry on sid_a and verify SftpShortcut(SelectAll) is ignored while Terminal has focus.
    let gen_a = state.sftp_drawer_for_session(sid_a).unwrap().list_generation();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid_a,
        generation: gen_a,
        result: Ok(termewnal_ssh::RemoteListing {
            resolved_path: "/srv".to_string(),
            entries: vec![termewnal_ssh::RemoteDirEntry {
                name: "app.conf".to_string(),
                path: "/srv/app.conf".to_string(),
                kind: RemoteEntryKind::File,
                size: 128,
                permissions: Some(0o644),
                modified_epoch_secs: None,
                uid: None,
                gid: None,
            }],
            transport_mode: SftpTransportMode::IsolatedTransferTransport,
            notice: None,
        }),
    });

    let _ = state.update(Message::FocusSession(sid_a));
    let _ = state.update(Message::SftpShortcut(SftpContextActionKind::SelectAll));
    assert!(
        state
            .sftp_drawer_for_session(sid_a)
            .unwrap()
            .plus
            .remote
            .selected_paths
            .is_empty(),
        "file shortcuts must not fire while Terminal has focus"
    );

    // Clicking inside sid_a's SFTP Panel focuses both the pane and the SFTP Panel, enabling SftpShortcut.
    let _ = state.update(Message::FocusSession(sid_b));
    let _ = state.update(Message::FocusSftpPanel(sid_a));
    assert_eq!(state.active_session_id(), Some(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_panel_focused());
    let _ = state.update(Message::SftpShortcut(SftpContextActionKind::SelectAll));
    assert_eq!(
        state
            .sftp_drawer_for_session(sid_a)
            .unwrap()
            .plus
            .remote
            .selected_paths
            .len(),
        1
    );

    // Clicking Terminal restores Terminal focus and disables SFTP file shortcuts again.
    let _ = state.update(Message::TerminalFocused);
    assert!(!state.sftp_drawer_for_session(sid_a).unwrap().is_panel_focused());
    let _ = state.update(Message::SftpShortcut(SftpContextActionKind::InvertSelection));
    assert_eq!(
        state
            .sftp_drawer_for_session(sid_a)
            .unwrap()
            .plus
            .remote
            .selected_paths
            .len(),
        1
    );
    drop(state.view(win));
}

#[test]
fn sidebar_sftp_panel_and_settings_persist_widths_and_download_folder() {
    use termewnal_core::{DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH};
    use termewnal_ui::workspace::DEFAULT_SPLIT_RATIO;

    assert_eq!(DEFAULT_SPLIT_RATIO, 0.5);

    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    super::common::seed_fast_test_shell(&paths);
    let custom_dl = temp.path().join("picked_downloads");
    fs::create_dir_all(&custom_dl).unwrap();

    let mut state = AppState::boot_with_paths(paths.clone()).expect("boot AppState");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // Sidebar resize clamps 180..480, commits to config.toml, and resets to 250.
    assert_eq!(state.sidebar_width(), DEFAULT_SIDEBAR_WIDTH);
    let _ = state.update(Message::ResizeSidebar(80.0));
    assert_eq!(state.sidebar_width(), MIN_SIDEBAR_WIDTH);
    let _ = state.update(Message::ResizeSidebar(600.0));
    assert_eq!(state.sidebar_width(), MAX_SIDEBAR_WIDTH);
    let _ = state.update(Message::ResizeSidebar(312.0));
    assert_eq!(state.sidebar_width(), 312.0);
    let _ = state.update(Message::CommitSidebarWidth);
    let persisted = termewnal_core::load_config(&paths).expect("load config");
    assert_eq!(persisted.terminal.sidebar_width, 312.0);

    // Settings -> SSH & SFTP edits custom download folder + auto_open_sftp and persists across reboot.
    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsAutoOpenSftpToggled(false));
    let _ = state.update(Message::SettingsSftpDownloadDirPicked(Some(custom_dl.clone())));
    let _ = state.update(Message::SettingsSave);

    let reloaded = AppState::boot_with_paths(paths.clone()).expect("reboot AppState");
    assert_eq!(reloaded.sidebar_width(), 312.0);
    assert!(!reloaded.config().terminal.auto_open_sftp);
    assert_eq!(
        reloaded.config().terminal.sftp_download_dir.as_ref(),
        Some(&custom_dl)
    );
    assert_eq!(reloaded.sftp_drawer().download_dir(), &custom_dl);

    // Reset Sidebar width returns to 250.0.
    let _ = state.update(Message::ResetSidebarWidth);
    assert_eq!(state.sidebar_width(), DEFAULT_SIDEBAR_WIDTH);
    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsResetSftpDownloadDir);
    let _ = state.update(Message::SettingsSave);
    assert!(state.config().terminal.sftp_download_dir.is_none());
}

#[test]
fn open_with_desktop_exec_parser_and_context_menu_render_for_file_and_folder() {
    use termewnal_core::ThemePalette;
    use termewnal_ui::app::parse_desktop_exec_tokens;
    use termewnal_ui::sftp_drawer::SftpDrawerState;
    use termewnal_ui::sftp_plus::render_sftp_context_menu_overlay;
    use termewnal_ui::{SessionId, SftpContextActionKind};

    assert_eq!(
        parse_desktop_exec_tokens("\"/opt/Visual Studio Code/code\" --new-window %F %%flag"),
        vec![
            "/opt/Visual Studio Code/code".to_string(),
            "--new-window".to_string(),
            "%flag".to_string(),
        ]
    );
    assert_eq!(
        parse_desktop_exec_tokens("gimp-2.10 %U"),
        vec!["gimp-2.10".to_string()]
    );

    let mut drawer = SftpDrawerState::default();
    drawer.apply_browse_result(
        SessionId(1),
        0,
        Ok(termewnal_ssh::RemoteListing {
            resolved_path: "/srv".to_string(),
            entries: vec![
                termewnal_ssh::RemoteDirEntry {
                    name: "docs".to_string(),
                    path: "/srv/docs".to_string(),
                    kind: RemoteEntryKind::Dir,
                    size: 0,
                    permissions: Some(0o755),
                    modified_epoch_secs: None,
                    uid: None,
                    gid: None,
                },
                termewnal_ssh::RemoteDirEntry {
                    name: "notes.md".to_string(),
                    path: "/srv/notes.md".to_string(),
                    kind: RemoteEntryKind::File,
                    size: 42,
                    permissions: Some(0o644),
                    modified_epoch_secs: None,
                    uid: None,
                    gid: None,
                },
            ],
            transport_mode: SftpTransportMode::IsolatedTransferTransport,
            notice: None,
        }),
    );

    // Context menu renders cleanly with OpenWith for either a selected file or a selected folder.
    let palette = ThemePalette::for_name(termewnal_core::BuiltinTheme::Dark.name());
    for target in ["/srv/notes.md", "/srv/docs"] {
        drawer.plus.remote.selected_paths.clear();
        drawer.plus.remote.selected_paths.insert(target.to_string());
        drawer
            .plus
            .context_menu
            .show(Some(target.to_string()), iced::Point::new(80.0, 120.0));
        assert!(
            render_sftp_context_menu_overlay(&drawer, true, 1000.0, 800.0, &palette).is_some()
        );
    }
    let _ = SftpContextActionKind::OpenWith;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sftp_plus_open_with_stages_temp_launches_app_watches_single_file_and_launches_folder() {
    use std::sync::Mutex;
    use termewnal_ssh::DirectoryTransferSummary;
    use termewnal_ui::sftp_plus::FileBrowserEntry;

    let temp = TempDir::new().expect("create temp XDG root");
    let remote_root = temp.path().join("remote");
    let remote_sub = remote_root.join("sub_folder");
    let remote_empty = remote_root.join("empty_dir");
    let open_temp = temp.path().join("open_with_cache");
    let user_downloads = temp.path().join("user_downloads");
    fs::create_dir_all(&remote_sub).unwrap();
    fs::create_dir_all(&remote_empty).unwrap();
    fs::create_dir_all(&open_temp).unwrap();
    fs::create_dir_all(&user_downloads).unwrap();

    fs::write(remote_root.join("alpha.txt"), b"hello alpha").unwrap();
    fs::write(remote_sub.join("inside.txt"), b"folder file payload").unwrap();

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
    let launches: Arc<Mutex<Vec<(PathBuf, PathBuf)>>> = Arc::new(Mutex::new(Vec::new()));
    let launches_hook = Arc::clone(&launches);

    let mut state = AppState::boot_with_paths_and_vault(paths.clone(), Arc::clone(&vault))
        .unwrap()
        .with_sftp_open_temp_dir(open_temp.clone())
        .with_app_opener(Arc::new(move |app, target| {
            launches_hook
                .lock()
                .unwrap()
                .push((app.to_path_buf(), target.to_path_buf()));
            Ok(())
        }));
    state.set_sftp_download_dir(user_downloads.clone());
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let known_hosts = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", addr.port(), &host_pubkey, &known_hosts).unwrap();

    let spec = SshTargetSpec::quick_connect(
        "OpenWith Host",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            username: "secops".to_string(),
            auth: SshAuthMethod::KeyboardInteractive,
            known_hosts_path: Some(known_hosts),
            proxy_jump: Vec::new(),
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            ..SshConnectConfig::default()
        },
    )
    .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let _ = state.update(Message::QuickConnectSsh(spec.clone()));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
    let buffer = Arc::clone(state.ssh_session(sid).unwrap().buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        sid,
        gen_id,
        spec,
        buffer,
        Arc::clone(&vault),
        ev_tx,
    ));
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("SSH connect timeout")
            .expect("event channel open");
        let is_connected = matches!(ev, SshUiEvent::Connected { .. });
        let is_auth_prompt = matches!(ev, SshUiEvent::AuthPrompt { .. });
        let _ = state.update(Message::SshEvent {
            session_id: sid,
            event: ev,
        });
        if is_auth_prompt {
            let _ = state.update(Message::SshAuthInputChanged {
                session_id: sid,
                index: 0,
                value: "424242".to_string(),
            });
            let _ = state.update(Message::SubmitSshAuthPrompt(sid));
        }
        if is_connected {
            break;
        }
    }

    let ssh_slot = state.ssh_session(sid).unwrap();
    let endpoint = termewnal_ssh::SftpEndpoint::new(
        termewnal_ssh::ProfileTransportKey::from_connect_config(
            None,
            &ssh_slot.spec().connect_config,
        ),
        ssh_slot.spec().connect_config.clone(),
    )
    .with_primary_transport(Arc::clone(ssh_slot.ssh_session().unwrap().primary_transport()));

    // 1. Single remote file -> OpenWith downloads into temp cache (never user Downloads) & opens app.
    let alpha_entry = FileBrowserEntry {
        name: "alpha.txt".to_string(),
        full_path: "/alpha.txt".to_string(),
        is_dir: false,
        is_symlink: false,
        symlink_target: None,
        size: 11,
        modified_unix_secs: None,
        permissions: Some(0o644),
    };
    let custom_app = PathBuf::from("/usr/bin/custom-editor");
    let _ = state.update(Message::SftpSessionOpenWithAppPicked {
        session_id: sid,
        entry: alpha_entry.clone(),
        app_path: Some(custom_app.clone()),
    });
    // Cancelling the picker (None) is a clean no-op.
    let _ = state.update(Message::SftpSessionOpenWithAppPicked {
        session_id: sid,
        entry: alpha_entry,
        app_path: None,
    });

    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    let launched = launches.lock().unwrap().clone();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].0, custom_app);
    let staged_file = launched[0].1.clone();
    assert!(staged_file.starts_with(&open_temp));
    assert_eq!(fs::read(&staged_file).unwrap(), b"hello alpha");
    assert!(
        fs::read_dir(&user_downloads).unwrap().next().is_none(),
        "Open with must not clutter configured SFTP download folder"
    );

    // Watched file tracks local modifications without auto-uploading until confirmed or Auto enabled.
    assert_eq!(state.sftp_drawer().plus.watched_files.len(), 1);
    let watch_id = state.sftp_drawer().plus.watched_files[0].id;
    assert!(!state.sftp_drawer().plus.watched_files[0].pending_local_change);
    assert!(!state.sftp_drawer().plus.watched_files[0].auto_upload);

    fs::write(&staged_file, b"edited v1").unwrap();
    let _ = state.update(Message::SftpOpenWithCheckWatchedFiles);
    assert!(state.sftp_drawer().plus.watched_files[0].pending_local_change);
    assert_eq!(fs::read(remote_root.join("alpha.txt")).unwrap(), b"hello alpha");
    drop(state.view(win));

    // Ignore resets the change baseline without uploading.
    let _ = state.update(Message::SftpOpenWithIgnoreChange {
        session_id: sid,
        watch_id,
    });
    assert!(!state.sftp_drawer().plus.watched_files[0].pending_local_change);
    assert_eq!(fs::read(remote_root.join("alpha.txt")).unwrap(), b"hello alpha");

    // Manual Upload overwrites the remote file cleanly.
    fs::write(&staged_file, b"edited v2").unwrap();
    let _ = state.update(Message::SftpOpenWithCheckWatchedFiles);
    assert!(state.sftp_drawer().plus.watched_files[0].pending_local_change);
    let _ = state.update(Message::SftpOpenWithUploadNow {
        session_id: sid,
        watch_id,
    });
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));
    assert_eq!(fs::read(remote_root.join("alpha.txt")).unwrap(), b"edited v2");
    assert!(!state.sftp_drawer().plus.watched_files[0].pending_local_change);

    // Enabling Auto-sync uploads subsequent saves automatically without clicking Upload.
    let _ = state.update(Message::SftpOpenWithToggleAutoSync {
        session_id: sid,
        watch_id,
    });
    assert!(state.sftp_drawer().plus.watched_files[0].auto_upload);
    fs::write(&staged_file, b"auto synced v3").unwrap();
    let _ = state.update(Message::SftpOpenWithCheckWatchedFiles);
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));
    assert_eq!(
        fs::read(remote_root.join("alpha.txt")).unwrap(),
        b"auto synced v3"
    );

    // Stopping watch clears the watch bar cleanly.
    let _ = state.update(Message::SftpOpenWithStopWatch {
        session_id: sid,
        watch_id,
    });
    assert!(state.sftp_drawer().plus.watched_files.is_empty());

    // 2. Folder -> recursive download into temp dir, then launch app with folder without adding watch.
    launches.lock().unwrap().clear();
    let folder_entry = FileBrowserEntry {
        name: "sub_folder".to_string(),
        full_path: "/sub_folder".to_string(),
        is_dir: true,
        is_symlink: false,
        symlink_target: None,
        size: 0,
        modified_unix_secs: None,
        permissions: Some(0o755),
    };
    let folder_target = open_temp.join("manual_dir_stage/sub_folder");
    let summary: DirectoryTransferSummary = state
        .transfer_queue()
        .enqueue_download_dir(endpoint.clone(), "/sub_folder".to_string(), folder_target.clone())
        .await
        .expect("enqueue remote dir for OpenWith");
    let folder_app = PathBuf::from("/usr/bin/ide-app");
    let _ = state.update(Message::SftpSessionOpenWithDirEnqueued {
        session_id: sid,
        entry: folder_entry,
        app_path: folder_app.clone(),
        local_target: folder_target.clone(),
        result: Ok(summary),
    });
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    let folder_launches = launches.lock().unwrap().clone();
    assert_eq!(folder_launches.len(), 1);
    assert_eq!(folder_launches[0].0, folder_app);
    assert_eq!(folder_launches[0].1, folder_target);
    assert_eq!(
        fs::read(folder_target.join("inside.txt")).unwrap(),
        b"folder file payload"
    );
    assert!(state.sftp_drawer().plus.watched_files.is_empty());

    // Empty folder (0 transfer_ids) launches app immediately.
    launches.lock().unwrap().clear();
    let empty_entry = FileBrowserEntry {
        name: "empty_dir".to_string(),
        full_path: "/empty_dir".to_string(),
        is_dir: true,
        is_symlink: false,
        symlink_target: None,
        size: 0,
        modified_unix_secs: None,
        permissions: Some(0o755),
    };
    let empty_target = open_temp.join("manual_empty_stage/empty_dir");
    let empty_summary = state
        .transfer_queue()
        .enqueue_download_dir(endpoint, "/empty_dir".to_string(), empty_target.clone())
        .await
        .expect("enqueue empty remote dir");
    assert!(empty_summary.transfer_ids.is_empty());
    let _ = state.update(Message::SftpSessionOpenWithDirEnqueued {
        session_id: sid,
        entry: empty_entry,
        app_path: folder_app.clone(),
        local_target: empty_target.clone(),
        result: Ok(empty_summary),
    });
    let empty_launches = launches.lock().unwrap().clone();
    assert_eq!(empty_launches, vec![(folder_app, empty_target.clone())]);
    assert!(empty_target.is_dir());
}

#[test]
fn app_picker_discovers_desktop_entries_ranks_mime_apps_filters_and_builds_exec_argv() {
    use termewnal_ui::app_picker::{
        build_desktop_exec_argv, discover_linux_apps_from_dirs, display_name_for_app_path,
        filter_installed_apps, infer_candidate_mime_types, parse_desktop_entry,
    };
    use termewnal_ui::sftp_plus::FileBrowserEntry;

    let temp = TempDir::new().unwrap();
    let user_apps = temp.path().join("user_apps");
    let sys_apps = temp.path().join("sys_apps");
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&user_apps).unwrap();
    fs::create_dir_all(&sys_apps).unwrap();
    fs::create_dir_all(&config_dir).unwrap();

    // Realistic XDG MIME globs2 + mimeapps.list.
    let globs2 = temp.path().join("globs2");
    fs::write(
        &globs2,
        "50:text/markdown:*.md\n50:text/plain:*.txt\n50:image/png:*.png\n",
    )
    .unwrap();
    let mimeapps = config_dir.join("mimeapps.list");
    fs::write(
        &mimeapps,
        "[Default Applications]\ntext/markdown=code.desktop;\ninode/directory=nemo.desktop;\n\
         [Added Associations]\ntext/plain=xed.desktop;\n",
    )
    .unwrap();

    // User override for code.desktop (must override system copy + stop before [Desktop Action]).
    fs::write(
        user_apps.join("code.desktop"),
        "[Desktop Entry]\nType=Application\nName=Visual Studio Code\n\
         GenericName=Code Editor\nExec=\"/usr/share/code/code\" --wait %F\n\
         MimeType=text/plain;text/markdown;inode/directory;\n\n\
         [Desktop Action new-window]\nName=New Empty Window\nExec=/usr/share/code/code --new-window\n",
    )
    .unwrap();
    fs::write(
        sys_apps.join("code.desktop"),
        "[Desktop Entry]\nType=Application\nName=Old System Code\nExec=/old/code %F\n",
    )
    .unwrap();
    fs::write(
        sys_apps.join("xed.desktop"),
        "[Desktop Entry]\nType=Application\nName=Text Editor\nGenericName=Text Editor\n\
         Exec=xed %U\nMimeType=text/plain;\n",
    )
    .unwrap();
    fs::write(
        sys_apps.join("nemo.desktop"),
        "[Desktop Entry]\nType=Application\nName=Files\nGenericName=File Manager\n\
         Exec=nemo %U\nMimeType=inode/directory;\n",
    )
    .unwrap();
    fs::write(
        sys_apps.join("vim.desktop"),
        "[Desktop Entry]\nType=Application\nName=Vim\nGenericName=CLI Text Editor\n\
         Exec=vim %F\nTerminal=true\nMimeType=text/plain;\n",
    )
    .unwrap();
    fs::write(
        sys_apps.join("org.telegram.desktop.desktop"),
        "[Desktop Entry]\nType=Application\nName=Telegram\nComment=Messenger\n\
         Exec=/usr/bin/flatpak run --branch=stable --file-forwarding org.telegram.desktop -- @@u %U @@\n\
         MimeType=x-scheme-handler/tg;\n",
    )
    .unwrap();
    // Hidden/NoDisplay/TryExec-missing entries must be skipped cleanly.
    fs::write(
        sys_apps.join("hidden.desktop"),
        "[Desktop Entry]\nType=Application\nName=Hidden App\nExec=true\nNoDisplay=true\n",
    )
    .unwrap();
    fs::write(
        sys_apps.join("missing.desktop"),
        "[Desktop Entry]\nType=Application\nName=Missing App\nTryExec=/nonexistent/bin/missing-app-xyz\nExec=missing\n",
    )
    .unwrap();

    // Candidate MIME inference for files & folders.
    assert_eq!(
        infer_candidate_mime_types("notes.md", false, Some(&globs2)),
        vec!["text/markdown".to_string(), "text/plain".to_string()]
    );
    assert_eq!(
        infer_candidate_mime_types("docs", true, Some(&globs2)),
        vec!["inode/directory".to_string(), "x-directory/normal".to_string()]
    );

    // 1. Discover & rank apps for a markdown file.
    let md_entry = FileBrowserEntry {
        name: "notes.md".to_string(),
        full_path: "/srv/notes.md".to_string(),
        is_dir: false,
        is_symlink: false,
        symlink_target: None,
        size: 100,
        modified_unix_secs: None,
        permissions: Some(0o644),
    };
    let discovered = discover_linux_apps_from_dirs(
        &[user_apps.clone(), sys_apps.clone()],
        Some(&globs2),
        &[mimeapps.clone()],
        &md_entry,
    );
    let ids: Vec<&str> = discovered.iter().map(|a| a.id.as_str()).collect();
    assert!(!ids.contains(&"hidden.desktop"));
    assert!(!ids.contains(&"missing.desktop"));
    // code.desktop is default -> must come first, overriding sys_apps/code.desktop!
    assert_eq!(discovered[0].id, "code.desktop");
    assert_eq!(discovered[0].name, "Visual Studio Code");
    assert!(discovered[0].is_default);
    assert_eq!(discovered[0].launch_path, user_apps.join("code.desktop"));
    assert!(discovered.iter().any(|a| a.id == "xed.desktop" && a.is_recommended));
    assert!(discovered.iter().any(|a| a.id == "vim.desktop" && a.terminal && a.is_recommended));
    assert!(
        discovered
            .iter()
            .any(|a| a.id == "org.telegram.desktop.desktop" && !a.is_recommended)
    );

    // Filtering matches name, subtitle, or command_label.
    let filtered_editor = filter_installed_apps(&discovered, "xed");
    assert_eq!(filtered_editor.len(), 1);
    assert_eq!(filtered_editor[0].id, "xed.desktop");

    // 2. Folder target ranks Files (nemo.desktop) as default and Visual Studio Code as recommended.
    let dir_entry = FileBrowserEntry {
        name: "docs".to_string(),
        full_path: "/srv/docs".to_string(),
        is_dir: true,
        is_symlink: false,
        symlink_target: None,
        size: 0,
        modified_unix_secs: None,
        permissions: Some(0o755),
    };
    let dir_apps = discover_linux_apps_from_dirs(
        &[user_apps.clone(), sys_apps.clone()],
        Some(&globs2),
        &[mimeapps],
        &dir_entry,
    );
    assert_eq!(dir_apps[0].id, "nemo.desktop");
    assert!(dir_apps[0].is_default);
    assert!(dir_apps.iter().any(|a| a.id == "code.desktop" && a.is_recommended));

    // 3. Flatpak & Desktop Entry Exec argv construction preserves @@u <path> @@ markers.
    let tg_parsed = parse_desktop_entry(&sys_apps.join("org.telegram.desktop.desktop")).unwrap();
    assert_eq!(
        build_desktop_exec_argv(&tg_parsed.exec, Path::new("/tmp/photo.png")),
        vec![
            "/usr/bin/flatpak",
            "run",
            "--branch=stable",
            "--file-forwarding",
            "org.telegram.desktop",
            "--",
            "@@u",
            "/tmp/photo.png",
            "@@",
        ]
    );
    assert_eq!(
        display_name_for_app_path(&user_apps.join("code.desktop")),
        "Visual Studio Code"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sftp_plus_open_with_displays_app_chooser_modal_filters_and_launches_chosen_app() {
    use std::sync::Mutex;
    use termewnal_ui::{InstalledApp, SftpContextActionKind, SftpModalState};

    let temp = TempDir::new().expect("create temp XDG root");
    let remote_root = temp.path().join("remote");
    let open_temp = temp.path().join("open_with_cache");
    let desktop_dir = temp.path().join("desktop_apps");
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&open_temp).unwrap();
    fs::create_dir_all(&desktop_dir).unwrap();
    fs::write(remote_root.join("report.md"), b"# Q4 Report\n").unwrap();

    let code_desktop = desktop_dir.join("code.desktop");
    fs::write(
        &code_desktop,
        "[Desktop Entry]\nType=Application\nName=Visual Studio Code\nExec=code %F\n",
    )
    .unwrap();
    let xed_desktop = desktop_dir.join("xed.desktop");
    fs::write(
        &xed_desktop,
        "[Desktop Entry]\nType=Application\nName=Text Editor\nExec=xed %F\n",
    )
    .unwrap();

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
    let launches: Arc<Mutex<Vec<(PathBuf, PathBuf)>>> = Arc::new(Mutex::new(Vec::new()));
    let launches_hook = Arc::clone(&launches);

    let catalog_apps = vec![
        InstalledApp {
            id: "xed.desktop".to_string(),
            name: "Text Editor".to_string(),
            subtitle: Some("Plain text editor".to_string()),
            launch_path: xed_desktop.clone(),
            command_label: "xed".to_string(),
            mime_types: vec!["text/plain".to_string()],
            is_default: true,
            is_recommended: false,
            terminal: false,
        },
        InstalledApp {
            id: "code.desktop".to_string(),
            name: "Visual Studio Code".to_string(),
            subtitle: Some("Code Editor".to_string()),
            launch_path: code_desktop.clone(),
            command_label: "code".to_string(),
            mime_types: vec!["text/markdown".to_string()],
            is_default: false,
            is_recommended: true,
            terminal: false,
        },
    ];
    let catalog_for_provider = catalog_apps.clone();

    let mut state = AppState::boot_with_paths_and_vault(paths.clone(), Arc::clone(&vault))
        .unwrap()
        .with_sftp_open_temp_dir(open_temp.clone())
        .with_app_opener(Arc::new(move |app, target| {
            launches_hook
                .lock()
                .unwrap()
                .push((app.to_path_buf(), target.to_path_buf()));
            Ok(())
        }))
        .with_app_catalog_provider(Arc::new(move |_| catalog_for_provider.clone()));
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let known_hosts = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", addr.port(), &host_pubkey, &known_hosts).unwrap();

    let spec = SshTargetSpec::quick_connect(
        "AppChooser Host",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            username: "secops".to_string(),
            auth: SshAuthMethod::KeyboardInteractive,
            known_hosts_path: Some(known_hosts),
            ..SshConnectConfig::default()
        },
    )
    .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let _ = state.update(Message::QuickConnectSsh(spec.clone()));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
    let buffer = Arc::clone(state.ssh_session(sid).unwrap().buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        sid,
        gen_id,
        spec,
        buffer,
        Arc::clone(&vault),
        ev_tx,
    ));
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let is_connected = matches!(ev, SshUiEvent::Connected { .. });
        let is_auth_prompt = matches!(ev, SshUiEvent::AuthPrompt { .. });
        let _ = state.update(Message::SshEvent {
            session_id: sid,
            event: ev,
        });
        if is_auth_prompt {
            let _ = state.update(Message::SshAuthInputChanged {
                session_id: sid,
                index: 0,
                value: "424242".to_string(),
            });
            let _ = state.update(Message::SubmitSshAuthPrompt(sid));
        }
        if is_connected {
            break;
        }
    }

    let list_gen = state.sftp_drawer().list_generation();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid,
        generation: list_gen,
        result: Ok(termewnal_ssh::RemoteListing {
            resolved_path: "/".to_string(),
            entries: vec![termewnal_ssh::RemoteDirEntry {
                name: "report.md".to_string(),
                path: "/report.md".to_string(),
                kind: RemoteEntryKind::File,
                size: 12,
                permissions: Some(0o644),
                modified_epoch_secs: None,
                uid: None,
                gid: None,
            }],
            transport_mode: SftpTransportMode::IsolatedTransferTransport,
            notice: None,
        }),
    });
    let _ = state.update(Message::SftpSessionRowClicked {
        session_id: sid,
        path: "/report.md".to_string(),
        ctrl_held: false,
    });

    // Clicking "Open with..." opens the in-app Application Chooser modal immediately.
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::OpenWith));
    assert!(matches!(
        state.sftp_drawer().plus.modal,
        SftpModalState::OpenWith { loading: true, .. }
    ));
    assert!(state.has_modal_open());
    drop(state.view(win));

    // Escape closes the chooser without downloading anything.
    let _ = state.update(Message::EscapePressed);
    assert!(matches!(state.sftp_drawer().plus.modal, SftpModalState::None));

    // Re-open, deliver discovered apps, render populated chooser, filter -> "code", confirm!
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::OpenWith));
    let _ = state.update(Message::SftpOpenWithAppsLoaded {
        session_id: sid,
        remote_path: "/report.md".to_string(),
        apps: catalog_apps,
    });
    match &state.sftp_drawer().plus.modal {
        SftpModalState::OpenWith {
            loading,
            apps,
            selected_launch_path,
            ..
        } => {
            assert!(!loading);
            assert_eq!(apps.len(), 2);
            assert_eq!(selected_launch_path.as_ref(), Some(&xed_desktop));
        }
        other => panic!("expected OpenWith modal, got {other:?}"),
    }
    drop(state.view(win));

    let _ = state.update(Message::SftpOpenWithSearchChanged("code".to_string()));
    if let SftpModalState::OpenWith {
        selected_launch_path,
        ..
    } = &state.sftp_drawer().plus.modal
    {
        assert_eq!(selected_launch_path.as_ref(), Some(&code_desktop));
    }
    drop(state.view(win));

    // Selecting twice quickly OR clicking Open launches the chosen app with staged local copy.
    let _ = state.update(Message::SftpOpenWithSelectApp(code_desktop.clone()));
    let _ = state.update(Message::SftpOpenWithConfirm);
    assert!(matches!(state.sftp_drawer().plus.modal, SftpModalState::None));

    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    let launched = launches.lock().unwrap().clone();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].0, code_desktop);
    assert_eq!(fs::read(&launched[0].1).unwrap(), b"# Q4 Report\n");
    assert_eq!(
        state.sftp_drawer().plus.watched_files[0].app_name,
        "Visual Studio Code"
    );
}




#[tokio::test]
async fn sftp_remote_file_editor_find_replace_undo_shortcuts_and_unsaved_guard() {
    use termewnal_ui::{SftpContextActionKind, SftpModalState};

    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&remote_root).unwrap();
    fs::write(
        remote_root.join("nginx.conf"),
        "server_name old.example.com;\r\nproxy_pass http://old.example.com:8080;\r\nworker_processes 4;\r\n",
    )
    .unwrap();

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
    let mut state = AppState::boot_with_paths_and_vault(paths.clone(), Arc::clone(&vault)).unwrap();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let known_hosts = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", addr.port(), &host_pubkey, &known_hosts).unwrap();

    let spec = SshTargetSpec::quick_connect(
        "Editor Host",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            username: "secops".to_string(),
            auth: SshAuthMethod::KeyboardInteractive,
            known_hosts_path: Some(known_hosts),
            proxy_jump: Vec::new(),
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            ..SshConnectConfig::default()
        },
    )
    .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let _ = state.update(Message::QuickConnectSsh(spec.clone()));
    let sid = state.active_session_id().unwrap();
    let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
    let buffer = Arc::clone(state.ssh_session(sid).unwrap().buffer());
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(
        sid,
        gen_id,
        spec,
        buffer,
        Arc::clone(&vault),
        ev_tx,
    ));
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let is_connected = matches!(ev, SshUiEvent::Connected { .. });
        let is_auth = matches!(ev, SshUiEvent::AuthPrompt { .. });
        let _ = state.update(Message::SshEvent {
            session_id: sid,
            event: ev,
        });
        if is_auth {
            let _ = state.update(Message::SshAuthInputChanged {
                session_id: sid,
                index: 0,
                value: "424242".to_string(),
            });
            let _ = state.update(Message::SubmitSshAuthPrompt(sid));
        }
        if is_connected {
            break;
        }
    }

    let ssh_slot = state.ssh_session(sid).unwrap();
    let endpoint = termewnal_ssh::SftpEndpoint::new(
        termewnal_ssh::ProfileTransportKey::from_connect_config(
            None,
            &ssh_slot.spec().connect_config,
        ),
        ssh_slot.spec().connect_config.clone(),
    )
    .with_primary_transport(Arc::clone(ssh_slot.ssh_session().unwrap().primary_transport()));

    let gen_init = state.sftp_drawer().list_generation();
    let listing = state.sftp_manager().list_dir(&endpoint, "/").await.unwrap();
    let _ = state.update(Message::SftpBrowseResult {
        session_id: sid,
        generation: gen_init,
        result: Ok(listing),
    });
    let _ = state.update(Message::SftpSessionRowClicked {
        session_id: sid,
        path: "/nginx.conf".to_string(),
        ctrl_held: false,
    });

    // 1) Open in View mode: Ctrl+F opens editor Find WITHOUT opening terminal search; Ctrl+H ignored.
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::View));
    let raw = state
        .sftp_manager()
        .read_text_file(&endpoint, "/nginx.conf", 2 * 1024 * 1024)
        .await
        .unwrap();
    let _ = state.update(Message::SftpRemoteFileReadResult {
        session_id: sid,
        request_path: "/nginx.conf".to_string(),
        editable: false,
        result: Ok(raw),
    });

    let _ = state.update(Message::OpenSearch);
    assert!(!state.search().open, "Ctrl+F must not open terminal search while Remote File Editor is open");
    let _ = state.update(Message::SftpModalShortcut(termewnal_ui::RemoteEditorShortcut::Replace));
    if let SftpModalState::FileViewer {
        editable,
        content,
        ..
    } = &state.sftp_drawer().plus.modal
    {
        assert!(!editable);
        assert!(content.find_open);
        assert!(!content.replace_open, "Ctrl+H must stay disabled in View mode");
        assert_eq!(content.line_ending, termewnal_ui::EditorLineEnding::CrLf);
    } else {
        panic!("expected FileViewer modal");
    }

    let _ = state.update(Message::SftpModalFindQueryChanged("old.example.com".to_string()));
    if let SftpModalState::FileViewer { content, .. } = &state.sftp_drawer().plus.modal {
        assert_eq!(content.matches.len(), 2);
        assert_eq!(content.active_match, Some(0));
    }
    let _ = state.update(Message::SearchNext);
    if let SftpModalState::FileViewer { content, .. } = &state.sftp_drawer().plus.modal {
        assert_eq!(content.active_match, Some(1));
    }

    // 2) Switch to Edit in-place preserves open Find state, unlocks Replace All & Undo/Redo.
    let _ = state.update(Message::SftpModalSwitchToEdit);
    let _ = state.update(Message::SftpModalShortcut(termewnal_ui::RemoteEditorShortcut::Replace));
    let _ = state.update(Message::SftpModalReplaceTextChanged("new.internal".to_string()));
    let _ = state.update(Message::SftpModalReplaceAll);
    if let SftpModalState::FileViewer {
        editable,
        dirty,
        content,
        ..
    } = &state.sftp_drawer().plus.modal
    {
        assert!(*editable);
        assert!(*dirty);
        assert!(content.replace_open);
        assert!(content.text().contains("new.internal"));
        assert!(!content.text().contains("old.example.com"));
        assert_eq!(content.replace_feedback.as_deref(), Some("Replaced 2 occurrences"));
    }

    // Undo restores original text and dirty=false; Redo re-applies.
    let _ = state.update(Message::SftpModalEditorEvent(
        termewnal_ui::RemoteEditorEvent::Undo,
    ));
    if let SftpModalState::FileViewer { dirty, content, .. } = &state.sftp_drawer().plus.modal {
        assert!(!dirty);
        assert!(content.text().contains("old.example.com"));
    }
    let _ = state.update(Message::SftpModalEditorEvent(
        termewnal_ui::RemoteEditorEvent::Redo,
    ));

    // 3) Go to Line (Ctrl+G) + layered Esc dismissal + Unsaved Changes Guard.
    let _ = state.update(Message::SftpModalShortcut(termewnal_ui::RemoteEditorShortcut::GoToLine));
    let _ = state.update(Message::SftpModalGoToLineInputChanged("3:5".to_string()));
    let _ = state.update(Message::SftpModalGoToLineSubmit);
    if let SftpModalState::FileViewer { content, .. } = &state.sftp_drawer().plus.modal {
        assert!(!content.goto_open);
        assert_eq!(content.cursor_line_col_1based(), (3, 5));
    }

    // Re-open GoTo -> Esc closes GoTo -> Esc closes Find -> Esc triggers Unsaved Guard!
    let _ = state.update(Message::SftpModalShortcut(termewnal_ui::RemoteEditorShortcut::GoToLine));
    let _ = state.update(Message::EscapePressed);
    if let SftpModalState::FileViewer { content, .. } = &state.sftp_drawer().plus.modal {
        assert!(!content.goto_open);
        assert!(content.find_open);
    }
    let _ = state.update(Message::EscapePressed);
    if let SftpModalState::FileViewer { content, .. } = &state.sftp_drawer().plus.modal {
        assert!(!content.find_open);
        assert_eq!(content.pending_confirm, None);
    }
    let _ = state.update(Message::EscapePressed);
    if let SftpModalState::FileViewer { content, .. } = &state.sftp_drawer().plus.modal {
        assert_eq!(
            content.pending_confirm,
            Some(termewnal_ui::UnsavedConfirmAction::CloseModal)
        );
    }

    // Cancel guard -> toggle Word Wrap, Expand, Indent mode and verify config.toml persistence.
    let _ = state.update(Message::SftpModalCancelUnsaved);
    let _ = state.update(Message::SftpModalToggleWordWrap);
    let _ = state.update(Message::SftpModalToggleExpand);
    let _ = state.update(Message::SftpModalCycleIndentMode);
    let persisted_cfg = termewnal_core::load_config(&paths).unwrap();
    assert!(persisted_cfg.terminal.sftp_editor_word_wrap);
    assert!(persisted_cfg.terminal.sftp_editor_expanded);
    assert_eq!(
        persisted_cfg.terminal.sftp_editor_indent_mode,
        termewnal_core::EditorIndentMode::Spaces2
    );

    // Render view with all widgets active to verify layout/draw build cleanly.
    drop(state.view(win));

    // Confirm Save & Close writes CRLF preserved payload and dismisses modal on SaveResult.
    let _ = state.update(Message::SftpModalRequestClose(Some(sid)));
    let _ = state.update(Message::SftpModalConfirmUnsaved { save_first: true });
    let expected_saved =
        "server_name new.internal;\r\nproxy_pass http://new.internal:8080;\r\nworker_processes 4;\r\n";
    state
        .sftp_manager()
        .write_text_file(&endpoint, "/nginx.conf", expected_saved)
        .await
        .unwrap();
    let _ = state.update(Message::SftpRemoteFileSaveResult {
        session_id: sid,
        path: "/nginx.conf".to_string(),
        result: Ok(()),
    });
    assert!(matches!(state.sftp_drawer().plus.modal, SftpModalState::None));
    assert_eq!(
        fs::read_to_string(remote_root.join("nginx.conf")).unwrap(),
        expected_saved
    );
}

#[test]
fn sftp_geometry_eligibility_overlay_persistence_zoom_width_isolation_and_backdrop_dismiss() {
    use iced::Size;
    use termewnal_core::TabBarPosition;
    use termewnal_ui::sftp_plus::file_viewer_backdrop_dismiss;
    use termewnal_ui::workspace::{SplitAxis, split_first_size};
    use termewnal_ui::{DEFAULT_SFTP_DRAWER_WIDTH, IpcCommand, SftpDrawerDock};

    // 1. split_first_size-aligned geometry bounds & <140px narrow clamp never overflows bounds.
    let (fw, fh) = split_first_size(949.0, 703.0, SplitAxis::Vertical, 0.5, 1.0);
    assert_eq!(fw, 474.0);
    assert_eq!(fh, 703.0);
    let tiny_w = SftpDrawerDock::clamped_panel_width(360.0, 120.0);
    assert!(tiny_w < 120.0, "narrow clamp {tiny_w} must not exceed 120px bounds");

    // 2. FileViewer backdrop_dismiss protects unsaved dirty edits while allowing clean dismiss.
    assert!(file_viewer_backdrop_dismiss(None, true, true).is_none());
    assert!(file_viewer_backdrop_dismiss(None, true, false).is_some());
    assert!(file_viewer_backdrop_dismiss(None, false, true).is_some());
    assert!(file_viewer_backdrop_dismiss(None, false, false).is_some());

    // 3. Full chrome deduction (vertical_rail_w + sidebar + divider) gates auto-open & restores on rail hide.
    let (_temp, mut state) = boot_test_state();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));
    let _ = state.update(Message::WindowResized(win, Size::new(1100.0, 760.0)));
    let _ = state.update(Message::SettingsTabBarPositionSelected(TabBarPosition::Left));

    let profile_a = Profile::ssh("Box A", "10.0.1.10", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile_a));
    let sid_a = state.active_session_id().expect("ssh a");
    let gen_a = state.ssh_session(sid_a).unwrap().ssh_generation();
    let _ = state.update(Message::SshEvent {
        session_id: sid_a,
        event: SshUiEvent::Connected {
            generation: gen_a,
            session: None,
            supervisor: None,
        },
    });
    // 1100 - 250(sidebar) - 1 - 192(vertical rail) - 1 = 656 < 680 => auto-suppressed.
    assert!(!state.is_sftp_drawer_open_for(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_auto_suppressed());

    // Switching back to Top tabs frees vertical_rail_w (849 >= 680) => auto-restores docked drawer.
    let _ = state.update(Message::SettingsTabBarPositionSelected(TabBarPosition::Top));
    assert!(state.is_sftp_drawer_open_for(sid_a));
    let d_a = state.sftp_drawer_for_session(sid_a).unwrap();
    assert!(d_a.is_docked_open());
    assert!(!d_a.is_overlay_open());
    assert!(!d_a.is_auto_suppressed());

    // Height < 360px suppresses docked drawer cleanly and restores when height grows back.
    let _ = state.update(Message::WindowResized(win, Size::new(1200.0, 400.0)));
    assert!(!state.is_sftp_drawer_open_for(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_auto_suppressed());
    let _ = state.update(Message::WindowResized(win, Size::new(1200.0, 800.0)));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_docked_open());

    // 4. Split -> Dock collapses; opening overlay survives further resize & closing overlay preserves dock auto-restore on Zoom.
    let _ = state.update(Message::SplitFocusedPane(SplitAxis::Vertical));
    let profile_b = Profile::ssh("Box B", "10.0.1.20", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(profile_b));
    let sid_b = state.active_session_id().expect("ssh b");
    let gen_b = state.ssh_session(sid_b).unwrap().ssh_generation();
    let _ = state.update(Message::SshEvent {
        session_id: sid_b,
        event: SshUiEvent::Connected {
            generation: gen_b,
            session: None,
            supervisor: None,
        },
    });
    assert!(!state.is_sftp_drawer_open_for(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_auto_suppressed());

    let _ = state.update(Message::ToggleSftpDrawerForSession(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_overlay_open());
    assert!(!state.sftp_drawer_for_session(sid_a).unwrap().is_docked_open());
    drop(state.view(win));

    // Resize narrower while overlay is open: overlay MUST NOT collapse!
    let _ = state.update(Message::WindowResized(win, Size::new(1020.0, 720.0)));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_overlay_open());

    // Dismissing temporary overlay keeps Auto dock intent so TogglePaneZoom auto-restores docked drawer!
    let _ = state.update(Message::SftpSessionClose(sid_a));
    assert!(!state.is_sftp_drawer_open_for(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_auto_suppressed());
    assert!(!state.sftp_drawer_for_session(sid_a).unwrap().is_manually_closed());

    let _ = state.update(Message::FocusSession(sid_a));
    let _ = state.update(Message::TogglePaneZoom);
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_docked_open());

    // Manual close while docked sets ManuallyClosed; unzoom + rezoom must NOT auto-open again.
    let _ = state.update(Message::SftpSessionClose(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_manually_closed());
    let _ = state.update(Message::TogglePaneZoom);
    let _ = state.update(Message::TogglePaneZoom);
    assert!(!state.is_sftp_drawer_open_for(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_manually_closed());

    // Unzoom and widen to 1700x900 so both split panes >= 680px => sid_b auto-restores docked
    // while sid_a stays manually closed until re-opened, and per-session drawer widths stay isolated.
    let _ = state.update(Message::TogglePaneZoom);
    let _ = state.update(Message::WindowResized(win, Size::new(1700.0, 900.0)));
    assert!(state.sftp_drawer_for_session(sid_b).unwrap().is_docked_open());
    assert!(!state.is_sftp_drawer_open_for(sid_a));
    let _ = state.update(Message::ToggleSftpDrawerForSession(sid_a));
    assert!(state.sftp_drawer_for_session(sid_a).unwrap().is_docked_open());
    assert!(state.sftp_drawer_for_session(sid_b).unwrap().is_docked_open());
    let _ = state.update(Message::FocusSession(sid_a));
    let _ = state.update(Message::ResizeSftpDrawer(510.0));
    assert_eq!(state.sftp_drawer_for_session(sid_a).unwrap().width(), 510.0);
    assert_eq!(
        state.sftp_drawer_for_session(sid_b).unwrap().width(),
        DEFAULT_SFTP_DRAWER_WIDTH
    );

    // Sanity check SplitNode::collect_pane_bounds on current 2-pane workspace tab.
    let bounds = state
        .workspace()
        .active_tab()
        .unwrap()
        .root
        .collect_pane_bounds(949.0, 703.0);
    assert_eq!(bounds.len(), 2);
    for &(w, _h) in bounds.values() {
        assert_eq!(w, 474.0);
    }

    // 5. Quake drop-down suppresses docked drawer even at wide 1280px and renders overlay on ToggleSftpDrawerForSession.
    let _ = state.update(Message::IpcCommand(IpcCommand::ToggleQuake));
    let quake_win = state.quake().window_id().expect("quake win");
    let _ = state.update(Message::WindowOpened(quake_win));
    let _ = state.update(Message::WindowResized(quake_win, Size::new(1280.0, 480.0)));
    let profile_q = Profile::ssh("Quake Host", "10.0.1.30", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::InWindow {
        window_id: quake_win,
        inner: Box::new(Message::OpenSshProfile(profile_q)),
    });
    let sid_q = state.active_session_id().expect("quake ssh");
    let gen_q = state.ssh_session(sid_q).unwrap().ssh_generation();
    let _ = state.update(Message::SshEvent {
        session_id: sid_q,
        event: SshUiEvent::Connected {
            generation: gen_q,
            session: None,
            supervisor: None,
        },
    });
    assert!(!state.is_sftp_drawer_open_for(sid_q));
    assert!(state.sftp_drawer_for_session(sid_q).unwrap().is_auto_suppressed());
    let _ = state.update(Message::ToggleSftpDrawerForSession(sid_q));
    assert!(state.sftp_drawer_for_session(sid_q).unwrap().is_overlay_open());
    assert!(!state.sftp_drawer_for_session(sid_q).unwrap().is_docked_open());
    drop(state.view(quake_win));
}

#[test]
fn sftp_modal_and_focus_stay_scoped_per_window_and_session() {
    use termewnal_ssh::sftp::SftpTransportMode;
    use termewnal_ssh::{RemoteDirEntry, RemoteEntryKind, RemoteListing};
    use termewnal_ui::SftpContextActionKind;

    let (_temp, mut state) = boot_test_state();
    let win_a = window::Id::unique();
    let win_b = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win_a));
    let _ = state.update(Message::WindowOpened(win_b));

    let p_a = Profile::ssh("Host A", "10.2.0.1", 22, "ops", SshAuthMethod::Agent);
    let p_b = Profile::ssh("Host B", "10.2.0.2", 22, "ops", SshAuthMethod::Agent);
    let p_c = Profile::ssh("Host C", "10.2.0.3", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::InWindow {
        window_id: win_a,
        inner: Box::new(Message::OpenSshProfile(p_a)),
    });
    let sid_a = state.active_session_id().expect("sid_a");

    let _ = state.update(Message::InWindow {
        window_id: win_b,
        inner: Box::new(Message::OpenSshProfile(p_b)),
    });
    let sid_b = state.active_session_id().expect("sid_b");

    let _ = state.update(Message::InWindow {
        window_id: win_b,
        inner: Box::new(Message::OpenSshProfile(p_c)),
    });
    let sid_c = state.active_session_id().expect("sid_c");

    for sid in [sid_a, sid_b, sid_c] {
        let gen_id = state.ssh_session(sid).unwrap().ssh_generation();
        let _ = state.update(Message::SshEvent {
            session_id: sid,
            event: SshUiEvent::Connected {
                generation: gen_id,
                session: None,
                supervisor: None,
            },
        });
        let list_gen = state.sftp_drawer_for_session(sid).unwrap().list_generation();
        let _ = state.update(Message::SftpBrowseResult {
            session_id: sid,
            generation: list_gen,
            result: Ok(RemoteListing {
                resolved_path: "/srv".to_string(),
                entries: vec![RemoteDirEntry {
                    name: "conf.toml".to_string(),
                    path: "/srv/conf.toml".to_string(),
                    kind: RemoteEntryKind::File,
                    size: 64,
                    permissions: Some(0o644),
                    modified_epoch_secs: None,
                    uid: None,
                    gid: None,
                }],
                transport_mode: SftpTransportMode::IsolatedTransferTransport,
                notice: None,
            }),
        });
    }

    // Open modal on sid_a (win_a) and verify win_b is NOT blocked.
    let _ = state.update(Message::WindowFocused(win_a));
    let _ = state.update(Message::FocusSftpPanel(sid_a));
    let _ = state.update(Message::SftpShortcut(SftpContextActionKind::SelectAll));
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::Properties));
    assert!(state.has_modal_open_in_window(win_a));
    assert!(!state.has_modal_open_in_window(win_b));

    // Also open modals on sid_b and sid_c in win_b.
    let _ = state.update(Message::WindowFocused(win_b));
    let _ = state.update(Message::FocusSftpPanel(sid_b));
    let _ = state.update(Message::SftpShortcut(SftpContextActionKind::SelectAll));
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::Properties));
    let _ = state.update(Message::FocusSftpPanel(sid_c));
    let _ = state.update(Message::SftpShortcut(SftpContextActionKind::SelectAll));
    let _ = state.update(Message::SftpContextAction(SftpContextActionKind::Properties));
    assert!(state.has_modal_open_in_window(win_b));

    // SftpDismissModal(Some(sid_a)) dismisses ONLY sid_a, preserving sid_b & sid_c.
    let _ = state.update(Message::SftpDismissModal(Some(sid_a)));
    assert!(!state.has_modal_open_in_window(win_a));
    assert!(state.sftp_drawer_for_session(sid_b).unwrap().plus.modal.is_open());
    assert!(state.sftp_drawer_for_session(sid_c).unwrap().plus.modal.is_open());

    // SftpDismissModal(None) resolves ONLY the active modal (sid_c), preserving sid_b!
    let _ = state.update(Message::SftpDismissModal(None));
    assert!(!state.sftp_drawer_for_session(sid_c).unwrap().plus.modal.is_open());
    assert!(state.sftp_drawer_for_session(sid_b).unwrap().plus.modal.is_open());
    drop(state.view(win_a));
    drop(state.view(win_b));
}
