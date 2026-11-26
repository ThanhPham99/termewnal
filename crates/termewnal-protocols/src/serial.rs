//! Async serial terminal engine backed by `serial2` with RS-232 settings, input/output modes,
//! hexdump rendering, newline conversion, and automatic reconnect.

use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serial2::{CharSize, FlowControl, Parity, SerialPort, Settings, StopBits};
use termewnal_core::{
    NewlineMode, ProfileTarget, SerialFlowControl, SerialInputMode, SerialOutputMode, SerialParity,
};
use termewnal_pty::TerminalBuffer;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::{ProtocolError, ProtocolStatus};

const READ_TIMEOUT: Duration = Duration::from_millis(40);
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const MAX_READLINE_HISTORY: usize = 64;

/// Discovered local serial device metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPortInfo {
    pub path: String,
    pub display_name: String,
}

/// Runtime configuration for opening and formatting a [`SerialSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialConnectConfig {
    pub port: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub parity: SerialParity,
    pub stop_bits: u8,
    pub flow_control: SerialFlowControl,
    pub input_mode: SerialInputMode,
    pub output_mode: SerialOutputMode,
    pub newline_mode: NewlineMode,
    pub auto_reconnect: bool,
    pub reconnect_delay: Duration,
}

impl Default for SerialConnectConfig {
    fn default() -> Self {
        Self {
            port: String::new(),
            baud_rate: 115_200,
            data_bits: 8,
            parity: SerialParity::None,
            stop_bits: 1,
            flow_control: SerialFlowControl::None,
            input_mode: SerialInputMode::Text,
            output_mode: SerialOutputMode::Text,
            newline_mode: NewlineMode::Cr,
            auto_reconnect: true,
            reconnect_delay: DEFAULT_RECONNECT_DELAY,
        }
    }
}

impl SerialConnectConfig {
    /// Converts a `ProfileTarget::Serial` into [`SerialConnectConfig`].
    pub fn from_profile_target(target: &ProfileTarget) -> Option<Self> {
        match target {
            ProfileTarget::Serial {
                port,
                baud_rate,
                data_bits,
                parity,
                stop_bits,
                flow_control,
                input_mode,
                output_mode,
                newline_mode,
                auto_reconnect,
            } => Some(Self {
                port: port.clone(),
                baud_rate: *baud_rate,
                data_bits: *data_bits,
                parity: *parity,
                stop_bits: *stop_bits,
                flow_control: *flow_control,
                input_mode: *input_mode,
                output_mode: *output_mode,
                newline_mode: *newline_mode,
                auto_reconnect: *auto_reconnect,
                reconnect_delay: DEFAULT_RECONNECT_DELAY,
            }),
            _ => None,
        }
    }

    /// Validates RS-232 parameters and applies them to `serial2::Settings`.
    pub fn apply_to_settings(&self, settings: &mut Settings) -> Result<(), ProtocolError> {
        if self.port.trim().is_empty() {
            return Err(ProtocolError::InvalidInput(
                "serial port path must not be empty".to_string(),
            ));
        }
        if self.baud_rate == 0 {
            return Err(ProtocolError::InvalidInput(
                "baud rate must be greater than zero".to_string(),
            ));
        }
        let char_size = CharSize::try_from(self.data_bits).map_err(|_| {
            ProtocolError::InvalidInput(format!(
                "unsupported serial data bits `{}`; expected 5, 6, 7, or 8",
                self.data_bits
            ))
        })?;
        let stop_bits = StopBits::try_from(self.stop_bits).map_err(|_| {
            ProtocolError::InvalidInput(format!(
                "unsupported serial stop bits `{}`; expected 1 or 2",
                self.stop_bits
            ))
        })?;
        let parity = match self.parity {
            SerialParity::None => Parity::None,
            SerialParity::Odd => Parity::Odd,
            SerialParity::Even => Parity::Even,
        };
        let flow = match self.flow_control {
            SerialFlowControl::None => FlowControl::None,
            SerialFlowControl::Software => FlowControl::XonXoff,
            SerialFlowControl::Hardware => FlowControl::RtsCts,
        };

        settings.set_raw();
        settings.set_baud_rate(self.baud_rate).map_err(|err| {
            ProtocolError::Serial(format!("unsupported baud rate {}: {err}", self.baud_rate))
        })?;
        settings.set_char_size(char_size);
        settings.set_stop_bits(stop_bits);
        settings.set_parity(parity);
        settings.set_flow_control(flow);
        Ok(())
    }
}

