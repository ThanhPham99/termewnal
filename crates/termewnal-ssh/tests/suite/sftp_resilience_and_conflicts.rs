use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Seek, SeekFrom, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use russh_sftp::protocol::{
    Attrs, Data, File as SftpProtoFile, FileAttributes, Handle, Name, OpenFlags, SftpHandle,
    SftpString, Status, StatusCode, Version,
};
use tempfile::TempDir;
use termewnal_core::{
    SshAuthMethod, TransferConfig, TransferConflictPolicy, VaultBackend, VaultConfig,
    vault::{EncryptedVault, InMemorySecretStore, SecretStore, VaultSession},
};
use termewnal_ssh::{
    ConflictResolution, OsAgentConnector, ProfileTransportKey, SftpEndpoint, SftpManager,
    SshConnectConfig, TransferDirection, TransferNotice, TransferQueue, TransferQueueConfig,
    TransferStatus, record_known_host_path,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

#[derive(Debug, Clone)]
struct RecordedWrite {
    conn_index: usize,
    path: String,
    offset: u64,
    len: usize,
}

struct FaultServerState {
    root_dir: PathBuf,
    username: String,
    password: String,
    connection_count: AtomicUsize,
    recorded_writes: Mutex<Vec<RecordedWrite>>,
    /// When `Some(limit)`, drops the active SSH TCP connection once cumulative `.part` writes
    /// on the triggering connection reach `limit` (only triggered once).
    drop_conn_after_part_bytes: Mutex<Option<u64>>,
    /// When `Some(limit)`, drops the active SSH TCP connection once cumulative reads
    /// on the triggering connection reach `limit` (only triggered once).
    drop_conn_after_read_bytes: Mutex<Option<u64>>,
    drop_conn_on_rename: AtomicBool,
    drop_conn_on_mkdir: AtomicBool,
    dropped_once: AtomicBool,
    deny_open_exact: Mutex<BTreeSet<String>>,
    deny_rename_paths: Mutex<BTreeSet<String>>,
    deny_rename_status: Mutex<StatusCode>,
    deny_setstat_paths: Mutex<BTreeSet<String>>,
    reject_rename_if_target_exists: AtomicBool,
    injected_dir_entries: Mutex<HashMap<String, Vec<String>>>,
}

impl FaultServerState {
    fn new(root_dir: PathBuf, username: &str, password: &str) -> Self {
        Self {
            root_dir,
            username: username.to_string(),
            password: password.to_string(),
            connection_count: AtomicUsize::new(0),
            recorded_writes: Mutex::new(Vec::new()),
            drop_conn_after_part_bytes: Mutex::new(None),
            drop_conn_after_read_bytes: Mutex::new(None),
            drop_conn_on_rename: AtomicBool::new(false),
            drop_conn_on_mkdir: AtomicBool::new(false),
            dropped_once: AtomicBool::new(false),
            deny_open_exact: Mutex::new(BTreeSet::new()),
            deny_rename_paths: Mutex::new(BTreeSet::new()),
            deny_rename_status: Mutex::new(StatusCode::PermissionDenied),
            deny_setstat_paths: Mutex::new(BTreeSet::new()),
            reject_rename_if_target_exists: AtomicBool::new(false),
            injected_dir_entries: Mutex::new(HashMap::new()),
        }
    }

    fn resolve_virtual(&self, raw: &str) -> Result<PathBuf, StatusCode> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "." || trimmed == "/" {
            return Ok(self.root_dir.clone());
        }
        let mut clean = PathBuf::new();
        for comp in Path::new(trimmed.trim_start_matches('/')).components() {
            match comp {
                std::path::Component::Normal(part) => clean.push(part),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
            }
        }
        Ok(self.root_dir.join(clean))
    }

    fn virtual_from_abs(&self, abs: &Path) -> String {
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

struct OpenHandle {
    path: String,
    file: Option<std::fs::File>,
    dir_entries: Option<Vec<SftpProtoFile>>,
}

struct FaultSftpSubsystem {
    conn_index: usize,
    state: Arc<FaultServerState>,
    kill_switch: Arc<TcpKillSwitch>,
    conn_part_bytes_written: Arc<AtomicU64>,
    conn_bytes_read: Arc<AtomicU64>,
    next_handle: u64,
    handles: HashMap<Vec<u8>, OpenHandle>,
}

impl FaultSftpSubsystem {
    fn new(
        conn_index: usize,
        state: Arc<FaultServerState>,
        kill_switch: Arc<TcpKillSwitch>,
    ) -> Self {
        Self {
            conn_index,
            state,
            kill_switch,
            conn_part_bytes_written: Arc::new(AtomicU64::new(0)),
            conn_bytes_read: Arc::new(AtomicU64::new(0)),
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

    fn trigger_tcp_drop(&self) {
        self.kill_switch.kill();
    }
}

impl russh_sftp::server::Handler for FaultSftpSubsystem {
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
        let abs = self.state.resolve_virtual(&path)?;
        let canon = fs::canonicalize(&abs).unwrap_or(abs);
        let virt = self.state.virtual_from_abs(&canon);
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
            if let Ok(meta) = fs::symlink_metadata(entry.path()) {
                files.push(SftpProtoFile::new(name, FileAttributes::from(&meta)));
            }
        }
        if let Some(injected) = self
            .state
            .injected_dir_entries
            .lock()
            .unwrap()
            .get(path.as_str())
        {
            for evil_name in injected {
                let mut attrs = FileAttributes::default();
                attrs.size = Some(11);
                attrs.permissions = Some(0o100644);
                files.push(SftpProtoFile::new(evil_name.clone(), attrs));
            }
        }
        let handle = self.alloc_handle();
        self.handles.insert(
            handle.clone(),
            OpenHandle {
                path: path.to_string(),
                file: None,
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

    async fn readlink(&mut self, id: u32, path: SftpString) -> Result<Name, Self::Error> {
        let abs = self.state.resolve_virtual(&path)?;
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
        if self
            .state
            .deny_setstat_paths
            .lock()
            .unwrap()
            .contains(path.as_str())
        {
            return Err(StatusCode::PermissionDenied);
        }
        let abs = self.state.resolve_virtual(&path)?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&abs, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|_| StatusCode::PermissionDenied)?;
        }
        #[cfg(not(unix))]
        let _ = (abs, attrs);
        Ok(Self::ok_status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: SftpString,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        if self.state.drop_conn_on_mkdir.swap(false, Ordering::SeqCst) {
            self.trigger_tcp_drop();
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(StatusCode::ConnectionLost);
        }
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
        if self.state.drop_conn_on_rename.swap(false, Ordering::SeqCst) {
            self.trigger_tcp_drop();
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(StatusCode::ConnectionLost);
        }
        if self
            .state
            .deny_rename_paths
            .lock()
            .unwrap()
            .contains(oldpath.as_str())
        {
            return Err(*self.state.deny_rename_status.lock().unwrap());
        }
        let src = self.state.resolve_virtual(&oldpath)?;
        let dst = self.state.resolve_virtual(&newpath)?;
        if self
            .state
            .reject_rename_if_target_exists
            .load(Ordering::SeqCst)
            && dst.exists()
        {
            return Err(StatusCode::Failure);
        }
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
            .deny_open_exact
            .lock()
            .unwrap()
            .contains(filename.as_str())
        {
            return Err(StatusCode::PermissionDenied);
        }
        let abs = self.state.resolve_virtual(&filename)?;
        let mut opts = OpenOptions::new();
        opts.read(pflags.contains(OpenFlags::READ));
        opts.write(pflags.contains(OpenFlags::WRITE));
        opts.create(pflags.contains(OpenFlags::CREATE));
        opts.truncate(pflags.contains(OpenFlags::TRUNCATE));
        opts.append(pflags.contains(OpenFlags::APPEND));
        let file = opts.open(&abs).map_err(|_| StatusCode::NoSuchFile)?;
        let handle = self.alloc_handle();
        self.handles.insert(
            handle.clone(),
            OpenHandle {
                path: filename.to_string(),
                file: Some(file),
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
        let read_limit = *self.state.drop_conn_after_read_bytes.lock().unwrap();
        if let Some(limit_bytes) = read_limit
            && self.conn_bytes_read.load(Ordering::SeqCst) >= limit_bytes
            && !self.state.dropped_once.swap(true, Ordering::SeqCst)
        {
            self.trigger_tcp_drop();
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(StatusCode::ConnectionLost);
        }
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
        self.conn_bytes_read.fetch_add(n as u64, Ordering::SeqCst);
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
        let path = {
            let entry = self
                .handles
                .get(handle.as_bytes())
                .ok_or(StatusCode::Failure)?;
            entry.path.clone()
        };

        if path.ends_with(".termewnal.part") {
            let limit = *self.state.drop_conn_after_part_bytes.lock().unwrap();
            if let Some(limit_bytes) = limit
                && self.conn_part_bytes_written.load(Ordering::SeqCst) >= limit_bytes
                && !self.state.dropped_once.swap(true, Ordering::SeqCst)
            {
                self.trigger_tcp_drop();
                tokio::time::sleep(Duration::from_millis(50)).await;
                return Err(StatusCode::ConnectionLost);
            }
        }

        let len = data.len();
        {
            let entry = self
                .handles
                .get_mut(handle.as_bytes())
                .ok_or(StatusCode::Failure)?;
            let file = entry.file.as_mut().ok_or(StatusCode::Failure)?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|_| StatusCode::Failure)?;
            file.write_all(&data).map_err(|_| StatusCode::Failure)?;
            file.flush().map_err(|_| StatusCode::Failure)?;
        }

        self.state.recorded_writes.lock().unwrap().push(RecordedWrite {
            conn_index: self.conn_index,
            path: path.clone(),
            offset,
            len,
        });

        if path.ends_with(".termewnal.part") {
            self.conn_part_bytes_written
                .fetch_add(len as u64, Ordering::SeqCst);
        }

        Ok(Self::ok_status(id))
    }

    async fn close(&mut self, id: u32, handle: SftpHandle) -> Result<Status, Self::Error> {
        if let Some(mut entry) = self.handles.remove(handle.as_bytes())
            && let Some(ref mut file) = entry.file
        {
            let _ = file.flush();
        }
        Ok(Self::ok_status(id))
    }
}

#[derive(Default)]
struct TcpKillSwitch {
    killed: AtomicBool,
    notify: Notify,
}

impl TcpKillSwitch {
    fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }
}

struct KillableStream<S> {
    inner: Option<S>,
    kill: Arc<TcpKillSwitch>,
}

impl<S> KillableStream<S> {
    fn new(inner: S, kill: Arc<TcpKillSwitch>) -> Self {
        Self {
            inner: Some(inner),
            kill,
        }
    }

    fn check_alive(&mut self) -> io::Result<&mut S> {
        if self.kill.is_killed() {
            // Drop the underlying TcpStream immediately so client observes real TCP RST/EOF.
            self.inner.take();
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected mid-transfer TCP drop",
            ));
        }
        self.inner.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected mid-transfer TCP drop",
            )
        })
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for KillableStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.kill.is_killed() {
            self.inner.take();
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected mid-transfer TCP drop",
            )));
        }
        let kill = Arc::clone(&self.kill);
        let pinned_notify = kill.notify.notified();
        tokio::pin!(pinned_notify);
        let _ = pinned_notify.as_mut().poll(cx);
        match self.inner.as_mut() {
            Some(stream) => Pin::new(stream).poll_read(cx, buf),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected mid-transfer TCP drop",
            ))),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for KillableStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let stream = match self.check_alive() {
            Ok(s) => s,
            Err(e) => return Poll::Ready(Err(e)),
        };
        Pin::new(stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = match self.check_alive() {
            Ok(s) => s,
            Err(e) => return Poll::Ready(Err(e)),
        };
        Pin::new(stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = match self.check_alive() {
            Ok(s) => s,
            Err(e) => return Poll::Ready(Err(e)),
        };
        Pin::new(stream).poll_shutdown(cx)
    }
}

