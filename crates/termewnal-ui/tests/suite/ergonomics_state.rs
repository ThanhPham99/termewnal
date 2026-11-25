use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::advanced::input_method;
use iced::{Rectangle, Size, Theme, window};
use tempfile::TempDir;
use termewnal_core::{AppConfig, BuiltinTheme, ConfigPaths, CursorStyle, save_config};
use termewnal_pty::{CursorShape, PtyEvent, SelectionType, Side};
use termewnal_ui::{
    AppState, FileDropAction, ImeComposition, ImeUpdate, Message, TerminalPalette,
    classify_file_drop, format_paste_bytes, ime_cursor_anchor_rect, is_multiline_paste,
};

fn boot_fast_test_state() -> (TempDir, AppState) {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    #[cfg(unix)]
    if Path::new("/bin/sh").exists() {
        let mut cfg = AppConfig::default();
        cfg.terminal.shell = Some("/bin/sh".to_string());
        save_config(&paths, &cfg).expect("write deterministic test shell config");
    }
    let state = AppState::boot_with_paths(paths).expect("boot AppState");
    (temp, state)
}

#[test]
fn builtin_themes_and_terminal_options_apply_immediately_from_config() {
    let (_temp, mut state) = boot_fast_test_state();
    let session_id = state.active_session_id().expect("active session");
    let win = window::Id::unique();

    let theme_cases = [
        ("Catppuccin", Theme::CatppuccinMocha, BuiltinTheme::Catppuccin),
        ("Tokyo Night", Theme::TokyoNight, BuiltinTheme::TokyoNight),
        ("Dracula", Theme::Dracula, BuiltinTheme::Dracula),
        ("Nord", Theme::Nord, BuiltinTheme::Nord),
        ("Dark", Theme::Dark, BuiltinTheme::Dark),
        ("Light", Theme::Light, BuiltinTheme::Light),
    ];

    for (name, expected_iced_theme, expected_builtin) in theme_cases {
        let mut cfg = state.config().terminal.clone();
        cfg.theme = name.to_string();
        cfg.font_family = "JetBrains Mono".to_string();
        cfg.font_size = 16.5;
        cfg.font_ligatures = true;
        cfg.cursor_style = CursorStyle::Beam;
        cfg.copy_on_select = true;

        let _ = state.update(Message::ApplyTerminalConfig(cfg));

        assert_eq!(state.theme(win), expected_iced_theme);
        let palette = TerminalPalette::from_config(&state.config().terminal);
        assert_eq!(palette.theme, expected_builtin);
        assert_eq!(state.config().terminal.font_family, "JetBrains Mono");
        assert_eq!(state.config().terminal.font_size, 16.5);
        assert!(state.config().terminal.font_ligatures);
        assert!(state.config().terminal.copy_on_select);
        if let Some(cursor) = state.session(session_id).unwrap().snapshot().cursor {
            assert_eq!(cursor.shape, CursorShape::Beam);
        }
    }

    let mut underline_cfg = state.config().terminal.clone();
    underline_cfg.cursor_style = CursorStyle::Underline;
    let _ = state.update(Message::ApplyTerminalConfig(underline_cfg));
    if let Some(cursor) = state.session(session_id).unwrap().snapshot().cursor {
        assert_eq!(cursor.shape, CursorShape::Underline);
    }
}

#[test]
fn ctrl_f_search_updates_matches_navigates_next_prev_and_closes_on_escape() {
    let (_temp, mut state) = boot_fast_test_state();
    let session_id = state.active_session_id().expect("active session");

    let _ = state.update(Message::ViewportResized {
        session_id,
        cols: 80,
        rows: 8,
    });
    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"printf '%s_%s_%s\\n' NEEDLE ONE 1; i=1; while [ \"$i\" -le 20 ]; do echo \"pad_$i\"; i=$((i+1)); done; printf '%s_%s_%s\\n' NEEDLE ONE 2; printf 'READY_%s\\n' SEARCH\r".to_vec(),
    });

    let pty = state.session(session_id).unwrap().pty().clone();
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("READY_SEARCH")
    }));
    let _ = state.update(Message::PtyEvent {
        session_id,
        event: PtyEvent::Output,
    });

    let _ = state.update(Message::OpenSearch);
    assert!(state.search().open);
    assert!(state.search().input_focused);

    let _ = state.update(Message::SearchQueryChanged("needle_one".to_string()));
    assert_eq!(state.search().matches.len(), 2);
    assert_eq!(state.search().active_index, Some(0));
    assert_eq!(state.search().status_label(), "1 / 2");

    // Active match 0 is in scrollback, so viewport must scroll to reveal it.
    let snap_after_query = state.session(session_id).unwrap().snapshot();
    assert!(snap_after_query.display_offset > 0);
    assert!(snap_after_query.visible_text().contains("NEEDLE_ONE_1"));

    // Next moves to 2 / 2, wrapping back to 1 / 2; Prev wraps back to 2 / 2.
    let _ = state.update(Message::SearchNext);
    assert_eq!(state.search().active_index, Some(1));
    assert_eq!(state.search().status_label(), "2 / 2");
    assert!(state.session(session_id).unwrap().snapshot().visible_text().contains("NEEDLE_ONE_2"));

    let _ = state.update(Message::SearchNext);
    assert_eq!(state.search().active_index, Some(0));

    let _ = state.update(Message::SearchPrev);
    assert_eq!(state.search().active_index, Some(1));

    // Escape closes the search bar when search input has focus.
    let _ = state.update(Message::EscapePressed);
    assert!(!state.search().open);
    assert!(state.search().matches.is_empty());
}

