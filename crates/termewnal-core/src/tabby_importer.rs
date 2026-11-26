use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::collections::{QuickCommand, load_quick_commands, save_quick_commands};
use crate::config::ConfigPaths;
use crate::error::CoreError;
use crate::profile::{
    DockerRuntime, LoginScriptStep, NewlineMode, Profile, ProfileTarget, ProfilesConfig,
    SerialFlowControl, SerialInputMode, SerialOutputMode, SerialParity, SshAuthMethod,
    TunnelDirection, TunnelRule, load_profiles, save_profiles, unique_profile_name,
};
use crate::vault::{SecretStore, VaultError};

/// Errors produced while reading or migrating a Tabby `config.yaml` file.
#[derive(Debug, Error)]
pub enum TabbyImportError {
    /// Filesystem I/O failure reading `config.yaml`.
    #[error("failed to read Tabby config at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// YAML syntax or schema deserialization failure.
    #[error("failed to parse Tabby YAML{}: {message}", format_location(*.line, *.column))]
    Parse {
        line: Option<usize>,
        column: Option<usize>,
        message: String,
    },

    /// Persistence failure while saving migrated profiles to `profiles.toml`.
    #[error(transparent)]
    Core(#[from] CoreError),

    /// Secret storage failure while writing credentials into Termewnal's `Vault`.
    #[error(transparent)]
    Vault(#[from] VaultError),
}

fn format_location(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(" at line {l}, column {c}"),
        (Some(l), None) => format!(" at line {l}"),
        _ => String::new(),
    }
}

/// Encryption status detected for Tabby's embedded `vault` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabbyVaultStatus {
    /// No encrypted Tabby vault detected and no missing credentials.
    None,
    /// Tabby's encrypted vault holds locked credentials that cannot be decrypted automatically.
    Locked { reason: String },
    /// One or more imported SSH profiles require credentials not stored in plaintext.
    MissingCredentials,
}

/// Kind of secret requested for an imported Tabby SSH `Profile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabbyCredentialKind {
    Password,
    KeyPassphrase,
}

/// Credential prompt metadata for an imported Tabby SSH `Profile` whose secret was locked or absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabbyCredentialRequirement {
    pub profile_id: String,
    pub profile_name: String,
    pub target_display: String,
    pub secret_ref: String,
    pub kind: TabbyCredentialKind,
}

/// Parsed migration result produced from Tabby's `config.yaml` before writing `profiles.toml`.
#[derive(Debug, Clone)]
pub struct TabbyImportReport {
    pub profiles: Vec<Profile>,
    pub quick_commands: Vec<QuickCommand>,
    pub imported_ssh_count: usize,
    pub imported_local_count: usize,
    pub imported_serial_count: usize,
    pub imported_telnet_count: usize,
    pub imported_docker_count: usize,
    pub imported_tunnel_count: usize,
    pub skipped_unsupported_count: usize,
    pub vault_status: TabbyVaultStatus,
    pub missing_credentials: Vec<TabbyCredentialRequirement>,
    plaintext_secrets: Vec<(String, Zeroizing<String>)>,
}

