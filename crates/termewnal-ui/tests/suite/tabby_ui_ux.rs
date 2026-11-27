use std::sync::Arc;

use iced::keyboard::{Key, Modifiers};
use iced::{Size, Theme, window};
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, BuiltinTheme, ConfigPaths, CursorStyle, Profile, ProfileTarget, SecretStore,
    SshAuthMethod, SshProxyConfig, SshProxyKind, TransferConflictPolicy, TunnelDirection,
    TunnelRule, VaultBackend, VaultConfig, load_config, load_profiles, save_config, save_profiles,
    vault::{EncryptedVault, InMemorySecretStore, VaultSession},
};
use termewnal_ssh::{ServerStatsSnapshot, TransportStatus};
use termewnal_ui::command_palette::{PaletteAction, QuickConnectTarget};
use termewnal_ui::server_stats_bar::{format_net_speed, format_stat_percent};
use termewnal_ui::status_bar::StatusBarAction;
use termewnal_ui::{
    AppState, Message, ProfileAuthChoice, ProfileKindChoice, SettingsTab, SshConnectionMode,
    shortcut_for_key_event,
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
    assert!(state.profile_editor().existing_secret_ref.is_some());
    assert!(!state.profile_editor().jump_panel_open);
    drop(state.view(win));
    let _ = state.update(Message::ProfileFormToggleJumpPanel);
    assert!(state.profile_editor().jump_panel_open);
    drop(state.view(win));
    let _ = state.update(Message::ProfileFormForgetSecret);
    assert!(state.profile_editor().existing_secret_ref.is_none());
    drop(state.view(win));
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
    assert!(!state.sftp_drawer().is_open());

    let ssh_profile = Profile::ssh("Render Host", "10.0.0.1", 22, "ops", termewnal_core::SshAuthMethod::Agent);
    let _ = state.update(Message::OpenSshProfile(ssh_profile));
    let _ = state.update(Message::SetSftpDrawerOpen(true));
    assert!(state.sftp_drawer().is_open());

    // Render every built-in theme with sidebar + per-pane SFTP panel open.
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

    // Closing all open tabs exposes the Tabby-style Welcome launcher view with saved profiles.
    while let Some(tab_id) = state.active_tab_id() {
        let _ = state.update(Message::CloseTab(tab_id));
    }
    assert!(state.active_tab_id().is_none());
    drop(state.view(win));
}

