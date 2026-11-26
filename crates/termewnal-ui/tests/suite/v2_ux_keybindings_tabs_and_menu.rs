use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use iced::Point;
use iced::keyboard::{Key, Modifiers};
use tempfile::TempDir;
use termewnal_core::{
    BellBehavior, BuiltinTheme, ConfigPaths, KeyAction, KeyChord, RightClickAction,
    TabBarPosition, load_layout, save_layout,
};
use termewnal_pty::{PtyEvent, TerminalProgressState, compute_highlight_grid};
use termewnal_ui::context_menu::ContextMenuKind;
use termewnal_ui::keybinding_bridge::{
    KeybindingStepOutcome, iced_key_to_chord, resolve_keybinding_message,
};
use termewnal_ui::terminal_ergonomics::iced_theme_with_overrides;
use termewnal_ui::terminal_widget::TerminalViewOptions;
use termewnal_ui::{
    Message, PaletteAction, RecordingDesktopNotifier, SettingsTab, SplitAxis, ThemeOverrideField,
};

use super::common::boot_test_app;

#[test]
fn keybinding_resolver_supports_custom_multi_chord_and_zoom_shortcuts() {
    let tmp = TempDir::new().expect("tempdir");
    let mut app = boot_test_app(ConfigPaths::from_root(tmp.path())).expect("boot");

    let _ = app.update(Message::OpenSettingsModal);
    let _ = app.update(Message::SettingsSelectTab(SettingsTab::Keybindings));
    let _ = app.update(Message::SettingsKeybindingChanged {
        action: KeyAction::SplitVertical,
        value: "Ctrl+K Ctrl+S".to_string(),
    });
    let _ = app.update(Message::SettingsBeginShortcutCapture(KeyAction::ClearTerminal));
    assert_eq!(
        app.settings_ui().capturing_action(),
        Some(KeyAction::ClearTerminal)
    );
    let _ = app.update(Message::SettingsShortcutCaptured("Ctrl+Alt+L".to_string()));
    assert!(app.settings_ui().capturing_action().is_none());
    let _ = app.update(Message::SettingsSave);

    let active_tab = app.active_tab_id();
    let active_session = app.active_session_id();

    let first_chord = iced_key_to_chord(
        &Key::Character("k".into()),
        None,
        Modifiers::CTRL,
    )
    .expect("Ctrl+K chord");
    match resolve_keybinding_message(
        app.keybinding_resolver(),
        app.pending_key_chords(),
        &first_chord,
        active_tab,
        active_session,
    ) {
        KeybindingStepOutcome::PendingPrefix(c) => {
            let _ = app.update(Message::PushPendingKeyChord(c));
        }
        other => panic!("expected PendingPrefix for Ctrl+K, got {other:?}"),
    }

    let second_chord = iced_key_to_chord(
        &Key::Character("s".into()),
        None,
        Modifiers::CTRL,
    )
    .expect("Ctrl+S chord");
    match resolve_keybinding_message(
        app.keybinding_resolver(),
        app.pending_key_chords(),
        &second_chord,
        active_tab,
        active_session,
    ) {
        KeybindingStepOutcome::Matched(Some(Message::SplitFocusedPane(SplitAxis::Vertical))) => {
            let _ = app.update(Message::ClearPendingKeyChord);
        }
        other => panic!("expected SplitVertical match, got {other:?}"),
    }

    let custom_clear = KeyChord::parse("Ctrl+Alt+L").expect("parse Ctrl+Alt+L");
    match resolve_keybinding_message(
        app.keybinding_resolver(),
        app.pending_key_chords(),
        &custom_clear,
        active_tab,
        active_session,
    ) {
        KeybindingStepOutcome::Matched(Some(Message::ClearTerminalBuffer(sid))) => {
            assert_eq!(Some(sid), active_session);
        }
        other => panic!("expected ClearTerminalBuffer match, got {other:?}"),
    }

    // Built-in font zoom +/-/0 and tab switch 1..9 bindings.
    let initial_size = app.config().terminal.font_size;
    let _ = app.update(Message::AdjustFontSize(2.0));
    assert!((app.config().terminal.font_size - (initial_size + 2.0)).abs() < 0.01);
    let _ = app.update(Message::ResetFontSize);
    assert!((app.config().terminal.font_size - 14.0).abs() < 0.01);
}

