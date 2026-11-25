use std::fs;
use std::path::PathBuf;

use std::sync::Arc;

use tempfile::TempDir;
use termewnal_core::{
    ConfigPaths, EncryptedVault, InMemorySecretStore, OpenSshConfigError, ProfileTarget,
    SecretStore, SshAuthMethod, TabbyCredentialKind, TabbyImportError, TabbyVaultStatus,
    TunnelDirection, VaultBackend, VaultConfig, VaultSession, clone_openssh_host_to_profile,
    import_tabby_config_file_into_profiles, load_profiles, load_system_ssh_hosts,
    parse_openssh_config_file, parse_tabby_config_str, save_cloned_openssh_profile,
};

#[test]
fn slice1a_parses_openssh_config_with_inheritance_includes_and_strict_readonly_invariant() {
    let temp = TempDir::new().expect("create temp home");
    let ssh_dir = temp.path().join(".ssh");
    let conf_d = ssh_dir.join("conf.d");
    fs::create_dir_all(&conf_d).expect("create .ssh/conf.d");

    let included_path = conf_d.join("prod.conf");
    let included_content = "\
Host analytics
    HostName analytics.prod.example
    User data
";
    fs::write(&included_path, included_content).expect("write included ssh config");

    let config_path = ssh_dir.join("config");
    let main_content = "\
Include conf.d/*.conf

Host bastion
    HostName 198.51.100.10
    User jump
    Port 2222
    IdentityFile ~/.ssh/id_bastion
    ForwardAgent yes

Host app-prod
    HostName 10.20.0.5
    ProxyJump bastion,ops@10.1.0.2:2202
    LocalForward 5432 127.0.0.1:5432
    DynamicForward 1080
    ControlMaster auto

Host direct-box
    HostName 10.20.0.99
    ProxyJump none

Host db.internal
    HostName 10.30.0.8

Host *.internal
    User internal-ops
    Port 2200
    IdentityFile ~/.ssh/id_internal

Host *
    User default-user
    ProxyJump default-jump
    ServerAliveInterval 15
    IdentityFile ~/.ssh/id_default
";
    fs::write(&config_path, main_content).expect("write ~/.ssh/config fixture");

    let orig_mtime = fs::metadata(&config_path)
        .expect("config metadata")
        .modified()
        .expect("config mtime");
    let orig_inc_mtime = fs::metadata(&included_path)
        .expect("include metadata")
        .modified()
        .expect("include mtime");

    let hosts = parse_openssh_config_file(&config_path, temp.path()).expect("parse ssh config");
    let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(
        aliases,
        vec!["analytics", "bastion", "app-prod", "direct-box", "db.internal"]
    );

    let bastion = hosts.iter().find(|h| h.alias == "bastion").unwrap();
    assert_eq!(bastion.host_name.as_deref(), Some("198.51.100.10"));
    assert_eq!(bastion.user.as_deref(), Some("jump"));
    assert_eq!(bastion.port, 2222);
    assert_eq!(
        bastion.identity_files,
        vec![
            ssh_dir.join("id_bastion"),
            ssh_dir.join("id_default"),
        ]
    );
    assert_eq!(bastion.proxy_jump, vec!["default-jump"]);
    assert_eq!(bastion.forward_agent, Some(true));

    let app = hosts.iter().find(|h| h.alias == "app-prod").unwrap();
    assert_eq!(app.host_name.as_deref(), Some("10.20.0.5"));
    assert_eq!(app.user.as_deref(), Some("default-user"));
    assert_eq!(app.port, 22);
    assert_eq!(app.proxy_jump, vec!["bastion", "ops@10.1.0.2:2202"]);

    let direct = hosts.iter().find(|h| h.alias == "direct-box").unwrap();
    assert!(
        direct.proxy_jump.is_empty(),
        "ProxyJump none must clear inherited jumps: {:?}",
        direct.proxy_jump
    );

    let db = hosts.iter().find(|h| h.alias == "db.internal").unwrap();
    assert_eq!(db.host_name.as_deref(), Some("10.30.0.8"));
    assert_eq!(db.user.as_deref(), Some("internal-ops"));
    assert_eq!(db.port, 2200);
    assert_eq!(
        db.identity_files,
        vec![ssh_dir.join("id_internal"), ssh_dir.join("id_default")]
    );

    let analytics = hosts.iter().find(|h| h.alias == "analytics").unwrap();
    assert_eq!(analytics.host_name.as_deref(), Some("analytics.prod.example"));
    assert_eq!(analytics.user.as_deref(), Some("data"));

    // ConfigPaths integration reads <home>/.ssh/config read-only without mutating files.
    let via_paths = load_system_ssh_hosts(&ConfigPaths::from_root(temp.path()))
        .expect("load system ssh hosts via ConfigPaths");
    assert_eq!(via_paths, hosts);

    assert_eq!(fs::read_to_string(&config_path).unwrap(), main_content);
    assert_eq!(fs::read_to_string(&included_path).unwrap(), included_content);
    assert_eq!(
        fs::metadata(&config_path).unwrap().modified().unwrap(),
        orig_mtime
    );
    assert_eq!(
        fs::metadata(&included_path).unwrap().modified().unwrap(),
        orig_inc_mtime
    );

    // Missing ~/.ssh/config returns empty cleanly.
    let empty_home = TempDir::new().expect("create empty home");
    let empty_hosts = load_system_ssh_hosts(&ConfigPaths::from_root(empty_home.path()))
        .expect("missing ssh config must not error");
    assert!(empty_hosts.is_empty());

    // Malformed syntax returns structured parse error without mutating file.
    let bad_path = empty_home.path().join("bad_ssh_config");
    fs::write(&bad_path, "Host broken\n    HostName \"unclosed\n").unwrap();
    let err = parse_openssh_config_file(&bad_path, empty_home.path())
        .expect_err("unterminated quote must fail");
    assert!(matches!(err, OpenSshConfigError::Parse { path, .. } if path == PathBuf::from(&bad_path)));
}