#[test]
fn responsive_apple_hig_layout_renders_across_all_viewport_tiers() {
    use iced::{Color, Size};
    use termewnal_core::{TabBarPosition, ThemePalette};
    use termewnal_ui::ui_theme::{ChromePalette, ViewportTier, modal_body_height, modal_width};
    use termewnal_ui::v2_modals::ProtocolLauncherTab;

    // 1. Viewport tier breakpoints.
    assert_eq!(ViewportTier::from_width(720.0), ViewportTier::Compact);
    assert_eq!(ViewportTier::from_width(859.9), ViewportTier::Compact);
    assert_eq!(ViewportTier::from_width(860.0), ViewportTier::Medium);
    assert_eq!(ViewportTier::from_width(1179.0), ViewportTier::Medium);
    assert_eq!(ViewportTier::from_width(1180.0), ViewportTier::Wide);

    // 2. Modal clamping bounds for wide, medium, compact, and extreme narrow windows.
    assert!((modal_width(760.0, 1440.0) - 760.0).abs() < f32::EPSILON);
    assert!((modal_width(760.0, 720.0) - 680.0).abs() < f32::EPSILON);
    assert!((modal_width(760.0, 500.0) - 480.0).abs() < f32::EPSILON);
    assert!((modal_width(760.0, 240.0) - 260.0).abs() < f32::EPSILON);
    assert!((modal_body_height(460.0, 800.0, 240.0) - 460.0).abs() < f32::EPSILON);
    assert!((modal_body_height(460.0, 480.0, 240.0) - 240.0).abs() < f32::EPSILON);
    assert!((modal_body_height(460.0, 320.0, 240.0) - 140.0).abs() < f32::EPSILON);

    // 3. Tabby Default Dark & Default Light tokens (`Follow the color scheme`).
    let dark_chrome = ChromePalette::from_palette(&ThemePalette::for_name(BuiltinTheme::Dark.name()));
    assert!(!dark_chrome.is_light);
    assert_eq!(dark_chrome.term_bg, Color::from_rgb8(0x17, 0x17, 0x17));
    assert_eq!(dark_chrome.accent, Color::from_rgb8(0x5d, 0xa9, 0xf6));

    let light_chrome =
        ChromePalette::from_palette(&ThemePalette::for_name(BuiltinTheme::Light.name()));
    assert!(light_chrome.is_light);
    assert_eq!(light_chrome.term_bg, Color::from_rgb8(0xf8, 0xf9, 0xfa));
    assert_eq!(light_chrome.accent, Color::from_rgb8(0x42, 0x71, 0xae));

    // 4. Full UI view rendering across Wide, Medium, and Compact (Quake-like) dimensions.
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // Profile sidebar is always visible and non-toggleable; bottom status bar starts hidden.
    assert!(state.sidebar().is_open());
    assert!(!state.config().terminal.show_status_bar);
    assert!(!state.config().terminal.hide_tab_index);
    assert!(!state.config().terminal.flex_tabs);

    // Exercise Tabby window/chrome toggles in Settings & live state.
    let _ = state.update(Message::OpenSettingsModal);
    let _ = state.update(Message::SettingsShowProfileSidebarToggled(false));
    let _ = state.update(Message::SettingsShowStatusBarToggled(true));
    let _ = state.update(Message::SettingsHideTabIndexToggled(true));
    let _ = state.update(Message::SettingsFlexTabsToggled(true));
    assert!(!state.sidebar().is_open());
    assert!(!state.effective_sidebar_open(win));
    assert!(state.config().terminal.show_status_bar);
    assert!(state.config().terminal.hide_tab_index);
    assert!(state.config().terminal.flex_tabs);
    let _ = state.update(Message::SettingsSave);

    // Per-window sidebar visibility supports explicit toggle/set alongside SFTP+ Panel.
    let _ = state.update(Message::SetSidebarOpen(true));
    assert!(state.effective_sidebar_open(win));
    let _ = state.update(Message::SetSidebarOpen(false));
    assert!(!state.effective_sidebar_open(win));
    assert!(!state.sidebar().is_open());
    let _ = state.update(Message::ToggleSftpDrawer);

    let viewports = [
        Size::new(1440.0, 900.0), // Wide
        Size::new(980.0, 700.0),  // Medium
        Size::new(720.0, 480.0),  // Compact / Quake
        Size::new(520.0, 420.0),  // Very narrow
    ];

    for size in viewports {
        let _ = state.update(Message::WindowResized(win, size));
        let _ = state.update(Message::SettingsTabBarPositionSelected(TabBarPosition::Top));
        drop(state.view(win));

        let _ = state.update(Message::SettingsTabBarPositionSelected(TabBarPosition::Left));
        drop(state.view(win));

        // Exercise Settings sheet across representative tabs at each size.
        let _ = state.update(Message::OpenSettingsModal);
        for tab in [
            SettingsTab::Appearance,
            SettingsTab::Terminal,
            SettingsTab::Keybindings,
            SettingsTab::Vault,
            SettingsTab::Sync,
        ] {
            let _ = state.update(Message::SettingsSelectTab(tab));
            drop(state.view(win));
        }
        let _ = state.update(Message::EscapePressed);

        // Exercise Profile Editor across all protocol kinds & SSH tabs, Tunnel Create, Command Palette,
        // Quick Commands, Workspaces, and Protocol Launcher.
        let _ = state.update(Message::OpenNewProfileModal);
        for ptab in [
            termewnal_ui::ProfileEditorTab::General,
            termewnal_ui::ProfileEditorTab::Ports,
            termewnal_ui::ProfileEditorTab::Advanced,
            termewnal_ui::ProfileEditorTab::LoginScripts,
        ] {
            let _ = state.update(Message::ProfileFormTabSelected(ptab));
            drop(state.view(win));
        }
        for kind in [
            ProfileKindChoice::Ssh,
            ProfileKindChoice::LocalShell,
            ProfileKindChoice::Serial,
            ProfileKindChoice::Telnet,
            ProfileKindChoice::Docker,
        ] {
            let _ = state.update(Message::ProfileFormKindChanged(kind));
            drop(state.view(win));
        }
        let _ = state.update(Message::EscapePressed);

        let _ = state.update(Message::OpenTunnelCreateModal);
        drop(state.view(win));
        let _ = state.update(Message::EscapePressed);

        let _ = state.update(Message::OpenQuickConnectPalette);
        drop(state.view(win));
        let _ = state.update(Message::EscapePressed);

        let _ = state.update(Message::OpenQuickCommandsModal);
        drop(state.view(win));
        let _ = state.update(Message::EscapePressed);

        let _ = state.update(Message::OpenWorkspacesModal);
        drop(state.view(win));
        let _ = state.update(Message::EscapePressed);

        for ptab in [
            ProtocolLauncherTab::Containers,
            ProtocolLauncherTab::Serial,
            ProtocolLauncherTab::Telnet,
        ] {
            let _ = state.update(Message::OpenProtocolLauncherModal(ptab));
            drop(state.view(win));
            let _ = state.update(Message::EscapePressed);
        }
    }

    // Close active tab and render Empty Welcome Launcher at Compact size too.
    if let Some(tab_id) = state.active_tab_id() {
        let _ = state.update(Message::CloseTab(tab_id));
    }
    let _ = state.update(Message::WindowResized(win, Size::new(720.0, 480.0)));
    drop(state.view(win));
}