/// Lists available serial ports discovered via `serial2`.
pub fn list_available_serial_ports() -> Result<Vec<SerialPortInfo>, ProtocolError> {
    let mut paths = SerialPort::available_ports().map_err(|err| {
        ProtocolError::Serial(format!("failed to enumerate serial ports: {err}"))
    })?;
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|p| {
            let path_str = p.to_string_lossy().into_owned();
            let display_name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| path_str.clone());
            SerialPortInfo {
                path: path_str,
                display_name,
            }
        })
        .collect())
}

/// Thread-safe full-duplex byte stream abstraction implemented by OS serial ports and virtual mocks.
pub trait SerialByteStream: Send + Sync {
    fn read_chunk(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_all_bytes(&self, bytes: &[u8]) -> io::Result<()>;
    fn flush_bytes(&self) -> io::Result<()>;
}

struct OsSerialStream {
    port: SerialPort,
}

impl SerialByteStream for OsSerialStream {
    fn read_chunk(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.port.read(buf)
    }

    fn write_all_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.port.write_all(bytes)
    }

    fn flush_bytes(&self) -> io::Result<()> {
        self.port.flush()
    }
}

/// Opens and discovers serial ports (production OS implementation or virtual test double).
pub trait SerialPortOpener: Send + Sync {
    fn list_ports(&self) -> Result<Vec<SerialPortInfo>, ProtocolError>;
    fn open(&self, config: &SerialConnectConfig)
    -> Result<Arc<dyn SerialByteStream>, ProtocolError>;
}

/// Production [`SerialPortOpener`] using `serial2`.
#[derive(Debug, Default)]
pub struct OsSerialPortOpener;

impl SerialPortOpener for OsSerialPortOpener {
    fn list_ports(&self) -> Result<Vec<SerialPortInfo>, ProtocolError> {
        list_available_serial_ports()
    }

    fn open(
        &self,
        config: &SerialConnectConfig,
    ) -> Result<Arc<dyn SerialByteStream>, ProtocolError> {
        // Validate early before touching the OS device node.
        let port_path = config.port.trim();
        if port_path.is_empty() {
            return Err(ProtocolError::InvalidInput(
                "serial port path must not be empty".to_string(),
            ));
        }

        let mut validation_error: Option<ProtocolError> = None;
        let mut port = SerialPort::open(port_path, |mut settings: Settings| {
            if let Err(err) = config.apply_to_settings(&mut settings) {
                validation_error = Some(err);
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid serial config"));
            }
            Ok(settings)
        })
        .map_err(|err| {
            validation_error.take().unwrap_or_else(|| {
                ProtocolError::Serial(format!("failed opening `{port_path}`: {err}"))
            })
        })?;

        port.set_read_timeout(READ_TIMEOUT).map_err(|err| {
            ProtocolError::Serial(format!("failed setting read timeout on `{port_path}`: {err}"))
        })?;

        Ok(Arc::new(OsSerialStream { port }))
    }
}

/// Parses whitespace/comma/semicolon/`0x`-separated hex bytes (e.g. `"AA 55 0D"` or `"0xaa,0x55"`).
pub fn parse_hex_input(input: &str) -> Result<Vec<u8>, ProtocolError> {
    let mut digits = String::new();
    for token in input.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ':') {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }
        let hex_part = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        if hex_part.is_empty() {
            return Err(ProtocolError::InvalidInput(format!(
                "empty hex token `{trimmed}`"
            )));
        }
        if !hex_part.len().is_multiple_of(2) {
            return Err(ProtocolError::InvalidInput(format!(
                "odd number of hex digits in token `{trimmed}`"
            )));
        }
        for b in hex_part.chars() {
            if !b.is_ascii_hexdigit() {
                return Err(ProtocolError::InvalidInput(format!(
                    "invalid hex character `{b}` in `{trimmed}`"
                )));
            }
        }
        digits.push_str(hex_part);
    }

    if digits.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(digits.len() / 2);
    let raw = digits.as_bytes();
    for pair in raw.chunks_exact(2) {
        let s = std::str::from_utf8(pair).expect("verified ASCII hex");
        let val = u8::from_str_radix(s, 16)
            .map_err(|e| ProtocolError::InvalidInput(format!("invalid hex byte `{s}`: {e}")))?;
        out.push(val);
    }
    Ok(out)
}

