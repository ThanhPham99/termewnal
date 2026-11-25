use std::path::Path;
use std::sync::Arc;

use termewnal_core::{AppConfig, ConfigPaths, VaultSession};
use termewnal_ui::AppState;

/// Seeds `config.toml` with `/bin/sh` on Unix when no config file exists yet so headless UI tests
/// spawn fast, deterministic shells without sourcing user interactive shell rc files.
pub fn seed_fast_test_shell(paths: &ConfigPaths) {
    #[cfg(unix)]
    if Path::new("/bin/sh").exists() && !paths.config_file().exists() {
        std::fs::create_dir_all(paths.config_dir()).expect("create test config dir");
        let mut cfg = AppConfig::default();
        cfg.terminal.shell = Some("/bin/sh".to_string());
        let raw = toml::to_string_pretty(&cfg).expect("serialize test AppConfig");
        std::fs::write(paths.config_file(), raw).expect("write test config.toml");
    }
}

/// Boots [`AppState`] with deterministic fast test shell configuration.
pub fn boot_test_app(paths: ConfigPaths) -> Result<AppState, String> {
    seed_fast_test_shell(&paths);
    AppState::boot_with_paths(paths)
}

/// Boots [`AppState`] with an explicit [`VaultSession`] and deterministic fast test shell.
pub fn boot_test_app_with_vault(
    paths: ConfigPaths,
    vault: Arc<VaultSession>,
) -> Result<AppState, String> {
    seed_fast_test_shell(&paths);
    AppState::boot_with_paths_and_vault(paths, vault)
}
