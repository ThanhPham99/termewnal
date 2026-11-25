use tempfile::TempDir;
use termewnal_core::{
    AppConfig, CURRENT_SCHEMA_VERSION, ConfigPaths, CoreError, Profile, ProfilesConfig,
    SshAuthMethod, backup_path, ensure_config_files, load_config, load_profiles, save_config,
    save_profiles,
};

#[test]
fn xdg_paths_and_default_configs_use_schema_version_1() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    assert!(paths.config_dir().starts_with(temp.path()));
    assert_eq!(paths.config_file().file_name().unwrap(), "config.toml");
    assert_eq!(paths.profiles_file().file_name().unwrap(), "profiles.toml");

    let xdg = ConfigPaths::detect_xdg().expect("resolve OS XDG directories");
    assert!(!xdg.config_dir().as_os_str().is_empty());
    assert!(!xdg.data_local_dir().as_os_str().is_empty());

    let app_cfg = AppConfig::default();
    let profiles_cfg = ProfilesConfig::default();
    assert_eq!(CURRENT_SCHEMA_VERSION, 1);
    assert_eq!(app_cfg.schema_version, 1);
    assert_eq!(profiles_cfg.schema_version, 1);
}

#[test]
fn save_and_load_round_trip_writes_valid_toml_without_leftover_temp_files() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    let mut cfg = AppConfig::default();
    cfg.terminal.scrollback_lines = 25_000;
    cfg.terminal.theme = "TokyoNight".to_string();
    save_config(&paths, &cfg).expect("save config");

    let mut profiles = ProfilesConfig::default();
    profiles.profiles.push(Profile::local_shell(
        "Default Shell",
        Some("/bin/zsh".to_string()),
    ));
    profiles.profiles.push(Profile::ssh(
        "Prod Bastion",
        "bastion.example.com",
        2222,
        "deploy",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/prod-bastion".to_string()),
        },
    ));
    save_profiles(&paths, &profiles).expect("save profiles");

    let loaded_cfg = load_config(&paths).expect("load config");
    assert_eq!(loaded_cfg, cfg);

    let loaded_profiles = load_profiles(&paths).expect("load profiles");
    assert_eq!(loaded_profiles, profiles);

    let raw_config = std::fs::read_to_string(paths.config_file()).expect("read config.toml");
    assert!(raw_config.contains("schema_version = 1"));

    let raw_profiles = std::fs::read_to_string(paths.profiles_file()).expect("read profiles.toml");
    assert!(raw_profiles.contains("schema_version = 1"));
    assert!(!raw_profiles.contains("password ="));
    assert!(!raw_profiles.contains("passphrase ="));

    let mut entries: Vec<String> = std::fs::read_dir(paths.config_dir())
        .expect("read config dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, vec!["config.toml", "profiles.toml"]);
}

#[test]
fn load_rejects_unsupported_schema_version_and_malformed_toml() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    std::fs::create_dir_all(paths.config_dir()).expect("create config dir");

    std::fs::write(paths.config_file(), "schema_version = 99\n").expect("write future version");
    let err = load_config(&paths).expect_err("must reject unsupported schema_version");
    assert!(matches!(
        err,
        CoreError::UnsupportedSchemaVersion {
            found: 99,
            expected: 1,
            ..
        }
    ));

    std::fs::write(paths.config_file(), "schema_version = [unterminated")
        .expect("write malformed toml");
    let err = load_config(&paths).expect_err("must reject malformed toml");
    assert!(matches!(err, CoreError::TomlDeserialize { span: Some(_), .. }));
}

#[test]
fn ensure_config_files_initializes_missing_files_on_disk() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    let (cfg, profiles) = ensure_config_files(&paths).expect("initialize config files");
    assert_eq!(cfg, AppConfig::default());
    assert_eq!(profiles, ProfilesConfig::default());
    assert!(paths.config_file().is_file());
    assert!(paths.profiles_file().is_file());
}

fn read_scrollback(path: &std::path::Path) -> usize {
    let raw = std::fs::read_to_string(path).expect("read config backup");
    let parsed: AppConfig = toml::from_str(&raw).expect("parse config backup");
    parsed.terminal.scrollback_lines
}

