use std::sync::Arc;

use iced::keyboard::{Key, Modifiers};
use iced::{Theme, window};
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, BuiltinTheme, ConfigPaths, CursorStyle, Profile, ProfileTarget, SecretStore,
    SshAuthMethod, TransferConflictPolicy, TunnelDirection, TunnelRule, VaultBackend, VaultConfig,
    load_config, load_profiles, save_config, save_profiles,
    vault::{EncryptedVault, InMemorySecretStore, VaultSession},
};
use termewnal_ui::command_palette::PaletteAction;
use termewnal_ui::status_bar::StatusBarAction;
use termewnal_ui::{
    AppState, Message, ProfileAuthChoice, ProfileKindChoice, SettingsTab, shortcut_for_key_event,
};

#[test]
fn settings_modal_ctrl_comma_live_preview_cancel_and_atomic_save() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = super::common::boot_test_app(paths.clone()).expect("boot AppState");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let active_tab = state.active_tab_id();
    assert!(
        matches!(
            shortcut_for_key_event(&Key::Character(",".into()), Modifiers::CTRL, active_tab),
            Some(Message::OpenSettingsModal)
        ),
        "Ctrl+, must map to OpenSettingsModal"
    );

    assert!(!state.settings_ui().is_open());
    assert!(!state.has_modal_open());

    let initial_terminal = state.config().terminal.clone();
    let _ = state.update(Message::OpenSettingsModal);
    assert!(state.settings_ui().is_open());
    assert!(state.has_modal_open());
    assert_eq!(state.settings_ui().active_tab(), SettingsTab::Appearance);
    drop(state.view(win));

    // Live appearance preview mutates active theme/font/cursor before save.
    let _ = state.update(Message::SettingsThemeSelected(BuiltinTheme::Light));
    let _ = state.update(Message::SettingsStepFontSize(2.0));
    let _ = state.update(Message::SettingsLigaturesToggled(
        !initial_terminal.font_ligatures,
    ));
    let _ = state.update(Message::SettingsCursorStyleSelected(CursorStyle::Beam));

    assert_eq!(state.config().terminal.theme, BuiltinTheme::Light.name());
    assert!((state.config().terminal.font_size - (initial_terminal.font_size + 2.0)).abs() < f32::EPSILON);
    assert_eq!(
        state.config().terminal.font_ligatures,
        !initial_terminal.font_ligatures
    );
    assert_eq!(state.config().terminal.cursor_style, CursorStyle::Beam);
    assert_eq!(state.theme(win), Theme::Light);
    drop(state.view(win));

    // Pressing Escape cancels uncommitted preview changes and restores saved config.
    let _ = state.update(Message::EscapePressed);
    assert!(!state.settings_ui().is_open());
    assert!(!state.has_modal_open());
    assert_eq!(state.config().terminal, initial_terminal);

    // Reopen, edit across all 4 tabs, and Save Settings -> atomically persists to config.toml.
    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsThemeSelected(BuiltinTheme::TokyoNight));
    let _ = state.update(Message::SettingsFontFamilyChanged(
        "Fira Code".to_string(),
    ));
    let _ = state.update(Message::SettingsFontSizeChanged("15".to_string()));
    let _ = state.update(Message::SettingsCursorStyleSelected(CursorStyle::Underline));

    let _ = state.update(Message::SettingsSelectTab(SettingsTab::Terminal));
    let _ = state.update(Message::SettingsScrollbackChanged("25000".to_string()));
    let _ = state.update(Message::SettingsCopyOnSelectToggled(true));
    let _ = state.update(Message::SettingsConfirmMultilinePasteToggled(false));
    drop(state.view(win));

    let _ = state.update(Message::SettingsSelectTab(SettingsTab::NetworkAndSftp));
    let _ = state.update(Message::SettingsKeepaliveIntervalChanged("20".to_string()));
    let _ = state.update(Message::SettingsKeepaliveMissedChanged("4".to_string()));
    let _ = state.update(Message::SettingsConflictPolicySelected(
        TransferConflictPolicy::Overwrite,
    ));
    let _ = state.update(Message::SettingsFollowSymlinksToggled(true));
    drop(state.view(win));

    let _ = state.update(Message::SettingsSelectTab(SettingsTab::Vault));
    let _ = state.update(Message::SettingsVaultBackendSelected(
        VaultBackend::EncryptedFile,
    ));
    let _ = state.update(Message::SettingsAutoLockChanged("45".to_string()));
    drop(state.view(win));

    let _ = state.update(Message::SettingsSave);
    assert!(!state.settings_ui().is_open());
    assert_eq!(state.theme(win), Theme::TokyoNight);
    assert_eq!(state.vault().backend(), VaultBackend::EncryptedFile);
    assert_eq!(state.vault().auto_lock_minutes(), 45);

    let persisted = load_config(&paths).expect("reload persisted config.toml");
    assert_eq!(persisted.terminal.theme, BuiltinTheme::TokyoNight.name());
    assert_eq!(persisted.terminal.font_family, "Fira Code");
    assert!((persisted.terminal.font_size - 15.0).abs() < f32::EPSILON);
    assert_eq!(persisted.terminal.cursor_style, CursorStyle::Underline);
    assert_eq!(persisted.terminal.scrollback_lines, 25_000);
    assert!(persisted.terminal.copy_on_select);
    assert!(!persisted.terminal.confirm_multiline_paste);
    assert_eq!(persisted.keepalive_interval_secs, 20);
    assert_eq!(persisted.keepalive_max_missed, 4);
    assert_eq!(
        persisted.transfer.default_conflict_policy,
        TransferConflictPolicy::Overwrite
    );
    assert!(persisted.transfer.follow_symlinks);
    assert_eq!(persisted.vault.backend, VaultBackend::EncryptedFile);
    assert_eq!(persisted.vault.auto_lock_minutes, 45);
}

