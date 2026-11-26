//! Native protocol engines for Termewnal (`Serial`, `Telnet`, `Docker`, `Zmodem`).

pub mod docker;
pub mod error;
pub mod serial;
pub mod telnet;
pub mod zmodem;

pub use docker::{
    CommandOutputRunner, ContainerExecConfig, ContainerInfo, DockerDiscovery, OsCommandRunner,
    build_exec_shell_spec, parse_ps_output,
};
pub use error::ProtocolError;
pub use serial::{
    HexdumpFormatter, OsSerialPortOpener, SerialByteStream, SerialConnectConfig, SerialPortInfo,
    SerialPortOpener, SerialReadlineBuffer, SerialSession, convert_incoming_newlines,
    convert_outgoing_newlines, list_available_serial_ports, parse_hex_input,
};
pub use telnet::{
    ParsedTelnetChunk, TelnetConnectConfig, TelnetEvent, TelnetParser, TelnetSession,
    build_naws_frame, build_ttype_is_frame, escape_telnet_outgoing,
};
pub use zmodem::{
    ZmodemDirection, ZmodemEngine, ZmodemEvent, ZmodemHeader, ZmodemStep, abort_sequence,
    crc16_xmodem, crc32_ieee, resolve_safe_download_path, sanitize_remote_filename,
};

/// Connection lifecycle state shared across streaming protocol sessions (`Serial`, `Telnet`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolStatus {
    Connecting,
    Connected,
    Reconnecting {
        attempt: u32,
        last_error: Option<String>,
    },
    Disconnected,
}

impl ProtocolStatus {
    /// Short human-readable label for tab badges and Status Bar indicators.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Reconnecting { .. } => "Reconnecting",
            Self::Disconnected => "Disconnected",
        }
    }
}