#[test]
fn atomic_save_rotates_three_backups_and_evicts_oldest() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    let target = paths.config_file();

    let bak1 = backup_path(&target, 1);
    let bak2 = backup_path(&target, 2);
    let bak3 = backup_path(&target, 3);
    let bak4 = backup_path(&target, 4);

    let mut cfg = AppConfig::default();

    // v1: initial write produces no backups.
    cfg.terminal.scrollback_lines = 1000;
    save_config(&paths, &cfg).expect("save v1");
    assert_eq!(read_scrollback(&target), 1000);
    assert!(!bak1.exists());

    // v2: v1 rotates to .bak.1.
    cfg.terminal.scrollback_lines = 2000;
    save_config(&paths, &cfg).expect("save v2");
    assert_eq!(read_scrollback(&target), 2000);
    assert_eq!(read_scrollback(&bak1), 1000);
    assert!(!bak2.exists());

    // v3: v2 -> .bak.1, v1 -> .bak.2.
    cfg.terminal.scrollback_lines = 3000;
    save_config(&paths, &cfg).expect("save v3");
    assert_eq!(read_scrollback(&target), 3000);
    assert_eq!(read_scrollback(&bak1), 2000);
    assert_eq!(read_scrollback(&bak2), 1000);
    assert!(!bak3.exists());

    // v4: v3 -> .bak.1, v2 -> .bak.2, v1 -> .bak.3.
    cfg.terminal.scrollback_lines = 4000;
    save_config(&paths, &cfg).expect("save v4");
    assert_eq!(read_scrollback(&target), 4000);
    assert_eq!(read_scrollback(&bak1), 3000);
    assert_eq!(read_scrollback(&bak2), 2000);
    assert_eq!(read_scrollback(&bak3), 1000);

    // v5: v4 -> .bak.1, v3 -> .bak.2, v2 -> .bak.3 (v1 evicted).
    cfg.terminal.scrollback_lines = 5000;
    save_config(&paths, &cfg).expect("save v5");
    assert_eq!(read_scrollback(&target), 5000);
    assert_eq!(read_scrollback(&bak1), 4000);
    assert_eq!(read_scrollback(&bak2), 3000);
    assert_eq!(read_scrollback(&bak3), 2000);
    assert!(!bak4.exists());

    // Verify profiles.toml rotates independently without colliding with config.toml backups.
    let mut profiles = ProfilesConfig::default();
    save_profiles(&paths, &profiles).expect("save profiles v1");
    profiles.profiles.push(Profile::local_shell("Shell", None));
    save_profiles(&paths, &profiles).expect("save profiles v2");

    let profiles_bak1 = backup_path(&paths.profiles_file(), 1);
    assert!(profiles_bak1.is_file());
    let prev_profiles: ProfilesConfig =
        toml::from_str(&std::fs::read_to_string(&profiles_bak1).unwrap()).unwrap();
    assert!(prev_profiles.profiles.is_empty());

    // No temp files remain in config_dir.
    for entry in std::fs::read_dir(paths.config_dir()).expect("read config dir") {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            name == "config.toml"
                || name.starts_with("config.toml.bak.")
                || name == "profiles.toml"
                || name.starts_with("profiles.toml.bak."),
            "unexpected leftover file: {name}"
        );
    }
}

#[test]
fn ssh_profile_defaults_auto_connect_on_startup_to_false_and_preserves_true() {
    let legacy_toml = r#"
schema_version = 1

[[profiles]]
id = "ssh-1"
name = "Bastion"

[profiles.target]
kind = "ssh"
host = "bastion.example.com"
username = "ops"
"#;
    let parsed: ProfilesConfig = toml::from_str(legacy_toml).expect("parse legacy SSH TOML");
    assert_eq!(parsed.profiles.len(), 1);
    assert!(!parsed.profiles[0].auto_connect_on_startup());

    let mut profile = Profile::ssh(
        "Auto Bastion",
        "auto.example.com",
        2222,
        "deploy",
        SshAuthMethod::Agent,
    );
    assert!(!profile.auto_connect_on_startup());
    profile.set_auto_connect_on_startup(true);
    assert!(profile.auto_connect_on_startup());

    let roundtrip: ProfilesConfig =
        toml::from_str(&toml::to_string_pretty(&ProfilesConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            profiles: vec![profile],
        })
        .unwrap())
        .unwrap();
    assert!(roundtrip.profiles[0].auto_connect_on_startup());
}

