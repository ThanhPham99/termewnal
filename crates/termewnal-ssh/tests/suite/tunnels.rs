use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{SshAuthMethod, TunnelDirection, TunnelRule};
use termewnal_pty::TerminalBuffer;
use termewnal_ssh::{
    BoxFuture, ConnectionSupervisor, DefaultTransportConnector, HostKeyDecision, HostKeyPrompt,
    KeyboardInteractiveChallenge, OsAgentConnector, SshConnectConfig, SshError,
    SshInteractionHandler, StandaloneTransportSpec, SupervisorConfig, SystemTimeSource,
    TransportStatus, TunnelManager, TunnelState, record_known_host_path,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

struct TunnelServerState {
    direct_tcpip_opens: AtomicUsize,
    shell_opens: AtomicUsize,
}

struct TunnelTestHandler {
    state: Arc<TunnelServerState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
}

impl server::Handler for TunnelTestHandler {
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

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, b"READY\r\n".to_vec())?;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.state.direct_tcpip_opens.fetch_add(1, Ordering::SeqCst);
        let host = host_to_connect.to_string();
        let port = u16::try_from(port_to_connect).unwrap_or(0);
        tokio::spawn(async move {
            match TcpStream::connect((host.as_str(), port)).await {
                Ok(mut target_tcp) => {
                    reply.accept().await;
                    let mut ssh_stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut ssh_stream, &mut target_tcp).await;
                }
                Err(_) => {
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                }
            }
        });
        Ok(())
    }
}

struct TestSshServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<TunnelServerState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for TestSshServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_ssh_server() -> TestSshServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();

    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

    let state = Arc::new(TunnelServerState {
        direct_tcpip_opens: AtomicUsize::new(0),
        shell_opens: AtomicUsize::new(0),
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
                    let cfg = Arc::clone(&cfg);
                    let st = Arc::clone(&srv_state);
                    tokio::spawn(async move {
                        let handler = TunnelTestHandler {
                            state: st,
                            channels: HashMap::new(),
                        };
                        let _ = server::run_stream(cfg, stream, handler).await;
                    });
                }
            }
        }
    });

    TestSshServer {
        addr,
        host_pubkey,
        state,
        shutdown_tx: Some(shutdown_tx),
    }
}

struct EchoServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_echo_server(bind_addr: &str) -> EchoServer {
    let listener = TcpListener::bind(bind_addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { break; };
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        loop {
                            match stream.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if stream.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
    });

    EchoServer {
        addr,
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

fn build_connect_config(addr: SocketAddr, known_hosts_path: &Path) -> SshConnectConfig {
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
        scrollback_lines: 1000,
    }
}

async fn start_supervised_session(
    addr: SocketAddr,
    host_pubkey: &PublicKey,
    known_hosts_path: &Path,
) -> ConnectionSupervisor {
    record_known_host_path(&addr.ip().to_string(), addr.port(), host_pubkey, known_hosts_path)
        .unwrap();

    let connector = Arc::new(DefaultTransportConnector::new(
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    ));
    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    sup_cfg.backoff_min = Duration::from_millis(150);
    sup_cfg.backoff_max = Duration::from_millis(300);

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 1000));
    let sup = ConnectionSupervisor::connect_with_connector(
        build_connect_config(addr, known_hosts_path),
        sup_cfg,
        buffer,
        connector,
        Arc::new(SystemTimeSource),
    );

    let mut ready_rx = sup.subscribe_ready();
    tokio::time::timeout(Duration::from_secs(5), async {
        ready_rx.wait_for(|s| s.is_connected()).await.unwrap();
    })
    .await
    .expect("supervisor should connect");

    sup
}

