//! Configuration bundle synchronization across `LocalDirectory`, `WebDav`, and `GitHubGist`
//! with optional Argon2id + AES-256-GCM encryption and `Vault`-isolated credentials.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::collections::{QuickCommandsConfig, WorkspacesConfig};
use crate::config::{AppConfig, ConfigPaths, SyncConfig, SyncProviderKind};
use crate::error::CoreError;
use crate::profile::ProfilesConfig;
use crate::storage::save_bytes_atomic;
use crate::vault::SecretStore;

pub const SYNC_BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const SYNC_FILE_NAME: &str = "termewnal-sync.json";

const SYNC_AAD_PREFIX: &[u8; 8] = b"TMNSYNC1";
const DEFAULT_SYNC_ARGON2_M_KIB: u32 = 19 * 1024;
const DEFAULT_SYNC_ARGON2_T: u32 = 2;
const DEFAULT_SYNC_ARGON2_P: u32 = 1;

const ALLOWED_BUNDLE_FILES: [&str; 4] = [
    "config.toml",
    "profiles.toml",
    "quick_commands.toml",
    "workspaces.toml",
];

pub type SyncFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Snapshot package of non-secret Termewnal TOML configuration files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncBundle {
    pub schema_version: u32,
    pub exported_at_unix_secs: u64,
    pub files: BTreeMap<String, String>,
}

/// Summary of files written atomically when applying a pulled [`SyncBundle`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncApplyReport {
    pub applied_files: Vec<String>,
    pub skipped_empty: usize,
}