/// Converts line breaks in outgoing user bytes according to [`NewlineMode`].
pub fn convert_outgoing_newlines(bytes: &[u8], mode: NewlineMode) -> Vec<u8> {
    if mode == NewlineMode::Keep || bytes.is_empty() {
        return bytes.to_vec();
    }

    let target: &[u8] = match mode {
        NewlineMode::Cr => b"\r",
        NewlineMode::Lf => b"\n",
        NewlineMode::CrLf => b"\r\n",
        NewlineMode::Keep => unreachable!(),
    };

    let mut out = Vec::with_capacity(bytes.len() + 4);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            out.extend_from_slice(target);
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
        } else if bytes[i] == b'\n' {
            out.extend_from_slice(target);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Normalizes incoming serial bytes before display in [`NewlineMode`] (`Text` mode).
pub fn convert_incoming_newlines(bytes: &[u8], mode: NewlineMode) -> Vec<u8> {
    if mode == NewlineMode::Keep || bytes.is_empty() {
        return bytes.to_vec();
    }

    let mut out = Vec::with_capacity(bytes.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        match mode {
            NewlineMode::Keep => out.push(bytes[i]),
            NewlineMode::CrLf => {
                if bytes[i] == b'\r' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        out.extend_from_slice(b"\r\n");
                        i += 2;
                    } else {
                        out.extend_from_slice(b"\r\n");
                        i += 1;
                    }
                } else if bytes[i] == b'\n' {
                    out.extend_from_slice(b"\r\n");
                    i += 1;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            NewlineMode::Lf => {
                if bytes[i] == b'\r' {
                    out.push(b'\n');
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            NewlineMode::Cr => {
                if bytes[i] == b'\r' {
                    out.push(b'\r');
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else if bytes[i] == b'\n' {
                    out.push(b'\r');
                    i += 1;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        }
    }
    out
}

/// Local-echo line editor used by [`SerialInputMode::Readline`] and [`SerialInputMode::HexBytes`].
#[derive(Debug, Default)]
pub struct SerialReadlineBuffer {
    line: String,
    history: VecDeque<String>,
    history_cursor: Option<usize>,
}

/// Action produced when feeding terminal key bytes into [`SerialReadlineBuffer`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReadlineStep {
    /// Bytes echoed back to the local [`TerminalBuffer`] immediately.
    pub local_echo: Vec<u8>,
    /// Completed lines ready for wire transmission (`Enter` pressed).
    pub submitted_lines: Vec<String>,
}

impl SerialReadlineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current_line(&self) -> &str {
        &self.line
    }

    pub fn reset(&mut self) {
        self.line.clear();
        self.history_cursor = None;
    }

    /// Processes raw terminal keystroke bytes, returning local echo and completed lines.
    pub fn feed_input(&mut self, bytes: &[u8]) -> ReadlineStep {
        let mut step = ReadlineStep::default();
        let mut i = 0;

        while i < bytes.len() {
            // Up/Down arrow history navigation (`ESC [ A` / `ESC [ B`).
            if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
                match bytes[i + 2] {
                    b'A' => {
                        self.recall_prev(&mut step.local_echo);
                        i += 3;
                        continue;
                    }
                    b'B' => {
                        self.recall_next(&mut step.local_echo);
                        i += 3;
                        continue;
                    }
                    _ => {
                        i += 3;
                        continue;
                    }
                }
            }

            match bytes[i] {
                b'\r' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        i += 1;
                    }
                    self.commit_line(&mut step);
                    i += 1;
                }
                b'\n' => {
                    self.commit_line(&mut step);
                    i += 1;
                }
                0x08 | 0x7f => {
                    if let Some(_popped) = self.line.pop() {
                        step.local_echo.extend_from_slice(b"\x08 \x08");
                    }
                    i += 1;
                }
                0x03 => {
                    self.line.clear();
                    self.history_cursor = None;
                    step.local_echo.extend_from_slice(b"^C\r\n");
                    i += 1;
                }
                b'\t' => {
                    self.line.push('\t');
                    step.local_echo.push(b'\t');
                    i += 1;
                }
                b if b < 0x20 => {
                    i += 1;
                }
                first => {
                    let width = utf8_char_width(first);
                    let end = (i + width).min(bytes.len());
                    let slice = &bytes[i..end];
                    if let Ok(text) = std::str::from_utf8(slice) {
                        self.line.push_str(text);
                        step.local_echo.extend_from_slice(slice);
                        self.history_cursor = None;
                    }
                    i = end;
                }
            }
        }

        step
    }

    fn commit_line(&mut self, step: &mut ReadlineStep) {
        let finished = std::mem::take(&mut self.line);
        self.history_cursor = None;
        if !finished.is_empty() && self.history.back().map(String::as_str) != Some(finished.as_str())
        {
            if self.history.len() >= MAX_READLINE_HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(finished.clone());
        }
        step.local_echo.extend_from_slice(b"\r\n");
        step.submitted_lines.push(finished);
    }

    fn replace_line(&mut self, next: String, local_echo: &mut Vec<u8>) {
        for _ in self.line.chars() {
            local_echo.extend_from_slice(b"\x08 \x08");
        }
        self.line = next;
        local_echo.extend_from_slice(self.line.as_bytes());
    }

    fn recall_prev(&mut self, local_echo: &mut Vec<u8>) {
        if self.history.is_empty() {
            return;
        }
        let next_idx = match self.history_cursor {
            Some(0) => 0,
            Some(idx) => idx - 1,
            None => self.history.len() - 1,
        };
        self.history_cursor = Some(next_idx);
        if let Some(entry) = self.history.get(next_idx).cloned() {
            self.replace_line(entry, local_echo);
        }
    }

    fn recall_next(&mut self, local_echo: &mut Vec<u8>) {
        let Some(idx) = self.history_cursor else {
            return;
        };
        if idx + 1 < self.history.len() {
            let next_idx = idx + 1;
            self.history_cursor = Some(next_idx);
            if let Some(entry) = self.history.get(next_idx).cloned() {
                self.replace_line(entry, local_echo);
            }
        } else {
            self.history_cursor = None;
            self.replace_line(String::new(), local_echo);
        }
    }
}

fn utf8_char_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first & 0b1110_0000 == 0b1100_0000 {
        2
    } else if first & 0b1111_0000 == 0b1110_0000 {
        3
    } else if first & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        1
    }
}