struct FaultSshHandler {
    conn_index: usize,
    state: Arc<FaultServerState>,
    kill_switch: Arc<TcpKillSwitch>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
}

impl server::Handler for FaultSshHandler {
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
            let subsystem = FaultSftpSubsystem::new(
                self.conn_index,
                Arc::clone(&self.state),
                Arc::clone(&self.kill_switch),
            );
            russh_sftp::server::run(ch.into_stream(), subsystem).await;
            return Ok(());
        }
        session.channel_failure(channel_id)?;
        Ok(())
    }
}

struct FaultServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<FaultServerState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for FaultServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_fault_server(root_dir: PathBuf, user: &str, password: &str) -> FaultServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();
    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

    let state = Arc::new(FaultServerState::new(root_dir, user, password));
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
                    let cfg = Arc::clone(&cfg);
                    let st = Arc::clone(&srv_state);
                    tokio::spawn(async move {
                        let kill_switch = Arc::new(TcpKillSwitch::default());
                        let killable = KillableStream::new(stream, Arc::clone(&kill_switch));
                        let handler = FaultSshHandler {
                            conn_index,
                            state: st,
                            kill_switch,
                            channels: HashMap::new(),
                        };
                        let _ = server::run_stream(cfg, killable, handler).await;
                    });
                }
            }
        }
    });

    FaultServer {
        addr,
        host_pubkey,
        state,
        shutdown_tx: Some(shutdown_tx),
    }
}

fn setup_endpoint(temp: &TempDir, srv: &FaultServer) -> SftpEndpoint {
    let kh = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &kh).unwrap();

    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(temp.path().join("vault.enc")),
    ));
    vault
        .set_secret("ssh/fault_pw", &srv.state.password)
        .unwrap();

    let cfg = SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port: srv.addr.port(),
        username: srv.state.username.clone(),
        auth: SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/fault_pw".to_string()),
        },
        known_hosts_path: Some(kh),
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 10_000,
        proxy_jump: Vec::new(),
        ..SshConnectConfig::default()
    };
    let key = ProfileTransportKey::from_connect_config(Some("prof-fault".to_string()), &cfg);
    SftpEndpoint::new(key, cfg)
        .with_vault(vault)
        .with_agent_connector(Arc::new(OsAgentConnector::default()))
}