#[tokio::test]
async fn local_tunnel_defaults_to_loopback_and_forwards_tcp_bytes_over_direct_tcpip() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let echo = spawn_echo_server("127.0.0.1:0").await;
    let ssh_srv = spawn_ssh_server().await;
    let sup = start_supervised_session(ssh_srv.addr, &ssh_srv.host_pubkey, &known_hosts).await;

    let manager = TunnelManager::new();
    let mut rule = TunnelRule::local("Echo Local", 0, "127.0.0.1", echo.addr.port());
    rule.bind_host = " ".to_string();

    let bound_addr = manager
        .start_session_tunnel(1, None, rule.clone(), sup.subscribe_ready())
        .await
        .expect("start local tunnel");

    assert!(
        bound_addr.ip().is_loopback(),
        "blank bind_host must default to loopback 127.0.0.1, got {bound_addr}"
    );

    let mut client = TcpStream::connect(bound_addr).await.expect("connect local tunnel");
    client.write_all(b"ping-local-tunnel\n").await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(3), client.read(&mut buf))
        .await
        .expect("echo response timeout")
        .unwrap();
    assert_eq!(&buf[..n], b"ping-local-tunnel\n");
    assert_eq!(ssh_srv.state.direct_tcpip_opens.load(Ordering::SeqCst), 1);

    let snap = manager.snapshot();
    assert_eq!(snap.tunnels.len(), 1);
    assert_eq!(snap.tunnels[0].direction, TunnelDirection::Local);
    assert_eq!(snap.tunnels[0].state, TunnelState::Active);

    assert!(manager.stop_tunnel(&rule.id));
    let stop_snap = manager.snapshot();
    assert_eq!(stop_snap.tunnels[0].state, TunnelState::Stopped);
    assert!(TcpStream::connect(bound_addr).await.is_err());

    sup.disconnect();
}

#[tokio::test]
async fn public_bind_requires_explicit_opt_in_for_zero_addr_and_succeeds_when_opted_in() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let echo = spawn_echo_server("127.0.0.1:0").await;
    let ssh_srv = spawn_ssh_server().await;
    let sup = start_supervised_session(ssh_srv.addr, &ssh_srv.host_pubkey, &known_hosts).await;

    let manager = TunnelManager::new();
    let mut rule = TunnelRule::local("Public DB", 0, "127.0.0.1", echo.addr.port());
    rule.bind_host = "0.0.0.0".to_string();
    rule.allow_public_bind = false;

    let err = manager
        .start_session_tunnel(1, None, rule.clone(), sup.subscribe_ready())
        .await
        .expect_err("0.0.0.0 without allow_public_bind must be refused");
    assert!(matches!(err, SshError::TunnelPublicBindDenied { .. }));
    assert!(manager.snapshot().tunnels.is_empty());

    rule.allow_public_bind = true;
    let bound_addr = manager
        .start_session_tunnel(1, None, rule.clone(), sup.subscribe_ready())
        .await
        .expect("0.0.0.0 with explicit opt-in should bind");
    assert!(bound_addr.ip().is_unspecified());

    let connect_addr = SocketAddr::from(([127, 0, 0, 1], bound_addr.port()));
    let mut client = TcpStream::connect(connect_addr).await.unwrap();
    client.write_all(b"opted-in-public\n").await.unwrap();
    let mut buf = [0u8; 64];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"opted-in-public\n");

    manager.stop_all();
    sup.disconnect();
}

