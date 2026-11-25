use std::sync::Arc;
use std::time::Duration;

use iced::keyboard::{Key, Modifiers, key::Named};
use iced::{Theme, window};
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigFileKind, ConfigPaths, VaultBackend, VaultConfig, load_config, save_config,
    vault::{
        EncryptedVault, InMemorySecretStore, ManualVaultClock, SecretStore, VaultError,
        VaultSession,
    },
};
use termewnal_pty::PtyEvent;
use termewnal_ui::{
    AppState, CycleDirection, FocusDirection, Message, SplitAxis, SplitNode, TabId,
    key_event_to_bytes, shortcut_for_key_event,
};

#[test]
fn boot_with_temp_config_initializes_config_files_and_local_shell_session() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());

    let state = AppState::boot_with_paths(paths.clone()).expect("boot headless AppState");

    assert!(paths.config_file().is_file());
    assert!(paths.profiles_file().is_file());
    assert_eq!(state.config().schema_version, 1);

    let session_id = state.active_session_id().expect("must have active Session");
    let session = state.session(session_id).expect("active Session present");
    assert!(!session.pty().shell().program.is_empty());
    assert_eq!(session.pty().dimensions(), (80, 24));
}

#[test]
fn update_routes_terminal_input_and_refreshes_snapshot_on_pty_output() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let session_id = state.active_session_id().expect("active Session");

    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"echo ELM_SLICE9_OK\r".to_vec(),
    });

    let pty = state.session(session_id).unwrap().pty().clone();
    assert!(
        pty.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("ELM_SLICE9_OK")
        }),
        "PTY did not produce expected output; got:\n{}",
        pty.snapshot().visible_text()
    );

    let _ = state.update(Message::PtyEvent {
        session_id,
        event: PtyEvent::Output,
    });

    let snapshot = state.session(session_id).unwrap().snapshot();
    assert!(snapshot.visible_text().contains("ELM_SLICE9_OK"));
}

#[test]
fn update_viewport_resized_updates_pty_and_session_snapshot_dimensions() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let session_id = state.active_session_id().expect("active Session");

    let _ = state.update(Message::ViewportResized {
        session_id,
        cols: 112,
        rows: 40,
    });

    let session = state.session(session_id).unwrap();
    assert_eq!(session.pty().dimensions(), (112, 40));
    assert_eq!(session.snapshot().cols, 112);
    assert_eq!(session.snapshot().rows, 40);
}

#[test]
fn update_tracks_daemon_windows_and_session_lifecycle_events() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let session_id = state.active_session_id().expect("active Session");
    let win = window::Id::unique();

    let _ = state.update(Message::WindowOpened(win));
    assert!(state.windows().contains(&win));

    let _ = state.update(Message::PtyEvent {
        session_id,
        event: PtyEvent::Title("zsh - workspace".to_string()),
    });
    assert_eq!(
        state.session(session_id).unwrap().title(),
        "zsh - workspace"
    );

    let _ = state.update(Message::PtyEvent {
        session_id,
        event: PtyEvent::Exited(Some(0)),
    });
    assert_eq!(state.session(session_id).unwrap().exited(), Some(Some(0)));

    let _ = state.update(Message::WindowClosed(win));
    assert!(state.windows().is_empty());
    assert_eq!(state.theme(win), Theme::Dark);
    assert!(state.title(win).contains("zsh - workspace"));
}

#[test]
fn key_event_to_bytes_encodes_ascii_utf8_ctrl_and_navigation_keys() {
    assert_eq!(
        key_event_to_bytes(
            &Key::Character("a".into()),
            &Key::Character("a".into()),
            Modifiers::empty(),
            Some("a"),
        ),
        b"a"
    );
    assert_eq!(
        key_event_to_bytes(
            &Key::Character("ệ".into()),
            &Key::Character("ệ".into()),
            Modifiers::empty(),
            Some("ệ"),
        ),
        "ệ".as_bytes()
    );
    assert_eq!(
        key_event_to_bytes(
            &Key::Character("c".into()),
            &Key::Character("c".into()),
            Modifiers::CTRL,
            None,
        ),
        vec![0x03]
    );
    assert_eq!(
        key_event_to_bytes(
            &Key::Named(Named::Enter),
            &Key::Named(Named::Enter),
            Modifiers::empty(),
            Some("\r"),
        ),
        b"\r"
    );
    assert_eq!(
        key_event_to_bytes(
            &Key::Named(Named::ArrowUp),
            &Key::Named(Named::ArrowUp),
            Modifiers::empty(),
            None,
        ),
        b"\x1b[A"
    );
    assert_eq!(
        key_event_to_bytes(
            &Key::Character("x".into()),
            &Key::Character("x".into()),
            Modifiers::ALT,
            Some("x"),
        ),
        b"\x1bx"
    );
}