#[test]
fn integrated_tabby_system_bar_borderless_window_controls_render_cleanly() {
    let win_settings = termewnal_ui::window_chrome::main_window_settings(1280, 800);
    assert!(!win_settings.decorations);
    assert!(!win_settings.exit_on_close_request);
    assert!(win_settings.icon.is_some());
    assert!(termewnal_ui::window_chrome::app_window_icon().is_some());

    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // Render across all TabBarPosition orientations (integrated Top System Bar vs docked Title Bar + Tab Strip).
    for pos in [
        termewnal_core::TabBarPosition::Top,
        termewnal_core::TabBarPosition::Bottom,
        termewnal_core::TabBarPosition::Left,
        termewnal_core::TabBarPosition::Right,
    ] {
        let _ = state.update(Message::SettingsTabBarPositionSelected(pos));
        drop(state.view(win));
    }

    // Exercise custom client-side System Bar window controls (Settings, Minimize, Maximize/Restore, Drag, Edge Resize).
    let _ = state.update(Message::OpenSettingsModal);
    assert!(state.settings_ui().is_open());
    drop(state.view(win));
    let _ = state.update(Message::EscapePressed);

    let _ = state.update(Message::WindowToggleMaximize(win));
    drop(state.view(win));
    let _ = state.update(Message::WindowToggleMaximize(win));
    drop(state.view(win));

    let _ = state.update(Message::WindowDragStart(win));
    let _ = state.update(Message::WindowMinimize(win));
    let _ = state.update(Message::WindowShowSystemMenu(win));
    let _ = state.update(Message::WindowEdgeResize(
        win,
        window::Direction::SouthEast,
    ));
}

#[test]
fn server_stats_bar_formats_renders_and_toggles_cleanly() {
    assert_eq!(format_stat_percent(None), "--%");
    assert_eq!(format_stat_percent(Some(18.4)), "18%");
    assert_eq!(format_net_speed(None), "--");
    assert_eq!(format_net_speed(Some(512)), "512 B/s");
    assert_eq!(format_net_speed(Some(423_936)), "414 K/s");
    assert_eq!(format_net_speed(Some(13_107_200)), "12.5 M/s");

    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    assert!(state.config().terminal.show_server_stats);

    let win = window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    let _ = state.update(Message::QuickConnect(QuickConnectTarget {
        host: "10.0.0.10".to_string(),
        port: 22,
        username: "root".to_string(),
    }));
    let sid = state.active_session_id().expect("ssh session active");
    let _ = state.update(Message::SetSessionTransportStatus {
        session_id: sid,
        status: TransportStatus::Connected {
            latency_ms: Some(12),
        },
    });
    // Render before first stats sample (--% placeholder).
    drop(state.view(win));

    let generation = state.ssh_session(sid).unwrap().ssh_generation();
    let stats = ServerStatsSnapshot {
        cpu_percent: Some(18.0),
        ram_percent: Some(58.0),
        ram_used_kb: Some(580_000),
        ram_total_kb: Some(1_000_000),
        disk_percent: Some(74.0),
        disk_used_kb: Some(74_000_000),
        disk_total_kb: Some(100_000_000),
        net_rx_bytes_per_sec: Some(423_936),
        net_tx_bytes_per_sec: Some(305_152),
    };
    let _ = state.update(Message::ServerStatsUpdated {
        session_id: sid,
        generation,
        stats: stats.clone(),
    });
    assert_eq!(state.ssh_session(sid).unwrap().server_stats(), Some(&stats));

    for (w, h) in [(1280.0, 800.0), (960.0, 640.0), (640.0, 480.0)] {
        let _ = state.update(Message::WindowResized(win, Size::new(w, h)));
        drop(state.view(win));
    }

    let _ = state.update(Message::SettingsShowServerStatsToggled(false));
    assert!(!state.config().terminal.show_server_stats);
    drop(state.view(win));

    let _ = state.update(Message::SettingsShowServerStatsToggled(true));
    assert!(state.config().terminal.show_server_stats);
    drop(state.view(win));

    let _ = state.update(Message::ServerStatsUnavailable {
        session_id: sid,
        generation,
    });
    assert!(state.ssh_session(sid).unwrap().server_stats().is_none());
    drop(state.view(win));
}

