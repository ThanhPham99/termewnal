use std::fs;
use std::sync::Arc;

use iced::window;
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigPaths, EncryptedVault, InMemorySecretStore, ProfileTarget, SecretStore,
    SshAuthMethod, TabbyCredentialKind, TabbyVaultStatus, TunnelDirection, VaultBackend,
    VaultConfig, VaultSession, load_profiles, save_config,
};
use termewnal_ui::{LaunchDisposition, Message};

#[test]
fn slice3a_sidebar_loads_readonly_ssh_config_launches_sessions_and_clones_profiles() {
    let temp = TempDir::new().expect("create temp root");
    let ssh_dir = temp.path().join(".ssh");
    let conf_d = ssh_dir.join("conf.d");
    fs::create_dir_all(&conf_d).expect("create .ssh/conf.d");

    fs::write(
        conf_d.join("team.conf"),
        "Host team-db\n    HostName 10.60.0.4\n    User dbuser\n",
    )
    .expect("write included conf");

    let ssh_config_path = ssh_dir.join("config");
    let ssh_config_raw = "\
Include conf.d/*.conf

Host bastion-ro
    HostName 203.0.113.10
    User jump
    Port 2222
    IdentityFile ~/.ssh/id_bastion

Host app-ro
    HostName 10.50.0.12
    User ops
    ProxyJump bastion-ro

Host *
    ServerAliveInterval 15
    IdentityFile ~/.ssh/id_default
";
    fs::write(&ssh_config_path, ssh_config_raw).expect("write ~/.ssh/config");
    let orig_mtime = fs::metadata(&ssh_config_path).unwrap().modified().unwrap();

    let paths = ConfigPaths::from_root(temp.path());
    let mut state = super::common::boot_test_app(paths.clone()).expect("boot AppState");

    let aliases: Vec<&str> = state
        .sidebar()
        .system_ssh_profiles()
        .iter()
        .map(|h| h.alias.as_str())
        .collect();
    assert_eq!(aliases, vec!["team-db", "bastion-ro", "app-ro"]);

    // Command Palette also exposes parsed System SSH entries.
    let palette_items = state.command_palette_items();
    assert!(
        palette_items
            .iter()
            .any(|item| item.title == "Launch System SSH: app-ro")
    );

    // Launch System SSH directly into an SSH Session slot preserving ProxyJump & IdentityFile.
    let _ = state.update(Message::LaunchSystemSsh {
        alias: "app-ro".to_string(),
        disposition: LaunchDisposition::NewTab,
    });
    let ssh_sid = state.active_session_id().expect("ssh session active");
    let slot = state.ssh_session(ssh_sid).expect("ssh session slot");
    assert_eq!(slot.target().host, "10.50.0.12");
    assert_eq!(slot.target().port, 22);
    assert_eq!(slot.target().username, "ops");
    assert_eq!(slot.target().proxy_jump, vec!["bastion-ro".to_string()]);
    assert_eq!(
        slot.target().auth,
        SshAuthMethod::PublicKey {
            key_path: Some(ssh_dir.join("id_default")),
            vault_secret_ref: None,
        }
    );

    // 1-click Clone to Profile persists an editable Profile into profiles.toml without touching ~/.ssh/config.
    let _ = state.update(Message::CloneSystemSshToProfile("bastion-ro".to_string()));
    assert_eq!(state.profiles().profiles.len(), 1);
    let cloned = &state.profiles().profiles[0];
    assert_eq!(cloned.name, "bastion-ro");
    assert_eq!(
        cloned.target,
        ProfileTarget::Ssh {
            host: "203.0.113.10".to_string(),
            port: 2222,
            username: "jump".to_string(),
            auth: SshAuthMethod::PublicKey {
                key_path: Some(ssh_dir.join("id_bastion")),
                vault_secret_ref: None,
            },
            proxy_jump: Vec::new(),
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
        }
    );

    let persisted = load_profiles(&paths).expect("load persisted profiles.toml");
    assert_eq!(persisted.profiles.len(), 1);
    assert_eq!(persisted.profiles[0].id, cloned.id);

    assert_eq!(fs::read_to_string(&ssh_config_path).unwrap(), ssh_config_raw);
    assert_eq!(
        fs::metadata(&ssh_config_path).unwrap().modified().unwrap(),
        orig_mtime
    );

    // Render sidebar & view cleanly without panics.
    let _ = state.update(Message::SetSidebarOpen(true));
    let win = window::Id::unique();
    drop(state.view(win));
}

