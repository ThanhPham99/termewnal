use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use keyring_core::CredentialStore;
use rand::RngExt;
use tempfile::NamedTempFile;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::config::{ConfigPaths, VaultBackend, VaultConfig};

/// Default service identifier used when storing credentials in the OS keyring.
pub const DEFAULT_KEYRING_SERVICE: &str = "io.termewnal.Termewnal";

const VAULT_MAGIC: [u8; 8] = *b"TMNVLT01";
const VAULT_FORMAT_VERSION: u32 = 1;
const PAYLOAD_MAGIC: [u8; 3] = *b"TV1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const HEADER_LEN: usize = 8 + 4 + 4 + 4 + 4 + SALT_LEN + NONCE_LEN;

const DEFAULT_ARGON2_M_COST_KIB: u32 = 19 * 1024; // 19 MiB OWASP Argon2id baseline
const DEFAULT_ARGON2_T_COST: u32 = 2;
const DEFAULT_ARGON2_P_COST: u32 = 1;

/// Errors returned by `Vault` secret storage, encryption, and unlock lifecycle operations.
#[derive(Debug, Error)]
pub enum VaultError {
    /// Encrypted local `Vault` is locked and must be unlocked with the master password first.
    #[error("Vault is locked; unlock with master password first")]
    Locked,

    /// Encrypted local `Vault` file does not exist yet and must be initialized first.
    #[error("encrypted Vault has not been initialized yet")]
    NotInitialized,

    /// Encrypted local `Vault` file already exists and cannot be overwritten via `initialize`.
    #[error("encrypted Vault already exists")]
    AlreadyInitialized,

    /// Supplied master password was empty.
    #[error("master password must not be empty")]
    EmptyMasterPassword,

    /// Master password is incorrect or the encrypted `Vault` ciphertext failed authentication.
    #[error("invalid master password or corrupted Vault ciphertext")]
    InvalidMasterPassword,

    /// Requested secret reference is invalid (for example empty).
    #[error("invalid secret reference: {0}")]
    InvalidSecretRef(String),

    /// Native OS keyring store is unavailable in the current environment.
    #[error("OS keyring is unavailable: {0}")]
    KeyringUnavailable(String),

    /// Native OS keyring returned an operational error.
    #[error("OS keyring error: {0}")]
    KeyringFailure(String),

    /// Vault file header or payload format is malformed or uses an unsupported version.
    #[error("invalid encrypted Vault format: {0}")]
    InvalidFormat(String),

    /// Cryptographic key derivation or AEAD encryption error.
    #[error("Vault cryptography error: {0}")]
    Crypto(String),

    /// Filesystem I/O failure while reading or atomically writing the encrypted `Vault`.
    #[error("Vault I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Trait boundary isolating password and private-key passphrase storage from TOML configuration.
pub trait SecretStore: Send + Sync {
    /// Retrieves the UTF-8 secret stored under `secret_ref`, or `Ok(None)` if not present.
    fn get_secret(&self, secret_ref: &str) -> Result<Option<Zeroizing<String>>, VaultError>;

    /// Stores or updates a UTF-8 secret under `secret_ref`.
    fn set_secret(&self, secret_ref: &str, secret: &str) -> Result<(), VaultError>;

    /// Deletes any secret stored under `secret_ref`. Returns `Ok(true)` if an entry was removed.
    fn delete_secret(&self, secret_ref: &str) -> Result<bool, VaultError>;
}

/// Thread-safe in-memory [`SecretStore`] double for headless testing and isolated environments.
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    entries: Mutex<BTreeMap<String, Zeroizing<String>>>,
}

