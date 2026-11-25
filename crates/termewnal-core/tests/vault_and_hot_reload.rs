use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigPaths, Profile, ProfilesConfig, VaultBackend, VaultConfig, load_config,
    save_config, save_profiles,
    hot_reload::{ConfigFileKind, ConfigReloader, ConfigWatchEvent, ConfigWatcher},
    vault::{
        EncryptedVault, InMemorySecretStore, KeyringSecretStore, ManualVaultClock, SecretStore,
        VaultError, VaultSession,
    },
};

#[test]
fn slice1_secret_store_trait_isolates_passwords_via_keyring_and_in_memory_doubles() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    assert_eq!(paths.vault_file().file_name().unwrap(), "vault.enc");

    let cfg = AppConfig::default();
    assert_eq!(
        cfg.vault,
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        }
    );

    let mem = InMemorySecretStore::new();
    assert!(mem.get_secret("ssh/prod-bastion").unwrap().is_none());
    mem.set_secret("ssh/prod-bastion", "s3cr3t-pass")
        .expect("store password in InMemorySecretStore");
    assert_eq!(
        mem.get_secret("ssh/prod-bastion")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("s3cr3t-pass")
    );
    assert!(mem.delete_secret("ssh/prod-bastion").unwrap());
    assert!(!mem.delete_secret("ssh/prod-bastion").unwrap());
    assert!(mem.get_secret("ssh/prod-bastion").unwrap().is_none());

    let mock_keyring_store =
        keyring_core::mock::Store::new().expect("create keyring-core in-memory mock store");
    let keyring = KeyringSecretStore::with_credential_store("io.termewnal.test", mock_keyring_store);
    assert!(keyring.get_secret("ssh/key-passphrase").unwrap().is_none());
    keyring
        .set_secret("ssh/key-passphrase", "passphrase-123")
        .expect("store passphrase via KeyringSecretStore");
    assert_eq!(
        keyring
            .get_secret("ssh/key-passphrase")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("passphrase-123")
    );
    assert!(keyring.delete_secret("ssh/key-passphrase").unwrap());
    assert!(!keyring.delete_secret("ssh/key-passphrase").unwrap());
    assert!(keyring.get_secret("ssh/key-passphrase").unwrap().is_none());
}

#[test]
fn slice2_encrypted_vault_uses_argon2id_aes_gcm_and_rejects_wrong_password_or_tamper() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    let vault_path = paths.vault_file();

    let vault = EncryptedVault::open(&vault_path);
    assert!(!vault.exists());
    assert!(!vault.is_unlocked());
    assert!(matches!(vault.unlock("anything"), Err(VaultError::NotInitialized)));
    assert!(matches!(
        vault.initialize(""),
        Err(VaultError::EmptyMasterPassword)
    ));

    vault
        .initialize("correct-horse-battery-staple")
        .expect("initialize local encrypted vault");
    assert!(vault.exists());
    assert!(vault.is_unlocked());

    vault
        .set_secret("ssh/prod", "db-secret-99!")
        .expect("write password into encrypted vault");
    vault
        .set_secret("keys/id_ed25519", "key-passphrase-xyz")
        .expect("write key passphrase into encrypted vault");

    let disk_bytes = std::fs::read(&vault_path).expect("read encrypted vault file");
    assert!(disk_bytes.starts_with(b"TMNVLT01"));
    assert!(
        !disk_bytes
            .windows(b"db-secret-99!".len())
            .any(|w| w == b"db-secret-99!"),
        "plaintext secret must never appear on disk"
    );
    assert!(
        !disk_bytes
            .windows(b"correct-horse-battery-staple".len())
            .any(|w| w == b"correct-horse-battery-staple"),
        "master password must never appear on disk"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&vault_path)
            .expect("vault metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "vault file permissions must be 0600");
    }

    // Manual lock prevents further reads/writes until re-unlocked.
    vault.lock();
    assert!(!vault.is_unlocked());
    assert!(matches!(
        vault.get_secret("ssh/prod"),
        Err(VaultError::Locked)
    ));
    assert!(matches!(
        vault.set_secret("ssh/prod", "new"),
        Err(VaultError::Locked)
    ));
    assert!(matches!(
        vault.delete_secret("ssh/prod"),
        Err(VaultError::Locked)
    ));

    // Re-open in a fresh EncryptedVault instance simulating a new process session.
    let reopened = EncryptedVault::open(&vault_path);
    assert!(matches!(
        reopened.unlock("wrong-password"),
        Err(VaultError::InvalidMasterPassword)
    ));
    assert!(!reopened.is_unlocked());

    reopened
        .unlock("correct-horse-battery-staple")
        .expect("unlock with valid master password");
    assert!(reopened.is_unlocked());
    assert_eq!(
        reopened
            .get_secret("ssh/prod")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("db-secret-99!")
    );
    assert_eq!(
        reopened
            .get_secret("keys/id_ed25519")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("key-passphrase-xyz")
    );
    assert!(reopened.delete_secret("keys/id_ed25519").unwrap());
    assert!(reopened.get_secret("keys/id_ed25519").unwrap().is_none());

    // Tampering ciphertext must fail AES-GCM authentication tag check.
    let mut tampered = std::fs::read(&vault_path).expect("read vault for tamper");
    let last_idx = tampered.len() - 1;
    tampered[last_idx] ^= 0xFF;
    std::fs::write(&vault_path, &tampered).expect("write tampered vault");

    let tampered_vault = EncryptedVault::open(&vault_path);
    assert!(matches!(
        tampered_vault.unlock("correct-horse-battery-staple"),
        Err(VaultError::InvalidMasterPassword)
    ));
}