#[test]
fn copy_on_select_copies_dragged_selection_only_when_enabled() {
    let (_temp, mut state) = boot_fast_test_state();
    let session_id = state.active_session_id().expect("active session");

    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"printf 'COPY_%s_LINE\\n' TARGET\r".to_vec(),
    });
    let pty = state.session(session_id).unwrap().pty().clone();
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("COPY_TARGET_LINE")
    }));
    let _ = state.update(Message::PtyEvent {
        session_id,
        event: PtyEvent::Output,
    });

    let snap = state.session(session_id).unwrap().snapshot().clone();
    let row = (0..snap.rows as usize)
        .find(|&r| snap.row_text(r).contains("COPY_TARGET_LINE"))
        .expect("find output row");
    let base_col = snap
        .row_text(row)
        .find("COPY_TARGET_LINE")
        .expect("find COPY_TARGET_LINE col");

    // Disabled by default: SelectionFinish does not copy automatically.
    let _ = state.update(Message::SelectionStart {
        session_id,
        col: base_col + 5,
        row,
        side: Side::Left,
        selection_type: SelectionType::Simple,
    });
    let _ = state.update(Message::SelectionUpdate {
        session_id,
        col: base_col + 10,
        row,
        side: Side::Right,
    });
    let _ = state.update(Message::SelectionFinish { session_id });
    assert_eq!(state.last_copied_text(), None);

    // Manual CopySelection always copies active selection.
    let _ = state.update(Message::CopySelection { session_id });
    assert_eq!(state.last_copied_text(), Some("TARGET"));

    // Enabling copy_on_select copies automatically on SelectionFinish.
    let mut cfg = state.config().terminal.clone();
    cfg.copy_on_select = true;
    let _ = state.update(Message::ApplyTerminalConfig(cfg));

    let _ = state.update(Message::SelectionStart {
        session_id,
        col: base_col,
        row,
        side: Side::Left,
        selection_type: SelectionType::Simple,
    });
    let _ = state.update(Message::SelectionUpdate {
        session_id,
        col: base_col + 3,
        row,
        side: Side::Right,
    });
    let _ = state.update(Message::SelectionFinish { session_id });
    assert_eq!(state.last_copied_text(), Some("COPY"));
}

#[test]
fn open_url_dispatches_to_boundary_opener_without_launching_real_browser() {
    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    let opened_ref = Arc::clone(&opened);

    let (_temp, base_state) = boot_fast_test_state();
    let mut state = base_state.with_url_opener(Arc::new(move |url| {
        opened_ref.lock().unwrap().push(url.to_string());
        Ok(())
    }));

    let _ = state.update(Message::OpenUrl("https://termewnal.example/docs".to_string()));
    assert_eq!(
        opened.lock().unwrap().as_slice(),
        &["https://termewnal.example/docs"]
    );
}