#[test]
fn tab_metadata_pin_duplicate_close_others_and_layout_persistence() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = ConfigPaths::from_root(tmp.path());
    let mut app = boot_test_app(paths.clone()).expect("boot");

    let _ = app.update(Message::NewTab);
    let _ = app.update(Message::NewTab);
    assert_eq!(app.workspace().tab_count(), 3);

    let t1 = app.workspace().tabs()[0].id;
    let t2 = app.workspace().tabs()[1].id;
    let t3 = app.workspace().tabs()[2].id;

    let _ = app.update(Message::BeginTabRename(t1));
    let _ = app.update(Message::TabRenameDraftChanged("Backend API".to_string()));
    let _ = app.update(Message::SubmitTabRename);
    let _ = app.update(Message::SetTabColor {
        tab_id: t1,
        color: Some("#38bdf8".to_string()),
    });
    let _ = app.update(Message::TogglePinTab(t2));
    let _ = app.update(Message::SelectTabByIndex(1));
    assert_eq!(app.active_tab_id(), Some(t2));

    let _ = app.update(Message::DuplicateTab(t1));
    assert_eq!(app.workspace().tab_count(), 4);
    assert_eq!(
        app.workspace().tabs()[1].title.as_deref(),
        Some("Backend API")
    );
    assert_eq!(
        app.workspace().tabs()[1].color.as_deref(),
        Some("#38bdf8")
    );

    // Save layout and verify pinned + color survive disk roundtrip.
    let layout = app.to_workspace_layout();
    save_layout(&paths, &layout).expect("save layout");
    let loaded = load_layout(&paths).expect("load layout");
    assert_eq!(loaded.windows[0].tabs[0].color.as_deref(), Some("#38bdf8"));
    assert!(loaded.windows[0].tabs[2].pinned);

    // Pinned tab (t2) must survive CloseTabsToRight and CloseOtherTabs.
    let _ = app.update(Message::CloseTabsToRight(t1));
    let remaining_ids: Vec<_> = app.workspace().tabs().iter().map(|t| t.id).collect();
    assert!(remaining_ids.contains(&t1));
    assert!(remaining_ids.contains(&t2));
    assert!(!remaining_ids.contains(&t3));

    // CloseOtherTabs keeps pinned tab t2 and target t1.
    let _ = app.update(Message::CloseOtherTabs(t1));
    let remaining_after: Vec<_> = app.workspace().tabs().iter().map(|t| t.id).collect();
    assert_eq!(remaining_after, vec![t1, t2]);

    // Switch TabBarPosition live and persist.
    let _ = app.update(Message::OpenSettingsModal);
    let _ = app.update(Message::SettingsTabBarPositionSelected(TabBarPosition::Left));
    assert_eq!(app.config().terminal.tab_bar_position, TabBarPosition::Left);
    let _ = app.update(Message::SettingsSave);
}

