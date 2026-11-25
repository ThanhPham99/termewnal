use std::time::Duration;

use iced::keyboard::{Key, Modifiers, key::Named};
use iced::{Theme, window};
use tempfile::TempDir;
use termewnal_core::ConfigPaths;
use termewnal_pty::PtyEvent;
use termewnal_ui::{AppState, Message, key_event_to_bytes};

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