impl SyncBundle {
    /// Collects non-secret configuration files from `paths` (never reads `Vault` or backups).
    pub fn collect_from_paths(paths: &ConfigPaths) -> Result<Self, CoreError> {
        let mut files = BTreeMap::new();
        let candidates = [
            ("config.toml", paths.config_file()),
            ("profiles.toml", paths.profiles_file()),
            ("quick_commands.toml", paths.quick_commands_file()),
            ("workspaces.toml", paths.workspaces_file()),
        ];

        for (name, path) in candidates {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    files.insert(name.to_string(), text);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(CoreError::Io { path, source: err });
                }
            }
        }

        let exported_at_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Ok(Self {
            schema_version: SYNC_BUNDLE_SCHEMA_VERSION,
            exported_at_unix_secs,
            files,
        })
    }

    /// Validates bundle schema version, filenames, and TOML syntax before applying anything to disk.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != SYNC_BUNDLE_SCHEMA_VERSION {
            return Err(CoreError::Sync(format!(
                "unsupported sync bundle version {} (expected {})",
                self.schema_version, SYNC_BUNDLE_SCHEMA_VERSION
            )));
        }

        for (name, raw) in &self.files {
            if !ALLOWED_BUNDLE_FILES.contains(&name.as_str()) {
                return Err(CoreError::Sync(format!(
                    "disallowed filename `{name}` in sync bundle"
                )));
            }
            match name.as_str() {
                "config.toml" => {
                    toml::from_str::<AppConfig>(raw).map_err(|e| {
                        CoreError::Sync(format!("invalid config.toml in sync bundle: {e}"))
                    })?;
                }
                "profiles.toml" => {
                    toml::from_str::<ProfilesConfig>(raw).map_err(|e| {
                        CoreError::Sync(format!("invalid profiles.toml in sync bundle: {e}"))
                    })?;
                }
                "quick_commands.toml" => {
                    toml::from_str::<QuickCommandsConfig>(raw).map_err(|e| {
                        CoreError::Sync(format!(
                            "invalid quick_commands.toml in sync bundle: {e}"
                        ))
                    })?;
                }
                "workspaces.toml" => {
                    toml::from_str::<WorkspacesConfig>(raw).map_err(|e| {
                        CoreError::Sync(format!("invalid workspaces.toml in sync bundle: {e}"))
                    })?;
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    /// Validates and atomically writes all bundled TOML files to `paths` with `.bak` backup rotation.
    pub fn apply_to_paths(&self, paths: &ConfigPaths) -> Result<SyncApplyReport, CoreError> {
        self.validate()?;
        let mut report = SyncApplyReport::default();

        for (name, content) in &self.files {
            let dest = match name.as_str() {
                "config.toml" => paths.config_file(),
                "profiles.toml" => paths.profiles_file(),
                "quick_commands.toml" => paths.quick_commands_file(),
                "workspaces.toml" => paths.workspaces_file(),
                _ => unreachable!("validated above"),
            };
            save_bytes_atomic(&dest, content.as_bytes())?;
            report.applied_files.push(name.clone());
        }

        Ok(report)
    }
}

/// Serialized JSON transport wrapper supporting plaintext or Argon2id+AES-256-GCM encrypted bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SyncEnvelope {
    Plaintext {
        version: u32,
        bundle: SyncBundle,
    },
    Encrypted {
        version: u32,
        kdf_m_cost_kib: u32,
        kdf_t_cost: u32,
        kdf_p_cost: u32,
        salt_b64: String,
        nonce_b64: String,
        ciphertext_b64: String,
    },
}

/// Encodes `bundle` as JSON wire bytes, encrypting with Argon2id + AES-256-GCM when `passphrase` is supplied.
pub fn encode_sync_envelope(
    bundle: &SyncBundle,
    passphrase: Option<&str>,
) -> Result<Vec<u8>, CoreError> {
    encode_sync_envelope_with_kdf(
        bundle,
        passphrase,
        DEFAULT_SYNC_ARGON2_M_KIB,
        DEFAULT_SYNC_ARGON2_T,
        DEFAULT_SYNC_ARGON2_P,
    )
}

/// Encodes `bundle` with explicit Argon2id cost parameters (allowing lightweight test parameters).
pub fn encode_sync_envelope_with_kdf(
    bundle: &SyncBundle,
    passphrase: Option<&str>,
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Vec<u8>, CoreError> {
    bundle.validate()?;
    let env = match passphrase.map(str::trim).filter(|s| !s.is_empty()) {
        None => SyncEnvelope::Plaintext {
            version: SYNC_BUNDLE_SCHEMA_VERSION,
            bundle: bundle.clone(),
        },
        Some(pw) => {
            let mut salt = [0_u8; 16];
            let mut nonce_bytes = [0_u8; 12];
            rand::rng().fill(&mut salt);
            rand::rng().fill(&mut nonce_bytes);

            let key = derive_sync_key(pw, &salt, m_cost_kib, t_cost, p_cost)?;
            let plaintext = Zeroizing::new(
                serde_json::to_vec(bundle)
                    .map_err(|e| CoreError::Sync(format!("failed serializing bundle JSON: {e}")))?,
            );
            let aad = sync_aad(
                SYNC_BUNDLE_SCHEMA_VERSION,
                m_cost_kib,
                t_cost,
                p_cost,
                &salt,
                &nonce_bytes,
            );
            let cipher = Aes256Gcm::new_from_slice(&*key)
                .map_err(|e| CoreError::Sync(format!("AEAD init failed: {e}")))?;
            let nonce = Nonce::from(nonce_bytes);
            let ct = cipher
                .encrypt(
                    &nonce,
                    Payload {
                        msg: plaintext.as_slice(),
                        aad: &aad,
                    },
                )
                .map_err(|e| CoreError::Sync(format!("sync bundle encryption failed: {e}")))?;

            SyncEnvelope::Encrypted {
                version: SYNC_BUNDLE_SCHEMA_VERSION,
                kdf_m_cost_kib: m_cost_kib,
                kdf_t_cost: t_cost,
                kdf_p_cost: p_cost,
                salt_b64: BASE64_STD.encode(salt),
                nonce_b64: BASE64_STD.encode(nonce_bytes),
                ciphertext_b64: BASE64_STD.encode(ct),
            }
        }
    };

    serde_json::to_vec_pretty(&env)
        .map_err(|e| CoreError::Sync(format!("failed serializing sync envelope: {e}")))
}

/// Decodes `raw` JSON wire bytes into a validated [`SyncBundle`], decrypting when needed.
pub fn decode_sync_envelope(raw: &[u8], passphrase: Option<&str>) -> Result<SyncBundle, CoreError> {
    let env: SyncEnvelope = serde_json::from_slice(raw)
        .map_err(|e| CoreError::Sync(format!("invalid sync envelope JSON: {e}")))?;

    let bundle = match env {
        SyncEnvelope::Plaintext { version, bundle } => {
            if version != SYNC_BUNDLE_SCHEMA_VERSION {
                return Err(CoreError::Sync(format!(
                    "unsupported envelope version {version}"
                )));
            }
            bundle
        }
        SyncEnvelope::Encrypted {
            version,
            kdf_m_cost_kib,
            kdf_t_cost,
            kdf_p_cost,
            salt_b64,
            nonce_b64,
            ciphertext_b64,
        } => {
            if version != SYNC_BUNDLE_SCHEMA_VERSION {
                return Err(CoreError::Sync(format!(
                    "unsupported encrypted envelope version {version}"
                )));
            }
            let pw = passphrase
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CoreError::Sync("sync bundle is encrypted; passphrase required".to_string())
                })?;

            let salt = BASE64_STD
                .decode(&salt_b64)
                .map_err(|e| CoreError::Sync(format!("invalid salt encoding: {e}")))?;
            let nonce_vec = BASE64_STD
                .decode(&nonce_b64)
                .map_err(|e| CoreError::Sync(format!("invalid nonce encoding: {e}")))?;
            if nonce_vec.len() != 12 {
                return Err(CoreError::Sync("invalid sync nonce length".to_string()));
            }
            let mut nonce_bytes = [0_u8; 12];
            nonce_bytes.copy_from_slice(&nonce_vec);

            let ct = BASE64_STD
                .decode(&ciphertext_b64)
                .map_err(|e| CoreError::Sync(format!("invalid ciphertext encoding: {e}")))?;

            let key = derive_sync_key(pw, &salt, kdf_m_cost_kib, kdf_t_cost, kdf_p_cost)?;
            let aad = sync_aad(
                version,
                kdf_m_cost_kib,
                kdf_t_cost,
                kdf_p_cost,
                &salt,
                &nonce_bytes,
            );
            let cipher = Aes256Gcm::new_from_slice(&*key)
                .map_err(|e| CoreError::Sync(format!("AEAD init failed: {e}")))?;
            let nonce = Nonce::from(nonce_bytes);
            let plaintext = Zeroizing::new(
                cipher
                    .decrypt(
                        &nonce,
                        Payload {
                            msg: ct.as_slice(),
                            aad: &aad,
                        },
                    )
                    .map_err(|_| {
                        CoreError::Sync(
                            "sync decryption failed (wrong passphrase or corrupted bundle)"
                                .to_string(),
                        )
                    })?,
            );

            serde_json::from_slice::<SyncBundle>(&plaintext).map_err(|e| {
                CoreError::Sync(format!("decrypted payload is not valid bundle JSON: {e}"))
            })?
        }
    };

    bundle.validate()?;
    Ok(bundle)
}

