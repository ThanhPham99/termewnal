//! Outbound SOCKS5 (RFC 1928/1929) and HTTP `CONNECT` proxy stream dialers for SSH transports.

use std::net::IpAddr;
use std::time::Duration;

use data_encoding::BASE64;
use termewnal_core::{SshProxyConfig, SshProxyKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zeroize::Zeroizing;

use crate::client::{TcpKeepaliveSettings, apply_tcp_keepalive};
use crate::error::SshError;

const PROXY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;

/// Opens a `TcpStream` to `target_host:target_port` tunneled through `proxy`, applying TCP
/// `nodelay` and optional keepalive settings, positioned at byte `0` of the SSH server stream.
pub async fn dial_via_proxy(
    proxy: &SshProxyConfig,
    proxy_password: Option<&str>,
    target_host: &str,
    target_port: u16,
    nodelay: bool,
    keepalive: Option<TcpKeepaliveSettings>,
) -> Result<TcpStream, SshError> {
    let proxy_host = proxy.host.trim();
    if proxy_host.is_empty() || proxy.port == 0 {
        return Err(SshError::Connection {
            host: target_host.to_string(),
            port: target_port,
            reason: "proxy host and non-zero port are required".to_string(),
        });
    }

    let mut stream = TcpStream::connect((proxy_host, proxy.port))
        .await
        .map_err(|err| SshError::Connection {
            host: target_host.to_string(),
            port: target_port,
            reason: format!("failed to connect to proxy {proxy_host}:{}: {err}", proxy.port),
        })?;
    let _ = stream.set_nodelay(nodelay);
    if let Some(ka) = keepalive
        && !ka.interval.is_zero()
    {
        apply_tcp_keepalive(&stream, ka);
    }

    let proxy_user = proxy
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let proxy_pass = proxy_password.filter(|s| !s.is_empty());

    let handshake = async {
        match proxy.kind {
            SshProxyKind::Socks5 => {
                dial_socks5(
                    &mut stream,
                    target_host,
                    target_port,
                    proxy_user,
                    proxy_pass,
                )
                .await
            }
            SshProxyKind::HttpConnect => {
                dial_http_connect(
                    &mut stream,
                    target_host,
                    target_port,
                    proxy_user,
                    proxy_pass,
                )
                .await
            }
        }
    };

    match tokio::time::timeout(PROXY_HANDSHAKE_TIMEOUT, handshake).await {
        Ok(res) => res?,
        Err(_) => {
            return Err(SshError::Connection {
                host: target_host.to_string(),
                port: target_port,
                reason: format!(
                    "timed out waiting for proxy handshake with {proxy_host}:{}",
                    proxy.port
                ),
            });
        }
    }

    Ok(stream)
}

async fn dial_socks5(
    stream: &mut TcpStream,
    target_host: &str,
    target_port: u16,
    proxy_user: Option<&str>,
    proxy_pass: Option<&str>,
) -> Result<(), SshError> {
    let conn_err = |reason: String| SshError::Connection {
        host: target_host.to_string(),
        port: target_port,
        reason,
    };

    let has_creds = proxy_user.is_some() || proxy_pass.is_some();
    let greeting: &[u8] = if has_creds {
        &[0x05, 0x02, 0x00, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    stream
        .write_all(greeting)
        .await
        .map_err(|e| conn_err(format!("SOCKS5 greeting write failed: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| conn_err(format!("SOCKS5 greeting flush failed: {e}")))?;

    let mut method_sel = [0u8; 2];
    stream
        .read_exact(&mut method_sel)
        .await
        .map_err(|e| conn_err(format!("SOCKS5 greeting reply failed: {e}")))?;
    if method_sel[0] != 0x05 {
        return Err(conn_err(format!(
            "unexpected SOCKS version 0x{:02x} in reply",
            method_sel[0]
        )));
    }

    match method_sel[1] {
        0x00 => {}
        0x02 => {
            let user = proxy_user.unwrap_or("");
            let pass = proxy_pass.unwrap_or("");
            if !has_creds {
                return Err(conn_err(
                    "SOCKS5 proxy requires username/password authentication".to_string(),
                ));
            }
            if user.len() > 255 || pass.len() > 255 {
                return Err(conn_err(
                    "SOCKS5 proxy username or password exceeds 255 bytes".to_string(),
                ));
            }
            let mut auth_buf = Zeroizing::new(Vec::with_capacity(3 + user.len() + pass.len()));
            auth_buf.push(0x01);
            auth_buf.push(user.len() as u8);
            auth_buf.extend_from_slice(user.as_bytes());
            auth_buf.push(pass.len() as u8);
            auth_buf.extend_from_slice(pass.as_bytes());
            stream
                .write_all(&auth_buf)
                .await
                .map_err(|e| conn_err(format!("SOCKS5 auth write failed: {e}")))?;
            stream
                .flush()
                .await
                .map_err(|e| conn_err(format!("SOCKS5 auth flush failed: {e}")))?;

            let mut auth_reply = [0u8; 2];
            stream
                .read_exact(&mut auth_reply)
                .await
                .map_err(|e| conn_err(format!("SOCKS5 auth reply failed: {e}")))?;
            if auth_reply[0] != 0x01 || auth_reply[1] != 0x00 {
                return Err(conn_err(
                    "SOCKS5 proxy rejected username/password authentication".to_string(),
                ));
            }
        }
        0xFF => {
            return Err(conn_err(
                "SOCKS5 proxy rejected all offered authentication methods".to_string(),
            ));
        }
        other => {
            return Err(conn_err(format!(
                "SOCKS5 proxy selected unsupported auth method 0x{other:02x}"
            )));
        }
    }

    let clean_host = strip_ipv6_brackets(target_host);
    if clean_host.is_empty() {
        return Err(conn_err("target host cannot be empty".to_string()));
    }

    let mut req = Vec::with_capacity(6 + clean_host.len());
    req.extend_from_slice(&[0x05, 0x01, 0x00]);
    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(v4) => {
                req.push(0x01);
                req.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                req.push(0x04);
                req.extend_from_slice(&v6.octets());
            }
        }
    } else {
        if clean_host.len() > 255 {
            return Err(conn_err(
                "target hostname exceeds 255 bytes for SOCKS5 CONNECT".to_string(),
            ));
        }
        req.push(0x03);
        req.push(clean_host.len() as u8);
        req.extend_from_slice(clean_host.as_bytes());
    }
    req.extend_from_slice(&target_port.to_be_bytes());

    stream
        .write_all(&req)
        .await
        .map_err(|e| conn_err(format!("SOCKS5 CONNECT write failed: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| conn_err(format!("SOCKS5 CONNECT flush failed: {e}")))?;

    let mut reply_hdr = [0u8; 4];
    stream
        .read_exact(&mut reply_hdr)
        .await
        .map_err(|e| conn_err(format!("SOCKS5 CONNECT reply failed: {e}")))?;
    if reply_hdr[0] != 0x05 {
        return Err(conn_err(format!(
            "unexpected SOCKS version 0x{:02x} in CONNECT reply",
            reply_hdr[0]
        )));
    }

    match reply_hdr[3] {
        0x01 => {
            let mut rest = [0u8; 6];
            stream
                .read_exact(&mut rest)
                .await
                .map_err(|e| conn_err(format!("SOCKS5 IPv4 bind read failed: {e}")))?;
        }
        0x04 => {
            let mut rest = [0u8; 18];
            stream
                .read_exact(&mut rest)
                .await
                .map_err(|e| conn_err(format!("SOCKS5 IPv6 bind read failed: {e}")))?;
        }
        0x03 => {
            let mut len_buf = [0u8; 1];
            stream
                .read_exact(&mut len_buf)
                .await
                .map_err(|e| conn_err(format!("SOCKS5 domain bind len read failed: {e}")))?;
            let mut rest = vec![0u8; len_buf[0] as usize + 2];
            stream
                .read_exact(&mut rest)
                .await
                .map_err(|e| conn_err(format!("SOCKS5 domain bind read failed: {e}")))?;
        }
        atyp => {
            if reply_hdr[1] == 0x00 {
                return Err(conn_err(format!(
                    "SOCKS5 CONNECT reply returned unknown address type 0x{atyp:02x}"
                )));
            }
        }
    }

    if reply_hdr[1] != 0x00 {
        return Err(conn_err(format!(
            "SOCKS5 CONNECT rejected ({})",
            socks5_reply_reason(reply_hdr[1])
        )));
    }

    Ok(())
}

async fn dial_http_connect(
    stream: &mut TcpStream,
    target_host: &str,
    target_port: u16,
    proxy_user: Option<&str>,
    proxy_pass: Option<&str>,
) -> Result<(), SshError> {
    let conn_err = |reason: String| SshError::Connection {
        host: target_host.to_string(),
        port: target_port,
        reason,
    };

    let clean_host = strip_ipv6_brackets(target_host);
    if clean_host.is_empty() || contains_header_control_chars(clean_host) {
        return Err(conn_err(
            "invalid target host for HTTP CONNECT request".to_string(),
        ));
    }

    let authority = if clean_host.contains(':') {
        format!("[{clean_host}]:{target_port}")
    } else {
        format!("{clean_host}:{target_port}")
    };

    let mut req = Zeroizing::new(format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
    ));
    if proxy_user.is_some() || proxy_pass.is_some() {
        let user = proxy_user.unwrap_or("");
        let pass = proxy_pass.unwrap_or("");
        if contains_header_control_chars(user) || contains_header_control_chars(pass) {
            return Err(conn_err(
                "HTTP proxy credentials cannot contain CR/LF/NUL characters".to_string(),
            ));
        }
        let creds = Zeroizing::new(format!("{user}:{pass}"));
        let encoded = Zeroizing::new(BASE64.encode(creds.as_bytes()));
        req.push_str("Proxy-Authorization: Basic ");
        req.push_str(&encoded);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");

    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| conn_err(format!("HTTP CONNECT write failed: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| conn_err(format!("HTTP CONNECT flush failed: {e}")))?;

    // Read strictly byte-by-byte until the blank header terminator so we never consume
    // any bytes of the remote SSH server's initial banner.
    let mut header_buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .await
            .map_err(|e| conn_err(format!("HTTP CONNECT response read failed: {e}")))?;
        if n == 0 {
            return Err(conn_err(
                "HTTP proxy closed connection before completing CONNECT headers".to_string(),
            ));
        }
        header_buf.push(byte[0]);
        if header_buf.ends_with(b"\r\n\r\n") || header_buf.ends_with(b"\n\n") {
            break;
        }
        if header_buf.len() >= MAX_HTTP_HEADER_BYTES {
            return Err(conn_err(
                "HTTP proxy CONNECT response header exceeded 16 KiB limit".to_string(),
            ));
        }
    }

    let header_str = String::from_utf8_lossy(&header_buf);
    let status_line = header_str
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| conn_err("HTTP proxy returned an empty CONNECT response".to_string()))?;

    let mut parts = status_line.split_whitespace();
    let http_ver = parts.next().unwrap_or("");
    let code = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if !http_ver.starts_with("HTTP/") || !(200..300).contains(&code) {
        return Err(conn_err(format!(
            "HTTP proxy CONNECT failed: {status_line}"
        )));
    }

    Ok(())
}

fn strip_ipv6_brackets(host: &str) -> &str {
    let trimmed = host.trim();
    trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed)
}

fn contains_header_control_chars(s: &str) -> bool {
    s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

fn socks5_reply_reason(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown SOCKS5 error",
    }
}