#[test]
fn settings_modal_validation_rejects_invalid_inputs_without_closing() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = super::common::boot_test_app(paths.clone()).expect("boot AppState");

    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsFontSizeChanged("99".to_string()));
    let _ = state.update(Message::SettingsSave);
    assert!(state.settings_ui().is_open());
    assert!(state.settings_ui().error().is_some());

    let _ = state.update(Message::SettingsFontSizeChanged("14".to_string()));
    let _ = state.update(Message::SettingsScrollbackChanged("50".to_string()));
    let _ = state.update(Message::SettingsSave);
    assert!(state.settings_ui().is_open());
    assert!(state.settings_ui().error().is_some());

    let _ = state.update(Message::SettingsScrollbackChanged("5000".to_string()));
    let _ = state.update(Message::SettingsKeepaliveMissedChanged("0".to_string()));
    let _ = state.update(Message::SettingsSave);
    assert!(state.settings_ui().is_open());
    assert!(state.settings_ui().error().is_some());

    let _ = state.update(Message::SettingsKeepaliveMissedChanged("3".to_string()));
    let _ = state.update(Message::SettingsKeepaliveIntervalChanged("bad".to_string()));
    let _ = state.update(Message::SettingsSave);
    assert!(state.settings_ui().is_open());
    assert!(state.settings_ui().error().is_some());

    // Validated recovery closes cleanly and writes updated keepalive.
    let _ = state.update(Message::SettingsKeepaliveIntervalChanged("15".to_string()));
    let _ = state.update(Message::SettingsSave);
    assert!(!state.settings_ui().is_open());
    assert_eq!(load_config(&paths).unwrap().keepalive_interval_secs, 15);
}

#[test]
fn command_palette_and_status_bar_expose_settings_and_new_profile() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        super::common::boot_test_app(ConfigPaths::from_root(temp.path())).expect("boot AppState");

    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged(
        "Open Settings".to_string(),
    ));
    let items = state.command_palette_items();
    assert!(
        items
            .iter()
            .any(|i| i.action == PaletteAction::OpenSettings && i.shortcut.as_deref() == Some("Ctrl+,")),
        "Command Palette must expose OpenSettings with Ctrl+, shortcut"
    );
    let _ = state.update(Message::CommandPaletteExecuteItem(
        PaletteAction::OpenSettings,
    ));
    assert!(!state.command_palette().is_open());
    assert!(state.settings_ui().is_open());

    let _ = state.update(Message::CloseSettingsModal);
    let _ = state.update(Message::StatusBarAction(StatusBarAction::OpenSettings));
    assert!(state.settings_ui().is_open());

    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged(
        "New Profile".to_string(),
    ));
    assert!(
        state
            .command_palette_items()
            .iter()
            .any(|i| i.action == PaletteAction::OpenNewProfile)
    );
    let _ = state.update(Message::CommandPaletteExecuteItem(
        PaletteAction::OpenNewProfile,
    ));
    assert!(!state.command_palette().is_open());
    assert!(state.profile_editor().is_open());
}

