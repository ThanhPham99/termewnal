use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::vault::{InMemorySecretStore, SecretStore};
use termewnal_core::{LoginScriptStep, SshAuthMethod, SshProxyConfig, SshProxyKind};
use termewnal_pty::TerminalBuffer;
use termewnal_ssh::proxy_cmd::expand_proxy_command;
use termewnal_ssh::{
    BoxFuture, ConnectionSupervisor, HostKeyDecision, HostKeyPrompt,
    KeyboardInteractiveChallenge, OsAgentConnector, SshConnectConfig, SshInteractionHandler,
    SshSession, SupervisorConfig, TransportStatus, record_known_host_path,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{Notify, mpsc, oneshot};
use zeroize::Zeroizing;

struct AdvServerState {
    shell_opens: AtomicUsize,
    agent_requests: AtomicUsize,
    x11_requests: AtomicUsize,
    x11_cookie_seen: Mutex<Option<(String, String, u32)>>,
    agent_roundtrip_tx: Mutex<Option<oneshot::Sender<Result<Vec<u8>, String>>>>,
    x11_roundtrip_tx: Mutex<Option<oneshot::Sender<Result<Vec<u8>, String>>>>,
    open_unsolicited_agent: bool,
    open_unsolicited_x11: bool,
    login_banner_mode: bool,
    received_lines: Mutex<Vec<String>>,
    drop_current_conn: Option<Arc<Notify>>,
}

struct AdvTestHandler {
    state: Arc<AdvServerState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    line_buf: Vec<u8>,
}

impl server::Handler for AdvTestHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::reject())
    }

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if user == "ops" && password == "secret" {
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
        self.state.shell_opens.fetch_add(1, Ordering::SeqCst);
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

    async fn agent_request(
        &mut self,
        _channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.state.agent_requests.fetch_add(1, Ordering::SeqCst);
        let handle = session.handle();
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let res = match handle.channel_open_agent().await {
                Ok(ch) => {
                    let mut stream = ch.into_stream();
                    if let Err(e) = stream.write_all(b"AGENTREQ").await {
                        Err(e.to_string())
                    } else {
                        let mut buf = [0u8; 8];
                        match stream.read_exact(&mut buf).await {
                            Ok(_) => Ok(buf.to_vec()),
                            Err(e) => Err(e.to_string()),
                        }
                    }
                }
                Err(e) => Err(e.to_string()),
            };
            if let Some(tx) = state.agent_roundtrip_tx.lock().unwrap().take() {
                let _ = tx.send(res);
            }
        });
        Ok(true)
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        x11_auth_protocol: &str,
        x11_auth_cookie: &str,
        x11_screen_number: u32,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.state.x11_requests.fetch_add(1, Ordering::SeqCst);
        *self.state.x11_cookie_seen.lock().unwrap() = Some((
            x11_auth_protocol.to_string(),
            x11_auth_cookie.to_string(),
            x11_screen_number,
        ));
        session.channel_success(channel)?;

        let handle = session.handle();
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let res = match handle.channel_open_x11("127.0.0.1", 60420).await {
                Ok(ch) => {
                    let mut stream = ch.into_stream();
                    if let Err(e) = stream.write_all(b"X11_INIT").await {
                        Err(e.to_string())
                    } else {
                        let mut buf = [0u8; 8];
                        match stream.read_exact(&mut buf).await {
                            Ok(_) => Ok(buf.to_vec()),
                            Err(e) => Err(e.to_string()),
                        }
                    }
                }
                Err(e) => Err(e.to_string()),
            };
            if let Some(tx) = state.x11_roundtrip_tx.lock().unwrap().take() {
                let _ = tx.send(res);
            }
        });
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        if self.state.login_banner_mode {
            session.data(channel, b"Welcome!\r\nlogin: ".to_vec())?;
        } else {
            session.data(channel, b"READY\r\n".to_vec())?;
        }

        if self.state.open_unsolicited_agent {
            let handle = session.handle();
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                let res = handle
                    .channel_open_agent()
                    .await
                    .map(|_| b"OPENED".to_vec())
                    .map_err(|e| e.to_string());
                if let Some(tx) = state.agent_roundtrip_tx.lock().unwrap().take() {
                    let _ = tx.send(res);
                }
            });
        }

        if self.state.open_unsolicited_x11 {
            let handle = session.handle();
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                let res = handle
                    .channel_open_x11("127.0.0.1", 60420)
                    .await
                    .map(|_| b"OPENED".to_vec())
                    .map_err(|e| e.to_string());
                if let Some(tx) = state.x11_roundtrip_tx.lock().unwrap().take() {
                    let _ = tx.send(res);
                }
            });
        }

        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.line_buf.extend_from_slice(data);
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\r' || b == b'\n') {
            let line_bytes: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes)
                .trim_matches(&['\r', '\n'][..])
                .to_string();
            if line.is_empty() {
                continue;
            }
            self.state.received_lines.lock().unwrap().push(line.clone());
            if self.state.login_banner_mode {
                if line == "admin" {
                    session.data(channel, b"Enter Vault Token: ".to_vec())?;
                } else if line == "top-secret-42" {
                    session.data(channel, b"AUTH_OK\r\n".to_vec())?;
                } else if line == "DROP" {
                    if let Some(notify) = &self.state.drop_current_conn {
                        notify.notify_waiters();
                    }
                    let _ = session.close(channel);
                    return Err(russh::Error::Disconnect);
                }
            } else {
                session.data(channel, format!("ECHO:{line}\r\n").into_bytes())?;
            }
        }
        Ok(())
    }
}