/// Stateful canonical 16-byte hexdump formatter tracking continuous stream offsets.
#[derive(Debug, Default)]
pub struct HexdumpFormatter {
    offset: usize,
}

impl HexdumpFormatter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn reset(&mut self) {
        self.offset = 0;
    }

    /// Formats `bytes` into `OFFSET  HH HH ...  |ASCII...|\\r\\n` rows for terminal rendering.
    pub fn format_chunk(&mut self, bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return String::new();
        }

        let mut out = String::with_capacity(bytes.len() * 5);
        for chunk in bytes.chunks(16) {
            out.push_str(&format!("{:08x}  ", self.offset));
            for idx in 0..16 {
                if let Some(b) = chunk.get(idx) {
                    out.push_str(&format!("{b:02x} "));
                } else {
                    out.push_str("   ");
                }
                if idx == 7 {
                    out.push(' ');
                }
            }
            out.push_str(" |");
            for &b in chunk {
                if (0x20..=0x7e).contains(&b) {
                    out.push(b as char);
                } else {
                    out.push('.');
                }
            }
            out.push_str("|\r\n");
            self.offset += chunk.len();
        }
        out
    }
}

#[derive(Debug, Clone, Copy)]
struct SerialModes {
    input: SerialInputMode,
    output: SerialOutputMode,
    newline: NewlineMode,
    auto_reconnect: bool,
    reset_hexdump: bool,
}

enum SerialCmd {
    UserInput(Vec<u8>),
    RawWire(Vec<u8>),
    AutoReconnectDisabled,
    AutoReconnectEnabled,
    ReconnectNow,
    Disconnect,
}

enum ReaderEvent {
    Data { gen_id: u64, bytes: Vec<u8> },
    Error { gen_id: u64, reason: String },
}