async fn socks5_connect_and_echo(
    proxy_addr: SocketAddr,
    atyp_payload: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut greeting_reply = [0u8; 2];
    client.read_exact(&mut greeting_reply).await.unwrap();
    assert_eq!(greeting_reply, [0x05, 0x00]);

    let mut req = Vec::with_capacity(4 + atyp_payload.len());
    req.extend_from_slice(&[0x05, 0x01, 0x00]);
    req.extend_from_slice(atyp_payload);
    client.write_all(&req).await.unwrap();

    let mut reply = [0u8; 10];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[0..4], [0x05, 0x00, 0x00, 0x01]);

    client.write_all(payload).await.unwrap();
    let mut out = vec![0u8; payload.len()];
    client.read_exact(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn dynamic_socks5_tunnel_forwards_ipv4_domain_ipv6_and_handles_protocol_errors() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let echo_v4 = spawn_echo_server("127.0.0.1:0").await;
    let echo_v6 = TcpListener::bind("[::1]:0").await.ok().map(|l| {
        let addr = l.local_addr().unwrap();
        let (tx, mut rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    accepted = l.accept() => {
                        let Ok((mut stream, _)) = accepted else { break; };
                        tokio::spawn(async move {
                            let mut buf = [0u8; 4096];
                            loop {
                                match stream.read(&mut buf).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        if stream.write_all(&buf[..n]).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
            }
        });
        (addr, tx)
    });

    let ssh_srv = spawn_ssh_server().await;
    let sup = start_supervised_session(ssh_srv.addr, &ssh_srv.host_pubkey, &known_hosts).await;

    let manager = TunnelManager::new();
    let rule = TunnelRule::dynamic_socks5("SOCKS5 Proxy", 0);
    let proxy_addr = manager
        .start_session_tunnel(1, None, rule.clone(), sup.subscribe_ready())
        .await
        .expect("start dynamic socks5 tunnel");

    // 1. ATYP 0x01: IPv4 127.0.0.1
    let mut v4_req = vec![0x01, 127, 0, 0, 1];
    v4_req.extend_from_slice(&echo_v4.addr.port().to_be_bytes());
    let echoed_v4 = socks5_connect_and_echo(proxy_addr, &v4_req, b"socks5-ipv4-ok").await;
    assert_eq!(echoed_v4, b"socks5-ipv4-ok");

    // 2. ATYP 0x03: Domain name ("localhost")
    let host = b"localhost";
    let mut dom_req = vec![0x03, host.len() as u8];
    dom_req.extend_from_slice(host);
    dom_req.extend_from_slice(&echo_v4.addr.port().to_be_bytes());
    let echoed_dom = socks5_connect_and_echo(proxy_addr, &dom_req, b"socks5-domain-ok").await;
    assert_eq!(echoed_dom, b"socks5-domain-ok");

    // 3. ATYP 0x04: IPv6 ::1 (when host supports ::1 loopback)
    if let Some((v6_addr, _v6_shutdown)) = &echo_v6 {
        let mut v6_req = vec![0x04];
        v6_req.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        v6_req.extend_from_slice(&v6_addr.port().to_be_bytes());
        let echoed_v6 = socks5_connect_and_echo(proxy_addr, &v6_req, b"socks5-ipv6-ok").await;
        assert_eq!(echoed_v6, b"socks5-ipv6-ok");
    }

    // 4. Unsupported auth method (0x02 user/pass only) -> replies [0x05, 0xFF].
    {
        let mut c = TcpStream::connect(proxy_addr).await.unwrap();
        c.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        let mut rep = [0u8; 2];
        c.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep, [0x05, 0xFF]);
    }

    // 5. Unsupported command (BIND 0x02) -> replies [0x05, 0x07, ...].
    {
        let mut c = TcpStream::connect(proxy_addr).await.unwrap();
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut greet = [0u8; 2];
        c.read_exact(&mut greet).await.unwrap();
        assert_eq!(greet, [0x05, 0x00]);
        c.write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[0..2], [0x05, 0x07]);
    }

    // 6. Unreachable remote port -> replies connection refused (0x05).
    {
        let unused_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = unused_listener.local_addr().unwrap().port();
        drop(unused_listener);

        let mut c = TcpStream::connect(proxy_addr).await.unwrap();
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut greet = [0u8; 2];
        c.read_exact(&mut greet).await.unwrap();
        let mut req = vec![0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1];
        req.extend_from_slice(&closed_port.to_be_bytes());
        c.write_all(&req).await.unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[0..2], [0x05, 0x05]);
    }

    manager.stop_all();
    sup.disconnect();
}