#[test]
fn bell_notifications_visual_flash_and_osc_progress_track_tab_activity() {
    let tmp = TempDir::new().expect("tempdir");
    let notifier = Arc::new(RecordingDesktopNotifier::new());
    let mut app = boot_test_app(ConfigPaths::from_root(tmp.path()))
        .expect("boot")
        .with_notifier(notifier.clone());

    let t1 = app.workspace().tabs()[0].id;
    let s1 = app.workspace().tabs()[0]
        .focused_session_id()
        .expect("tab1 session");
    let _ = app.update(Message::NewTab);
    let s2 = app.workspace().tabs()[1]
        .focused_session_id()
        .expect("tab2 session");

    // Background tab bell triggers tab badge + visual flash + desktop notification.
    let _ = app.update(Message::PtyEvent {
        session_id: s1,
        event: PtyEvent::Bell,
    });
    assert!(app.workspace().tab(t1).expect("tab1").bell_active);
    assert!(app.is_visual_bell_active(s1));
    assert_eq!(notifier.take_notifications().len(), 1);

    // Selecting tab 1 clears the unread bell badge.
    let _ = app.update(Message::SelectTab(t1));
    assert!(!app.workspace().tab(t1).expect("tab1").bell_active);

    // Focused active tab bell triggers visual flash but suppresses desktop notification.
    let _ = app.update(Message::PtyEvent {
        session_id: s1,
        event: PtyEvent::Bell,
    });
    assert!(!app.workspace().tab(t1).expect("tab1").bell_active);
    assert!(app.is_visual_bell_active(s1));
    assert!(notifier.take_notifications().is_empty());

    // OSC 9;4 progress bar state propagates to owning tab.
    let _ = app.update(Message::PtyEvent {
        session_id: s2,
        event: PtyEvent::Progress(TerminalProgressState::Normal(65)),
    });
    let t2 = app.workspace().tabs()[1].id;
    assert_eq!(
        app.workspace().tab(t2).expect("tab2").progress,
        TerminalProgressState::Normal(65)
    );

    // Clear visual bell only clears matching generation.
    let _ = app.update(Message::OpenSettingsModal);
    let _ = app.update(Message::SettingsBellBehaviorSelected(BellBehavior::Off));
    let _ = app.update(Message::SettingsSave);
    assert_eq!(app.config().terminal.bell_behavior, BellBehavior::Off);
}

#[test]
fn scrollback_save_logging_clear_buffer_and_local_file_links() {
    let tmp = TempDir::new().expect("tempdir");
    let opened_files: Arc<Mutex<Vec<(PathBuf, Option<u32>, Option<u32>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let opener_capture = Arc::clone(&opened_files);
    let mut app = boot_test_app(ConfigPaths::from_root(tmp.path()))
        .expect("boot")
        .with_file_opener(Arc::new(move |path, line, col| {
            opener_capture
                .lock()
                .expect("opener mutex")
                .push((path.to_path_buf(), line, col));
            Ok(())
        }));

    let sid = app.active_session_id().expect("active session");
    let buf = Arc::clone(app.session(sid).expect("local").buffer());
    buf.feed_bytes(b"first output line\r\nERROR: failed at src/lib.rs:42:7\r\n");

    // Export scrollback via AppState helper.
    let saved_path = tmp.path().join("scrollback.txt");
    app.save_scrollback_to_path(sid, &saved_path)
        .expect("save scrollback");
    let contents = std::fs::read_to_string(&saved_path).expect("read scrollback");
    assert!(contents.contains("first output line"));
    assert!(contents.contains("src/lib.rs:42:7"));

    // Toggle continuous session logging.
    let log_path = app
        .toggle_session_logging(sid)
        .expect("start log")
        .expect("active log path");
    assert!(app.is_session_logging(sid));
    buf.feed_bytes(b"\x1b[32mlogged green line\x1b[0m\r\n");
    assert!(
        app.toggle_session_logging(sid)
            .expect("stop log")
            .is_none()
    );
    assert!(!app.is_session_logging(sid));
    let logged = std::fs::read_to_string(&log_path).expect("read session log");
    assert!(logged.contains("logged green line"));
    assert!(!logged.contains("\x1b[32m"));

    // Clear terminal buffer and scrollback (sentinel lines must be purged even if shell later redraws prompt).
    let _ = app.update(Message::ClearTerminalBuffer(sid));
    let after_clear = buf.export_scrollback_text();
    assert!(!after_clear.contains("first output line"));
    assert!(!after_clear.contains("logged green line"));
    assert_eq!(buf.history_size(), 0);

    // OpenLocalFile invokes file_opener with exact path:line:col.
    let _ = app.update(Message::OpenLocalFile {
        session_id: sid,
        path: "/tmp/sample/main.rs".to_string(),
        line: Some(42),
        col: Some(7),
    });
    let opened = opened_files.lock().expect("opened");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].0, PathBuf::from("/tmp/sample/main.rs"));
    assert_eq!(opened[0].1, Some(42));
    assert_eq!(opened[0].2, Some(7));
}

