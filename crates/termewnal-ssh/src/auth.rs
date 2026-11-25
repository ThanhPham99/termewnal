use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use directories::BaseDirs;
use russh::client::{self, AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, decode_secret_key};
use termewnal_core::{SshAuthMethod, vault::SecretStore};
use zeroize::Zeroizing;

use crate::error::SshError;

/// Boxed `Send` future alias used at async interaction and SSH agent boundaries.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Details presented to the user during an interactive Trust On First Use (TOFU) host key check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint_sha256: String,
}

/// Decision returned by the user for an unknown SSH host key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Trust this host key and append it to `known_hosts`.
    AcceptAndSave,
    /// Reject this host key and abort the connection without touching `known_hosts`.
    Reject,
}

/// Single prompt item inside a `keyboard-interactive` (2FA/OTP) server challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractivePromptItem {
    pub prompt: String,
    pub echo: bool,
}

/// Multi-prompt `keyboard-interactive` challenge received from the SSH server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardInteractiveChallenge {
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<InteractivePromptItem>,
}

/// Async UI/test callback boundary for TOFU host key verification and interactive authentication.
pub trait SshInteractionHandler: Send + Sync {
    /// Prompts the user to accept or reject an unknown SSH host key.
    fn verify_unknown_host(&self, prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision>;

    /// Prompts the user for a private key passphrase when the key is encrypted and not in `Vault`.
    fn prompt_key_passphrase(&self, key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>>;

    /// Prompts the user for an SSH login password when not stored in `Vault`.
    fn prompt_password(&self, username: &str, host: &str)
        -> BoxFuture<'_, Option<Zeroizing<String>>>;

    /// Prompts the user to answer a `keyboard-interactive` (e.g., 2FA/OTP) challenge.
    fn prompt_keyboard_interactive(
        &self,
        challenge: KeyboardInteractiveChallenge,
    ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>>;
}

/// Non-interactive [`SshInteractionHandler`] that rejects unknown host keys and interactive prompts.
#[derive(Debug, Default)]
pub struct RejectAllInteractionHandler;

impl SshInteractionHandler for RejectAllInteractionHandler {
    fn verify_unknown_host(&self, _prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async { HostKeyDecision::Reject })
    }

    fn prompt_key_passphrase(&self, _key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        Box::pin(async { None })
    }

    fn prompt_password(
        &self,
        _username: &str,
        _host: &str,
    ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        Box::pin(async { None })
    }

    fn prompt_keyboard_interactive(
        &self,
        _challenge: KeyboardInteractiveChallenge,
    ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
        Box::pin(async { None })
    }
}

/// Type-erased SSH agent client stream.
pub type BoxedAgentClient = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// Boundary for connecting to an OS or test SSH Agent (`SSH_AUTH_SOCK`, OpenSSH pipe, or Pageant).
pub trait AgentConnector: Send + Sync {
    /// Opens a fresh connection to the SSH Agent.
    fn connect(&self) -> BoxFuture<'_, Result<BoxedAgentClient, SshError>>;
}

/// Production [`AgentConnector`] supporting Unix `SSH_AUTH_SOCK` (or explicit socket path) and
/// Windows OpenSSH named pipe / Pageant.
#[derive(Debug, Clone, Default)]
pub struct OsAgentConnector {
    #[cfg(unix)]
    unix_socket_override: Option<PathBuf>,
}

impl OsAgentConnector {
    /// Creates an [`OsAgentConnector`] bound to an explicit Unix domain socket path (ideal for tests).
    #[cfg(unix)]
    pub fn from_unix_socket(path: impl Into<PathBuf>) -> Self {
        Self {
            unix_socket_override: Some(path.into()),
        }
    }
}

impl AgentConnector for OsAgentConnector {
    fn connect(&self) -> BoxFuture<'_, Result<BoxedAgentClient, SshError>> {
        #[cfg(unix)]
        {
            let override_path = self.unix_socket_override.clone();
            Box::pin(async move {
                let client = match override_path {
                    Some(path) => AgentClient::connect_uds(path).await,
                    None => AgentClient::connect_env().await,
                }
                .map_err(|err| SshError::Agent(err.to_string()))?;
                Ok(client.dynamic())
            })
        }

        #[cfg(windows)]
        {
            Box::pin(async move {
                match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
                    Ok(client) => Ok(client.dynamic()),
                    Err(pipe_err) => match AgentClient::connect_pageant().await {
                        Ok(client) => Ok(client.dynamic()),
                        Err(pageant_err) => Err(SshError::Agent(format!(
                            "OpenSSH pipe failed ({pipe_err}); Pageant failed ({pageant_err})"
                        ))),
                    },
                }
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            Box::pin(async move {
                Err(SshError::Agent(
                    "SSH agent is unsupported on this platform".to_string(),
                ))
            })
        }
    }
}

/// Summary of a completed SSH authentication handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthOutcome {
    /// `true` if interactive user input (2FA/OTP or interactive prompt) was required, meaning
    /// background `Transfer Transport` connections cannot replay authentication silently.
    pub used_interactive_auth: bool,
}

/// Authenticates an established [`client::Handle`] using the configured [`SshAuthMethod`].
pub async fn authenticate_session<H: client::Handler>(
    handle: &mut client::Handle<H>,
    host: &str,
    username: &str,
    auth: &SshAuthMethod,
    vault: Option<&dyn SecretStore>,
    interaction: &dyn SshInteractionHandler,
    agent_connector: &dyn AgentConnector,
) -> Result<AuthOutcome, SshError> {
    match auth {
        SshAuthMethod::PublicKey {
            key_path,
            vault_secret_ref,
        } => {
            authenticate_with_public_key(
                handle,
                username,
                key_path.as_deref(),
                vault_secret_ref.as_deref(),
                vault,
                interaction,
            )
            .await
        }
        SshAuthMethod::Agent => {
            authenticate_with_agent(handle, username, agent_connector).await
        }
        SshAuthMethod::Password { vault_secret_ref } => {
            authenticate_with_password(
                handle,
                host,
                username,
                vault_secret_ref.as_deref(),
                vault,
                interaction,
            )
            .await
        }
        SshAuthMethod::KeyboardInteractive => {
            authenticate_with_keyboard_interactive(handle, username, interaction).await
        }
    }
}

async fn authenticate_with_public_key<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    explicit_path: Option<&Path>,
    vault_secret_ref: Option<&str>,
    vault: Option<&dyn SecretStore>,
    interaction: &dyn SshInteractionHandler,
) -> Result<AuthOutcome, SshError> {
    let candidates = resolve_key_candidates(explicit_path)?;
    let mut last_error: Option<SshError> = None;
    let mut used_interactive = false;

    for path in candidates {
        let (key, interactive) = match load_private_key_with_vault(
            &path,
            vault_secret_ref,
            vault,
            interaction,
        )
        .await
        {
            Ok(loaded) => loaded,
            Err(err) => {
                last_error = Some(err);
                if explicit_path.is_some() {
                    break;
                }
                continue;
            }
        };
        used_interactive |= interactive;

        let key = Arc::new(key);
        let result = try_publickey_auth(handle, username, &key).await?;
        if result.success() {
            return Ok(AuthOutcome {
                used_interactive_auth: used_interactive,
            });
        }
        last_error = Some(SshError::AuthenticationFailed {
            username: username.to_string(),
            method: "publickey",
            detail: format!("server rejected public key {}", path.display()),
        });
    }

    Err(last_error.unwrap_or_else(|| SshError::AuthenticationFailed {
        username: username.to_string(),
        method: "publickey",
        detail: "no usable SSH private key found".to_string(),
    }))
}