impl InMemorySecretStore {
    /// Creates an empty [`InMemorySecretStore`].
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for InMemorySecretStore {
    fn get_secret(&self, secret_ref: &str) -> Result<Option<Zeroizing<String>>, VaultError> {
        validate_secret_ref(secret_ref)?;
        let guard = self.entries.lock().expect("in-memory vault mutex poisoned");
        Ok(guard.get(secret_ref).cloned())
    }

    fn set_secret(&self, secret_ref: &str, secret: &str) -> Result<(), VaultError> {
        validate_secret_ref(secret_ref)?;
        let mut guard = self.entries.lock().expect("in-memory vault mutex poisoned");
        guard.insert(secret_ref.to_string(), Zeroizing::new(secret.to_string()));
        Ok(())
    }

    fn delete_secret(&self, secret_ref: &str) -> Result<bool, VaultError> {
        validate_secret_ref(secret_ref)?;
        let mut guard = self.entries.lock().expect("in-memory vault mutex poisoned");
        Ok(guard.remove(secret_ref).is_some())
    }
}

enum KeyringBackend {
    NativeOs,
    Injected(Arc<CredentialStore>),
}

/// OS Keyring [`SecretStore`] implementation using `keyring` (`Secret Service` / `Keychain` / `Credential Manager`).
///
/// Tests can inject a [`keyring_core::CredentialStore`] (such as `keyring_core::mock::Store`)
/// via [`KeyringSecretStore::with_credential_store`] without mutating global static state.
pub struct KeyringSecretStore {
    service: String,
    backend: KeyringBackend,
}

impl KeyringSecretStore {
    /// Creates a [`KeyringSecretStore`] targeting the native OS keyring under [`DEFAULT_KEYRING_SERVICE`].
    pub fn new() -> Self {
        Self::with_service(DEFAULT_KEYRING_SERVICE)
    }

    /// Creates a [`KeyringSecretStore`] targeting the native OS keyring under a custom `service` name.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            backend: KeyringBackend::NativeOs,
        }
    }

    /// Creates a [`KeyringSecretStore`] backed by an explicit [`CredentialStore`] (ideal for tests).
    pub fn with_credential_store(
        service: impl Into<String>,
        store: Arc<CredentialStore>,
    ) -> Self {
        Self {
            service: service.into(),
            backend: KeyringBackend::Injected(store),
        }
    }

    fn open_entry(&self, secret_ref: &str) -> Result<KeyringEntryHandle, VaultError> {
        validate_secret_ref(secret_ref)?;
        match &self.backend {
            KeyringBackend::NativeOs => keyring::Entry::new(&self.service, secret_ref)
                .map(KeyringEntryHandle::Os)
                .map_err(map_keyring_error),
            KeyringBackend::Injected(store) => {
                keyring_core::Entry::new_in_store(store, &self.service, secret_ref)
                    .map(KeyringEntryHandle::Core)
                    .map_err(map_keyring_error)
            }
        }
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringSecretStore {
    fn get_secret(&self, secret_ref: &str) -> Result<Option<Zeroizing<String>>, VaultError> {
        let entry = self.open_entry(secret_ref)?;
        match entry.get_secret_bytes() {
            Ok(raw) => {
                let text = String::from_utf8(raw.to_vec()).map_err(|err| {
                    let mut bytes = err.into_bytes();
                    bytes.zeroize();
                    VaultError::KeyringFailure("stored keyring secret is not valid UTF-8".into())
                })?;
                Ok(Some(Zeroizing::new(text)))
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_keyring_error(err)),
        }
    }

    fn set_secret(&self, secret_ref: &str, secret: &str) -> Result<(), VaultError> {
        let entry = self.open_entry(secret_ref)?;
        entry
            .set_secret_bytes(secret.as_bytes())
            .map_err(map_keyring_error)
    }

    fn delete_secret(&self, secret_ref: &str) -> Result<bool, VaultError> {
        let entry = self.open_entry(secret_ref)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(err) => Err(map_keyring_error(err)),
        }
    }
}

enum KeyringEntryHandle {
    Os(keyring::Entry),
    Core(keyring_core::Entry),
}

impl KeyringEntryHandle {
    fn get_secret_bytes(&self) -> keyring_core::Result<Zeroizing<Vec<u8>>> {
        let bytes = match self {
            Self::Os(e) => e.get_secret()?,
            Self::Core(e) => e.get_secret()?,
        };
        Ok(Zeroizing::new(bytes))
    }

    fn set_secret_bytes(&self, secret: &[u8]) -> keyring_core::Result<()> {
        match self {
            Self::Os(e) => e.set_secret(secret),
            Self::Core(e) => e.set_secret(secret),
        }
    }

    fn delete_credential(&self) -> keyring_core::Result<()> {
        match self {
            Self::Os(e) => e.delete_credential(),
            Self::Core(e) => e.delete_credential(),
        }
    }
}

fn map_keyring_error(err: keyring_core::Error) -> VaultError {
    match err {
        keyring_core::Error::NoDefaultStore | keyring_core::Error::NoStorageAccess(_) => {
            VaultError::KeyringUnavailable(err.to_string())
        }
        other => VaultError::KeyringFailure(other.to_string()),
    }
}