#[test]
fn slice2_vault_session_unlocks_once_and_auto_locks_on_idle_or_sleep() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    let clock = Arc::new(ManualVaultClock::new());
    let keyring = Arc::new(InMemorySecretStore::new());
    keyring
        .set_secret("keyring/entry", "from-keyring")
        .expect("seed keyring secret");

    let encrypted = EncryptedVault::with_clock(
        paths.vault_file(),
        Some(Duration::from_secs(30 * 60)),
        clock.clone(),
    );
    let session = VaultSession::new(
        VaultConfig {
            backend: VaultBackend::EncryptedFile,
            auto_lock_minutes: 30,
        },
        keyring,
        encrypted,
    );

    assert!(session.requires_unlock());
    session
        .initialize_encrypted("master-pass-1")
        .expect("initialize vault session");
    assert!(!session.requires_unlock());
    assert!(session.is_unlocked());

    session
        .set_secret("ssh/db", "vault-secret-value")
        .expect("store secret in unlocked session");

    // Idle 29m59s -> stays unlocked and activity refreshes the idle timer.
    clock.advance_idle(Duration::from_secs(29 * 60 + 59));
    assert!(!session.tick_idle());
    assert!(session.is_unlocked());
    assert_eq!(
        session
            .get_secret("ssh/db")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("vault-secret-value")
    );

    // Another 29m59s after last activity -> still unlocked because get_secret refreshed activity.
    clock.advance_idle(Duration::from_secs(29 * 60 + 59));
    assert!(session.is_unlocked());

    // Advance 1s more -> hits 30m idle threshold -> auto-locks!
    clock.advance_idle(Duration::from_secs(1));
    assert!(session.tick_idle());
    assert!(!session.is_unlocked());
    assert!(session.requires_unlock());
    assert!(matches!(
        session.get_secret("ssh/db"),
        Err(VaultError::Locked)
    ));

    // Re-unlock once, then simulate OS sleep where monotonic time pauses while wall clock jumps 31m.
    session
        .unlock("master-pass-1")
        .expect("re-unlock after idle lock");
    assert!(session.is_unlocked());
    clock.advance_sleep(Duration::from_secs(31 * 60));
    assert!(!session.is_unlocked());
    assert!(matches!(
        session.get_secret("ssh/db"),
        Err(VaultError::Locked)
    ));

    // Switching backend via apply_config respects Keyring vs EncryptedFile cleanly.
    session.apply_config(&VaultConfig {
        backend: VaultBackend::Keyring,
        auto_lock_minutes: 15,
    });
    assert!(!session.requires_unlock());
    assert_eq!(
        session
            .get_secret("keyring/entry")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("from-keyring")
    );
}

