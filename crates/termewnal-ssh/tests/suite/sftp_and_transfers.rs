use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Seek, SeekFrom, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use russh_sftp::protocol::{
    Attrs, Data, File as SftpProtoFile, FileAttributes, Handle, Name, OpenFlags, SftpHandle,
    SftpString, Status, StatusCode, Version,
};
use tempfile::TempDir;
use termewnal_core::{
    SshAuthMethod, VaultBackend, VaultConfig,
    vault::{EncryptedVault, InMemorySecretStore, SecretStore, VaultSession},
};
use termewnal_ssh::{
    AgentConnector, BoxFuture, DEFAULT_MAX_CONCURRENT_TRANSFERS, DEFAULT_TRANSFER_CHUNK_SIZE,
    HostKeyDecision, HostKeyPrompt, KeyboardInteractiveChallenge, OsAgentConnector,
    ProfileTransportKey, RemoteEntryKind, SftpEndpoint, SftpManager, SftpTransportMode,
    SshConnectConfig, SshInteractionHandler, SshSession, TransferDirection, TransferQueue,
    TransferQueueConfig, TransferStatus, record_known_host_path,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use zeroize::Zeroizing;

#[derive(Clone)]
enum ServerAuthMode {
    Password {
        username: String,
        password: String,
    },
    KeyboardInteractive {
        username: String,
        otp_code: String,
    },
    PublicKeyFirstThenKbi {
        username: String,
        allowed_key: PublicKey,
        otp_code: String,
    },
}

struct SftpTestServerState {
    root_dir: PathBuf,
    auth_mode: ServerAuthMode,
    connection_count: AtomicUsize,
    auth_attempts: AtomicUsize,
    kbi_prompts_issued: AtomicUsize,
    sftp_channels_opened: AtomicUsize,
    active_write_handles: AtomicUsize,
    max_concurrent_write_handles: AtomicUsize,
    observed_write_chunk_sizes: Mutex<Vec<usize>>,
    write_delay: Mutex<Option<Duration>>,
    fail_open_paths: Mutex<BTreeSet<String>>,
}

impl SftpTestServerState {
    fn resolve_virtual(&self, raw: &str) -> Result<PathBuf, StatusCode> {
        let trimmed = raw.trim();
        let rel = if trimmed.is_empty() || trimmed == "." || trimmed == "/" {
            PathBuf::new()
        } else {
            let stripped = trimmed.trim_start_matches('/');
            let mut clean = PathBuf::new();
            for comp in Path::new(stripped).components() {
                match comp {
                    std::path::Component::Normal(part) => clean.push(part),
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        clean.pop();
                    }
                    std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
                }
            }
            clean
        };
        Ok(self.root_dir.join(rel))
    }

    fn virtual_from_abs(&self, abs: &Path) -> String {
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
    is_write: bool,
    dir_entries: Option<Vec<SftpProtoFile>>,
}

struct TestSftpSubsystem {
    state: Arc<SftpTestServerState>,
    version: Option<u32>,
    next_handle: u64,
    handles: HashMap<Vec<u8>, OpenHandle>,
}

impl TestSftpSubsystem {
    fn new(state: Arc<SftpTestServerState>) -> Self {
        Self {
            state,
            version: None,
            next_handle: 1,
            handles: HashMap::new(),
        }
    }

    fn alloc_handle(&mut self) -> Vec<u8> {
        let id = self.next_handle;
        self.next_handle += 1;
        id.to_be_bytes().to_vec()
    }

    fn ok_status(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        }
    }
}

