use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use termewnal_core::{
    NewlineMode, Profile, SerialFlowControl, SerialInputMode, SerialOutputMode, SerialParity,
};
use termewnal_protocols::{
    HexdumpFormatter, ProtocolError, ProtocolStatus, SerialByteStream, SerialConnectConfig,
    SerialPortInfo, SerialPortOpener, SerialReadlineBuffer, SerialSession,
    convert_incoming_newlines, convert_outgoing_newlines, list_available_serial_ports,
    parse_hex_input,
};
use termewnal_pty::TerminalBuffer;

#[derive(Default)]
struct VirtualSerialPort {
    rx_queue: Mutex<VecDeque<io::Result<Vec<u8>>>>,
    tx_log: Mutex<Vec<Vec<u8>>>,
    fail_writes: AtomicBool,
}

impl VirtualSerialPort {
    fn push_rx(&self, data: &[u8]) {
        self.rx_queue
            .lock()
            .unwrap()
            .push_back(Ok(data.to_vec()));
    }

    fn push_error(&self, kind: ErrorKind, msg: &str) {
        self.rx_queue
            .lock()
            .unwrap()
            .push_back(Err(io::Error::new(kind, msg)));
    }

    fn take_tx(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in self.tx_log.lock().unwrap().drain(..) {
            out.extend(chunk);
        }
        out
    }
}

impl SerialByteStream for VirtualSerialPort {
    fn read_chunk(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut queue = self.rx_queue.lock().unwrap();
        match queue.pop_front() {
            Some(Ok(chunk)) => {
                let len = chunk.len().min(buf.len());
                buf[..len].copy_from_slice(&chunk[..len]);
                if len < chunk.len() {
                    queue.push_front(Ok(chunk[len..].to_vec()));
                }
                Ok(len)
            }
            Some(Err(err)) => Err(err),
            None => {
                drop(queue);
                std::thread::sleep(Duration::from_millis(5));
                Err(io::Error::new(ErrorKind::WouldBlock, "no virtual RX data"))
            }
        }
    }

    fn write_all_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "simulated unplug"));
        }
        self.tx_log.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }

    fn flush_bytes(&self) -> io::Result<()> {
        Ok(())
    }
}

struct MockOpener {
    ports: Vec<SerialPortInfo>,
    opened_ports: Mutex<Vec<Arc<VirtualSerialPort>>>,
    open_calls: AtomicUsize,
    fail_next_opens: AtomicUsize,
}

impl MockOpener {
    fn new(ports: Vec<SerialPortInfo>) -> Self {
        Self {
            ports,
            opened_ports: Mutex::new(Vec::new()),
            open_calls: AtomicUsize::new(0),
            fail_next_opens: AtomicUsize::new(0),
        }
    }

    fn latest_port(&self) -> Option<Arc<VirtualSerialPort>> {
        self.opened_ports.lock().unwrap().last().cloned()
    }
}

impl SerialPortOpener for MockOpener {
    fn list_ports(&self) -> Result<Vec<SerialPortInfo>, ProtocolError> {
        Ok(self.ports.clone())
    }

    fn open(
        &self,
        config: &SerialConnectConfig,
    ) -> Result<Arc<dyn SerialByteStream>, ProtocolError> {
        self.open_calls.fetch_add(1, Ordering::SeqCst);
        let remain = self.fail_next_opens.load(Ordering::SeqCst);
        if remain > 0 {
            self.fail_next_opens.fetch_sub(1, Ordering::SeqCst);
            return Err(ProtocolError::Serial(format!(
                "device {} busy",
                config.port
            )));
        }
        let port = Arc::new(VirtualSerialPort::default());
        self.opened_ports.lock().unwrap().push(Arc::clone(&port));
        Ok(port)
    }
}

#[test]
fn hex_parser_newlines_and_hexdump_format_correctly() {
    assert_eq!(parse_hex_input("AA 55 0D").unwrap(), vec![0xAA, 0x55, 0x0D]);
    assert_eq!(
        parse_hex_input("0x01, 0x7f;dead:beef").unwrap(),
        vec![0x01, 0x7F, 0xDE, 0xAD, 0xBE, 0xEF]
    );
    assert!(parse_hex_input("ABC").is_err());
    assert!(parse_hex_input("ZZ").is_err());

    assert_eq!(
        convert_outgoing_newlines(b"AT\r\nOK\n", NewlineMode::Cr),
        b"AT\rOK\r"
    );
    assert_eq!(
        convert_outgoing_newlines(b"AT\r", NewlineMode::Lf),
        b"AT\n"
    );
    assert_eq!(
        convert_outgoing_newlines(b"AT\r", NewlineMode::CrLf),
        b"AT\r\n"
    );
    assert_eq!(
        convert_incoming_newlines(b"line1\nline2\r\nline3\r", NewlineMode::CrLf),
        b"line1\r\nline2\r\nline3\r\n"
    );

    let mut dump = HexdumpFormatter::new();
    let row1 = dump.format_chunk(b"Hello\x00\xff!");
    assert_eq!(dump.offset(), 8);
    assert!(row1.starts_with("00000000  48 65 6c 6c 6f 00 ff 21"));
    assert!(row1.ends_with("|Hello..!|\r\n"));
    let row2 = dump.format_chunk(b"AB");
    assert!(row2.starts_with("00000008  41 42"));
    assert_eq!(dump.offset(), 10);
}