#[test]
fn slice3_hot_reload_retains_valid_config_on_malformed_toml_never_overwrites_and_restores_bak1() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    // Save v1 then v2 so `.bak.1` holds v1 for both config.toml and profiles.toml.
    let mut cfg_v1 = AppConfig::default();
    cfg_v1.terminal.scrollback_lines = 12_000;
    cfg_v1.terminal.theme = "Nord".to_string();
    save_config(&paths, &cfg_v1).expect("save config v1");

    let mut cfg_v2 = AppConfig::default();
    cfg_v2.terminal.scrollback_lines = 24_000;
    cfg_v2.terminal.theme = "Dracula".to_string();
    save_config(&paths, &cfg_v2).expect("save config v2");

    let profiles_v1 = ProfilesConfig::default();
    save_profiles(&paths, &profiles_v1).expect("save profiles v1");

    let mut profiles_v2 = ProfilesConfig::default();
    profiles_v2
        .profiles
        .push(Profile::local_shell("Bastion", None));
    save_profiles(&paths, &profiles_v2).expect("save profiles v2");

    let mut reloader = ConfigReloader::load(paths.clone()).expect("load initial reloader");
    assert_eq!(reloader.config().terminal.scrollback_lines, 24_000);
    assert_eq!(reloader.profiles().profiles.len(), 1);

    // Valid external edit applies cleanly.
    let mut cfg_v3 = cfg_v2.clone();
    cfg_v3.terminal.scrollback_lines = 36_000;
    cfg_v3.terminal.theme = "TokyoNight".to_string();
    std::fs::write(
        paths.config_file(),
        toml::to_string_pretty(&cfg_v3).unwrap(),
    )
    .expect("write external valid config.toml");

    let changed = reloader
        .reload_file(ConfigFileKind::AppConfig)
        .expect("valid external config edit should reload");
    assert!(changed);
    assert_eq!(reloader.config().terminal.scrollback_lines, 36_000);
    assert_eq!(reloader.config().terminal.theme, "TokyoNight");
    assert!(reloader.error_for(ConfigFileKind::AppConfig).is_none());

    // Malformed external edit keeps last valid in-memory config and preserves broken file on disk.
    let broken_toml = "schema_version = 1\n[terminal]\nscrollback_lines = \"not-a-number\"\n";
    std::fs::write(paths.config_file(), broken_toml).expect("write malformed config.toml");

    let err = reloader
        .reload_file(ConfigFileKind::AppConfig)
        .expect_err("malformed TOML must fail reload");
    assert_eq!(err.kind, ConfigFileKind::AppConfig);
    assert_eq!(err.line, Some(3));
    assert!(err.column.is_some());
    assert!(!err.message.is_empty());
    assert!(err.backup_available);

    assert_eq!(reloader.config().terminal.scrollback_lines, 36_000);
    assert_eq!(reloader.config().terminal.theme, "TokyoNight");
    assert_eq!(
        std::fs::read_to_string(paths.config_file()).unwrap(),
        broken_toml,
        "broken external edit must NEVER be overwritten automatically"
    );
    assert_eq!(
        reloader.error_for(ConfigFileKind::AppConfig),
        Some(&err)
    );

    // 1-click restore from `.bak.1` recovers v1 cleanly and clears the error without corrupting `.bak.1`.
    reloader
        .restore_backup(ConfigFileKind::AppConfig)
        .expect("restore config.toml from .bak.1");
    assert_eq!(reloader.config().terminal.scrollback_lines, 12_000);
    assert_eq!(reloader.config().terminal.theme, "Nord");
    assert!(reloader.error_for(ConfigFileKind::AppConfig).is_none());
    assert_eq!(load_config(&paths).unwrap().terminal.scrollback_lines, 12_000);

    // Malformed profiles.toml retains last valid profiles and clears once user fixes file.
    let broken_profiles = "schema_version = 1\n[[profiles]\n";
    std::fs::write(paths.profiles_file(), broken_profiles).expect("write malformed profiles.toml");
    let prof_err = reloader
        .reload_file(ConfigFileKind::Profiles)
        .expect_err("malformed profiles.toml must fail reload");
    assert_eq!(prof_err.kind, ConfigFileKind::Profiles);
    assert_eq!(prof_err.line, Some(2));
    assert!(prof_err.backup_available);
    assert_eq!(reloader.profiles().profiles.len(), 1);
    assert_eq!(
        std::fs::read_to_string(paths.profiles_file()).unwrap(),
        broken_profiles
    );

    // Fixing profiles.toml externally reloads and clears error.
    let mut profiles_v3 = profiles_v2.clone();
    profiles_v3
        .profiles
        .push(Profile::local_shell("Secondary", None));
    std::fs::write(
        paths.profiles_file(),
        toml::to_string_pretty(&profiles_v3).unwrap(),
    )
    .expect("fix profiles.toml");
    assert!(
        reloader
            .reload_file(ConfigFileKind::Profiles)
            .expect("fixed profiles.toml should reload")
    );
    assert_eq!(reloader.profiles().profiles.len(), 2);
    assert!(reloader.error_for(ConfigFileKind::Profiles).is_none());
}

