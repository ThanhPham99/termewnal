use std::time::Duration;

use iced::window;
use tempfile::TempDir;
use termewnal_core::{
    CURRENT_SCHEMA_VERSION, ConfigPaths, LayoutSplitAxis, LayoutSplitNode, Profile,
    ProfilesConfig, SessionDescriptor, SshAuthMethod, TabLayout, WindowLayout, WorkspaceLayout,
    save_layout, save_profiles,
};
use termewnal_ssh::TransportStatus;
use termewnal_ui::ipc::{IpcCommand, LaunchOutcome, acquire_or_forward};
use termewnal_ui::{
    BackgroundWorkStatus, CloseGuardState, DaemonLifecycle, Message, SplitAxis, SplitNode,
};

#[test]
fn close_guard_triggers_only_on_last_window_when_background_work_is_active() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        super::common::boot_test_app(ConfigPaths::from_root(temp.path())).expect("boot AppState");

    let w1 = window::Id::unique();
    let w2 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));
    let _ = state.update(Message::WindowOpened(w2));
    assert_eq!(state.windows().len(), 2);

    let tracker = state.background_work_tracker().clone();
    tracker.set_active_standalone_tunnels(1);
    assert_eq!(
        tracker.status(),
        BackgroundWorkStatus {
            active_standalone_tunnels: 1,
            active_transfers: 0,
        }
    );

    // Closing a non-last window must NOT trigger the Close Guard prompt.
    let _ = state.update(Message::CloseRequested(w1));
    assert_eq!(state.close_guard(), CloseGuardState::Idle);
    let _ = state.update(Message::WindowClosed(w1));
    assert_eq!(state.windows().len(), 1);
    assert!(state.windows().contains(&w2));
    assert_eq!(state.lifecycle(), DaemonLifecycle::Active);

    // Closing the LAST window with active background work enters Confirming state.
    let _ = state.update(Message::CloseRequested(w2));
    assert_eq!(
        state.close_guard(),
        CloseGuardState::Confirming {
            window_id: w2,
            active_standalone_tunnels: 1,
            active_transfers: 0,
        }
    );
    assert!(state.windows().contains(&w2));

    // Canceling dismisses the dialog without closing the window.
    let _ = state.update(Message::CloseGuardCancel);
    assert_eq!(state.close_guard(), CloseGuardState::Idle);
    assert!(state.windows().contains(&w2));
    assert_eq!(state.lifecycle(), DaemonLifecycle::Active);

    // Keep Running in Background closes the GUI window while keeping the daemon alive.
    tracker.set_active_transfers(2);
    let _ = state.update(Message::CloseRequested(w2));
    assert_eq!(
        state.close_guard(),
        CloseGuardState::Confirming {
            window_id: w2,
            active_standalone_tunnels: 1,
            active_transfers: 2,
        }
    );

    let _ = state.update(Message::CloseGuardKeepRunningInBackground);
    assert_eq!(state.close_guard(), CloseGuardState::Idle);
    assert_eq!(state.lifecycle(), DaemonLifecycle::BackgroundRunning);
    assert!(!tracker.stop_requested());

    let _ = state.update(Message::WindowClosed(w2));
    assert!(state.windows().is_empty());
    assert_eq!(state.lifecycle(), DaemonLifecycle::BackgroundRunning);
    assert!(!tracker.stop_requested());

    // Subsequent IPC OpenWindow while running in background restores the window cleanly.
    let _ = state.update(Message::IpcCommand(IpcCommand::OpenWindow));
    assert_eq!(state.windows().len(), 1);
    assert_eq!(state.lifecycle(), DaemonLifecycle::Active);
}

#[test]
fn close_guard_stop_and_quit_stops_background_work_and_idle_last_window_exits_directly() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        super::common::boot_test_app(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));

    let tracker = state.background_work_tracker().clone();
    tracker.set_active_transfers(3);

    let _ = state.update(Message::CloseRequested(w1));
    assert_eq!(
        state.close_guard(),
        CloseGuardState::Confirming {
            window_id: w1,
            active_standalone_tunnels: 0,
            active_transfers: 3,
        }
    );

    let _ = state.update(Message::CloseGuardStopAndQuit);
    assert_eq!(state.close_guard(), CloseGuardState::Idle);
    assert_eq!(state.lifecycle(), DaemonLifecycle::Exiting);
    assert!(tracker.stop_requested());

    // When idle (no active Standalone Tunnels or Transfers), closing last window skips modal and exits.
    let temp2 = TempDir::new().expect("create second temp XDG root");
    let mut idle_state =
        super::common::boot_test_app(ConfigPaths::from_root(temp2.path())).expect("boot idle AppState");
    let w2 = window::Id::unique();
    let _ = idle_state.update(Message::WindowOpened(w2));

    let _ = idle_state.update(Message::CloseRequested(w2));
    assert_eq!(idle_state.close_guard(), CloseGuardState::Idle);

    let _ = idle_state.update(Message::WindowClosed(w2));
    assert!(idle_state.windows().is_empty());
    assert_eq!(idle_state.lifecycle(), DaemonLifecycle::Exiting);
}

