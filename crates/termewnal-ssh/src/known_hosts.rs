use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use directories::BaseDirs;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{HashAlg, PublicKey};
use sha1::Sha1;
use tracing::warn;

use crate::error::SshError;

/// Result of checking a server host key against `known_hosts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnownHostStatus {
    /// Server key matches a valid entry recorded in `known_hosts`.
    Trusted,
    /// Host/algorithm has no recorded entry yet and requires a TOFU user decision.
    Unknown {
        host: String,
        port: u16,
        key_type: String,
        fingerprint_sha256: String,
    },
    /// Recorded key (or `@revoked` marker) conflicts with the server's offered key.
    Mismatch {
        host: String,
        port: u16,
        line: usize,
        recorded_fingerprint_sha256: String,
        offered_fingerprint_sha256: String,
        key_type: String,
    },
}

/// Computes the OpenSSH `SHA256:...` fingerprint string for `pubkey`.
pub fn fingerprint_sha256(pubkey: &PublicKey) -> String {
    pubkey.fingerprint(HashAlg::Sha256).to_string()
}

/// Resolves the standard user `~/.ssh/known_hosts` path.
pub fn default_known_hosts_path() -> Result<PathBuf, SshError> {
    if let Some(base) = BaseDirs::new() {
        return Ok(base.home_dir().join(".ssh").join("known_hosts"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join(".ssh").join("known_hosts"));
    }
    Err(SshError::KnownHosts {
        path: PathBuf::from("~/.ssh/known_hosts"),
        reason: "unable to determine user home directory".to_string(),
    })
}

/// Verifies `pubkey` for `(host, port)` against the user's `~/.ssh/known_hosts` file.
pub fn verify_known_host(
    host: &str,
    port: u16,
    pubkey: &PublicKey,
) -> Result<KnownHostStatus, SshError> {
    let path = default_known_hosts_path()?;
    verify_known_host_path(host, port, pubkey, &path)
}

/// Records `pubkey` for `(host, port)` in the user's `~/.ssh/known_hosts` file.
pub fn record_known_host(host: &str, port: u16, pubkey: &PublicKey) -> Result<(), SshError> {
    let path = default_known_hosts_path()?;
    record_known_host_path(host, port, pubkey, &path)
}

/// Verifies `pubkey` for `(host, port)` against an explicit `known_hosts` file `path`.
pub fn verify_known_host_path(
    host: &str,
    port: u16,
    pubkey: &PublicKey,
    path: &Path,
) -> Result<KnownHostStatus, SshError> {
    let offered_fp = fingerprint_sha256(pubkey);
    let key_type = pubkey.algorithm().as_str().to_string();

    let raw = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(KnownHostStatus::Unknown {
                host: host.to_string(),
                port,
                key_type,
                fingerprint_sha256: offered_fp,
            });
        }
        Err(err) => {
            return Err(SshError::KnownHosts {
                path: path.to_path_buf(),
                reason: err.to_string(),
            });
        }
    };

    let mut matched_exact = false;
    let mut first_mismatch: Option<(usize, String)> = None;

    for (idx, raw_line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some(entry) = parse_known_hosts_line(trimmed, line_no, path) else {
            continue;
        };

        if entry.marker == Some(Marker::CertAuthority) {
            continue;
        }

        if !matches_host_patterns(host, port, entry.hosts) {
            continue;
        }

        if entry.marker == Some(Marker::Revoked) {
            if entry.pubkey == *pubkey {
                return Ok(KnownHostStatus::Mismatch {
                    host: host.to_string(),
                    port,
                    line: line_no,
                    recorded_fingerprint_sha256: fingerprint_sha256(&entry.pubkey),
                    offered_fingerprint_sha256: offered_fp,
                    key_type,
                });
            }
            continue;
        }

        if entry.pubkey == *pubkey {
            matched_exact = true;
        } else if entry.pubkey.algorithm() == pubkey.algorithm() && first_mismatch.is_none() {
            first_mismatch = Some((line_no, fingerprint_sha256(&entry.pubkey)));
        }
    }

    if matched_exact {
        return Ok(KnownHostStatus::Trusted);
    }

    if let Some((line, recorded_fp)) = first_mismatch {
        return Ok(KnownHostStatus::Mismatch {
            host: host.to_string(),
            port,
            line,
            recorded_fingerprint_sha256: recorded_fp,
            offered_fingerprint_sha256: offered_fp,
            key_type,
        });
    }

    Ok(KnownHostStatus::Unknown {
        host: host.to_string(),
        port,
        key_type,
        fingerprint_sha256: offered_fp,
    })
}