/// Snapshot of monotonic and wall-clock time for detecting both idle timeouts and OS sleep jumps.
#[derive(Debug, Clone, Copy)]
pub struct ClockSample {
    pub monotonic: Instant,
    pub wall: SystemTime,
}

impl ClockSample {
    /// Captures the current OS monotonic and wall clocks.
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// Computes effective elapsed time since `earlier`, accounting for monotonic idle AND wall-clock
    /// sleep jumps across suspend/hibernate.
    pub fn elapsed_since(self, earlier: Self) -> Duration {
        let mono_elapsed = self.monotonic.saturating_duration_since(earlier.monotonic);
        let wall_elapsed = self
            .wall
            .duration_since(earlier.wall)
            .unwrap_or(Duration::ZERO);
        mono_elapsed.max(wall_elapsed)
    }
}

/// Time source abstraction used by [`EncryptedVault`] and [`VaultSession`].
pub trait VaultClock: Send + Sync {
    /// Returns the current [`ClockSample`].
    fn now(&self) -> ClockSample;
}

/// Production [`VaultClock`] backed by [`Instant::now`] and [`SystemTime::now`].
#[derive(Debug, Default)]
pub struct SystemVaultClock;

impl VaultClock for SystemVaultClock {
    fn now(&self) -> ClockSample {
        ClockSample::now()
    }
}

/// Controllable [`VaultClock`] test double supporting deterministic idle and sleep advances.
#[derive(Debug)]
pub struct ManualVaultClock {
    current: Mutex<ClockSample>,
}

impl ManualVaultClock {
    /// Creates a [`ManualVaultClock`] anchored at the current instant.
    pub fn new() -> Self {
        Self {
            current: Mutex::new(ClockSample::now()),
        }
    }

    /// Advances both monotonic and wall clocks by `duration` (normal awake idle time).
    pub fn advance_idle(&self, duration: Duration) {
        let mut guard = self.current.lock().expect("manual clock mutex poisoned");
        guard.monotonic += duration;
        guard.wall += duration;
    }

    /// Advances only wall clock by `duration` while monotonic time stays paused (OS sleep/suspend).
    pub fn advance_sleep(&self, duration: Duration) {
        let mut guard = self.current.lock().expect("manual clock mutex poisoned");
        guard.wall += duration;
    }
}

impl Default for ManualVaultClock {
    fn default() -> Self {
        Self::new()
    }
}

impl VaultClock for ManualVaultClock {
    fn now(&self) -> ClockSample {
        *self.current.lock().expect("manual clock mutex poisoned")
    }
}

/// Argon2id parameters persisted inside the local encrypted `Vault` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultKdfParams {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for VaultKdfParams {
    fn default() -> Self {
        Self {
            m_cost_kib: DEFAULT_ARGON2_M_COST_KIB,
            t_cost: DEFAULT_ARGON2_T_COST,
            p_cost: DEFAULT_ARGON2_P_COST,
        }
    }
}