#[test]
fn multi_window_ipc_open_window_and_open_tab_share_runtime_with_isolated_workspaces() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let ipc_handle = match acquire_or_forward(&paths, IpcCommand::OpenWindow).expect("bind IPC") {
        LaunchOutcome::Primary(handle) => handle,
        LaunchOutcome::SecondaryForwarded => panic!("expected primary IPC"),
    };

    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    state.attach_ipc(ipc_handle);

    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));
    assert_eq!(state.windows().len(), 1);
    assert_eq!(state.window_workspace(w1).unwrap().tab_count(), 1);
    let s1 = state
        .window_workspace(w1)
        .unwrap()
        .active_tab()
        .unwrap()
        .focused_session_id()
        .unwrap();

    // IPC OpenWindow creates a second coordinated window with its own independent workspace.
    let _ = state.update(Message::IpcCommand(IpcCommand::OpenWindow));
    assert_eq!(state.windows().len(), 2);
    let w2 = *state
        .windows()
        .iter()
        .find(|&&id| id != w1)
        .expect("second window opened");
    assert_eq!(state.window_workspace(w1).unwrap().tab_count(), 1);
    assert_eq!(state.window_workspace(w2).unwrap().tab_count(), 1);
    let s2 = state
        .window_workspace(w2)
        .unwrap()
        .active_tab()
        .unwrap()
        .focused_session_id()
        .unwrap();
    assert_ne!(s1, s2);

    // Focus w1, then dispatch IPC OpenTab -> must open tab in w1, not w2.
    let _ = state.update(Message::WindowFocused(w1));
    let _ = state.update(Message::IpcCommand(IpcCommand::OpenTab {
        profile_id: None,
        cwd: None,
    }));
    assert_eq!(state.window_workspace(w1).unwrap().tab_count(), 2);
    assert_eq!(state.window_workspace(w2).unwrap().tab_count(), 1);
    let s3 = state
        .window_workspace(w1)
        .unwrap()
        .active_tab()
        .unwrap()
        .focused_session_id()
        .unwrap();

    // View-scoped InWindow message targeting w2 must mutate w2 without affecting w1.
    let _ = state.update(Message::InWindow {
        window_id: w2,
        inner: Box::new(Message::SplitFocusedPane(SplitAxis::Vertical)),
    });
    assert_eq!(
        state
            .window_workspace(w2)
            .unwrap()
            .active_tab()
            .unwrap()
            .root
            .pane_count(),
        2
    );
    assert_eq!(
        state
            .window_workspace(w1)
            .unwrap()
            .active_tab()
            .unwrap()
            .root
            .pane_count(),
        1
    );

    // Closing w1 cleans up w1's sessions while keeping w2's sessions intact.
    let _ = state.update(Message::WindowClosed(w1));
    assert!(state.session(s1).is_none());
    assert!(state.session(s3).is_none());
    assert!(state.session(s2).is_some());
    assert_eq!(state.windows().len(), 1);
    assert!(state.windows().contains(&w2));
}