struct AdvSshServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for AdvSshServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_adv_ssh_server(state: Arc<AdvServerState>) -> AdvSshServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();

    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

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
                    let cfg = Arc::clone(&cfg);
                    let st = Arc::clone(&srv_state);
                    tokio::spawn(async move {
                        let handler = AdvTestHandler {
                            state: Arc::clone(&st),
                            channels: HashMap::new(),
                            line_buf: Vec::new(),
                        };
                        let _ = server::run_stream(cfg, stream, handler).await;
                    });
                }
            }
        }
    });

    AdvSshServer {
        addr,
        host_pubkey,
        shutdown_tx: Some(shutdown_tx),
    }
}

struct FixedPasswordInteraction {
    password: String,
}

impl SshInteractionHandler for FixedPasswordInteraction {
    fn verify_unknown_host(&self, _prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async { HostKeyDecision::Reject })
    }

    fn prompt_key_passphrase(&self, _key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        Box::pin(async { None })
    }

    fn prompt_password(
        &self,
        _username: &str,
        _host: &str,
    ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        let pw = Zeroizing::new(self.password.clone());
        Box::pin(async move { Some(pw) })
    }

    fn prompt_keyboard_interactive(
        &self,
        _challenge: KeyboardInteractiveChallenge,
    ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
        Box::pin(async { None })
    }
}

fn base_connect_cfg(addr: SocketAddr, known_hosts_path: &Path) -> SshConnectConfig {
    SshConnectConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "ops".to_string(),
        auth: SshAuthMethod::Password {
            vault_secret_ref: None,
        },
        known_hosts_path: Some(known_hosts_path.to_path_buf()),
        proxy_jump: Vec::new(),
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 10_000,
        ..SshConnectConfig::default()
    }
}