#[test]
fn keepalive_and_mux_attach_defaults_overrides_and_backward_compat_roundtrip() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    // Default AppConfig uses 15s keepalive interval and 3 max missed probes.
    let default_cfg = AppConfig::default();
    assert_eq!(default_cfg.keepalive_interval_secs, 15);
    assert_eq!(default_cfg.keepalive_max_missed, 3);

    // Older config.toml without keepalive fields loads cleanly with (15, 3).
    std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
    std::fs::write(paths.config_file(), "schema_version = 1\n").expect("write legacy config");
    let compat_cfg = load_config(&paths).expect("load legacy config");
    assert_eq!(compat_cfg.keepalive_interval_secs, 15);
    assert_eq!(compat_cfg.keepalive_max_missed, 3);

    // Explicit global keepalive settings persist and reload.
    let mut custom_cfg = AppConfig::default();
    custom_cfg.keepalive_interval_secs = 20;
    custom_cfg.keepalive_max_missed = 4;
    save_config(&paths, &custom_cfg).expect("save custom keepalive config");
    let reloaded_cfg = load_config(&paths).expect("reload custom keepalive config");
    assert_eq!(reloaded_cfg.keepalive_interval_secs, 20);
    assert_eq!(reloaded_cfg.keepalive_max_missed, 4);

    // Per-Profile overrides & auto_attach_mux / mux_attach_command round-trip and override global config.
    let mut profile = Profile::ssh(
        "Supervised Host",
        "srv.internal",
        2222,
        "ops",
        SshAuthMethod::Agent,
    );
    assert_eq!(profile.resolve_keepalive(&reloaded_cfg), (20, 4));
    assert_eq!(profile.effective_mux_attach_command(), None);

    profile.set_keepalive_overrides(Some(5), Some(2));
    profile.set_mux_attach(true, Some("tmux attach -t ops || tmux new -s ops".to_string()));
    assert_eq!(profile.resolve_keepalive(&reloaded_cfg), (5, 2));
    assert_eq!(
        profile.effective_mux_attach_command().as_deref(),
        Some("tmux attach -t ops || tmux new -s ops")
    );

    let mut profiles = ProfilesConfig::default();
    profiles.profiles.push(profile);
    save_profiles(&paths, &profiles).expect("save profiles with keepalive and mux");
    let loaded_profiles = load_profiles(&paths).expect("reload profiles");
    let loaded_ssh = &loaded_profiles.profiles[0];
    assert_eq!(loaded_ssh.resolve_keepalive(&reloaded_cfg), (5, 2));
    assert_eq!(
        loaded_ssh.effective_mux_attach_command().as_deref(),
        Some("tmux attach -t ops || tmux new -s ops")
    );

    // When auto_attach_mux is disabled, effective_mux_attach_command returns None even if command is stored.
    let mut disabled = loaded_ssh.clone();
    disabled.set_mux_attach(false, Some("screen -xR".to_string()));
    assert_eq!(disabled.effective_mux_attach_command(), None);

    // Legacy tmux_reattach session name works when auto_attach_mux is enabled and mux_attach_command is None.
    let tmux_legacy_toml = r#"
schema_version = 1

[[profiles]]
id = "ssh-mux"
name = "Mux Legacy"

[profiles.target]
kind = "ssh"
host = "mux.internal"
username = "ops"
auto_attach_mux = true
tmux_reattach = "main"
"#;
    let parsed_mux: ProfilesConfig =
        toml::from_str(tmux_legacy_toml).expect("parse legacy tmux_reattach TOML");
    assert_eq!(
        parsed_mux.profiles[0]
            .effective_mux_attach_command()
            .as_deref(),
        Some("tmux attach -t main || tmux new -s main")
    );
}