#[test]
fn restart_restores_tabs_split_geometry_and_respawns_local_sessions_at_captured_cwd() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let dir_a = temp.path().join("work-a");
    let dir_b = temp.path().join("work-b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    let dir_a = dir_a.canonicalize().unwrap();
    let dir_b = dir_b.canonicalize().unwrap();

    let mut state = super::common::boot_test_app(paths.clone()).expect("boot initial AppState");
    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));

    let tab1 = state.window_workspace(w1).unwrap().active_tab_id().unwrap();
    let s1 = state
        .window_workspace(w1)
        .unwrap()
        .active_tab()
        .unwrap()
        .focused_session_id()
        .unwrap();

    let _ = state.update(Message::SplitFocusedPane(SplitAxis::Vertical));
    let s2 = state
        .window_workspace(w1)
        .unwrap()
        .active_tab()
        .unwrap()
        .focused_session_id()
        .unwrap();

    let split_id = match &state.window_workspace(w1).unwrap().tab(tab1).unwrap().root {
        SplitNode::Split { split_id, .. } => *split_id,
        SplitNode::Leaf { .. } => panic!("expected split node"),
    };
    let _ = state.update(Message::ResizeSplit {
        split_id,
        ratio: 0.37,
    });
    let _ = state.update(Message::TogglePaneZoom);
    assert!(
        state
            .window_workspace(w1)
            .unwrap()
            .tab(tab1)
            .unwrap()
            .zoomed_pane
            .is_some()
    );

    // Move each split pane to a distinct working directory via real shells on Linux (`/proc/<pid>/cwd`).
    #[cfg(not(target_os = "linux"))]
    let _ = &dir_a;
    #[cfg(target_os = "linux")]
    {
        let pty1 = state.session(s1).unwrap().pty().clone();
        pty1.write_bytes(
            format!("cd '{}' && printf \"CD_A_%s\\n\" OK\r", dir_a.display()).as_bytes(),
        )
        .unwrap();
        assert!(pty1.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("CD_A_OK")
        }));

        let pty2 = state.session(s2).unwrap().pty().clone();
        pty2.write_bytes(
            format!("cd '{}' && printf \"CD_B_%s\\n\" OK\r", dir_b.display()).as_bytes(),
        )
        .unwrap();
        assert!(pty2.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("CD_B_OK")
        }));
    }

    // Add a second tab in w1 and a second window w2, then keep tab1 active on w1.
    let _ = state.update(Message::NewTab);
    let _ = state.update(Message::SelectTab(tab1));
    let _ = state.update(Message::NewWindow);
    assert_eq!(state.windows().len(), 2);

    // Save/quit persists multi-window layout atomically before shutting down sessions.
    let _ = state.update(Message::CloseGuardStopAndQuit);
    assert!(paths.layout_file().is_file());
    drop(state);

    // Simulate process restart from saved layout.toml.
    let mut restored = super::common::boot_test_app(paths.clone()).expect("restart AppState");
    let rw1 = window::Id::unique();
    let rw2 = window::Id::unique();
    let _ = restored.update(Message::WindowOpened(rw1));
    let _ = restored.update(Message::WindowOpened(rw2));

    let ws1 = restored.window_workspace(rw1).expect("restored window 1");
    assert_eq!(ws1.tab_count(), 2);
    let rtab1 = ws1.active_tab().expect("tab1 active");
    assert_eq!(rtab1.root.pane_count(), 2);
    assert!(rtab1.zoomed_pane.is_some());
    match &rtab1.root {
        SplitNode::Split { axis, ratio, .. } => {
            assert_eq!(*axis, SplitAxis::Vertical);
            assert!((ratio - 0.37).abs() < 1e-4);
        }
        SplitNode::Leaf { .. } => panic!("expected restored split"),
    }

    let restored_sessions = rtab1.root.session_ids();
    assert_eq!(restored_sessions.len(), 2);

    #[cfg(target_os = "linux")]
    {
        let rpty1 = restored.session(restored_sessions[0]).unwrap().pty().clone();
        rpty1
            .write_bytes(b"pwd && printf \"RESTORE_A_%s\\n\" OK\r")
            .unwrap();
        assert!(
            rpty1.wait_for_output(Duration::from_secs(5), |s| {
                s.visible_text().contains("RESTORE_A_OK")
            })
        );
        assert!(
            rpty1
                .snapshot()
                .visible_text()
                .contains(&dir_a.to_string_lossy().to_string()),
            "restored pane 1 pwd mismatch:\n{}",
            rpty1.snapshot().visible_text()
        );

        let rpty2 = restored.session(restored_sessions[1]).unwrap().pty().clone();
        rpty2
            .write_bytes(b"pwd && printf \"RESTORE_B_%s\\n\" OK\r")
            .unwrap();
        assert!(
            rpty2.wait_for_output(Duration::from_secs(5), |s| {
                s.visible_text().contains("RESTORE_B_OK")
            })
        );
        assert!(
            rpty2
                .snapshot()
                .visible_text()
                .contains(&dir_b.to_string_lossy().to_string()),
            "restored pane 2 pwd mismatch:\n{}",
            rpty2.snapshot().visible_text()
        );
    }

    let ws2 = restored.window_workspace(rw2).expect("restored window 2");
    assert_eq!(ws2.tab_count(), 1);
    drop(restored);

    // Deleting a saved directory must fall back cleanly instead of crashing startup.
    std::fs::remove_dir_all(&dir_b).unwrap();
    let mut fallback = super::common::boot_test_app(paths).expect("restart with missing cwd");
    let fw1 = window::Id::unique();
    let _ = fallback.update(Message::WindowOpened(fw1));
    assert_eq!(fallback.window_workspace(fw1).unwrap().tab_count(), 2);
}

