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
