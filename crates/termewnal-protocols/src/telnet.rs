//! Async pure-Rust Telnet (`RFC 854`/`855`/`1073`/`1091`) terminal engine with IAC negotiation,
//! dynamic `NAWS` window resizing, `LoginScriptRunner` automation, and automatic reconnect.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use termewnal_core::{LoginScriptStep, ProfileTarget};
use termewnal_pty::{LoginScriptDispatch, LoginScriptRunner, TerminalBuffer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::{ProtocolError, ProtocolStatus};

pub const IAC: u8 = 255;
pub const SE: u8 = 240;
pub const NOP: u8 = 241;
pub const DM: u8 = 242;
pub const BRK: u8 = 243;
pub const IP: u8 = 244;
pub const AO: u8 = 245;
pub const AYT: u8 = 246;
pub const EC: u8 = 247;
pub const EL: u8 = 248;
pub const GA: u8 = 249;
pub const SB: u8 = 250;
pub const WILL: u8 = 251;
pub const WONT: u8 = 252;
pub const DO: u8 = 253;
pub const DONT: u8 = 254;

pub const OPT_BINARY: u8 = 0;
pub const OPT_ECHO: u8 = 1;
pub const OPT_SGA: u8 = 3;
pub const OPT_TTYPE: u8 = 24;
pub const OPT_NAWS: u8 = 31;

const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// Parsed Telnet control/negotiation event emitted by [`TelnetParser`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetEvent {
    Command(u8),
    Negotiation { command: u8, option: u8 },
    Subnegotiation { option: u8, payload: Vec<u8> },
}

/// Output produced by feeding a wire byte chunk into [`TelnetParser`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedTelnetChunk {
    pub payload: Vec<u8>,
    pub events: Vec<TelnetEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParserState {
    Normal,
    Iac,
    Negotiation(u8),
    SubnegStart,
    SubnegData { option: u8, buf: Vec<u8> },
    SubnegIac { option: u8, buf: Vec<u8> },
}

/// Incremental NVT/IAC state machine handling split TCP reads without corrupting control sequences.
#[derive(Debug)]
pub struct TelnetParser {
    state: ParserState,
}

impl Default for TelnetParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TelnetParser {
    pub fn new() -> Self {
        Self {
            state: ParserState::Normal,
        }
    }

    pub fn reset(&mut self) {
        self.state = ParserState::Normal;
    }

    pub fn feed(&mut self, bytes: &[u8]) -> ParsedTelnetChunk {
        let mut out = ParsedTelnetChunk {
            payload: Vec::with_capacity(bytes.len()),
            events: Vec::new(),
        };

        for &b in bytes {
            let state = std::mem::replace(&mut self.state, ParserState::Normal);
            match state {
                ParserState::Normal => {
                    if b == IAC {
                        self.state = ParserState::Iac;
                    } else {
                        out.payload.push(b);
                    }
                }
                ParserState::Iac => match b {
                    IAC => {
                        out.payload.push(IAC);
                    }
                    WILL | WONT | DO | DONT => {
                        self.state = ParserState::Negotiation(b);
                    }
                    SB => {
                        self.state = ParserState::SubnegStart;
                    }
                    cmd => {
                        out.events.push(TelnetEvent::Command(cmd));
                    }
                },
                ParserState::Negotiation(command) => {
                    out.events.push(TelnetEvent::Negotiation {
                        command,
                        option: b,
                    });
                }
                ParserState::SubnegStart => {
                    if b == IAC {
                        // Malformed empty SB; wait for SE/next.
                        self.state = ParserState::Iac;
                    } else {
                        self.state = ParserState::SubnegData {
                            option: b,
                            buf: Vec::new(),
                        };
                    }
                }
                ParserState::SubnegData { option, mut buf } => {
                    if b == IAC {
                        self.state = ParserState::SubnegIac { option, buf };
                    } else {
                        if buf.len() < 4096 {
                            buf.push(b);
                        }
                        self.state = ParserState::SubnegData { option, buf };
                    }
                }
                ParserState::SubnegIac { option, mut buf } => match b {
                    SE => {
                        out.events.push(TelnetEvent::Subnegotiation {
                            option,
                            payload: buf,
                        });
                    }
                    IAC => {
                        if buf.len() < 4096 {
                            buf.push(IAC);
                        }
                        self.state = ParserState::SubnegData { option, buf };
                    }
                    other => {
                        // Unescaped IAC inside subneg aborted subneg cleanly.
                        if other == WILL || other == WONT || other == DO || other == DONT {
                            self.state = ParserState::Negotiation(other);
                        }
                    }
                },
            }
        }

        out
    }
}