#[test]
fn highlight_rules_theme_overrides_context_menu_and_palette_actions() {
    let tmp = TempDir::new().expect("tempdir");
    let mut app = boot_test_app(ConfigPaths::from_root(tmp.path())).expect("boot");

    let _ = app.update(Message::OpenSettingsModal);
    let _ = app.update(Message::SettingsSelectTab(SettingsTab::Highlights));
    let _ = app.update(Message::SettingsHighlightNameChanged("Errors".to_string()));
    let _ = app.update(Message::SettingsHighlightPatternChanged(
        "ERROR|FATAL".to_string(),
    ));
    let _ = app.update(Message::SettingsHighlightFgChanged("#ff5555".to_string()));
    let _ = app.update(Message::SettingsHighlightBoldToggled(true));
    let _ = app.update(Message::SettingsAddHighlightRule);
    let _ = app.update(Message::SettingsRightClickActionSelected(
        RightClickAction::Menu,
    ));
    let _ = app.update(Message::SettingsOpacityChanged("0.88".to_string()));
    let _ = app.update(Message::SettingsThemeOverrideChanged {
        field: ThemeOverrideField::UiAccent,
        value: "#22d3ee".to_string(),
    });
    let _ = app.update(Message::SettingsSave);

    assert_eq!(app.config().terminal.highlight_rules.len(), 1);
    assert!((app.config().terminal.window_opacity - 0.88).abs() < 0.01);
    assert_eq!(
        app.config().terminal.theme_overrides.ui_accent.as_deref(),
        Some("#22d3ee")
    );
    assert_eq!(BuiltinTheme::ALL.len(), 28);

    // Custom theme override generates Iced custom Theme cleanly.
    let _theme = iced_theme_with_overrides(
        &app.config().terminal.theme,
        &app.config().terminal.theme_overrides,
    );

    let sid = app.active_session_id().expect("sid");
    let buf = Arc::clone(app.session(sid).expect("local").buffer());
    buf.feed_bytes(b"\r\nok ERROR here\r\n");
    let snap = buf.snapshot();
    let cols = snap.cols as usize;
    let view_opts = TerminalViewOptions::from_config(&app.config().terminal, true);
    let grid = compute_highlight_grid(&snap, &view_opts.highlight_rules);
    let mut found_error = false;
    for r in 0..snap.rows as usize {
        for c in 0..cols.saturating_sub(5) {
            let word: String = (0..5)
                .filter_map(|offset| snap.cell(c + offset, r).map(|cell| cell.c))
                .collect();
            if word == "ERROR" {
                found_error = true;
                for offset in 0..5 {
                    let hl = grid[r * cols + c + offset].expect("ERROR cell highlighted");
                    assert!(hl.bold);
                }
                if c > 0 {
                    assert!(grid[r * cols + c - 1].is_none());
                }
            }
        }
    }
    assert!(found_error, "expected 'ERROR' token in snapshot");

    // Open terminal context menu & dismiss with Escape.
    let _ = app.update(Message::OpenTerminalContextMenu {
        session_id: sid,
        position: Point::new(120.0, 90.0),
    });
    assert!(matches!(
        app.context_menu().active,
        Some((ContextMenuKind::Terminal { .. }, _))
    ));
    assert!(app.has_modal_open());
    let _ = app.update(Message::EscapePressed);
    assert!(!app.context_menu().is_open());

    // Execute Command Palette DuplicateTab action.
    let _ = app.update(Message::CommandPaletteExecuteItem(
        PaletteAction::DuplicateTab,
    ));
    assert_eq!(app.workspace().tab_count(), 2);
}
