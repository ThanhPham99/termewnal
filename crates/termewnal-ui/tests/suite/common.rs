use std::path::Path;
use std::sync::Arc;

use termewnal_core::{AppConfig, ConfigPaths, VaultSession};
use termewnal_ui::AppState;

/// Seeds `config.toml` with a fast deterministic shell when no config file exists yet so
/// headless UI tests spawn quickly without sourcing user interactive shell rc files.
pub fn seed_fast_test_shell(paths: &ConfigPaths) {
    if paths.config_file().exists() {
        return;
    }

    #[cfg(unix)]
    let fast_shell = Path::new("/bin/sh")
        .exists()
        .then(|| "/bin/sh".to_string());
    #[cfg(windows)]
    let fast_shell = Some(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()));
    #[cfg(not(any(unix, windows)))]
    let fast_shell: Option<String> = None;

    if let Some(shell) = fast_shell {
        std::fs::create_dir_all(paths.config_dir()).expect("create test config dir");
        let mut cfg = AppConfig::default();
        cfg.terminal.shell = Some(shell);
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