#[test]
fn update_hot_reloads_valid_config_and_profiles_without_interrupting_running_session() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = AppState::boot_with_paths(paths.clone()).expect("boot AppState");
    let win = window::Id::unique();
    let session_id = state.active_session_id().expect("active Session");

    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"echo HOT_RELOAD_BEFORE\r".to_vec(),
    });
    let pty = state.session(session_id).unwrap().pty().clone();
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("HOT_RELOAD_BEFORE")
    }));

    let next_cfg_toml = r#"
schema_version = 1

[terminal]
theme = "Nord"
font_size = 18.0
scrollback_lines = 22000

[vault]
backend = "encrypted_file"
auto_lock_minutes = 15
"#;
    std::fs::write(paths.config_file(), next_cfg_toml).expect("write external config.toml");

    let next_profiles_toml = r#"
schema_version = 1

[[profiles]]
id = "p-1"
name = "Prod SSH"

[profiles.target]
kind = "local_shell"
"#;
    std::fs::write(paths.profiles_file(), next_profiles_toml)
        .expect("write external profiles.toml");

    let _ = state.update(Message::ConfigFilesChanged(vec![
        ConfigFileKind::AppConfig,
        ConfigFileKind::Profiles,
    ]));

    assert_eq!(state.config().terminal.theme, "Nord");
    assert_eq!(state.config().terminal.font_size, 18.0);
    assert_eq!(state.theme(win), Theme::Nord);
    assert_eq!(state.profiles().profiles.len(), 1);
    assert_eq!(state.profiles().profiles[0].name, "Prod SSH");
    assert_eq!(state.vault().backend(), VaultBackend::EncryptedFile);
    assert_eq!(state.vault().auto_lock_minutes(), 15);
    assert!(state.reload_errors().is_empty());

    // Existing terminal Session remains alive and interactive across reload.
    assert_eq!(state.active_session_id(), Some(session_id));
    assert_eq!(state.session(session_id).unwrap().exited(), None);
    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"echo HOT_RELOAD_AFTER\r".to_vec(),
    });
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("HOT_RELOAD_AFTER")
    }));
}

#[test]
fn update_retains_active_config_on_malformed_toml_surfaces_toast_and_restores_bak1() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let mut state = AppState::boot_with_paths(paths.clone()).expect("boot AppState");
    let win = window::Id::unique();

    // Save v2 so `.bak.1` holds v1 (`Dark`, `10_000` scrollback).
    let mut cfg_v2 = state.config().clone();
    cfg_v2.terminal.theme = "Dracula".to_string();
    cfg_v2.terminal.scrollback_lines = 15_000;
    save_config(&paths, &cfg_v2).expect("save config v2");
    let _ = state.update(Message::ConfigFilesChanged(vec![ConfigFileKind::AppConfig]));
    assert_eq!(state.theme(win), Theme::Dracula);

    let broken = "schema_version = 1\n[terminal]\nfont_size = \"oops\"\n";
    std::fs::write(paths.config_file(), broken).expect("write malformed config.toml");

    let _ = state.update(Message::ConfigFilesChanged(vec![ConfigFileKind::AppConfig]));

    assert_eq!(state.config().terminal.theme, "Dracula");
    assert_eq!(state.theme(win), Theme::Dracula);
    assert_eq!(state.config().terminal.scrollback_lines, 15_000);
    assert_eq!(
        std::fs::read_to_string(paths.config_file()).unwrap(),
        broken,
        "broken TOML edit must not be overwritten"
    );

    let toast_err = state
        .reload_errors()
        .get(&ConfigFileKind::AppConfig)
        .expect("reload toast error should be surfaced");
    assert_eq!(toast_err.line, Some(3));
    assert!(toast_err.backup_available);
    drop(state.view(win));

    let _ = state.update(Message::RestoreConfigBackup(ConfigFileKind::AppConfig));
    assert!(state.reload_errors().is_empty());
    assert_eq!(state.config().terminal.theme, "Dark");
    assert_eq!(state.theme(win), Theme::Dark);
    assert_eq!(state.config().terminal.scrollback_lines, 10_000);
    assert_eq!(load_config(&paths).unwrap().terminal.scrollback_lines, 10_000);
}