fn default_server_state() -> AdvServerState {
    AdvServerState {
        shell_opens: AtomicUsize::new(0),
        agent_requests: AtomicUsize::new(0),
        x11_requests: AtomicUsize::new(0),
        x11_cookie_seen: Mutex::new(None),
        agent_roundtrip_tx: Mutex::new(None),
        x11_roundtrip_tx: Mutex::new(None),
        open_unsolicited_agent: false,
        open_unsolicited_x11: false,
        login_banner_mode: false,
        received_lines: Mutex::new(Vec::new()),
        drop_current_conn: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn agent_forwarding_bridges_channel_open_agent_to_unix_socket_and_rejects_when_disabled() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let agent_sock = temp.path().join("agent.sock");

    let agent_listener = UnixListener::bind(&agent_sock).unwrap();
    let (agent_seen_tx, agent_seen_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = agent_listener.accept().await {
            let mut buf = [0u8; 8];
            let _ = stream.read_exact(&mut buf).await;
            let _ = stream.write_all(b"AGENTOK!").await;
            let _ = agent_seen_tx.send(buf.to_vec());
        }
    });

    // Case A: ForwardAgent enabled.
    let (rt_tx, rt_rx) = oneshot::channel();
    let state = Arc::new(AdvServerState {
        agent_roundtrip_tx: Mutex::new(Some(rt_tx)),
        ..default_server_state()
    });
    let srv = spawn_adv_ssh_server(Arc::clone(&state)).await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let mut cfg = base_connect_cfg(srv.addr, &known_hosts);
    cfg.forward_agent = true;

    let interaction: Arc<dyn SshInteractionHandler> = Arc::new(FixedPasswordInteraction {
        password: "secret".to_string(),
    });
    let agent_connector: Arc<dyn termewnal_ssh::AgentConnector> =
        Arc::new(OsAgentConnector::from_unix_socket(&agent_sock));

    let session = SshSession::connect(cfg, None, interaction.clone(), agent_connector.clone())
        .await
        .expect("agent forwarding SSH connect");
    assert!(
        session.wait_for_output_async(Duration::from_secs(3), |s| s.visible_text().contains("READY")).await,
        "expected READY shell output"
    );

    let srv_reply = tokio::time::timeout(Duration::from_secs(3), rt_rx)
        .await
        .expect("agent roundtrip timeout")
        .unwrap()
        .expect("server channel_open_agent should succeed");
    assert_eq!(srv_reply, b"AGENTOK!");
    let seen_by_agent = tokio::time::timeout(Duration::from_secs(2), agent_seen_rx)
        .await
        .expect("mock agent read timeout")
        .unwrap();
    assert_eq!(seen_by_agent, b"AGENTREQ");
    assert_eq!(state.agent_requests.load(Ordering::SeqCst), 1);
    session.disconnect();

    // Case B: ForwardAgent disabled -> unsolicited channel_open_agent rejected.
    let (unsol_tx, unsol_rx) = oneshot::channel();
    let state_disabled = Arc::new(AdvServerState {
        agent_roundtrip_tx: Mutex::new(Some(unsol_tx)),
        open_unsolicited_agent: true,
        ..default_server_state()
    });
    let srv2 = spawn_adv_ssh_server(Arc::clone(&state_disabled)).await;
    record_known_host_path("127.0.0.1", srv2.addr.port(), &srv2.host_pubkey, &known_hosts).unwrap();

    let mut cfg2 = base_connect_cfg(srv2.addr, &known_hosts);
    cfg2.forward_agent = false;
    let session2 = SshSession::connect(cfg2, None, interaction, agent_connector)
        .await
        .expect("connect without agent forward");
    let unsol_res = tokio::time::timeout(Duration::from_secs(3), unsol_rx)
        .await
        .expect("unsolicited result timeout")
        .unwrap();
    assert!(
        unsol_res.is_err(),
        "server channel_open_agent must be rejected when forward_agent=false, got {unsol_res:?}"
    );
    assert_eq!(state_disabled.agent_requests.load(Ordering::SeqCst), 0);
    session2.disconnect();
}