#[tokio::test]
async fn slice2_part_atomic_upload_and_mid_upload_tcp_drop_resume_from_verified_offset() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    // Inject mid-upload TCP drop after >= 512 KiB reaches `.termewnal.part`.
    *srv.state.drop_conn_after_part_bytes.lock().unwrap() = Some(512 * 1024);

    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::with_config(
        SftpManager::new(),
        TransferQueueConfig {
            transfer: TransferConfig {
                default_conflict_policy: TransferConflictPolicy::Ask,
                follow_symlinks: false,
                part_threshold_bytes: 1_048_576,
            },
            ..TransferQueueConfig::default()
        },
    );

    // 1. Upload > 1 MiB file (2 MiB) -> writes `.termewnal.part`, drops TCP, resumes from exact remote byte offset.
    let big_size = 2 * 1024 * 1024usize;
    let big_payload: Vec<u8> = (0..big_size).map(|i| ((i * 29 + 7) % 251) as u8).collect();
    let big_local = local_dir.join("big_upload.bin");
    fs::write(&big_local, &big_payload).unwrap();

    let big_id = queue.enqueue_upload(endpoint.clone(), big_local, "/big_upload.bin".to_string());
    assert!(
        queue.wait_until_idle(Duration::from_secs(20)).await,
        "resumed upload did not settle: {:?}",
        queue.snapshot()
    );

    let big_snap = queue.get(big_id).unwrap();
    assert_eq!(big_snap.status, TransferStatus::Completed);
    assert!(big_snap.using_part_file);
    assert!(
        big_snap.resume_offset >= 512 * 1024,
        "expected verified resume offset >= 512 KiB after TCP drop, got {}",
        big_snap.resume_offset
    );
    assert_eq!(big_snap.bytes_transferred, big_size as u64);
    assert_eq!(fs::read(remote_root.join("big_upload.bin")).unwrap(), big_payload);
    assert!(
        !remote_root.join("big_upload.bin.termewnal.part").exists(),
        "atomic rename must remove .termewnal.part after completion"
    );

    // Verify completed bytes before TCP drop were NEVER re-uploaded on connection #1+.
    let writes = srv.state.recorded_writes.lock().unwrap().clone();
    assert!(
        writes
            .iter()
            .all(|w| w.path == "/big_upload.bin.termewnal.part"),
        "expected >1MB upload to write exclusively to .termewnal.part before rename: {writes:?}"
    );
    let conn0_max_end = writes
        .iter()
        .filter(|w| w.conn_index == 0)
        .map(|w| w.offset + w.len as u64)
        .max()
        .unwrap_or(0);
    let conn1_min_offset = writes
        .iter()
        .filter(|w| w.conn_index >= 1)
        .map(|w| w.offset)
        .min()
        .expect("expected resumed writes on reconnected connection");
    assert_eq!(
        conn1_min_offset, conn0_max_end,
        "resumed writes must begin at exact verified remote offset without re-uploading completed bytes"
    );
    assert_eq!(conn1_min_offset, big_snap.resume_offset);

    // 2. Upload <= 1 MiB exact (1_048_576 bytes) writes directly without `.termewnal.part`.
    srv.state.recorded_writes.lock().unwrap().clear();
    let exact_1mb = vec![0x3Cu8; 1_048_576];
    let small_local = local_dir.join("exact_1mb.bin");
    fs::write(&small_local, &exact_1mb).unwrap();
    let small_id = queue.enqueue_upload(endpoint, small_local, "/exact_1mb.bin".to_string());
    assert!(queue.wait_until_idle(Duration::from_secs(10)).await);
    let small_snap = queue.get(small_id).unwrap();
    assert_eq!(small_snap.status, TransferStatus::Completed);
    assert!(!small_snap.using_part_file);
    assert_eq!(fs::read(remote_root.join("exact_1mb.bin")).unwrap(), exact_1mb);
    assert!(
        srv.state
            .recorded_writes
            .lock()
            .unwrap()
            .iter()
            .all(|w| w.path == "/exact_1mb.bin")
    );
}

#[tokio::test]
async fn slice2_tcp_drop_right_before_rename_resumes_at_eof_and_renames_with_zero_reupload() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    srv.state.drop_conn_on_rename.store(true, Ordering::SeqCst);

    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::new(SftpManager::new());

    let size = 1_500_000usize;
    let payload: Vec<u8> = (0..size).map(|i| (i % 249) as u8).collect();
    let local_file = local_dir.join("drop_on_rename.bin");
    fs::write(&local_file, &payload).unwrap();

    let tid = queue.enqueue_upload(endpoint, local_file, "/drop_on_rename.bin".to_string());
    assert!(queue.wait_until_idle(Duration::from_secs(20)).await);

    let snap = queue.get(tid).unwrap();
    assert_eq!(snap.status, TransferStatus::Completed);
    assert!(snap.using_part_file);
    assert_eq!(snap.resume_offset, size as u64);
    assert_eq!(fs::read(remote_root.join("drop_on_rename.bin")).unwrap(), payload);
    assert!(!remote_root.join("drop_on_rename.bin.termewnal.part").exists());

    // Zero bytes re-uploaded on connection #1+.
    let conn1_writes = srv
        .state
        .recorded_writes
        .lock()
        .unwrap()
        .iter()
        .filter(|w| w.conn_index >= 1)
        .count();
    assert_eq!(conn1_writes, 0);
}

#[tokio::test]
async fn slice3_part_create_denied_falls_back_to_direct_write_with_notice() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    srv.state
        .deny_open_exact
        .lock()
        .unwrap()
        .insert("/denied_create.bin.termewnal.part".to_string());

    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::new(SftpManager::new());

    let size = 1_572_864usize;
    let payload: Vec<u8> = (0..size).map(|i| ((i * 13 + 3) % 251) as u8).collect();
    let local_file = local_dir.join("denied_create.bin");
    fs::write(&local_file, &payload).unwrap();

    let tid = queue.enqueue_upload(endpoint, local_file, "/denied_create.bin".to_string());
    assert!(queue.wait_until_idle(Duration::from_secs(10)).await);

    let snap = queue.get(tid).unwrap();
    assert_eq!(snap.status, TransferStatus::Completed);
    assert!(!snap.using_part_file);
    assert!(matches!(
        snap.notice,
        Some(TransferNotice::PartFallbackDirectWrite { .. })
    ));
    assert!(snap.status_notice.as_deref().is_some_and(|s| !s.is_empty()));
    assert_eq!(
        fs::read(remote_root.join("denied_create.bin")).unwrap(),
        payload
    );
    assert!(!remote_root.join("denied_create.bin.termewnal.part").exists());
}