#[test]
fn slice1b_clones_openssh_hosts_into_editable_profiles_without_touching_ssh_config() {
    let temp = TempDir::new().expect("create temp home");
    let ssh_dir = temp.path().join(".ssh");
    fs::create_dir_all(&ssh_dir).expect("create .ssh");
    let config_path = ssh_dir.join("config");
    let ssh_config_raw = "\
Host app-prod
    HostName 10.20.0.5
    User ops
    Port 2222
    IdentityFile ~/.ssh/id_app
    ProxyJump bastion,jump2:2202

Host agent-box
    HostName 10.20.0.9
";
    fs::write(&config_path, ssh_config_raw).expect("write ~/.ssh/config");
    let orig_mtime = fs::metadata(&config_path).unwrap().modified().unwrap();

    let paths = ConfigPaths::from_root(temp.path());
    let hosts = load_system_ssh_hosts(&paths).expect("load system ssh hosts");
    assert_eq!(hosts.len(), 2);

    let cloned_app = clone_openssh_host_to_profile(&hosts[0], "fallback");
    assert!(!cloned_app.id.is_empty());
    assert_eq!(cloned_app.name, "app-prod");
    assert_eq!(
        cloned_app.target,
        ProfileTarget::Ssh {
            host: "10.20.0.5".to_string(),
            port: 2222,
            username: "ops".to_string(),
            auth: SshAuthMethod::PublicKey {
                key_path: Some(ssh_dir.join("id_app")),
                vault_secret_ref: None,
            },
            proxy_jump: vec!["bastion".to_string(), "jump2:2202".to_string()],
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
        }
    );

    let cloned_agent = clone_openssh_host_to_profile(&hosts[1], "alice");
    assert_eq!(cloned_agent.name, "agent-box");
    assert_eq!(
        cloned_agent.target,
        ProfileTarget::Ssh {
            host: "10.20.0.9".to_string(),
            port: 22,
            username: "alice".to_string(),
            auth: SshAuthMethod::Agent,
            proxy_jump: Vec::new(),
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
        }
    );

    let saved1 = save_cloned_openssh_profile(&paths, &hosts[0], "fallback")
        .expect("save first cloned profile");
    let saved2 = save_cloned_openssh_profile(&paths, &hosts[0], "fallback")
        .expect("save second cloned profile");
    assert_eq!(saved1.name, "app-prod");
    assert_eq!(saved2.name, "app-prod (2)");
    assert_ne!(saved1.id, saved2.id);

    let loaded = load_profiles(&paths).expect("reload profiles.toml");
    assert_eq!(loaded.profiles.len(), 2);
    assert_eq!(loaded.profiles[0], saved1);
    assert_eq!(loaded.profiles[1], saved2);

    // ~/.ssh/config remains strictly untouched.
    assert_eq!(fs::read_to_string(&config_path).unwrap(), ssh_config_raw);
    assert_eq!(
        fs::metadata(&config_path).unwrap().modified().unwrap(),
        orig_mtime
    );
}

