use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use interprocess::local_socket::tokio::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::{ConnectOptions, ListenerOptions, Name};
use serde::{Deserialize, Serialize};
use termewnal_core::ConfigPaths;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

const MAX_IPC_FRAME_BYTES: usize = 64 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const BIND_ATTEMPTS: usize = 3;

/// Errors raised during single-instance local IPC setup or message forwarding.
#[derive(Debug, Error)]
pub enum IpcError {
    /// Failed to construct a platform local socket name from the socket path.
    #[error("invalid IPC socket path {path}: {source}")]
    InvalidSocketName {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Underlying I/O error while binding, probing, or reading/writing the local socket.
    #[error("IPC I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Wire payload exceeded frame limits or failed TOML encoding/decoding.
    #[error("invalid IPC payload: {0}")]
    Protocol(String),

    /// Primary instance rejected or failed to acknowledge the forwarded command.
    #[error("primary instance rejected IPC command: {0}")]
    Rejected(String),

    /// Background IPC server thread panicked or exited before reporting launch outcome.
    #[error("IPC coordinator thread terminated unexpectedly")]
    CoordinatorExited,
}

/// Command forwarded from a secondary Termewnal launch to the running primary process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum IpcCommand {
    /// Open a new coordinated application window in the primary process.
    OpenWindow,
    /// Open a new tab inside the primary process (optionally launching a specific `Profile` or `cwd`).
    OpenTab {
        #[serde(default)]
        profile_id: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// Toggle visibility of the Quake drop-down console window without closing active sessions.
    ToggleQuake,
}

/// Parses CLI flags (`--toggle-quake`, `--profile <id-or-name>`, `--new-tab`, `--cwd <path>`)
/// into a single-instance [`IpcCommand`].
pub fn parse_cli_launch_command<I, S>(args: I) -> IpcCommand
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut toggle_quake = false;
    let mut open_tab = false;
    let mut profile_id = None;
    let mut cwd = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let s = arg.as_ref();
        if s == "--toggle-quake" {
            toggle_quake = true;
        } else if s == "--new-tab" {
            open_tab = true;
        } else if s == "--profile" {
            if let Some(id) = iter.next() {
                let val = id.as_ref().trim();
                if !val.is_empty() {
                    profile_id = Some(val.to_string());
                    open_tab = true;
                }
            }
        } else if let Some(val) = s.strip_prefix("--profile=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                profile_id = Some(trimmed.to_string());
                open_tab = true;
            }
        } else if s == "--cwd" {
            if let Some(dir) = iter.next() {
                let trimmed = dir.as_ref().trim();
                if !trimmed.is_empty() {
                    cwd = Some(PathBuf::from(trimmed));
                    open_tab = true;
                }
            }
        } else if let Some(val) = s.strip_prefix("--cwd=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                cwd = Some(PathBuf::from(trimmed));
                open_tab = true;
            }
        }
    }

    if toggle_quake {
        IpcCommand::ToggleQuake
    } else if open_tab {
        IpcCommand::OpenTab { profile_id, cwd }
    } else {
        IpcCommand::OpenWindow
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IpcRequest {
    command: IpcCommand,
}

#[derive(Debug, Serialize, Deserialize)]
struct IpcResponse {
    accepted: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Result of resolving single-instance ownership at process startup.
pub enum LaunchOutcome {
    /// This process became the primary instance and owns the listening IPC server.
    Primary(IpcServerHandle),
    /// An active primary instance accepted our forwarded command.
    SecondaryForwarded,
}

struct IpcInner {
    socket_path: PathBuf,
    rx: Mutex<Option<mpsc::UnboundedReceiver<IpcCommand>>>,
    shutdown_tx: watch::Sender<bool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for IpcInner {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Ok(mut guard) = self.thread.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

/// Handle to the running primary-instance local IPC server.
///
/// Dropping or calling [`Self::shutdown`] stops the accept loop and reclaims the local socket.
#[derive(Clone)]
pub struct IpcServerHandle {
    inner: Arc<IpcInner>,
}

impl IpcServerHandle {
    /// Takes the incoming [`IpcCommand`] receiver once for wiring into an event subscription.
    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<IpcCommand>> {
        self.inner.rx.lock().ok()?.take()
    }

    /// Returns the filesystem path of the local IPC socket.
    pub fn socket_path(&self) -> &Path {
        &self.inner.socket_path
    }

    /// Signals the IPC accept loop to stop and waits for the listener to close and clean up.
    pub fn shutdown(&self) {
        let _ = self.inner.shutdown_tx.send(true);
        if let Ok(mut guard) = self.inner.thread.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

/// Acquires single-instance primary ownership over `paths`'s local IPC socket, or forwards
/// `command` to the already-running primary instance.
///
/// If a stale socket file remains on disk after an unclean exit, this probes it first and only
/// unlinks the file when connection fails, preventing interference with a live primary process.
pub fn acquire_or_forward(
    paths: &ConfigPaths,
    command: IpcCommand,
) -> Result<LaunchOutcome, IpcError> {
    let socket_path = paths.ipc_socket_file();
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let thread_socket_path = socket_path.clone();
    let handle = std::thread::Builder::new()
        .name("termewnal-ipc".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(source) => {
                    let _ = startup_tx.send(Err(IpcError::Io {
                        path: thread_socket_path,
                        source,
                    }));
                    return;
                }
            };

            rt.block_on(async move {
                match bind_or_forward(&thread_socket_path, &command).await {
                    Ok(BindOutcome::Primary(listener)) => {
                        let _ = startup_tx.send(Ok(true));
                        serve_listener(listener, &thread_socket_path, cmd_tx, &mut shutdown_rx)
                            .await;
                    }
                    Ok(BindOutcome::Forwarded) => {
                        let _ = startup_tx.send(Ok(false));
                    }
                    Err(err) => {
                        let _ = startup_tx.send(Err(err));
                    }
                }
            });
        })
        .map_err(|source| IpcError::Io {
            path: socket_path.clone(),
            source,
        })?;

    match startup_rx.recv() {
        Ok(Ok(true)) => Ok(LaunchOutcome::Primary(IpcServerHandle {
            inner: Arc::new(IpcInner {
                socket_path,
                rx: Mutex::new(Some(cmd_rx)),
                shutdown_tx,
                thread: Mutex::new(Some(handle)),
            }),
        })),
        Ok(Ok(false)) => {
            let _ = handle.join();
            Ok(LaunchOutcome::SecondaryForwarded)
        }
        Ok(Err(err)) => {
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            let _ = handle.join();
            Err(IpcError::CoordinatorExited)
        }
    }
}

/// Sends `command` to an already-running primary instance over `paths`'s local IPC socket.
pub fn send_command(paths: &ConfigPaths, command: &IpcCommand) -> Result<(), IpcError> {
    let socket_path = paths.ipc_socket_file();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| IpcError::Io {
            path: socket_path.clone(),
            source,
        })?;
    rt.block_on(send_command_async(&socket_path, command))
}

enum BindOutcome {
    Primary(LocalSocketListener),
    Forwarded,
}

fn is_listener_in_use(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::AddrInUse {
        return true;
    }
    #[cfg(windows)]
    if err.kind() == io::ErrorKind::PermissionDenied
        || matches!(err.raw_os_error(), Some(5) | Some(231))
    {
        return true;
    }
    false
}

async fn bind_or_forward(
    socket_path: &Path,
    command: &IpcCommand,
) -> Result<BindOutcome, IpcError> {
    ensure_socket_parent_dir(socket_path).await?;

    for attempt in 0..BIND_ATTEMPTS {
        let name = local_socket_name(socket_path)?;
        match ListenerOptions::new().name(name).create_tokio() {
            Ok(listener) => {
                restrict_unix_socket_permissions(socket_path);
                return Ok(BindOutcome::Primary(listener));
            }
            Err(err) if is_listener_in_use(&err) => {
                match send_command_async(socket_path, command).await {
                    Ok(()) => return Ok(BindOutcome::Forwarded),
                    Err(probe_err) => {
                        debug!(
                            path = %socket_path.display(),
                            attempt,
                            error = %probe_err,
                            "local socket probe failed; cleaning up stale socket"
                        );
                        remove_stale_socket(socket_path);
                    }
                }
            }
            Err(source) => {
                return Err(IpcError::Io {
                    path: socket_path.to_path_buf(),
                    source,
                });
            }
        }
    }

    // Final bind/forward attempt after cleanup retries.
    let name = local_socket_name(socket_path)?;
    match ListenerOptions::new().name(name).create_tokio() {
        Ok(listener) => {
            restrict_unix_socket_permissions(socket_path);
            Ok(BindOutcome::Primary(listener))
        }
        Err(err) if is_listener_in_use(&err) => {
            send_command_async(socket_path, command).await?;
            Ok(BindOutcome::Forwarded)
        }
        Err(source) => Err(IpcError::Io {
            path: socket_path.to_path_buf(),
            source,
        }),
    }
}

async fn serve_listener(
    listener: LocalSocketListener,
    socket_path: &Path,
    cmd_tx: mpsc::UnboundedSender<IpcCommand>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok(stream) => {
                        if let Err(err) = handle_client(stream, socket_path, &cmd_tx).await {
                            warn!(path = %socket_path.display(), error = %err, "failed to process IPC client");
                        }
                    }
                    Err(err) => {
                        warn!(path = %socket_path.display(), error = %err, "IPC accept error");
                    }
                }
            }
        }
    }
    drop(listener);
}