impl Drop for TestSftpSubsystem {
    fn drop(&mut self) {
        for handle in self.handles.values() {
            if handle.is_write {
                self.state
                    .active_write_handles
                    .fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

impl russh_sftp::server::Handler for TestSftpSubsystem {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        if self.version.is_some() {
            return Err(StatusCode::ConnectionLost);
        }
        self.version = Some(version);
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: SftpString) -> Result<Name, Self::Error> {
        let abs = self.state.resolve_virtual(&path)?;
        let virt = self.state.virtual_from_abs(&abs);
        Ok(Name {
            id,
            files: vec![SftpProtoFile::dummy(virt)],
        })
    }

    async fn opendir(&mut self, id: u32, path: SftpString) -> Result<Handle, Self::Error> {
        let abs = self.state.resolve_virtual(&path)?;
        let entries = fs::read_dir(&abs).map_err(|_| StatusCode::NoSuchFile)?;
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(meta) = entry.metadata() {
                files.push(SftpProtoFile::new(name, FileAttributes::from(&meta)));
            }
        }
        let handle = self.alloc_handle();
        self.handles.insert(
            handle.clone(),
            OpenHandle {
                file: None,
                is_write: false,
                dir_entries: Some(files),
            },
        );
        Ok(Handle {
            id,
            handle: handle.into(),
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
        let abs = self.state.resolve_virtual(&path)?;
        let meta = fs::metadata(&abs).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn lstat(&mut self, id: u32, path: SftpString) -> Result<Attrs, Self::Error> {
        let abs = self.state.resolve_virtual(&path)?;
        let meta = fs::symlink_metadata(&abs).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: SftpString,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let abs = self.state.resolve_virtual(&path)?;
        fs::create_dir(&abs).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: SftpString) -> Result<Status, Self::Error> {
        let abs = self.state.resolve_virtual(&path)?;
        fs::remove_dir(&abs).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: SftpString) -> Result<Status, Self::Error> {
        let abs = self.state.resolve_virtual(&filename)?;
        fs::remove_file(&abs).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Self::ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: SftpString,
        newpath: SftpString,
    ) -> Result<Status, Self::Error> {
        let src = self.state.resolve_virtual(&oldpath)?;
        let dst = self.state.resolve_virtual(&newpath)?;
        fs::rename(src, dst).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok_status(id))
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
            .fail_open_paths
            .lock()
            .unwrap()
            .contains(filename.as_str())
        {
            return Err(StatusCode::PermissionDenied);
        }
        let abs = self.state.resolve_virtual(&filename)?;
        let is_write = pflags.contains(OpenFlags::WRITE);
        let mut opts = OpenOptions::new();
        opts.read(pflags.contains(OpenFlags::READ));
        opts.write(is_write);
        opts.create(pflags.contains(OpenFlags::CREATE));
        opts.truncate(pflags.contains(OpenFlags::TRUNCATE));
        opts.append(pflags.contains(OpenFlags::APPEND));
        let file = opts.open(&abs).map_err(|_| StatusCode::NoSuchFile)?;

        if is_write {
            let current = self.state.active_write_handles.fetch_add(1, Ordering::SeqCst) + 1;
            self.state
                .max_concurrent_write_handles
                .fetch_max(current, Ordering::SeqCst);
        }

        let handle = self.alloc_handle();
        self.handles.insert(
            handle.clone(),
            OpenHandle {
                file: Some(file),
                is_write,
                dir_entries: None,
            },
        );
        Ok(Handle {
            id,
            handle: handle.into(),
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
        let delay = *self.state.write_delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        self.state
            .observed_write_chunk_sizes
            .lock()
            .unwrap()
            .push(data.len());
        let entry = self
            .handles
            .get_mut(handle.as_bytes())
            .ok_or(StatusCode::Failure)?;
        let file = entry.file.as_mut().ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        file.write_all(&data).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok_status(id))
    }

    async fn close(&mut self, id: u32, handle: SftpHandle) -> Result<Status, Self::Error> {
        if let Some(mut entry) = self.handles.remove(handle.as_bytes()) {
            if let Some(ref mut file) = entry.file {
                let _ = file.flush();
            }
            if entry.is_write {
                self.state
                    .active_write_handles
                    .fetch_sub(1, Ordering::SeqCst);
            }
        }
        Ok(Self::ok_status(id))
    }
}

struct TestSshConnHandler {
    conn_index: usize,
    state: Arc<SftpTestServerState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    shell_channels: BTreeSet<ChannelId>,
    kbi_stage: u8,
}

impl server::Handler for TestSshConnHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.state.auth_attempts.fetch_add(1, Ordering::SeqCst);
        match &self.state.auth_mode {
            ServerAuthMode::Password {
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
        match &self.state.auth_mode {
            ServerAuthMode::PublicKeyFirstThenKbi { .. } if self.conn_index == 0 => {
                Ok(server::Auth::Accept)
            }
            _ => Ok(server::Auth::reject()),
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: server::VerifiedPublicKey<'_>,
    ) -> Result<server::Auth, Self::Error> {
        self.state.auth_attempts.fetch_add(1, Ordering::SeqCst);
        match &self.state.auth_mode {
            ServerAuthMode::PublicKeyFirstThenKbi {
                username,
                allowed_key,
                ..
            } if self.conn_index == 0 && user == username && key.public_key == allowed_key => {
                Ok(server::Auth::Accept)
            }
            _ => Ok(server::Auth::reject()),
        }
    }

    async fn auth_keyboard_interactive(
        &mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'_>>,
    ) -> Result<server::Auth, Self::Error> {
        self.state.auth_attempts.fetch_add(1, Ordering::SeqCst);
        let (expected_user, expected_otp) = match &self.state.auth_mode {
            ServerAuthMode::KeyboardInteractive { username, otp_code } => (username, otp_code),
            ServerAuthMode::PublicKeyFirstThenKbi {
                username, otp_code, ..
            } => (username, otp_code),
            _ => return Ok(server::Auth::reject()),
        };
        if user != expected_user {
            return Ok(server::Auth::reject());
        }
        match (self.kbi_stage, response) {
            (0, None) => {
                self.kbi_stage = 1;
                self.state.kbi_prompts_issued.fetch_add(1, Ordering::SeqCst);
                Ok(server::Auth::Partial {
                    name: "2FA".into(),
                    instructions: "Enter OTP".into(),
                    prompts: vec![("OTP: ".into(), true)].into(),
                })
            }
            (1, Some(mut resp)) => {
                let code = resp.next().map(|b| String::from_utf8_lossy(&b).into_owned());
                if code.as_deref() == Some(expected_otp.as_str()) {
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
        self.shell_channels.insert(channel);
        session.channel_success(channel)?;
        session.data(channel, b"SHELL_READY\r\n".to_vec())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.shell_channels.contains(&channel) {
            let mut out = Vec::new();
            for &b in data {
                if b == b'\r' {
                    out.extend_from_slice(b"\r\n");
                } else {
                    out.push(b);
                }
            }
            session.data(channel, out)?;
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
            && let Some(channel) = self.channels.remove(&channel_id)
        {
            self.state
                .sftp_channels_opened
                .fetch_add(1, Ordering::SeqCst);
            session.channel_success(channel_id)?;
            let subsystem = TestSftpSubsystem::new(Arc::clone(&self.state));
            russh_sftp::server::run(channel.into_stream(), subsystem).await;
            return Ok(());
        }
        session.channel_failure(channel_id)?;
        Ok(())
    }
}

struct SpawnedSftpServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<SftpTestServerState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for SpawnedSftpServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_sftp_server(root_dir: PathBuf, auth_mode: ServerAuthMode) -> SpawnedSftpServer {
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

    let state = Arc::new(SftpTestServerState {
        root_dir,
        auth_mode,
        connection_count: AtomicUsize::new(0),
        auth_attempts: AtomicUsize::new(0),
        kbi_prompts_issued: AtomicUsize::new(0),
        sftp_channels_opened: AtomicUsize::new(0),
        active_write_handles: AtomicUsize::new(0),
        max_concurrent_write_handles: AtomicUsize::new(0),
        observed_write_chunk_sizes: Mutex::new(Vec::new()),
        write_delay: Mutex::new(None),
        fail_open_paths: Mutex::new(BTreeSet::new()),
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
                    let conn_index = srv_state.connection_count.fetch_add(1, Ordering::SeqCst);
                    let cfg = Arc::clone(&config);
                    let st = Arc::clone(&srv_state);
                    tokio::spawn(async move {
                        let handler = TestSshConnHandler {
                            conn_index,
                            state: st,
                            channels: HashMap::new(),
                            shell_channels: BTreeSet::new(),
                            kbi_stage: 0,
                        };
                        let _ = server::run_stream(cfg, stream, handler).await;
                    });
                }
            }
        }
    });

    SpawnedSftpServer {
        addr,
        host_pubkey,
        state,
        shutdown_tx: Some(shutdown_tx),
    }
}

struct TestInteraction {
    otp: Option<String>,
}

impl TestInteraction {
    fn tofu_only() -> Arc<Self> {
        Arc::new(Self { otp: None })
    }

    fn with_otp(otp: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            otp: Some(otp.into()),
        })
    }
}

impl SshInteractionHandler for TestInteraction {
    fn verify_unknown_host(&self, _prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async { HostKeyDecision::AcceptAndSave })
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
        let otp = self.otp.clone();
        Box::pin(async move { otp.map(|code| vec![Zeroizing::new(code)]) })
    }
}

fn make_vault(temp: &TempDir) -> Arc<VaultSession> {
    Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(temp.path().join("vault.enc")),
    ))
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
        ..SshConnectConfig::default()
    }
}

#[tokio::test]
async fn isolated_transfer_transport_opens_once_reuses_across_browser_ops_and_lists_sorted_dir() {
    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(remote_root.join("zeta_dir")).unwrap();
    fs::create_dir_all(remote_root.join("alpha_dir")).unwrap();
    fs::write(remote_root.join("notes.txt"), b"hello sftp").unwrap();
    fs::write(remote_root.join("archive.tar"), vec![42u8; 4096]).unwrap();

    let known_hosts = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    vault.set_secret("ssh/sftp_pw", "secret-pass").unwrap();

    let srv = spawn_sftp_server(
        remote_root.clone(),
        ServerAuthMode::Password {
            username: "ops".to_string(),
            password: "secret-pass".to_string(),
        },
    )
    .await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let cfg = connect_cfg(
        srv.addr,
        "ops",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/sftp_pw".to_string()),
        },
        &known_hosts,
    );
    let agent: Arc<dyn AgentConnector> = Arc::new(OsAgentConnector::default());
    let primary = SshSession::connect(
        cfg.clone(),
        Some(vault.as_ref()),
        TestInteraction::tofu_only(),
        Arc::clone(&agent),
    )
    .await
    .unwrap();
    assert_eq!(srv.state.connection_count.load(Ordering::SeqCst), 1);