#[test]
fn ssh_connection_mode_dropdown_and_proxy_settings_persist_with_vault_isolation() {
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

    let _ = state.update(Message::OpenNewProfileModal);
    assert_eq!(state.profile_editor().connection_mode, SshConnectionMode::Direct);
    assert!(!state.profile_editor().connection_menu_open);

    // Open and close the 5-option Connection popover cleanly across viewport tiers.
    let _ = state.update(Message::ProfileFormToggleConnectionMenu);
    assert!(state.profile_editor().connection_menu_open);
    drop(state.view(win));
    let _ = state.update(Message::ProfileFormCloseConnectionMenu);
    assert!(!state.profile_editor().connection_menu_open);

    let _ = state.update(Message::ProfileFormNameChanged("Bastion App".to_string()));
    let _ = state.update(Message::ProfileFormHostChanged("10.0.0.10".to_string()));
    let _ = state.update(Message::ProfileFormUsernameChanged("deploy".to_string()));

    // Render all connection modes (wide & compact).
    for mode in SshConnectionMode::ALL {
        let _ = state.update(Message::ProfileFormConnectionModeSelected(mode));
        assert_eq!(state.profile_editor().connection_mode, mode);
        assert!(!state.profile_editor().connection_menu_open);
        for (w, h) in [(1280.0, 800.0), (540.0, 480.0)] {
            let _ = state.update(Message::WindowResized(win, Size::new(w, h)));
            drop(state.view(win));
        }
    }
    let _ = state.update(Message::WindowResized(win, Size::new(1280.0, 800.0)));

    // Empty Proxy command validates cleanly.
    let _ = state.update(Message::ProfileFormConnectionModeSelected(
        SshConnectionMode::ProxyCommand,
    ));
    let _ = state.update(Message::ProfileSave);
    assert!(state.profile_editor().validation_error.is_some());

    // Select SOCKS proxy, validate required host, then persist with Vault-stored proxy password.
    let _ = state.update(Message::ProfileFormConnectionModeSelected(
        SshConnectionMode::SocksProxy,
    ));
    let _ = state.update(Message::ProfileSave);
    assert!(state.profile_editor().validation_error.is_some());

    let _ = state.update(Message::ProfileFormProxyHostChanged("127.0.0.1".to_string()));
    let _ = state.update(Message::ProfileFormProxyPortChanged("1080".to_string()));
    let _ = state.update(Message::ProfileFormProxyUsernameChanged("socks-user".to_string()));
    let _ = state.update(Message::ProfileFormProxySecretChanged(
        "socks-proxy-secret".to_string(),
    ));
    let _ = state.update(Message::ProfileSave);
    assert!(!state.profile_editor().is_open());

    let profiles_raw = std::fs::read_to_string(paths.profiles_file()).unwrap();
    assert!(
        !profiles_raw.contains("socks-proxy-secret"),
        "profiles.toml must never store plaintext proxy secrets"
    );

    let saved = load_profiles(&paths).unwrap();
    let prof_id = saved.profiles[0].id.clone();
    let secret_ref = match &saved.profiles[0].target {
        ProfileTarget::Ssh {
            proxy: Some(SshProxyConfig {
                kind,
                host,
                port,
                username,
                vault_secret_ref,
            }),
            proxy_command,
            proxy_jump,
            ..
        } => {
            assert_eq!(*kind, SshProxyKind::Socks5);
            assert_eq!(host, "127.0.0.1");
            assert_eq!(*port, 1080);
            assert_eq!(username.as_deref(), Some("socks-user"));
            assert!(proxy_command.is_none());
            assert!(proxy_jump.is_empty());
            vault_secret_ref.clone().expect("proxy vault_secret_ref stored")
        }
        other => panic!("expected SSH SOCKS proxy target, got {other:?}"),
    };
    assert_eq!(
        state
            .vault()
            .get_secret(&secret_ref)
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("socks-proxy-secret")
    );

    // Re-open Edit modal -> restores SOCKS proxy mode + Vault indicator, then switch to HTTP proxy.
    let _ = state.update(Message::OpenEditProfileModal(prof_id.clone()));
    assert_eq!(
        state.profile_editor().connection_mode,
        SshConnectionMode::SocksProxy
    );
    assert!(state.profile_editor().socks_proxy.existing_secret_ref.is_some());
    let _ = state.update(Message::ProfileFormForgetProxySecret);
    assert!(state.profile_editor().socks_proxy.existing_secret_ref.is_none());

    let _ = state.update(Message::ProfileFormConnectionModeSelected(
        SshConnectionMode::HttpProxy,
    ));
    assert_eq!(state.profile_editor().http_proxy.port, "8080");
    let _ = state.update(Message::ProfileFormProxyHostChanged(
        "proxy.corp.local".to_string(),
    ));
    let _ = state.update(Message::ProfileFormProxyPortChanged("3128".to_string()));
    let _ = state.update(Message::ProfileSave);

    let reloaded = load_profiles(&paths).unwrap();
    match &reloaded.profiles[0].target {
        ProfileTarget::Ssh {
            proxy: Some(proxy),
            ..
        } => {
            assert_eq!(proxy.kind, SshProxyKind::HttpConnect);
            assert_eq!(proxy.host, "proxy.corp.local");
            assert_eq!(proxy.port, 3128);
            assert!(proxy.vault_secret_ref.is_none());
        }
        other => panic!("expected HTTP CONNECT proxy target, got {other:?}"),
    }
}