async fn handle_client(
    mut stream: LocalSocketStream,
    socket_path: &Path,
    cmd_tx: &mpsc::UnboundedSender<IpcCommand>,
) -> Result<(), IpcError> {
    let req: IpcRequest = read_frame(&mut stream, socket_path).await?;
    let response = match cmd_tx.send(req.command) {
        Ok(()) => IpcResponse {
            accepted: true,
            error: None,
        },
        Err(_) => IpcResponse {
            accepted: false,
            error: Some("primary event loop is shutting down".to_string()),
        },
    };
    write_frame(&mut stream, socket_path, &response).await
}

/// Connects to `socket_path`, sends `command`, and awaits primary acknowledgement.
pub async fn send_command_async(socket_path: &Path, command: &IpcCommand) -> Result<(), IpcError> {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        let name = local_socket_name(socket_path)?;
        let opts = ConnectOptions::new().name(name);
        let mut stream = opts.connect_tokio().await.map_err(|source| IpcError::Io {
            path: socket_path.to_path_buf(),
            source,
        })?;

        let req = IpcRequest {
            command: command.clone(),
        };
        write_frame(&mut stream, socket_path, &req).await?;
        let resp: IpcResponse = read_frame(&mut stream, socket_path).await?;
        if resp.accepted {
            Ok(())
        } else {
            Err(IpcError::Rejected(
                resp.error
                    .unwrap_or_else(|| "unknown IPC error".to_string()),
            ))
        }
    })
    .await
    .map_err(|_| IpcError::Io {
        path: socket_path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::TimedOut, "IPC request timed out"),
    })?
}