struct SerialWriteSink {
    tx: mpsc::UnboundedSender<SerialCmd>,
}

impl io::Write for SerialWriteSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.tx.send(SerialCmd::RawWire(buf.to_vec()));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Supervised live serial session connected to a [`TerminalBuffer`].
#[derive(Clone)]
pub struct SerialSession {
    cmd_tx: mpsc::UnboundedSender<SerialCmd>,
    status_rx: watch::Receiver<ProtocolStatus>,
    modes: Arc<std::sync::Mutex<SerialModes>>,
    readline: Arc<std::sync::Mutex<SerialReadlineBuffer>>,
    shutdown_flag: Arc<AtomicBool>,
    buffer: Arc<TerminalBuffer>,
}

impl SerialSession {
    /// Connects a serial port session on `buffer` using the OS `serial2` backend.
    pub fn connect_on_buffer(config: SerialConnectConfig, buffer: Arc<TerminalBuffer>) -> Self {
        Self::connect_with_opener(config, buffer, Arc::new(OsSerialPortOpener))
    }

    /// Connects a serial port session on `buffer` with a custom [`SerialPortOpener`].
    pub fn connect_with_opener(
        config: SerialConnectConfig,
        buffer: Arc<TerminalBuffer>,
        opener: Arc<dyn SerialPortOpener>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = watch::channel(ProtocolStatus::Connecting);
        let modes = Arc::new(std::sync::Mutex::new(SerialModes {
            input: config.input_mode,
            output: config.output_mode,
            newline: config.newline_mode,
            auto_reconnect: config.auto_reconnect,
            reset_hexdump: false,
        }));
        let readline = Arc::new(std::sync::Mutex::new(SerialReadlineBuffer::new()));
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        buffer.set_write_sink(Some(Box::new(SerialWriteSink {
            tx: cmd_tx.clone(),
        })));

        let actor = SerialActor {
            config,
            opener,
            buffer: Arc::clone(&buffer),
            cmd_rx,
            status_tx,
            modes: Arc::clone(&modes),
            readline: Arc::clone(&readline),
            shutdown_flag: Arc::clone(&shutdown_flag),
            hexdump: HexdumpFormatter::new(),
            active_stream: None,
            reader_stop: None,
            reader_gen: 0,
        };
        tokio::spawn(actor.run());

        Self {
            cmd_tx,
            status_rx,
            modes,
            readline,
            shutdown_flag,
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

    /// Sends interactive terminal bytes (respecting current `input_mode` & `newline_mode`).
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), ProtocolError> {
        self.cmd_tx
            .send(SerialCmd::UserInput(bytes.to_vec()))
            .map_err(|_| ProtocolError::Disconnected)
    }

    /// Parses hex string (`AA 55 0D`) and writes resulting bytes directly to the wire.
    pub fn send_hex(&self, hex: &str) -> Result<(), ProtocolError> {
        let parsed = parse_hex_input(hex)?;
        self.send_raw_bytes(&parsed)
    }

    /// Writes exact raw bytes to the serial port without mode translation.
    pub fn send_raw_bytes(&self, bytes: &[u8]) -> Result<(), ProtocolError> {
        self.cmd_tx
            .send(SerialCmd::RawWire(bytes.to_vec()))
            .map_err(|_| ProtocolError::Disconnected)
    }

    pub fn set_input_mode(&self, mode: SerialInputMode) {
        let changed = {
            let mut guard = self.modes.lock().expect("serial modes mutex");
            let changed = guard.input != mode;
            guard.input = mode;
            changed
        };
        if changed {
            self.readline.lock().expect("serial readline mutex").reset();
        }
    }

    pub fn set_output_mode(&self, mode: SerialOutputMode) {
        let mut guard = self.modes.lock().expect("serial modes mutex");
        if guard.output != mode {
            guard.output = mode;
            if mode == SerialOutputMode::Hexdump {
                guard.reset_hexdump = true;
            }
        }
    }

    pub fn set_newline_mode(&self, mode: NewlineMode) {
        self.modes.lock().expect("serial modes mutex").newline = mode;
    }

    pub fn set_auto_reconnect(&self, enabled: bool) {
        let changed = {
            let mut guard = self.modes.lock().expect("serial modes mutex");
            let changed = guard.auto_reconnect != enabled;
            guard.auto_reconnect = enabled;
            changed
        };
        if changed {
            let cmd = if enabled {
                SerialCmd::AutoReconnectEnabled
            } else {
                SerialCmd::AutoReconnectDisabled
            };
            let _ = self.cmd_tx.send(cmd);
        }
    }

    pub fn reconnect_now(&self) {
        let _ = self.cmd_tx.send(SerialCmd::ReconnectNow);
    }

    pub fn disconnect(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(SerialCmd::Disconnect);
    }
}

struct SerialActor {
    config: SerialConnectConfig,
    opener: Arc<dyn SerialPortOpener>,
    buffer: Arc<TerminalBuffer>,
    cmd_rx: mpsc::UnboundedReceiver<SerialCmd>,
    status_tx: watch::Sender<ProtocolStatus>,
    modes: Arc<std::sync::Mutex<SerialModes>>,
    readline: Arc<std::sync::Mutex<SerialReadlineBuffer>>,
    shutdown_flag: Arc<AtomicBool>,
    hexdump: HexdumpFormatter,
    active_stream: Option<Arc<dyn SerialByteStream>>,
    reader_stop: Option<Arc<AtomicBool>>,
    reader_gen: u64,
}

impl Drop for SerialActor {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        self.teardown_stream();
        self.buffer.set_write_sink(None);
    }
}