#[test]
fn profile_manager_creates_edits_deletes_and_isolates_vault_secrets() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut cfg = AppConfig::default();
    #[cfg(unix)]
    if std::path::Path::new("/bin/sh").exists() {
        cfg.terminal.shell = Some("/bin/sh".to_string());
    }
    cfg.vault = VaultConfig {
        backend: VaultBackend::EncryptedFile,
        auto_lock_minutes: 30,
    };
    save_config(&paths, &cfg).expect("save EncryptedFile config");

    let vault = Arc::new(VaultSession::new(
        cfg.vault.clone(),
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    let mut state = AppState::boot_with_paths_and_vault(paths.clone(), vault).expect("boot app");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));
    let _ = state.update(Message::VaultInitializeSubmitted(
        "vault-master-pw".to_string(),
    ));

    // Attempting to save a password profile while blank fails validation.
    let _ = state.update(Message::OpenNewProfileModal);
    assert!(state.profile_editor().is_open());
    assert!(state.has_modal_open());
    let _ = state.update(Message::ProfileSave);
    assert!(state.profile_editor().is_open());
    assert!(state.profile_editor().validation_error.is_some());

    // Fill SSH profile with Password auth -> stores password in Vault only.
    let _ = state.update(Message::ProfileFormNameChanged("Prod API".to_string()));
    let _ = state.update(Message::ProfileFormGroupChanged("Production".to_string()));
    let _ = state.update(Message::ProfileFormTagsChanged("prod, eu".to_string()));
    let _ = state.update(Message::ProfileFormHostChanged("api.internal".to_string()));
    let _ = state.update(Message::ProfileFormPortChanged("2222".to_string()));
    let _ = state.update(Message::ProfileFormUsernameChanged("deploy".to_string()));
    let _ = state.update(Message::ProfileFormAuthChanged(ProfileAuthChoice::Password));
    let _ = state.update(Message::ProfileFormSecretChanged(
        "super-secret-pw".to_string(),
    ));
    let _ = state.update(Message::ProfileFormAutoMuxToggled(true));
    drop(state.view(win));
    let _ = state.update(Message::ProfileSave);

    assert!(!state.profile_editor().is_open());
    assert_eq!(state.profiles().profiles.len(), 1);
    let created = &state.profiles().profiles[0];
    let created_id = created.id.clone();
    assert_eq!(created.name, "Prod API");
    assert_eq!(created.group.as_deref(), Some("Production"));
    assert_eq!(created.tags, vec!["prod", "eu"]);

    let secret_ref = match &created.target {
        ProfileTarget::Ssh {
            host,
            port,
            username,
            auth: SshAuthMethod::Password { vault_secret_ref },
            auto_attach_mux,
            ..
        } => {
            assert_eq!(host, "api.internal");
            assert_eq!(*port, 2222);
            assert_eq!(username, "deploy");
            assert!(
                *auto_attach_mux,
                "auto_attach_mux should persist on SSH profile"
            );
            vault_secret_ref
                .clone()
                .expect("password auth must store vault_secret_ref")
        }
        other => panic!("expected SSH Password target, got {other:?}"),
    };

    assert_eq!(
        state
            .vault()
            .get_secret(&secret_ref)
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("super-secret-pw")
    );
    let profiles_raw = std::fs::read_to_string(paths.profiles_file()).unwrap();
    assert!(
        !profiles_raw.contains("super-secret-pw"),
        "profiles.toml must never store plaintext credentials"
    );

    // Attach a TunnelRule on disk to confirm GUI edit preserves existing attached tunnels
    // and existing vault_secret_ref when the secret field is left blank.
    let mut with_tunnel = state.profiles().clone();
    with_tunnel.profiles[0]
        .tunnels
        .push(TunnelRule::local("pg", 5432, "127.0.0.1", 5432));
    save_profiles(&paths, &with_tunnel).unwrap();
    let _ = state.update(Message::ConfigFilesChanged(vec![
        termewnal_core::ConfigFileKind::Profiles,
    ]));

    let _ = state.update(Message::OpenEditProfileModal(created_id.clone()));
    assert!(state.profile_editor().is_open());
    assert_eq!(state.profile_editor().name, "Prod API");
    let _ = state.update(Message::ProfileFormNameChanged(
        "Prod API Primary".to_string(),
    ));
    let _ = state.update(Message::ProfileFormPortChanged("2200".to_string()));
    let _ = state.update(Message::ProfileSave);

    let reloaded = load_profiles(&paths).unwrap();
    assert_eq!(reloaded.profiles.len(), 1);
    assert_eq!(reloaded.profiles[0].name, "Prod API Primary");
    assert_eq!(reloaded.profiles[0].tunnels.len(), 1);
    assert_eq!(reloaded.profiles[0].tunnels[0].direction, TunnelDirection::Local);
    if let ProfileTarget::Ssh { port, auth, .. } = &reloaded.profiles[0].target {
        assert_eq!(*port, 2200);
        assert_eq!(
            *auth,
            SshAuthMethod::Password {
                vault_secret_ref: Some(secret_ref.clone())
            }
        );
    }

    // Create a Local Shell profile too, then delete the SSH profile via modal.
    let _ = state.update(Message::OpenNewProfileModal);
    let _ = state.update(Message::ProfileFormKindChanged(
        ProfileKindChoice::LocalShell,
    ));
    let _ = state.update(Message::ProfileFormNameChanged("Fast Sh".to_string()));
    let _ = state.update(Message::ProfileFormLocalShellChanged("/bin/sh".to_string()));
    let _ = state.update(Message::ProfileSave);
    assert_eq!(load_profiles(&paths).unwrap().profiles.len(), 2);

    let _ = state.update(Message::OpenEditProfileModal(created_id.clone()));
    let _ = state.update(Message::ProfileDelete(created_id));
    let after_delete = load_profiles(&paths).unwrap();
    assert_eq!(after_delete.profiles.len(), 1);
    assert_eq!(after_delete.profiles[0].name, "Fast Sh");
}

