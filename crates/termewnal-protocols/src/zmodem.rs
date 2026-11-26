//! Pure-Rust Zmodem (`rz`/`sz`) stream auto-detector and full-duplex CRC-16/CRC-32 transfer state machine.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::ProtocolError;

pub const ZPAD: u8 = b'*';
pub const ZDLE: u8 = 0x18;
pub const ZBIN: u8 = b'A';
pub const ZHEX: u8 = b'B';
pub const ZBIN32: u8 = b'C';

pub const ZRQINIT: u8 = 0;
pub const ZRINIT: u8 = 1;
pub const ZSINIT: u8 = 2;
pub const ZACK: u8 = 3;
pub const ZFILE: u8 = 4;
pub const ZSKIP: u8 = 5;
pub const ZNAK: u8 = 6;
pub const ZABORT: u8 = 7;
pub const ZFIN: u8 = 8;
pub const ZRPOS: u8 = 9;
pub const ZDATA: u8 = 10;
pub const ZEOF: u8 = 11;
pub const ZFERR: u8 = 12;
pub const ZCAN: u8 = 16;

pub const ZCRCE: u8 = b'h';
pub const ZCRCG: u8 = b'i';
pub const ZCRCQ: u8 = b'j';
pub const ZCRCW: u8 = b'k';

/// `ZRINIT` capability flags (`p3` byte).
pub const CANFDX: u8 = 0x01;
pub const CANOVIO: u8 = 0x02;
pub const CANFC32: u8 = 0x20;

const XON: u8 = 0x11;
const DLE: u8 = 0x10;
const XOFF: u8 = 0x13;
const SEND_CHUNK_SIZE: usize = 4096;

/// Header encoding flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderEncoding {
    ZBin,
    ZBin32,
    ZHex,
}

/// Decoded 5-byte Zmodem frame header (`frame_type` + 4 positional/flag bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZmodemHeader {
    pub encoding: HeaderEncoding,
    pub frame_type: u8,
    pub data: [u8; 4],
}

impl ZmodemHeader {
    pub fn with_pos(encoding: HeaderEncoding, frame_type: u8, pos: u32) -> Self {
        Self {
            encoding,
            frame_type,
            data: pos.to_le_bytes(),
        }
    }

    pub fn pos(&self) -> u32 {
        u32::from_le_bytes(self.data)
    }

    /// Serializes this header (including `ZPAD`/`ZDLE`/prefix, payload, CRC, and trailer).
    pub fn encode(&self) -> Vec<u8> {
        let raw5 = [
            self.frame_type,
            self.data[0],
            self.data[1],
            self.data[2],
            self.data[3],
        ];
        match self.encoding {
            HeaderEncoding::ZBin => {
                let mut out = Vec::with_capacity(16);
                out.push(ZPAD);
                out.push(ZDLE);
                out.push(ZBIN);
                for b in raw5 {
                    push_zdle_escaped(&mut out, b, false);
                }
                let crc = crc16_xmodem(&raw5);
                for b in crc.to_be_bytes() {
                    push_zdle_escaped(&mut out, b, false);
                }
                out
            }
            HeaderEncoding::ZBin32 => {
                let mut out = Vec::with_capacity(20);
                out.push(ZPAD);
                out.push(ZDLE);
                out.push(ZBIN32);
                for b in raw5 {
                    push_zdle_escaped(&mut out, b, false);
                }
                let crc = crc32_ieee(&raw5);
                for b in crc.to_le_bytes() {
                    push_zdle_escaped(&mut out, b, false);
                }
                out
            }
            HeaderEncoding::ZHex => {
                let mut out = Vec::with_capacity(24);
                out.push(ZPAD);
                out.push(ZPAD);
                out.push(ZDLE);
                out.push(ZHEX);
                for b in raw5 {
                    push_hex_byte(&mut out, b);
                }
                let crc = crc16_xmodem(&raw5);
                for b in crc.to_be_bytes() {
                    push_hex_byte(&mut out, b);
                }
                out.extend_from_slice(b"\r\n");
                if !matches!(self.frame_type, ZFIN | ZACK) {
                    out.push(XON);
                }
                out
            }
        }
    }
}

/// Computes Zmodem CRC-16 (CCITT XMODEM, polynomial `0x1021`, init `0`).
pub fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Computes Zmodem CRC-32 (IEEE 802.3, reflected polynomial `0xEDB88320`).
pub fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn push_zdle_escaped(out: &mut Vec<u8>, byte: u8, prev_is_at: bool) {
    let needs_escape = match byte {
        ZDLE | DLE | 0x90 | XON | 0x91 | XOFF | 0x93 => true,
        b'\r' | 0x8d if prev_is_at => true,
        _ => false,
    };
    if needs_escape {
        out.push(ZDLE);
        out.push(byte ^ 0x40);
    } else {
        out.push(byte);
    }
}

fn push_hex_byte(out: &mut Vec<u8>, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(byte >> 4) as usize]);
    out.push(HEX[(byte & 0x0f) as usize]);
}