/// Escapes literal `0xFF` bytes as `IAC IAC` before transmitting user payload over Telnet.
pub fn escape_telnet_outgoing(bytes: &[u8]) -> Vec<u8> {
    if !bytes.contains(&IAC) {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 8);
    for &b in bytes {
        if b == IAC {
            out.push(IAC);
            out.push(IAC);
        } else {
            out.push(b);
        }
    }
    out
}

/// Builds an `IAC SB NAWS <cols:be16> <rows:be16> IAC SE` Telnet subnegotiation frame.
pub fn build_naws_frame(cols: u16, rows: u16) -> Vec<u8> {
    let mut frame = Vec::with_capacity(11);
    frame.extend_from_slice(&[IAC, SB, OPT_NAWS]);
    for b in cols.to_be_bytes() {
        frame.push(b);
        if b == IAC {
            frame.push(IAC);
        }
    }
    for b in rows.to_be_bytes() {
        frame.push(b);
        if b == IAC {
            frame.push(IAC);
        }
    }
    frame.extend_from_slice(&[IAC, SE]);
    frame
}

/// Builds an `IAC SB TTYPE IS <term_name> IAC SE` Telnet subnegotiation frame.
pub fn build_ttype_is_frame(term_name: &str) -> Vec<u8> {
    let mut frame = Vec::with_capacity(6 + term_name.len());
    frame.extend_from_slice(&[IAC, SB, OPT_TTYPE, TTYPE_IS]);
    for &b in term_name.as_bytes() {
        frame.push(b);
        if b == IAC {
            frame.push(IAC);
        }
    }
    frame.extend_from_slice(&[IAC, SE]);
    frame
}

/// Tracks negotiated Telnet options to avoid negotiation loops.
#[derive(Debug)]
struct TelnetOptionState {
    term_name: String,
    cols: u16,
    rows: u16,
    local_will: [bool; 256],
    local_announced: [bool; 256],
    peer_will: [bool; 256],
}

impl TelnetOptionState {
    fn new(term_name: String, cols: u16, rows: u16) -> Self {
        Self {
            term_name,
            cols: cols.max(1),
            rows: rows.max(1),
            local_will: [false; 256],
            local_announced: [false; 256],
            peer_will: [false; 256],
        }
    }

    fn reset(&mut self) {
        self.local_will = [false; 256];
        self.local_announced = [false; 256];
        self.peer_will = [false; 256];
    }

    fn initial_offers(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for opt in [OPT_NAWS, OPT_TTYPE, OPT_SGA, OPT_BINARY] {
            self.local_will[opt as usize] = true;
            self.local_announced[opt as usize] = true;
            out.extend_from_slice(&[IAC, WILL, opt]);
        }
        self.peer_will[OPT_SGA as usize] = true;
        out.extend_from_slice(&[IAC, DO, OPT_SGA]);
        out
    }

    fn naws_enabled(&self) -> bool {
        self.local_will[OPT_NAWS as usize]
    }