async fn try_publickey_auth<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    key: &Arc<PrivateKey>,
) -> Result<AuthResult, SshError> {
    if !key.algorithm().is_rsa() {
        return Ok(handle
            .authenticate_publickey(username, PrivateKeyWithHashAlg::new(Arc::clone(key), None))
            .await?);
    }

    let advertised = handle.best_supported_rsa_hash().await?;
    let primary_hash = match advertised {
        Some(hash) => hash,
        None => Some(HashAlg::Sha256),
    };

    let res = handle
        .authenticate_publickey(
            username,
            PrivateKeyWithHashAlg::new(Arc::clone(key), primary_hash),
        )
        .await?;
    if !res.success() && advertised.is_none() && primary_hash.is_some() {
        return Ok(handle
            .authenticate_publickey(username, PrivateKeyWithHashAlg::new(Arc::clone(key), None))
            .await?);
    }
    Ok(res)
}

async fn authenticate_with_agent<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    agent_connector: &dyn AgentConnector,
) -> Result<AuthOutcome, SshError> {
    let mut agent = agent_connector.connect().await?;
    if let Some(binding) = handle.agent_session_binding().cloned() {
        let _ = agent.bind_session(&binding, false).await;
    }

    let identities = agent
        .request_identities()
        .await
        .map_err(|err| SshError::Agent(err.to_string()))?;
    if identities.is_empty() {
        return Err(SshError::AuthenticationFailed {
            username: username.to_string(),
            method: "agent",
            detail: "SSH agent has no identities loaded".to_string(),
        });
    }

    let has_rsa = identities.iter().any(|id| id.public_key().algorithm().is_rsa());
    let rsa_hash = if has_rsa {
        match handle.best_supported_rsa_hash().await? {
            Some(alg) => alg,
            None => Some(HashAlg::Sha256),
        }
    } else {
        None
    };

    for identity in identities {
        let hash = if identity.public_key().algorithm().is_rsa() {
            rsa_hash
        } else {
            None
        };
        let res = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                handle
                    .authenticate_publickey_with(username, key, hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                handle
                    .authenticate_certificate_with(username, certificate, hash, &mut agent)
                    .await
            }
        };

        match res {
            Ok(AuthResult::Success) => {
                return Ok(AuthOutcome {
                    used_interactive_auth: false,
                });
            }
            Ok(AuthResult::Failure { .. }) => continue,
            Err(err) => {
                tracing::debug!(error = %err, "SSH agent identity signing failed, trying next");
                continue;
            }
        }
    }

    Err(SshError::AuthenticationFailed {
        username: username.to_string(),
        method: "agent",
        detail: "none of the SSH agent identities were accepted by the server".to_string(),
    })
}

