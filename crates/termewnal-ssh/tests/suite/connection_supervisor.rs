use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex_helper::has_reconnected_divider;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet, server};
use tempfile::TempDir;
use termewnal_core::{AppConfig, Profile, SshAuthMethod};
use termewnal_pty::TerminalBuffer;
use termewnal_ssh::{
    BoxFuture, ConnectionSupervisor, DEFAULT_BACKOFF_MAX, DEFAULT_BACKOFF_MIN,
    DefaultTransportConnector, HostKeyDecision, HostKeyPrompt, KeyboardInteractiveChallenge,
    ManualTimeSource, OsAgentConnector, SshConnectConfig, SshInteractionHandler, SupervisorConfig,
    SupervisorEvent, TransportStatus, compute_reconnect_backoff, record_known_host_path,
};

mod regex_helper {
    pub fn has_reconnected_divider(text: &str) -> bool {
        text.lines().any(|line| {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("--- Reconnected at ")
                && let Some(ts) = rest.strip_suffix(" ---")
            {
                let parts: Vec<&str> = ts.split(':').collect();
                parts.len() == 3
                    && parts
                        .iter()
                        .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()))
            } else {
                false
            }
        })
    }
}
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

struct FaultServerState {
    connections: AtomicUsize,
    shell_commands: Mutex<Vec<String>>,
}

struct FaultSshHandler {
    state: Arc<FaultServerState>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    line_buf: String,
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
        let conn_num = self.state.connections.fetch_add(1, Ordering::SeqCst) + 1;
        session.channel_success(channel)?;
        session.data(
            channel,
            format!("SHELL_READY_{conn_num}\r\n").into_bytes(),
        )?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let mut echo = Vec::new();
        let mut saw_exit = false;
        for &b in data {
            if b == b'\r' || b == b'\n' {
                if !self.line_buf.is_empty() {
                    let cmd = std::mem::take(&mut self.line_buf);
                    if cmd == "exit" {
                        saw_exit = true;
                    }
                    self.state.shell_commands.lock().unwrap().push(cmd);
                }
                echo.extend_from_slice(b"\r\n");
            } else {
                self.line_buf.push(b as char);
                echo.push(b);
            }
        }
        session.data(channel, echo)?;
        if saw_exit {
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }
}

struct TestSshServer {
    addr: SocketAddr,
    host_pubkey: PublicKey,
    state: Arc<FaultServerState>,
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