#[cfg(unix)]
#[tokio::test]
async fn x11_forwarding_requests_x11_and_bridges_remote_x11_channel_to_local_display() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let x11_sock = temp.path().join("X42");

    let x11_listener = UnixListener::bind(&x11_sock).unwrap();
    let (x11_seen_tx, x11_seen_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = x11_listener.accept().await {
            let mut buf = [0u8; 8];
            let _ = stream.read_exact(&mut buf).await;
            let _ = stream.write_all(b"X11_ACK!").await;
            let _ = x11_seen_tx.send(buf.to_vec());
        }
    });

    // Case A: ForwardX11 enabled.
    let (rt_tx, rt_rx) = oneshot::channel();
    let state = Arc::new(AdvServerState {
        x11_roundtrip_tx: Mutex::new(Some(rt_tx)),
        ..default_server_state()
    });
    let srv = spawn_adv_ssh_server(Arc::clone(&state)).await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let mut cfg = base_connect_cfg(srv.addr, &known_hosts);
    cfg.forward_x11 = true;
    cfg.x11_display = Some(x11_sock.display().to_string());

    let interaction: Arc<dyn SshInteractionHandler> = Arc::new(FixedPasswordInteraction {
        password: "secret".to_string(),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> = Arc::new(OsAgentConnector::default());

    let session = SshSession::connect(cfg, None, interaction.clone(), agent.clone())
        .await
        .expect("X11 forwarding connect");
    assert!(
        session.wait_for_output_async(Duration::from_secs(3), |s| s.visible_text().contains("READY")).await
    );

    let x11_reply = tokio::time::timeout(Duration::from_secs(3), rt_rx)
        .await
        .expect("X11 roundtrip timeout")
        .unwrap()
        .expect("server channel_open_x11 should succeed");
    assert_eq!(x11_reply, b"X11_ACK!");
    let seen_by_x11 = tokio::time::timeout(Duration::from_secs(2), x11_seen_rx)
        .await
        .expect("mock X11 read timeout")
        .unwrap();
    assert_eq!(seen_by_x11, b"X11_INIT");

    let cookie = state.x11_cookie_seen.lock().unwrap().clone().unwrap();
    assert_eq!(cookie.0, "MIT-MAGIC-COOKIE-1");
    assert!(!cookie.1.is_empty());
    assert_eq!(state.x11_requests.load(Ordering::SeqCst), 1);
    session.disconnect();

    // Case B: ForwardX11 disabled -> unsolicited channel_open_x11 rejected.
    let (unsol_tx, unsol_rx) = oneshot::channel();
    let state_disabled = Arc::new(AdvServerState {
        x11_roundtrip_tx: Mutex::new(Some(unsol_tx)),
        open_unsolicited_x11: true,
        ..default_server_state()
    });
    let srv2 = spawn_adv_ssh_server(Arc::clone(&state_disabled)).await;
    record_known_host_path("127.0.0.1", srv2.addr.port(), &srv2.host_pubkey, &known_hosts).unwrap();

    let mut cfg2 = base_connect_cfg(srv2.addr, &known_hosts);
    cfg2.forward_x11 = false;
    let session2 = SshSession::connect(cfg2, None, interaction, agent)
        .await
        .expect("connect without X11");
    let unsol_res = tokio::time::timeout(Duration::from_secs(3), unsol_rx)
        .await
        .expect("unsolicited X11 result timeout")
        .unwrap();
    assert!(
        unsol_res.is_err(),
        "server channel_open_x11 must be rejected when forward_x11=false, got {unsol_res:?}"
    );
    assert_eq!(state_disabled.x11_requests.load(Ordering::SeqCst), 0);
    session2.disconnect();
}

#[tokio::test]
async fn proxy_command_expands_tokens_and_connects_ssh_transport() {
    assert_eq!(
        expand_proxy_command("nc -X 5 %h %p %r %%", "bastion.internal", 2222, "alice"),
        "nc -X 5 bastion.internal 2222 alice %"
    );

    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let marker = temp.path().join("proxy_user.txt");
    let relay_script = temp.path().join("proxy_relay.py");
    fs::write(
        &relay_script,
        r#"import os, socket, sys, threading
if sys.platform == "win32":
    import msvcrt
    msvcrt.setmode(sys.stdin.fileno(), os.O_BINARY)
    msvcrt.setmode(sys.stdout.fileno(), os.O_BINARY)
host, port, user, marker = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
with open(marker, "w") as f:
    f.write(f"{host}:{port}:{user}")
sock = socket.create_connection((host, port))
stdin_fd = sys.stdin.fileno()
stdout_fd = sys.stdout.fileno()
def write_all(fd, data):
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        view = view[written:]
def c2s():
    try:
        while True:
            d = os.read(stdin_fd, 4096)
            if not d:
                break
            sock.sendall(d)
    except Exception:
        pass
    try:
        sock.shutdown(socket.SHUT_WR)
    except Exception:
        pass
def s2c():
    try:
        while True:
            d = sock.recv(4096)
            if not d:
                break
            write_all(stdout_fd, d)
    except Exception:
        pass
t = threading.Thread(target=c2s, daemon=True)
t.start()
s2c()
"#,
    )
    .unwrap();

    let state = Arc::new(default_server_state());
    let srv = spawn_adv_ssh_server(Arc::clone(&state)).await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let mut cfg = base_connect_cfg(srv.addr, &known_hosts);
    #[cfg(unix)]
    {
        let c_src = temp.path().join("proxy_relay.c");
        let c_bin = temp.path().join("proxy_relay");
        fs::write(
            &c_src,
            r#"#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

static int write_all_fd(int fd, const char *buf, ssize_t len) {
    ssize_t off = 0;
    while (off < len) {
        ssize_t w = write(fd, buf + off, (size_t)(len - off));
        if (w <= 0) return -1;
        off += w;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 5) return 2;
    const char *host = argv[1];
    int port = atoi(argv[2]);
    const char *user = argv[3];
    const char *marker = argv[4];
    FILE *f = fopen(marker, "w");
    if (!f) return 3;
    fprintf(f, "%s:%d:%s", host, port, user);
    fclose(f);

    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) return 4;
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, host, &addr.sin_addr) != 1) return 5;
    if (connect(sock, (struct sockaddr *)&addr, sizeof(addr)) != 0) return 6;

    struct pollfd fds[2];
    fds[0].events = POLLIN;
    fds[1].fd = sock;
    fds[1].events = POLLIN;
    char buf[4096];
    int in_open = 1, sock_open = 1;
    while (sock_open) {
        fds[0].fd = in_open ? STDIN_FILENO : -1;
        if (poll(fds, 2, -1) <= 0) break;
        if (in_open && (fds[0].revents & (POLLIN | POLLHUP | POLLERR))) {
            ssize_t n = read(STDIN_FILENO, buf, sizeof(buf));
            if (n <= 0) {
                in_open = 0;
                shutdown(sock, SHUT_WR);
            } else if (write_all_fd(sock, buf, n) != 0) {
                sock_open = 0;
            }
        }
        if (sock_open && (fds[1].revents & (POLLIN | POLLHUP | POLLERR))) {
            ssize_t n = read(sock, buf, sizeof(buf));
            if (n <= 0 || write_all_fd(STDOUT_FILENO, buf, n) != 0) {
                sock_open = 0;
            }
        }
    }
    close(sock);
    return 0;
}
"#,
        )
        .unwrap();
        let cc_ok = std::process::Command::new("cc")
            .arg(&c_src)
            .arg("-o")
            .arg(&c_bin)
            .status()
            .is_ok_and(|s| s.success());
        if cc_ok {
            cfg.proxy_command = Some(format!(
                "'{}' %h %p %r '{}'",
                c_bin.display(),
                marker.display()
            ));
        } else {
            cfg.proxy_command = Some(format!(
                "python3 -u '{}' %h %p %r '{}'",
                relay_script.display(),
                marker.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        let py_launcher = [("python", vec![]), ("py", vec!["-3"]), ("python3", vec![])]
            .into_iter()
            .find_map(|(exe, extra_args)| {
                let mut probe = std::process::Command::new(exe);
                for arg in &extra_args {
                    probe.arg(arg);
                }
                probe.args(["-c", "import sys"]);
                probe
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|_| {
                        if extra_args.is_empty() {
                            exe.to_string()
                        } else {
                            format!("{exe} {}", extra_args.join(" "))
                        }
                    })
            })
            .unwrap_or_else(|| "python".to_string());
        cfg.proxy_command = Some(format!(
            "{py_launcher} -u \"{}\" %h %p %r \"{}\"",
            relay_script.display(),
            marker.display()
        ));
    }

    let interaction: Arc<dyn SshInteractionHandler> = Arc::new(FixedPasswordInteraction {
        password: "secret".to_string(),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> = Arc::new(OsAgentConnector::default());

    let session = SshSession::connect(cfg, None, interaction, agent)
        .await
        .expect("SSH session over ProxyCommand should connect");
    assert!(
        session.wait_for_output_async(Duration::from_secs(4), |s| s.visible_text().contains("READY")).await,
        "expected READY over ProxyCommand"
    );
    session.write_bytes(b"ping\r").unwrap();
    assert!(
        session.wait_for_output_async(Duration::from_secs(3), |s| s.visible_text().contains("ECHO:ping")).await,
        "expected bidirectional echo over ProxyCommand"
    );

    let recorded = fs::read_to_string(&marker).unwrap();
    assert_eq!(recorded, format!("127.0.0.1:{}:ops", srv.addr.port()));
    session.disconnect();
}

#[tokio::test]
async fn login_script_automates_multi_step_vault_secret_and_replays_on_reconnect() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let drop_notify = Arc::new(Notify::new());

    let state = Arc::new(AdvServerState {
        login_banner_mode: true,
        drop_current_conn: Some(Arc::clone(&drop_notify)),
        ..default_server_state()
    });
    let srv = spawn_adv_ssh_server(Arc::clone(&state)).await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    let vault = Arc::new(InMemorySecretStore::new());
    vault.set_secret("login/token", "top-secret-42").unwrap();

    let mut cfg = base_connect_cfg(srv.addr, &known_hosts);
    cfg.login_scripts = vec![
        LoginScriptStep {
            expect: "login:".to_string(),
            send: "admin".to_string(),
            is_regex: false,
            secret_ref: None,
            delay_ms: 0,
            append_newline: true,
        },
        LoginScriptStep {
            expect: r"Token:\s*$".to_string(),
            send: String::new(),
            is_regex: true,
            secret_ref: Some("login/token".to_string()),
            delay_ms: 15,
            append_newline: true,
        },
    ];

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    sup_cfg.backoff_min = Duration::from_millis(20);
    sup_cfg.backoff_max = Duration::from_millis(40);

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
    let (_tx_ev, _rx_ev) = mpsc::unbounded_channel::<termewnal_pty::PtyEvent>();

    let interaction: Arc<dyn SshInteractionHandler> = Arc::new(FixedPasswordInteraction {
        password: "secret".to_string(),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> = Arc::new(OsAgentConnector::default());

    let supervisor = ConnectionSupervisor::connect_on_buffer(
        cfg,
        sup_cfg,
        Arc::clone(&buffer),
        Some(vault),
        interaction,
        agent,
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if buffer.export_scrollback_text().contains("AUTH_OK") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        buffer.export_scrollback_text().contains("AUTH_OK"),
        "initial LoginScript execution failed; text={}",
        buffer.export_scrollback_text()
    );

    // Trigger drop via server side to exercise supervisor reconnect + LoginScript replay.
    supervisor.write_bytes(b"DROP\r").unwrap();

    let mut status_rx = supervisor.subscribe_status();
    let deadline2 = Instant::now() + Duration::from_secs(5);
    let mut saw_two_auth_ok = false;
    while Instant::now() < deadline2 {
        let text = buffer.export_scrollback_text();
        if text.matches("AUTH_OK").count() >= 2 && text.contains("Reconnected at") {
            saw_two_auth_ok = true;
            break;
        }
        let _ = tokio::time::timeout(Duration::from_millis(25), status_rx.changed()).await;
    }

    assert!(
        saw_two_auth_ok,
        "LoginScript should replay after reconnect; shell_opens={} lines={:?} status={:?} text={}",
        state.shell_opens.load(Ordering::SeqCst),
        *state.received_lines.lock().unwrap(),
        supervisor.status(),
        buffer.export_scrollback_text()
    );
    assert!(matches!(
        supervisor.status(),
        TransportStatus::Connected { .. }
    ));

    supervisor.disconnect();
}

async fn relay_bidirectional(mut client: tokio::net::TcpStream, mut upstream: tokio::net::TcpStream) {
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

#[tokio::test]
async fn socks5_and_http_connect_proxies_establish_authenticated_ssh_transport() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let state = Arc::new(default_server_state());
    let srv = spawn_adv_ssh_server(Arc::clone(&state)).await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &known_hosts).unwrap();

    // 1. Spawn local RFC 1928/1929 SOCKS5 proxy requiring username/password auth.
    let socks_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socks_port = socks_listener.local_addr().unwrap().port();
    let socks_handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = socks_listener.accept().await {
            tokio::spawn(async move {
                let mut hdr = [0u8; 2];
                if sock.read_exact(&mut hdr).await.is_err() {
                    return;
                }
                let mut methods = vec![0u8; hdr[1] as usize];
                if sock.read_exact(&mut methods).await.is_err() {
                    return;
                }
                if !methods.contains(&0x02) {
                    let _ = sock.write_all(&[0x05, 0xFF]).await;
                    return;
                }
                let _ = sock.write_all(&[0x05, 0x02]).await;

                let mut auth_hdr = [0u8; 2];
                if sock.read_exact(&mut auth_hdr).await.is_err() {
                    return;
                }
                let mut user = vec![0u8; auth_hdr[1] as usize];
                if sock.read_exact(&mut user).await.is_err() {
                    return;
                }
                let mut plen = [0u8; 1];
                if sock.read_exact(&mut plen).await.is_err() {
                    return;
                }
                let mut pass = vec![0u8; plen[0] as usize];
                if sock.read_exact(&mut pass).await.is_err() {
                    return;
                }
                if user != b"proxyuser" || pass != b"proxypass" {
                    let _ = sock.write_all(&[0x01, 0x01]).await;
                    return;
                }
                let _ = sock.write_all(&[0x01, 0x00]).await;

                let mut req_hdr = [0u8; 4];
                if sock.read_exact(&mut req_hdr).await.is_err() {
                    return;
                }
                let target_host = match req_hdr[3] {
                    0x01 => {
                        let mut ip = [0u8; 4];
                        if sock.read_exact(&mut ip).await.is_err() {
                            return;
                        }
                        std::net::Ipv4Addr::from(ip).to_string()
                    }
                    0x03 => {
                        let mut len = [0u8; 1];
                        if sock.read_exact(&mut len).await.is_err() {
                            return;
                        }
                        let mut dom = vec![0u8; len[0] as usize];
                        if sock.read_exact(&mut dom).await.is_err() {
                            return;
                        }
                        String::from_utf8_lossy(&dom).into_owned()
                    }
                    _ => return,
                };
                let mut port_buf = [0u8; 2];
                if sock.read_exact(&mut port_buf).await.is_err() {
                    return;
                }
                let target_port = u16::from_be_bytes(port_buf);
                let Ok(upstream) = tokio::net::TcpStream::connect((target_host.as_str(), target_port)).await else {
                    let _ = sock.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
                    return;
                };
                let _ = sock.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0]).await;
                relay_bidirectional(sock, upstream).await;
            });
        }
    });

    // 2. Spawn local HTTP CONNECT proxy validating Basic auth.
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http_listener.local_addr().unwrap().port();
    let http_handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = http_listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while let Ok(n) = sock.read(&mut b).await {
                    if n == 0 {
                        return;
                    }
                    buf.push(b[0]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let req_str = String::from_utf8_lossy(&buf);
                let first_line = req_str.lines().next().unwrap_or("");
                let Some(authority) = first_line
                    .strip_prefix("CONNECT ")
                    .and_then(|rest| rest.split_whitespace().next())
                else {
                    let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
                    return;
                };
                let has_valid_auth = req_str
                    .lines()
                    .any(|line| line == "Proxy-Authorization: Basic aHR0cHVzZXI6aHR0cHBhc3M=");
                if !has_valid_auth {
                    let _ = sock
                        .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                        .await;
                    return;
                }
                let Ok(upstream) = tokio::net::TcpStream::connect(authority).await else {
                    let _ = sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                let _ = sock
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await;
                relay_bidirectional(sock, upstream).await;
            });
        }
    });

    let vault = InMemorySecretStore::new();
    vault.set_secret("proxy/socks", "proxypass").unwrap();
    vault.set_secret("proxy/http", "httppass").unwrap();
    vault.set_secret("proxy/bad", "wrong").unwrap();

    let interaction: Arc<dyn SshInteractionHandler> = Arc::new(FixedPasswordInteraction {
        password: "secret".to_string(),
    });
    let agent: Arc<dyn termewnal_ssh::AgentConnector> = Arc::new(OsAgentConnector::default());

    // SOCKS5 end-to-end SSH session + echo
    let mut socks_cfg = base_connect_cfg(srv.addr, &known_hosts);
    socks_cfg.proxy = Some(SshProxyConfig {
        kind: SshProxyKind::Socks5,
        host: "127.0.0.1".to_string(),
        port: socks_port,
        username: Some("proxyuser".to_string()),
        vault_secret_ref: Some("proxy/socks".to_string()),
    });
    let socks_session = SshSession::connect(
        socks_cfg.clone(),
        Some(&vault),
        Arc::clone(&interaction),
        Arc::clone(&agent),
    )
    .await
    .expect("SSH session over SOCKS5 proxy should connect");
    assert!(
        socks_session
            .wait_for_output_async(Duration::from_secs(4), |s| s.visible_text().contains("READY"))
            .await
    );
    socks_session.write_bytes(b"ping\r").unwrap();
    assert!(
        socks_session
            .wait_for_output_async(Duration::from_secs(3), |s| s.visible_text().contains("ECHO:ping"))
            .await
    );
    socks_session.disconnect();

    // SOCKS5 auth rejection
    socks_cfg.proxy.as_mut().unwrap().vault_secret_ref = Some("proxy/bad".to_string());
    let err = match SshSession::connect(
        socks_cfg,
        Some(&vault),
        Arc::clone(&interaction),
        Arc::clone(&agent),
    )
    .await
    {
        Ok(_) => panic!("expected SOCKS5 auth rejection"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("authentication") || err.to_string().contains("SOCKS5"),
        "unexpected SOCKS5 error: {err}"
    );

    // HTTP CONNECT end-to-end SSH session + echo
    let mut http_cfg = base_connect_cfg(srv.addr, &known_hosts);
    http_cfg.proxy = Some(SshProxyConfig {
        kind: SshProxyKind::HttpConnect,
        host: "127.0.0.1".to_string(),
        port: http_port,
        username: Some("httpuser".to_string()),
        vault_secret_ref: Some("proxy/http".to_string()),
    });
    let http_session = SshSession::connect(
        http_cfg.clone(),
        Some(&vault),
        Arc::clone(&interaction),
        Arc::clone(&agent),
    )
    .await
    .expect("SSH session over HTTP CONNECT proxy should connect");
    assert!(
        http_session
            .wait_for_output_async(Duration::from_secs(4), |s| s.visible_text().contains("READY"))
            .await
    );
    http_session.write_bytes(b"viahttp\r").unwrap();
    assert!(
        http_session
            .wait_for_output_async(Duration::from_secs(3), |s| s.visible_text().contains("ECHO:viahttp"))
            .await
    );
    http_session.disconnect();

    // HTTP 407 rejection
    http_cfg.proxy.as_mut().unwrap().vault_secret_ref = Some("proxy/bad".to_string());
    let err = match SshSession::connect(http_cfg, Some(&vault), interaction, agent).await {
        Ok(_) => panic!("expected HTTP 407 rejection"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("407"),
        "unexpected HTTP proxy error: {err}"
    );

    socks_handle.abort();
    http_handle.abort();
}