impl VaultKdfParams {
    fn validate(self) -> Result<(), VaultError> {
        if self.p_cost == 0 || self.t_cost == 0 || self.m_cost_kib < 8 * self.p_cost {
            return Err(VaultError::InvalidFormat(
                "invalid Argon2id parameters in Vault header".to_string(),
            ));
        }
        if self.m_cost_kib > 1024 * 1024 {
            return Err(VaultError::InvalidFormat(
                "Argon2id memory parameter exceeds 1 GiB limit".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct VaultKey([u8; KEY_LEN]);

struct UnlockedVaultState {
    key: Zeroizing<VaultKey>,
    kdf_params: VaultKdfParams,
    salt: [u8; SALT_LEN],
    entries: BTreeMap<String, Zeroizing<String>>,
    last_activity: ClockSample,
}

impl UnlockedVaultState {
    fn zeroize_and_clear(&mut self) {
        self.key.zeroize();
        self.salt.zeroize();
        for value in self.entries.values_mut() {
            value.zeroize();
        }
        self.entries.clear();
    }
}

struct EncryptedVaultInner {
    unlocked: Option<UnlockedVaultState>,
    auto_lock_timeout: Option<Duration>,
}

/// Local master-password encrypted `Vault` using Argon2id + AES-256-GCM and atomic `0600` writes.
///
/// Unlocks once per running process session, supports manual locking, auto-locks on idle or OS
/// sleep timeout, and zeroizes decrypted key material in RAM on lock and drop.
pub struct EncryptedVault {
    path: PathBuf,
    clock: Arc<dyn VaultClock>,
    inner: Mutex<EncryptedVaultInner>,
}

impl EncryptedVault {
    /// Opens an [`EncryptedVault`] handle at `path` using the default 30-minute auto-lock timeout.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self::with_timeout(path, Some(Duration::from_secs(30 * 60)))
    }

    /// Opens an [`EncryptedVault`] handle at `path` with an optional auto-lock timeout (`None` disables).
    pub fn with_timeout(path: impl Into<PathBuf>, auto_lock_timeout: Option<Duration>) -> Self {
        Self::with_clock(path, auto_lock_timeout, Arc::new(SystemVaultClock))
    }

    /// Opens an [`EncryptedVault`] handle with an injected [`VaultClock`] for deterministic testing.
    pub fn with_clock(
        path: impl Into<PathBuf>,
        auto_lock_timeout: Option<Duration>,
        clock: Arc<dyn VaultClock>,
    ) -> Self {
        let normalized_timeout = auto_lock_timeout.filter(|d| !d.is_zero());
        Self {
            path: path.into(),
            clock,
            inner: Mutex::new(EncryptedVaultInner {
                unlocked: None,
                auto_lock_timeout: normalized_timeout,
            }),
        }
    }

    /// Path to the backing encrypted `Vault` file (`vault.enc`).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns `true` if the backing encrypted `Vault` file exists on disk.
    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// Updates the idle/sleep auto-lock timeout (`None` or `Duration::ZERO` disables auto-lock).
    pub fn set_auto_lock_timeout(&self, timeout: Option<Duration>) {
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        guard.auto_lock_timeout = timeout.filter(|d| !d.is_zero());
        let now = self.clock.now();
        enforce_timeout_locked(&mut guard, now);
    }

    /// Returns `true` if the `Vault` is currently unlocked (enforcing idle/sleep timeout first).
    pub fn is_unlocked(&self) -> bool {
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        let now = self.clock.now();
        enforce_timeout_locked(&mut guard, now);
        guard.unlocked.is_some()
    }

    /// Checks idle/sleep auto-lock timeout and zeroizes key material if expired.
    ///
    /// Returns `true` if the `Vault` transitioned from unlocked to locked during this check.
    pub fn tick_idle(&self) -> bool {
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        let now = self.clock.now();
        enforce_timeout_locked(&mut guard, now)
    }

    /// Creates a brand-new encrypted `Vault` file on disk protected by `master_password` and
    /// unlocks it for the current process session.
    pub fn initialize(&self, master_password: &str) -> Result<(), VaultError> {
        if master_password.is_empty() {
            return Err(VaultError::EmptyMasterPassword);
        }
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        if self.path.exists() {
            return Err(VaultError::AlreadyInitialized);
        }

        let kdf_params = VaultKdfParams::default();
        let mut salt = [0_u8; SALT_LEN];
        rand::rng().fill(&mut salt);

        let key = derive_vault_key(master_password, &salt, kdf_params)?;
        let entries = BTreeMap::new();
        persist_encrypted_vault(&self.path, &key, kdf_params, &salt, &entries)?;

        guard.unlocked = Some(UnlockedVaultState {
            key,
            kdf_params,
            salt,
            entries,
            last_activity: self.clock.now(),
        });
        Ok(())
    }

    /// Unlocks an existing encrypted `Vault` file once for the current process session.
    pub fn unlock(&self, master_password: &str) -> Result<(), VaultError> {
        if master_password.is_empty() {
            return Err(VaultError::EmptyMasterPassword);
        }

        let raw = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(VaultError::NotInitialized);
            }
            Err(source) => {
                return Err(VaultError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let parsed = parse_vault_envelope(&raw)?;
        let key = derive_vault_key(master_password, &parsed.salt, parsed.kdf_params)?;
        let cipher = Aes256Gcm::new_from_slice(&key.0)
            .map_err(|err| VaultError::Crypto(err.to_string()))?;
        let nonce = Nonce::from(parsed.nonce);
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: parsed.ciphertext,
                        aad: parsed.header_aad,
                    },
                )
                .map_err(|_| VaultError::InvalidMasterPassword)?,
        );
        let entries = decode_payload(&plaintext)?;

        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        if let Some(mut old) = guard.unlocked.take() {
            old.zeroize_and_clear();
        }
        guard.unlocked = Some(UnlockedVaultState {
            key,
            kdf_params: parsed.kdf_params,
            salt: parsed.salt,
            entries,
            last_activity: self.clock.now(),
        });
        Ok(())
    }

    /// Manually locks the `Vault` and immediately zeroizes decrypted key material and cached entries.
    pub fn lock(&self) {
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        lock_and_zeroize_locked(&mut guard);
    }
}

impl Drop for EncryptedVault {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.lock() {
            lock_and_zeroize_locked(&mut guard);
        }
    }
}

impl SecretStore for EncryptedVault {
    fn get_secret(&self, secret_ref: &str) -> Result<Option<Zeroizing<String>>, VaultError> {
        validate_secret_ref(secret_ref)?;
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        let now = self.clock.now();
        enforce_timeout_locked(&mut guard, now);

        let state = guard.unlocked.as_mut().ok_or(VaultError::Locked)?;
        state.last_activity = now;
        Ok(state.entries.get(secret_ref).cloned())
    }