/// Appends `(host, port, pubkey)` to `path` if not already present, creating parent directories
/// and enforcing `0600` file permissions on Unix.
pub fn record_known_host_path(
    host: &str,
    port: u16,
    pubkey: &PublicKey,
    path: &Path,
) -> Result<(), SshError> {
    if matches!(
        verify_known_host_path(host, port, pubkey, path)?,
        KnownHostStatus::Trusted
    ) {
        return Ok(());
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| SshError::KnownHosts {
            path: parent.to_path_buf(),
            reason: err.to_string(),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    let openssh_key = pubkey.to_openssh().map_err(|err| SshError::KnownHosts {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    let host_token = canonical_host_token(host, port);

    let mut open_opts = OpenOptions::new();
    open_opts.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o600);
    }

    let mut file = open_opts.open(path).map_err(|err| SshError::KnownHosts {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    let file_len = file
        .metadata()
        .map_err(|err| SshError::KnownHosts {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?
        .len();

    let mut needs_leading_newline = false;
    if file_len > 0 {
        file.seek(SeekFrom::End(-1))
            .map_err(|err| SshError::KnownHosts {
                path: path.to_path_buf(),
                reason: err.to_string(),
            })?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)
            .map_err(|err| SshError::KnownHosts {
                path: path.to_path_buf(),
                reason: err.to_string(),
            })?;
        needs_leading_newline = last[0] != b'\n';
    }

    file.seek(SeekFrom::End(0))
        .map_err(|err| SshError::KnownHosts {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?;

    if needs_leading_newline {
        file.write_all(b"\n")?;
    }
    writeln!(file, "{host_token} {openssh_key}")?;
    file.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    CertAuthority,
    Revoked,
}

struct ParsedLine<'a> {
    marker: Option<Marker>,
    hosts: &'a str,
    pubkey: PublicKey,
}

fn parse_known_hosts_line<'a>(line: &'a str, line_no: usize, path: &Path) -> Option<ParsedLine<'a>> {
    let mut tokens = line.split_whitespace();
    let first = tokens.next()?;

    let (marker, hosts) = if let Some(marker_name) = first.strip_prefix('@') {
        let marker = match marker_name {
            "cert-authority" => Marker::CertAuthority,
            "revoked" => Marker::Revoked,
            _ => return None,
        };
        let hosts = tokens.next()?;
        (Some(marker), hosts)
    } else {
        (None, first)
    };

    let key_type = tokens.next()?;
    let key_b64 = tokens.next()?;

    let pubkey = match PublicKey::from_openssh(&format!("{key_type} {key_b64}")) {
        Ok(pk) => pk,
        Err(err) => {
            warn!(
                path = %path.display(),
                line = line_no,
                error = %err,
                "ignoring unparseable key entry in known_hosts"
            );
            return None;
        }
    };

    Some(ParsedLine {
        marker,
        hosts,
        pubkey,
    })
}

fn canonical_host_token(host: &str, port: u16) -> String {
    let normalized = host.trim().to_ascii_lowercase();
    if port == 22 && !normalized.contains(':') {
        normalized
    } else {
        format!("[{normalized}]:{port}")
    }
}

fn matches_host_patterns(host: &str, port: u16, patterns: &str) -> bool {
    let normalized_host = host.trim().to_ascii_lowercase();
    let bracketed = format!("[{normalized_host}]:{port}");
    let candidates: [&str; 2] = if port == 22 {
        [normalized_host.as_str(), bracketed.as_str()]
    } else {
        [bracketed.as_str(), bracketed.as_str()]
    };

    let mut positive_match = false;
    for raw_pattern in patterns.split(',') {
        let pat = raw_pattern.trim();
        if pat.is_empty() {
            continue;
        }

        if let Some(negated) = pat.strip_prefix('!') {
            if matches_single_pattern(&candidates, negated) {
                return false;
            }
        } else if matches_single_pattern(&candidates, pat) {
            positive_match = true;
        }
    }

    positive_match
}

fn matches_single_pattern(candidates: &[&str; 2], pattern: &str) -> bool {
    if pattern.starts_with("|1|") {
        return candidates
            .iter()
            .any(|candidate| matches_hashed_host(candidate, pattern));
    }
    let lowered = pattern.to_ascii_lowercase();
    candidates
        .iter()
        .any(|candidate| wildcard_match(&lowered, candidate))
}

fn matches_hashed_host(candidate: &str, hashed_entry: &str) -> bool {
    let Some(rest) = hashed_entry.strip_prefix("|1|") else {
        return false;
    };
    let mut parts = rest.split('|');
    let Some(salt_b64) = parts.next() else {
        return false;
    };
    let Some(hash_b64) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }

    let Ok(salt) = BASE64.decode(salt_b64.as_bytes()) else {
        return false;
    };
    let Ok(expected_hash) = BASE64.decode(hash_b64.as_bytes()) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(candidate.as_bytes());
    mac.verify_slice(&expected_hash).is_ok()
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}
