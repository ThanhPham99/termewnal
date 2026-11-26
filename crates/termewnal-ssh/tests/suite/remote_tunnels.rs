use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{SshAuthMethod, TunnelRule};
use termewnal_pty::TerminalBuffer;
use termewnal_ssh::{
    BoxFuture, ConnectionSupervisor, DefaultTransportConnector, HostKeyDecision, HostKeyPrompt,
    KeyboardInteractiveChallenge, OsAgentConnector, RemoteForwardBackoffConfig, SshConnectConfig,
    SshInteractionHandler, StandaloneTransportSpec, SupervisorConfig, SystemTimeSource,
    TunnelManager, TunnelState, record_known_host_path,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteServerEvent {
    TcpIpForwardRequested {
        address: String,
        requested_port: u32,
        accepted: bool,
        allocated_port: u32,
    },
    CancelTcpIpForward {
        address: String,
        port: u32,
    },
    TransportClosed {
        conn_id: usize,
    },
}

struct ActiveRemoteListener {
    shutdown_tx: oneshot::Sender<()>,
}

struct RemoteTestServerState {
    shell_opens: AtomicUsize,
    next_conn_id: AtomicUsize,
    events: Mutex<Vec<RemoteServerEvent>>,
    active_listeners: Mutex<HashMap<u16, ActiveRemoteListener>>,
    stale_ports: Mutex<HashSet<u16>>,
    deny_next_forward_counts: Mutex<HashMap<u16, usize>>,
}

impl RemoteTestServerState {
    fn events(&self) -> Vec<RemoteServerEvent> {
        self.events.lock().unwrap().clone()
    }

    fn release_stale_port(&self, port: u16) {
        self.stale_ports.lock().unwrap().remove(&port);
        if let Some(listener) = self.active_listeners.lock().unwrap().remove(&port) {
            let _ = listener.shutdown_tx.send(());
        }
    }
}

struct RemoteTestHandler {
    state: Arc<RemoteTestServerState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    owned_ports: Vec<u16>,
    hold_stale_on_ungraceful_drop: bool,
}

impl RemoteTestHandler {
    fn cleanup_on_exit(&mut self) {
        for port in self.owned_ports.drain(..) {
            if self.hold_stale_on_ungraceful_drop {
                self.state.stale_ports.lock().unwrap().insert(port);
            }
            if let Some(listener) = self.state.active_listeners.lock().unwrap().remove(&port) {
                let _ = listener.shutdown_tx.send(());
            }
        }
    }
}

impl Drop for RemoteTestHandler {
    fn drop(&mut self) {
        self.cleanup_on_exit();
    }
}

impl server::Handler for RemoteTestHandler {
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
        session.data(channel, b"REMOTE_TUNNEL_READY\r\n".to_vec())?;
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        let requested_port = *port;
        let requested_u16 = u16::try_from(requested_port).unwrap_or(0);

        if requested_u16 > 0 {
            let mut deny_map = self.state.deny_next_forward_counts.lock().unwrap();
            if let Some(rem) = deny_map.get_mut(&requested_u16)
                && *rem > 0
            {
                *rem -= 1;
                self.state.events.lock().unwrap().push(
                    RemoteServerEvent::TcpIpForwardRequested {
                        address: address.to_string(),
                        requested_port,
                        accepted: false,
                        allocated_port: 0,
                    },
                );
                return Ok(false);
            }
            if self.state.stale_ports.lock().unwrap().contains(&requested_u16)
                || self.state.active_listeners.lock().unwrap().contains_key(&requested_u16)
            {
                self.state.events.lock().unwrap().push(
                    RemoteServerEvent::TcpIpForwardRequested {
                        address: address.to_string(),
                        requested_port,
                        accepted: false,
                        allocated_port: 0,
                    },
                );
                return Ok(false);
            }
        }

        let listener = match TcpListener::bind(("127.0.0.1", requested_u16)).await {
            Ok(l) => l,
            Err(_) => {
                self.state.events.lock().unwrap().push(
                    RemoteServerEvent::TcpIpForwardRequested {
                        address: address.to_string(),
                        requested_port,
                        accepted: false,
                        allocated_port: 0,
                    },
                );
                return Ok(false);
            }
        };

        let bound_port = listener.local_addr().unwrap().port();
        if requested_port == 0 {
            *port = u32::from(bound_port);
        }

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        self.state
            .active_listeners
            .lock()
            .unwrap()
            .insert(bound_port, ActiveRemoteListener { shutdown_tx });
        self.owned_ports.push(bound_port);

        self.state.events.lock().unwrap().push(
            RemoteServerEvent::TcpIpForwardRequested {
                address: address.to_string(),
                requested_port,
                accepted: true,
                allocated_port: u32::from(bound_port),
            },
        );

        let sess_handle = session.handle();
        let connected_address = address.to_string();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((mut tcp, peer)) = accepted else { break; };
                        let handle = sess_handle.clone();
                        let addr = connected_address.clone();
                        tokio::spawn(async move {
                            let Ok(ch) = handle
                                .channel_open_forwarded_tcpip(
                                    addr,
                                    u32::from(bound_port),
                                    peer.ip().to_string(),
                                    u32::from(peer.port()),
                                )
                                .await
                            else {
                                return;
                            };
                            let mut ssh_stream = ch.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut ssh_stream).await;
                        });
                    }
                }
            }
        });

        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        let port_u16 = u16::try_from(port).unwrap_or(0);
        self.owned_ports.retain(|&p| p != port_u16);
        self.state.stale_ports.lock().unwrap().remove(&port_u16);
        if let Some(listener) = self.state.active_listeners.lock().unwrap().remove(&port_u16) {
            let _ = listener.shutdown_tx.send(());
        }
        self.state.events.lock().unwrap().push(
            RemoteServerEvent::CancelTcpIpForward {
                address: address.to_string(),
                port,
            },
        );
        Ok(true)
    }
}