#[test]
fn update_vault_unlock_manual_lock_and_idle_auto_lock_tick_manage_session_secrets() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());

    let mut cfg = AppConfig::default();
    cfg.vault = VaultConfig {
        backend: VaultBackend::EncryptedFile,
        auto_lock_minutes: 30,
    };
    save_config(&paths, &cfg).expect("save config with EncryptedFile vault");

    let clock = Arc::new(ManualVaultClock::new());
    let encrypted = EncryptedVault::with_clock(
        paths.vault_file(),
        Some(Duration::from_secs(30 * 60)),
        clock.clone(),
    );
    let vault = Arc::new(VaultSession::new(
        cfg.vault.clone(),
        Arc::new(InMemorySecretStore::new()),
        encrypted,
    ));

    let mut state = AppState::boot_with_paths_and_vault(paths, vault.clone())
        .expect("boot AppState with custom VaultSession");
    assert!(state.vault().requires_unlock());
    drop(state.view(window::Id::unique()));

    let _ = state.update(Message::VaultInitializeSubmitted(
        "vault-pass-42".to_string(),
    ));
    assert!(!state.vault().requires_unlock());
    assert!(state.vault().is_unlocked());

    state
        .vault()
        .set_secret("ssh/key", "passphrase")
        .expect("store secret in unlocked session");

    let _ = state.update(Message::VaultLock);
    assert!(state.vault().requires_unlock());
    assert!(matches!(
        state.vault().get_secret("ssh/key"),
        Err(VaultError::Locked)
    ));

    let _ = state.update(Message::VaultUnlockSubmitted("wrong-pass".to_string()));
    assert!(state.vault().requires_unlock());
    assert!(state.last_error().is_some());

    let _ = state.update(Message::VaultUnlockSubmitted("vault-pass-42".to_string()));
    assert!(!state.vault().requires_unlock());
    assert_eq!(
        state
            .vault()
            .get_secret("ssh/key")
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("passphrase")
    );

    clock.advance_idle(Duration::from_secs(30 * 60));
    let _ = state.update(Message::VaultAutoLockTick);
    assert!(state.vault().requires_unlock());
    assert!(matches!(
        state.vault().get_secret("ssh/key"),
        Err(VaultError::Locked)
    ));
}

#[test]
fn update_creates_switches_reorders_and_closes_workspace_tabs_with_real_sessions() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");

    assert_eq!(state.workspace().tab_count(), 1);
    let tab1 = state.active_tab_id().expect("initial tab");
    let s1 = state.active_session_id().expect("initial session");

    let _ = state.update(Message::NewTab);
    assert_eq!(state.workspace().tab_count(), 2);
    let tab2 = state.active_tab_id().expect("new active tab");
    let s2 = state.active_session_id().expect("new active session");
    assert_ne!(tab1, tab2);
    assert_ne!(s1, s2);
    assert!(state.session(s1).is_some());
    assert!(state.session(s2).is_some());

    let _ = state.update(Message::SelectTab(tab1));
    assert_eq!(state.active_tab_id(), Some(tab1));
    assert_eq!(state.active_session_id(), Some(s1));

    let _ = state.update(Message::CycleTab(CycleDirection::Next));
    assert_eq!(state.active_tab_id(), Some(tab2));
    assert_eq!(state.active_session_id(), Some(s2));

    let _ = state.update(Message::MoveTab {
        tab_id: tab2,
        to_index: 0,
    });
    assert_eq!(state.workspace().tab_ids(), vec![tab2, tab1]);

    let _ = state.update(Message::CloseTab(tab2));
    assert_eq!(state.workspace().tab_count(), 1);
    assert_eq!(state.active_tab_id(), Some(tab1));
    assert_eq!(state.active_session_id(), Some(s1));
    assert!(state.session(s2).is_none());
}