#[tokio::test]
async fn slice3_part_rename_denied_or_unsupported_falls_back_to_direct_write_with_notice() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::new(SftpManager::new());

    for (idx, code) in [
        StatusCode::PermissionDenied,
        StatusCode::OpUnsupported,
        StatusCode::Failure,
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("denied_rename_{idx}.bin");
        let remote_path = format!("/{name}");
        srv.state
            .deny_rename_paths
            .lock()
            .unwrap()
            .insert(format!("{remote_path}.termewnal.part"));
        *srv.state.deny_rename_status.lock().unwrap() = code;

        let size = 1_400_000usize;
        let payload: Vec<u8> = (0..size).map(|i| ((i * 19 + idx) % 251) as u8).collect();
        let local_file = local_dir.join(&name);
        fs::write(&local_file, &payload).unwrap();

        let tid = queue.enqueue_upload(endpoint.clone(), local_file, remote_path);
        assert!(queue.wait_until_idle(Duration::from_secs(10)).await);

        let snap = queue.get(tid).unwrap();
        assert_eq!(snap.status, TransferStatus::Completed);
        assert!(!snap.using_part_file);
        assert!(matches!(
            snap.notice,
            Some(TransferNotice::PartFallbackDirectWrite { .. })
        ));
        assert!(snap.status_notice.as_deref().is_some_and(|s| !s.is_empty()));
        assert_eq!(fs::read(remote_root.join(&name)).unwrap(), payload);
        assert!(!remote_root.join(format!("{name}.termewnal.part")).exists());
    }
}

#[tokio::test]
async fn slice4_conflict_ask_pauses_and_resolves_overwrite_resume_skip_and_rename() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let download_dir = temp.path().join("downloads");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&download_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    // Simulate SFTPv3 rename rejection when target already exists -> exercises remove_file + rename.
    srv.state
        .reject_rename_if_target_exists
        .store(true, Ordering::SeqCst);

    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::new(SftpManager::new());

    let size = 1_500_000usize;
    let overwrite_data: Vec<u8> = (0..size).map(|i| ((i * 5 + 1) % 251) as u8).collect();
    let resume_data: Vec<u8> = (0..size).map(|i| ((i * 7 + 2) % 251) as u8).collect();
    let skip_data = b"new-skip-payload".to_vec();
    let rename_data = b"new-rename-payload".to_vec();

    // Seed pre-existing colliding remote targets.
    fs::write(remote_root.join("overwrite.bin"), b"stale-overwrite").unwrap();
    let resume_prefix_len = 512 * 1024usize;
    fs::write(
        remote_root.join("resume.bin"),
        &resume_data[..resume_prefix_len],
    )
    .unwrap();
    fs::write(remote_root.join("skip.bin"), b"keep-existing-skip").unwrap();
    fs::write(remote_root.join("rename.bin"), b"keep-existing-rename").unwrap();

    let p_overwrite = local_dir.join("overwrite.bin");
    let p_resume = local_dir.join("resume.bin");
    let p_skip = local_dir.join("skip.bin");
    let p_rename = local_dir.join("rename.bin");
    fs::write(&p_overwrite, &overwrite_data).unwrap();
    fs::write(&p_resume, &resume_data).unwrap();
    fs::write(&p_skip, &skip_data).unwrap();
    fs::write(&p_rename, &rename_data).unwrap();

    let id_overwrite =
        queue.enqueue_upload(endpoint.clone(), p_overwrite, "/overwrite.bin".to_string());
    let id_resume = queue.enqueue_upload(endpoint.clone(), p_resume, "/resume.bin".to_string());
    let id_skip = queue.enqueue_upload(endpoint.clone(), p_skip, "/skip.bin".to_string());
    let id_rename = queue.enqueue_upload(endpoint.clone(), p_rename, "/rename.bin".to_string());

    // Wait until all 4 settle into AwaitingConflict under default policy `Ask`.
    let mut rx = queue.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if rx.borrow().awaiting_conflict_count == 4 {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await
    .expect("expected 4 transfers to enter AwaitingConflict");

    for tid in [id_overwrite, id_resume, id_skip, id_rename] {
        let s = queue.get(tid).unwrap();
        assert_eq!(s.status, TransferStatus::AwaitingConflict);
        assert!(s.pending_conflict.is_some());
    }
    let resume_conflict = queue.get(id_resume).unwrap().pending_conflict.unwrap();
    assert_eq!(resume_conflict.direction, TransferDirection::Upload);
    assert_eq!(resume_conflict.source_size, size as u64);
    assert_eq!(resume_conflict.existing_target_size, resume_prefix_len as u64);

    // Resolve each conflict independently (apply_to_all = false).
    assert!(queue.resolve_conflict(id_overwrite, ConflictResolution::Overwrite, false));
    assert!(queue.resolve_conflict(id_resume, ConflictResolution::Resume, false));
    assert!(queue.resolve_conflict(id_skip, ConflictResolution::Skip, false));
    assert!(queue.resolve_conflict(
        id_rename,
        ConflictResolution::Rename {
            new_name: "renamed_target.bin".to_string()
        },
        false
    ));

    assert!(queue.wait_until_idle(Duration::from_secs(15)).await);

    assert_eq!(queue.get(id_overwrite).unwrap().status, TransferStatus::Completed);
    assert_eq!(
        fs::read(remote_root.join("overwrite.bin")).unwrap(),
        overwrite_data
    );

    let resumed_snap = queue.get(id_resume).unwrap();
    assert_eq!(resumed_snap.status, TransferStatus::Completed);
    assert_eq!(resumed_snap.resume_offset, resume_prefix_len as u64);
    assert_eq!(fs::read(remote_root.join("resume.bin")).unwrap(), resume_data);
    let resume_writes: Vec<_> = srv
        .state
        .recorded_writes
        .lock()
        .unwrap()
        .iter()
        .filter(|w| w.path == "/resume.bin")
        .cloned()
        .collect();
    assert!(
        !resume_writes.is_empty()
            && resume_writes
                .iter()
                .all(|w| w.offset >= resume_prefix_len as u64),
        "Resume must not re-upload bytes [0..{resume_prefix_len}): {resume_writes:?}"
    );

    assert_eq!(queue.get(id_skip).unwrap().status, TransferStatus::Skipped);
    assert_eq!(
        fs::read(remote_root.join("skip.bin")).unwrap(),
        b"keep-existing-skip"
    );

    let renamed_snap = queue.get(id_rename).unwrap();
    assert_eq!(renamed_snap.status, TransferStatus::Completed);
    assert_eq!(renamed_snap.file_name, "renamed_target.bin");
    assert_eq!(
        fs::read(remote_root.join("rename.bin")).unwrap(),
        b"keep-existing-rename"
    );
    assert_eq!(
        fs::read(remote_root.join("renamed_target.bin")).unwrap(),
        rename_data
    );

    // Download collision under `Ask` -> Rename leaves existing local file intact.
    fs::write(remote_root.join("dl.txt"), b"fresh-remote-dl").unwrap();
    fs::write(download_dir.join("dl.txt"), b"existing-local-dl").unwrap();
    let dl_id = queue.enqueue_download(
        endpoint,
        "/dl.txt".to_string(),
        download_dir.join("dl.txt"),
    );
    assert!(queue.wait_until_idle(Duration::from_secs(5)).await);
    assert_eq!(
        queue.get(dl_id).unwrap().status,
        TransferStatus::AwaitingConflict
    );
    assert!(queue.resolve_conflict(
        dl_id,
        ConflictResolution::Rename {
            new_name: "dl_renamed.txt".to_string()
        },
        false
    ));
    assert!(queue.wait_until_idle(Duration::from_secs(5)).await);
    assert_eq!(queue.get(dl_id).unwrap().status, TransferStatus::Completed);
    assert_eq!(
        fs::read(download_dir.join("dl.txt")).unwrap(),
        b"existing-local-dl"
    );
    assert_eq!(
        fs::read(download_dir.join("dl_renamed.txt")).unwrap(),
        b"fresh-remote-dl"
    );
}