#[test]
fn restored_remote_sessions_start_disconnected_and_honor_profile_auto_connect_on_startup() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());

    let p_manual = Profile::ssh(
        "Manual SSH",
        "manual.example.com",
        22,
        "alice",
        SshAuthMethod::Agent,
    );
    let mut p_auto = Profile::ssh(
        "Auto SSH",
        "auto.example.com",
        2222,
        "deploy",
        SshAuthMethod::Agent,
    );
    p_auto.set_auto_connect_on_startup(true);

    let mut profiles = ProfilesConfig::default();
    profiles.profiles.push(p_manual.clone());
    profiles.profiles.push(p_auto.clone());
    save_profiles(&paths, &profiles).expect("save profiles");

    let layout = WorkspaceLayout {
        schema_version: CURRENT_SCHEMA_VERSION,
        windows: vec![WindowLayout {
            width: 1200,
            height: 800,
            active_tab: 0,
            tabs: vec![
                TabLayout {
                    title: Some("SSH Pair".to_string()),
                    color: None,
                    pinned: false,
                    focused_pane: 0,
                    zoomed_pane: None,
                    root: LayoutSplitNode::Split {
                        axis: LayoutSplitAxis::Vertical,
                        ratio: 0.5,
                        first: Box::new(LayoutSplitNode::Leaf {
                            session: SessionDescriptor::Remote {
                                profile_id: Some(p_manual.id.clone()),
                                host: "manual.example.com".to_string(),
                                port: 22,
                                username: Some("alice".to_string()),
                                title: Some("Manual".to_string()),
                                auto_connect_on_startup: false,
                            },
                        }),
                        second: Box::new(LayoutSplitNode::Leaf {
                            session: SessionDescriptor::Remote {
                                profile_id: Some(p_auto.id.clone()),
                                host: "auto.example.com".to_string(),
                                port: 2222,
                                username: Some("deploy".to_string()),
                                title: Some("Auto".to_string()),
                                // Profile has auto_connect_on_startup = true; restore should honor it.
                                auto_connect_on_startup: false,
                            },
                        }),
                    },
                },
                TabLayout {
                    title: Some("Ad-hoc SSH".to_string()),
                    color: None,
                    pinned: false,
                    focused_pane: 0,
                    zoomed_pane: None,
                    root: LayoutSplitNode::Leaf {
                        session: SessionDescriptor::Remote {
                            profile_id: None,
                            host: "adhoc.example.com".to_string(),
                            port: 22,
                            username: None,
                            title: None,
                            auto_connect_on_startup: false,
                        },
                    },
                },
            ],
        }],
    };
    save_layout(&paths, &layout).expect("save remote layout");

    let mut state = super::common::boot_test_app(paths).expect("boot with remote layout");
    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));

    let ws = state.window_workspace(w1).expect("window workspace");
    assert_eq!(ws.tab_count(), 2);

    let pair_sessions = ws.tabs()[0].root.session_ids();
    assert_eq!(pair_sessions.len(), 2);
    let manual_sid = pair_sessions[0];
    let auto_sid = pair_sessions[1];
    let adhoc_sid = ws.tabs()[1].root.session_ids()[0];

    // Remote sessions must NOT spawn local PTY shells, and MUST create real SessionEntry::Ssh slots.
    assert!(state.session(manual_sid).is_none());
    assert!(state.session(auto_sid).is_none());
    assert!(state.session(adhoc_sid).is_none());
    assert!(state.ssh_session(manual_sid).is_some());
    assert!(state.ssh_session(auto_sid).is_some());
    assert!(state.ssh_session(adhoc_sid).is_some());

    // All restored remote sessions start in Disconnected state.
    let manual_remote = state.remote_session(manual_sid).expect("manual remote");
    assert_eq!(manual_remote.transport_status, TransportStatus::Disconnected);
    assert!(!manual_remote.connect_requested);

    let auto_remote = state.remote_session(auto_sid).expect("auto remote");
    assert_eq!(auto_remote.transport_status, TransportStatus::Disconnected);
    assert!(auto_remote.auto_connect_on_startup);
    assert!(auto_remote.connect_requested);

    let adhoc_remote = state.remote_session(adhoc_sid).expect("adhoc remote");
    assert_eq!(adhoc_remote.transport_status, TransportStatus::Disconnected);
    assert!(!adhoc_remote.connect_requested);

    // Only the auto-connect session emitted a connect intent on startup.
    assert_eq!(state.pending_remote_connects().len(), 1);
    assert_eq!(state.pending_remote_connects()[0].session_id, auto_sid);
    assert_eq!(state.pending_remote_connects()[0].host, "auto.example.com");

    // Manual trigger requests connection for manual_sid and fires a real connect_ssh_task.
    let connect_task = state.update(Message::ConnectRemoteSession(manual_sid));
    assert!(connect_task.units() >= 1);
    assert!(state.remote_session(manual_sid).unwrap().connect_requested);
    assert_eq!(
        state.ssh_session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );
    assert_eq!(state.pending_remote_connects().len(), 2);
    assert_eq!(state.pending_remote_connects()[1].session_id, manual_sid);

    // Restored ad-hoc SSH session connects on Enter key without dead stub placeholder.
    let enter_task = state.update(Message::TerminalInput {
        session_id: adhoc_sid,
        bytes: b"\r".to_vec(),
    });
    assert!(enter_task.units() >= 1);
    assert_eq!(
        state.ssh_session(adhoc_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );
}