fn derive_sync_key(
    passphrase: &str,
    salt: &[u8],
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; 32]>, CoreError> {
    if p_cost == 0 || t_cost == 0 || m_cost_kib < 8 * p_cost || m_cost_kib > 1024 * 1024 {
        return Err(CoreError::Sync("invalid Argon2id parameters".to_string()));
    }
    let params = Params::new(m_cost_kib, t_cost, p_cost, Some(32))
        .map_err(|e| CoreError::Sync(format!("Argon2 params error: {e}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; 32]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut *key)
        .map_err(|e| CoreError::Sync(format!("Argon2 key derivation failed: {e}")))?;
    Ok(key)
}

fn sync_aad(
    version: u32,
    m: u32,
    t: u32,
    p: u32,
    salt: &[u8],
    nonce: &[u8; 12],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(8 + 16 + salt.len() + nonce.len());
    aad.extend_from_slice(SYNC_AAD_PREFIX);
    aad.extend_from_slice(&version.to_le_bytes());
    aad.extend_from_slice(&m.to_le_bytes());
    aad.extend_from_slice(&t.to_le_bytes());
    aad.extend_from_slice(&p.to_le_bytes());
    aad.extend_from_slice(salt);
    aad.extend_from_slice(nonce);
    aad
}

/// HTTP verbs used by remote sync providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncHttpMethod {
    Get,
    Put,
    Post,
    Patch,
}

#[derive(Debug, Clone)]
pub struct SyncHttpRequest {
    pub method: SyncHttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct SyncHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Injectable async HTTP client boundary for GitHub Gist and WebDAV providers.
pub trait SyncHttpClient: Send + Sync {
    fn send(&self, req: SyncHttpRequest) -> SyncFuture<'_, Result<SyncHttpResponse, CoreError>>;
}

/// Production `reqwest` HTTP/HTTPS adapter using `rustls-tls`.
#[derive(Clone)]
pub struct ReqwestSyncHttpClient {
    client: reqwest::Client,
}

impl Default for ReqwestSyncHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestSyncHttpClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("termewnal-sync/0.1")
            .build()
            .expect("reqwest client construction");
        Self { client }
    }
}

impl SyncHttpClient for ReqwestSyncHttpClient {
    fn send(&self, req: SyncHttpRequest) -> SyncFuture<'_, Result<SyncHttpResponse, CoreError>> {
        let client = self.client.clone();
        Box::pin(async move {
            let method = match req.method {
                SyncHttpMethod::Get => reqwest::Method::GET,
                SyncHttpMethod::Put => reqwest::Method::PUT,
                SyncHttpMethod::Post => reqwest::Method::POST,
                SyncHttpMethod::Patch => reqwest::Method::PATCH,
            };
            let mut builder = client.request(method, &req.url);
            for (k, v) in req.headers {
                builder = builder.header(k, v);
            }
            if let Some(body) = req.body {
                builder = builder.body(body);
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| CoreError::Sync(format!("HTTP request failed: {e}")))?;
            let status = resp.status().as_u16();
            let body = resp
                .bytes()
                .await
                .map_err(|e| CoreError::Sync(format!("failed reading HTTP body: {e}")))?
                .to_vec();
            Ok(SyncHttpResponse { status, body })
        })
    }
}