    let key = ProfileTransportKey::from_connect_config(Some("prof-1".to_string()), &cfg);
    let endpoint = SftpEndpoint::new(key, cfg)
        .with_vault(vault)
        .with_agent_connector(agent)
        .with_primary_transport(Arc::clone(primary.primary_transport()));

    let sftp = SftpManager::new();
    let listing = sftp.list_dir(&endpoint, "/").await.unwrap();
    assert_eq!(listing.resolved_path, "/");
    assert_eq!(
        listing.transport_mode,
        SftpTransportMode::IsolatedTransferTransport
    );
    let summary: Vec<(&str, RemoteEntryKind, u64)> = listing
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind, e.size))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("alpha_dir", RemoteEntryKind::Dir, listing.entries[0].size),
            ("zeta_dir", RemoteEntryKind::Dir, listing.entries[1].size),
            ("archive.tar", RemoteEntryKind::File, 4096),
            ("notes.txt", RemoteEntryKind::File, 10),
        ]
    );

    // Subsequent browser mutations reuse the same dedicated Transfer Transport.
    sftp.mkdir(&endpoint, "/created_dir").await.unwrap();
    let meta = sftp.stat(&endpoint, "/created_dir").await.unwrap();
    assert_eq!(meta.kind, RemoteEntryKind::Dir);
    sftp.rename(&endpoint, "/notes.txt", "/notes_renamed.txt")
        .await
        .unwrap();
    sftp.remove_file(&endpoint, "/notes_renamed.txt")
        .await
        .unwrap();
    sftp.remove_dir(&endpoint, "/created_dir").await.unwrap();

    // Exactly 2 SSH transports opened total: 1 Primary + 1 reused Transfer Transport.
    assert_eq!(srv.state.connection_count.load(Ordering::SeqCst), 2);
    assert_eq!(srv.state.sftp_channels_opened.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn interactive_2fa_falls_back_to_primary_transport_sftp_mux_without_reprompting() {
    use russh::keys::ssh_key::LineEnding;

    // Case A: Primary Transport authenticated with KeyboardInteractive (2FA/OTP once).
    let temp_a = TempDir::new().unwrap();
    let root_a = temp_a.path().join("remote_a");
    fs::create_dir_all(&root_a).unwrap();
    fs::write(root_a.join("2fa_only.txt"), b"ok").unwrap();
    let kh_a = temp_a.path().join("known_hosts");

    let srv_a = spawn_sftp_server(
        root_a,
        ServerAuthMode::KeyboardInteractive {
            username: "secops".to_string(),
            otp_code: "654321".to_string(),
        },
    )
    .await;
    record_known_host_path("127.0.0.1", srv_a.addr.port(), &srv_a.host_pubkey, &kh_a).unwrap();

    let cfg_a = connect_cfg(srv_a.addr, "secops", SshAuthMethod::KeyboardInteractive, &kh_a);
    let agent: Arc<dyn AgentConnector> = Arc::new(OsAgentConnector::default());
    let primary_a = SshSession::connect(
        cfg_a.clone(),
        None,
        TestInteraction::with_otp("654321"),
        Arc::clone(&agent),
    )
    .await
    .unwrap();
    assert!(primary_a.used_interactive_auth());
    assert_eq!(srv_a.state.kbi_prompts_issued.load(Ordering::SeqCst), 1);

    let key_a = ProfileTransportKey::from_connect_config(Some("prof-2fa".to_string()), &cfg_a);
    let endpoint_a = SftpEndpoint::new(key_a.clone(), cfg_a)
        .with_agent_connector(Arc::clone(&agent))
        .with_primary_transport(Arc::clone(primary_a.primary_transport()));

    let sftp_a = SftpManager::new();
    let listing_a = sftp_a.list_dir(&endpoint_a, "/").await.unwrap();
    assert_eq!(listing_a.transport_mode, SftpTransportMode::MuxFallback);
    assert!(listing_a.notice.as_deref().is_some_and(|n| !n.is_empty()));
    assert_eq!(
        listing_a
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["2fa_only.txt"]
    );
    // Zero second connection or second OTP challenge issued!
    assert_eq!(srv_a.state.connection_count.load(Ordering::SeqCst), 1);
    assert_eq!(srv_a.state.kbi_prompts_issued.load(Ordering::SeqCst), 1);
    assert_eq!(
        sftp_a.status_notice(&key_a).unwrap().mode,
        SftpTransportMode::MuxFallback
    );

    // Case B: Primary uses PublicKey, server blocks non-2FA on secondary connection -> falls back.
    let temp_b = TempDir::new().unwrap();
    let root_b = temp_b.path().join("remote_b");
    fs::create_dir_all(&root_b).unwrap();
    fs::write(root_b.join("fallback.bin"), b"mux").unwrap();
    let kh_b = temp_b.path().join("known_hosts");

    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_path = temp_b.path().join("id_ed25519");
    fs::write(&key_path, client_key.to_openssh(LineEnding::LF).unwrap()).unwrap();

    let srv_b = spawn_sftp_server(
        root_b,
        ServerAuthMode::PublicKeyFirstThenKbi {
            username: "deploy".to_string(),
            allowed_key: client_key.public_key().clone(),
            otp_code: "999111".to_string(),
        },
    )
    .await;
    record_known_host_path("127.0.0.1", srv_b.addr.port(), &srv_b.host_pubkey, &kh_b).unwrap();

    let cfg_b = connect_cfg(
        srv_b.addr,
        "deploy",
        SshAuthMethod::PublicKey {
            key_path: Some(key_path),
            vault_secret_ref: None,
        },
        &kh_b,
    );
    let primary_b = SshSession::connect(
        cfg_b.clone(),
        None,
        TestInteraction::tofu_only(),
        Arc::clone(&agent),
    )
    .await
    .unwrap();
    assert!(!primary_b.used_interactive_auth());

    let key_b = ProfileTransportKey::from_connect_config(Some("prof-pub-2fa".to_string()), &cfg_b);
    let endpoint_b = SftpEndpoint::new(key_b, cfg_b)
        .with_agent_connector(agent)
        .with_primary_transport(Arc::clone(primary_b.primary_transport()));

    let sftp_b = SftpManager::new();
    let listing_b = sftp_b.list_dir(&endpoint_b, "/").await.unwrap();
    assert_eq!(listing_b.transport_mode, SftpTransportMode::MuxFallback);
    assert!(listing_b.notice.as_deref().is_some_and(|n| !n.is_empty()));
    assert_eq!(listing_b.entries[0].name, "fallback.bin");

    // Subsequent ops reuse the mux fallback SFTP channel without reopening secondary connections.
    sftp_b.mkdir(&endpoint_b, "/sub").await.unwrap();
    assert_eq!(srv_b.state.connection_count.load(Ordering::SeqCst), 2);
    assert_eq!(srv_b.state.sftp_channels_opened.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transfer_queue_streams_pipelined_chunks_caps_concurrency_at_three_and_downloads_match_bytes() {
    assert_eq!(DEFAULT_MAX_CONCURRENT_TRANSFERS, 3);
    assert!((128 * 1024..=256 * 1024).contains(&DEFAULT_TRANSFER_CHUNK_SIZE));

    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let download_dir = temp.path().join("downloads");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&download_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let known_hosts = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    vault.set_secret("ssh/xfer_pw", "pipe-secret").unwrap();

    let srv = spawn_sftp_server(
        remote_root.clone(),
        ServerAuthMode::Password {
            username: "uploader".to_string(),
            password: "pipe-secret".to_string(),
        },
    )
    .await;
    // Slight write delay makes concurrent active write handles deterministically observable.
    *srv.state.write_delay.lock().unwrap() = Some(Duration::from_millis(8));
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let cfg = connect_cfg(
        srv.addr,
        "uploader",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/xfer_pw".to_string()),
        },
        &known_hosts,
    );
    let key = ProfileTransportKey::from_connect_config(Some("prof-xfer".to_string()), &cfg);
    let endpoint = SftpEndpoint::new(key, cfg)
        .with_vault(vault)
        .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let sftp = SftpManager::new();
    let queue = TransferQueue::with_config(sftp, TransferQueueConfig::default());

    let mut payloads = Vec::new();
    let mut upload_ids = Vec::new();
    let file_size = 1_572_864usize; // 1.5 MiB each (6 * 256 KiB chunks)
    for idx in 0..5usize {
        let local_path = local_dir.join(format!("blob_{idx}.bin"));
        let data: Vec<u8> = (0..file_size)
            .map(|off| ((off * 31 + idx * 17) % 251) as u8)
            .collect();
        fs::write(&local_path, &data).unwrap();
        payloads.push(data);
        let tid = queue.enqueue_upload(
            endpoint.clone(),
            local_path,
            format!("/blob_{idx}.bin"),
        );
        upload_ids.push(tid);
    }

    assert!(
        queue.wait_until_idle(Duration::from_secs(20)).await,
        "uploads did not finish in time: {:?}",
        queue.snapshot()
    );

    let snap = queue.snapshot();
    assert_eq!(snap.completed_count, 5);
    assert_eq!(snap.failed_count, 0);
    for (idx, tid) in upload_ids.iter().enumerate() {
        let item = queue.get(*tid).unwrap();
        assert_eq!(item.direction, TransferDirection::Upload);
        assert_eq!(item.status, TransferStatus::Completed);
        assert_eq!(item.bytes_transferred, file_size as u64);
        assert_eq!(item.total_bytes, file_size as u64);
        assert_eq!(
            item.transport_mode,
            Some(SftpTransportMode::IsolatedTransferTransport)
        );
        assert!(item.throughput_bytes_per_sec > 0);
        assert_eq!(
            fs::read(remote_root.join(format!("blob_{idx}.bin"))).unwrap(),
            payloads[idx]
        );
    }

    // Concurrency strictly capped at 3 simultaneous active writes, with real parallelism (> 1).
    let max_concurrent = srv
        .state
        .max_concurrent_write_handles
        .load(Ordering::SeqCst);
    assert!(
        (2..=3).contains(&max_concurrent),
        "expected 2..=3 concurrent transfers, observed {max_concurrent}"
    );

    // Chunk sizes must reflect 128-256 KiB pipelined packets rather than 8 KiB / 32 KiB defaults.
    let max_chunk = srv
        .state
        .observed_write_chunk_sizes
        .lock()
        .unwrap()
        .iter()
        .copied()
        .max()
        .unwrap_or(0);
    assert!(
        max_chunk >= 131_072,
        "expected pipelined SFTP write chunks >= 128 KiB, got {max_chunk}"
    );

    // Download one file back and verify byte-for-byte parity.
    let dl_path = download_dir.join("blob_3.bin");
    let dl_id = queue.enqueue_download(endpoint, "/blob_3.bin".to_string(), dl_path.clone());
    assert!(queue.wait_until_idle(Duration::from_secs(10)).await);
    let dl_item = queue.get(dl_id).unwrap();
    assert_eq!(dl_item.direction, TransferDirection::Download);
    assert_eq!(dl_item.status, TransferStatus::Completed);
    assert_eq!(dl_item.bytes_transferred, file_size as u64);
    assert_eq!(fs::read(&dl_path).unwrap(), payloads[3]);

    // All 6 transfers reused the single Transfer Transport!
    assert_eq!(srv.state.connection_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transfer_queue_supports_independent_pause_resume_cancel_and_retry() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let known_hosts = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    vault.set_secret("ssh/ctl_pw", "ctl-secret").unwrap();

    let srv = spawn_sftp_server(
        remote_root.clone(),
        ServerAuthMode::Password {
            username: "ops".to_string(),
            password: "ctl-secret".to_string(),
        },
    )
    .await;
    // Moderate write delay so pause/cancel reliably catch transfers in-flight.
    *srv.state.write_delay.lock().unwrap() = Some(Duration::from_millis(25));
    srv.state
        .fail_open_paths
        .lock()
        .unwrap()
        .insert("/retry_target.bin".to_string());
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let cfg = connect_cfg(
        srv.addr,
        "ops",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/ctl_pw".to_string()),
        },
        &known_hosts,
    );
    let key = ProfileTransportKey::from_connect_config(Some("prof-ctl".to_string()), &cfg);
    let endpoint = SftpEndpoint::new(key, cfg)
        .with_vault(vault)
        .with_agent_connector(Arc::new(OsAgentConnector::default()));

    let queue = TransferQueue::new(SftpManager::new());

    let size = 2 * 1024 * 1024usize; // 2 MiB (8 x 256 KiB chunks)
    let data_pause: Vec<u8> = (0..size).map(|i| ((i * 7) % 253) as u8).collect();
    let data_cancel: Vec<u8> = (0..size).map(|i| ((i * 11) % 251) as u8).collect();
    let data_retry: Vec<u8> = vec![0xAB; 512 * 1024];

    let p_pause = local_dir.join("pause.bin");
    let p_cancel = local_dir.join("cancel.bin");
    let p_retry = local_dir.join("retry.bin");
    fs::write(&p_pause, &data_pause).unwrap();
    fs::write(&p_cancel, &data_cancel).unwrap();
    fs::write(&p_retry, &data_retry).unwrap();

    let id_pause = queue.enqueue_upload(endpoint.clone(), p_pause, "/pause_target.bin".to_string());
    let id_cancel =
        queue.enqueue_upload(endpoint.clone(), p_cancel, "/cancel_target.bin".to_string());
    let id_retry = queue.enqueue_upload(endpoint, p_retry, "/retry_target.bin".to_string());

    // Wait until at least one chunk has progressed on pause/cancel transfers, or failure on retry.
    let mut rx = queue.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snap = rx.borrow().clone();
            let pause_started = snap
                .transfers
                .iter()
                .find(|t| t.id == id_pause)
                .is_some_and(|t| t.bytes_transferred > 0);
            let retry_failed = snap
                .transfers
                .iter()
                .find(|t| t.id == id_retry)
                .is_some_and(|t| matches!(t.status, TransferStatus::Failed { .. }));
            if pause_started && retry_failed {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await
    .expect("timed out waiting for active upload progress & injected failure");

    assert!(queue.pause(id_pause));
    assert!(queue.cancel(id_cancel));

    // Wait until queue settles (retry failed, cancel cancelled, pause paused).
    assert!(queue.wait_until_idle(Duration::from_secs(5)).await);

    let paused_snap = queue.get(id_pause).unwrap();
    assert_eq!(paused_snap.status, TransferStatus::Paused);
    assert!(paused_snap.bytes_transferred > 0);
    assert!(paused_snap.bytes_transferred < size as u64);

    let cancelled_snap = queue.get(id_cancel).unwrap();
    assert_eq!(cancelled_snap.status, TransferStatus::Cancelled);

    assert!(matches!(
        queue.get(id_retry).unwrap().status,
        TransferStatus::Failed { .. }
    ));

    // Speed up writes, lift injected permission failure, then resume paused & retry failed!
    *srv.state.write_delay.lock().unwrap() = None;
    srv.state
        .fail_open_paths
        .lock()
        .unwrap()
        .remove("/retry_target.bin");

    assert!(queue.resume(id_pause));
    assert!(queue.retry(id_retry));
    assert!(!queue.resume(id_cancel)); // Cancelled cannot be resumed (only retried)

    assert!(queue.wait_until_idle(Duration::from_secs(10)).await);

    let resumed = queue.get(id_pause).unwrap();
    assert_eq!(resumed.status, TransferStatus::Completed);
    assert_eq!(resumed.bytes_transferred, size as u64);
    assert_eq!(fs::read(remote_root.join("pause_target.bin")).unwrap(), data_pause);

    let retried = queue.get(id_retry).unwrap();
    assert_eq!(retried.status, TransferStatus::Completed);
    assert_eq!(fs::read(remote_root.join("retry_target.bin")).unwrap(), data_retry);

    // Cancelled transfer stayed Cancelled while others completed.
    assert_eq!(queue.get(id_cancel).unwrap().status, TransferStatus::Cancelled);
}

#[tokio::test]
async fn concurrent_multi_mb_transfers_do_not_starve_terminal_ssh_session() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let known_hosts = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    vault.set_secret("ssh/term_pw", "term-secret").unwrap();

    let srv = spawn_sftp_server(
        remote_root.clone(),
        ServerAuthMode::Password {
            username: "ops".to_string(),
            password: "term-secret".to_string(),
        },
    )
    .await;
    *srv.state.write_delay.lock().unwrap() = Some(Duration::from_millis(4));
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let cfg = connect_cfg(
        srv.addr,
        "ops",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/term_pw".to_string()),
        },
        &known_hosts,
    );
    let agent: Arc<dyn AgentConnector> = Arc::new(OsAgentConnector::default());
    let session = SshSession::connect(
        cfg.clone(),
        Some(vault.as_ref()),
        TestInteraction::tofu_only(),
        Arc::clone(&agent),
    )
    .await
    .unwrap();
    assert!(
        session
            .wait_for_output_async(Duration::from_secs(5), |s| {
                s.visible_text().contains("SHELL_READY")
            })
            .await
    );

    let key = ProfileTransportKey::from_connect_config(Some("prof-nonblock".to_string()), &cfg);
    let endpoint = SftpEndpoint::new(key, cfg)
        .with_vault(vault)
        .with_agent_connector(agent)
        .with_primary_transport(Arc::clone(session.primary_transport()));

    let queue = TransferQueue::new(SftpManager::new());

    // Start 3 concurrent 3 MiB uploads (9 MiB total) on Transfer Transport.
    let size = 3 * 1024 * 1024usize;
    for idx in 0..3usize {
        let local_path = local_dir.join(format!("heavy_{idx}.bin"));
        fs::write(&local_path, vec![(idx + 1) as u8; size]).unwrap();
        queue.enqueue_upload(endpoint.clone(), local_path, format!("/heavy_{idx}.bin"));
    }

    // Wait until transfers are actively streaming.
    let mut rx = queue.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if rx.borrow().active_count >= 2 {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await
    .expect("expected active concurrent transfers");

    // Interactive shell keystrokes on Primary Transport must respond rapidly without blocking.
    for probe in 1..=3 {
        let marker = format!("PING_NONBLOCK_{probe}");
        let cmd = format!("{marker}\r");
        let start = std::time::Instant::now();
        session.write_bytes(cmd.as_bytes()).unwrap();
        assert!(
            session
                .wait_for_output_async(Duration::from_millis(500), |s| {
                    s.visible_text().contains(&marker)
                })
                .await,
            "interactive terminal starved during heavy SFTP transfers on {marker} (elapsed {:?})",
            start.elapsed()
        );
    }

    assert!(queue.wait_until_idle(Duration::from_secs(20)).await);
    assert_eq!(queue.snapshot().completed_count, 3);
    assert_eq!(srv.state.connection_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sftp_manager_text_edit_create_copy_and_recursive_rmdir() {
    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(remote_root.join("nested/sub")).unwrap();
    fs::write(remote_root.join("nested/a.txt"), b"alpha").unwrap();
    fs::write(remote_root.join("nested/sub/b.txt"), b"beta").unwrap();
    fs::write(remote_root.join("binary.bin"), b"\x00\x01\x02\x03").unwrap();

    let known_hosts = temp.path().join("known_hosts");
    let vault = make_vault(&temp);
    vault.set_secret("ssh/plus_pw", "secret-pass").unwrap();

    let srv = spawn_sftp_server(
        remote_root.clone(),
        ServerAuthMode::Password {
            username: "ops".to_string(),
            password: "secret-pass".to_string(),
        },
    )
    .await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let cfg = connect_cfg(
        srv.addr,
        "ops",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/plus_pw".to_string()),
        },
        &known_hosts,
    );
    let agent: Arc<dyn AgentConnector> = Arc::new(OsAgentConnector::default());
    let primary = SshSession::connect(
        cfg.clone(),
        Some(vault.as_ref()),
        TestInteraction::tofu_only(),
        Arc::clone(&agent),
    )
    .await
    .unwrap();

    let key = ProfileTransportKey::from_connect_config(Some("prof-plus".to_string()), &cfg);
    let endpoint = SftpEndpoint::new(key, cfg)
        .with_vault(vault)
        .with_agent_connector(agent)
        .with_primary_transport(Arc::clone(primary.primary_transport()));

    let sftp = SftpManager::new();

    sftp.create_empty_file(&endpoint, "/empty.conf")
        .await
        .unwrap();
    assert_eq!(sftp.read_text_file(&endpoint, "/empty.conf", 65536).await.unwrap(), "");

    sftp.write_text_file(&endpoint, "/empty.conf", "port=8080\nmode=prod\n")
        .await
        .unwrap();
    assert_eq!(
        sftp.read_text_file(&endpoint, "/empty.conf", 65536).await.unwrap(),
        "port=8080\nmode=prod\n"
    );

    sftp.copy_file(&endpoint, "/empty.conf", "/empty.copy.conf")
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(remote_root.join("empty.copy.conf")).unwrap(),
        "port=8080\nmode=prod\n"
    );

    // Binary and oversized safeguards must reject cleanly.
    let bin_err = sftp.read_text_file(&endpoint, "/binary.bin", 65536).await;
    assert!(bin_err.is_err());
    let limit_err = sftp.read_text_file(&endpoint, "/empty.conf", 4).await;
    assert!(limit_err.is_err());

    // Recursive directory deletion removes nested subdirectories and files.
    assert!(remote_root.join("nested/sub/b.txt").exists());
    sftp.remove_dir_recursive(&endpoint, "/nested")
        .await
        .unwrap();
    assert!(!remote_root.join("nested").exists());
}