    fn handle_event(&mut self, event: TelnetEvent) -> Vec<u8> {
        let mut replies = Vec::new();
        match event {
            TelnetEvent::Command(_) => {}
            TelnetEvent::Negotiation { command, option } => {
                let idx = option as usize;
                match command {
                    DO => {
                        let supported = matches!(
                            option,
                            OPT_NAWS | OPT_TTYPE | OPT_SGA | OPT_BINARY
                        );
                        if supported {
                            if !self.local_will[idx] || !self.local_announced[idx] {
                                self.local_will[idx] = true;
                                self.local_announced[idx] = true;
                                replies.extend_from_slice(&[IAC, WILL, option]);
                            }
                            if option == OPT_NAWS {
                                replies.extend_from_slice(&build_naws_frame(self.cols, self.rows));
                            }
                        } else if !self.local_announced[idx] || self.local_will[idx] {
                            self.local_will[idx] = false;
                            self.local_announced[idx] = true;
                            replies.extend_from_slice(&[IAC, WONT, option]);
                        }
                    }
                    DONT => {
                        if self.local_will[idx] {
                            self.local_will[idx] = false;
                            replies.extend_from_slice(&[IAC, WONT, option]);
                        }
                    }
                    WILL => {
                        let accept = matches!(option, OPT_ECHO | OPT_SGA | OPT_BINARY);
                        if accept {
                            if !self.peer_will[idx] {
                                self.peer_will[idx] = true;
                                replies.extend_from_slice(&[IAC, DO, option]);
                            }
                        } else {
                            self.peer_will[idx] = false;
                            replies.extend_from_slice(&[IAC, DONT, option]);
                        }
                    }
                    WONT => {
                        if self.peer_will[idx] {
                            self.peer_will[idx] = false;
                            replies.extend_from_slice(&[IAC, DONT, option]);
                        }
                    }
                    _ => {}
                }
            }
            TelnetEvent::Subnegotiation { option, payload } => {
                if option == OPT_TTYPE && payload.first().copied() == Some(TTYPE_SEND) {
                    replies.extend_from_slice(&build_ttype_is_frame(&self.term_name));
                }
            }
        }
        replies
    }
}

/// Connection configuration for [`TelnetSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelnetConnectConfig {
    pub host: String,
    pub port: u16,
    pub term_name: String,
    pub cols: u16,
    pub rows: u16,
    pub auto_reconnect: bool,
    pub reconnect_delay: Duration,
    pub connect_timeout: Duration,
    pub login_scripts: Vec<LoginScriptStep>,
}

impl Default for TelnetConnectConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 23,
            term_name: "xterm-256color".to_string(),
            cols: 80,
            rows: 24,
            auto_reconnect: false,
            reconnect_delay: DEFAULT_RECONNECT_DELAY,
            connect_timeout: Duration::from_secs(10),
            login_scripts: Vec::new(),
        }
    }
}

impl TelnetConnectConfig {
    /// Converts `ProfileTarget::Telnet` into [`TelnetConnectConfig`].
    pub fn from_profile_target(target: &ProfileTarget, cols: u16, rows: u16) -> Option<Self> {
        match target {
            ProfileTarget::Telnet {
                host,
                port,
                auto_reconnect,
                login_scripts,
            } => Some(Self {
                host: host.clone(),
                port: *port,
                cols: cols.max(1),
                rows: rows.max(1),
                auto_reconnect: *auto_reconnect,
                login_scripts: login_scripts.clone(),
                ..Self::default()
            }),
            _ => None,
        }
    }
}