#[test]
fn readline_buffer_edits_backspace_ctrl_c_and_history() {
    let mut rl = SerialReadlineBuffer::new();
    let s1 = rl.feed_input(b"ATZ\x7f\x7fI\r");
    assert_eq!(s1.submitted_lines, vec!["AI".to_string()]);
    assert!(s1.local_echo.ends_with(b"\r\n"));

    let s2 = rl.feed_input(b"ABORT\x03HELP\n");
    assert_eq!(s2.submitted_lines, vec!["HELP".to_string()]);

    // Recall previous (`HELP`) then earlier (`AI`).
    let s3 = rl.feed_input(b"\x1b[A\x1b[A\r");
    assert_eq!(s3.submitted_lines, vec!["AI".to_string()]);
}

#[test]
fn profile_serial_conversion_and_port_enumeration_do_not_panic() {
    let profile = Profile::serial("MCU UART", "/dev/ttyUSB0", 921_600);
    let mut cfg = SerialConnectConfig::from_profile_target(&profile.target).unwrap();
    assert_eq!(cfg.baud_rate, 921_600);
    assert_eq!(cfg.data_bits, 8);
    assert_eq!(cfg.parity, SerialParity::None);
    assert_eq!(cfg.flow_control, SerialFlowControl::None);

    // Ensure OS discovery runs cleanly even without physical adapters attached.
    let ports = list_available_serial_ports().unwrap();
    for p in &ports {
        assert!(!p.path.is_empty());
    }

    cfg.port = String::new();
    // Invalid empty port fails deterministically on OsSerialPortOpener.
    let opener = termewnal_protocols::OsSerialPortOpener;
    assert!(opener.open(&cfg).is_err());
}

#[tokio::test]
async fn serial_session_handles_modes_hexdump_and_auto_reconnect_preserving_scrollback() {
    let opener = Arc::new(MockOpener::new(vec![SerialPortInfo {
        path: "/dev/ttyV0".to_string(),
        display_name: "ttyV0".to_string(),
    }]));
    assert_eq!(opener.list_ports().unwrap().len(), 1);

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 2_000));
    let session = SerialSession::connect_with_opener(
        SerialConnectConfig {
            port: "/dev/ttyV0".to_string(),
            baud_rate: 115_200,
            input_mode: SerialInputMode::Readline,
            output_mode: SerialOutputMode::Text,
            newline_mode: NewlineMode::CrLf,
            auto_reconnect: true,
            reconnect_delay: Duration::from_millis(25),
            ..SerialConnectConfig::default()
        },
        Arc::clone(&buffer),
        opener.clone(),
    );

    let mut status_rx = session.subscribe_status();
    tokio::time::timeout(Duration::from_secs(2), async {
        while *status_rx.borrow() != ProtocolStatus::Connected {
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("serial session must reach Connected");

    // 1. Send Readline input and verify DSR query flows through TerminalBuffer write sink.
    session.write_bytes(b"AT+VER\r").unwrap();
    let port1 = opener.latest_port().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let tx = port1.take_tx();
        if !tx.is_empty() {
            assert_eq!(tx, b"AT+VER\r\n");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 2. Receive device text and check NewlineMode::CrLf rendering into TerminalBuffer.
    port1.push_rx(b"OK_V1\n");
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(2), |s| s.visible_text().contains("OK_V1"))
            .await
    );

    // 3. Switch to HexBytes input + Hexdump output live mid-session.
    session.set_input_mode(SerialInputMode::HexBytes);
    session.set_output_mode(SerialOutputMode::Hexdump);
    session.write_bytes(b"AA 55 0D\r").unwrap();
    port1.push_rx(b"\xAA\x55\r");

    let deadline_hex = Instant::now() + Duration::from_secs(2);
    let mut got_hex_tx = false;
    while Instant::now() < deadline_hex {
        let tx = port1.take_tx();
        if !tx.is_empty() {
            assert_eq!(tx, vec![0xAA, 0x55, 0x0D]);
            got_hex_tx = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(got_hex_tx, "HexBytes mode must transmit parsed binary bytes");
    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(2), |s| {
                s.visible_text().contains("aa 55 0d")
            })
            .await,
        "Hexdump output must render live hex rows: {}",
        buffer.export_scrollback_text()
    );

    // 4. Simulate USB unplug / BrokenPipe -> fails once, then auto-reconnects preserving scrollback!
    opener.fail_next_opens.store(1, Ordering::SeqCst);
    port1.push_error(ErrorKind::BrokenPipe, "adapter unplugged");

    let deadline_recon = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline_recon {
        if opener.open_calls.load(Ordering::SeqCst) >= 3
            && matches!(*status_rx.borrow(), ProtocolStatus::Connected)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert_eq!(*status_rx.borrow(), ProtocolStatus::Connected);

    let full_text = buffer.export_scrollback_text();
    assert!(full_text.contains("OK_V1"), "prior scrollback lost: {full_text}");
    assert!(
        full_text.contains("Serial port reconnected"),
        "expected reconnect divider in buffer: {full_text}"
    );

    session.disconnect();
}