impl SerialActor {
    fn current_modes(&self) -> SerialModes {
        let mut guard = self.modes.lock().expect("serial modes mutex");
        let snapshot = *guard;
        guard.reset_hexdump = false;
        snapshot
    }

    async fn run(mut self) {
        let (reader_tx, mut reader_rx) = mpsc::unbounded_channel::<ReaderEvent>();
        let mut attempt: u32 = 0;
        let mut has_connected_before = false;
        let mut last_error: Option<String> = None;

        'lifecycle: loop {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                break 'lifecycle;
            }
            if attempt == 0 {
                attempt = 1;
            }
            let _ = self.status_tx.send(if attempt == 1 && !has_connected_before && last_error.is_none() {
                ProtocolStatus::Connecting
            } else {
                ProtocolStatus::Reconnecting {
                    attempt,
                    last_error: last_error.clone(),
                }
            });

            let open_res = self.open_current(&reader_tx).await;
            match open_res {
                Ok(()) => {
                    if has_connected_before {
                        self.buffer
                            .feed_bytes(b"\r\n--- Serial port reconnected ---\r\n");
                    }
                    has_connected_before = true;
                    let _ = self.status_tx.send(ProtocolStatus::Connected);
                }
                Err(err) => {
                    let msg = err.to_string();
                    warn!(port = %self.config.port, error = %msg, "serial open failed");
                    if !self.current_modes().auto_reconnect {
                        let _ = self.status_tx.send(ProtocolStatus::Disconnected);
                        self.buffer.feed_bytes(
                            format!("\r\n--- Serial open failed: {msg} ---\r\n").as_bytes(),
                        );
                        if !self.wait_when_disconnected().await {
                            break 'lifecycle;
                        }
                        attempt = 0;
                        last_error = None;
                        continue 'lifecycle;
                    }

                    last_error = Some(msg.clone());
                    let _ = self.status_tx.send(ProtocolStatus::Reconnecting {
                        attempt,
                        last_error: Some(msg),
                    });
                    if !self.wait_reconnect_delay().await {
                        break 'lifecycle;
                    }
                    attempt = attempt.saturating_add(1);
                    continue 'lifecycle;
                }
            }

