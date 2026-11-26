//! Local X11 display discovery, `.Xauthority` cookie parsing, and bidirectional X11 stream dialer.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use rand::RngExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

/// Local X11 display socket or TCP target derived from `DISPLAY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X11DisplayTarget {
    UnixSocket(PathBuf),
    Tcp { host: String, port: u16 },
}

/// Resolved X11 authentication and connection details sent in `x11-req`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X11AuthInfo {
    pub protocol: String,
    pub cookie_hex: String,
    pub screen: u32,
    pub display_num: u32,
    pub target: X11DisplayTarget,
}

/// Trait alias for bidirectional async X11 streams.
pub trait X11Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> X11Stream for T {}

/// Resolves X11 display target and MIT-MAGIC-COOKIE-1 credentials from `display_override` or `$DISPLAY`.
pub fn resolve_x11_auth(display_override: Option<&str>) -> Option<X11AuthInfo> {
    let raw_display = display_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| env::var("DISPLAY").ok().filter(|s| !s.trim().is_empty()))?;

    let (target, display_num, screen) = parse_display_spec(&raw_display)?;
    let (protocol, cookie_hex) = read_xauthority_cookie(display_num).unwrap_or_else(|| {
        let mut bytes = [0_u8; 16];
        rand::rng().fill(&mut bytes);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        ("MIT-MAGIC-COOKIE-1".to_string(), hex)
    });

    Some(X11AuthInfo {
        protocol,
        cookie_hex,
        screen,
        display_num,
        target,
    })
}

/// Parses standard `$DISPLAY` formats (`:0`, `:10.0`, `localhost:10.0`, `127.0.0.1:6010`, `/tmp/.X11-unix/X0`).
pub fn parse_display_spec(raw: &str) -> Option<(X11DisplayTarget, u32, u32)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('/') {
        let path = PathBuf::from(trimmed);
        let display_num = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.strip_prefix('X'))
            .and_then(|d| d.parse::<u32>().ok())
            .unwrap_or(0);
        return Some((X11DisplayTarget::UnixSocket(path), display_num, 0));
    }

    let (host_part, disp_part) = trimmed.rsplit_once(':')?;
    let (disp_str, screen_str) = match disp_part.split_once('.') {
        Some((d, s)) => (d, Some(s)),
        None => (disp_part, None),
    };
    let num: u32 = disp_str.parse().ok()?;
    let screen: u32 = screen_str
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    // Explicit raw TCP port override (e.g. `127.0.0.1:6010` for test harnesses).
    if !host_part.is_empty() && num >= 6000 {
        let port = u16::try_from(num).ok()?;
        return Some((
            X11DisplayTarget::Tcp {
                host: host_part.to_string(),
                port,
            },
            num - 6000,
            screen,
        ));
    }

    if host_part.is_empty() || host_part.eq_ignore_ascii_case("unix") {
        let sock = PathBuf::from(format!("/tmp/.X11-unix/X{num}"));
        if sock.exists() {
            return Some((X11DisplayTarget::UnixSocket(sock), num, screen));
        }
        return Some((
            X11DisplayTarget::Tcp {
                host: "127.0.0.1".to_string(),
                port: 6000_u16.saturating_add(num as u16),
            },
            num,
            screen,
        ));
    }

    Some((
        X11DisplayTarget::Tcp {
            host: host_part.to_string(),
            port: 6000_u16.saturating_add(num as u16),
        },
        num,
        screen,
    ))
}

/// Connects to the resolved local X11 display endpoint.
pub async fn connect_x11_target(target: &X11DisplayTarget) -> io::Result<Box<dyn X11Stream>> {
    match target {
        #[cfg(unix)]
        X11DisplayTarget::UnixSocket(path) => {
            let stream = UnixStream::connect(path).await?;
            Ok(Box::new(stream))
        }
        #[cfg(not(unix))]
        X11DisplayTarget::UnixSocket(path) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Unix domain X11 socket unsupported: {}", path.display()),
        )),
        X11DisplayTarget::Tcp { host, port } => {
            let stream = TcpStream::connect((host.as_str(), *port)).await?;
            Ok(Box::new(stream))
        }
    }
}

fn read_xauthority_cookie(display_num: u32) -> Option<(String, String)> {
    let path = env::var_os("XAUTHORITY")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|b| b.home_dir().join(".Xauthority")))?;
    parse_xauthority_file(&path, display_num)
}

fn parse_xauthority_file(path: &Path, display_num: u32) -> Option<(String, String)> {
    let bytes = fs::read(path).ok()?;
    let target_disp = display_num.to_string();
    let mut pos = 0_usize;
    let mut fallback: Option<(String, String)> = None;

    while pos + 2 <= bytes.len() {
        let _family = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;
        let _addr = read_counted_slice(&bytes, &mut pos)?;
        let disp = read_counted_slice(&bytes, &mut pos)?;
        let name = read_counted_slice(&bytes, &mut pos)?;
        let data = read_counted_slice(&bytes, &mut pos)?;

        let proto = String::from_utf8_lossy(name).into_owned();
        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
        if proto.is_empty() || hex.is_empty() {
            continue;
        }
        let disp_str = String::from_utf8_lossy(disp);
        if disp_str.trim() == target_disp {
            return Some((proto, hex));
        }
        if fallback.is_none() {
            fallback = Some((proto, hex));
        }
    }

    fallback
}

fn read_counted_slice<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    if *pos + 2 > bytes.len() {
        return None;
    }
    let len = u16::from_be_bytes([bytes[*pos], bytes[*pos + 1]]) as usize;
    *pos += 2;
    if *pos + len > bytes.len() {
        return None;
    }
    let slice = &bytes[*pos..*pos + len];
    *pos += len;
    Some(slice)
}