    fn set_secret(&self, secret_ref: &str, secret: &str) -> Result<(), VaultError> {
        validate_secret_ref(secret_ref)?;
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        let now = self.clock.now();
        enforce_timeout_locked(&mut guard, now);

        let state = guard.unlocked.as_mut().ok_or(VaultError::Locked)?;
        let previous = state
            .entries
            .insert(secret_ref.to_string(), Zeroizing::new(secret.to_string()));

        if let Err(err) = persist_encrypted_vault(
            &self.path,
            &state.key,
            state.kdf_params,
            &state.salt,
            &state.entries,
        ) {
            if let Some(prev) = previous {
                state.entries.insert(secret_ref.to_string(), prev);
            } else {
                state.entries.remove(secret_ref);
            }
            return Err(err);
        }

        state.last_activity = now;
        Ok(())
    }

    fn delete_secret(&self, secret_ref: &str) -> Result<bool, VaultError> {
        validate_secret_ref(secret_ref)?;
        let mut guard = self.inner.lock().expect("encrypted vault mutex poisoned");
        let now = self.clock.now();
        enforce_timeout_locked(&mut guard, now);

        let state = guard.unlocked.as_mut().ok_or(VaultError::Locked)?;
        let Some(removed) = state.entries.remove(secret_ref) else {
            state.last_activity = now;
            return Ok(false);
        };

        if let Err(err) = persist_encrypted_vault(
            &self.path,
            &state.key,
            state.kdf_params,
            &state.salt,
            &state.entries,
        ) {
            state.entries.insert(secret_ref.to_string(), removed);
            return Err(err);
        }

        state.last_activity = now;
        Ok(true)
    }
}

fn lock_and_zeroize_locked(guard: &mut EncryptedVaultInner) {
    if let Some(mut state) = guard.unlocked.take() {
        state.zeroize_and_clear();
    }
}

fn enforce_timeout_locked(guard: &mut EncryptedVaultInner, now: ClockSample) -> bool {
    let Some(timeout) = guard.auto_lock_timeout else {
        return false;
    };
    let Some(state) = guard.unlocked.as_ref() else {
        return false;
    };
    if now.elapsed_since(state.last_activity) >= timeout {
        lock_and_zeroize_locked(guard);
        return true;
    }
    false
}

/// Process-level `Vault` session coordinating the configured [`VaultBackend`] (`Keyring` vs
/// `EncryptedFile`), unlock-once lifecycle, manual lock, and idle/sleep auto-lock.
pub struct VaultSession {
    config: Mutex<VaultConfig>,
    keyring: Arc<dyn SecretStore>,
    encrypted: EncryptedVault,
}

impl VaultSession {
    /// Builds a [`VaultSession`] using standard OS keyring and `paths.vault_file()`.
    pub fn for_paths(paths: &ConfigPaths, config: &VaultConfig) -> Self {
        let encrypted = EncryptedVault::with_timeout(
            paths.vault_file(),
            timeout_from_minutes(config.auto_lock_minutes),
        );
        Self::new(config.clone(), Arc::new(KeyringSecretStore::new()), encrypted)
    }

