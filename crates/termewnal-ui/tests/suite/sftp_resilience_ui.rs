use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Seek, SeekFrom, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::window;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use russh_sftp::protocol::{
    Attrs, Data, File as SftpProtoFile, FileAttributes, Handle, Name, OpenFlags, SftpHandle,
    SftpString, Status, StatusCode, Version,
};
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigFileKind, ConfigPaths, SshAuthMethod, TransferConflictPolicy, VaultBackend,
    VaultConfig, save_config,
    vault::{EncryptedVault, InMemorySecretStore, SecretStore, VaultSession},
};
use termewnal_ssh::{
    ConflictResolution, OsAgentConnector, SshConnectConfig, TransferNotice, TransferStatus,
    record_known_host_path,
};
use termewnal_ui::{Message, SshTargetSpec, SshUiEvent, drive_ssh_connect};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use super::common::boot_test_app_with_vault;

struct UiResilienceSftpState {
    root_dir: PathBuf,
    username: String,
    password: String,
    deny_open_exact: Mutex<BTreeSet<String>>,
}

impl UiResilienceSftpState {
    fn resolve(&self, raw: &str) -> PathBuf {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "." || trimmed == "/" {
            return self.root_dir.clone();
        }
        let mut clean = PathBuf::new();
        for comp in Path::new(trimmed.trim_start_matches('/')).components() {
            match comp {
                std::path::Component::Normal(p) => clean.push(p),
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                _ => {}
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

struct OpenHandle {
    file: Option<std::fs::File>,
    dir_entries: Option<Vec<SftpProtoFile>>,
}

struct UiResilienceSftp {
    state: Arc<UiResilienceSftpState>,
    next_id: u64,
    handles: HashMap<Vec<u8>, OpenHandle>,
}

impl UiResilienceSftp {
    fn new(state: Arc<UiResilienceSftpState>) -> Self {
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

impl russh_sftp::server::Handler for UiResilienceSftp {
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
        let canon = fs::canonicalize(&abs).unwrap_or(abs);
        Ok(Name {
            id,
            files: vec![SftpProtoFile::dummy(self.state.virt(&canon))],
        })
    }

    async fn opendir(&mut self, id: u32, path: SftpString) -> Result<Handle, Self::Error> {
        let abs = self.state.resolve(&path);
        let mut files = Vec::new();
        for entry in fs::read_dir(&abs).map_err(|_| StatusCode::NoSuchFile)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(meta) = fs::symlink_metadata(entry.path()) {
                files.push(SftpProtoFile::new(name, FileAttributes::from(&meta)));
            }
        }
        let h = self.next_id.to_be_bytes().to_vec();
        self.next_id += 1;
        self.handles.insert(
            h.clone(),
            OpenHandle {
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

    async fn readlink(&mut self, id: u32, path: SftpString) -> Result<Name, Self::Error> {
        let abs = self.state.resolve(&path);
        let target = fs::read_link(&abs).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Name {
            id,
            files: vec![SftpProtoFile::dummy(target.to_string_lossy().into_owned())],
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
        if self
            .state
            .deny_open_exact
            .lock()
            .unwrap()
            .contains(filename.as_str())
        {
            return Err(StatusCode::PermissionDenied);
        }
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
            OpenHandle {
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

struct UiResilienceSshHandler {
    state: Arc<UiResilienceSftpState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    shells: BTreeSet<ChannelId>,
}

impl server::Handler for UiResilienceSshHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if user == self.state.username && password == self.state.password {
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
        session.data(channel, b"UI_READY\r\n".to_vec())?;
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
                UiResilienceSftp::new(Arc::clone(&self.state)),
            )
            .await;
            return Ok(());
        }
        session.channel_failure(channel_id)?;
        Ok(())
    }
}

async fn spawn_ui_resilience_server(
    root_dir: PathBuf,
) -> (
    SocketAddr,
    PublicKey,
    Arc<UiResilienceSftpState>,
    oneshot::Sender<()>,
) {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();
    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

    let state = Arc::new(UiResilienceSftpState {
        root_dir,
        username: "ops".to_string(),
        password: "ui-secret".to_string(),
        deny_open_exact: Mutex::new(BTreeSet::new()),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = oneshot::channel();
    let srv_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    let cfg = Arc::clone(&cfg);
                    let st = Arc::clone(&srv_state);
                    tokio::spawn(async move {
                        let _ = server::run_stream(
                            cfg,
                            stream,
                            UiResilienceSshHandler {
                                state: st,
                                channels: HashMap::new(),
                                shells: BTreeSet::new(),
                            },
                        )
                        .await;
                    });
                }
            }
        }
    });
    (addr, host_pubkey, state, tx)
}

async fn boot_connected_ui(
    temp: &TempDir,
    addr: SocketAddr,
    host_pubkey: &PublicKey,
) -> (termewnal_ui::AppState, window::Id, termewnal_ui::SessionId) {
    let paths = ConfigPaths::from_root(temp.path());
    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    vault.set_secret("ssh/ui_pw", "ui-secret").unwrap();

    let mut state = boot_test_app_with_vault(paths, Arc::clone(&vault)).unwrap();
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let known_hosts = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", addr.port(), host_pubkey, &known_hosts).unwrap();

    let spec = SshTargetSpec::quick_connect(
        "UI Resilience Host",
        SshConnectConfig {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            username: "ops".to_string(),
            auth: SshAuthMethod::Password {
                vault_secret_ref: Some("ssh/ui_pw".to_string()),
            },
            known_hosts_path: Some(known_hosts),
            nodelay: true,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            proxy_jump: Vec::new(),
        },
    )
    .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let _ = state.update(Message::QuickConnectSsh(spec.clone()));
    let sid = state.active_session_id().unwrap();
    let slot = state.ssh_session(sid).unwrap();
    let gen_id = slot.ssh_generation();
    let buffer = Arc::clone(slot.buffer());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(drive_ssh_connect(sid, gen_id, spec, buffer, vault, ev_tx));

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("SSH connect timeout")
            .expect("event channel open");
        let done = matches!(ev, SshUiEvent::Connected { .. });
        let _ = state.update(Message::SshEvent {
            session_id: sid,
            event: ev,
        });
        if done {
            break;
        }
    }

    (state, win, sid)
}

#[tokio::test]
async fn slice6_recursive_folder_drop_and_sftp_dir_download_preserve_permissions_and_skip_loops() {
    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    let local_src = temp.path().join("bundle_dir");
    let local_dl = temp.path().join("downloads");
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(local_src.join("bin")).unwrap();
    fs::create_dir_all(&local_dl).unwrap();

    fs::write(local_src.join("bin/run.sh"), b"#!/bin/sh\necho ok\n").unwrap();
    fs::write(local_src.join("readme.md"), b"hello recursive").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::set_permissions(
            local_src.join("bin/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("..", local_src.join("bin/loop_up")).unwrap();
    }

    let (addr, host_pubkey, _srv_state, _shutdown) =
        spawn_ui_resilience_server(remote_root.clone()).await;
    let (mut state, win, _sid) = boot_connected_ui(&temp, addr, &host_pubkey).await;
    state.set_sftp_download_dir(local_dl.clone());

    let _ = state.update(Message::ToggleSftpDrawer);
    let _ = state.update(Message::SftpNavigateTo("/".to_string()));
    let _ = state.update(Message::SftpDrawerFileDropped {
        path: local_src.clone(),
    });

    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    assert_eq!(state.sftp_drawer().transfers().len(), 2);
    assert!(
        state
            .sftp_drawer()
            .transfers()
            .iter()
            .all(|t| t.status == TransferStatus::Completed)
    );
    assert_eq!(
        fs::read(remote_root.join("bundle_dir/bin/run.sh")).unwrap(),
        b"#!/bin/sh\necho ok\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(remote_root.join("bundle_dir/bin/run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(state.sftp_drawer().last_transfer_notice().is_some());
    }

    // Trigger recursive folder download action via Message::SftpDownloadDir.
    let _ = state.update(Message::SftpDownloadDir {
        remote_path: "/bundle_dir".to_string(),
        dir_name: "bundle_dir".to_string(),
    });
    // Drive the directory walk task via TransferQueue directly as well if headless test doesn't poll iced::Task.
    let slot = state
        .ssh_session(state.active_session_id().unwrap())
        .unwrap();
    let endpoint = termewnal_ssh::SftpEndpoint::new(
        termewnal_ssh::ProfileTransportKey::from_connect_config(
            None,
            &slot.spec().connect_config,
        ),
        slot.spec().connect_config.clone(),
    )
    .with_primary_transport(Arc::clone(slot.ssh_session().unwrap().primary_transport()));
    let dl_summary = state
        .transfer_queue()
        .enqueue_download_dir(
            endpoint,
            "/bundle_dir".to_string(),
            local_dl.join("bundle_dir_dl"),
        )
        .await
        .unwrap();
    let _ = state.update(Message::SftpDirEnqueueResult {
        result: Ok(dl_summary),
    });
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));
    drop(state.view(win));

    assert_eq!(
        fs::read(local_dl.join("bundle_dir_dl/bin/run.sh")).unwrap(),
        b"#!/bin/sh\necho ok\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(local_dl.join("bundle_dir_dl/bin/run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}

#[tokio::test]
async fn slice6_conflict_dialog_renders_and_resolves_rename_skip_and_apply_to_all() {
    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    let uploads = temp.path().join("uploads");
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&uploads).unwrap();

    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(remote_root.join(name), format!("existing-{name}")).unwrap();
        fs::write(uploads.join(name), format!("incoming-{name}")).unwrap();
    }

    let (addr, host_pubkey, _srv_state, _shutdown) =
        spawn_ui_resilience_server(remote_root.clone()).await;
    let (mut state, win, _sid) = boot_connected_ui(&temp, addr, &host_pubkey).await;

    let _ = state.update(Message::ToggleSftpDrawer);
    let _ = state.update(Message::SftpNavigateTo("/".to_string()));
    for name in ["a.txt", "b.txt", "c.txt"] {
        let _ = state.update(Message::SftpDrawerFileDropped {
            path: uploads.join(name),
        });
    }

    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    let conflict = state
        .sftp_drawer()
        .active_conflict()
        .expect("expected Conflict Resolution Dialog state")
        .clone();
    assert_eq!(conflict.file_name, "a.txt");
    assert_eq!(state.sftp_drawer().conflict_rename_input(), "a (1).txt");
    drop(state.view(win));

    // Rename the first file (`apply_to_all = false`).
    let _ = state.update(Message::TransferConflictRenameInputChanged(
        "a_custom.txt".to_string(),
    ));
    assert_eq!(state.sftp_drawer().conflict_rename_input(), "a_custom.txt");
    let _ = state.update(Message::ResolveTransferConflict {
        transfer_id: conflict.transfer_id,
        resolution: ConflictResolution::Rename {
            new_name: state.sftp_drawer().conflict_rename_input().to_string(),
        },
        apply_to_all: false,
    });
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    // Next pending conflict (`b.txt`) surfaces -> toggle `Apply to all` and `Skip`.
    let second = state
        .sftp_drawer()
        .active_conflict()
        .expect("expected second conflict")
        .clone();
    assert_eq!(second.file_name, "b.txt");
    let _ = state.update(Message::TransferConflictApplyToAllToggled(true));
    assert!(state.sftp_drawer().conflict_apply_to_all());
    let _ = state.update(Message::ResolveTransferConflict {
        transfer_id: second.transfer_id,
        resolution: ConflictResolution::Skip,
        apply_to_all: state.sftp_drawer().conflict_apply_to_all(),
    });
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));