    let state = Arc::new(FaultServerState {
        connections: AtomicUsize::new(0),
        shell_commands: Mutex::new(Vec::new()),
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
                        let handler = FaultSshHandler {
                            state: st,
                            channels: HashMap::new(),
                            line_buf: String::new(),
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

struct FaultProxy {
    addr: SocketAddr,
    blackhole_active: Arc<AtomicBool>,
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
    let blackhole_active = Arc::new(AtomicBool::new(false));
    let accept_new = Arc::new(AtomicBool::new(true));
    let (drop_tx, drop_rx) = watch::channel(0_u64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    let bh = Arc::clone(&blackhole_active);
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
                    let bh = Arc::clone(&bh);
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
                                            if !bh.load(Ordering::SeqCst) {
                                                if u_tx.write_all(&c_buf[..n]).await.is_err() {
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                n = u_rx.read(&mut u_buf) => {
                                    match n {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if !bh.load(Ordering::SeqCst) {
                                                if c_tx.write_all(&u_buf[..n]).await.is_err() {
                                                    break;
                                                }
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
        blackhole_active,
        accept_new,
        drop_tx,
        shutdown_tx: Some(shutdown_tx),
    }
}

struct FixedPasswordInteraction {
    password: String,
}

impl SshInteractionHandler for FixedPasswordInteraction {
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

fn build_connect_config(port: u16, known_hosts: &Path) -> SshConnectConfig {
    SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port,
        username: "ops".to_string(),
        auth: SshAuthMethod::Password {
            vault_secret_ref: None,
        },
        known_hosts_path: Some(known_hosts.to_path_buf()),
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
async fn keepalive_timeout_transitions_connected_to_degraded_to_reconnecting_and_recovers() {
    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    let srv = spawn_ssh_server().await;
    let proxy = spawn_fault_proxy(srv.addr).await;
    record_known_host_path("127.0.0.1", proxy.addr.port(), &srv.host_pubkey, &kh).unwrap();

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_millis(60);
    sup_cfg.keepalive_max_missed = 3;
    sup_cfg.probe_timeout = Duration::from_millis(50);
    sup_cfg.backoff_min = Duration::from_millis(50);
    sup_cfg.backoff_max = Duration::from_millis(200);

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
    let supervisor = ConnectionSupervisor::connect_on_buffer(
        build_connect_config(proxy.addr.port(), &kh),
        sup_cfg,
        Arc::clone(&buffer),
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );

    let mut status_rx = supervisor.subscribe_status();
    let ready_rx = supervisor.subscribe_transport();

    // Wait until Connected with live RTT latency measurement.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                *status_rx.borrow(),
                TransportStatus::Connected {
                    latency_ms: Some(_)
                }
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("timed out waiting for Connected with RTT latency");
    assert!(ready_rx.borrow().is_some());

    // Blackhole all traffic without closing TCP -> missed probes increment Degraded(1) -> Degraded(2) -> Reconnecting.
    proxy.blackhole_active.store(true, Ordering::SeqCst);

    let mut saw_degraded_1 = false;
    let mut saw_degraded_2 = false;
    let mut saw_reconnecting = false;

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match status_rx.borrow().clone() {
                TransportStatus::Degraded { missed_probes: 1 } => saw_degraded_1 = true,
                TransportStatus::Degraded { missed_probes: 2 } => saw_degraded_2 = true,
                TransportStatus::Reconnecting { attempt, .. } if attempt >= 1 => {
                    saw_reconnecting = true;
                    break;
                }
                _ => {}
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("timed out waiting for Degraded -> Reconnecting on missed keepalives");

    assert!(saw_degraded_1);
    assert!(saw_degraded_2);
    assert!(saw_reconnecting);

    // Clear blackhole -> supervisor reconnects cleanly on a new transport epoch.
    proxy.blackhole_active.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Connected { .. })
                && ready_rx.borrow().is_some()
            {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("timed out waiting for recovery after blackhole cleared");

    assert!(srv.state.connections.load(Ordering::SeqCst) >= 2);
    supervisor.disconnect();
}

#[tokio::test]
async fn clock_jump_and_manual_probe_now_trigger_immediate_fast_probe_recovery() {
    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    let srv = spawn_ssh_server().await;
    let proxy = spawn_fault_proxy(srv.addr).await;
    record_known_host_path("127.0.0.1", proxy.addr.port(), &srv.host_pubkey, &kh).unwrap();

    // Keep full 15s keepalive interval so normal timeout would take ~45s!
    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(15);
    sup_cfg.keepalive_max_missed = 3;
    sup_cfg.wake_probe_timeout = Duration::from_millis(80);
    sup_cfg.backoff_min = Duration::from_millis(40);
    sup_cfg.backoff_max = Duration::from_millis(120);

    let clock = Arc::new(ManualTimeSource::new());
    let connector = Arc::new(DefaultTransportConnector::new(
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    ));
    let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
    let supervisor = ConnectionSupervisor::connect_with_connector(
        build_connect_config(proxy.addr.port(), &kh),
        sup_cfg,
        Arc::clone(&buffer),
        connector,
        clock.clone(),
    );

    let mut status_rx = supervisor.subscribe_status();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                *status_rx.borrow(),
                TransportStatus::Connected {
                    latency_ms: Some(_)
                }
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("initial connect must complete");

    // Simulate OS suspend/sleep jump (> 15s) while network died silently.
    proxy.blackhole_active.store(true, Ordering::SeqCst);
    clock.advance_sleep(Duration::from_secs(20));

    // Must enter Reconnecting immediately via wake fast-probe (far faster than 45s).
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Reconnecting { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("clock jump must trigger immediate fast-probe -> Reconnecting");

    proxy.blackhole_active.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Connected { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("must recover after wake reconnect");

    // Manual probe_now() on healthy transport keeps it Connected; on blackholed transport triggers immediate Reconnecting.
    status_rx.borrow_and_update();
    supervisor.probe_now();
    tokio::time::timeout(Duration::from_secs(1), status_rx.changed())
        .await
        .expect("probe_now must refresh status")
        .expect("status channel must remain open");
    assert!(matches!(
        supervisor.status(),
        TransportStatus::Connected { .. }
    ));

    proxy.blackhole_active.store(true, Ordering::SeqCst);
    supervisor.probe_now();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Reconnecting { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("manual probe_now() on dead transport must enter Reconnecting immediately");

    proxy.blackhole_active.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Connected { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("must recover after manual probe reconnect");

    supervisor.disconnect();
}

#[tokio::test]
async fn steady_elapsed_interval_without_clock_divergence_does_not_trigger_false_clock_jump_probe() {
    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    let srv = spawn_ssh_server().await;
    let proxy = spawn_fault_proxy(srv.addr).await;
    record_known_host_path("127.0.0.1", proxy.addr.port(), &srv.host_pubkey, &kh).unwrap();

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(15);
    sup_cfg.keepalive_max_missed = 3;
    sup_cfg.wake_probe_timeout = Duration::from_millis(80);
    sup_cfg.backoff_min = Duration::from_millis(40);
    sup_cfg.backoff_max = Duration::from_millis(120);

    let clock = Arc::new(ManualTimeSource::new());
    let connector = Arc::new(DefaultTransportConnector::new(
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    ));
    let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
    let supervisor = ConnectionSupervisor::connect_with_connector(
        build_connect_config(proxy.addr.port(), &kh),
        sup_cfg,
        Arc::clone(&buffer),
        connector,
        clock.clone(),
    );

    let mut status_rx = supervisor.subscribe_status();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                *status_rx.borrow(),
                TransportStatus::Connected {
                    latency_ms: Some(_)
                }
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("initial connect must complete");

    // Advance both wall and monotonic clocks equally (> 15s) while blackholed:
    // no suspend/clock jump happened, so wake fast-probe MUST NOT fire.
    proxy.blackhole_active.store(true, Ordering::SeqCst);
    clock.advance_steady(Duration::from_secs(20));

    let false_jump = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Reconnecting { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await;
    assert!(
        false_jump.is_err(),
        "steady elapsed interval without wall/monotonic divergence must NOT trigger fast-probe -> Reconnecting (status: {:?})",
        supervisor.status()
    );

    // Genuine sleep/clock-jump divergence (wall +20s, monotonic unchanged) MUST trigger fast-probe.
    clock.advance_sleep(Duration::from_secs(20));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Reconnecting { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("genuine sleep divergence must trigger immediate fast-probe -> Reconnecting");

    supervisor.disconnect();
}

#[test]
fn backoff_schedule_stays_within_1s_to_30s_with_jitter() {
    let min = DEFAULT_BACKOFF_MIN;
    let max = DEFAULT_BACKOFF_MAX;
    assert_eq!(min, Duration::from_secs(1));
    assert_eq!(max, Duration::from_secs(30));

    for attempt in 1..=10 {
        for jitter in [0.0, 0.25, 0.5, 0.99] {
            let d = compute_reconnect_backoff(attempt, min, max, jitter);
            assert!(d >= min, "attempt {attempt} jitter {jitter} gave {d:?} < 1s");
            assert!(d <= max, "attempt {attempt} jitter {jitter} gave {d:?} > 30s");
        }
    }

    let d1 = compute_reconnect_backoff(1, min, max, 0.5);
    let d3 = compute_reconnect_backoff(3, min, max, 0.5);
    let d6 = compute_reconnect_backoff(6, min, max, 0.5);
    assert_eq!(d1, Duration::from_secs(1));
    assert_eq!(d3, Duration::from_secs(4));
    assert_eq!(d6, Duration::from_secs(30));
}

#[tokio::test]
async fn tcp_drop_preserves_scrollback_supports_cancel_and_reconnect_now_with_divider() {
    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    let srv = spawn_ssh_server().await;
    let proxy = spawn_fault_proxy(srv.addr).await;
    record_known_host_path("127.0.0.1", proxy.addr.port(), &srv.host_pubkey, &kh).unwrap();

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.keepalive_interval = Duration::from_secs(15);
    sup_cfg.backoff_min = Duration::from_millis(60);
    sup_cfg.backoff_max = Duration::from_millis(400);

    let mut cfg = build_connect_config(proxy.addr.port(), &kh);
    cfg.rows = 10;

    let buffer = Arc::new(TerminalBuffer::new(80, 10, 10_000));
    let supervisor = ConnectionSupervisor::connect_on_buffer(
        cfg,
        sup_cfg,
        Arc::clone(&buffer),
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );
    let mut status_rx = supervisor.subscribe_status();
    let mut events_rx = supervisor.subscribe_events();

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("SHELL_READY_1")
            })
            .await
    );

    // Push enough lines to populate scrollback history above the 10-row viewport.
    for idx in 1..=20 {
        supervisor
            .write_bytes(format!("SCROLLBACK_LINE_{idx:02}\r").as_bytes())
            .unwrap();
    }
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("SCROLLBACK_LINE_20")
            })
            .await
    );
    assert!(buffer.history_size() > 0);
    assert!(!buffer.search_scrollback("SCROLLBACK_LINE_01").is_empty());

    // Refuse new connections temporarily and drop active TCP socket.
    proxy.accept_new.store(false, Ordering::SeqCst);
    proxy.drop_active_connections();

    // Verify supervisor enters Reconnecting and keeps advancing attempts without exiting buffer.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                *status_rx.borrow(),
                TransportStatus::Reconnecting { attempt, .. } if attempt >= 2
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("must enter Reconnecting and advance backoff attempts");

    assert!(!buffer.is_exited());
    assert!(!buffer.search_scrollback("SCROLLBACK_LINE_01").is_empty());

    // Cancelling transitions cleanly to Disconnected while preserving full scrollback.
    supervisor.cancel_reconnect();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if *status_rx.borrow() == TransportStatus::Disconnected {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("cancel_reconnect must reach Disconnected");
    assert!(!buffer.search_scrollback("SCROLLBACK_LINE_01").is_empty());

    // Restore server acceptance and trigger reconnect_now() -> inserts Reconnected divider + preserves scrollback.
    proxy.accept_new.store(true, Ordering::SeqCst);
    supervisor.reconnect_now();

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("SHELL_READY_2")
            })
            .await
    );
    assert!(has_reconnected_divider(&buffer.snapshot().visible_text()));
    assert!(!buffer.search_scrollback("SCROLLBACK_LINE_01").is_empty());
    assert!(!buffer.search_scrollback("SHELL_READY_1").is_empty());

    // Verify Reconnected lifecycle event also carried the HH:MM:SS divider.
    let mut saw_reconnected_event = false;
    while let Ok(ev) = events_rx.try_recv() {
        if let SupervisorEvent::Reconnected {
            divider,
            mux_command_sent,
            ..
        } = ev
        {
            assert!(has_reconnected_divider(&divider));
            assert_eq!(mux_command_sent, None);
            saw_reconnected_event = true;
        }
    }
    assert!(saw_reconnected_event);

    // Also verify reconnect_now() interrupts a multi-second backoff sleep immediately.
    proxy.accept_new.store(false, Ordering::SeqCst);
    proxy.drop_active_connections();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(*status_rx.borrow(), TransportStatus::Reconnecting { .. }) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    proxy.accept_new.store(true, Ordering::SeqCst);
    supervisor.reconnect_now();
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(2), |s| {
                s.visible_text().contains("SHELL_READY_3")
            })
            .await
    );

    supervisor.disconnect();
}

#[tokio::test]
async fn reconnect_invokes_profile_tmux_and_screen_attach_commands_when_enabled() {
    for mux_cmd in [
        "tmux attach -t work || tmux new -s work",
        "screen -xR dev",
    ] {
        let temp = TempDir::new().unwrap();
        let kh = temp.path().join("known_hosts");
        let srv = spawn_ssh_server().await;
        let proxy = spawn_fault_proxy(srv.addr).await;
        record_known_host_path("127.0.0.1", proxy.addr.port(), &srv.host_pubkey, &kh).unwrap();

        let app_cfg = AppConfig::default();
        let mut profile = Profile::ssh(
            "Mux Host",
            "127.0.0.1",
            proxy.addr.port(),
            "ops",
            SshAuthMethod::Password {
                vault_secret_ref: None,
            },
        );
        profile.set_mux_attach(true, Some(mux_cmd.to_string()));

        let mut sup_cfg = SupervisorConfig::from_configs(&app_cfg, &profile);
        sup_cfg.backoff_min = Duration::from_millis(50);
        sup_cfg.backoff_max = Duration::from_millis(150);

        let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
        let supervisor = ConnectionSupervisor::connect_on_buffer(
            build_connect_config(proxy.addr.port(), &kh),
            sup_cfg,
            Arc::clone(&buffer),
            None,
            Arc::new(FixedPasswordInteraction {
                password: "secret".to_string(),
            }),
            Arc::new(OsAgentConnector::default()),
        );
        let mut events_rx = supervisor.subscribe_events();

        assert!(
            buffer
                .wait_for_output_async(Duration::from_secs(3), |s| {
                    s.visible_text().contains("SHELL_READY_1")
                })
                .await
        );
        // Initial connect does not inject mux attach before drop.
        assert!(srv.state.shell_commands.lock().unwrap().is_empty());

        // Drop TCP -> supervisor reconnects, prints divider, and invokes mux attach command.
        proxy.drop_active_connections();
        assert!(
            buffer
                .wait_for_output_async(Duration::from_secs(3), |s| {
                    let txt = s.visible_text();
                    txt.contains("SHELL_READY_2") && txt.contains(mux_cmd)
                })
                .await,
            "expected mux command `{mux_cmd}` after reconnect, got:\n{}",
            buffer.snapshot().visible_text()
        );
        assert!(has_reconnected_divider(&buffer.snapshot().visible_text()));
        assert_eq!(
            srv.state.shell_commands.lock().unwrap().as_slice(),
            &[mux_cmd.to_string()]
        );

        let mut saw_mux_event = false;
        while let Ok(ev) = events_rx.try_recv() {
            if let SupervisorEvent::Reconnected {
                mux_command_sent, ..
            } = ev
            {
                assert_eq!(mux_command_sent.as_deref(), Some(mux_cmd));
                saw_mux_event = true;
            }
        }
        assert!(saw_mux_event);

        supervisor.disconnect();
    }
}

#[tokio::test]
async fn clean_remote_shell_exit_marks_buffer_exited_and_stops_without_reconnecting() {
    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    let srv = spawn_ssh_server().await;
    record_known_host_path("127.0.0.1", srv.addr.port(), &srv.host_pubkey, &kh).unwrap();

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.backoff_min = Duration::from_millis(50);

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 10_000));
    let supervisor = ConnectionSupervisor::connect_on_buffer(
        build_connect_config(srv.addr.port(), &kh),
        sup_cfg,
        Arc::clone(&buffer),
        None,
        Arc::new(FixedPasswordInteraction {
            password: "secret".to_string(),
        }),
        Arc::new(OsAgentConnector::default()),
    );
    let mut status_rx = supervisor.subscribe_status();

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("SHELL_READY_1")
            })
            .await
    );

    supervisor.write_bytes(b"exit\r").unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if *status_rx.borrow() == TransportStatus::Disconnected && buffer.is_exited() {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("clean exit must mark buffer exited and transition to Disconnected");

    tokio::time::timeout(Duration::from_secs(2), async {
        while status_rx.changed().await.is_ok() {}
    })
    .await
    .expect("supervisor task must terminate cleanly without reconnecting");
    assert_eq!(srv.state.connections.load(Ordering::SeqCst), 1);
    assert_eq!(supervisor.status(), TransportStatus::Disconnected);
}


#[tokio::test]
async fn initial_retryable_connection_error_auto_reconnects_and_emits_reconnected_status() {
    use russh::keys::{Algorithm, PrivateKey};
    use russh::{MethodKind, MethodSet, server};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use termewnal_core::SshAuthMethod;
    use termewnal_pty::TerminalBuffer;
    use termewnal_ssh::{
        BoxFuture, ConnectionSupervisor, HostKeyDecision, HostKeyPrompt,
        KeyboardInteractiveChallenge, OsAgentConnector, SshConnectConfig, SshInteractionHandler,
        SupervisorConfig, SupervisorEvent, TransportStatus, record_known_host_path,
    };
    use tokio::io::copy_bidirectional;
    use tokio::net::TcpListener;
    use zeroize::Zeroizing;

    struct LocalSshHandler;
    impl server::Handler for LocalSshHandler {
        type Error = russh::Error;

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
            _channel: russh::Channel<server::Msg>,
            reply: server::ChannelOpenHandle,
            _session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        async fn pty_request(
            &mut self,
            channel: russh::ChannelId,
            _term: &str,
            _c: u32,
            _r: u32,
            _pw: u32,
            _ph: u32,
            _m: &[(russh::Pty, u32)],
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            session.data(channel, b"OK\r\n".to_vec())?;
            Ok(())
        }
    }

    struct LocalPassAuth;
    impl SshInteractionHandler for LocalPassAuth {
        fn verify_unknown_host(&self, _prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async { HostKeyDecision::Reject })
        }
        fn prompt_key_passphrase(
            &self,
            _key_path: &std::path::Path,
        ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
            Box::pin(async { None })
        }
        fn prompt_password(
            &self,
            _u: &str,
            _h: &str,
        ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
            Box::pin(async { Some(Zeroizing::new("secret".to_string())) })
        }
        fn prompt_keyboard_interactive(
            &self,
            _c: KeyboardInteractiveChallenge,
        ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
            Box::pin(async { None })
        }
    }

    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let pubkey = key.public_key().clone();
    let mut cfg = server::Config::default();
    cfg.auth_rejection_time = Duration::from_millis(1);
    cfg.methods = MethodSet::from(&[MethodKind::Password][..]);
    cfg.keys.push(key);
    let cfg = Arc::new(cfg);

    let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ssh_addr = ssh_listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = ssh_listener.accept().await {
            let cfg = Arc::clone(&cfg);
            tokio::spawn(async move {
                let _ = server::run_stream(cfg, stream, LocalSshHandler).await;
            });
        }
    });

    // Gate proxy rejects the initial connection so supervisor starts auto-reconnecting on attempt #1.
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let allow_connect = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicUsize::new(0));
    let allow_clone = Arc::clone(&allow_connect);
    let attempts_clone = Arc::clone(&attempts);
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = proxy_listener.accept().await {
            attempts_clone.fetch_add(1, Ordering::SeqCst);
            if !allow_clone.load(Ordering::SeqCst) {
                drop(inbound);
                continue;
            }
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::TcpStream::connect(ssh_addr).await {
                    let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });

    let temp = TempDir::new().unwrap();
    let kh = temp.path().join("known_hosts");
    record_known_host_path("127.0.0.1", proxy_addr.port(), &pubkey, &kh).unwrap();

    let mut sup_cfg = SupervisorConfig::default();
    sup_cfg.backoff_min = Duration::from_millis(50);
    sup_cfg.backoff_max = Duration::from_millis(100);
    sup_cfg.keepalive_interval = Duration::from_secs(30);

    let connect_cfg = SshConnectConfig {
        host: "127.0.0.1".to_string(),
        port: proxy_addr.port(),
        username: "ops".to_string(),
        auth: SshAuthMethod::Password {
            vault_secret_ref: None,
        },
        known_hosts_path: Some(kh),
        nodelay: true,
        term_name: "xterm-256color".to_string(),
        cols: 80,
        rows: 24,
        scrollback_lines: 1000,
        ..SshConnectConfig::default()
    };

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 1000));
    let (sup, mut events_rx) = ConnectionSupervisor::connect_on_buffer_with_events(
        connect_cfg,
        sup_cfg,
        buffer,
        None,
        Arc::new(LocalPassAuth),
        Arc::new(OsAgentConnector::default()),
    );

    let mut saw_reconnecting_with_err = false;
    let mut kept_error_on_connecting_retry = false;
    let mut saw_connected = false;

    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = events_rx.recv().await {
            if let SupervisorEvent::StatusChanged(status_ev) = ev {
                match status_ev.status {
                    TransportStatus::Reconnecting { attempt, .. } if attempt >= 1 => {
                        assert!(
                            status_ev.last_error.is_some(),
                            "Reconnecting event must carry initial connection error"
                        );
                        saw_reconnecting_with_err = true;
                        allow_connect.store(true, Ordering::SeqCst);
                    }
                    TransportStatus::Connecting if saw_reconnecting_with_err => {
                        if status_ev.last_error.is_some() {
                            kept_error_on_connecting_retry = true;
                        }
                    }
                    TransportStatus::Connected { .. } if saw_reconnecting_with_err => {
                        saw_connected = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("supervisor must recover from initial retryable failure");

    assert!(saw_reconnecting_with_err);
    assert!(kept_error_on_connecting_retry);
    assert!(saw_connected);
    sup.disconnect();
}