    /// Builds a [`VaultSession`] with explicit keyring store and [`EncryptedVault`] (for tests).
    pub fn new(
        config: VaultConfig,
        keyring: Arc<dyn SecretStore>,
        encrypted: EncryptedVault,
    ) -> Self {
        encrypted.set_auto_lock_timeout(timeout_from_minutes(config.auto_lock_minutes));
        Self {
            config: Mutex::new(config),
            keyring,
            encrypted,
        }
    }

    /// Returns the currently active [`VaultBackend`].
    pub fn backend(&self) -> VaultBackend {
        self.config.lock().expect("vault config mutex poisoned").backend
    }

    /// Returns the configured auto-lock timeout in minutes (`0` disables auto-lock).
    pub fn auto_lock_minutes(&self) -> u64 {
        self.config
            .lock()
            .expect("vault config mutex poisoned")
            .auto_lock_minutes
    }

    /// Applies updated non-secret [`VaultConfig`] settings (e.g. from TOML hot-reload).
    pub fn apply_config(&self, next: &VaultConfig) {
        let mut guard = self.config.lock().expect("vault config mutex poisoned");
        let switched_away_from_encrypted = guard.backend == VaultBackend::EncryptedFile
            && next.backend != VaultBackend::EncryptedFile;
        *guard = next.clone();
        drop(guard);

        if switched_away_from_encrypted {
            self.encrypted.lock();
        }
        self.encrypted
            .set_auto_lock_timeout(timeout_from_minutes(next.auto_lock_minutes));
    }

    /// Returns `true` if `EncryptedFile` backend is active and currently locked.
    pub fn requires_unlock(&self) -> bool {
        self.backend() == VaultBackend::EncryptedFile && !self.encrypted.is_unlocked()
    }

    /// Returns `true` if secrets can currently be accessed without unlocking.
    pub fn is_unlocked(&self) -> bool {
        match self.backend() {
            VaultBackend::Keyring => true,
            VaultBackend::EncryptedFile => self.encrypted.is_unlocked(),
        }
    }

    /// Returns `true` if a local encrypted `Vault` file exists on disk.
    pub fn has_encrypted_vault(&self) -> bool {
        self.encrypted.exists()
    }

    /// Initializes and unlocks a fresh local encrypted `Vault` using `master_password`.
    pub fn initialize_encrypted(&self, master_password: &str) -> Result<(), VaultError> {
        self.encrypted.initialize(master_password)
    }

    /// Unlocks the local encrypted `Vault` once for the current process session.
    pub fn unlock(&self, master_password: &str) -> Result<(), VaultError> {
        self.encrypted.unlock(master_password)
    }

    /// Manually locks the local encrypted `Vault` and zeroizes decrypted key material.
    pub fn lock(&self) {
        self.encrypted.lock();
    }

    /// Evaluates idle/sleep timeout and auto-locks if expired. Returns `true` if newly locked.
    pub fn tick_idle(&self) -> bool {
        self.encrypted.tick_idle()
    }
}

impl SecretStore for VaultSession {
    fn get_secret(&self, secret_ref: &str) -> Result<Option<Zeroizing<String>>, VaultError> {
        match self.backend() {
            VaultBackend::Keyring => self.keyring.get_secret(secret_ref),
            VaultBackend::EncryptedFile => self.encrypted.get_secret(secret_ref),
        }
    }

    fn set_secret(&self, secret_ref: &str, secret: &str) -> Result<(), VaultError> {
        match self.backend() {
            VaultBackend::Keyring => self.keyring.set_secret(secret_ref, secret),
            VaultBackend::EncryptedFile => self.encrypted.set_secret(secret_ref, secret),
        }
    }

