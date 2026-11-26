use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use ssh2_config::{ParseRule, SshConfig, SshParserError};
use thiserror::Error;
use uuid::Uuid;

use crate::config::ConfigPaths;
use crate::error::CoreError;
use crate::profile::{
    Profile, ProfileTarget, SshAuthMethod, load_profiles, save_profiles, unique_profile_name,
};

/// Concrete OpenSSH host entry parsed strictly read-only from `~/.ssh/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSshHost {
    pub alias: String,
    pub host_name: Option<String>,
    pub port: u16,
    pub user: Option<String>,
    pub identity_files: Vec<PathBuf>,
    pub proxy_jump: Vec<String>,
    pub forward_agent: Option<bool>,
    pub forward_x11: Option<bool>,
    pub proxy_command: Option<String>,
}

impl OpenSshHost {
    /// Returns the effective destination hostname (`HostName`, falling back to `alias`).
    pub fn resolved_host(&self) -> &str {
        self.host_name.as_deref().unwrap_or(&self.alias)
    }

    /// Returns the highest-precedence resolved `IdentityFile`, if any.
    pub fn primary_identity_file(&self) -> Option<&PathBuf> {
        self.identity_files.first()
    }
}

/// Errors produced while reading or parsing `~/.ssh/config`.
#[derive(Debug, Error)]
pub enum OpenSshConfigError {
    /// Filesystem I/O failure while reading `~/.ssh/config` or an included file.
    #[error("failed to read OpenSSH config at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Syntax or directive validation failure in `~/.ssh/config`.
    #[error("failed to parse OpenSSH config at {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

/// Parses an OpenSSH client config file strictly read-only, resolving `Include`s and inherited
/// directives (`Host *`, wildcard sections) for every concrete non-wildcard `Host` alias.
pub fn parse_openssh_config_file(
    config_path: &Path,
    home_dir: &Path,
) -> Result<Vec<OpenSshHost>, OpenSshConfigError> {
    let file = File::open(config_path).map_err(|source| OpenSshConfigError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::new(file);

    let parsed = SshConfig::default()
        .home_dir(home_dir)
        .parse(
            &mut reader,
            ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS,
        )
        .map_err(|err| map_parser_error(config_path, err))?;

    let mut seen_aliases = BTreeSet::new();
    let mut hosts = Vec::new();

    for host_entry in parsed.get_hosts() {
        for clause in &host_entry.pattern {
            if !is_concrete_alias(clause.negated, &clause.pattern) {
                continue;
            }
            let alias = clause.pattern.trim().to_string();
            if !seen_aliases.insert(alias.clone()) {
                continue;
            }

            let params = parsed.query(&alias);
            let identity_files = deduplicate_paths(params.identity_file.unwrap_or_default());
            let proxy_jump = clean_proxy_jump(params.proxy_jump);
            let forward_x11 = lookup_ssh_bool_field(
                &params.unsupported_fields,
                &params.ignored_fields,
                "forwardx11",
            );
            let proxy_command = lookup_ssh_string_field(
                &params.unsupported_fields,
                &params.ignored_fields,
                "proxycommand",
            )
            .filter(|cmd| !cmd.eq_ignore_ascii_case("none"));

            hosts.push(OpenSshHost {
                alias,
                host_name: params.host_name.and_then(|s| non_empty_trimmed(&s)),
                port: params.port.unwrap_or(22),
                user: params.user.and_then(|s| non_empty_trimmed(&s)),
                identity_files,
                proxy_jump,
                forward_agent: params.forward_agent,
                forward_x11,
                proxy_command,
            });
        }
    }

    Ok(hosts)
}

/// Parses `config_path` strictly read-only, returning `Ok(Vec::new())` if the file does not exist.
pub fn load_openssh_hosts_or_empty(
    config_path: &Path,
    home_dir: &Path,
) -> Result<Vec<OpenSshHost>, OpenSshConfigError> {
    match parse_openssh_config_file(config_path, home_dir) {
        Ok(hosts) => Ok(hosts),
        Err(OpenSshConfigError::Io { source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            Ok(Vec::new())
        }
        Err(err) => Err(err),
    }
}

/// Loads concrete System SSH hosts from `paths.ssh_config_file()` strictly read-only,
/// returning `Ok(Vec::new())` when no home directory or `~/.ssh/config` exists.
pub fn load_system_ssh_hosts(paths: &ConfigPaths) -> Result<Vec<OpenSshHost>, OpenSshConfigError> {
    let (Some(home_dir), Some(config_path)) = (paths.home_dir(), paths.ssh_config_file()) else {
        return Ok(Vec::new());
    };
    load_openssh_hosts_or_empty(&config_path, home_dir)
}

fn map_parser_error(config_path: &Path, err: SshParserError) -> OpenSshConfigError {
    match err {
        SshParserError::Io(source) => OpenSshConfigError::Io {
            path: config_path.to_path_buf(),
            source,
        },
        other => OpenSshConfigError::Parse {
            path: config_path.to_path_buf(),
            message: other.to_string(),
        },
    }
}

fn is_concrete_alias(negated: bool, pattern: &str) -> bool {
    let trimmed = pattern.trim();
    !negated
        && !trimmed.is_empty()
        && !trimmed.contains('*')
        && !trimmed.contains('?')
        && !trimmed.chars().any(char::is_whitespace)
}

fn non_empty_trimmed(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn deduplicate_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        if !path.as_os_str().is_empty() && seen.insert(path.clone()) {
            out.push(path);
        }
    }
    out
}

fn clean_proxy_jump(raw: Option<Vec<String>>) -> Vec<String> {
    let Some(items) = raw else {
        return Vec::new();
    };
    let cleaned: Vec<String> = items
        .into_iter()
        .filter_map(|item| non_empty_trimmed(&item))
        .filter(|item| !item.eq_ignore_ascii_case("none"))
        .collect();
    cleaned
}

fn lookup_ssh_string_field(
    unsupported: &std::collections::HashMap<String, Vec<String>>,
    ignored: &std::collections::HashMap<String, Vec<String>>,
    target_key_lower: &str,
) -> Option<String> {
    for map in [unsupported, ignored] {
        for (k, values) in map {
            if k.eq_ignore_ascii_case(target_key_lower) {
                let joined = values
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !joined.is_empty() {
                    return Some(joined);
                }
            }
        }
    }
    None
}

fn lookup_ssh_bool_field(
    unsupported: &std::collections::HashMap<String, Vec<String>>,
    ignored: &std::collections::HashMap<String, Vec<String>>,
    target_key_lower: &str,
) -> Option<bool> {
    let val = lookup_ssh_string_field(unsupported, ignored, target_key_lower)?;
    match val.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" | "1" | "on" => Some(true),
        "no" | "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

/// Converts a read-only [`OpenSshHost`] entry into a standalone editable [`Profile`].
///
/// Secrets are never read from or written to `~/.ssh/config`.
pub fn clone_openssh_host_to_profile(host: &OpenSshHost, default_username: &str) -> Profile {
    let username = host
        .user
        .as_deref()
        .and_then(non_empty_trimmed)
        .unwrap_or_else(|| {
            non_empty_trimmed(default_username).unwrap_or_else(|| "root".to_string())
        });

    let auth = match host.primary_identity_file() {
        Some(path) => SshAuthMethod::PublicKey {
            key_path: Some(path.clone()),
            vault_secret_ref: None,
        },
        None => SshAuthMethod::Agent,
    };

    Profile {
        id: Uuid::new_v4().to_string(),
        name: host.alias.clone(),
        group: None,
        tags: Vec::new(),
        target: ProfileTarget::Ssh {
            host: host.resolved_host().to_string(),
            port: host.port,
            username,
            auth,
            proxy_jump: host.proxy_jump.clone(),
            proxy_command: host.proxy_command.clone(),
            proxy: None,
            forward_agent: host.forward_agent.unwrap_or(false),
            forward_x11: host.forward_x11.unwrap_or(false),
            login_scripts: Vec::new(),
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
        },
        tunnels: Vec::new(),
    }
}

/// Clones `host` into a standalone editable [`Profile`] and appends it atomically to `profiles.toml`.
///
/// Never mutates `~/.ssh/config`.
pub fn save_cloned_openssh_profile(
    paths: &ConfigPaths,
    host: &OpenSshHost,
    default_username: &str,
) -> Result<Profile, CoreError> {
    let mut cfg = load_profiles(paths)?;
    let mut profile = clone_openssh_host_to_profile(host, default_username);
    profile.name = unique_profile_name(&cfg.profiles, &profile.name);
    cfg.profiles.push(profile.clone());
    save_profiles(paths, &cfg)?;
    Ok(profile)
}