#[test]
fn slice3b_command_palette_tabby_importer_migrates_profiles_and_prompts_for_locked_vault() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    let mem_store = Arc::new(InMemorySecretStore::new());
    let vault = Arc::new(VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        mem_store.clone(),
        EncryptedVault::open(paths.vault_file()),
    ));

    let mut state =
        super::common::boot_test_app_with_vault(paths.clone(), Arc::clone(&vault)).expect("boot AppState");
    let win = window::Id::unique();

    // 1. Trigger Tabby Importer from Command Palette and close via EscapePressed.
    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("tabby".to_string()));
    let items = state.command_palette_items();
    assert!(!items.is_empty());
    assert!(items[0].title.contains("Import Tabby"));

    let _ = state.update(Message::CommandPaletteConfirm);
    assert!(!state.command_palette().is_open());
    assert!(state.tabby_import().is_open());
    drop(state.view(win));

    let _ = state.update(Message::EscapePressed);
    assert!(!state.tabby_import().is_open());

    // 2. Open via sidebar button message; missing file surfaces inline error without modifying profiles.toml.
    let _ = state.update(Message::OpenTabbyImportModal);
    assert!(state.tabby_import().is_open());
    let missing_path = temp.path().join("does-not-exist.yaml");
    let _ = state.update(Message::TabbyImportPathChanged(
        missing_path.to_string_lossy().into_owned(),
    ));
    let _ = state.update(Message::SubmitTabbyImport);
    assert!(state.tabby_import().is_open());
    assert!(state.tabby_import().import_error().is_some());
    assert!(state.profiles().profiles.is_empty());

    // 3. Import a Tabby config.yaml with locked vault, group, tags, tunnel, and plugin blocks.
    let tabby_yaml_path = temp.path().join("tabby-config.yaml");
    let tabby_yaml = r#"
version: 7
vault:
  version: 1
  encrypt: true
  contents: "LOCKED_TABBY_VAULT_PAYLOAD"
groups:
  - id: grp-prod
    name: Production
profiles:
  - id: tabby-db
    type: ssh
    name: Prod DB
    group: grp-prod
    tags: [db, prod]
    options:
      host: db.prod.internal
      port: 2222
      user: dbadmin
      auth: password
      forwardedPorts:
        - type: Local
          host: 127.0.0.1
          port: 5432
          targetAddress: 127.0.0.1
          targetPort: 5432
          description: Postgres
  - id: tabby-sh
    type: local
    name: Dev Shell
    tags: [dev]
    options:
      command: /bin/sh
appearance:
  theme: TokyoNight
docker:
  enabled: true
"#;
    fs::write(&tabby_yaml_path, tabby_yaml).expect("write tabby config.yaml");

    let _ = state.update(Message::TabbyImportPathChanged(
        tabby_yaml_path.to_string_lossy().into_owned(),
    ));
    let _ = state.update(Message::SubmitTabbyImport);
    assert_eq!(state.tabby_import().import_error(), None);

    let summary = state
        .tabby_import()
        .summary()
        .expect("tabby import summary recorded");
    assert_eq!(summary.added_count, 2);
    assert_eq!(summary.imported_tunnel_count, 1);
    assert!(matches!(summary.vault_status, TabbyVaultStatus::Locked { .. }));
    assert!(summary.vault_notice.is_some());

    // Profiles and tunnels migrated into state and profiles.toml.
    assert_eq!(state.profiles().profiles.len(), 2);
    let prod_db = state
        .profiles()
        .profiles
        .iter()
        .find(|p| p.name == "Prod DB")
        .expect("Prod DB imported");
    assert_eq!(prod_db.group.as_deref(), Some("Production"));
    assert_eq!(prod_db.tags, vec!["db", "prod"]);
    assert_eq!(prod_db.tunnels.len(), 1);
    assert_eq!(prod_db.tunnels[0].direction, TunnelDirection::Local);
    assert_eq!(prod_db.tunnels[0].bind_port, 5432);
    assert!(matches!(
        prod_db.target,
        ProfileTarget::Ssh {
            auth: SshAuthMethod::Password {
                vault_secret_ref: Some(_)
            },
            ..
        }
    ));

    let prod_db_id = prod_db.id.clone();
    let persisted = load_profiles(&paths).expect("load persisted profiles.toml");
    assert_eq!(persisted.profiles.len(), 2);

    // 4. Locked Tabby vault prompts cleanly to save credential into Termewnal's Vault.
    let rows = state.tabby_import().credential_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].profile_id, prod_db_id);
    assert_eq!(rows[0].kind, TabbyCredentialKind::Password);
    assert!(!rows[0].saved);
    let secret_ref = rows[0].secret_ref.clone();

    drop(state.view(win));

    let _ = state.update(Message::TabbyVaultCredentialChanged {
        profile_id: prod_db_id.clone(),
        value: "migrated-db-pw".to_string(),
    });
    let _ = state.update(Message::SaveTabbyVaultCredential(prod_db_id));

    let updated_row = &state.tabby_import().credential_rows()[0];
    assert!(updated_row.saved);
    assert!(updated_row.secret_draft.is_empty());
    assert_eq!(updated_row.error, None);
    assert_eq!(
        mem_store
            .get_secret(&secret_ref)
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("migrated-db-pw")
    );

    let raw_toml = fs::read_to_string(paths.profiles_file()).expect("read profiles.toml");
    assert!(!raw_toml.contains("migrated-db-pw"));
    assert!(!raw_toml.contains("LOCKED_TABBY_VAULT_PAYLOAD"));

    let _ = state.update(Message::CloseTabbyImportModal);
    assert!(!state.tabby_import().is_open());
}