struct RemoteTestSshServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<RemoteTestServerState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for RemoteTestSshServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_remote_ssh_server(hold_stale_on_ungraceful_drop: bool) -> RemoteTestSshServer {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_pubkey = host_key.public_key().clone();

    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.auth_rejection_time_initial = Some(Duration::from_millis(1));
    cfg.inactivity_timeout = None;
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(host_key);
    let cfg = Arc::new(cfg);

    let state = Arc::new(RemoteTestServerState {
        shell_opens: AtomicUsize::new(0),
        next_conn_id: AtomicUsize::new(1),
        events: Mutex::new(Vec::new()),
        active_listeners: Mutex::new(HashMap::new()),
        stale_ports: Mutex::new(HashSet::new()),
        deny_next_forward_counts: Mutex::new(HashMap::new()),
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
                        let conn_id = st.next_conn_id.fetch_add(1, Ordering::SeqCst);
                        let handler = RemoteTestHandler {
                            state: Arc::clone(&st),
                            channels: HashMap::new(),
                            owned_ports: Vec::new(),
                            hold_stale_on_ungraceful_drop,
                        };
                        if let Ok(running_session) = server::run_stream(cfg, stream, handler).await {
                            let _ = running_session.await;
                        }
                        st.events.lock().unwrap().push(RemoteServerEvent::TransportClosed { conn_id });
                    });
                }
            }
        }
    });

    RemoteTestSshServer {
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

async fn spawn_echo_server() -> EchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        ..SshConnectConfig::default()
    }
}