#[test]
fn multiline_paste_guard_prompts_confirms_cancels_and_respects_config_toggle() {
    assert!(!is_multiline_paste("echo hello"));
    assert!(is_multiline_paste("echo hello\n"));
    assert!(is_multiline_paste("line1\r\nline2"));
    assert_eq!(
        format_paste_bytes("a\nb\x1b[201~c", true),
        b"\x1b[200~a\nbc\x1b[201~"
    );
    assert_eq!(format_paste_bytes("a\r\nb\n", false), b"a\rb\r");

    let (_temp, mut state) = boot_fast_test_state();
    let session_id = state.active_session_id().expect("active session");
    let pty = state.session(session_id).unwrap().pty().clone();

    // Single-line paste without newlines goes straight through without prompting.
    let _ = state.update(Message::PasteText {
        session_id,
        text: "printf 'PLAIN_%s\\n' FAST".to_string(),
    });
    assert!(state.pending_paste().is_none());
    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"\r".to_vec(),
    });
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("PLAIN_FAST")
    }));

    // Multi-line paste pauses in PendingPaste and Cancel drops it cleanly.
    let _ = state.update(Message::PasteText {
        session_id,
        text: "printf 'CANCELED_%s\\n' LINE1\nprintf 'CANCELED_%s\\n' LINE2\n".to_string(),
    });
    let pending = state.pending_paste().expect("multiline paste must prompt");
    assert!(pending.line_count >= 2);
    let _ = state.update(Message::CancelPendingPaste);
    assert!(state.pending_paste().is_none());
    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"printf 'AFTER_CANCEL_%s\\n' OK\r".to_vec(),
    });
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("AFTER_CANCEL_OK")
    }));
    assert!(!pty.snapshot().visible_text().contains("CANCELED_LINE1"));

    // ConfirmPendingPaste dispatches buffered multi-line text to active Session.
    let _ = state.update(Message::PasteText {
        session_id,
        text: "printf 'CONFIRMED_%s\\n' ONE\nprintf 'CONFIRMED_%s\\n' TWO\n".to_string(),
    });
    assert!(state.pending_paste().is_some());
    let _ = state.update(Message::ConfirmPendingPaste);
    assert!(state.pending_paste().is_none());
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("CONFIRMED_ONE") && s.visible_text().contains("CONFIRMED_TWO")
    }));

    // Disabling confirm_multiline_paste bypasses the prompt.
    let mut cfg = state.config().terminal.clone();
    cfg.confirm_multiline_paste = false;
    let _ = state.update(Message::ApplyTerminalConfig(cfg));
    let _ = state.update(Message::PasteText {
        session_id,
        text: "printf 'DIRECT_%s\\n' MULTI\n".to_string(),
    });
    assert!(state.pending_paste().is_none());
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("DIRECT_MULTI")
    }));
}

#[test]
fn ime_preedit_commit_and_cursor_anchor_handle_vietnamese_and_cjk() {
    let mut ime = ImeComposition::default();
    assert_eq!(
        ime.apply_event(&input_method::Event::Preedit(
            "tiến".to_string(),
            Some(0..5)
        )),
        ImeUpdate::Redraw
    );
    assert_eq!(ime.content, "tiến");
    let iced_pre = ime.to_iced_preedit(14.0).expect("active preedit");
    assert_eq!(iced_pre.content, "tiến");
    assert_eq!(iced_pre.selection, Some(0..5));

    assert_eq!(
        ime.apply_event(&input_method::Event::Commit("tiếng Việt ".to_string())),
        ImeUpdate::Commit("tiếng Việt ".to_string())
    );
    assert!(ime.content.is_empty());

    let _ = ime.apply_event(&input_method::Event::Preedit(
        "にほん".to_string(),
        Some(0..9),
    ));
    assert_eq!(
        ime.apply_event(&input_method::Event::Closed),
        ImeUpdate::Redraw
    );
    assert!(ime.content.is_empty());

    // Anchor rect follows visible terminal cursor cell.
    let (_temp, mut state) = boot_fast_test_state();
    let session_id = state.active_session_id().expect("active session");
    let pty = state.session(session_id).unwrap().pty().clone();

    let snap = state.session(session_id).unwrap().snapshot();
    let anchor = ime_cursor_anchor_rect(
        snap,
        Rectangle::new(iced::Point::new(10.0, 20.0), Size::new(800.0, 480.0)),
        Size::new(8.0, 16.0),
    );
    assert!(anchor.x >= 10.0 && anchor.y >= 20.0);

    // Committing UTF-8 Vietnamese + CJK text dispatches intact bytes into active Session.
    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"printf 'IME:%s\\n' '".to_vec(),
    });
    let _ = state.update(Message::ImeCommitted {
        session_id,
        text: "Tiếng Việt 日本語".to_string(),
    });
    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"'\r".to_vec(),
    });
    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("IME:Tiếng Việt 日本語")
    }));
}

#[test]
fn file_dropped_inserts_shell_escaped_path_and_classifies_sftp_routing() {
    let (_temp, mut state) = boot_fast_test_state();
    let session_id = state.active_session_id().expect("active session");
    let pty = state.session(session_id).unwrap().pty().clone();

    // Pure routing classifier leaves Shift-drop SFTP upload ready for Ticket 13.
    assert_eq!(
        classify_file_drop(session_id, Path::new("/tmp/a b.txt"), false, false, false),
        FileDropAction::InsertShellPath {
            session_id,
            escaped_text: "'/tmp/a b.txt' ".to_string(),
        }
    );
    assert_eq!(
        classify_file_drop(session_id, Path::new("/tmp/a b.txt"), true, true, false),
        FileDropAction::UploadToSftp {
            session_id,
            path: PathBuf::from("/tmp/a b.txt"),
        }
    );

    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"printf 'DROP:%s\\n' ".to_vec(),
    });
    let _ = state.update(Message::FileDropped {
        session_id,
        path: PathBuf::from("/tmp/My File's (1).txt"),
        shift_held: false,
    });
    let _ = state.update(Message::TerminalInput {
        session_id,
        bytes: b"\r".to_vec(),
    });

    assert!(pty.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("DROP:/tmp/My File's (1).txt")
    }));
}