#[test]
fn update_splits_panes_resizes_dividers_moves_focus_and_closes_panes() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");

    let tab1 = state.active_tab_id().unwrap();
    let p1 = state.workspace().tab(tab1).unwrap().focused_pane;
    let s1 = state.active_session_id().unwrap();

    let _ = state.update(Message::SplitFocusedPane(SplitAxis::Vertical));
    let tab = state.workspace().tab(tab1).unwrap();
    assert_eq!(tab.root.pane_count(), 2);
    let p2 = tab.focused_pane;
    let s2 = state.active_session_id().unwrap();
    assert_ne!(p1, p2);
    assert_ne!(s1, s2);

    let root_split_id = match &tab.root {
        SplitNode::Split { split_id, .. } => *split_id,
        SplitNode::Leaf { .. } => panic!("expected root split"),
    };

    let _ = state.update(Message::SplitFocusedPane(SplitAxis::Horizontal));
    let tab = state.workspace().tab(tab1).unwrap();
    assert_eq!(tab.root.pane_count(), 3);
    let p3 = tab.focused_pane;
    let s3 = state.active_session_id().unwrap();
    assert_ne!(p2, p3);

    let _ = state.update(Message::ResizeSplit {
        split_id: root_split_id,
        ratio: 0.4,
    });
    assert!(
        (state
            .workspace()
            .tab(tab1)
            .unwrap()
            .root
            .find_split(root_split_id)
            .unwrap()
            .1
            - 0.4)
            .abs()
            < 1e-6
    );

    let _ = state.update(Message::MoveFocus(FocusDirection::Left));
    assert_eq!(state.active_session_id(), Some(s1));

    let _ = state.update(Message::CycleFocus(CycleDirection::Next));
    assert_eq!(state.active_session_id(), Some(s2));

    let _ = state.update(Message::FocusSession(s3));
    assert_eq!(state.active_session_id(), Some(s3));

    let _ = state.update(Message::CloseFocusedPane);
    assert!(state.session(s3).is_none());
    assert!(state.session(s1).is_some());
    assert!(state.session(s2).is_some());
    assert_eq!(state.workspace().tab(tab1).unwrap().root.pane_count(), 2);
    assert_eq!(state.active_session_id(), Some(s2));
}

#[test]
fn update_toggles_pane_zoom_and_routes_broadcast_input_with_banner_state() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");

    let tab1 = state.active_tab_id().unwrap();
    let s1 = state.active_session_id().unwrap();
    let _ = state.update(Message::SplitFocusedPane(SplitAxis::Vertical));
    let s2 = state.active_session_id().unwrap();
    let p2 = state.workspace().tab(tab1).unwrap().focused_pane;

    let root_split_id = match &state.workspace().tab(tab1).unwrap().root {
        SplitNode::Split { split_id, .. } => *split_id,
        SplitNode::Leaf { .. } => panic!("expected root split"),
    };
    let _ = state.update(Message::ResizeSplit {
        split_id: root_split_id,
        ratio: 0.35,
    });
    let expected_tree = state.workspace().tab(tab1).unwrap().root.clone();

    // Open a second tab to verify Broadcast Input stays strictly isolated to Tab 1.
    let _ = state.update(Message::NewTab);
    let s3 = state.active_session_id().unwrap();
    let _ = state.update(Message::SelectTab(tab1));

    // Pane Zoom toggle maximizes active pane and restores split geometry on toggle-off.
    assert_eq!(state.active_tab_zoomed_pane(), None);
    let _ = state.update(Message::TogglePaneZoom);
    assert_eq!(state.active_tab_zoomed_pane(), Some(p2));
    assert_eq!(state.workspace().tab(tab1).unwrap().root, expected_tree);

    let _ = state.update(Message::TogglePaneZoom);
    assert_eq!(state.active_tab_zoomed_pane(), None);
    assert_eq!(state.workspace().tab(tab1).unwrap().root, expected_tree);

    // Broadcast Input enables warning banner and mirrors keystrokes to all panes in Tab 1.
    assert!(!state.is_broadcast_banner_visible());
    assert_eq!(state.broadcast_banner_text(), None);

    let _ = state.update(Message::ToggleBroadcastInput);
    assert!(state.is_broadcast_banner_visible());
    let banner = state.broadcast_banner_text().expect("banner text active");
    assert!(banner.contains("BROADCAST"));
    assert!(banner.contains("2 panes"));

    let _ = state.update(Message::TerminalInput {
        session_id: s2,
        bytes: b"echo BCAST_V1_OK\r".to_vec(),
    });

    let pty1 = state.session(s1).unwrap().pty().clone();
    let pty2 = state.session(s2).unwrap().pty().clone();
    assert!(
        pty1.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("BCAST_V1_OK")
        }),
        "pane 1 did not receive broadcast input:\n{}",
        pty1.snapshot().visible_text()
    );
    assert!(
        pty2.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("BCAST_V1_OK")
        }),
        "pane 2 did not receive broadcast input:\n{}",
        pty2.snapshot().visible_text()
    );

    // Sync Tab 2 PTY deterministically and ensure Tab 1's broadcast did not leak there.
    let pty3 = state.session(s3).unwrap().pty().clone();
    pty3.write_bytes(b"echo TAB2_ISOLATED\r").unwrap();
    assert!(
        pty3.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("TAB2_ISOLATED")
        })
    );
    assert!(!pty3.snapshot().visible_text().contains("BCAST_V1_OK"));

    // Turning off Broadcast Input hides banner and stops mirroring.
    let _ = state.update(Message::SetBroadcastInput(false));
    assert!(!state.is_broadcast_banner_visible());
    assert_eq!(state.broadcast_banner_text(), None);

    let _ = state.update(Message::TerminalInput {
        session_id: s2,
        bytes: b"echo SINGLE_ONLY_OK\r".to_vec(),
    });
    assert!(pty2.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("SINGLE_ONLY_OK")
    }));

    pty1.write_bytes(b"echo PANE1_SYNC\r").unwrap();
    assert!(pty1.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("PANE1_SYNC")
    }));
    assert!(!pty1.snapshot().visible_text().contains("SINGLE_ONLY_OK"));
}