/// Outcome of persisting migrated Tabby `Profile`s into `profiles.toml` (and optional `Vault`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabbyImportOutcome {
    pub updated_profiles: ProfilesConfig,
    pub added_count: usize,
    pub added_quick_commands_count: usize,
    pub skipped_duplicate_count: usize,
    pub imported_tunnel_count: usize,
    pub skipped_unsupported_count: usize,
    pub vault_status: TabbyVaultStatus,
    pub missing_credentials: Vec<TabbyCredentialRequirement>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TabbyRawConfig {
    profiles: Vec<TabbyRawProfile>,
    groups: Vec<TabbyRawGroup>,
    vault: Option<TabbyRawVault>,
    #[serde(alias = "quickCmds", alias = "quick-cmds")]
    quick_cmds: Option<noyalib::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TabbyRawQuickCmd {
    name: String,
    text: String,
    #[serde(alias = "command")]
    command: Option<String>,
    group: Option<String>,
    #[serde(alias = "appendCR", alias = "append_cr")]
    append_cr: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TabbyRawLoginScript {
    expect: String,
    send: String,
    is_regex: Option<bool>,
    #[serde(alias = "isRegExp")]
    is_regexp: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TabbyRawGroup {
    id: String,
    name: String,
    parent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TabbyRawVault {
    version: Option<u32>,
    contents: Option<noyalib::Value>,
    encrypt: Option<bool>,
    encrypted: Option<bool>,
    locked: Option<bool>,
    key: Option<noyalib::Value>,
    iv: Option<noyalib::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TabbyRawProfile {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    group: Option<String>,
    tags: Vec<String>,
    options: Option<noyalib::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TabbyPortValue {
    Num(u64),
    Text(String),
}

impl TabbyPortValue {
    fn as_u16(&self) -> Option<u16> {
        match self {
            Self::Num(n) => u16::try_from(*n).ok(),
            Self::Text(s) => s.trim().parse::<u16>().ok(),
        }
    }

    fn as_device_str(&self) -> Option<String> {
        match self {
            Self::Text(s) => non_empty_trimmed(s),
            Self::Num(n) => Some(n.to_string()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TabbyRawOptions {
    // Shared / SSH / Telnet / Serial port field (numeric for SSH/Telnet, device path for Serial)
    port: Option<TabbyPortValue>,
    // SSH & Telnet fields
    host: Option<String>,
    #[serde(alias = "username")]
    user: Option<String>,
    auth: Option<String>,
    password: Option<String>,
    passphrase: Option<String>,
    #[serde(alias = "private_keys")]
    private_keys: Vec<String>,
    #[serde(alias = "private_key")]
    private_key: Option<String>,
    #[serde(alias = "jump_host")]
    jump_host: Option<String>,
    #[serde(alias = "jump_hosts")]
    jump_hosts: Vec<String>,
    #[serde(alias = "forwarded_ports")]
    forwarded_ports: Vec<TabbyRawForwardedPort>,
    #[serde(alias = "agent_forward", alias = "agentForwarding")]
    agent_forward: Option<bool>,
    #[serde(alias = "forwardX11", alias = "forward_x11")]
    x11: Option<bool>,
    #[serde(alias = "proxy_command")]
    proxy_command: Option<String>,
    #[serde(alias = "socks_proxy_host")]
    socks_proxy_host: Option<String>,
    #[serde(alias = "socks_proxy_port")]
    socks_proxy_port: Option<u16>,
    #[serde(alias = "http_proxy_host")]
    http_proxy_host: Option<String>,
    #[serde(alias = "http_proxy_port")]
    http_proxy_port: Option<u16>,
    #[serde(alias = "login_scripts")]
    login_scripts: Vec<TabbyRawLoginScript>,
    // Serial fields
    #[serde(alias = "baud_rate")]
    baudrate: Option<u32>,
    #[serde(alias = "data_bits")]
    databits: Option<u8>,
    parity: Option<String>,
    #[serde(alias = "stop_bits")]
    stopbits: Option<u8>,
    rtscts: Option<bool>,
    #[serde(alias = "flow_control")]
    flow_control: Option<String>,
    #[serde(alias = "input_mode")]
    input_mode: Option<String>,
    #[serde(alias = "output_mode")]
    output_mode: Option<String>,
    #[serde(alias = "newline_mode")]
    newline_mode: Option<String>,
    #[serde(alias = "auto_reconnect")]
    auto_reconnect: Option<bool>,
    // Docker fields
    #[serde(alias = "container_id", alias = "containerId")]
    container: Option<String>,
    runtime: Option<String>,
    // Local shell fields
    command: Option<String>,
    args: Vec<String>,
    #[serde(alias = "workingDir", alias = "working_dir")]
    cwd: Option<PathBuf>,
    // Optional extended tags
    tags: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TabbyRawForwardedPort {
    #[serde(rename = "type")]
    kind: String,
    host: Option<String>,
    #[serde(deserialize_with = "deserialize_u16_or_zero")]
    port: u16,
    #[serde(
        alias = "targetHost",
        alias = "target_address",
        alias = "target_host"
    )]
    target_address: Option<String>,
    #[serde(alias = "target_port", deserialize_with = "deserialize_opt_u16")]
    target_port: Option<u16>,
    #[serde(alias = "name")]
    description: Option<String>,
    standalone: Option<bool>,
}

fn deserialize_opt_u16<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum PortOrStr {
        Num(u64),
        Text(String),
    }

    match Option::<PortOrStr>::deserialize(deserializer)? {
        None => Ok(None),
        Some(PortOrStr::Num(n)) => u16::try_from(n)
            .map(Some)
            .map_err(|_| serde::de::Error::custom("port out of u16 range")),
        Some(PortOrStr::Text(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<u16>()
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
        }
    }
}

fn deserialize_u16_or_zero<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(deserialize_opt_u16(deserializer)?.unwrap_or(0))
}

/// Parses a Tabby `config.yaml` string into Termewnal `Profile`s, groups/tags, and `TunnelRule`s,
/// ignoring Tabby plugin blocks and detecting locked Tabby vault state.
pub fn parse_tabby_config_str(
    yaml: &str,
    default_username: &str,
) -> Result<TabbyImportReport, TabbyImportError> {
    if yaml.trim().is_empty() {
        return Ok(TabbyImportReport {
            profiles: Vec::new(),
            quick_commands: Vec::new(),
            imported_ssh_count: 0,
            imported_local_count: 0,
            imported_serial_count: 0,
            imported_telnet_count: 0,
            imported_docker_count: 0,
            imported_tunnel_count: 0,
            skipped_unsupported_count: 0,
            vault_status: TabbyVaultStatus::None,
            missing_credentials: Vec::new(),
            plaintext_secrets: Vec::new(),
        });
    }

    let raw: TabbyRawConfig = noyalib::from_str(yaml).map_err(|err| {
        let loc = err.location();
        TabbyImportError::Parse {
            line: loc.map(|l| l.line()),
            column: loc.map(|l| l.column()),
            message: err.to_string(),
        }
    })?;

    let groups_by_id: HashMap<&str, &TabbyRawGroup> = raw
        .groups
        .iter()
        .filter(|g| !g.id.trim().is_empty())
        .map(|g| (g.id.trim(), g))
        .collect();

    let mut parsed_profiles: Vec<(&TabbyRawProfile, TabbyRawOptions)> = Vec::new();
    let mut skipped_unsupported_count = 0_usize;

    for raw_prof in &raw.profiles {
        let kind = raw_prof.kind.trim().to_ascii_lowercase();
        if !matches!(
            kind.as_str(),
            "ssh" | "local" | "serial" | "telnet" | "docker"
        ) {
            skipped_unsupported_count += 1;
            continue;
        }
        let opts = decode_profile_options(raw_prof.options.as_ref())?;
        parsed_profiles.push((raw_prof, opts));
    }

    let ssh_by_id: HashMap<&str, (&TabbyRawProfile, &TabbyRawOptions)> = parsed_profiles
        .iter()
        .filter(|(p, _)| p.kind.trim().eq_ignore_ascii_case("ssh"))
        .filter_map(|(p, opts)| {
            p.id.as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(|id| (id, (*p, opts)))
        })
        .collect();

    let vault_locked_reason = detect_locked_tabby_vault(raw.vault.as_ref(), &parsed_profiles);
    let vault_is_locked = vault_locked_reason.is_some();

    let fallback_user = non_empty_trimmed(default_username).unwrap_or_else(|| "root".to_string());

    let mut profiles = Vec::new();
    let mut imported_ssh_count = 0_usize;
    let mut imported_local_count = 0_usize;
    let mut imported_serial_count = 0_usize;
    let mut imported_telnet_count = 0_usize;
    let mut imported_docker_count = 0_usize;
    let mut imported_tunnel_count = 0_usize;
    let mut missing_credentials = Vec::new();
    let mut plaintext_secrets = Vec::new();

    for (raw_prof, opts) in &parsed_profiles {
        let kind = raw_prof.kind.trim().to_ascii_lowercase();
        let group = raw_prof
            .group
            .as_deref()
            .and_then(|g| resolve_group_name(g, &groups_by_id));
        let tags = collect_tags(&raw_prof.tags, &opts.tags);

        match kind.as_str() {
            "ssh" => {
                let Some(host) = opts.host.as_deref().and_then(non_empty_trimmed) else {
                    skipped_unsupported_count += 1;
                    continue;
                };
                let port = opts
                    .port
                    .as_ref()
                    .and_then(TabbyPortValue::as_u16)
                    .filter(|&p| p > 0)
                    .unwrap_or(22);
                let username = opts
                    .user
                    .as_deref()
                    .and_then(non_empty_trimmed)
                    .unwrap_or_else(|| fallback_user.clone());
                let name = non_empty_trimmed(&raw_prof.name).unwrap_or_else(|| host.clone());
                let profile_id = Uuid::new_v4().to_string();
                let secret_ref = format!("ssh/{profile_id}");

                let auth_ctx = SshAuthContext {
                    profile_id: &profile_id,
                    profile_name: &name,
                    target_display: format!("{username}@{host}:{port}"),
                    secret_ref: &secret_ref,
                    vault_is_locked,
                };
                let (auth, cred_req, plaintext) = classify_ssh_auth(opts, &auth_ctx);
                if let Some(req) = cred_req {
                    missing_credentials.push(req);
                }
                if let Some(secret) = plaintext {
                    plaintext_secrets.push((secret_ref, secret));
                }

                let proxy_jump = resolve_tabby_proxy_jumps(opts, &ssh_by_id);
                let tunnels = convert_tunnels(&opts.forwarded_ports);
                imported_tunnel_count += tunnels.len();
                imported_ssh_count += 1;

                let login_scripts = convert_login_scripts(&opts.login_scripts);

                let proxy = if let Some(ph) = opts
                    .socks_proxy_host
                    .as_deref()
                    .and_then(non_empty_trimmed)
                {
                    Some(crate::SshProxyConfig {
                        kind: crate::SshProxyKind::Socks5,
                        host: ph,
                        port: opts.socks_proxy_port.filter(|&p| p > 0).unwrap_or(1080),
                        username: None,
                        vault_secret_ref: None,
                    })
                } else {
                    opts.http_proxy_host
                        .as_deref()
                        .and_then(non_empty_trimmed)
                        .map(|ph| crate::SshProxyConfig {
                            kind: crate::SshProxyKind::HttpConnect,
                            host: ph,
                            port: opts.http_proxy_port.filter(|&p| p > 0).unwrap_or(8080),
                            username: None,
                            vault_secret_ref: None,
                        })
                };

                profiles.push(Profile {
                    id: profile_id,
                    name,
                    group,
                    tags,
                    target: ProfileTarget::Ssh {
                        host,
                        port,
                        username,
                        auth,
                        proxy_jump,
                        proxy_command: opts
                            .proxy_command
                            .as_deref()
                            .and_then(non_empty_trimmed),
                        proxy,
                        forward_agent: opts.agent_forward.unwrap_or(false),
                        forward_x11: opts.x11.unwrap_or(false),
                        login_scripts,
                        auto_connect_on_startup: false,
                        tmux_reattach: None,
                        keepalive_interval_secs: None,
                        keepalive_max_missed: None,
                        auto_attach_mux: false,
                        mux_attach_command: None,
                    },
                    tunnels,
                });
            }
            "local" => {
                let shell = opts.command.as_deref().and_then(non_empty_trimmed);
                let working_dir = opts
                    .cwd
                    .as_ref()
                    .filter(|p| !p.as_os_str().is_empty())
                    .cloned();
                let name = non_empty_trimmed(&raw_prof.name)
                    .or_else(|| shell.clone())
                    .unwrap_or_else(|| "Local Shell".to_string());

                imported_local_count += 1;
                profiles.push(Profile {
                    id: Uuid::new_v4().to_string(),
                    name,
                    group,
                    tags,
                    target: ProfileTarget::LocalShell {
                        shell,
                        working_dir,
                        args: opts.args.clone(),
                    },
                    tunnels: Vec::new(),
                });
            }
            "serial" => {
                let Some(port) = opts.port.as_ref().and_then(TabbyPortValue::as_device_str) else {
                    skipped_unsupported_count += 1;
                    continue;
                };
                let name = non_empty_trimmed(&raw_prof.name).unwrap_or_else(|| port.clone());
                imported_serial_count += 1;
                profiles.push(Profile {
                    id: Uuid::new_v4().to_string(),
                    name,
                    group,
                    tags,
                    target: ProfileTarget::Serial {
                        port,
                        baud_rate: opts.baudrate.filter(|&b| b > 0).unwrap_or(115_200),
                        data_bits: opts.databits.filter(|&d| (5..=8).contains(&d)).unwrap_or(8),
                        parity: parse_serial_parity(opts.parity.as_deref()),
                        stop_bits: opts.stopbits.filter(|&s| s == 1 || s == 2).unwrap_or(1),
                        flow_control: parse_serial_flow(
                            opts.flow_control.as_deref(),
                            opts.rtscts.unwrap_or(false),
                        ),
                        input_mode: parse_serial_input(opts.input_mode.as_deref()),
                        output_mode: parse_serial_output(opts.output_mode.as_deref()),
                        newline_mode: parse_newline_mode(opts.newline_mode.as_deref()),
                        auto_reconnect: opts.auto_reconnect.unwrap_or(true),
                    },
                    tunnels: Vec::new(),
                });
            }
            "telnet" => {
                let Some(host) = opts.host.as_deref().and_then(non_empty_trimmed) else {
                    skipped_unsupported_count += 1;
                    continue;
                };
                let port = opts
                    .port
                    .as_ref()
                    .and_then(TabbyPortValue::as_u16)
                    .filter(|&p| p > 0)
                    .unwrap_or(23);
                let name = non_empty_trimmed(&raw_prof.name).unwrap_or_else(|| host.clone());
                imported_telnet_count += 1;
                profiles.push(Profile {
                    id: Uuid::new_v4().to_string(),
                    name,
                    group,
                    tags,
                    target: ProfileTarget::Telnet {
                        host,
                        port,
                        auto_reconnect: opts.auto_reconnect.unwrap_or(false),
                        login_scripts: convert_login_scripts(&opts.login_scripts),
                    },
                    tunnels: Vec::new(),
                });
            }
            "docker" => {
                let Some(container) = opts.container.as_deref().and_then(non_empty_trimmed) else {
                    skipped_unsupported_count += 1;
                    continue;
                };
                let runtime = match opts
                    .runtime
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("podman") => DockerRuntime::Podman,
                    _ => DockerRuntime::Docker,
                };
                let name =
                    non_empty_trimmed(&raw_prof.name).unwrap_or_else(|| container.clone());
                imported_docker_count += 1;
                profiles.push(Profile {
                    id: Uuid::new_v4().to_string(),
                    name,
                    group,
                    tags,
                    target: ProfileTarget::Docker {
                        runtime,
                        container,
                        shell: opts.command.as_deref().and_then(non_empty_trimmed),
                        user: opts.user.as_deref().and_then(non_empty_trimmed),
                        working_dir: opts
                            .cwd
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .filter(|s| !s.trim().is_empty()),
                    },
                    tunnels: Vec::new(),
                });
            }
            _ => {
                skipped_unsupported_count += 1;
            }
        }
    }

    let quick_commands = extract_quick_commands(raw.quick_cmds.as_ref());

    let vault_status = if let Some(reason) = vault_locked_reason {
        TabbyVaultStatus::Locked { reason }
    } else if !missing_credentials.is_empty() {
        TabbyVaultStatus::MissingCredentials
    } else {
        TabbyVaultStatus::None
    };

    Ok(TabbyImportReport {
        profiles,
        quick_commands,
        imported_ssh_count,
        imported_local_count,
        imported_serial_count,
        imported_telnet_count,
        imported_docker_count,
        imported_tunnel_count,
        skipped_unsupported_count,
        vault_status,
        missing_credentials,
        plaintext_secrets,
    })
}

/// Reads and parses a Tabby `config.yaml` file from disk without mutating it.
pub fn import_tabby_config_file(
    path: &Path,
    default_username: &str,
) -> Result<TabbyImportReport, TabbyImportError> {
    let raw = fs::read_to_string(path).map_err(|source| TabbyImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_tabby_config_str(&raw, default_username)
}

/// Persists migrated Tabby `Profile`s atomically into `profiles.toml` and stores any extracted
/// plaintext secrets strictly inside `secret_store` (never into TOML).
pub fn import_tabby_into_profiles(
    report: TabbyImportReport,
    paths: &ConfigPaths,
    secret_store: Option<&dyn SecretStore>,
) -> Result<TabbyImportOutcome, TabbyImportError> {
    let mut cfg = load_profiles(paths)?;
    let mut added_count = 0_usize;
    let mut skipped_duplicate_count = 0_usize;
    let mut imported_tunnel_count = 0_usize;
    let mut skipped_ids: HashSet<String> = HashSet::new();
    let mut renamed_names: HashMap<String, String> = HashMap::new();

    for mut profile in report.profiles {
        let orig_id = profile.id.clone();
        if cfg
            .profiles
            .iter()
            .any(|e| profiles_match_ignoring_ids(e, &profile))
        {
            skipped_duplicate_count += 1;
            skipped_ids.insert(orig_id);
            continue;
        }

        profile.name = unique_profile_name(&cfg.profiles, &profile.name);
        renamed_names.insert(orig_id, profile.name.clone());
        imported_tunnel_count += profile.tunnels.len();
        cfg.profiles.push(profile);
        added_count += 1;
    }

    save_profiles(paths, &cfg)?;

    let mut added_quick_commands_count = 0_usize;
    if !report.quick_commands.is_empty() {
        let mut quick_cfg = load_quick_commands(paths)?;
        for cmd in report.quick_commands {
            let duplicate = quick_cfg
                .commands
                .iter()
                .any(|existing| existing.name == cmd.name && existing.command == cmd.command);
            if !duplicate {
                quick_cfg.commands.push(cmd);
                added_quick_commands_count += 1;
            }
        }
        if added_quick_commands_count > 0 {
            save_quick_commands(paths, &quick_cfg)?;
        }
    }

    let mut missing_credentials: Vec<TabbyCredentialRequirement> = report
        .missing_credentials
        .into_iter()
        .filter(|req| !skipped_ids.contains(&req.profile_id))
        .map(|mut req| {
            if let Some(updated_name) = renamed_names.get(&req.profile_id) {
                req.profile_name = updated_name.clone();
            }
            req
        })
        .collect();

    for (secret_ref, secret) in report.plaintext_secrets {
        let maybe_profile = cfg.profiles.iter().find(|p| {
            matches!(
                &p.target,
                ProfileTarget::Ssh {
                    auth: SshAuthMethod::Password { vault_secret_ref: Some(r) }
                        | SshAuthMethod::PublicKey { vault_secret_ref: Some(r), .. },
                    ..
                } if r == &secret_ref
            )
        });
        let Some(profile) = maybe_profile else {
            continue;
        };
        let stored = match secret_store {
            Some(store) => match store.set_secret(&secret_ref, secret.as_str()) {
                Ok(()) => true,
                Err(VaultError::Locked) => false,
                Err(err) => return Err(TabbyImportError::Vault(err)),
            },
            None => false,
        };
        if !stored {
            let (target_display, kind) = match &profile.target {
                ProfileTarget::Ssh {
                    host,
                    port,
                    username,
                    auth,
                    ..
                } => {
                    let kind = match auth {
                        SshAuthMethod::PublicKey { .. } => TabbyCredentialKind::KeyPassphrase,
                        _ => TabbyCredentialKind::Password,
                    };
                    (format!("{username}@{host}:{port}"), kind)
                }
                ProfileTarget::LocalShell { .. }
                | ProfileTarget::Serial { .. }
                | ProfileTarget::Telnet { .. }
                | ProfileTarget::Docker { .. } => {
                    (profile.name.clone(), TabbyCredentialKind::Password)
                }
            };
            missing_credentials.push(TabbyCredentialRequirement {
                profile_id: profile.id.clone(),
                profile_name: profile.name.clone(),
                target_display,
                secret_ref,
                kind,
            });
        }
    }

    let vault_status = match report.vault_status {
        TabbyVaultStatus::Locked { reason } => TabbyVaultStatus::Locked { reason },
        _ if !missing_credentials.is_empty() => TabbyVaultStatus::MissingCredentials,
        _ => TabbyVaultStatus::None,
    };

    Ok(TabbyImportOutcome {
        updated_profiles: cfg,
        added_count,
        added_quick_commands_count,
        skipped_duplicate_count,
        imported_tunnel_count,
        skipped_unsupported_count: report.skipped_unsupported_count,
        vault_status,
        missing_credentials,
    })
}

/// Reads Tabby `config.yaml` from `path` and persists migrated profiles into `paths` (`profiles.toml`).
pub fn import_tabby_config_file_into_profiles(
    path: &Path,
    paths: &ConfigPaths,
    default_username: &str,
    secret_store: Option<&dyn SecretStore>,
) -> Result<TabbyImportOutcome, TabbyImportError> {
    let report = import_tabby_config_file(path, default_username)?;
    import_tabby_into_profiles(report, paths, secret_store)
}

/// Returns the default OS location of Tabby's `config.yaml`, if user config directories resolve.
pub fn default_tabby_config_path() -> Option<PathBuf> {
    BaseDirs::new().map(|base| base.config_dir().join("tabby").join("config.yaml"))
}

fn decode_profile_options(value: Option<&noyalib::Value>) -> Result<TabbyRawOptions, TabbyImportError> {
    match value {
        None => Ok(TabbyRawOptions::default()),
        Some(noyalib::Value::Null) => Ok(TabbyRawOptions::default()),
        Some(v) => noyalib::from_value(v).map_err(|err| {
            let loc = err.location();
            TabbyImportError::Parse {
                line: loc.map(|l| l.line()),
                column: loc.map(|l| l.column()),
                message: err.to_string(),
            }
        }),
    }
}

fn vault_value_non_empty(value: Option<&noyalib::Value>) -> bool {
    match value {
        None | Some(noyalib::Value::Null) => false,
        Some(noyalib::Value::String(s)) => !s.trim().is_empty(),
        Some(noyalib::Value::Mapping(m)) => !m.is_empty(),
        Some(noyalib::Value::Sequence(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn detect_locked_tabby_vault(
    vault: Option<&TabbyRawVault>,
    profiles: &[(&TabbyRawProfile, TabbyRawOptions)],
) -> Option<String> {
    if let Some(v) = vault {
        if vault_value_non_empty(v.contents.as_ref())
            || v.encrypt == Some(true)
            || v.encrypted == Some(true)
            || v.locked == Some(true)
        {
            return Some(
                "Tabby's encrypted vault holds locked credentials that cannot be decrypted automatically."
                    .to_string(),
            );
        }
    }

    let has_encrypted_inline = profiles.iter().any(|(_, opts)| {
        opts.password
            .as_deref()
            .is_some_and(looks_like_encrypted_secret)
            || opts
                .passphrase
                .as_deref()
                .is_some_and(looks_like_encrypted_secret)
    });
    if has_encrypted_inline {
        Some("Tabby profile contains encrypted credential ciphertexts.".to_string())
    } else {
        None
    }
}

fn looks_like_encrypted_secret(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("enc:")
        || trimmed.starts_with("tabby-vault:")
        || (trimmed.starts_with('{') && trimmed.contains("ciphertext"))
}

struct SshAuthContext<'a> {
    profile_id: &'a str,
    profile_name: &'a str,
    target_display: String,
    secret_ref: &'a str,
    vault_is_locked: bool,
}

impl SshAuthContext<'_> {
    fn requirement(&self, kind: TabbyCredentialKind) -> TabbyCredentialRequirement {
        TabbyCredentialRequirement {
            profile_id: self.profile_id.to_string(),
            profile_name: self.profile_name.to_string(),
            target_display: self.target_display.clone(),
            secret_ref: self.secret_ref.to_string(),
            kind,
        }
    }
}

fn classify_public_key_auth(
    key_path: Option<PathBuf>,
    raw_passphrase: Option<Zeroizing<String>>,
    ctx: &SshAuthContext<'_>,
) -> (
    SshAuthMethod,
    Option<TabbyCredentialRequirement>,
    Option<Zeroizing<String>>,
) {
    if let Some(pass) = raw_passphrase
        .as_ref()
        .filter(|p| !ctx.vault_is_locked && !looks_like_encrypted_secret(p))
        .cloned()
    {
        return (
            SshAuthMethod::PublicKey {
                key_path,
                vault_secret_ref: Some(ctx.secret_ref.to_string()),
            },
            None,
            Some(pass),
        );
    }
    let encrypted_or_locked = ctx.vault_is_locked
        || raw_passphrase
            .as_deref()
            .is_some_and(|p| looks_like_encrypted_secret(p));
    let (vault_secret_ref, req) = if encrypted_or_locked {
        (
            Some(ctx.secret_ref.to_string()),
            Some(ctx.requirement(TabbyCredentialKind::KeyPassphrase)),
        )
    } else {
        (None, None)
    };
    (
        SshAuthMethod::PublicKey {
            key_path,
            vault_secret_ref,
        },
        req,
        None,
    )
}

fn classify_password_auth(
    raw_password: Option<Zeroizing<String>>,
    ctx: &SshAuthContext<'_>,
) -> (
    SshAuthMethod,
    Option<TabbyCredentialRequirement>,
    Option<Zeroizing<String>>,
) {
    let plaintext =
        raw_password.filter(|p| !ctx.vault_is_locked && !looks_like_encrypted_secret(p));
    let req = plaintext
        .is_none()
        .then(|| ctx.requirement(TabbyCredentialKind::Password));
    (
        SshAuthMethod::Password {
            vault_secret_ref: Some(ctx.secret_ref.to_string()),
        },
        req,
        plaintext,
    )
}

fn classify_ssh_auth(
    options: &TabbyRawOptions,
    ctx: &SshAuthContext<'_>,
) -> (
    SshAuthMethod,
    Option<TabbyCredentialRequirement>,
    Option<Zeroizing<String>>,
) {
    let auth_mode = options
        .auth
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    let first_key = options
        .private_keys
        .iter()
        .chain(options.private_key.iter())
        .find_map(|k| non_empty_trimmed(k))
        .map(PathBuf::from);

    let raw_password = options
        .password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Zeroizing::new(s.to_string()));

    let raw_passphrase = options
        .passphrase
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Zeroizing::new(s.to_string()));

    match auth_mode.as_deref() {
        Some("publickey" | "public-key" | "public_key") => {
            classify_public_key_auth(first_key, raw_passphrase, ctx)
        }
        Some("password") => classify_password_auth(raw_password, ctx),
        Some("agent") => (SshAuthMethod::Agent, None, None),
        Some("keyboardinteractive" | "keyboard-interactive" | "keyboard_interactive") => {
            (SshAuthMethod::KeyboardInteractive, None, None)
        }
        _ => {
            if first_key.is_some() {
                classify_public_key_auth(first_key, raw_passphrase, ctx)
            } else if raw_password.is_some() || ctx.vault_is_locked {
                classify_password_auth(raw_password, ctx)
            } else {
                (SshAuthMethod::Agent, None, None)
            }
        }
    }
}

fn resolve_group_name(
    raw_group: &str,
    groups_by_id: &HashMap<&str, &TabbyRawGroup>,
) -> Option<String> {
    let mut current = non_empty_trimmed(raw_group)?;
    if !groups_by_id.contains_key(current.as_str()) {
        return Some(current);
    }

    let mut parts = Vec::new();
    let mut visited = HashSet::new();
    while let Some(grp) = groups_by_id.get(current.as_str()) {
        if !visited.insert(current.clone()) {
            break;
        }
        if let Some(name) = non_empty_trimmed(&grp.name) {
            parts.push(name);
        }
        match grp.parent.as_deref().and_then(non_empty_trimmed) {
            Some(parent_id) => current = parent_id,
            None => break,
        }
    }
    parts.reverse();
    (!parts.is_empty()).then(|| parts.join(" / "))
}

fn collect_tags(profile_tags: &[String], option_tags: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for raw in profile_tags.iter().chain(option_tags.iter()) {
        if let Some(tag) = non_empty_trimmed(raw)
            && seen.insert(tag.clone())
        {
            out.push(tag);
        }
    }
    out
}

fn resolve_tabby_proxy_jumps(
    options: &TabbyRawOptions,
    ssh_by_id: &HashMap<&str, (&TabbyRawProfile, &TabbyRawOptions)>,
) -> Vec<String> {
    let mut hops = Vec::new();
    let mut visited = HashSet::new();
    let raw_specs: Vec<&str> = options
        .jump_host
        .iter()
        .map(String::as_str)
        .chain(options.jump_hosts.iter().map(String::as_str))
        .collect();

    for spec in raw_specs {
        collect_jump_spec(spec, ssh_by_id, &mut visited, &mut hops);
    }
    hops
}

fn collect_jump_spec(
    raw_spec: &str,
    ssh_by_id: &HashMap<&str, (&TabbyRawProfile, &TabbyRawOptions)>,
    visited: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    let trimmed = raw_spec.trim();
    if trimmed.is_empty() {
        return;
    }

    if let Some((_, jump_opts)) = ssh_by_id.get(trimmed) {
        if !visited.insert(trimmed.to_string()) {
            return;
        }
        if let Some(parent_jump) = jump_opts.jump_host.as_deref() {
            collect_jump_spec(parent_jump, ssh_by_id, visited, out);
        }
        for parent_jump in &jump_opts.jump_hosts {
            collect_jump_spec(parent_jump, ssh_by_id, visited, out);
        }

        if let Some(host) = jump_opts.host.as_deref().and_then(non_empty_trimmed) {
            let port = jump_opts
                .port
                .as_ref()
                .and_then(TabbyPortValue::as_u16)
                .filter(|&p| p > 0)
                .unwrap_or(22);
            let formatted = match jump_opts.user.as_deref().and_then(non_empty_trimmed) {
                Some(user) if port != 22 => format!("{user}@{host}:{port}"),
                Some(user) => format!("{user}@{host}"),
                None if port != 22 => format!("{host}:{port}"),
                None => host,
            };
            out.push(formatted);
        }
        return;
    }

    for part in trimmed.split(',') {
        if let Some(clean) = non_empty_trimmed(part) {
            out.push(clean);
        }
    }
}

fn convert_tunnels(ports: &[TabbyRawForwardedPort]) -> Vec<TunnelRule> {
    let mut rules = Vec::new();
    for entry in ports {
        if entry.port == 0 {
            continue;
        }
        let direction = match entry.kind.trim().to_ascii_lowercase().as_str() {
            "local" => TunnelDirection::Local,
            "remote" => TunnelDirection::Remote,
            "dynamic" | "socks" | "socks5" => TunnelDirection::Dynamic,
            _ => continue,
        };
        let bind_host = entry
            .host
            .as_deref()
            .and_then(non_empty_trimmed)
            .unwrap_or_else(|| "127.0.0.1".to_string());

        let (target_host, target_port) = match direction {
            TunnelDirection::Dynamic => (None, None),
            TunnelDirection::Local | TunnelDirection::Remote => (
                Some(
                    entry
                        .target_address
                        .as_deref()
                        .and_then(non_empty_trimmed)
                        .unwrap_or_else(|| "127.0.0.1".to_string()),
                ),
                entry.target_port.filter(|&p| p > 0),
            ),
        };

        let name = entry
            .description
            .as_deref()
            .and_then(non_empty_trimmed)
            .unwrap_or_else(|| format_default_tunnel_name(direction, &bind_host, entry.port));

        rules.push(TunnelRule {
            id: Uuid::new_v4().to_string(),
            name,
            direction,
            bind_host,
            bind_port: entry.port,
            target_host,
            target_port,
            standalone: entry.standalone.unwrap_or(false),
            allow_public_bind: false,
            hold_grace_secs: None,
        });
    }
    rules
}

fn format_default_tunnel_name(direction: TunnelDirection, bind_host: &str, bind_port: u16) -> String {
    let tag = match direction {
        TunnelDirection::Local => "Local",
        TunnelDirection::Remote => "Remote",
        TunnelDirection::Dynamic => "Dynamic",
    };
    format!("{tag} {bind_host}:{bind_port}")
}

fn profiles_match_ignoring_ids(a: &Profile, b: &Profile) -> bool {
    a.name == b.name
        && a.group == b.group
        && a.tags == b.tags
        && targets_match_ignoring_secret_refs(&a.target, &b.target)
        && a.tunnels.len() == b.tunnels.len()
        && a.tunnels.iter().zip(&b.tunnels).all(|(ta, tb)| {
            ta.name == tb.name
                && ta.direction == tb.direction
                && ta.bind_host == tb.bind_host
                && ta.bind_port == tb.bind_port
                && ta.target_host == tb.target_host
                && ta.target_port == tb.target_port
                && ta.standalone == tb.standalone
        })
}

fn targets_match_ignoring_secret_refs(a: &ProfileTarget, b: &ProfileTarget) -> bool {
    match (a, b) {
        (
            ProfileTarget::LocalShell {
                shell: s1,
                working_dir: w1,
                args: a1,
            },
            ProfileTarget::LocalShell {
                shell: s2,
                working_dir: w2,
                args: a2,
            },
        ) => s1 == s2 && w1 == w2 && a1 == a2,
        (
            ProfileTarget::Ssh {
                host: h1,
                port: p1,
                username: u1,
                auth: auth1,
                proxy_jump: pj1,
                proxy_command: pc1,
                proxy: px1,
                forward_agent: fa1,
                forward_x11: fx1,
                login_scripts: ls1,
                auto_connect_on_startup: ac1,
                tmux_reattach: tr1,
                keepalive_interval_secs: ki1,
                keepalive_max_missed: km1,
                auto_attach_mux: am1,
                mux_attach_command: mc1,
            },
            ProfileTarget::Ssh {
                host: h2,
                port: p2,
                username: u2,
                auth: auth2,
                proxy_jump: pj2,
                proxy_command: pc2,
                proxy: px2,
                forward_agent: fa2,
                forward_x11: fx2,
                login_scripts: ls2,
                auto_connect_on_startup: ac2,
                tmux_reattach: tr2,
                keepalive_interval_secs: ki2,
                keepalive_max_missed: km2,
                auto_attach_mux: am2,
                mux_attach_command: mc2,
            },
        ) => {
            h1 == h2
                && p1 == p2
                && u1 == u2
                && pj1 == pj2
                && pc1 == pc2
                && px1 == px2
                && fa1 == fa2
                && fx1 == fx2
                && ls1 == ls2
                && ac1 == ac2
                && tr1 == tr2
                && ki1 == ki2
                && km1 == km2
                && am1 == am2
                && mc1 == mc2
                && auth_methods_equivalent(auth1, auth2)
        }
        (
            ProfileTarget::Serial {
                port: p1,
                baud_rate: b1,
                data_bits: d1,
                parity: pa1,
                stop_bits: s1,
                flow_control: fc1,
                input_mode: im1,
                output_mode: om1,
                newline_mode: nm1,
                auto_reconnect: ar1,
            },
            ProfileTarget::Serial {
                port: p2,
                baud_rate: b2,
                data_bits: d2,
                parity: pa2,
                stop_bits: s2,
                flow_control: fc2,
                input_mode: im2,
                output_mode: om2,
                newline_mode: nm2,
                auto_reconnect: ar2,
            },
        ) => {
            p1 == p2
                && b1 == b2
                && d1 == d2
                && pa1 == pa2
                && s1 == s2
                && fc1 == fc2
                && im1 == im2
                && om1 == om2
                && nm1 == nm2
                && ar1 == ar2
        }
        (
            ProfileTarget::Telnet {
                host: h1,
                port: p1,
                auto_reconnect: ar1,
                login_scripts: ls1,
            },
            ProfileTarget::Telnet {
                host: h2,
                port: p2,
                auto_reconnect: ar2,
                login_scripts: ls2,
            },
        ) => h1 == h2 && p1 == p2 && ar1 == ar2 && ls1 == ls2,
        (
            ProfileTarget::Docker {
                runtime: r1,
                container: c1,
                shell: s1,
                user: u1,
                working_dir: w1,
            },
            ProfileTarget::Docker {
                runtime: r2,
                container: c2,
                shell: s2,
                user: u2,
                working_dir: w2,
            },
        ) => r1 == r2 && c1 == c2 && s1 == s2 && u1 == u2 && w1 == w2,
        _ => false,
    }
}

fn convert_login_scripts(raw: &[TabbyRawLoginScript]) -> Vec<LoginScriptStep> {
    raw.iter()
        .filter(|s| !s.expect.is_empty() || !s.send.is_empty())
        .map(|s| LoginScriptStep {
            expect: s.expect.clone(),
            send: s.send.clone(),
            is_regex: s.is_regex.or(s.is_regexp).unwrap_or(false),
            append_newline: true,
            delay_ms: 0,
            secret_ref: None,
        })
        .collect()
}

fn extract_quick_commands(value: Option<&noyalib::Value>) -> Vec<QuickCommand> {
    let Some(val) = value else {
        return Vec::new();
    };
    let raw_list: Vec<TabbyRawQuickCmd> = match val {
        noyalib::Value::Sequence(_) => noyalib::from_value(val).unwrap_or_default(),
        noyalib::Value::Mapping(map) => {
            if let Some(cmds) = map.get("commands") {
                noyalib::from_value(cmds).unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    raw_list
        .into_iter()
        .filter_map(|raw| {
            let cmd_text = non_empty_trimmed(&raw.text)
                .or_else(|| raw.command.as_deref().and_then(non_empty_trimmed))?;
            let name = non_empty_trimmed(&raw.name).unwrap_or_else(|| cmd_text.clone());
            let mut qc = QuickCommand::new(name, cmd_text);
            qc.group = raw.group.as_deref().and_then(non_empty_trimmed);
            qc.append_newline = raw.append_cr.unwrap_or(true);
            Some(qc)
        })
        .collect()
}

fn parse_serial_parity(raw: Option<&str>) -> SerialParity {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("odd") => SerialParity::Odd,
        Some("even") => SerialParity::Even,
        _ => SerialParity::None,
    }
}

fn parse_serial_flow(flow: Option<&str>, rtscts: bool) -> SerialFlowControl {
    if rtscts {
        return SerialFlowControl::Hardware;
    }
    match flow.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("rtscts" | "hardware" | "rts/cts") => SerialFlowControl::Hardware,
        Some("xonxoff" | "software" | "xon/xoff") => SerialFlowControl::Software,
        _ => SerialFlowControl::None,
    }
}

fn parse_serial_input(raw: Option<&str>) -> SerialInputMode {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("readline") => SerialInputMode::Readline,
        Some("hex" | "hex_bytes" | "hexbytes") => SerialInputMode::HexBytes,
        _ => SerialInputMode::Text,
    }
}

fn parse_serial_output(raw: Option<&str>) -> SerialOutputMode {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("hex" | "hexdump") => SerialOutputMode::Hexdump,
        _ => SerialOutputMode::Text,
    }
}

fn parse_newline_mode(raw: Option<&str>) -> NewlineMode {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("lf") => NewlineMode::Lf,
        Some("crlf") => NewlineMode::CrLf,
        Some("keep" | "raw") => NewlineMode::Keep,
        _ => NewlineMode::Cr,
    }
}

fn auth_methods_equivalent(a: &SshAuthMethod, b: &SshAuthMethod) -> bool {
    match (a, b) {
        (SshAuthMethod::Agent, SshAuthMethod::Agent) => true,
        (SshAuthMethod::KeyboardInteractive, SshAuthMethod::KeyboardInteractive) => true,
        (SshAuthMethod::Password { .. }, SshAuthMethod::Password { .. }) => true,
        (
            SshAuthMethod::PublicKey { key_path: k1, .. },
            SshAuthMethod::PublicKey { key_path: k2, .. },
        ) => k1 == k2,
        _ => false,
    }
}

fn non_empty_trimmed(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