/// Metadata returned when pushing configuration to a sync provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPushOutcome {
    pub bytes_uploaded: usize,
    pub target_location: String,
    pub created_gist_id: Option<String>,
    pub synced_at_unix_secs: u64,
}

/// Metadata returned when pulling and applying configuration from a sync provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPullOutcome {
    pub bundle: SyncBundle,
    pub apply_report: SyncApplyReport,
    pub source_location: String,
}

#[derive(Debug, Serialize)]
struct GistWritePayload {
    description: String,
    public: bool,
    files: BTreeMap<String, GistWriteFile>,
}

#[derive(Debug, Serialize)]
struct GistWriteFile {
    content: String,
}

#[derive(Debug, Deserialize)]
struct GistReadResponse {
    id: Option<String>,
    #[serde(default)]
    files: BTreeMap<String, GistReadFile>,
}

#[derive(Debug, Deserialize)]
struct GistReadFile {
    content: Option<String>,
    #[serde(default)]
    truncated: bool,
    raw_url: Option<String>,
}

/// Orchestrates config bundle serialization, optional encryption, provider transfer, and atomic restore.
pub struct ConfigSyncService {
    http: Arc<dyn SyncHttpClient>,
    github_api_base: String,
    kdf_params: (u32, u32, u32),
}

impl Default for ConfigSyncService {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigSyncService {
    pub fn new() -> Self {
        Self::with_http_client(Arc::new(ReqwestSyncHttpClient::new()))
    }