    assert!(state.sftp_drawer().active_conflict().is_none());
    assert_eq!(fs::read(remote_root.join("a.txt")).unwrap(), b"existing-a.txt");
    assert_eq!(
        fs::read(remote_root.join("a_custom.txt")).unwrap(),
        b"incoming-a.txt"
    );
    assert_eq!(fs::read(remote_root.join("b.txt")).unwrap(), b"existing-b.txt");
    assert_eq!(fs::read(remote_root.join("c.txt")).unwrap(), b"existing-c.txt");
}

#[tokio::test]
async fn slice6_part_fallback_notice_and_config_hot_reload_propagate_to_queue() {
    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    let uploads = temp.path().join("uploads");
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&uploads).unwrap();

    let (addr, host_pubkey, srv_state, _shutdown) =
        spawn_ui_resilience_server(remote_root.clone()).await;
    srv_state
        .deny_open_exact
        .lock()
        .unwrap()
        .insert("/large.bin.termewnal.part".to_string());

    let (mut state, win, _sid) = boot_connected_ui(&temp, addr, &host_pubkey).await;
    let _ = state.update(Message::ToggleSftpDrawer);
    let _ = state.update(Message::SftpNavigateTo("/".to_string()));

    let payload = vec![0x77u8; 1_200_000];
    let local_large = uploads.join("large.bin");
    fs::write(&local_large, &payload).unwrap();

    let _ = state.update(Message::SftpDrawerFileDropped { path: local_large });
    assert!(state.transfer_queue().wait_until_idle(Duration::from_secs(10)).await);
    let _ = state.update(Message::TransferQueueUpdated(
        state.transfer_queue().snapshot(),
    ));
    drop(state.view(win));

    let item = &state.sftp_drawer().transfers()[0];
    assert_eq!(item.status, TransferStatus::Completed);
    assert!(!item.using_part_file);
    assert!(matches!(
        item.notice,
        Some(TransferNotice::PartFallbackDirectWrite { .. })
    ));
    assert!(state.sftp_drawer().last_transfer_notice().is_some());
    assert!(state.last_error().is_none());
    assert_eq!(fs::read(remote_root.join("large.bin")).unwrap(), payload);

    // Hot-reload `config.toml` and verify TransferConfig propagates to TransferQueue.
    let paths = ConfigPaths::from_root(temp.path());
    let mut cfg = AppConfig::default();
    cfg.terminal.shell = Some("/bin/sh".to_string());
    cfg.transfer.default_conflict_policy = TransferConflictPolicy::Overwrite;
    cfg.transfer.follow_symlinks = true;
    cfg.transfer.part_threshold_bytes = 2_097_152;
    save_config(&paths, &cfg).unwrap();

    let _ = state.update(Message::ConfigFilesChanged(vec![ConfigFileKind::AppConfig]));
    assert_eq!(state.transfer_queue().transfer_config(), cfg.transfer);
}