fn srgb_luminance(c: iced::Color) -> f32 {
    let lin = |ch: f32| {
        if ch <= 0.04045 {
            ch / 12.92
        } else {
            ((ch + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

fn wcag_contrast(a: iced::Color, b: iced::Color) -> f32 {
    let la = srgb_luminance(a);
    let lb = srgb_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[test]
fn all_28_themes_and_extreme_overrides_produce_monotonic_oklch_chrome_and_contrast() {
    use termewnal_core::{ThemeOverrides, ThemePalette};
    use termewnal_ui::ui_theme::{ChromePalette, TAB_BAR_HEIGHT};

    assert!((TAB_BAR_HEIGHT - 40.0).abs() < f32::EPSILON);

    let mut palettes: Vec<ThemePalette> = BuiltinTheme::ALL
        .iter()
        .map(|t| t.palette())
        .collect();
    palettes.push(ThemePalette::for_name_with_overrides(
        BuiltinTheme::Dark.name(),
        &ThemeOverrides {
            background: Some("#000000".to_string()),
            foreground: Some("#e5e5e5".to_string()),
            ..ThemeOverrides::default()
        },
    ));
    palettes.push(ThemePalette::for_name_with_overrides(
        BuiltinTheme::Light.name(),
        &ThemeOverrides {
            background: Some("#ffffff".to_string()),
            ui_accent: Some("#ffea00".to_string()),
            ..ThemeOverrides::default()
        },
    ));

    for pal in palettes {
        let chrome = ChromePalette::from_palette(&pal);
        for surface in [
            chrome.term_bg,
            chrome.bg_more,
            chrome.chrome_bg,
            chrome.well_bg,
            chrome.panel_bg,
            chrome.card_bg,
            chrome.secondary,
            chrome.secondary_hover,
            chrome.hairline,
            chrome.border_subtle,
            chrome.border,
        ] {
            assert!(
                (surface.a - 1.0).abs() < 1e-4,
                "theme {} must have opaque surfaces/borders (found alpha {})",
                pal.theme.name(),
                surface.a
            );
        }

        let l_more = srgb_luminance(chrome.bg_more);
        let l_chrome = srgb_luminance(chrome.chrome_bg);
        let l_term = srgb_luminance(chrome.term_bg);
        let l_well = srgb_luminance(chrome.well_bg);
        let l_panel = srgb_luminance(chrome.panel_bg);
        let l_card = srgb_luminance(chrome.card_bg);
        let l_hover = srgb_luminance(chrome.secondary_hover);

        if chrome.is_light {
            assert!(
                l_hover < l_card && l_card < l_more && l_more <= l_chrome && l_chrome < l_well && l_well < l_term && l_term <= l_panel,
                "light theme {} surface ordering broken: hover={l_hover:.4} card={l_card:.4} more={l_more:.4} chrome={l_chrome:.4} well={l_well:.4} term={l_term:.4} panel={l_panel:.4}",
                pal.theme.name()
            );
        } else if l_term < 0.005 {
            assert!(
                l_term < l_more && l_more <= l_chrome && l_chrome < l_well && l_well < l_panel && l_panel < l_card && l_card < l_hover,
                "ultra-dark theme {} surface ordering broken: term={l_term:.4} more={l_more:.4} chrome={l_chrome:.4} well={l_well:.4} panel={l_panel:.4} card={l_card:.4} hover={l_hover:.4}",
                pal.theme.name()
            );
        } else {
            assert!(
                l_more <= l_chrome && l_chrome < l_term && l_term < l_well && l_well < l_panel && l_panel < l_card && l_card < l_hover,
                "dark theme {} surface ordering broken: more={l_more:.4} chrome={l_chrome:.4} term={l_term:.4} well={l_well:.4} panel={l_panel:.4} card={l_card:.4} hover={l_hover:.4}",
                pal.theme.name()
            );
        }

        for bg in [chrome.term_bg, chrome.chrome_bg, chrome.panel_bg] {
            assert!(wcag_contrast(chrome.text_primary, bg) >= 4.5);
            assert!(wcag_contrast(chrome.text_secondary, bg) >= 4.5);
            assert!(wcag_contrast(chrome.text_muted, bg) >= 3.1);
            assert!(wcag_contrast(chrome.text_emphasis, bg) >= 7.0);
        }
        for semantic in [chrome.accent, chrome.success, chrome.warning, chrome.danger] {
            assert!(
                wcag_contrast(semantic, chrome.chrome_bg) >= 3.1
                    && wcag_contrast(semantic, chrome.panel_bg) >= 3.1,
                "theme {} semantic color lacks 3.1 contrast",
                pal.theme.name()
            );
        }
        assert!(wcag_contrast(chrome.accent_fg, chrome.accent) >= 4.5);
    }
}

#[test]
fn unified_40px_top_header_and_borderless_hit_precedence() {
    use iced::{Point, Rectangle, Size, window};
    use termewnal_core::TabBarPosition;
    use termewnal_ui::ui_theme::TAB_BAR_HEIGHT;
    use termewnal_ui::window_chrome::{BorderlessResizeFrame, TitleBarDragZone};

    let bounds = Rectangle::new(Point::ORIGIN, Size::new(1200.0, 800.0));

    // Clicking inside top-right Close/controls captured by child must never trigger East/NorthEast resize.
    assert!(BorderlessResizeFrame::resolve_resize_press(
        true,
        true,
        true,
        bounds,
        Some(Point::new(1198.0, 2.0)),
    )
    .is_none());

    // Top 5px edge over uncaptured drag spacer triggers North resize, and TitleBarDragZone yields it.
    let drag_bounds = Rectangle::new(Point::new(400.0, 0.0), Size::new(600.0, TAB_BAR_HEIGHT));
    assert!(!TitleBarDragZone::allows_drag_at(
        true,
        false,
        5.0,
        drag_bounds,
        Some(Point::new(600.0, 2.0)),
    ));
    assert!(matches!(
        BorderlessResizeFrame::resolve_resize_press(
            true,
            false,
            true,
            bounds,
            Some(Point::new(600.0, 2.0)),
        ),
        Some(window::Direction::North)
    ));

    // Below 5px resize bezel inside header drag spacer starts drag cleanly.
    assert!(TitleBarDragZone::allows_drag_at(
        true,
        false,
        5.0,
        drag_bounds,
        Some(Point::new(600.0, 20.0)),
    ));

    // Top tab header reserves >= 72px drag spacer even in Compact 720px windows and anchors context menus below one 40px header.
    let (max_tabs_w, reserved_drag) = AppState::top_header_tab_budget(720.0, false, true, true);
    assert!(reserved_drag >= 72.0);
    assert!(max_tabs_w >= 60.0);
    assert!(
        (AppState::tab_context_menu_anchor_y(TabBarPosition::Top, 800.0) - (TAB_BAR_HEIGHT + 1.0))
            .abs()
            < f32::EPSILON
    );
}