async fn reserve_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn remote_session_and_standalone_tunnels_forward_traffic_and_cancel_before_disconnect() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let local_echo = spawn_echo_server().await;
    let ssh_srv = spawn_remote_ssh_server(false).await;

    record_known_host_path(
        &ssh_srv.addr.ip().to_string(),
        ssh_srv.addr.port(),
        &ssh_srv.host_pubkey,
        &known_hosts,
    )
    .unwrap();

    // Part A: Session-bound Remote (-R) tunnel with explicit remote bind port.
    let explicit_remote_port = reserve_free_port().await;
    let connector = Arc::new(DefaultTransportConnector::new(
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    ));
    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);

    let sup = ConnectionSupervisor::connect_with_connector(
        build_connect_config(ssh_srv.addr, &known_hosts),
        sup_cfg.clone(),
        Arc::new(TerminalBuffer::new(80, 24, 1000)),
        Arc::clone(&connector) as Arc<_>,
        Arc::new(SystemTimeSource),
    );
    let mut ready_rx = sup.subscribe_ready();
    tokio::time::timeout(Duration::from_secs(5), async {
        ready_rx.wait_for(|s| s.is_connected()).await.unwrap();
    })
    .await
    .expect("supervisor connects");

    let manager = TunnelManager::new();
    let mut sub = manager.subscribe();

    let session_rule = TunnelRule::remote(
        "Remote Explicit",
        explicit_remote_port,
        "127.0.0.1",
        local_echo.addr.port(),
    );
    manager
        .start_session_tunnel(1, None, session_rule.clone(), sup.subscribe_ready())
        .await
        .expect("start session remote tunnel");

    let active_snap = tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| {
            s.tunnels.iter().any(|t| {
                t.id == session_rule.id
                    && t.state == TunnelState::Active
                    && t.remote_bound_port == Some(explicit_remote_port)
            })
        })
        .await
        .unwrap()
        .clone()
    })
    .await
    .expect("session remote tunnel reaches Active");
    assert_eq!(active_snap.active_count, 1);

    // Connect to the SSH server's remote forwarded port -> bytes flow through forwarded-tcpip to local_echo!
    {
        let mut remote_client = TcpStream::connect(("127.0.0.1", explicit_remote_port))
            .await
            .unwrap();
        remote_client.write_all(b"remote-session-ok").await.unwrap();
        let mut reply = [0u8; 17];
        remote_client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"remote-session-ok");
    }

    // Stopping session tunnels asynchronously awaits cancel_tcpip_forward BEFORE sup.disconnect().
    let stopped_count = manager.stop_session_tunnels_async(1).await;
    assert_eq!(stopped_count, 1);
    sup.disconnect();

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let evs = ssh_srv.state.events();
            if evs
                .iter()
                .any(|e| matches!(e, RemoteServerEvent::TransportClosed { conn_id: 1 }))
            {
                break evs;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("server records session transport close");

    let events_after_a = ssh_srv.state.events();
    let cancel_idx = events_after_a
        .iter()
        .position(|e| {
            matches!(
                e,
                RemoteServerEvent::CancelTcpIpForward { port, .. }
                    if *port == u32::from(explicit_remote_port)
            )
        })
        .expect("cancel_tcpip_forward must be received by server");
    let close_idx = events_after_a
        .iter()
        .position(|e| matches!(e, RemoteServerEvent::TransportClosed { conn_id: 1 }))
        .expect("transport close recorded");
    assert!(
        cancel_idx < close_idx,
        "cancel_tcpip_forward ({cancel_idx}) must complete BEFORE transport close ({close_idx}): {events_after_a:?}"
    );

    // Part B: Standalone Remote (-R) tunnel with bind_host = "0.0.0.0" and ephemeral port 0.
    let shell_opens_before = ssh_srv.state.shell_opens.load(Ordering::SeqCst);
    let mut standalone_rule =
        TunnelRule::remote("Standalone Remote", 0, "127.0.0.1", local_echo.addr.port());
    standalone_rule.bind_host = "0.0.0.0".to_string();
    standalone_rule.standalone = true;
    standalone_rule.allow_public_bind = false; // Must NOT be blocked by local 0.0.0.0 guard.

    let spec = StandaloneTransportSpec::new(
        build_connect_config(ssh_srv.addr, &known_hosts),
        sup_cfg,
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );
    manager
        .start_standalone_tunnel(Some("prof-remote".to_string()), standalone_rule.clone(), spec)
        .await
        .expect("start standalone remote tunnel");

    let standalone_snap = tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| {
            s.active_standalone_count == 1
                && s.tunnels.iter().any(|t| {
                    t.id == standalone_rule.id
                        && t.state == TunnelState::Active
                        && t.remote_bound_port.is_some_and(|p| p > 0)
                })
        })
        .await
        .unwrap()
        .clone()
    })
    .await
    .expect("standalone remote tunnel allocates port and reaches Active");

    assert_eq!(
        ssh_srv.state.shell_opens.load(Ordering::SeqCst),
        shell_opens_before,
        "standalone remote tunnel must not allocate a PTY shell"
    );
    let allocated_port = standalone_snap
        .tunnels
        .iter()
        .find(|t| t.id == standalone_rule.id)
        .and_then(|t| t.remote_bound_port)
        .expect("allocated remote port");

    {
        let mut remote_client = TcpStream::connect(("127.0.0.1", allocated_port))
            .await
            .unwrap();
        remote_client
            .write_all(b"standalone-remote-ok")
            .await
            .unwrap();
        let mut reply = [0u8; 20];
        remote_client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"standalone-remote-ok");
    }

    assert!(manager.stop_tunnel_async(&standalone_rule.id).await);
    let final_snap = manager.snapshot();
    assert_eq!(final_snap.active_standalone_count, 0);
    assert_eq!(
        final_snap
            .tunnels
            .iter()
            .find(|t| t.id == standalone_rule.id)
            .map(|t| &t.state),
        Some(&TunnelState::Stopped)
    );

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let evs = ssh_srv.state.events();
            if evs
                .iter()
                .any(|e| matches!(e, RemoteServerEvent::TransportClosed { conn_id: 2 }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("server records standalone transport close");

    let events_after_b = ssh_srv.state.events();
    let cancel2_idx = events_after_b
        .iter()
        .position(|e| {
            matches!(
                e,
                RemoteServerEvent::CancelTcpIpForward { address, port }
                    if address == "0.0.0.0" && *port == u32::from(allocated_port)
            )
        })
        .expect("standalone cancel_tcpip_forward must pass exact address and allocated port");
    let close2_idx = events_after_b
        .iter()
        .position(|e| matches!(e, RemoteServerEvent::TransportClosed { conn_id: 2 }))
        .expect("standalone transport close recorded");
    assert!(
        cancel2_idx < close2_idx,
        "standalone cancel_tcpip_forward ({cancel2_idx}) must complete BEFORE transport close ({close2_idx}): {events_after_b:?}"
    );
}

struct FaultProxy {
    addr: SocketAddr,
    drop_tx: watch::Sender<u64>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl FaultProxy {
    fn drop_active_connections(&self) {
        let next = self.drop_tx.borrow().wrapping_add(1);
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
    let (drop_tx, drop_rx) = watch::channel(0u64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((client, _)) = accepted else { break; };
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
        drop_tx,
        shutdown_tx: Some(shutdown_tx),
    }
}

#[tokio::test]
async fn remote_tunnel_recovers_from_ungraceful_drop_and_request_denied_with_exponential_backoff() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let local_echo = spawn_echo_server().await;
    let ssh_srv = spawn_remote_ssh_server(true).await;
    let proxy = spawn_fault_proxy(ssh_srv.addr).await;

    record_known_host_path(
        &proxy.addr.ip().to_string(),
        proxy.addr.port(),
        &ssh_srv.host_pubkey,
        &known_hosts,
    )
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
    sup_cfg.backoff_min = Duration::from_millis(80);
    sup_cfg.backoff_max = Duration::from_millis(160);

    let sup = ConnectionSupervisor::connect_with_connector(
        build_connect_config(proxy.addr, &known_hosts),
        sup_cfg,
        Arc::new(TerminalBuffer::new(80, 24, 1000)),
        connector,
        Arc::new(SystemTimeSource),
    );

    let mut ready_rx = sup.subscribe_ready();
    tokio::time::timeout(Duration::from_secs(5), async {
        ready_rx.wait_for(|s| s.is_connected()).await.unwrap();
    })
    .await
    .expect("initial connect succeeds");

    let manager = TunnelManager::with_remote_backoff(RemoteForwardBackoffConfig {
        min: Duration::from_millis(80),
        max: Duration::from_millis(320),
    });
    let mut sub = manager.subscribe();

    let remote_port = reserve_free_port().await;
    let rule = TunnelRule::remote("Remote Stale Recovery", remote_port, "127.0.0.1", local_echo.addr.port());
    manager
        .start_session_tunnel(7, None, rule.clone(), sup.subscribe_ready())
        .await
        .expect("start remote tunnel");

    tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| {
            s.tunnels
                .iter()
                .any(|t| t.id == rule.id && t.state == TunnelState::Active)
        })
        .await
        .unwrap();
    })
    .await
    .expect("initial remote tunnel Active");

    // Verify traffic works before ungraceful drop.
    {
        let mut c = TcpStream::connect(("127.0.0.1", remote_port)).await.unwrap();
        c.write_all(b"pre-drop").await.unwrap();
        let mut buf = [0u8; 8];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pre-drop");
    }

    // Simulate ungraceful TCP disconnect (NO cancel_tcpip_forward!).
    // Server handler Drop holds `remote_port` in `stale_ports`, forcing `RequestDenied` on reconnect.
    proxy.drop_active_connections();

    tokio::time::timeout(Duration::from_secs(5), async {
        ready_rx
            .wait_for(|s| s.is_connected() && s.epoch >= 2)
            .await
            .unwrap();
    })
    .await
    .expect("supervisor reconnects on epoch >= 2");

    // Tunnel MUST transition into Retrying (not permanent Error) when server rejects with RequestDenied.
    let retry_started = Instant::now();
    let first_retry = tokio::time::timeout(Duration::from_secs(3), async {
        sub.wait_for(|s| {
            s.retrying_count == 1
                && s.error_count == 0
                && s.tunnels.iter().any(|t| {
                    t.id == rule.id
                        && matches!(
                            &t.state,
                            TunnelState::Retrying { attempt: 1, reason, .. }
                                if reason.contains("RequestDenied")
                        )
                })
        })
        .await
        .unwrap()
        .clone()
    })
    .await
    .expect("tunnel enters Retrying #1 on RequestDenied");
    assert_eq!(first_retry.error_count, 0);

    // Let exponential backoff advance to attempt >= 3 (~80ms + ~160ms with +-20% jitter => >= ~180ms).
    tokio::time::timeout(Duration::from_secs(3), async {
        sub.wait_for(|s| {
            s.tunnels.iter().any(|t| {
                t.id == rule.id
                    && matches!(t.state, TunnelState::Retrying { attempt, .. } if attempt >= 3)
            })
        })
        .await
        .unwrap();
    })
    .await
    .expect("tunnel advances through exponential backoff retries");

    let retry_elapsed = retry_started.elapsed();
    assert!(
        retry_elapsed >= Duration::from_millis(160),
        "expected exponential backoff across attempts 1..=3, elapsed only {retry_elapsed:?}"
    );

    let denied_count = ssh_srv
        .state
        .events()
        .iter()
        .filter(|e| {
            matches!(
                e,
                RemoteServerEvent::TcpIpForwardRequested {
                    requested_port,
                    accepted: false,
                    ..
                } if *requested_port == u32::from(remote_port)
            )
        })
        .count();
    assert!(
        denied_count >= 3,
        "expected at least 3 denied tcpip_forward attempts, got {denied_count}"
    );

    // Release stale port on the mock server -> next retry recovers automatically to Active!
    ssh_srv.state.release_stale_port(remote_port);

    tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| {
            s.retrying_count == 0
                && s.active_count == 1
                && s.tunnels.iter().any(|t| {
                    t.id == rule.id
                        && t.state == TunnelState::Active
                        && t.remote_bound_port == Some(remote_port)
                })
        })
        .await
        .unwrap();
    })
    .await
    .expect("remote tunnel recovers to Active once stale port is released");

    // Verify traffic flows end-to-end after recovery.
    {
        let mut c = TcpStream::connect(("127.0.0.1", remote_port)).await.unwrap();
        c.write_all(b"post-recovery-ok").await.unwrap();
        let mut buf = [0u8; 16];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"post-recovery-ok");
    }

    // Clean shutdown still sends cancel_tcpip_forward before transport close.
    manager.stop_all_async().await;
    sup.disconnect();

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let evs = ssh_srv.state.events();
            if evs
                .iter()
                .any(|e| matches!(e, RemoteServerEvent::TransportClosed { conn_id: 2 }))
            {
                break evs;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("server records reconnect transport close");

    let evs = ssh_srv.state.events();
    let cancel_idx = evs
        .iter()
        .rposition(|e| {
            matches!(
                e,
                RemoteServerEvent::CancelTcpIpForward { port, .. }
                    if *port == u32::from(remote_port)
            )
        })
        .expect("cancel_tcpip_forward after recovery");
    let close_idx = evs
        .iter()
        .rposition(|e| matches!(e, RemoteServerEvent::TransportClosed { conn_id: 2 }))
        .expect("transport close recorded");
    assert!(cancel_idx < close_idx);
}