    pub fn with_http_client(http: Arc<dyn SyncHttpClient>) -> Self {
        Self {
            http,
            github_api_base: "https://api.github.com".to_string(),
            kdf_params: (
                DEFAULT_SYNC_ARGON2_M_KIB,
                DEFAULT_SYNC_ARGON2_T,
                DEFAULT_SYNC_ARGON2_P,
            ),
        }
    }

    pub fn with_github_api_base(mut self, base: impl Into<String>) -> Self {
        self.github_api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Overrides Argon2id costs (useful for fast deterministic unit/integration tests).
    pub fn with_kdf_params(mut self, m_cost_kib: u32, t_cost: u32, p_cost: u32) -> Self {
        self.kdf_params = (m_cost_kib, t_cost, p_cost);
        self
    }

    /// Collects local non-secret configs, encodes/encrypts them, and pushes to the configured provider.
    pub async fn push(
        &self,
        paths: &ConfigPaths,
        sync_cfg: &SyncConfig,
        vault: Option<&dyn SecretStore>,
        passphrase: Option<&str>,
    ) -> Result<SyncPushOutcome, CoreError> {
        let bundle = SyncBundle::collect_from_paths(paths)?;
        let effective_pw = if sync_cfg.encrypt_bundle {
            Some(
                passphrase
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        CoreError::Sync(
                            "sync encryption is enabled; passphrase must not be empty".to_string(),
                        )
                    })?,
            )
        } else {
            None
        };

        let wire_bytes = encode_sync_envelope_with_kdf(
            &bundle,
            effective_pw,
            self.kdf_params.0,
            self.kdf_params.1,
            self.kdf_params.2,
        )?;
        let bytes_uploaded = wire_bytes.len();
        let synced_at_unix_secs = bundle.exported_at_unix_secs;

        match sync_cfg.provider {
            SyncProviderKind::None => Err(CoreError::Sync(
                "no sync provider configured (SyncProviderKind::None)".to_string(),
            )),
            SyncProviderKind::LocalDirectory => {
                let target = resolve_local_sync_file(sync_cfg)?;
                save_bytes_atomic(&target, &wire_bytes)?;
                Ok(SyncPushOutcome {
                    bytes_uploaded,
                    target_location: target.display().to_string(),
                    created_gist_id: None,
                    synced_at_unix_secs,
                })
            }
            SyncProviderKind::WebDav => {
                let url = resolve_webdav_file_url(sync_cfg)?;
                let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
                if let Some(auth_header) = build_webdav_auth_header(sync_cfg, vault)? {
                    headers.push(("Authorization".to_string(), auth_header));
                }
                let resp = self
                    .http
                    .send(SyncHttpRequest {
                        method: SyncHttpMethod::Put,
                        url: url.clone(),
                        headers,
                        body: Some(wire_bytes),
                    })
                    .await?;
                if !(200..300).contains(&resp.status) {
                    return Err(CoreError::Sync(format!(
                        "WebDAV PUT `{url}` returned HTTP {}: {}",
                        resp.status,
                        String::from_utf8_lossy(&resp.body)
                    )));
                }
                Ok(SyncPushOutcome {
                    bytes_uploaded,
                    target_location: url,
                    created_gist_id: None,
                    synced_at_unix_secs,
                })
            }
            SyncProviderKind::GitHubGist => {
                let token = require_vault_token(sync_cfg, vault, "GitHub Gist")?;
                let content = String::from_utf8(wire_bytes)
                    .map_err(|e| CoreError::Sync(format!("envelope is not UTF-8: {e}")))?;
                let mut files = BTreeMap::new();
                files.insert(SYNC_FILE_NAME.to_string(), GistWriteFile { content });
                let payload = GistWritePayload {
                    description: "Termewnal synced settings bundle".to_string(),
                    public: false,
                    files,
                };
                let body = serde_json::to_vec(&payload)
                    .map_err(|e| CoreError::Sync(format!("failed serializing Gist payload: {e}")))?;

                let existing_id = sync_cfg
                    .gist_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let (method, url) = match existing_id {
                    Some(id) => (
                        SyncHttpMethod::Patch,
                        format!("{}/gists/{id}", self.github_api_base),
                    ),
                    None => (
                        SyncHttpMethod::Post,
                        format!("{}/gists", self.github_api_base),
                    ),
                };

                let headers = github_headers(&token);
                let resp = self
                    .http
                    .send(SyncHttpRequest {
                        method,
                        url: url.clone(),
                        headers,
                        body: Some(body),
                    })
                    .await?;
                if !(200..300).contains(&resp.status) {
                    return Err(CoreError::Sync(format!(
                        "GitHub Gist push returned HTTP {}: {}",
                        resp.status,
                        String::from_utf8_lossy(&resp.body)
                    )));
                }
                let parsed: GistReadResponse = serde_json::from_slice(&resp.body)
                    .map_err(|e| CoreError::Sync(format!("invalid GitHub Gist JSON response: {e}")))?;
                let created_gist_id = if existing_id.is_none() { parsed.id } else { None };
                Ok(SyncPushOutcome {
                    bytes_uploaded,
                    target_location: url,
                    created_gist_id,
                    synced_at_unix_secs,
                })
            }
        }
    }