struct FaultProxy {
    addr: SocketAddr,
    accept_new: Arc<AtomicBool>,
    drop_tx: watch::Sender<u64>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl FaultProxy {
    fn drop_active_connections(&self) {
        let next = *self.drop_tx.borrow() + 1;
        let _ = self.drop_tx.send(next);
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_fault_proxy(target: SocketAddr) -> FaultProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_new = Arc::new(AtomicBool::new(true));
    let (drop_tx, drop_rx) = watch::channel(0_u64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let accept_flag = Arc::clone(&accept_new);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = listener.accept() => {
                    let Ok((client, _)) = res else { break; };
                    if !accept_flag.load(Ordering::SeqCst) {
                        drop(client);
                        continue;
                    }
                    let Ok(upstream) = TcpStream::connect(target).await else {
                        drop(client);
                        continue;
                    };
                    let mut drop_rx = drop_rx.clone();
                    let conn_gen = *drop_rx.borrow_and_update();
                    tokio::spawn(async move {
                        let (mut c_rx, mut c_tx) = client.into_split();
                        let (mut u_rx, mut u_tx) = upstream.into_split();
                        let mut c_buf = [0u8; 8192];
                        let mut u_buf = [0u8; 8192];
                        loop {
                            if *drop_rx.borrow_and_update() != conn_gen {
                                break;
                            }
                            tokio::select! {
                                _ = drop_rx.changed() => break,
                                n = c_rx.read(&mut c_buf) => {
                                    match n {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if u_tx.write_all(&c_buf[..n]).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                                n = u_rx.read(&mut u_buf) => {
                                    match n {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if c_tx.write_all(&u_buf[..n]).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
    });

    FaultProxy {
        addr,
        accept_new,
        drop_tx,
        shutdown_tx: Some(shutdown_tx),
    }
}

async fn start_supervised_with_config(
    addr: SocketAddr,
    host_pubkey: &PublicKey,
    known_hosts_path: &Path,
    sup_cfg: SupervisorConfig,
) -> ConnectionSupervisor {
    record_known_host_path(&addr.ip().to_string(), addr.port(), host_pubkey, known_hosts_path)
        .unwrap();

    let connector = Arc::new(DefaultTransportConnector::new(
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    ));

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 1000));
    let sup = ConnectionSupervisor::connect_with_connector(
        build_connect_config(addr, known_hosts_path),
        sup_cfg,
        buffer,
        connector,
        Arc::new(SystemTimeSource),
    );

    let mut ready_rx = sup.subscribe_ready();
    tokio::time::timeout(Duration::from_secs(5), async {
        ready_rx.wait_for(|s| s.is_connected()).await.unwrap();
    })
    .await
    .expect("supervisor should connect");

    sup
}

#[tokio::test]
async fn local_and_socks5_tunnels_preserve_listener_without_eaddrinuse_and_hold_then_flush_mid_reconnect() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let echo = spawn_echo_server("127.0.0.1:0").await;
    let ssh_srv = spawn_ssh_server().await;
    let proxy = spawn_fault_proxy(ssh_srv.addr).await;

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    sup_cfg.backoff_min = Duration::from_millis(600);
    sup_cfg.backoff_max = Duration::from_millis(900);

    let sup = start_supervised_with_config(
        proxy.addr,
        &ssh_srv.host_pubkey,
        &known_hosts,
        sup_cfg,
    )
    .await;

    let manager = TunnelManager::new();
    let mut tunnel_sub = manager.subscribe();

    let local_rule = TunnelRule::local("DB Local", 0, "127.0.0.1", echo.addr.port());
    let local_addr = manager
        .start_session_tunnel(1, None, local_rule.clone(), sup.subscribe_ready())
        .await
        .expect("start local tunnel");

    let socks_rule = TunnelRule::dynamic_socks5("SOCKS5", 0);
    let socks_addr = manager
        .start_session_tunnel(1, None, socks_rule.clone(), sup.subscribe_ready())
        .await
        .expect("start socks5 tunnel");

    // 1. Verify initial connectivity on both tunnels before disconnect.
    {
        let mut c = TcpStream::connect(local_addr).await.unwrap();
        c.write_all(b"before-drop").await.unwrap();
        let mut buf = [0u8; 11];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"before-drop");
    }

    // 2. Simulate abrupt network drop while holding new reconnects briefly.
    proxy.accept_new.store(false, Ordering::SeqCst);
    proxy.drop_active_connections();

    let mut sup_ready = sup.subscribe_ready();
    tokio::time::timeout(Duration::from_secs(3), async {
        sup_ready
            .wait_for(|s| matches!(s.status, TransportStatus::Reconnecting { .. }))
            .await
            .unwrap();
    })
    .await
    .expect("supervisor must enter Reconnecting");

    // Listener addresses remain bound during Reconnecting (no unbind/rebind -> zero EADDRINUSE).
    tokio::time::timeout(Duration::from_secs(2), async {
        tunnel_sub
            .wait_for(|snap| {
                snap.tunnels.iter().all(|t| {
                    t.state == TunnelState::Reconnecting && t.bound_addr.is_some()
                })
            })
            .await
            .unwrap();
    })
    .await
    .expect("tunnels enter Reconnecting while keeping listeners bound");

    // 3. Connect local TCP client and SOCKS5 client MID-RECONNECT; write bytes immediately.
    let mut held_local = TcpStream::connect(local_addr).await.unwrap();
    held_local.write_all(b"held-during-drop\n").await.unwrap();

    let mut held_socks = TcpStream::connect(socks_addr).await.unwrap();
    held_socks.write_all(&[0x05, 0x01, 0x00]).await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        tunnel_sub
            .wait_for(|snap| {
                snap.holding_count == 2
                    && snap.tunnels.iter().all(|t| {
                        t.state == TunnelState::Holding && t.held_clients == 1
                    })
            })
            .await
            .unwrap();
    })
    .await
    .expect("both held clients surface Holding state");

    // 4. Allow reconnect and verify seamless recovery + flush on the exact same listener ports!
    proxy.accept_new.store(true, Ordering::SeqCst);
    sup.reconnect_now();

    tokio::time::timeout(Duration::from_secs(5), async {
        sup_ready
            .wait_for(|s| s.is_connected() && s.epoch >= 2)
            .await
            .unwrap();
    })
    .await
    .expect("supervisor reconnects cleanly");

    let mut local_echo_buf = [0u8; 17];
    tokio::time::timeout(Duration::from_secs(3), held_local.read_exact(&mut local_echo_buf))
        .await
        .expect("held local client should flush after reconnect")
        .unwrap();
    assert_eq!(&local_echo_buf, b"held-during-drop\n");

    let mut greet_reply = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(3), held_socks.read_exact(&mut greet_reply))
        .await
        .expect("held socks5 client should complete greeting after reconnect")
        .unwrap();
    assert_eq!(greet_reply, [0x05, 0x00]);

    let mut req = vec![0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1];
    req.extend_from_slice(&echo.addr.port().to_be_bytes());
    held_socks.write_all(&req).await.unwrap();
    let mut rep = [0u8; 10];
    held_socks.read_exact(&mut rep).await.unwrap();
    assert_eq!(rep[0..2], [0x05, 0x00]);

    held_socks.write_all(b"socks-after-reconnect").await.unwrap();
    let mut socks_echo = [0u8; 21];
    held_socks.read_exact(&mut socks_echo).await.unwrap();
    assert_eq!(&socks_echo, b"socks-after-reconnect");

    tokio::time::timeout(Duration::from_secs(2), async {
        tunnel_sub
            .wait_for(|snap| {
                snap.active_count == 2
                    && snap.tunnels.iter().all(|t| {
                        t.state == TunnelState::Active
                            && t.held_clients == 0
                            && (t.bound_addr == Some(local_addr) || t.bound_addr == Some(socks_addr))
                    })
            })
            .await
            .unwrap();
    })
    .await
    .expect("tunnels return to Active on the same bound ports");

    manager.stop_all();
    sup.disconnect();
}

#[tokio::test]
async fn held_client_resets_cleanly_on_grace_expiry_or_reconnect_cancel() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let echo = spawn_echo_server("127.0.0.1:0").await;
    let ssh_srv = spawn_ssh_server().await;
    let proxy = spawn_fault_proxy(ssh_srv.addr).await;

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    sup_cfg.backoff_min = Duration::from_secs(10);
    sup_cfg.backoff_max = Duration::from_secs(20);

    let sup = start_supervised_with_config(
        proxy.addr,
        &ssh_srv.host_pubkey,
        &known_hosts,
        sup_cfg,
    )
    .await;

    let manager = TunnelManager::new();

    // Case A: 1-second grace timeout expires while Transport remains Reconnecting -> client resets cleanly,
    // while listener stays bound so a later client succeeds once Transport reconnects.
    let mut short_grace_rule = TunnelRule::local("Short Grace", 0, "127.0.0.1", echo.addr.port());
    short_grace_rule.hold_grace_secs = Some(1);
    let short_addr = manager
        .start_session_tunnel(1, None, short_grace_rule.clone(), sup.subscribe_ready())
        .await
        .unwrap();

    // Case B: Default 10-second grace timeout -> cancelled reconnect immediately resets held client!
    let cancel_rule = TunnelRule::local("Cancel Grace", 0, "127.0.0.1", echo.addr.port());
    let cancel_addr = manager
        .start_session_tunnel(1, None, cancel_rule.clone(), sup.subscribe_ready())
        .await
        .unwrap();

    proxy.accept_new.store(false, Ordering::SeqCst);
    proxy.drop_active_connections();

    let mut sup_ready = sup.subscribe_ready();
    tokio::time::timeout(Duration::from_secs(3), async {
        sup_ready
            .wait_for(|s| matches!(s.status, TransportStatus::Reconnecting { .. }))
            .await
            .unwrap();
    })
    .await
    .unwrap();

    let started = Instant::now();
    let mut short_client = TcpStream::connect(short_addr).await.unwrap();
    short_client.write_all(b"will-expire").await.unwrap();
    let mut buf = [0u8; 32];
    let short_res = tokio::time::timeout(Duration::from_secs(3), short_client.read(&mut buf))
        .await
        .expect("short grace client must reset rather than hang");
    assert!(matches!(short_res, Ok(0) | Err(_)));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(800) && elapsed < Duration::from_secs(3),
        "expected ~1s hold expiry, took {elapsed:?}"
    );

    // Listener for short_grace_rule must STILL be bound in Reconnecting!
    assert_eq!(
        manager
            .snapshot()
            .tunnels
            .iter()
            .find(|t| t.id == short_grace_rule.id)
            .and_then(|t| t.bound_addr),
        Some(short_addr),
        "listener stays bound after single client grace timeout"
    );

    // Now test Case B: connect to cancel_addr (10s grace) and immediately cancel reconnect.
    let mut cancel_client = TcpStream::connect(cancel_addr).await.unwrap();
    cancel_client.write_all(b"will-cancel").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cancel_started = Instant::now();
    sup.cancel_reconnect();
    let cancel_res = tokio::time::timeout(Duration::from_secs(2), cancel_client.read(&mut buf))
        .await
        .expect("cancelled reconnect must reset held client promptly");
    assert!(matches!(cancel_res, Ok(0) | Err(_)));
    assert!(
        cancel_started.elapsed() < Duration::from_secs(2),
        "cancelled reconnect reset took too long: {:?}",
        cancel_started.elapsed()
    );

    manager.stop_all();
    sup.disconnect();
}

#[tokio::test]
async fn standalone_tunnel_runs_without_pty_shell_recovers_after_drop_and_tracks_active_count() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let echo = spawn_echo_server("127.0.0.1:0").await;
    let ssh_srv = spawn_ssh_server().await;
    let proxy = spawn_fault_proxy(ssh_srv.addr).await;