    fn delete_secret(&self, secret_ref: &str) -> Result<bool, VaultError> {
        match self.backend() {
            VaultBackend::Keyring => self.keyring.delete_secret(secret_ref),
            VaultBackend::EncryptedFile => self.encrypted.delete_secret(secret_ref),
        }
    }
}

fn timeout_from_minutes(minutes: u64) -> Option<Duration> {
    if minutes == 0 {
        None
    } else {
        Some(Duration::from_secs(minutes.saturating_mul(60)))
    }
}

fn derive_vault_key(
    master_password: &str,
    salt: &[u8; SALT_LEN],
    kdf_params: VaultKdfParams,
) -> Result<Zeroizing<VaultKey>, VaultError> {
    kdf_params.validate()?;
    let params = Params::new(
        kdf_params.m_cost_kib,
        kdf_params.t_cost,
        kdf_params.p_cost,
        Some(KEY_LEN),
    )
    .map_err(|err| VaultError::Crypto(err.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = Zeroizing::new(VaultKey([0_u8; KEY_LEN]));
    argon2
        .hash_password_into(master_password.as_bytes(), salt, &mut key.0)
        .map_err(|err| VaultError::Crypto(err.to_string()))?;
    Ok(key)
}

struct ParsedVaultEnvelope<'a> {
    kdf_params: VaultKdfParams,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    header_aad: &'a [u8],
    ciphertext: &'a [u8],
}

fn parse_vault_envelope(raw: &[u8]) -> Result<ParsedVaultEnvelope<'_>, VaultError> {
    if raw.len() < HEADER_LEN {
        return Err(VaultError::InvalidFormat(
            "encrypted Vault file is truncated".to_string(),
        ));
    }
    if raw[0..8] != VAULT_MAGIC {
        return Err(VaultError::InvalidFormat(
            "unexpected Vault magic header".to_string(),
        ));
    }
    let version = u32::from_le_bytes(raw[8..12].try_into().expect("4-byte version slice"));
    if version != VAULT_FORMAT_VERSION {
        return Err(VaultError::InvalidFormat(format!(
            "unsupported Vault format version {version}"
        )));
    }
    let m_cost_kib = u32::from_le_bytes(raw[12..16].try_into().expect("4-byte m_cost slice"));
    let t_cost = u32::from_le_bytes(raw[16..20].try_into().expect("4-byte t_cost slice"));
    let p_cost = u32::from_le_bytes(raw[20..24].try_into().expect("4-byte p_cost slice"));
    let kdf_params = VaultKdfParams {
        m_cost_kib,
        t_cost,
        p_cost,
    };
    kdf_params.validate()?;

    let mut salt = [0_u8; SALT_LEN];
    salt.copy_from_slice(&raw[24..24 + SALT_LEN]);

    let mut nonce = [0_u8; NONCE_LEN];
    nonce.copy_from_slice(&raw[24 + SALT_LEN..HEADER_LEN]);

    Ok(ParsedVaultEnvelope {
        kdf_params,
        salt,
        nonce,
        header_aad: &raw[..HEADER_LEN],
        ciphertext: &raw[HEADER_LEN..],
    })
}

fn encode_payload(entries: &BTreeMap<String, Zeroizing<String>>) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let mut buf = Zeroizing::new(Vec::with_capacity(64));
    buf.extend_from_slice(&PAYLOAD_MAGIC);
    let count = u32::try_from(entries.len())
        .map_err(|_| VaultError::InvalidFormat("too many entries in Vault".to_string()))?;
    buf.extend_from_slice(&count.to_le_bytes());

    for (key, value) in entries {
        let key_len = u32::try_from(key.len())
            .map_err(|_| VaultError::InvalidFormat("secret_ref too long".to_string()))?;
        let val_len = u32::try_from(value.len())
            .map_err(|_| VaultError::InvalidFormat("secret value too long".to_string()))?;
        buf.extend_from_slice(&key_len.to_le_bytes());
        buf.extend_from_slice(key.as_bytes());
        buf.extend_from_slice(&val_len.to_le_bytes());
        buf.extend_from_slice(value.as_bytes());
    }
    Ok(buf)
}

fn decode_payload(plaintext: &[u8]) -> Result<BTreeMap<String, Zeroizing<String>>, VaultError> {
    if plaintext.len() < PAYLOAD_MAGIC.len() + 4 || plaintext[0..3] != PAYLOAD_MAGIC {
        return Err(VaultError::InvalidFormat(
            "corrupted Vault payload header".to_string(),
        ));
    }
    let mut offset = PAYLOAD_MAGIC.len();
    let count = u32::from_le_bytes(
        plaintext[offset..offset + 4]
            .try_into()
            .expect("4-byte entry count"),
    ) as usize;
    offset += 4;

    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let key_len = read_u32_len(plaintext, &mut offset)?;
        let key_bytes = read_slice(plaintext, &mut offset, key_len)?;
        let key = std::str::from_utf8(key_bytes)
            .map_err(|_| VaultError::InvalidFormat("non-UTF-8 secret_ref in Vault".to_string()))?
            .to_string();

        let val_len = read_u32_len(plaintext, &mut offset)?;
        let val_bytes = read_slice(plaintext, &mut offset, val_len)?;
        let value = std::str::from_utf8(val_bytes)
            .map_err(|_| VaultError::InvalidFormat("non-UTF-8 secret in Vault".to_string()))?;
        entries.insert(key, Zeroizing::new(value.to_string()));
    }

    if offset != plaintext.len() {
        return Err(VaultError::InvalidFormat(
            "trailing bytes in decrypted Vault payload".to_string(),
        ));
    }

    Ok(entries)
}