#[test]
fn ipc_open_tab_with_ssh_profile_creates_real_ssh_session_and_honors_auto_connect() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());

    let mut p_manual = Profile::ssh(
        "IPC Manual SSH",
        "ipc-manual.example.com",
        2222,
        "alice",
        SshAuthMethod::Password {
            vault_secret_ref: Some("ssh/ipc-manual".to_string()),
        },
    );
    p_manual.id = "ipc-ssh-manual".to_string();

    let mut p_auto = Profile::ssh(
        "IPC Auto SSH",
        "ipc-auto.example.com",
        2223,
        "deploy",
        SshAuthMethod::Agent,
    );
    p_auto.id = "ipc-ssh-auto".to_string();
    p_auto.set_auto_connect_on_startup(true);

    let mut profiles = ProfilesConfig::default();
    profiles.profiles.push(p_manual);
    profiles.profiles.push(p_auto);
    save_profiles(&paths, &profiles).expect("save profiles");

    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));

    // IPC OpenTab with auto_connect_on_startup = false creates SessionEntry::Ssh in Disconnected state.
    let _ = state.update(Message::IpcCommand(IpcCommand::OpenTab {
        profile_id: Some("ipc-ssh-manual".to_string()),
        cwd: None,
    }));
    let manual_sid = state.active_session_id().expect("manual IPC tab session");
    assert!(state.session(manual_sid).is_none());
    let manual_slot = state.ssh_session(manual_sid).expect("real SshSessionSlot");
    assert_eq!(manual_slot.transport_status(), &TransportStatus::Disconnected);
    assert_eq!(manual_slot.ssh_generation(), 0);
    assert_eq!(manual_slot.target().host, "ipc-manual.example.com");
    assert_eq!(manual_slot.target().port, 2222);
    drop(state.view(w1));

    // Pressing Enter connects the IPC-opened SSH session.
    let enter_task = state.update(Message::TerminalInput {
        session_id: manual_sid,
        bytes: b"\r".to_vec(),
    });
    assert!(enter_task.units() >= 1);
    assert_eq!(
        state.ssh_session(manual_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );
    assert_eq!(state.ssh_session(manual_sid).unwrap().ssh_generation(), 1);

    // IPC OpenTab with auto_connect_on_startup = true starts Connecting immediately and returns task.
    let auto_task = state.update(Message::IpcCommand(IpcCommand::OpenTab {
        profile_id: Some("ipc-ssh-auto".to_string()),
        cwd: None,
    }));
    assert!(auto_task.units() >= 2);
    let auto_sid = state.active_session_id().expect("auto IPC tab session");
    let auto_slot = state.ssh_session(auto_sid).expect("real auto SshSessionSlot");
    assert_eq!(auto_slot.transport_status(), &TransportStatus::Connecting);
    assert_eq!(auto_slot.ssh_generation(), 1);
}