    record_known_host_path(
        &proxy.addr.ip().to_string(),
        proxy.addr.port(),
        &ssh_srv.host_pubkey,
        &known_hosts,
    )
    .unwrap();

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    sup_cfg.backoff_min = Duration::from_millis(500);
    sup_cfg.backoff_max = Duration::from_millis(800);

    let spec = StandaloneTransportSpec::new(
        build_connect_config(proxy.addr, &known_hosts),
        sup_cfg,
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );

    let manager = TunnelManager::new();
    let mut sub = manager.subscribe();

    let mut rule = TunnelRule::local("Standalone DB", 0, "127.0.0.1", echo.addr.port());
    rule.standalone = true;
    let bound_addr = manager
        .start_standalone_tunnel(Some("prof-standalone".to_string()), rule.clone(), spec)
        .await
        .expect("start standalone tunnel");

    tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|snap| {
            snap.active_standalone_count == 1
                && snap.active_count == 1
                && snap.tunnels[0].state == TunnelState::Active
        })
        .await
        .unwrap();
    })
    .await
    .expect("standalone tunnel reaches Active");

    // Standalone tunnel must NOT open any interactive PTY shell session.
    assert_eq!(ssh_srv.state.shell_opens.load(Ordering::SeqCst), 0);

    // Verify forwarding works.
    {
        let mut c = TcpStream::connect(bound_addr).await.unwrap();
        c.write_all(b"standalone-ping").await.unwrap();
        let mut buf = [0u8; 15];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"standalone-ping");
    }

    // Drop transport and connect local client mid-reconnect; hold and flush once recovered.
    proxy.accept_new.store(false, Ordering::SeqCst);
    proxy.drop_active_connections();

    tokio::time::timeout(Duration::from_secs(3), async {
        sub.wait_for(|snap| snap.reconnecting_count == 1 && snap.active_standalone_count == 1)
            .await
            .unwrap();
    })
    .await
    .expect("standalone tunnel transitions to Reconnecting while staying counted as active");

    let mut held_client = TcpStream::connect(bound_addr).await.unwrap();
    held_client.write_all(b"standalone-held\n").await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        sub.wait_for(|snap| snap.holding_count == 1 && snap.tunnels[0].state == TunnelState::Holding)
            .await
            .unwrap();
    })
    .await
    .expect("standalone tunnel surfaces Holding state");

    proxy.accept_new.store(true, Ordering::SeqCst);
    manager.reconnect_tunnel(&rule.id).unwrap();

    let mut echo_buf = [0u8; 16];
    tokio::time::timeout(Duration::from_secs(5), held_client.read_exact(&mut echo_buf))
        .await
        .expect("standalone held client flushes on recovery")
        .unwrap();
    assert_eq!(&echo_buf, b"standalone-held\n");
    assert_eq!(ssh_srv.state.shell_opens.load(Ordering::SeqCst), 0);

    assert!(manager.stop_tunnel(&rule.id));
    let final_snap = manager.snapshot();
    assert_eq!(final_snap.active_standalone_count, 0);
    assert_eq!(final_snap.tunnels[0].state, TunnelState::Stopped);
    assert!(TcpStream::connect(bound_addr).await.is_err());
}