            // Connected message pump.
            loop {
                tokio::select! {
                    ev = reader_rx.recv() => {
                        match ev {
                            Some(ReaderEvent::Data { gen_id, bytes }) if gen_id == self.reader_gen => {
                                self.render_incoming(&bytes);
                            }
                            Some(ReaderEvent::Error { gen_id, reason }) if gen_id == self.reader_gen => {
                                self.teardown_stream();
                                debug!(port = %self.config.port, error = %reason, "serial stream lost");
                                self.buffer.feed_bytes(
                                    format!("\r\n--- Serial disconnected ({reason}) ---\r\n").as_bytes(),
                                );
                                if self.current_modes().auto_reconnect {
                                    attempt = 1;
                                    last_error = Some(reason.clone());
                                    let _ = self.status_tx.send(ProtocolStatus::Reconnecting {
                                        attempt,
                                        last_error: Some(reason),
                                    });
                                    if !self.wait_reconnect_delay().await {
                                        break 'lifecycle;
                                    }
                                    continue 'lifecycle;
                                } else {
                                    let _ = self.status_tx.send(ProtocolStatus::Disconnected);
                                    if !self.wait_when_disconnected().await {
                                        break 'lifecycle;
                                    }
                                    attempt = 0;
                                    last_error = None;
                                    continue 'lifecycle;
                                }
                            }
                            Some(_) => {}
                            None => break 'lifecycle,
                        }
                    }
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(SerialCmd::UserInput(bytes)) => {
                                if let Err(reason) = self.handle_user_input(&bytes) {
                                    self.teardown_stream();
                                    self.buffer.feed_bytes(
                                        format!("\r\n--- Serial write failed ({reason}) ---\r\n").as_bytes(),
                                    );
                                    if self.current_modes().auto_reconnect {
                                        attempt = 1;
                                        last_error = Some(reason.clone());
                                        let _ = self.status_tx.send(ProtocolStatus::Reconnecting {
                                            attempt,
                                            last_error: Some(reason),
                                        });
                                        if !self.wait_reconnect_delay().await {
                                            break 'lifecycle;
                                        }
                                        continue 'lifecycle;
                                    } else {
                                        let _ = self.status_tx.send(ProtocolStatus::Disconnected);
                                        if !self.wait_when_disconnected().await {
                                            break 'lifecycle;
                                        }
                                        attempt = 0;
                                        last_error = None;
                                        continue 'lifecycle;
                                    }
                                }
                            }
                            Some(SerialCmd::RawWire(bytes)) => {
                                if let Err(reason) = self.write_to_stream(&bytes) {
                                    self.teardown_stream();
                                    if self.current_modes().auto_reconnect {
                                        attempt = 1;
                                        last_error = Some(reason.clone());
                                        let _ = self.status_tx.send(ProtocolStatus::Reconnecting {
                                            attempt,
                                            last_error: Some(reason),
                                        });
                                        if !self.wait_reconnect_delay().await {
                                            break 'lifecycle;
                                        }
                                        continue 'lifecycle;
                                    }
                                    let _ = self.status_tx.send(ProtocolStatus::Disconnected);
                                }
                            }
                            Some(SerialCmd::AutoReconnectDisabled | SerialCmd::AutoReconnectEnabled) => {}
                            Some(SerialCmd::ReconnectNow) => {
                                self.teardown_stream();
                                attempt = 0;
                                last_error = None;
                                continue 'lifecycle;
                            }
                            Some(SerialCmd::Disconnect) | None => {
                                break 'lifecycle;
                            }
                        }
                    }
                }
            }
        }

        self.teardown_stream();
        self.buffer.set_write_sink(None);
        let _ = self.status_tx.send(ProtocolStatus::Disconnected);
    }

    async fn open_current(
        &mut self,
        reader_tx: &mpsc::UnboundedSender<ReaderEvent>,
    ) -> Result<(), ProtocolError> {
        let opener = Arc::clone(&self.opener);
        let cfg = self.config.clone();
        let stream = tokio::task::spawn_blocking(move || opener.open(&cfg))
            .await
            .map_err(|err| ProtocolError::Serial(format!("serial open join failed: {err}")))??;

        self.reader_gen = self.reader_gen.saturating_add(1);
        let gen_id = self.reader_gen;
        let stop = Arc::new(AtomicBool::new(false));
        self.reader_stop = Some(Arc::clone(&stop));
        self.active_stream = Some(Arc::clone(&stream));

        let tx = reader_tx.clone();
        let shutdown = Arc::clone(&self.shutdown_flag);
        tokio::task::spawn_blocking(move || {
            let mut buf = [0_u8; 4096];
            while !stop.load(Ordering::Relaxed) && !shutdown.load(Ordering::Relaxed) {
                match stream.read_chunk(&mut buf) {
                    Ok(n) if n > 0 => {
                        if tx
                            .send(ReaderEvent::Data {
                                gen_id,
                                bytes: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {
                        let _ = tx.send(ReaderEvent::Error {
                            gen_id,
                            reason: "serial EOF".to_string(),
                        });
                        break;
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(err) => {
                        let _ = tx.send(ReaderEvent::Error {
                            gen_id,
                            reason: err.to_string(),
                        });
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    fn teardown_stream(&mut self) {
        if let Some(stop) = self.reader_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.active_stream = None;
    }

    fn render_incoming(&mut self, bytes: &[u8]) {
        let filtered = self.buffer.run_output_interceptor(bytes);
        let display_bytes = filtered.as_deref().unwrap_or(bytes);
        if display_bytes.is_empty() {
            self.buffer.notify_output();
            return;
        }
        let modes = self.current_modes();
        if modes.reset_hexdump {
            self.hexdump.reset();
        }
        match modes.output {
            SerialOutputMode::Text => {
                let converted = convert_incoming_newlines(display_bytes, modes.newline);
                self.buffer.feed_terminal_bytes(&converted);
            }
            SerialOutputMode::Hexdump => {
                let dump = self.hexdump.format_chunk(display_bytes);
                self.buffer.feed_terminal_bytes(dump.as_bytes());
            }
        }
    }

    fn handle_user_input(&mut self, bytes: &[u8]) -> Result<(), String> {
        let modes = self.current_modes();
        match modes.input {
            SerialInputMode::Text => {
                let converted = convert_outgoing_newlines(bytes, modes.newline);
                self.write_to_stream(&converted)
            }
            SerialInputMode::Readline => {
                let step = self
                    .readline
                    .lock()
                    .expect("serial readline mutex")
                    .feed_input(bytes);
                if !step.local_echo.is_empty() {
                    self.buffer.feed_bytes(&step.local_echo);
                }
                for line in step.submitted_lines {
                    let mut payload = line.into_bytes();
                    payload.push(b'\n');
                    let converted = convert_outgoing_newlines(&payload, modes.newline);
                    self.write_to_stream(&converted)?;
                }
                Ok(())
            }
            SerialInputMode::HexBytes => {
                let step = self
                    .readline
                    .lock()
                    .expect("serial readline mutex")
                    .feed_input(bytes);
                if !step.local_echo.is_empty() {
                    self.buffer.feed_bytes(&step.local_echo);
                }
                for line in step.submitted_lines {
                    match parse_hex_input(&line) {
                        Ok(parsed) if !parsed.is_empty() => {
                            self.write_to_stream(&parsed)?;
                        }
                        Ok(_) => {}
                        Err(err) => {
                            self.buffer
                                .feed_bytes(format!("[hex error: {err}]\r\n").as_bytes());
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn write_to_stream(&self, bytes: &[u8]) -> Result<(), String> {
        let Some(stream) = &self.active_stream else {
            return Err("serial port not connected".to_string());
        };
        stream.write_all_bytes(bytes).map_err(|e| e.to_string())?;
        stream.flush_bytes().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Waits for `reconnect_delay` while remaining responsive to commands. Returns `false` to exit.
    async fn wait_reconnect_delay(&mut self) -> bool {
        let delay = self.config.reconnect_delay.max(Duration::from_millis(10));
        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);

        loop {
            tokio::select! {
                _ = &mut timer => return true,
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(SerialCmd::ReconnectNow) => return true,
                        Some(SerialCmd::AutoReconnectDisabled) => {
                            if !self.current_modes().auto_reconnect {
                                let _ = self.status_tx.send(ProtocolStatus::Disconnected);
                                return self.wait_when_disconnected().await;
                            }
                        }
                        Some(SerialCmd::Disconnect) | None => return false,
                        Some(SerialCmd::AutoReconnectEnabled)
                        | Some(SerialCmd::UserInput(_))
                        | Some(SerialCmd::RawWire(_)) => {}
                    }
                }
            }
        }
    }

    /// Waits while manually/non-auto disconnected until `ReconnectNow` or `Disconnect`.
    async fn wait_when_disconnected(&mut self) -> bool {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                SerialCmd::ReconnectNow | SerialCmd::AutoReconnectEnabled => return true,
                SerialCmd::Disconnect => return false,
                SerialCmd::AutoReconnectDisabled
                | SerialCmd::UserInput(_)
                | SerialCmd::RawWire(_) => {}
            }
        }
        false
    }
}