#[tokio::test]
async fn standalone_remote_tunnel_request_denied_supports_retry_now_and_clean_stop_while_retrying() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let local_echo = spawn_echo_server().await;
    let ssh_srv = spawn_remote_ssh_server(false).await;

    record_known_host_path(
        &ssh_srv.addr.ip().to_string(),
        ssh_srv.addr.port(),
        &ssh_srv.host_pubkey,
        &known_hosts,
    )
    .unwrap();

    let remote_port = reserve_free_port().await;
    // Pre-hold port as stale so initial Standalone start enters Retrying.
    ssh_srv.state.stale_ports.lock().unwrap().insert(remote_port);

    let manager = TunnelManager::with_remote_backoff(RemoteForwardBackoffConfig {
        min: Duration::from_secs(10),
        max: Duration::from_secs(20),
    });
    let mut sub = manager.subscribe();

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    let spec = StandaloneTransportSpec::new(
        build_connect_config(ssh_srv.addr, &known_hosts),
        sup_cfg,
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );

    let mut rule = TunnelRule::remote("Standalone Retrying", remote_port, "127.0.0.1", local_echo.addr.port());
    rule.standalone = true;
    manager
        .start_standalone_tunnel(Some("prof-1".to_string()), rule.clone(), spec)
        .await
        .expect("start standalone remote tunnel");

    tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| {
            s.retrying_count == 1
                && s.active_standalone_count == 1
                && s.tunnels.iter().any(|t| {
                    t.id == rule.id && matches!(t.state, TunnelState::Retrying { attempt: 1, .. })
                })
        })
        .await
        .unwrap();
    })
    .await
    .expect("standalone remote tunnel enters Retrying #1");

    // Manual reconnect_tunnel during Retrying triggers immediate retry without waiting 10s backoff!
    ssh_srv.state.release_stale_port(remote_port);
    manager.reconnect_tunnel(&rule.id).unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        sub.wait_for(|s| {
            s.retrying_count == 0
                && s.active_count == 1
                && s.tunnels
                    .iter()
                    .any(|t| t.id == rule.id && t.state == TunnelState::Active)
        })
        .await
        .unwrap();
    })
    .await
    .expect("immediate retry recovers without dropping transport");
    // Only 1 connection was ever opened (no transport reconnect needed).
    assert_eq!(
        ssh_srv
            .state
            .events()
            .iter()
            .filter(|e| matches!(e, RemoteServerEvent::TransportClosed { .. }))
            .count(),
        0
    );

    // Also verify stopping while Retrying stops cleanly without sending cancel_tcpip_forward for unbound port.
    manager.stop_tunnel_async(&rule.id).await;
    ssh_srv.state.stale_ports.lock().unwrap().insert(remote_port);

    let mut rule2 = TunnelRule::remote("Stop While Retrying", remote_port, "127.0.0.1", local_echo.addr.port());
    rule2.standalone = true;
    let mut sup_cfg2 = SupervisorConfig::default();
    sup_cfg2.keepalive_interval = Duration::from_secs(30);
    let spec2 = StandaloneTransportSpec::new(
        build_connect_config(ssh_srv.addr, &known_hosts),
        sup_cfg2,
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );
    manager
        .start_standalone_tunnel(Some("prof-1".to_string()), rule2.clone(), spec2)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| {
            s.tunnels
                .iter()
                .any(|t| t.id == rule2.id && matches!(t.state, TunnelState::Retrying { .. }))
        })
        .await
        .unwrap();
    })
    .await
    .unwrap();

    let cancels_before = ssh_srv
        .state
        .events()
        .iter()
        .filter(|e| matches!(e, RemoteServerEvent::CancelTcpIpForward { .. }))
        .count();
    assert!(manager.stop_tunnel_async(&rule2.id).await);
    let snap = manager.snapshot();
    assert_eq!(snap.active_standalone_count, 0);
    assert_eq!(
        snap.tunnels.iter().find(|t| t.id == rule2.id).map(|t| &t.state),
        Some(&TunnelState::Stopped)
    );
    let cancels_after = ssh_srv
        .state
        .events()
        .iter()
        .filter(|e| matches!(e, RemoteServerEvent::CancelTcpIpForward { .. }))
        .count();
    assert_eq!(cancels_before, cancels_after);
}