#[tokio::test]
async fn slice4_conflict_apply_to_all_and_config_default_policy() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    let endpoint = setup_endpoint(&temp, &srv);

    // Part A: `apply_to_all = true` applies Skip across pending and subsequent collisions.
    let queue_ask = TransferQueue::new(SftpManager::new());
    for i in 0..4 {
        let name = format!("batch_{i}.txt");
        fs::write(remote_root.join(&name), format!("existing-{i}")).unwrap();
        fs::write(local_dir.join(&name), format!("new-{i}")).unwrap();
    }
    let id0 = queue_ask.enqueue_upload(
        endpoint.clone(),
        local_dir.join("batch_0.txt"),
        "/batch_0.txt".to_string(),
    );
    let id1 = queue_ask.enqueue_upload(
        endpoint.clone(),
        local_dir.join("batch_1.txt"),
        "/batch_1.txt".to_string(),
    );
    let id2 = queue_ask.enqueue_upload(
        endpoint.clone(),
        local_dir.join("batch_2.txt"),
        "/batch_2.txt".to_string(),
    );

    let mut rx = queue_ask.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if rx.borrow().awaiting_conflict_count >= 1
                && rx.borrow().active_count + rx.borrow().queued_count == 0
            {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await
    .expect("expected AwaitingConflict entries");

    assert!(queue_ask.resolve_conflict(id0, ConflictResolution::Skip, true));
    // Enqueue a 4th colliding file after apply_to_all=true was chosen.
    let id3 = queue_ask.enqueue_upload(
        endpoint.clone(),
        local_dir.join("batch_3.txt"),
        "/batch_3.txt".to_string(),
    );
    assert!(queue_ask.wait_until_idle(Duration::from_secs(5)).await);

    for (idx, tid) in [id0, id1, id2, id3].into_iter().enumerate() {
        assert_eq!(queue_ask.get(tid).unwrap().status, TransferStatus::Skipped);
        assert_eq!(
            fs::read_to_string(remote_root.join(format!("batch_{idx}.txt"))).unwrap(),
            format!("existing-{idx}")
        );
    }

    // Part B: Configurable default policy `Rename` resolves collisions without prompting.
    let queue_auto_rename = TransferQueue::with_config(
        SftpManager::new(),
        TransferQueueConfig {
            transfer: TransferConfig {
                default_conflict_policy: TransferConflictPolicy::Rename,
                ..TransferConfig::default()
            },
            ..TransferQueueConfig::default()
        },
    );
    fs::write(remote_root.join("report.txt"), b"original-report").unwrap();
    fs::write(local_dir.join("report.txt"), b"second-report").unwrap();
    let auto_id = queue_auto_rename.enqueue_upload(
        endpoint,
        local_dir.join("report.txt"),
        "/report.txt".to_string(),
    );
    assert!(queue_auto_rename.wait_until_idle(Duration::from_secs(5)).await);
    let auto_snap = queue_auto_rename.get(auto_id).unwrap();
    assert_eq!(auto_snap.status, TransferStatus::Completed);
    assert_eq!(auto_snap.file_name, "report (1).txt");
    assert_eq!(
        fs::read(remote_root.join("report.txt")).unwrap(),
        b"original-report"
    );
    assert_eq!(
        fs::read(remote_root.join("report (1).txt")).unwrap(),
        b"second-report"
    );
}