fn read_u32_len(buf: &[u8], offset: &mut usize) -> Result<usize, VaultError> {
    let slice = buf
        .get(*offset..*offset + 4)
        .ok_or_else(|| VaultError::InvalidFormat("truncated Vault entry length".to_string()))?;
    *offset += 4;
    Ok(u32::from_le_bytes(slice.try_into().expect("4-byte len")) as usize)
}

fn read_slice<'a>(buf: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], VaultError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| VaultError::InvalidFormat("Vault entry length overflow".to_string()))?;
    let slice = buf
        .get(*offset..end)
        .ok_or_else(|| VaultError::InvalidFormat("truncated Vault entry data".to_string()))?;
    *offset = end;
    Ok(slice)
}

fn persist_encrypted_vault(
    path: &Path,
    key: &VaultKey,
    kdf_params: VaultKdfParams,
    salt: &[u8; SALT_LEN],
    entries: &BTreeMap<String, Zeroizing<String>>,
) -> Result<(), VaultError> {
    let parent = path.parent().ok_or_else(|| VaultError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Vault path has no parent directory",
        ),
    })?;
    fs::create_dir_all(parent).map_err(|source| VaultError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let mut nonce_bytes = [0_u8; NONCE_LEN];
    rand::rng().fill(&mut nonce_bytes);

    let mut header = [0_u8; HEADER_LEN];
    header[0..8].copy_from_slice(&VAULT_MAGIC);
    header[8..12].copy_from_slice(&VAULT_FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&kdf_params.m_cost_kib.to_le_bytes());
    header[16..20].copy_from_slice(&kdf_params.t_cost.to_le_bytes());
    header[20..24].copy_from_slice(&kdf_params.p_cost.to_le_bytes());
    header[24..24 + SALT_LEN].copy_from_slice(salt);
    header[24 + SALT_LEN..HEADER_LEN].copy_from_slice(&nonce_bytes);

    let plaintext = encode_payload(entries)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key.0).map_err(|err| VaultError::Crypto(err.to_string()))?;
    let nonce = Nonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_slice(),
                aad: &header,
            },
        )
        .map_err(|err| VaultError::Crypto(err.to_string()))?;

    let mut envelope = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    envelope.extend_from_slice(&header);
    envelope.extend_from_slice(&ciphertext);

    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| VaultError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600)).map_err(|source| {
            VaultError::Io {
                path: tmp.path().to_path_buf(),
                source,
            }
        })?;
    }
    tmp.write_all(&envelope).map_err(|source| VaultError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.as_file().sync_all().map_err(|source| VaultError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.persist(path).map_err(|err| VaultError::Io {
        path: path.to_path_buf(),
        source: err.error,
    })?;

    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

pub(crate) fn validate_secret_ref(secret_ref: &str) -> Result<(), VaultError> {
    if secret_ref.trim().is_empty() {
        return Err(VaultError::InvalidSecretRef(
            "secret_ref must not be empty".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroize_clears_vault_key_and_unlocked_entries() {
        let mut state = UnlockedVaultState {
            key: Zeroizing::new(VaultKey([0xA5; KEY_LEN])),
            kdf_params: VaultKdfParams::default(),
            salt: [0x5A; SALT_LEN],
            entries: BTreeMap::from([(
                "ssh/prod".to_string(),
                Zeroizing::new("secret-pass-value".to_string()),
            )]),
            last_activity: ClockSample::now(),
        };

        state.zeroize_and_clear();
        assert_eq!(state.key.0, [0_u8; KEY_LEN]);
        assert_eq!(state.salt, [0_u8; SALT_LEN]);
        assert!(state.entries.is_empty());
    }
}