/// Encodes a Zmodem data/file-info subpacket with `ZDLE + frame_end` and CRC-16 or CRC-32.
pub fn encode_subpacket(payload: &[u8], frame_end: u8, use_crc32: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() * 2 + 8);
    let mut prev_is_at = false;
    for &b in payload {
        push_zdle_escaped(&mut out, b, prev_is_at);
        prev_is_at = (b & 0x7f) == b'@';
    }
    out.push(ZDLE);
    out.push(frame_end);

    if use_crc32 {
        let mut crc_input = Vec::with_capacity(payload.len() + 1);
        crc_input.extend_from_slice(payload);
        crc_input.push(frame_end);
        let crc = crc32_ieee(&crc_input);
        for b in crc.to_le_bytes() {
            push_zdle_escaped(&mut out, b, false);
        }
    } else {
        let mut crc_input = Vec::with_capacity(payload.len() + 1);
        crc_input.extend_from_slice(payload);
        crc_input.push(frame_end);
        let crc = crc16_xmodem(&crc_input);
        for b in crc.to_be_bytes() {
            push_zdle_escaped(&mut out, b, false);
        }
    }
    out
}

/// Builds the 8xCAN + 8xBS Zmodem abort sequence.
pub fn abort_sequence() -> Vec<u8> {
    let mut out = vec![ZDLE; 8];
    out.extend([0x08; 8]);
    out
}

/// Builds a `ZFILE` payload (`filename\0size ...`).
pub fn encode_zfile_info(file_name: &str, file_size: u64) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(file_name.as_bytes());
    info.push(0);
    info.extend_from_slice(format!("{file_size} 0 0 0 0 0").as_bytes());
    info.push(0);
    info
}

/// Parses a `ZFILE` payload (`filename\0size ...`), extracting file name and optional total byte count.
pub fn parse_zfile_info(payload: &[u8]) -> Result<(String, Option<u64>), ProtocolError> {
    let nul_pos = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    let raw_name = String::from_utf8_lossy(&payload[..nul_pos]).into_owned();
    let name = sanitize_remote_filename(&raw_name);
    let size = if nul_pos + 1 < payload.len() {
        let rest = &payload[nul_pos + 1..];
        let rest_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let meta = String::from_utf8_lossy(&rest[..rest_end]);
        meta.split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
    } else {
        None
    };
    Ok((name, size))
}

/// Sanitizes remote filenames to prevent path traversal (`..`, `/`, `\`).
pub fn sanitize_remote_filename(raw: &str) -> String {
    let normalized = raw.replace('\\', "/");
    let leaf = normalized
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| c == '\0' || c.is_control() || c.is_whitespace());
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        "zmodem_download.bin".to_string()
    } else {
        leaf.to_string()
    }
}