#[tokio::test]
async fn slice5_recursive_dir_upload_and_download_preserves_posix_permissions_and_caps_concurrency() {
    let temp = TempDir::new().unwrap();
    let local_proj = temp.path().join("proj");
    let dl_proj = temp.path().join("downloaded_proj");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(local_proj.join("bin")).unwrap();
    fs::create_dir_all(local_proj.join("config")).unwrap();
    fs::create_dir_all(local_proj.join("empty_sub")).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let script_bytes = b"#!/bin/sh\necho deploy\n".repeat(500);
    let secret_bytes = b"API_TOKEN=supersecret\n".to_vec();
    let big_bytes: Vec<u8> = (0..1_300_000usize).map(|i| (i % 251) as u8).collect();

    fs::write(local_proj.join("bin/deploy.sh"), &script_bytes).unwrap();
    fs::write(local_proj.join("config/secret.env"), &secret_bytes).unwrap();
    fs::write(local_proj.join("big.bin"), &big_bytes).unwrap();
    for idx in 0..4usize {
        fs::write(
            local_proj.join(format!("extra_{idx}.dat")),
            vec![(idx + 10) as u8; 64 * 1024],
        )
        .unwrap();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            local_proj.join("bin/deploy.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::set_permissions(
            local_proj.join("config/secret.env"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::set_permissions(local_proj.join("bin"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(local_proj.join("config"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(
            local_proj.join("empty_sub"),
            fs::Permissions::from_mode(0o711),
        )
        .unwrap();
    }

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::new(SftpManager::new());

    let up_summary = queue
        .enqueue_upload_dir(endpoint.clone(), local_proj.clone(), "/uploaded_proj".to_string())
        .await
        .expect("enqueue recursive upload");
    assert_eq!(up_summary.transfer_ids.len(), 7);
    assert!(up_summary.created_dir_count >= 4);
    assert!(queue.wait_until_idle(Duration::from_secs(20)).await);
    assert_eq!(queue.snapshot().completed_count, 7);

    assert_eq!(
        fs::read(remote_root.join("uploaded_proj/bin/deploy.sh")).unwrap(),
        script_bytes
    );
    assert_eq!(
        fs::read(remote_root.join("uploaded_proj/config/secret.env")).unwrap(),
        secret_bytes
    );
    assert_eq!(
        fs::read(remote_root.join("uploaded_proj/big.bin")).unwrap(),
        big_bytes
    );
    assert!(remote_root.join("uploaded_proj/empty_sub").is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(remote_root.join("uploaded_proj/bin/deploy.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(remote_root.join("uploaded_proj/config/secret.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(remote_root.join("uploaded_proj/bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(
            fs::metadata(remote_root.join("uploaded_proj/config"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(remote_root.join("uploaded_proj/empty_sub"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o711
        );
    }

    // Recursive download restores files, empty subdirs, and POSIX permissions locally.
    let dl_summary = queue
        .enqueue_download_dir(endpoint, "/uploaded_proj".to_string(), dl_proj.clone())
        .await
        .expect("enqueue recursive download");
    assert_eq!(dl_summary.transfer_ids.len(), 7);
    assert!(queue.wait_until_idle(Duration::from_secs(20)).await);
    assert_eq!(queue.snapshot().completed_count, 14);

    assert_eq!(
        fs::read(dl_proj.join("bin/deploy.sh")).unwrap(),
        script_bytes
    );
    assert_eq!(
        fs::read(dl_proj.join("config/secret.env")).unwrap(),
        secret_bytes
    );
    assert_eq!(fs::read(dl_proj.join("big.bin")).unwrap(), big_bytes);
    assert!(dl_proj.join("empty_sub").is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dl_proj.join("bin/deploy.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(dl_proj.join("config/secret.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dl_proj.join("bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(
            fs::metadata(dl_proj.join("config"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dl_proj.join("empty_sub"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o711
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn slice5_recursive_transfers_detect_and_skip_symlink_loops_by_default_and_when_following() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let local_sym = temp.path().join("sym_src");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(local_sym.join("dir_a")).unwrap();
    fs::create_dir_all(local_sym.join("dir_b")).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    fs::write(local_sym.join("real.txt"), b"real-file-content").unwrap();
    fs::write(local_sym.join("dir_a/a.txt"), b"from-a").unwrap();
    fs::write(local_sym.join("dir_b/b.txt"), b"from-b").unwrap();

    // Create both an ancestor loop (`dir_a/loop_up -> ..`) and mutual loops (`dir_a/to_b <-> dir_b/to_a`),
    // plus an acyclic file symlink (`link_real.txt -> real.txt`).
    symlink("real.txt", local_sym.join("link_real.txt")).unwrap();
    symlink("..", local_sym.join("dir_a/loop_up")).unwrap();
    symlink("../dir_b", local_sym.join("dir_a/to_b")).unwrap();
    symlink("../dir_a", local_sym.join("dir_b/to_a")).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    let endpoint = setup_endpoint(&temp, &srv);

    // Default (`follow_symlinks = false`): skips ALL symlinks safely without looping.
    let queue_default = TransferQueue::new(SftpManager::new());
    let up_default = queue_default
        .enqueue_upload_dir(endpoint.clone(), local_sym.clone(), "/sym_default".to_string())
        .await
        .expect("upload with follow_symlinks=false");
    assert_eq!(up_default.transfer_ids.len(), 3);
    assert!(!up_default.skipped_symlinks.is_empty());
    assert!(!up_default.skipped_symlink_loops.is_empty());
    assert!(queue_default.wait_until_idle(Duration::from_secs(10)).await);
    assert_eq!(queue_default.snapshot().completed_count, 3);
    assert!(!remote_root.join("sym_default/link_real.txt").exists());

    // Prepare remote tree WITH symlink loops for download verification.
    let remote_sym = remote_root.join("remote_sym");
    fs::create_dir_all(remote_sym.join("dir_a")).unwrap();
    fs::create_dir_all(remote_sym.join("dir_b")).unwrap();
    fs::write(remote_sym.join("real.txt"), b"remote-real").unwrap();
    fs::write(remote_sym.join("dir_a/a.txt"), b"remote-a").unwrap();
    fs::write(remote_sym.join("dir_b/b.txt"), b"remote-b").unwrap();
    symlink("real.txt", remote_sym.join("link_real.txt")).unwrap();
    symlink("..", remote_sym.join("dir_a/loop_up")).unwrap();
    symlink("../dir_b", remote_sym.join("dir_a/to_b")).unwrap();
    symlink("../dir_a", remote_sym.join("dir_b/to_a")).unwrap();

    let dl_default_dir = temp.path().join("dl_default");
    let dl_default = queue_default
        .enqueue_download_dir(
            endpoint.clone(),
            "/remote_sym".to_string(),
            dl_default_dir.clone(),
        )
        .await
        .expect("download with follow_symlinks=false");
    assert_eq!(dl_default.transfer_ids.len(), 3);
    assert!(!dl_default.skipped_symlinks.is_empty());
    assert!(!dl_default.skipped_symlink_loops.is_empty());
    assert!(queue_default.wait_until_idle(Duration::from_secs(10)).await);
    assert_eq!(queue_default.snapshot().completed_count, 6);
    assert!(!dl_default_dir.join("link_real.txt").exists());
    assert_eq!(fs::read(dl_default_dir.join("real.txt")).unwrap(), b"remote-real");

    // Configured (`follow_symlinks = true`): follows acyclic symlink `link_real.txt` AND still
    // detects/skips all cyclic symlink loops safely!
    let queue_follow = TransferQueue::with_config(
        SftpManager::new(),
        TransferQueueConfig {
            transfer: TransferConfig {
                follow_symlinks: true,
                ..TransferConfig::default()
            },
            ..TransferQueueConfig::default()
        },
    );

    let up_follow = queue_follow
        .enqueue_upload_dir(endpoint.clone(), local_sym, "/sym_follow".to_string())
        .await
        .expect("upload with follow_symlinks=true");
    assert_eq!(up_follow.transfer_ids.len(), 4);
    assert!(!up_follow.skipped_symlink_loops.is_empty());
    assert!(queue_follow.wait_until_idle(Duration::from_secs(10)).await);
    assert_eq!(
        fs::read(remote_root.join("sym_follow/link_real.txt")).unwrap(),
        b"real-file-content"
    );

    let dl_follow_dir = temp.path().join("dl_follow");
    let dl_follow = queue_follow
        .enqueue_download_dir(endpoint, "/remote_sym".to_string(), dl_follow_dir.clone())
        .await
        .expect("download with follow_symlinks=true");
    assert_eq!(dl_follow.transfer_ids.len(), 4);
    assert!(!dl_follow.skipped_symlink_loops.is_empty());
    assert!(queue_follow.wait_until_idle(Duration::from_secs(10)).await);
    assert_eq!(
        fs::read(dl_follow_dir.join("link_real.txt")).unwrap(),
        b"remote-real"
    );
}

#[tokio::test]
async fn slice6_malicious_remote_dir_entry_traversal_names_are_rejected_without_escaping_local_dir() {
    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    let evil_dir = remote_root.join("evil_dir");
    fs::create_dir_all(&evil_dir).unwrap();
    fs::write(evil_dir.join("safe.txt"), b"safe-payload").unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::new(SftpManager::new());

    let local_base = temp.path().join("downloads");
    let escaped_target = local_base.join("escaped.txt");
    let abs_escape = temp.path().join("abs_escaped.txt");

    for bad_name in [
        "../escaped.txt".to_string(),
        abs_escape.to_string_lossy().into_owned(),
        "sub/nested.txt".to_string(),
        r"bad\win.txt".to_string(),
        "bad\0null.txt".to_string(),
        String::new(),
    ] {
        srv.state
            .injected_dir_entries
            .lock()
            .unwrap()
            .insert("/evil_dir".to_string(), vec![bad_name.clone()]);

        let target_dir = local_base.join("target");
        let err = queue
            .enqueue_download_dir(endpoint.clone(), "/evil_dir".to_string(), target_dir.clone())
            .await
            .expect_err(&format!("malicious SFTP entry `{bad_name:?}` must be rejected"));
        assert!(
            err.to_string().to_lowercase().contains("unsafe")
                || err.to_string().to_lowercase().contains("invalid"),
            "unexpected error for `{bad_name:?}`: {err}"
        );
        assert!(
            !escaped_target.exists(),
            "escaped file must not exist for `{bad_name:?}`"
        );
        assert!(
            !abs_escape.exists(),
            "absolute-path file must not exist for `{bad_name:?}`"
        );
    }
    assert!(queue.snapshot().transfers.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn slice6_permission_preserve_failure_emits_permission_preserve_skipped_notice() {
    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("local");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(local_dir.join("dir_perm/sub")).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let single_src = local_dir.join("denied_perm.txt");
    fs::write(&single_src, b"perm-test-payload").unwrap();
    fs::write(local_dir.join("dir_perm/sub/nested.txt"), b"dir-perm-payload").unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pass").await;
    srv.state
        .deny_setstat_paths
        .lock()
        .unwrap()
        .insert("/denied_perm.txt".to_string());
    srv.state
        .deny_setstat_paths
        .lock()
        .unwrap()
        .insert("/dir_perm/sub".to_string());

    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::new(SftpManager::new());

    // 1. Single file upload: chmod failure emits PermissionPreserveSkipped notice while completing data transfer.
    let single_id = queue.enqueue_upload(endpoint.clone(), single_src, "/denied_perm.txt".to_string());
    assert!(queue.wait_until_idle(Duration::from_secs(10)).await);
    let single_snap = queue.get(single_id).unwrap();
    assert_eq!(single_snap.status, TransferStatus::Completed);
    assert!(
        matches!(
            single_snap.notice,
            Some(TransferNotice::PermissionPreserveSkipped { .. })
        ),
        "expected PermissionPreserveSkipped notice, got {:?}",
        single_snap.notice
    );
    assert!(
        single_snap
            .status_notice
            .as_deref()
            .is_some_and(|s| s.contains("Could not preserve POSIX permissions"))
    );
    assert_eq!(
        fs::read(remote_root.join("denied_perm.txt")).unwrap(),
        b"perm-test-payload"
    );

    // 2. Directory upload: dir chmod failure emits PermissionPreserveSkipped notice on batch completion.
    let dir_summary = queue
        .enqueue_upload_dir(
            endpoint,
            local_dir.join("dir_perm"),
            "/dir_perm".to_string(),
        )
        .await
        .expect("enqueue dir upload");
    assert_eq!(dir_summary.transfer_ids.len(), 1);
    assert!(queue.wait_until_idle(Duration::from_secs(10)).await);
    let dir_snap = queue.get(dir_summary.transfer_ids[0]).unwrap();
    assert_eq!(dir_snap.status, TransferStatus::Completed);
    assert!(
        matches!(
            dir_snap.notice,
            Some(TransferNotice::PermissionPreserveSkipped { .. })
        ),
        "expected PermissionPreserveSkipped notice for dir chmod failure, got {:?}",
        dir_snap.notice
    );
}


#[tokio::test]
async fn sftp_manager_auto_reconnects_on_connection_drop_and_emits_reconnected_status() {
    use termewnal_ssh::SftpTransportStatus;

    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&remote_root).unwrap();
    fs::write(remote_root.join("src.txt"), b"reconnect-payload").unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pw").await;
    let ep = setup_endpoint(&temp, &srv);
    let manager = SftpManager::new().with_retry_delay(Duration::from_millis(20));

    // Warm up the Transfer Transport cache.
    manager.list_dir(&ep, ".").await.expect("initial list_dir");

    let observed = Arc::new(Mutex::new(Vec::<SftpTransportStatus>::new()));
    let observed_clone = Arc::clone(&observed);
    let mut status_rx = manager.subscribe_status();
    let watch_key = ep.key.clone();
    let (done_tx, done_rx) = oneshot::channel();
    let watcher = tokio::spawn(async move {
        let mut done_tx = Some(done_tx);
        while status_rx.changed().await.is_ok() {
            if let Some(st) = status_rx.borrow_and_update().get(&watch_key).cloned() {
                let is_reconnected = matches!(st, SftpTransportStatus::Reconnected { .. });
                observed_clone.lock().unwrap().push(st);
                if is_reconnected {
                    if let Some(tx) = done_tx.take() {
                        let _ = tx.send(());
                    }
                    break;
                }
            }
        }
    });

    // Drop the TCP connection on the first rename attempt -> SftpManager must auto-reconnect and finish.
    srv.state.drop_conn_on_rename.store(true, Ordering::SeqCst);
    manager
        .rename(&ep, "/src.txt", "/dst.txt")
        .await
        .expect("rename must recover after connection drop");

    let _ = tokio::time::timeout(Duration::from_secs(2), done_rx).await;
    watcher.abort();
    assert!(remote_root.join("dst.txt").exists());
    let states = observed.lock().unwrap().clone();
    assert!(
        states
            .iter()
            .any(|s| matches!(s, SftpTransportStatus::AutoReconnecting { attempt: 1, .. })),
        "expected SftpTransportStatus::AutoReconnecting, got {states:?}"
    );
    assert!(
        matches!(
            manager.transport_status(&ep.key),
            SftpTransportStatus::Reconnected { .. }
        ) || states
            .iter()
            .any(|s| matches!(s, SftpTransportStatus::Reconnected { .. })),
        "expected SftpTransportStatus::Reconnected, got {states:?}"
    );
}

#[tokio::test]
async fn transfer_queue_tracks_auto_reconnecting_and_reconnected_on_drop() {
    use termewnal_ssh::TransferReconnectState;

    let temp = TempDir::new().unwrap();
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&remote_root).unwrap();
    let local_file = temp.path().join("upload.bin");
    fs::write(&local_file, vec![b'A'; 1024 * 1024]).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pw").await;
    *srv.state.drop_conn_after_part_bytes.lock().unwrap() = Some(256 * 1024);
    let ep = setup_endpoint(&temp, &srv);

    let mut transfer_cfg = TransferConfig::default();
    transfer_cfg.part_threshold_bytes = 64 * 1024;
    let manager = SftpManager::new().with_retry_delay(Duration::from_millis(50));
    let queue = TransferQueue::with_config(
        manager,
        TransferQueueConfig {
            transfer: transfer_cfg,
            reconnect_retry_delay: Duration::from_millis(250),
            ..TransferQueueConfig::default()
        },
    );
    let mut sub = queue.subscribe();
    let tid = queue.enqueue_upload(ep, local_file, "/upload.bin".to_string());

    let mut saw_auto_reconnecting = false;
    let final_snap = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snap = sub.borrow_and_update().clone();
            if let Some(item) = snap.transfers.iter().find(|t| t.id == tid) {
                if matches!(
                    item.reconnect_state,
                    TransferReconnectState::AutoReconnecting { .. }
                ) {
                    saw_auto_reconnecting = true;
                }
                if item.status == TransferStatus::Completed {
                    return item.clone();
                }
                if let TransferStatus::Failed { ref error } = item.status {
                    panic!("transfer failed instead of recovering: {error}");
                }
            }
            sub.changed().await.unwrap();
        }
    })
    .await
    .expect("transfer must complete after auto-reconnect");

    assert!(saw_auto_reconnecting, "expected AutoReconnecting before Reconnected");
    assert_eq!(
        final_snap.reconnect_state,
        TransferReconnectState::Reconnected
    );
    assert_eq!(
        final_snap.status_notice.as_deref(),
        Some("Reconnected successfully")
    );
    assert_eq!(
        fs::read(remote_root.join("upload.bin")).unwrap().len(),
        1024 * 1024
    );
}

#[tokio::test]
async fn download_part_atomic_mid_drop_resume_and_source_change_reset() {
    use termewnal_ssh::local_part_path_for;

    let temp = TempDir::new().unwrap();
    let local_dir = temp.path().join("downloads");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_dir).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let size = 2 * 1024 * 1024usize;
    let payload: Vec<u8> = (0..size).map(|i| ((i * 31 + 11) % 251) as u8).collect();
    fs::write(remote_root.join("big_dl.bin"), &payload).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pw").await;
    *srv.state.drop_conn_after_read_bytes.lock().unwrap() = Some(512 * 1024);

    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::with_config(
        SftpManager::new().with_retry_delay(Duration::from_millis(20)),
        TransferQueueConfig {
            reconnect_retry_delay: Duration::from_millis(40),
            ..TransferQueueConfig::default()
        },
    );

    // 1. >1 MiB download writes to `.termewnal.part`, recovers on TCP drop, resumes, and renames atomically.
    let dl_dest = local_dir.join("big_dl.bin");
    let tid = queue.enqueue_download(endpoint.clone(), "/big_dl.bin".to_string(), dl_dest.clone());
    assert!(queue.wait_until_idle(Duration::from_secs(15)).await);

    let snap = queue.get(tid).unwrap();
    assert_eq!(snap.status, TransferStatus::Completed);
    assert!(snap.using_part_file);
    assert!(
        snap.resume_offset >= 256 * 1024 && snap.resume_offset < size as u64,
        "expected download resume offset >= 256 KiB and < {size}, got {}",
        snap.resume_offset
    );
    assert_eq!(snap.bytes_transferred, size as u64);
    assert_eq!(fs::read(&dl_dest).unwrap(), payload);
    assert!(
        !local_part_path_for(&dl_dest).exists(),
        "atomic local rename must remove .termewnal.part after completion"
    );

    // 2. If remote source file changes size while paused/failed with partial `.part`, retry resets to 0
    //    instead of corrupting the file with mismatched offset splicing.
    let changed_dest = local_dir.join("mutating.bin");
    fs::write(remote_root.join("mutating.bin"), &payload).unwrap();
    srv.state.dropped_once.store(false, Ordering::SeqCst);
    *srv.state.drop_conn_after_read_bytes.lock().unwrap() = Some(512 * 1024);

    let no_retry_queue = TransferQueue::with_config(
        SftpManager::new(),
        TransferQueueConfig {
            max_transport_retries: 0,
            ..TransferQueueConfig::default()
        },
    );
    let mut_id = no_retry_queue.enqueue_download(
        endpoint,
        "/mutating.bin".to_string(),
        changed_dest.clone(),
    );
    assert!(no_retry_queue.wait_until_idle(Duration::from_secs(10)).await);
    let failed_snap = no_retry_queue.get(mut_id).unwrap();
    assert!(matches!(failed_snap.status, TransferStatus::Failed { .. }));
    assert!(failed_snap.using_part_file);
    assert!(local_part_path_for(&changed_dest).exists());
    assert!(no_retry_queue.has_reconnectable_failures());

    // Replace remote source with completely different shorter payload before retry.
    *srv.state.drop_conn_after_read_bytes.lock().unwrap() = None;
    let updated_payload: Vec<u8> = (0..1_300_000usize).map(|i| ((i * 17 + 5) % 241) as u8).collect();
    fs::write(remote_root.join("mutating.bin"), &updated_payload).unwrap();

    assert!(no_retry_queue.retry(mut_id));
    assert!(no_retry_queue.wait_until_idle(Duration::from_secs(10)).await);
    let retried_snap = no_retry_queue.get(mut_id).unwrap();
    assert_eq!(retried_snap.status, TransferStatus::Completed);
    assert_eq!(retried_snap.resume_offset, 0);
    assert_eq!(fs::read(&changed_dest).unwrap(), updated_payload);
    assert!(!local_part_path_for(&changed_dest).exists());
}

#[tokio::test]
async fn upload_dir_batch_retries_after_mkdir_drop_and_concurrent_invalidate_session() {
    let temp = TempDir::new().unwrap();
    let local_proj = temp.path().join("proj");
    let local_sub = local_proj.join("nested");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&local_sub).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let payload_a = vec![b'X'; 300_000];
    let payload_b = vec![b'Y'; 300_000];
    fs::write(local_proj.join("top.bin"), &payload_a).unwrap();
    fs::write(local_sub.join("nested.bin"), &payload_b).unwrap();

    let srv = spawn_fault_server(remote_root.clone(), "ops", "fault-pw").await;
    // Inject a transient drop on the very first mkdir: UploadDirBatch must NOT permanently cache Err!
    srv.state.drop_conn_on_mkdir.store(true, Ordering::SeqCst);

    let endpoint = setup_endpoint(&temp, &srv);
    let queue = TransferQueue::with_config(
        SftpManager::new().with_retry_delay(Duration::from_millis(20)),
        TransferQueueConfig {
            reconnect_retry_delay: Duration::from_millis(40),
            ..TransferQueueConfig::default()
        },
    );

    let summary = queue
        .enqueue_upload_path(endpoint.clone(), local_proj, "/proj".to_string())
        .expect("enqueue upload dir");
    assert_eq!(summary.transfer_ids.len(), 2);
    assert!(queue.wait_until_idle(Duration::from_secs(15)).await);

    for tid in &summary.transfer_ids {
        let s = queue.get(*tid).unwrap();
        assert_eq!(
            s.status,
            TransferStatus::Completed,
            "dir upload failed after mkdir drop recovery: {:?}",
            s
        );
    }
    assert_eq!(fs::read(remote_root.join("proj/top.bin")).unwrap(), payload_a);
    assert_eq!(
        fs::read(remote_root.join("proj/nested/nested.bin")).unwrap(),
        payload_b
    );

    // 3 concurrent `.termewnal.part` uploads hit one TCP drop: `invalidate_session` prevents late
    // sibling errors from evicting the freshly reconnected pooled transport.
    let conns_before = srv.state.connection_count.load(Ordering::SeqCst);
    srv.state.dropped_once.store(false, Ordering::SeqCst);
    *srv.state.drop_conn_after_part_bytes.lock().unwrap() = Some(256 * 1024);

    let mut ids = Vec::new();
    for idx in 0..3 {
        let data: Vec<u8> = vec![(0x40 + idx) as u8; 1_500_000];
        let p = temp.path().join(format!("conc_{idx}.bin"));
        fs::write(&p, &data).unwrap();
        ids.push((
            queue.enqueue_upload(endpoint.clone(), p, format!("/conc_{idx}.bin")),
            data,
        ));
    }
    assert!(queue.wait_until_idle(Duration::from_secs(20)).await);
    for (tid, data) in &ids {
        assert_eq!(queue.get(*tid).unwrap().status, TransferStatus::Completed);
        let remote_name = queue.get(*tid).unwrap().file_name;
        assert_eq!(fs::read(remote_root.join(remote_name)).unwrap(), *data);
    }
    let conns_delta = srv.state.connection_count.load(Ordering::SeqCst) - conns_before;
    assert!(
        conns_delta <= 2,
        "expected minimal reconnects without sibling thrashing, got {conns_delta} new connections"
    );
}