enum TelnetCmd {
    WriteUser(Vec<u8>),
    WriteRaw(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    ReconnectNow,
    Disconnect,
}

struct TelnetWriteSink {
    tx: mpsc::UnboundedSender<TelnetCmd>,
}

impl io::Write for TelnetWriteSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.tx.send(TelnetCmd::WriteRaw(buf.to_vec()));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Live async Telnet TCP client session streaming into a [`TerminalBuffer`].
#[derive(Clone)]
pub struct TelnetSession {
    cmd_tx: mpsc::UnboundedSender<TelnetCmd>,
    status_rx: watch::Receiver<ProtocolStatus>,
    naws_rx: watch::Receiver<bool>,
    buffer: Arc<TerminalBuffer>,
}

impl TelnetSession {
    /// Connects asynchronously to `config.host:config.port` and renders into `buffer`.
    pub fn connect_on_buffer(config: TelnetConnectConfig, buffer: Arc<TerminalBuffer>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = watch::channel(ProtocolStatus::Connecting);
        let (naws_tx, naws_rx) = watch::channel(false);

        buffer.set_write_sink(Some(Box::new(TelnetWriteSink {
            tx: cmd_tx.clone(),
        })));

        let login_runner = LoginScriptRunner::compile(&config.login_scripts, None);
        let opts = TelnetOptionState::new(config.term_name.clone(), config.cols, config.rows);

        let actor = TelnetActor {
            config,
            buffer: Arc::clone(&buffer),
            cmd_tx: cmd_tx.clone(),
            cmd_rx,
            status_tx,
            naws_tx,
            parser: TelnetParser::new(),
            opts,
            login_runner,
        };
        tokio::spawn(actor.run());

        Self {
            cmd_tx,
            status_rx,
            naws_rx,
            buffer,
        }
    }

    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
    }

    pub fn status(&self) -> ProtocolStatus {
        self.status_rx.borrow().clone()
    }

    pub fn subscribe_status(&self) -> watch::Receiver<ProtocolStatus> {
        self.status_rx.clone()
    }

    pub fn is_naws_enabled(&self) -> bool {
        *self.naws_rx.borrow()
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), ProtocolError> {
        self.cmd_tx
            .send(TelnetCmd::WriteUser(bytes.to_vec()))
            .map_err(|_| ProtocolError::Disconnected)
    }

    pub fn write_raw_bytes(&self, bytes: &[u8]) -> Result<(), ProtocolError> {
        self.cmd_tx
            .send(TelnetCmd::WriteRaw(bytes.to_vec()))
            .map_err(|_| ProtocolError::Disconnected)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), ProtocolError> {
        self.buffer.resize(cols, rows);
        self.cmd_tx
            .send(TelnetCmd::Resize {
                cols: cols.max(1),
                rows: rows.max(1),
            })
            .map_err(|_| ProtocolError::Disconnected)
    }

    pub fn reconnect_now(&self) {
        let _ = self.cmd_tx.send(TelnetCmd::ReconnectNow);
    }

    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(TelnetCmd::Disconnect);
    }
}

struct TelnetActor {
    config: TelnetConnectConfig,
    buffer: Arc<TerminalBuffer>,
    cmd_tx: mpsc::UnboundedSender<TelnetCmd>,
    cmd_rx: mpsc::UnboundedReceiver<TelnetCmd>,
    status_tx: watch::Sender<ProtocolStatus>,
    naws_tx: watch::Sender<bool>,
    parser: TelnetParser,
    opts: TelnetOptionState,
    login_runner: LoginScriptRunner,
}