/// Picks a non-colliding local path inside `download_dir` (`name (1).ext`, `name (2).ext`, ...).
pub fn resolve_safe_download_path(download_dir: &Path, raw_remote_name: &str) -> PathBuf {
    let safe_name = sanitize_remote_filename(raw_remote_name);
    let initial = download_dir.join(&safe_name);
    if !initial.exists() {
        return initial;
    }

    let p = Path::new(&safe_name);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("zmodem_download");
    let ext = p.extension().and_then(|e| e.to_str());

    for n in 1..=10_000 {
        let candidate_name = match ext {
            Some(ext) if !ext.is_empty() => format!("{stem} ({n}).{ext}"),
            _ => format!("{stem} ({n})"),
        };
        let candidate = download_dir.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    download_dir.join(format!("{stem}_{}", std::process::id()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZmodemDirection {
    Send,
    Receive,
}

/// High-level transfer lifecycle events emitted by [`ZmodemEngine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZmodemEvent {
    /// Remote `rz` emitted `ZRINIT`, signaling readiness to receive files from user.
    RemoteReadyToReceive,
    /// Remote `sz` offered a file transfer (`ZFILE`) and receive began.
    TransferStarted {
        direction: ZmodemDirection,
        file_name: String,
        total_bytes: Option<u64>,
    },
    /// Chunk progress update during active send/receive.
    TransferProgress {
        direction: ZmodemDirection,
        file_name: String,
        transferred_bytes: u64,
        total_bytes: Option<u64>,
    },
    /// Individual file completed successfully.
    TransferCompleted {
        direction: ZmodemDirection,
        file_name: String,
        local_path: PathBuf,
        bytes: u64,
    },
    /// Whole Zmodem session (`ZFIN` handshake) finished and stream returned to terminal mode.
    SessionFinished,
    /// Protocol error or remote cancellation aborted the transfer.
    Error(String),
}

/// Result returned by [`ZmodemEngine::feed_wire`].
#[derive(Debug, Default)]
pub struct ZmodemStep {
    /// Normal non-Zmodem terminal bytes ready for `TerminalBuffer::feed_bytes`.
    pub terminal_output: Vec<u8>,
    /// Zmodem control/data bytes to transmit back over the wire.
    pub wire_output: Vec<u8>,
    /// Lifecycle and progress events generated during this step.
    pub events: Vec<ZmodemEvent>,
}

/// Incremental wire frame decoder extracting Zmodem headers and data subpackets cleanly across arbitrary TCP/serial chunks.
#[derive(Debug)]
struct ZmodemFrameReader {
    pending: Vec<u8>,
    reading_subpacket: bool,
    subpacket_crc32: bool,
    subpacket_decoded: Vec<u8>,
    subpacket_zdle: bool,
    collecting_crc: Option<SubpacketCrcPending>,
}

#[derive(Debug)]
struct SubpacketCrcPending {
    frame_end: u8,
    needed: usize,
    raw_crc: Vec<u8>,
    saw_zdle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DecodedFrame {
    Header {
        leading_terminal: Vec<u8>,
        header: ZmodemHeader,
    },
    Subpacket {
        frame_end: u8,
        payload: Vec<u8>,
    },
    Abort,
}

impl ZmodemFrameReader {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            reading_subpacket: false,
            subpacket_crc32: false,
            subpacket_decoded: Vec::new(),
            subpacket_zdle: false,
            collecting_crc: None,
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.reading_subpacket = false;
        self.subpacket_decoded.clear();
        self.subpacket_zdle = false;
        self.collecting_crc = None;
    }

    fn enter_subpacket_mode(&mut self, use_crc32: bool) {
        self.reading_subpacket = true;
        self.subpacket_crc32 = use_crc32;
        self.subpacket_decoded.clear();
        self.subpacket_zdle = false;
        self.collecting_crc = None;
    }

    fn exit_subpacket_mode(&mut self) {
        self.reading_subpacket = false;
        self.subpacket_decoded.clear();
        self.subpacket_zdle = false;
        self.collecting_crc = None;
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// Attempts to decode one frame or safely flush non-header prefix bytes when `idle`.
    fn next_frame(&mut self) -> Result<Option<DecodedFrame>, ProtocolError> {
        if let Some(can_pos) = find_cancel_sequence(&self.pending) {
            self.pending.drain(..can_pos + 5);
            self.exit_subpacket_mode();
            return Ok(Some(DecodedFrame::Abort));
        }

        if self.reading_subpacket {
            return self.decode_subpacket_step();
        }

        let mut search_from = 0;
        while let Some(rel_zpad) = self.pending[search_from..].iter().position(|&b| b == ZPAD) {
            let zpad_idx = search_from + rel_zpad;
            if zpad_idx + 1 >= self.pending.len() {
                return Ok(None);
            }

            let mut cursor = zpad_idx + 1;
            if self.pending[cursor] == ZPAD {
                cursor += 1;
                if cursor >= self.pending.len() {
                    return Ok(None);
                }
            }

            if self.pending[cursor] != ZDLE {
                search_from = zpad_idx + 1;
                continue;
            }
            if cursor + 1 >= self.pending.len() {
                return Ok(None);
            }

            let kind = self.pending[cursor + 1];
            let body_start = cursor + 2;
            match kind {
                ZBIN => {
                    if let Some((header, consumed_end)) =
                        try_parse_zbin_header(&self.pending, body_start, false)?
                    {
                        let leading_terminal = self.pending[..zpad_idx].to_vec();
                        self.pending.drain(..consumed_end);
                        return Ok(Some(DecodedFrame::Header {
                            leading_terminal,
                            header,
                        }));
                    }
                    return Ok(None);
                }
                ZBIN32 => {
                    if let Some((header, consumed_end)) =
                        try_parse_zbin_header(&self.pending, body_start, true)?
                    {
                        let leading_terminal = self.pending[..zpad_idx].to_vec();
                        self.pending.drain(..consumed_end);
                        return Ok(Some(DecodedFrame::Header {
                            leading_terminal,
                            header,
                        }));
                    }
                    return Ok(None);
                }
                ZHEX => {
                    if let Some((header, consumed_end)) =
                        try_parse_zhex_header(&self.pending, body_start)?
                    {
                        let leading_terminal = self.pending[..zpad_idx].to_vec();
                        self.pending.drain(..consumed_end);
                        return Ok(Some(DecodedFrame::Header {
                            leading_terminal,
                            header,
                        }));
                    }
                    return Ok(None);
                }
                _ => {
                    search_from = zpad_idx + 1;
                }
            }
        }
        Ok(None)
    }

    /// Flushes bytes proven NOT to begin a Zmodem header, holding back only a valid partial candidate suffix.
    fn take_safe_idle_prefix(&mut self) -> Vec<u8> {
        if self.reading_subpacket || self.pending.is_empty() {
            return Vec::new();
        }
        let keep_from = candidate_header_suffix_start(&self.pending);
        if keep_from == 0 {
            return Vec::new();
        }
        self.pending.drain(..keep_from).collect()
    }

    fn decode_subpacket_step(&mut self) -> Result<Option<DecodedFrame>, ProtocolError> {
        let mut consumed = 0;
        let len = self.pending.len();
        while consumed < len {
            let b = self.pending[consumed];
            consumed += 1;

            if let Some(crc_state) = self.collecting_crc.as_mut() {
                if crc_state.saw_zdle {
                    crc_state.saw_zdle = false;
                    crc_state.raw_crc.push(b ^ 0x40);
                } else if b == ZDLE {
                    crc_state.saw_zdle = true;
                } else {
                    crc_state.raw_crc.push(b);
                }

                if crc_state.raw_crc.len() == crc_state.needed {
                    let crc_done = self.collecting_crc.take().expect("crc_state");
                    let payload = std::mem::take(&mut self.subpacket_decoded);
                    self.pending.drain(..consumed);

                    if self.subpacket_crc32 {
                        let expected = u32::from_le_bytes([
                            crc_done.raw_crc[0],
                            crc_done.raw_crc[1],
                            crc_done.raw_crc[2],
                            crc_done.raw_crc[3],
                        ]);
                        let mut crc_in = Vec::with_capacity(payload.len() + 1);
                        crc_in.extend_from_slice(&payload);
                        crc_in.push(crc_done.frame_end);
                        let actual = crc32_ieee(&crc_in);
                        if actual != expected {
                            return Err(ProtocolError::Zmodem(format!(
                                "subpacket CRC-32 mismatch: expected {expected:#010x}, got {actual:#010x}"
                            )));
                        }
                    } else {
                        let expected =
                            u16::from_be_bytes([crc_done.raw_crc[0], crc_done.raw_crc[1]]);
                        let mut crc_in = Vec::with_capacity(payload.len() + 1);
                        crc_in.extend_from_slice(&payload);
                        crc_in.push(crc_done.frame_end);
                        let actual = crc16_xmodem(&crc_in);
                        if actual != expected {
                            return Err(ProtocolError::Zmodem(format!(
                                "subpacket CRC-16 mismatch: expected {expected:#06x}, got {actual:#06x}"
                            )));
                        }
                    }

                    // Caller exits subpacket mode if frame_end requires header next (`ZCRCE`/`ZCRCW`).
                    return Ok(Some(DecodedFrame::Subpacket {
                        frame_end: crc_done.frame_end,
                        payload,
                    }));
                }
                continue;
            }

            if self.subpacket_zdle {
                self.subpacket_zdle = false;
                if matches!(b, ZCRCE | ZCRCG | ZCRCQ | ZCRCW) {
                    self.collecting_crc = Some(SubpacketCrcPending {
                        frame_end: b,
                        needed: if self.subpacket_crc32 { 4 } else { 2 },
                        raw_crc: Vec::with_capacity(4),
                        saw_zdle: false,
                    });
                } else {
                    self.subpacket_decoded.push(b ^ 0x40);
                }
            } else if b == ZDLE {
                self.subpacket_zdle = true;
            } else {
                self.subpacket_decoded.push(b);
            }
        }
        self.pending.clear();
        Ok(None)
    }
}

fn find_cancel_sequence(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(5)
        .position(|w| w.iter().all(|&b| b == ZDLE))
}

fn candidate_header_suffix_start(buf: &[u8]) -> usize {
    for i in 0..buf.len() {
        if buf[i] != ZPAD {
            continue;
        }
        let next = i + 1;
        if next == buf.len() {
            return i;
        }
        let cursor = if buf[next] == ZPAD {
            if next + 1 == buf.len() {
                return i;
            }
            next + 1
        } else {
            next
        };
        if buf[cursor] == ZDLE {
            if cursor + 1 >= buf.len() || matches!(buf[cursor + 1], ZBIN | ZBIN32 | ZHEX) {
                return i;
            }
        }
    }
    buf.len()
}

fn try_parse_zbin_header(
    buf: &[u8],
    body_start: usize,
    use_crc32: bool,
) -> Result<Option<(ZmodemHeader, usize)>, ProtocolError> {
    let target_len = if use_crc32 { 9 } else { 7 }; // 5 header bytes + 4/2 CRC bytes
    let mut decoded = Vec::with_capacity(target_len);
    let mut idx = body_start;
    let mut saw_zdle = false;

    while idx < buf.len() && decoded.len() < target_len {
        let b = buf[idx];
        idx += 1;
        if saw_zdle {
            saw_zdle = false;
            decoded.push(b ^ 0x40);
        } else if b == ZDLE {
            saw_zdle = true;
        } else {
            decoded.push(b);
        }
    }

    if decoded.len() < target_len {
        return Ok(None);
    }

    let raw5: [u8; 5] = [decoded[0], decoded[1], decoded[2], decoded[3], decoded[4]];
    if use_crc32 {
        let expected = u32::from_le_bytes([decoded[5], decoded[6], decoded[7], decoded[8]]);
        let actual = crc32_ieee(&raw5);
        if actual != expected {
            return Err(ProtocolError::Zmodem(format!(
                "ZBIN32 header CRC mismatch: expected {expected:#010x}, got {actual:#010x}"
            )));
        }
    } else {
        let expected = u16::from_be_bytes([decoded[5], decoded[6]]);
        let actual = crc16_xmodem(&raw5);
        if actual != expected {
            return Err(ProtocolError::Zmodem(format!(
                "ZBIN header CRC mismatch: expected {expected:#06x}, got {actual:#06x}"
            )));
        }
    }

    Ok(Some((
        ZmodemHeader {
            encoding: if use_crc32 {
                HeaderEncoding::ZBin32
            } else {
                HeaderEncoding::ZBin
            },
            frame_type: raw5[0],
            data: [raw5[1], raw5[2], raw5[3], raw5[4]],
        },
        idx,
    )))
}

fn try_parse_zhex_header(
    buf: &[u8],
    body_start: usize,
) -> Result<Option<(ZmodemHeader, usize)>, ProtocolError> {
    // Expect 14 ASCII hex chars (5 bytes frame + 2 bytes CRC-16) + CR/LF (+ optional XON).
    if buf.len() < body_start + 14 {
        return Ok(None);
    }
    let hex_slice = &buf[body_start..body_start + 14];
    let hex_str = std::str::from_utf8(hex_slice)
        .map_err(|_| ProtocolError::Zmodem("non-UTF8 ZHEX header".to_string()))?;

    let mut decoded = [0_u8; 7];
    for i in 0..7 {
        decoded[i] = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16)
            .map_err(|_| ProtocolError::Zmodem(format!("invalid ZHEX digits `{hex_str}`")))?;
    }

    let raw5 = [decoded[0], decoded[1], decoded[2], decoded[3], decoded[4]];
    let expected = u16::from_be_bytes([decoded[5], decoded[6]]);
    let actual = crc16_xmodem(&raw5);
    if actual != expected {
        return Err(ProtocolError::Zmodem(format!(
            "ZHEX header CRC mismatch: expected {expected:#06x}, got {actual:#06x}"
        )));
    }

    // Consume trailing CR / LF / XON bytes cleanly if already present.
    let mut end = body_start + 14;
    while end < buf.len() && matches!(buf[end], b'\r' | b'\n' | XON) {
        end += 1;
    }

    Ok(Some((
        ZmodemHeader {
            encoding: HeaderEncoding::ZHex,
            frame_type: raw5[0],
            data: [raw5[1], raw5[2], raw5[3], raw5[4]],
        },
        end,
    )))
}

struct ActiveReceiveFile {
    file_name: String,
    local_path: PathBuf,
    total_bytes: Option<u64>,
    received_bytes: u64,
    writer: BufWriter<File>,
    expect_crc32: bool,
}

#[derive(Debug)]
struct ActiveSendFile {
    file_name: String,
    local_path: PathBuf,
    total_bytes: u64,
    sent_bytes: u64,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendPhase {
    AwaitZrinit,
    AwaitZrpos,
    AwaitZfin,
}

enum EngineMode {
    Idle,
    ReceivingAwaitFileHeader,
    ReceivingAwaitZsinitData,
    ReceivingAwaitZfileData,
    ReceivingAwaitZdataHeader(ActiveReceiveFile),
    ReceivingData(ActiveReceiveFile),
    Sending {
        phase: SendPhase,
        use_crc32: bool,
        queue: VecDeque<PathBuf>,
        current: Option<ActiveSendFile>,
    },
}

/// Sans-IO Zmodem (`rz`/`sz`) stream detector and transfer protocol state machine.
pub struct ZmodemEngine {
    download_dir: PathBuf,
    reader: ZmodemFrameReader,
    mode: EngineMode,
    pending_oo_bytes: usize,
}

impl ZmodemEngine {
    pub fn new(download_dir: impl Into<PathBuf>) -> Self {
        Self {
            download_dir: download_dir.into(),
            reader: ZmodemFrameReader::new(),
            mode: EngineMode::Idle,
            pending_oo_bytes: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.mode, EngineMode::Idle)
    }

    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    pub fn set_download_dir(&mut self, dir: impl Into<PathBuf>) {
        self.download_dir = dir.into();
    }

    /// Initiates an outbound `sz`-compatible file send session, returning the initial `ZRQINIT` bytes to write.
    pub fn start_send(&mut self, files: Vec<PathBuf>) -> Result<ZmodemStep, ProtocolError> {
        if files.is_empty() {
            return Err(ProtocolError::InvalidInput(
                "at least one file must be selected for Zmodem send".to_string(),
            ));
        }
        let queue: VecDeque<PathBuf> = files.into_iter().collect();
        self.reader.reset();
        self.pending_oo_bytes = 0;
        self.mode = EngineMode::Sending {
            phase: SendPhase::AwaitZrinit,
            use_crc32: true,
            queue,
            current: None,
        };
        let mut step = ZmodemStep::default();
        step.wire_output.extend_from_slice(&self.zrinit_or_zrqinit_header(ZRQINIT, 0));
        Ok(step)
    }

    /// Aborts any active transfer and returns bytes containing the `8xCAN + 8xBS` sequence.
    pub fn cancel(&mut self) -> ZmodemStep {
        self.reset_to_idle();
        ZmodemStep {
            terminal_output: Vec::new(),
            wire_output: abort_sequence(),
            events: vec![ZmodemEvent::Error("Zmodem transfer cancelled".to_string())],
        }
    }

    /// Feeds incoming bytes from PTY/SSH/Serial/Telnet stream and returns terminal passthrough, wire replies, and events.
    pub fn feed_wire(&mut self, incoming: &[u8]) -> Result<ZmodemStep, ProtocolError> {
        let mut step = ZmodemStep::default();
        if incoming.is_empty() {
            return Ok(step);
        }

        // Strip trailing "OO" sent at end of sender ZFIN cleanly without polluting terminal output.
        let slice = if matches!(self.mode, EngineMode::Idle) && self.pending_oo_bytes > 0 {
            let mut skip = 0;
            while skip < incoming.len()
                && self.pending_oo_bytes > 0
                && incoming[skip] == b'O'
            {
                skip += 1;
                self.pending_oo_bytes -= 1;
            }
            if skip > 0 && self.pending_oo_bytes > 0 && skip < incoming.len() {
                self.pending_oo_bytes = 0;
            }
            &incoming[skip..]
        } else {
            incoming
        };

        if slice.is_empty() {
            return Ok(step);
        }
        self.reader.push_bytes(slice);

        loop {
            let idle = matches!(self.mode, EngineMode::Idle);
            match self.reader.next_frame() {
                Ok(Some(DecodedFrame::Abort)) => {
                    self.reset_to_idle();
                    step.events
                        .push(ZmodemEvent::Error("remote peer aborted Zmodem transfer".to_string()));
                    break;
                }
                Ok(Some(DecodedFrame::Header {
                    leading_terminal,
                    header,
                })) => {
                    if idle && !leading_terminal.is_empty() {
                        step.terminal_output.extend_from_slice(&leading_terminal);
                    }
                    self.handle_header(header, &mut step)?;
                }
                Ok(Some(DecodedFrame::Subpacket { frame_end, payload })) => {
                    self.handle_subpacket(frame_end, payload, &mut step)?;
                }
                Ok(None) => {
                    if idle {
                        let safe = self.reader.take_safe_idle_prefix();
                        step.terminal_output.extend_from_slice(&safe);
                    }
                    break;
                }
                Err(err) => {
                    self.reset_to_idle();
                    step.wire_output.extend_from_slice(&abort_sequence());
                    step.events.push(ZmodemEvent::Error(err.to_string()));
                    return Err(err);
                }
            }
        }

        Ok(step)
    }

    fn reset_to_idle(&mut self) {
        self.reader.reset();
        self.mode = EngineMode::Idle;
    }

    fn zrinit_or_zrqinit_header(&self, frame_type: u8, capabilities: u8) -> Vec<u8> {
        let mut hdr = ZmodemHeader {
            encoding: HeaderEncoding::ZHex,
            frame_type,
            data: [0; 4],
        };
        if frame_type == ZRINIT {
            hdr.data[3] = capabilities;
        }
        hdr.encode()
    }

    fn send_zrinit(&self) -> Vec<u8> {
        self.zrinit_or_zrqinit_header(ZRINIT, CANFDX | CANOVIO | CANFC32)
    }

    fn handle_header(
        &mut self,
        header: ZmodemHeader,
        step: &mut ZmodemStep,
    ) -> Result<(), ProtocolError> {
        match &mut self.mode {
            EngineMode::Idle => match header.frame_type {
                ZRQINIT => {
                    self.mode = EngineMode::ReceivingAwaitFileHeader;
                    step.wire_output.extend_from_slice(&self.send_zrinit());
                }
                ZRINIT => {
                    step.events.push(ZmodemEvent::RemoteReadyToReceive);
                }
                _ => {}
            },
            EngineMode::ReceivingAwaitFileHeader => match header.frame_type {
                ZRQINIT => {
                    step.wire_output.extend_from_slice(&self.send_zrinit());
                }
                ZSINIT => {
                    let crc32 = header.encoding == HeaderEncoding::ZBin32;
                    self.mode = EngineMode::ReceivingAwaitZsinitData;
                    self.reader.enter_subpacket_mode(crc32);
                }
                ZFILE => {
                    let crc32 = header.encoding == HeaderEncoding::ZBin32;
                    self.mode = EngineMode::ReceivingAwaitZfileData;
                    self.reader.enter_subpacket_mode(crc32);
                }
                ZFIN => {
                    step.wire_output.extend_from_slice(
                        &ZmodemHeader::with_pos(HeaderEncoding::ZHex, ZFIN, 0).encode(),
                    );
                    self.pending_oo_bytes = 2;
                    self.reset_to_idle();
                    step.events.push(ZmodemEvent::SessionFinished);
                }
                ZABORT | ZCAN => {
                    self.reset_to_idle();
                    step.events
                        .push(ZmodemEvent::Error("remote cancelled receive".to_string()));
                }
                _ => {}
            },
            EngineMode::ReceivingAwaitZsinitData | EngineMode::ReceivingAwaitZfileData => {}
            EngineMode::ReceivingAwaitZdataHeader(_) => match header.frame_type {
                ZDATA => {
                    let crc32 = header.encoding == HeaderEncoding::ZBin32;
                    let owned = std::mem::replace(&mut self.mode, EngineMode::Idle);
                    if let EngineMode::ReceivingAwaitZdataHeader(mut recv) = owned {
                        recv.expect_crc32 = crc32;
                        self.reader.enter_subpacket_mode(crc32);
                        self.mode = EngineMode::ReceivingData(recv);
                    }
                }
                ZEOF => {
                    let finished = std::mem::replace(&mut self.mode, EngineMode::Idle);
                    if let EngineMode::ReceivingAwaitZdataHeader(mut f) = finished {
                        let _ = f.writer.flush();
                        step.events.push(ZmodemEvent::TransferCompleted {
                            direction: ZmodemDirection::Receive,
                            file_name: f.file_name,
                            local_path: f.local_path,
                            bytes: f.received_bytes,
                        });
                    }
                    self.mode = EngineMode::ReceivingAwaitFileHeader;
                    step.wire_output.extend_from_slice(&self.send_zrinit());
                }
                ZFIN => {
                    step.wire_output.extend_from_slice(
                        &ZmodemHeader::with_pos(HeaderEncoding::ZHex, ZFIN, 0).encode(),
                    );
                    self.pending_oo_bytes = 2;
                    self.reset_to_idle();
                    step.events.push(ZmodemEvent::SessionFinished);
                }
                _ => {}
            },
            EngineMode::ReceivingData(_) => {}
            EngineMode::Sending { .. } => {
                self.handle_send_header(header, step)?;
            }
        }
        Ok(())
    }

    fn handle_subpacket(
        &mut self,
        frame_end: u8,
        payload: Vec<u8>,
        step: &mut ZmodemStep,
    ) -> Result<(), ProtocolError> {
        let mode = std::mem::replace(&mut self.mode, EngineMode::Idle);
        match mode {
            EngineMode::ReceivingAwaitZsinitData => {
                self.reader.exit_subpacket_mode();
                step.wire_output.extend_from_slice(
                    &ZmodemHeader::with_pos(HeaderEncoding::ZHex, ZACK, 0).encode(),
                );
                self.mode = EngineMode::ReceivingAwaitFileHeader;
            }
            EngineMode::ReceivingAwaitZfileData => {
                let (file_name, total_bytes) = parse_zfile_info(&payload)?;
                fs::create_dir_all(&self.download_dir)?;
                let local_path = resolve_safe_download_path(&self.download_dir, &file_name);
                let file = File::create(&local_path)?;
                let writer = BufWriter::new(file);

                self.reader.exit_subpacket_mode();
                step.events.push(ZmodemEvent::TransferStarted {
                    direction: ZmodemDirection::Receive,
                    file_name: file_name.clone(),
                    total_bytes,
                });
                step.wire_output.extend_from_slice(
                    &ZmodemHeader::with_pos(HeaderEncoding::ZHex, ZRPOS, 0).encode(),
                );
                self.mode = EngineMode::ReceivingAwaitZdataHeader(ActiveReceiveFile {
                    file_name,
                    local_path,
                    total_bytes,
                    received_bytes: 0,
                    writer,
                    expect_crc32: false,
                });
            }
            EngineMode::ReceivingData(mut recv) => {
                recv.writer.write_all(&payload)?;
                recv.received_bytes += payload.len() as u64;
                step.events.push(ZmodemEvent::TransferProgress {
                    direction: ZmodemDirection::Receive,
                    file_name: recv.file_name.clone(),
                    transferred_bytes: recv.received_bytes,
                    total_bytes: recv.total_bytes,
                });

                if matches!(frame_end, ZCRCQ | ZCRCW) {
                    step.wire_output.extend_from_slice(
                        &ZmodemHeader::with_pos(
                            HeaderEncoding::ZHex,
                            ZACK,
                            recv.received_bytes as u32,
                        )
                        .encode(),
                    );
                }

                if matches!(frame_end, ZCRCE | ZCRCW) {
                    self.reader.exit_subpacket_mode();
                    self.mode = EngineMode::ReceivingAwaitZdataHeader(recv);
                } else {
                    self.mode = EngineMode::ReceivingData(recv);
                }
            }
            other => {
                self.mode = other;
            }
        }
        Ok(())
    }

    fn handle_send_header(
        &mut self,
        header: ZmodemHeader,
        step: &mut ZmodemStep,
    ) -> Result<(), ProtocolError> {
        let EngineMode::Sending {
            phase,
            use_crc32,
            queue,
            current,
        } = &mut self.mode
        else {
            return Ok(());
        };

        if matches!(header.frame_type, ZABORT | ZCAN | ZFERR) {
            self.reset_to_idle();
            step.events.push(ZmodemEvent::Error(format!(
                "receiver rejected transfer with frame {}",
                header.frame_type
            )));
            return Ok(());
        }

        match *phase {
            SendPhase::AwaitZrinit => {
                if header.frame_type == ZRINIT {
                    *use_crc32 = (header.data[3] & CANFC32) != 0;
                    if let Some(next_path) = queue.pop_front() {
                        let mut bytes = Vec::new();
                        File::open(&next_path)?.read_to_end(&mut bytes)?;
                        let file_name = next_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("upload.bin")
                            .to_string();
                        let total_bytes = bytes.len() as u64;

                        step.events.push(ZmodemEvent::TransferStarted {
                            direction: ZmodemDirection::Send,
                            file_name: file_name.clone(),
                            total_bytes: Some(total_bytes),
                        });

                        let enc = if *use_crc32 {
                            HeaderEncoding::ZBin32
                        } else {
                            HeaderEncoding::ZBin
                        };
                        let zfile_hdr = ZmodemHeader::with_pos(enc, ZFILE, 0).encode();
                        let zfile_sub = encode_subpacket(
                            &encode_zfile_info(&file_name, total_bytes),
                            ZCRCW,
                            *use_crc32,
                        );
                        step.wire_output.extend_from_slice(&zfile_hdr);
                        step.wire_output.extend_from_slice(&zfile_sub);

                        *current = Some(ActiveSendFile {
                            file_name,
                            local_path: next_path,
                            total_bytes,
                            sent_bytes: 0,
                            data: bytes,
                        });
                        *phase = SendPhase::AwaitZrpos;
                    } else {
                        step.wire_output.extend_from_slice(
                            &ZmodemHeader::with_pos(HeaderEncoding::ZHex, ZFIN, 0).encode(),
                        );
                        *phase = SendPhase::AwaitZfin;
                    }
                }
            }
            SendPhase::AwaitZrpos => {
                if header.frame_type == ZSKIP {
                    *current = None;
                    *phase = SendPhase::AwaitZrinit;
                    // Immediately offer next or finish.
                    let fake_zrinit = ZmodemHeader {
                        encoding: HeaderEncoding::ZHex,
                        frame_type: ZRINIT,
                        data: [0, 0, 0, if *use_crc32 { CANFC32 } else { 0 }],
                    };
                    return self.handle_send_header(fake_zrinit, step);
                }
                if header.frame_type == ZRPOS || header.frame_type == ZACK {
                    let offset = header.pos() as usize;
                    let crc32 = *use_crc32;
                    let enc = if crc32 {
                        HeaderEncoding::ZBin32
                    } else {
                        HeaderEncoding::ZBin
                    };
                    if let Some(active) = current.as_mut() {
                        let start = offset.min(active.data.len());
                        active.sent_bytes = start as u64;
                        step.wire_output.extend_from_slice(
                            &ZmodemHeader::with_pos(enc, ZDATA, start as u32).encode(),
                        );

                        let remaining = &active.data[start..];
                        if remaining.is_empty() {
                            step.wire_output.extend_from_slice(&encode_subpacket(
                                &[],
                                ZCRCE,
                                crc32,
                            ));
                        } else {
                            let chunks: Vec<&[u8]> = remaining.chunks(SEND_CHUNK_SIZE).collect();
                            let last_idx = chunks.len() - 1;
                            for (idx, chunk) in chunks.into_iter().enumerate() {
                                let marker = if idx == last_idx { ZCRCE } else { ZCRCG };
                                step.wire_output.extend_from_slice(&encode_subpacket(
                                    chunk, marker, crc32,
                                ));
                                active.sent_bytes += chunk.len() as u64;
                                step.events.push(ZmodemEvent::TransferProgress {
                                    direction: ZmodemDirection::Send,
                                    file_name: active.file_name.clone(),
                                    transferred_bytes: active.sent_bytes,
                                    total_bytes: Some(active.total_bytes),
                                });
                            }
                        }

                        step.wire_output.extend_from_slice(
                            &ZmodemHeader::with_pos(enc, ZEOF, active.total_bytes as u32).encode(),
                        );
                        step.events.push(ZmodemEvent::TransferCompleted {
                            direction: ZmodemDirection::Send,
                            file_name: active.file_name.clone(),
                            local_path: active.local_path.clone(),
                            bytes: active.total_bytes,
                        });
                        *current = None;
                        *phase = SendPhase::AwaitZrinit;
                    }
                }
            }
            SendPhase::AwaitZfin => {
                if header.frame_type == ZFIN {
                    step.wire_output.extend_from_slice(b"OO");
                    self.reset_to_idle();
                    step.events.push(ZmodemEvent::SessionFinished);
                }
            }
        }
        Ok(())
    }
}