#[test]
fn workspace_shortcuts_map_cleanly_without_hijacking_terminal_keys() {
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
    let ctrl_alt = Modifiers::CTRL | Modifiers::ALT;
    let tab = Some(TabId(7));

    assert!(matches!(
        shortcut_for_key_event(&Key::Character("T".into()), ctrl_shift, tab),
        Some(Message::NewTab)
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("w".into()), ctrl_shift, tab),
        Some(Message::CloseFocusedPane)
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("D".into()), ctrl_shift, tab),
        Some(Message::SplitFocusedPane(SplitAxis::Vertical))
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("o".into()), ctrl_shift, tab),
        Some(Message::SplitFocusedPane(SplitAxis::Horizontal))
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("z".into()), ctrl_shift, tab),
        Some(Message::TogglePaneZoom)
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("B".into()), ctrl_shift, tab),
        Some(Message::ToggleBroadcastInput)
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Named(Named::Tab), Modifiers::CTRL, tab),
        Some(Message::CycleTab(CycleDirection::Next))
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Named(Named::Tab), ctrl_shift, tab),
        Some(Message::CycleTab(CycleDirection::Previous))
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Named(Named::PageUp), ctrl_shift, tab),
        Some(Message::MoveTabBy {
            tab_id: TabId(7),
            delta: -1
        })
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Named(Named::ArrowRight), ctrl_alt, tab),
        Some(Message::MoveFocus(FocusDirection::Right))
    ));

    // Regular shell control keys and reserved shortcuts (Ctrl+B, Ctrl+K, Ctrl+F, Ctrl+Shift+E)
    // must NOT be hijacked by Ticket 02 workspace shortcuts.
    assert!(shortcut_for_key_event(&Key::Character("c".into()), Modifiers::CTRL, tab).is_none());
    assert!(shortcut_for_key_event(&Key::Character("d".into()), Modifiers::CTRL, tab).is_none());
    assert!(shortcut_for_key_event(&Key::Character("b".into()), Modifiers::CTRL, tab).is_none());
    assert!(shortcut_for_key_event(&Key::Character("f".into()), Modifiers::CTRL, tab).is_none());
    assert!(shortcut_for_key_event(&Key::Character("k".into()), Modifiers::CTRL, tab).is_none());
    assert!(shortcut_for_key_event(&Key::Character("E".into()), ctrl_shift, tab).is_none());
}