impl TelnetActor {
    async fn run(mut self) {
        let mut attempt: u32 = 0;
        let mut has_connected = false;
        let mut last_error: Option<String> = None;

        'lifecycle: loop {
            if attempt == 0 {
                attempt = 1;
            }
            let _ = self.status_tx.send(if attempt == 1 && !has_connected && last_error.is_none() {
                ProtocolStatus::Connecting
            } else {
                ProtocolStatus::Reconnecting {
                    attempt,
                    last_error: last_error.clone(),
                }
            });

            self.parser.reset();
            self.opts.reset();
            self.login_runner.reset();
            let _ = self.naws_tx.send(false);

            let addr = (self.config.host.as_str(), self.config.port);
            let stream_res =
                tokio::time::timeout(self.config.connect_timeout, TcpStream::connect(addr)).await;
            let mut stream = match stream_res {
                Ok(Ok(s)) => {
                    let _ = s.set_nodelay(true);
                    s
                }
                Ok(Err(err)) => {
                    let msg = format!("{}:{} ({err})", self.config.host, self.config.port);
                    warn!(error = %msg, "telnet TCP connect failed");
                    if !self
                        .handle_disconnect_transition(msg, has_connected, &mut attempt, &mut last_error)
                        .await
                    {
                        break 'lifecycle;
                    }
                    continue 'lifecycle;
                }
                Err(_) => {
                    let msg = format!("{}:{} timed out", self.config.host, self.config.port);
                    warn!(error = %msg, "telnet TCP connect timed out");
                    if !self
                        .handle_disconnect_transition(msg, has_connected, &mut attempt, &mut last_error)
                        .await
                    {
                        break 'lifecycle;
                    }
                    continue 'lifecycle;
                }
            };

            // Connected! Send initial Telnet capability offers.
            let offers = self.opts.initial_offers();
            let _ = self.naws_tx.send(self.opts.naws_enabled());
            if let Err(err) = stream.write_all(&offers).await {
                if !self
                    .handle_disconnect_transition(
                        err.to_string(),
                        has_connected,
                        &mut attempt,
                        &mut last_error,
                    )
                    .await
                {
                    break 'lifecycle;
                }
                continue 'lifecycle;
            }

            if has_connected {
                self.buffer
                    .feed_bytes(b"\r\n--- Telnet reconnected ---\r\n");
            }
            has_connected = true;
            attempt = 0;
            last_error = None;
            let _ = self.status_tx.send(ProtocolStatus::Connected);

            if !self.login_runner.is_finished() {
                let dispatches = self.login_runner.advance(&[]);
                schedule_telnet_dispatches(&self.cmd_tx, dispatches);
            }

            let mut read_buf = [0_u8; 4096];
            loop {
                tokio::select! {
                    read_res = stream.read(&mut read_buf) => {
                        match read_res {
                            Ok(0) => {
                                debug!("telnet server closed TCP connection");
                                if !self.handle_disconnect_transition("remote host closed connection".to_string(), true, &mut attempt, &mut last_error).await {
                                    break 'lifecycle;
                                }
                                continue 'lifecycle;
                            }
                            Ok(n) => {
                                let parsed = self.parser.feed(&read_buf[..n]);
                                let mut replies = Vec::new();
                                for ev in parsed.events {
                                    replies.extend_from_slice(&self.opts.handle_event(ev));
                                }
                                let _ = self.naws_tx.send(self.opts.naws_enabled());
                                if !replies.is_empty() && let Err(err) = stream.write_all(&replies).await {
                                    if !self.handle_disconnect_transition(err.to_string(), true, &mut attempt, &mut last_error).await {
                                        break 'lifecycle;
                                    }
                                    continue 'lifecycle;
                                }
                                if !parsed.payload.is_empty() {
                                    self.buffer.feed_bytes(&parsed.payload);
                                    if !self.login_runner.is_finished() {
                                        let dispatches = self.login_runner.advance(&parsed.payload);
                                        schedule_telnet_dispatches(&self.cmd_tx, dispatches);
                                    }
                                }
                            }
                            Err(err) => {
                                if !self.handle_disconnect_transition(err.to_string(), true, &mut attempt, &mut last_error).await {
                                    break 'lifecycle;
                                }
                                continue 'lifecycle;
                            }
                        }
                    }
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(TelnetCmd::WriteUser(bytes)) => {
                                let escaped = escape_telnet_outgoing(&bytes);
                                if let Err(err) = stream.write_all(&escaped).await {
                                    if !self.handle_disconnect_transition(err.to_string(), true, &mut attempt, &mut last_error).await {
                                        break 'lifecycle;
                                    }
                                    continue 'lifecycle;
                                }
                            }
                            Some(TelnetCmd::WriteRaw(bytes)) => {
                                if let Err(err) = stream.write_all(&bytes).await {
                                    if !self.handle_disconnect_transition(err.to_string(), true, &mut attempt, &mut last_error).await {
                                        break 'lifecycle;
                                    }
                                    continue 'lifecycle;
                                }
                            }
                            Some(TelnetCmd::Resize { cols, rows }) => {
                                self.opts.cols = cols;
                                self.opts.rows = rows;
                                if self.opts.naws_enabled() {
                                    let frame = build_naws_frame(cols, rows);
                                    if let Err(err) = stream.write_all(&frame).await {
                                        if !self.handle_disconnect_transition(err.to_string(), true, &mut attempt, &mut last_error).await {
                                            break 'lifecycle;
                                        }
                                        continue 'lifecycle;
                                    }
                                }
                            }
                            Some(TelnetCmd::ReconnectNow) => {
                                let _ = stream.shutdown().await;
                                attempt = 0;
                                last_error = None;
                                continue 'lifecycle;
                            }
                            Some(TelnetCmd::Disconnect) | None => {
                                let _ = stream.shutdown().await;
                                break 'lifecycle;
                            }
                        }
                    }
                }
            }
        }

        self.buffer.set_write_sink(None);
        let _ = self.status_tx.send(ProtocolStatus::Disconnected);
    }

    async fn handle_disconnect_transition(
        &mut self,
        reason: String,
        had_session: bool,
        attempt: &mut u32,
        last_error: &mut Option<String>,
    ) -> bool {
        if had_session || !self.config.auto_reconnect {
            self.buffer.feed_bytes(
                format!("\r\n--- Telnet disconnected ({reason}) ---\r\n").as_bytes(),
            );
        }
        if !self.config.auto_reconnect {
            let _ = self.status_tx.send(ProtocolStatus::Disconnected);
            *attempt = 0;
            *last_error = None;
            return self.wait_when_disconnected().await;
        }
        *attempt = if *attempt == 0 {
            1
        } else {
            attempt.saturating_add(1)
        };
        *last_error = Some(reason.clone());
        let _ = self.status_tx.send(ProtocolStatus::Reconnecting {
            attempt: *attempt,
            last_error: Some(reason),
        });
        self.wait_reconnect_delay().await
    }

    async fn wait_reconnect_delay(&mut self) -> bool {
        let delay = self.config.reconnect_delay.max(Duration::from_millis(10));
        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);

        loop {
            tokio::select! {
                _ = &mut timer => return true,
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(TelnetCmd::ReconnectNow) => return true,
                        Some(TelnetCmd::Resize { cols, rows }) => {
                            self.opts.cols = cols;
                            self.opts.rows = rows;
                        }
                        Some(TelnetCmd::Disconnect) | None => return false,
                        Some(TelnetCmd::WriteUser(_)) | Some(TelnetCmd::WriteRaw(_)) => {}
                    }
                }
            }
        }
    }

    async fn wait_when_disconnected(&mut self) -> bool {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                TelnetCmd::ReconnectNow => return true,
                TelnetCmd::Resize { cols, rows } => {
                    self.opts.cols = cols;
                    self.opts.rows = rows;
                }
                TelnetCmd::Disconnect => return false,
                TelnetCmd::WriteUser(_) | TelnetCmd::WriteRaw(_) => {}
            }
        }
        false
    }
}

fn schedule_telnet_dispatches(
    cmd_tx: &mpsc::UnboundedSender<TelnetCmd>,
    dispatches: Vec<LoginScriptDispatch>,
) {
    if dispatches.is_empty() {
        return;
    }
    let mut iter = dispatches.into_iter().peekable();
    while let Some(next) = iter.peek() {
        if !next.delay.is_zero() {
            break;
        }
        let d = iter.next().expect("peeked dispatch");
        let _ = cmd_tx.send(TelnetCmd::WriteUser(d.bytes.to_vec()));
    }
    let remaining: Vec<_> = iter.collect();
    if remaining.is_empty() {
        return;
    }
    let tx = cmd_tx.clone();
    tokio::spawn(async move {
        for dispatch in remaining {
            if !dispatch.delay.is_zero() {
                tokio::time::sleep(dispatch.delay).await;
            }
            if tx.send(TelnetCmd::WriteUser(dispatch.bytes.to_vec())).is_err() {
                break;
            }
        }
    });
}