#[test]
fn slice3_config_watcher_detects_atomic_and_direct_external_edits_in_config_dir() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    save_config(&paths, &AppConfig::default()).expect("create initial config.toml");
    save_profiles(&paths, &ProfilesConfig::default()).expect("create initial profiles.toml");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create tokio runtime");

    rt.block_on(async {
        let (_watcher, mut rx) =
            ConfigWatcher::start_with_debounce(paths.clone(), Duration::from_millis(40))
                .expect("start ConfigWatcher");

        // Give OS watcher brief settle time before writing.
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Atomic save (new inode via NamedTempFile::persist) on config.toml.
        let mut updated = AppConfig::default();
        updated.terminal.scrollback_lines = 42_000;
        save_config(&paths, &updated).expect("atomic save config.toml");

        let mut saw_config = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !saw_config {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ConfigWatchEvent::Changed(kinds))) => {
                    if kinds.contains(&ConfigFileKind::AppConfig) {
                        saw_config = true;
                    }
                }
                Ok(Some(ConfigWatchEvent::Rescan)) => {
                    saw_config = true;
                }
                Ok(Some(ConfigWatchEvent::Error(err))) => panic!("watcher error: {err}"),
                Ok(None) => panic!("watcher channel closed unexpectedly"),
                Err(_) => break,
            }
        }
        assert!(saw_config, "expected ConfigWatcher to detect atomic save of config.toml");

        // Direct in-place write on profiles.toml.
        let mut updated_profiles = ProfilesConfig::default();
        updated_profiles
            .profiles
            .push(Profile::local_shell("Watched", None));
        std::fs::write(
            paths.profiles_file(),
            toml::to_string_pretty(&updated_profiles).unwrap(),
        )
        .expect("direct write profiles.toml");

        let mut saw_profiles = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !saw_profiles {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ConfigWatchEvent::Changed(kinds))) => {
                    if kinds.contains(&ConfigFileKind::Profiles) {
                        saw_profiles = true;
                    }
                }
                Ok(Some(ConfigWatchEvent::Rescan)) => {
                    saw_profiles = true;
                }
                Ok(Some(ConfigWatchEvent::Error(err))) => panic!("watcher error: {err}"),
                Ok(None) => panic!("watcher channel closed unexpectedly"),
                Err(_) => break,
            }
        }
        assert!(saw_profiles, "expected ConfigWatcher to detect direct write to profiles.toml");
    });
}