#[tokio::test]
async fn remote_tunnel_rejects_forwarded_tcpip_when_local_target_refuses_without_dropping_tunnel() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let ssh_srv = spawn_remote_ssh_server(false).await;
    record_known_host_path(
        &ssh_srv.addr.ip().to_string(),
        ssh_srv.addr.port(),
        &ssh_srv.host_pubkey,
        &known_hosts,
    )
    .unwrap();

    // Reserve a local port with NO listener bound, then later bind an echo server on that exact port.
    let target_port = reserve_free_port().await;
    let remote_port = reserve_free_port().await;

    let manager = TunnelManager::new();
    let mut sub = manager.subscribe();
    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(30);
    let spec = StandaloneTransportSpec::new(
        build_connect_config(ssh_srv.addr, &known_hosts),
        sup_cfg,
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );

    let mut rule = TunnelRule::remote("Target Refused Check", remote_port, "127.0.0.1", target_port);
    rule.standalone = true;
    manager
        .start_standalone_tunnel(None, rule.clone(), spec)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        sub.wait_for(|s| s.active_count == 1).await.unwrap();
    })
    .await
    .unwrap();

    // While local target_port is closed, incoming connection to remote_port closes cleanly without breaking the tunnel.
    {
        let mut client = TcpStream::connect(("127.0.0.1", remote_port)).await.unwrap();
        let mut buf = [0u8; 16];
        // Windows loopback TCP SYN retries take ~2s before WSAECONNREFUSED fires.
        let res = tokio::time::timeout(Duration::from_secs(6), client.read(&mut buf))
            .await
            .expect("client must close cleanly on ConnectFailed");
        assert!(matches!(res, Ok(0) | Err(_)));
    }
    assert_eq!(manager.snapshot().tunnels[0].state, TunnelState::Active);

    // Once local target comes up on target_port, subsequent remote connections forward bytes normally.
    let listener = TcpListener::bind(("127.0.0.1", target_port)).await.unwrap();
    tokio::spawn(async move {
        if let Ok((mut s, _)) = listener.accept().await {
            let mut b = [0u8; 64];
            if let Ok(n) = s.read(&mut b).await {
                let _ = s.write_all(&b[..n]).await;
            }
        }
    });

    let mut ok_client = TcpStream::connect(("127.0.0.1", remote_port)).await.unwrap();
    ok_client.write_all(b"after-target-up").await.unwrap();
    let mut reply = [0u8; 15];
    ok_client.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"after-target-up");

    manager.stop_all_async().await;
}