async fn write_frame<T: Serialize>(
    stream: &mut LocalSocketStream,
    socket_path: &Path,
    payload: &T,
) -> Result<(), IpcError> {
    let toml_str =
        toml::to_string(payload).map_err(|err| IpcError::Protocol(err.to_string()))?;
    let bytes = toml_str.as_bytes();
    if bytes.len() > MAX_IPC_FRAME_BYTES {
        return Err(IpcError::Protocol(format!(
            "IPC message length {} exceeds limit {MAX_IPC_FRAME_BYTES}",
            bytes.len()
        )));
    }
    let len_bytes = (bytes.len() as u32).to_be_bytes();
    stream
        .write_all(&len_bytes)
        .await
        .map_err(|source| IpcError::Io {
            path: socket_path.to_path_buf(),
            source,
        })?;
    stream
        .write_all(bytes)
        .await
        .map_err(|source| IpcError::Io {
            path: socket_path.to_path_buf(),
            source,
        })?;
    stream.flush().await.map_err(|source| IpcError::Io {
        path: socket_path.to_path_buf(),
        source,
    })?;
    Ok(())
}

async fn read_frame<T: for<'de> Deserialize<'de>>(
    stream: &mut LocalSocketStream,
    socket_path: &Path,
) -> Result<T, IpcError> {
    let mut len_buf = [0_u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|source| IpcError::Io {
            path: socket_path.to_path_buf(),
            source,
        })?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_IPC_FRAME_BYTES {
        return Err(IpcError::Protocol(format!(
            "invalid IPC frame length {len}"
        )));
    }
    let mut buf = vec![0_u8; len];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|source| IpcError::Io {
            path: socket_path.to_path_buf(),
            source,
        })?;
    let text =
        std::str::from_utf8(&buf).map_err(|err| IpcError::Protocol(err.to_string()))?;
    toml::from_str(text).map_err(|err| IpcError::Protocol(err.to_string()))
}

#[cfg(unix)]
fn local_socket_name(socket_path: &Path) -> Result<Name<'_>, IpcError> {
    socket_path
        .as_os_str()
        .to_fs_name::<GenericFilePath>()
        .map_err(|source| IpcError::InvalidSocketName {
            path: socket_path.to_path_buf(),
            source,
        })
}

#[cfg(windows)]
fn local_socket_name(socket_path: &Path) -> Result<Name<'static>, IpcError> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    socket_path.hash(&mut hasher);
    let pipe_name = format!("termewnal-{:016x}.sock", hasher.finish());
    pipe_name
        .to_ns_name::<GenericNamespaced>()
        .map_err(|source| IpcError::InvalidSocketName {
            path: socket_path.to_path_buf(),
            source,
        })
}

async fn ensure_socket_parent_dir(socket_path: &Path) -> Result<(), IpcError> {
    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|source| IpcError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_unix_socket_permissions(socket_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_unix_socket_permissions(_socket_path: &Path) {}

#[cfg(unix)]
fn remove_stale_socket(socket_path: &Path) {
    let _ = std::fs::remove_file(socket_path);
}

#[cfg(not(unix))]
fn remove_stale_socket(_socket_path: &Path) {}