async fn authenticate_with_password<H: client::Handler>(
    handle: &mut client::Handle<H>,
    host: &str,
    username: &str,
    vault_secret_ref: Option<&str>,
    vault: Option<&dyn SecretStore>,
    interaction: &dyn SshInteractionHandler,
) -> Result<AuthOutcome, SshError> {
    let mut used_interactive = false;
    let password = match (vault, vault_secret_ref) {
        (Some(store), Some(secret_ref)) => match store.get_secret(secret_ref)? {
            Some(secret) => secret,
            None => {
                used_interactive = true;
                interaction
                    .prompt_password(username, host)
                    .await
                    .ok_or(SshError::InteractiveCancelled)?
            }
        },
        _ => {
            used_interactive = true;
            interaction
                .prompt_password(username, host)
                .await
                .ok_or(SshError::InteractiveCancelled)?
        }
    };

    let res = handle
        .authenticate_password(username, password.as_str())
        .await?;
    if res.success() {
        Ok(AuthOutcome {
            used_interactive_auth: used_interactive,
        })
    } else {
        Err(SshError::AuthenticationFailed {
            username: username.to_string(),
            method: "password",
            detail: "server rejected password".to_string(),
        })
    }
}

async fn authenticate_with_keyboard_interactive<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    interaction: &dyn SshInteractionHandler,
) -> Result<AuthOutcome, SshError> {
    let mut resp = handle
        .authenticate_keyboard_interactive_start(username, None)
        .await?;

    loop {
        match resp {
            KeyboardInteractiveAuthResponse::Success => {
                return Ok(AuthOutcome {
                    used_interactive_auth: true,
                });
            }
            KeyboardInteractiveAuthResponse::Failure { .. } => {
                return Err(SshError::AuthenticationFailed {
                    username: username.to_string(),
                    method: "keyboard-interactive",
                    detail: "server rejected keyboard-interactive authentication".to_string(),
                });
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                let answers: Vec<String> =
                    if prompts.is_empty() && name.is_empty() && instructions.is_empty() {
                        Vec::new()
                    } else {
                        let challenge = KeyboardInteractiveChallenge {
                            name,
                            instructions,
                            prompts: prompts
                                .into_iter()
                                .map(|p| InteractivePromptItem {
                                    prompt: p.prompt,
                                    echo: p.echo,
                                })
                                .collect(),
                        };
                        let provided = interaction
                            .prompt_keyboard_interactive(challenge)
                            .await
                            .ok_or(SshError::InteractiveCancelled)?;
                        provided
                            .into_iter()
                            .map(|mut s| std::mem::take(&mut *s))
                            .collect()
                    };

                resp = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
}

fn resolve_key_candidates(explicit: Option<&Path>) -> Result<Vec<PathBuf>, SshError> {
    if let Some(path) = explicit {
        return Ok(vec![expand_tilde(path)]);
    }

    let ssh_dir = BaseDirs::new()
        .map(|b| b.home_dir().join(".ssh"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ssh")))
        .ok_or_else(|| SshError::KeyLoad {
            path: PathBuf::from("~/.ssh"),
            reason: "unable to determine home directory".to_string(),
        })?;

    let existing: Vec<PathBuf> = ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| ssh_dir.join(name))
        .filter(|path| path.is_file())
        .collect();
    Ok(existing)
}

fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(stripped) = raw.strip_prefix("~/")
        && let Some(home) = BaseDirs::new()
            .map(|b| b.home_dir().to_path_buf())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
    {
        return home.join(stripped);
    }
    path.to_path_buf()
}

async fn load_private_key_with_vault(
    path: &Path,
    vault_secret_ref: Option<&str>,
    vault: Option<&dyn SecretStore>,
    interaction: &dyn SshInteractionHandler,
) -> Result<(PrivateKey, bool), SshError> {
    let pem = Zeroizing::new(
        std::fs::read_to_string(path).map_err(|err| SshError::KeyLoad {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?,
    );

    if !requires_passphrase_upfront(&pem) {
        match decode_secret_key(&pem, None) {
            Ok(key) => return Ok((key, false)),
            Err(russh::keys::Error::KeyIsEncrypted) => {}
            Err(err) => {
                return Err(SshError::KeyLoad {
                    path: path.to_path_buf(),
                    reason: err.to_string(),
                });
            }
        }
    }

    if let (Some(store), Some(secret_ref)) = (vault, vault_secret_ref)
        && let Some(passphrase) = store.get_secret(secret_ref)?
    {
        let key = decode_secret_key(&pem, Some(passphrase.as_str())).map_err(|err| {
            SshError::KeyLoad {
                path: path.to_path_buf(),
                reason: err.to_string(),
            }
        })?;
        return Ok((key, false));
    }

    let passphrase = interaction
        .prompt_key_passphrase(path)
        .await
        .ok_or_else(|| SshError::KeyPassphraseRequired {
            path: path.to_path_buf(),
        })?;

    let key = decode_secret_key(&pem, Some(passphrase.as_str())).map_err(|err| {
        SshError::KeyLoad {
            path: path.to_path_buf(),
            reason: err.to_string(),
        }
    })?;
    Ok((key, true))
}

fn requires_passphrase_upfront(pem: &str) -> bool {
    if pem.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
        || pem.contains("Proc-Type: 4,ENCRYPTED")
    {
        return true;
    }
    if pem.trim_start().starts_with("PuTTY-User-Key-File-") {
        return pem.lines().any(|line| {
            line.strip_prefix("Encryption:")
                .is_some_and(|alg| alg.trim() != "none")
        });
    }
    false
}