#[test]
fn slice3c_locked_encrypted_vault_defers_tabby_secret_and_reload_command_picks_up_ssh_edits() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut cfg = AppConfig::default();
    cfg.vault = VaultConfig {
        backend: VaultBackend::EncryptedFile,
        auto_lock_minutes: 30,
    };
    save_config(&paths, &cfg).expect("save config with EncryptedFile vault");
    let mem_store = Arc::new(InMemorySecretStore::new());
    let vault = Arc::new(VaultSession::new(
        cfg.vault,
        mem_store,
        EncryptedVault::open(paths.vault_file()),
    ));

    let mut state =
        super::common::boot_test_app_with_vault(paths.clone(), Arc::clone(&vault)).expect("boot AppState");

    let tabby_yaml_path = temp.path().join("tabby-locked-termewnal-vault.yaml");
    fs::write(
        &tabby_yaml_path,
        r#"
vault:
  encrypt: true
  contents: "LOCKED"
profiles:
  - id: p1
    type: ssh
    name: Vaulted Host
    options:
      host: 10.0.0.44
      user: admin
      auth: password
"#,
    )
    .unwrap();

    let _ = state.update(Message::ImportTabbyConfigFromPath(tabby_yaml_path));
    assert!(state.tabby_import().is_open());
    let pid = state.tabby_import().credential_rows()[0].profile_id.clone();
    let secret_ref = state.tabby_import().credential_rows()[0].secret_ref.clone();

    // Attempt to save while Termewnal's EncryptedFile Vault is still locked.
    let _ = state.update(Message::TabbyVaultCredentialChanged {
        profile_id: pid.clone(),
        value: "vaulted-pw-123".to_string(),
    });
    let _ = state.update(Message::SaveTabbyVaultCredential(pid.clone()));
    assert!(!state.tabby_import().credential_rows()[0].saved);
    assert!(state.tabby_import().credential_rows()[0].error.is_some());

    // Initialize/unlock Termewnal's local Vault, then save succeeds.
    let _ = state.update(Message::VaultInitializeSubmitted("master-pass-123".to_string()));
    let _ = state.update(Message::SaveTabbyVaultCredential(pid));
    assert!(state.tabby_import().credential_rows()[0].saved);
    assert_eq!(
        vault
            .get_secret(&secret_ref)
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("vaulted-pw-123")
    );

    // Reload ~/.ssh/config via Command Palette picks up new host read-only.
    let ssh_dir = temp.path().join(".ssh");
    fs::create_dir_all(&ssh_dir).unwrap();
    fs::write(
        ssh_dir.join("config"),
        "Host edge-gw\n    HostName 192.0.2.55\n    User netops\n",
    )
    .unwrap();

    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("reload ~/.ssh".to_string()));
    let _ = state.update(Message::CommandPaletteConfirm);
    assert_eq!(state.sidebar().system_ssh_profiles().len(), 1);
    assert_eq!(state.sidebar().system_ssh_profiles()[0].alias, "edge-gw");
}