    /// Pulls configuration bundle from the configured provider, decrypts/validates it, and applies atomically.
    pub async fn pull(
        &self,
        paths: &ConfigPaths,
        sync_cfg: &SyncConfig,
        vault: Option<&dyn SecretStore>,
        passphrase: Option<&str>,
    ) -> Result<SyncPullOutcome, CoreError> {
        let (wire_bytes, source_location) = match sync_cfg.provider {
            SyncProviderKind::None => {
                return Err(CoreError::Sync(
                    "no sync provider configured (SyncProviderKind::None)".to_string(),
                ));
            }
            SyncProviderKind::LocalDirectory => {
                let target = resolve_local_sync_file(sync_cfg)?;
                let raw = fs::read(&target).map_err(|source| CoreError::Io {
                    path: target.clone(),
                    source,
                })?;
                (raw, target.display().to_string())
            }
            SyncProviderKind::WebDav => {
                let url = resolve_webdav_file_url(sync_cfg)?;
                let mut headers = Vec::new();
                if let Some(auth_header) = build_webdav_auth_header(sync_cfg, vault)? {
                    headers.push(("Authorization".to_string(), auth_header));
                }
                let resp = self
                    .http
                    .send(SyncHttpRequest {
                        method: SyncHttpMethod::Get,
                        url: url.clone(),
                        headers,
                        body: None,
                    })
                    .await?;
                if !(200..300).contains(&resp.status) {
                    return Err(CoreError::Sync(format!(
                        "WebDAV GET `{url}` returned HTTP {}",
                        resp.status
                    )));
                }
                (resp.body, url)
            }
            SyncProviderKind::GitHubGist => {
                let gist_id = sync_cfg
                    .gist_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        CoreError::Sync("GitHub Gist pull requires a non-empty `gist_id`".to_string())
                    })?;
                let url = format!("{}/gists/{gist_id}", self.github_api_base);
                let mut headers = vec![("Accept".to_string(), "application/vnd.github+json".to_string())];
                if let Some(token) = lookup_vault_token(sync_cfg, vault)? {
                    headers.push(("Authorization".to_string(), format!("Bearer {}", &*token)));
                }
                let resp = self
                    .http
                    .send(SyncHttpRequest {
                        method: SyncHttpMethod::Get,
                        url: url.clone(),
                        headers,
                        body: None,
                    })
                    .await?;
                if !(200..300).contains(&resp.status) {
                    return Err(CoreError::Sync(format!(
                        "GitHub Gist pull returned HTTP {}",
                        resp.status
                    )));
                }
                let parsed: GistReadResponse = serde_json::from_slice(&resp.body)
                    .map_err(|e| CoreError::Sync(format!("invalid Gist response JSON: {e}")))?;
                let file = parsed.files.get(SYNC_FILE_NAME).ok_or_else(|| {
                    CoreError::Sync(format!(
                        "Gist `{gist_id}` does not contain `{SYNC_FILE_NAME}`"
                    ))
                })?;

                let content_bytes = if !file.truncated {
                    if let Some(text) = &file.content {
                        text.as_bytes().to_vec()
                    } else if let Some(raw_url) = &file.raw_url {
                        self.fetch_raw_url(raw_url).await?
                    } else {
                        return Err(CoreError::Sync("Gist file has empty content".to_string()));
                    }
                } else if let Some(raw_url) = &file.raw_url {
                    self.fetch_raw_url(raw_url).await?
                } else {
                    return Err(CoreError::Sync("truncated Gist file missing raw_url".to_string()));
                };
                (content_bytes, url)
            }
        };

        let bundle = decode_sync_envelope(&wire_bytes, passphrase)?;
        let apply_report = bundle.apply_to_paths(paths)?;
        Ok(SyncPullOutcome {
            bundle,
            apply_report,
            source_location,
        })
    }

    async fn fetch_raw_url(&self, raw_url: &str) -> Result<Vec<u8>, CoreError> {
        let resp = self
            .http
            .send(SyncHttpRequest {
                method: SyncHttpMethod::Get,
                url: raw_url.to_string(),
                headers: Vec::new(),
                body: None,
            })
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(CoreError::Sync(format!(
                "failed fetching raw Gist URL `{raw_url}` (HTTP {})",
                resp.status
            )));
        }
        Ok(resp.body)
    }
}

