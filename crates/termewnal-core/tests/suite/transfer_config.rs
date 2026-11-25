use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigPaths, DEFAULT_PART_THRESHOLD_BYTES, TransferConfig, TransferConflictPolicy,
    load_config, save_config,
};

#[test]
fn transfer_config_defaults_and_legacy_toml_backward_compatibility() {
    assert_eq!(DEFAULT_PART_THRESHOLD_BYTES, 1_048_576);

    let cfg = AppConfig::default();
    assert_eq!(
        cfg.transfer,
        TransferConfig {
            default_conflict_policy: TransferConflictPolicy::Ask,
            follow_symlinks: false,
            part_threshold_bytes: 1_048_576,
        }
    );

    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());
    std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
    std::fs::write(paths.config_file(), "schema_version = 1\n").expect("write legacy config");

    let loaded = load_config(&paths).expect("load legacy config without [transfer]");
    assert_eq!(loaded.transfer, TransferConfig::default());
}

#[test]
fn transfer_config_conflict_policies_and_settings_round_trip_in_config_toml() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    for (policy, expected_toml) in [
        (TransferConflictPolicy::Ask, "default_conflict_policy = \"ask\""),
        (
            TransferConflictPolicy::Overwrite,
            "default_conflict_policy = \"overwrite\"",
        ),
        (
            TransferConflictPolicy::Resume,
            "default_conflict_policy = \"resume\"",
        ),
        (
            TransferConflictPolicy::Skip,
            "default_conflict_policy = \"skip\"",
        ),
        (
            TransferConflictPolicy::Rename,
            "default_conflict_policy = \"rename\"",
        ),
    ] {
        let mut cfg = AppConfig::default();
        cfg.transfer.default_conflict_policy = policy;
        cfg.transfer.follow_symlinks = true;
        cfg.transfer.part_threshold_bytes = 2_097_152;
        save_config(&paths, &cfg).expect("save config with transfer settings");

        let raw = std::fs::read_to_string(paths.config_file()).expect("read raw config.toml");
        assert!(
            raw.contains("[transfer]") && raw.contains(expected_toml),
            "expected [transfer] and {expected_toml} in:\n{raw}"
        );

        let reloaded = load_config(&paths).expect("reload config");
        assert_eq!(reloaded.transfer, cfg.transfer);
    }
}