#[test]
fn slice2a_imports_tabby_ssh_local_groups_tags_tunnels_and_ignores_plugins() {
    let yaml = r#"
version: 7
groups:
  - id: grp-prod
    name: Production
  - id: grp-db
    name: Databases
    parent: grp-prod
profiles:
  - id: tabby-bastion
    type: ssh
    name: Bastion
    group: grp-prod
    tags: [jump, prod]
    icon: fas fa-server
    options:
      host: bastion.prod.example
      port: 2222
      user: jump
      auth: publicKey
      privateKeys:
        - ~/.ssh/id_bastion
  - id: tabby-app
    type: ssh
    name: Prod API
    group: grp-db
    options:
      host: 10.0.1.50
      user: deploy
      jumpHost: tabby-bastion
      privateKey: ~/.ssh/id_app
      tags: [api]
      forwardedPorts:
        - type: Local
          host: 127.0.0.1
          port: 5432
          targetAddress: 10.0.1.80
          targetPort: 5432
          description: Postgres
        - type: Remote
          host: 0.0.0.0
          port: 8080
          targetAddress: 127.0.0.1
          targetPort: 3000
        - type: Dynamic
          port: 1080
          description: SOCKS5
  - id: tabby-local
    type: local
    name: Zsh Dev
    group: Work
    tags: [local]
    options:
      command: /bin/zsh
      args: ["-l"]
      cwd: /tmp/workspace
  - id: tabby-serial
    type: serial
    name: UART
    options:
      port: /dev/ttyUSB0
  - id: tabby-telnet
    type: telnet
    name: Legacy Switch
    options:
      host: 192.168.1.2
appearance:
  theme: Dracula
terminal:
  fontSize: 15
hotkeys:
  toggle-window:
    - Ctrl-Space
docker:
  socketPath: /var/run/docker.sock
unknownPluginBlock:
  enabled: true
  nested: [1, 2, 3]
"#;

    let report = parse_tabby_config_str(yaml, "fallback").expect("parse tabby yaml");
    assert_eq!(report.imported_ssh_count, 2);
    assert_eq!(report.imported_local_count, 1);
    assert_eq!(report.imported_tunnel_count, 3);
    assert_eq!(report.skipped_unsupported_count, 2);
    assert_eq!(report.vault_status, TabbyVaultStatus::None);
    assert!(report.missing_credentials.is_empty());
    assert_eq!(report.profiles.len(), 3);

    let bastion = &report.profiles[0];
    assert_eq!(bastion.name, "Bastion");
    assert_eq!(bastion.group.as_deref(), Some("Production"));
    assert_eq!(bastion.tags, vec!["jump", "prod"]);
    assert_eq!(
        bastion.target,
        ProfileTarget::Ssh {
            host: "bastion.prod.example".to_string(),
            port: 2222,
            username: "jump".to_string(),
            auth: SshAuthMethod::PublicKey {
                key_path: Some(PathBuf::from("~/.ssh/id_bastion")),
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

    let app = &report.profiles[1];
    assert_eq!(app.name, "Prod API");
    assert_eq!(app.group.as_deref(), Some("Production / Databases"));
    assert_eq!(app.tags, vec!["api"]);
    assert_eq!(
        app.target,
        ProfileTarget::Ssh {
            host: "10.0.1.50".to_string(),
            port: 22,
            username: "deploy".to_string(),
            auth: SshAuthMethod::PublicKey {
                key_path: Some(PathBuf::from("~/.ssh/id_app")),
                vault_secret_ref: None,
            },
            proxy_jump: vec!["jump@bastion.prod.example:2222".to_string()],
            auto_connect_on_startup: false,
            tmux_reattach: None,
            keepalive_interval_secs: None,
            keepalive_max_missed: None,
            auto_attach_mux: false,
            mux_attach_command: None,
        }
    );
    assert_eq!(app.tunnels.len(), 3);
    assert_eq!(app.tunnels[0].name, "Postgres");
    assert_eq!(app.tunnels[0].direction, TunnelDirection::Local);
    assert_eq!(app.tunnels[0].bind_host, "127.0.0.1");
    assert_eq!(app.tunnels[0].bind_port, 5432);
    assert_eq!(app.tunnels[0].target_host.as_deref(), Some("10.0.1.80"));
    assert_eq!(app.tunnels[0].target_port, Some(5432));

    assert_eq!(app.tunnels[1].direction, TunnelDirection::Remote);
    assert_eq!(app.tunnels[1].bind_host, "0.0.0.0");
    assert_eq!(app.tunnels[1].bind_port, 8080);
    assert_eq!(app.tunnels[1].target_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(app.tunnels[1].target_port, Some(3000));

    assert_eq!(app.tunnels[2].name, "SOCKS5");
    assert_eq!(app.tunnels[2].direction, TunnelDirection::Dynamic);
    assert_eq!(app.tunnels[2].bind_host, "127.0.0.1");
    assert_eq!(app.tunnels[2].bind_port, 1080);
    assert_eq!(app.tunnels[2].target_host, None);
    assert_eq!(app.tunnels[2].target_port, None);

    let local = &report.profiles[2];
    assert_eq!(local.name, "Zsh Dev");
    assert_eq!(local.group.as_deref(), Some("Work"));
    assert_eq!(local.tags, vec!["local"]);
    assert_eq!(
        local.target,
        ProfileTarget::LocalShell {
            shell: Some("/bin/zsh".to_string()),
            working_dir: Some(PathBuf::from("/tmp/workspace")),
            args: vec!["-l".to_string()],
        }
    );

    // Invalid YAML reports structured parse error with location.
    let err = parse_tabby_config_str("profiles:\n  - [broken\n", "alice")
        .expect_err("malformed yaml must fail");
    assert!(matches!(err, TabbyImportError::Parse { line: Some(_), .. }));
}

#[test]
fn slice2b_tabby_importer_isolates_secrets_detects_locked_vault_and_is_idempotent() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    // 1. Locked Tabby vault: profiles + tunnels import unconditionally; locked credentials prompt cleanly.
    let locked_yaml = r#"
version: 7
vault:
  version: 1
  encrypt: true
  contents: "AESGCM_LOCKED_BASE64_CIPHERTEXT"
groups:
  - id: g1
    name: Staging
profiles:
  - id: p-pw
    type: ssh
    name: Staging DB
    group: g1
    options:
      host: db.staging.internal
      port: 2222
      user: pgadmin
      auth: password
      forwardedPorts:
        - type: Local
          port: 5432
          targetAddress: 127.0.0.1
          targetPort: 5432
  - id: p-key
    type: ssh
    name: Staging App
    group: g1
    options:
      host: app.staging.internal
      user: deploy
      auth: publicKey
      privateKeys:
        - ~/.ssh/id_staging
  - id: p-agent
    type: ssh
    name: Staging Bastion
    options:
      host: bastion.staging.internal
      user: jump
      auth: agent
"#;
    let locked_yaml_path = temp.path().join("tabby-locked.yaml");
    fs::write(&locked_yaml_path, locked_yaml).unwrap();

    let mem = Arc::new(InMemorySecretStore::new());
    let vault = VaultSession::new(
        VaultConfig {
            backend: VaultBackend::Keyring,
            auto_lock_minutes: 30,
        },
        mem.clone(),
        EncryptedVault::open(paths.vault_file()),
    );

    let outcome = import_tabby_config_file_into_profiles(
        &locked_yaml_path,
        &paths,
        "fallback",
        Some(&vault),
    )
    .expect("import locked tabby config");

    assert_eq!(outcome.added_count, 3);
    assert_eq!(outcome.skipped_duplicate_count, 0);
    assert_eq!(outcome.imported_tunnel_count, 1);
    assert!(matches!(outcome.vault_status, TabbyVaultStatus::Locked { .. }));
    assert_eq!(outcome.missing_credentials.len(), 2);
    assert_eq!(outcome.missing_credentials[0].profile_name, "Staging DB");
    assert_eq!(
        outcome.missing_credentials[0].kind,
        TabbyCredentialKind::Password
    );
    assert_eq!(outcome.missing_credentials[1].profile_name, "Staging App");
    assert_eq!(
        outcome.missing_credentials[1].kind,
        TabbyCredentialKind::KeyPassphrase
    );

    let raw_toml = fs::read_to_string(paths.profiles_file()).expect("read profiles.toml");
    assert!(!raw_toml.contains("AESGCM_LOCKED_BASE64_CIPHERTEXT"));
    assert!(!raw_toml.contains("password ="));

    // Re-importing the exact same Tabby config skips duplicates cleanly.
    let second = import_tabby_config_file_into_profiles(
        &locked_yaml_path,
        &paths,
        "fallback",
        Some(&vault),
    )
    .expect("re-import locked tabby config");
    assert_eq!(second.added_count, 0);
    assert_eq!(second.skipped_duplicate_count, 3);
    assert_eq!(load_profiles(&paths).unwrap().profiles.len(), 3);

    // 2. Unencrypted Tabby export with plaintext password isolates secret into Vault, never TOML.
    let plaintext_yaml = r#"
profiles:
  - id: p-plain
    type: ssh
    name: Plaintext Test Box
    options:
      host: 192.168.99.5
      port: 22
      user: tester
      auth: password
      password: "super-secret-password-99"
"#;
    let plain_yaml_path = temp.path().join("tabby-plain.yaml");
    fs::write(&plain_yaml_path, plaintext_yaml).unwrap();

    let plain_outcome = import_tabby_config_file_into_profiles(
        &plain_yaml_path,
        &paths,
        "fallback",
        Some(&vault),
    )
    .expect("import plaintext tabby config");
    assert_eq!(plain_outcome.added_count, 1);
    assert_eq!(plain_outcome.vault_status, TabbyVaultStatus::None);
    assert!(plain_outcome.missing_credentials.is_empty());

    let imported_plain = plain_outcome
        .updated_profiles
        .profiles
        .iter()
        .find(|p| p.name == "Plaintext Test Box")
        .expect("imported plaintext profile");
    let secret_ref = match &imported_plain.target {
        ProfileTarget::Ssh {
            auth: SshAuthMethod::Password { vault_secret_ref },
            ..
        } => vault_secret_ref.clone().expect("vault_secret_ref set"),
        other => panic!("unexpected target: {other:?}"),
    };
    assert_eq!(
        mem.get_secret(&secret_ref).unwrap().as_deref().map(String::as_str),
        Some("super-secret-password-99")
    );

    let updated_toml = fs::read_to_string(paths.profiles_file()).expect("read updated profiles.toml");
    assert!(!updated_toml.contains("super-secret-password-99"));
}