fn resolve_local_sync_file(sync_cfg: &SyncConfig) -> Result<PathBuf, CoreError> {
    let dir = sync_cfg.local_dir.as_ref().ok_or_else(|| {
        CoreError::Sync("LocalDirectory sync requires `local_dir` to be set".to_string())
    })?;
    Ok(dir.join(SYNC_FILE_NAME))
}

fn resolve_webdav_file_url(sync_cfg: &SyncConfig) -> Result<String, CoreError> {
    let raw = sync_cfg
        .webdav_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Sync("WebDAV sync requires `webdav_url`".to_string()))?;
    if raw.ends_with(".json") {
        Ok(raw.to_string())
    } else {
        Ok(format!("{}/{SYNC_FILE_NAME}", raw.trim_end_matches('/')))
    }
}

fn lookup_vault_token(
    sync_cfg: &SyncConfig,
    vault: Option<&dyn SecretStore>,
) -> Result<Option<Zeroizing<String>>, CoreError> {
    let Some(secret_ref) = sync_cfg
        .vault_token_ref
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let store = vault.ok_or_else(|| {
        CoreError::Sync("sync token reference configured but Vault is locked or unavailable".to_string())
    })?;
    let secret = store
        .get_secret(secret_ref)
        .map_err(|e| CoreError::Sync(format!("failed reading token `{secret_ref}` from Vault: {e}")))?;
    Ok(secret.map(|s| Zeroizing::new(s.to_string())))
}

fn require_vault_token(
    sync_cfg: &SyncConfig,
    vault: Option<&dyn SecretStore>,
    provider_name: &str,
) -> Result<Zeroizing<String>, CoreError> {
    lookup_vault_token(sync_cfg, vault)?.ok_or_else(|| {
        CoreError::Sync(format!(
            "{provider_name} push requires a token stored in Vault (`vault_token_ref`)"
        ))
    })
}

fn build_webdav_auth_header(
    sync_cfg: &SyncConfig,
    vault: Option<&dyn SecretStore>,
) -> Result<Option<String>, CoreError> {
    let Some(token) = lookup_vault_token(sync_cfg, vault)? else {
        return Ok(None);
    };
    if let Some(user) = sync_cfg
        .webdav_username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mut creds = format!("{user}:{}", &*token);
        let encoded = BASE64_STD.encode(creds.as_bytes());
        creds.zeroize();
        Ok(Some(format!("Basic {encoded}")))
    } else {
        Ok(Some(format!("Bearer {}", &*token)))
    }
}

fn github_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "application/vnd.github+json".to_string()),
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("Content-Type".to_string(), "application/json".to_string()),
    ]
}