#[test]
fn profile_manager_rejects_secret_save_when_encrypted_vault_locked() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut cfg = AppConfig::default();
    #[cfg(unix)]
    if std::path::Path::new("/bin/sh").exists() {
        cfg.terminal.shell = Some("/bin/sh".to_string());
    }
    cfg.vault = VaultConfig {
        backend: VaultBackend::EncryptedFile,
        auto_lock_minutes: 30,
    };
    save_config(&paths, &cfg).expect("save EncryptedFile config");

    let vault = Arc::new(VaultSession::new(
        cfg.vault.clone(),
        Arc::new(InMemorySecretStore::new()),
        EncryptedVault::open(paths.vault_file()),
    ));
    let mut state = AppState::boot_with_paths_and_vault(paths.clone(), vault).expect("boot app");

    let _ = state.update(Message::OpenNewProfileModal);
    let _ = state.update(Message::ProfileFormNameChanged("Locked Host".to_string()));
    let _ = state.update(Message::ProfileFormHostChanged("locked.internal".to_string()));
    let _ = state.update(Message::ProfileFormUsernameChanged("alice".to_string()));
    let _ = state.update(Message::ProfileFormAuthChanged(ProfileAuthChoice::Password));
    let _ = state.update(Message::ProfileFormSecretChanged("pw".to_string()));
    let _ = state.update(Message::ProfileSave);

    assert!(state.profile_editor().is_open());
    assert!(
        state
            .profile_editor()
            .validation_error
            .as_deref()
            .is_some_and(|e| e.contains("locked"))
    );
    assert!(load_profiles(&paths).unwrap().profiles.is_empty());
}

#[test]
fn modal_focus_isolation_and_multi_theme_and_welcome_views_render_cleanly() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut profiles_cfg = termewnal_core::ProfilesConfig::default();
    let sample_profile = Profile::local_shell("Dev Shell".to_string(), Some("/bin/sh".to_string()));
    profiles_cfg.profiles.push(sample_profile);
    save_profiles(&paths, &profiles_cfg).expect("seed sample profile");

    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));
    let _ = state.update(Message::SetSidebarOpen(true));
    let _ = state.update(Message::SetSftpDrawerOpen(true));

    // Render every built-in theme with sidebar + SFTP drawer open.
    for theme in BuiltinTheme::ALL {
        let mut tcfg = state.config().terminal.clone();
        tcfg.theme = theme.name().to_string();
        let _ = state.update(Message::ApplyTerminalConfig(tcfg));
        drop(state.view(win));
    }

    // Verify all overlays trigger `has_modal_open()` and clear cleanly on dismiss.
    let _ = state.update(Message::OpenTunnelCreateModal);
    assert!(state.has_modal_open());
    drop(state.view(win));
    let _ = state.update(Message::EscapePressed);
    assert!(!state.has_modal_open());

    let sid = state.active_session_id().expect("active session");
    let _ = state.update(Message::PasteText {
        session_id: sid,
        text: "echo one\necho two".to_string(),
    });
    assert!(state.has_modal_open());
    drop(state.view(win));
    let _ = state.update(Message::EscapePressed);
    assert!(!state.has_modal_open());

    let _ = state.update(Message::OpenTabbyImportModal);
    assert!(state.has_modal_open());
    drop(state.view(win));
    let _ = state.update(Message::EscapePressed);
    assert!(!state.has_modal_open());

    // Closing the active tab exposes the Tabby-style Welcome launcher view with saved profiles.
    let tab_id = state.active_tab_id().expect("active tab");
    let _ = state.update(Message::CloseTab(tab_id));
    assert!(state.active_tab_id().is_none());
    drop(state.view(win));
}
