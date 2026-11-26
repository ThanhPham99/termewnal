use std::time::Duration;

use iced::keyboard::{Key, Modifiers, key::Named};
use tempfile::TempDir;
use termewnal_core::{
    ConfigPaths, Profile, ProfileTarget, ProfilesConfig, SshAuthMethod, TunnelDirection,
    load_profiles, save_profiles,
};
use termewnal_ssh::TransportStatus;
use termewnal_ui::command_palette::{CommandPaletteMode, PaletteAction, QuickConnectTarget};
use termewnal_ui::sidebar::{
    ActiveTunnelEntry, SidebarFilterScope, SidebarSection, SystemSshProfile, TunnelRuntimeState,
    UNGROUPED_LABEL,
};
use termewnal_ui::status_bar::{
    SessionKindLabel, StatusBarAction, TransferSummaryHook, TunnelSummaryHook,
};
use termewnal_ui::{
    ContextMenuKind, DragPayload, DropTarget, LaunchDisposition, Message, SessionId, SplitAxis,
    TabId, shortcut_for_key_event, shortcut_for_key_event_with_palette,
};

fn seed_profiles(paths: &ConfigPaths) -> Vec<Profile> {
    let mut prod_ssh = Profile::ssh(
        "Prod Bastion",
        "bastion.prod.internal",
        2222,
        "ops",
        SshAuthMethod::Agent,
    );
    prod_ssh.group = Some("Production".to_string());
    prod_ssh.tags = vec!["prod".to_string(), "bastion".to_string()];

    let mut staging_ssh = Profile::ssh(
        "Staging Web",
        "web.staging.internal",
        22,
        "deploy",
        SshAuthMethod::Agent,
    );
    staging_ssh.group = Some("Staging".to_string());
    staging_ssh.tags = vec!["staging".to_string(), "web".to_string()];

    let mut local_dev = Profile::local_shell("Dev Shell", None);
    local_dev.tags = vec!["local".to_string(), "prod".to_string()];

    let profiles = vec![staging_ssh, local_dev, prod_ssh];
    let mut cfg = ProfilesConfig::default();
    cfg.profiles = profiles.clone();
    save_profiles(paths, &cfg).expect("save seed profiles");
    profiles
}

#[test]
fn ctrl_b_toggles_left_sidebar_and_groups_saved_profiles_by_group_and_tag() {
    let tab = Some(TabId(1));
    assert!(shortcut_for_key_event(&Key::Character("b".into()), Modifiers::CTRL, tab).is_none());
    assert!(shortcut_for_key_event(&Key::Character("B".into()), Modifiers::CTRL, tab).is_none());

    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    seed_profiles(&paths);

    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    assert!(state.sidebar().is_open());

    let _ = state.update(Message::ToggleSidebar);
    assert!(state.sidebar().is_open());

    let _ = state.update(Message::SetSidebarOpen(false));
    assert!(state.sidebar().is_open());

    let _ = state.update(Message::SetSidebarOpen(true));
    assert!(state.sidebar().is_open());

    // Named groups sort alphabetically; ungrouped profiles appear last under UNGROUPED_LABEL.
    let groups = state
        .sidebar()
        .grouped_saved_profiles(&state.profiles().profiles);
    let group_names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(group_names, vec!["Production", "Staging", UNGROUPED_LABEL]);
    assert_eq!(groups[0].profiles[0].name, "Prod Bastion");
    assert_eq!(groups[1].profiles[0].name, "Staging Web");
    assert_eq!(groups[2].profiles[0].name, "Dev Shell");

    assert_eq!(
        state.sidebar().available_tags(&state.profiles().profiles),
        vec!["bastion", "local", "prod", "staging", "web"]
    );

    // Filtering by tag narrows across groups and clears cleanly when toggled again.
    let _ = state.update(Message::SidebarToggleTagFilter("prod".to_string()));
    assert_eq!(state.sidebar().selected_tag(), Some("prod"));
    let tagged_groups = state
        .sidebar()
        .grouped_saved_profiles(&state.profiles().profiles);
    let tagged_names: Vec<&str> = tagged_groups
        .iter()
        .flat_map(|g| g.profiles.iter().map(|p| p.name.as_str()))
        .collect();
    assert_eq!(tagged_names, vec!["Prod Bastion", "Dev Shell"]);

    let _ = state.update(Message::SidebarToggleTagFilter("prod".to_string()));
    assert_eq!(state.sidebar().selected_tag(), None);

    // Text filtering matches profile name, host, group, and tags.
    let _ = state.update(Message::SidebarFilterChanged("staging".to_string()));
    let filtered = state
        .sidebar()
        .grouped_saved_profiles(&state.profiles().profiles);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].profiles[0].name, "Staging Web");
    let _ = state.update(Message::SidebarFilterChanged(String::new()));

    // Group & section collapse state toggles cleanly.
    assert!(!state.sidebar().is_group_collapsed("Production"));
    let _ = state.update(Message::SidebarToggleGroup("Production".to_string()));
    assert!(state.sidebar().is_group_collapsed("Production"));

    assert!(!state.sidebar().is_section_collapsed(SidebarSection::ActiveTunnels));
    let _ = state.update(Message::SidebarToggleSection(SidebarSection::ActiveTunnels));
    assert!(state.sidebar().is_section_collapsed(SidebarSection::ActiveTunnels));

    // Extensible hooks for System SSH Profiles (Ticket 08) and Active Tunnels (Tickets 11-12).
    let _ = state.update(Message::SetSystemSshProfiles(vec![SystemSshProfile {
        alias: "db-prod".to_string(),
        host_name: Some("10.10.0.8".to_string()),
        port: 22,
        user: Some("postgres".to_string()),
        identity_file: None,
        proxy_jump: vec!["bastion".to_string()],
        forward_agent: None,
        forward_x11: None,
        proxy_command: None,
    }]));
    assert_eq!(state.sidebar().system_ssh_profiles().len(), 1);
    assert_eq!(state.sidebar().system_ssh_profiles()[0].alias, "db-prod");

    let _ = state.update(Message::SetActiveTunnels(vec![ActiveTunnelEntry {
        id: "tun-1".to_string(),
        name: "Postgres Local".to_string(),
        direction: TunnelDirection::Local,
        bind_addr: "127.0.0.1:5432".to_string(),
        target_addr: "127.0.0.1:5432".to_string(),
        state: TunnelRuntimeState::Active,
        standalone: true,
        session_id: Some(SessionId(1)),
        reconnect_attempt: None,
        next_retry_secs: None,
        last_error: None,
        reconnected_notice: None,
    }]));
    assert_eq!(state.sidebar().active_tunnels().len(), 1);
    assert_eq!(state.sidebar().active_tunnels()[0].name, "Postgres Local");

    // Sidebar `[Active]`/`[All]` filter scope and Active Sessions collapse.
    let win = iced::window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));
    assert_eq!(state.sidebar().filter_scope(), SidebarFilterScope::All);
    drop(state.view(win));

    let _ = state.update(Message::SetSidebarFilterScope(SidebarFilterScope::Active));
    assert_eq!(state.sidebar().filter_scope(), SidebarFilterScope::Active);
    drop(state.view(win));

    let _ = state.update(Message::SetSidebarFilterScope(SidebarFilterScope::All));
    let _ = state.update(Message::SidebarToggleSection(SidebarSection::ActiveSessions));
    assert!(state.sidebar().is_section_collapsed(SidebarSection::ActiveSessions));
    drop(state.view(win));
}

#[test]
fn selecting_saved_profile_opens_local_pty_or_ssh_session_slot_in_tab_or_split_pane() {
    let temp = TempDir::new().expect("create temp XDG root");
    let work_dir = temp.path().join("repo_alpha");
    std::fs::create_dir_all(&work_dir).expect("create target working dir");
    let canonical_work_dir = std::fs::canonicalize(&work_dir).expect("canonicalize working dir");
    let paths = ConfigPaths::from_root(temp.path());

    let mut local_profile = Profile::local_shell("Repo Alpha", None);
    local_profile.id = "prof-local-alpha".to_string();
    local_profile.group = Some("Work".to_string());
    local_profile.target = ProfileTarget::LocalShell {
        shell: None,
        working_dir: Some(work_dir),
        args: Vec::new(),
    };

    let mut ssh_profile = Profile::ssh(
        "Prod App",
        "10.20.30.40",
        2222,
        "deploy",
        SshAuthMethod::Agent,
    );
    ssh_profile.id = "prof-ssh-prod".to_string();
    ssh_profile.group = Some("Production".to_string());
    ssh_profile.target = ProfileTarget::Ssh {
        host: "10.20.30.40".to_string(),
        port: 2222,
        username: "deploy".to_string(),
        auth: SshAuthMethod::Agent,
        proxy_jump: vec!["bastion.internal".to_string()],
        proxy_command: None,
        proxy: None,
        forward_agent: false,
        forward_x11: false,
        login_scripts: Vec::new(),
        auto_connect_on_startup: true,
        tmux_reattach: Some("main".to_string()),
        keepalive_interval_secs: None,
        keepalive_max_missed: None,
        auto_attach_mux: false,
        mux_attach_command: None,
    };

    let mut cfg = ProfilesConfig::default();
    cfg.profiles = vec![local_profile, ssh_profile];
    save_profiles(&paths, &cfg).expect("save profiles");

    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    let initial_tab = state.active_tab_id().expect("initial tab");

    // Launching a saved local Profile opens a new tab with a live PTY in its configured working_dir.
    let _ = state.update(Message::LaunchProfile {
        profile_id: "prof-local-alpha".to_string(),
        disposition: LaunchDisposition::NewTab,
    });
    assert_eq!(state.workspace().tab_count(), 2);
    let local_tab = state.active_tab_id().expect("new tab active");
    assert_ne!(local_tab, initial_tab);
    let local_sid = state.active_session_id().expect("local profile session");
    let local_session = state
        .session(local_sid)
        .expect("local profile spawns live LocalSession");
    assert_eq!(local_session.title(), "Repo Alpha");
    assert_eq!(local_session.profile_id(), Some("prof-local-alpha"));
    let pty = local_session.pty().clone();

    #[cfg(unix)]
    {
        let _ = state.update(Message::TerminalInput {
            session_id: local_sid,
            bytes: b"pwd\r".to_vec(),
        });
        let expected_dir = canonical_work_dir.to_string_lossy().into_owned();
        assert!(
            pty.wait_for_output(Duration::from_secs(5), |s| {
                s.visible_text().contains(&expected_dir)
            }),
            "expected PTY cwd {expected_dir}, got:\n{}",
            pty.snapshot().visible_text()
        );
    }
    #[cfg(windows)]
    {
        let _ = canonical_work_dir;
        let _ = state.update(Message::TerminalInput {
            session_id: local_sid,
            bytes: b"cd\r".to_vec(),
        });
        assert!(
            pty.wait_for_output(Duration::from_secs(5), |s| {
                s.visible_text().contains("repo_alpha")
            }),
            "expected PTY cwd to include repo_alpha, got:\n{}",
            pty.snapshot().visible_text()
        );
    }

    // Launching a saved SSH Profile with Split(Vertical) splits the active tab into an SSH Session slot.
    let _ = state.update(Message::LaunchProfile {
        profile_id: "prof-ssh-prod".to_string(),
        disposition: LaunchDisposition::Split(SplitAxis::Vertical),
    });
    assert_eq!(state.workspace().tab_count(), 2);
    assert_eq!(state.workspace().tab(local_tab).unwrap().root.pane_count(), 2);

    let ssh_sid = state.active_session_id().expect("ssh slot session active");
    assert_ne!(ssh_sid, local_sid);
    assert!(state.session(ssh_sid).is_none());
    let ssh_slot = state
        .ssh_session(ssh_sid)
        .expect("ssh profile opens SshSessionSlot ready for Ticket 07");
    assert_eq!(ssh_slot.title(), "Prod App");
    assert_eq!(ssh_slot.target().profile_id.as_deref(), Some("prof-ssh-prod"));
    assert_eq!(ssh_slot.target().host, "10.20.30.40");
    assert_eq!(ssh_slot.target().port, 2222);
    assert_eq!(ssh_slot.target().username, "deploy");
    assert_eq!(
        ssh_slot.target().proxy_jump,
        vec!["bastion.internal".to_string()]
    );
    assert_eq!(ssh_slot.target().tmux_reattach.as_deref(), Some("main"));
    assert_eq!(ssh_slot.transport_status(), &TransportStatus::Connecting);
}

#[test]
fn ctrl_k_and_ctrl_shift_p_open_command_palette_with_fuzzy_search_and_execution() {
    let tab = Some(TabId(3));
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;

    assert!(matches!(
        shortcut_for_key_event(&Key::Character("k".into()), Modifiers::CTRL, tab),
        Some(Message::ToggleCommandPalette)
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("P".into()), ctrl_shift, tab),
        Some(Message::ToggleCommandPalette)
    ));

    // Escape / Arrow navigation / Enter are only intercepted when Command Palette is open.
    assert!(shortcut_for_key_event_with_palette(
        &Key::Named(Named::Escape),
        Modifiers::empty(),
        tab,
        false
    )
    .is_none());
    assert!(matches!(
        shortcut_for_key_event_with_palette(
            &Key::Named(Named::Escape),
            Modifiers::empty(),
            tab,
            true
        ),
        Some(Message::CloseCommandPalette)
    ));
    assert!(matches!(
        shortcut_for_key_event_with_palette(
            &Key::Named(Named::ArrowDown),
            Modifiers::empty(),
            tab,
            true
        ),
        Some(Message::CommandPaletteSelectNext)
    ));
    assert!(matches!(
        shortcut_for_key_event_with_palette(
            &Key::Named(Named::ArrowUp),
            Modifiers::empty(),
            tab,
            true
        ),
        Some(Message::CommandPaletteSelectPrevious)
    ));
    assert!(matches!(
        shortcut_for_key_event_with_palette(
            &Key::Named(Named::Enter),
            Modifiers::empty(),
            tab,
            true
        ),
        Some(Message::CommandPaletteConfirm)
    ));

    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    seed_profiles(&paths);

    let mut state = super::common::boot_test_app(paths).expect("boot AppState");
    let initial_session = state.active_session_id().expect("initial session");
    let initial_tab = state.active_tab_id().expect("initial tab");

    assert!(!state.command_palette().is_open());
    let _ = state.update(Message::ToggleCommandPalette);
    assert!(state.command_palette().is_open());

    // Fuzzy query matches saved Profile "Prod Bastion" via subsequence "pbas" and executes it.
    let _ = state.update(Message::CommandPaletteQueryChanged("pbas".to_string()));
    let items = state.command_palette_items();
    assert!(!items.is_empty());
    assert!(items[0].title.contains("Prod Bastion"));

    let _ = state.update(Message::CommandPaletteConfirm);
    assert!(!state.command_palette().is_open());
    let ssh_sid = state.active_session_id().expect("ssh slot active");
    assert_ne!(ssh_sid, initial_session);
    assert_eq!(state.ssh_session(ssh_sid).unwrap().target().host, "bastion.prod.internal");

    // Re-open palette and switch back to the initial local Session.
    let _ = state.update(Message::ToggleCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged(
        format!("session #{}", initial_session.0),
    ));
    let session_items = state.command_palette_items();
    assert!(matches!(
        session_items.first().map(|i| &i.action),
        Some(PaletteAction::SwitchSession(sid)) if *sid == initial_session
    ));
    let _ = state.update(Message::CommandPaletteConfirm);
    assert!(!state.command_palette().is_open());
    assert_eq!(state.active_session_id(), Some(initial_session));
    assert_eq!(state.active_tab_id(), Some(initial_tab));

    // Workspace actions (Toggle Broadcast Input, Split Vertically) execute via palette.
    let _ = state.update(Message::ToggleCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("broadcast".to_string()));
    assert!(!state.is_broadcast_banner_visible());
    let _ = state.update(Message::CommandPaletteConfirm);
    assert!(state.is_broadcast_banner_visible());

    let _ = state.update(Message::ToggleCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("sidebar".to_string()));
    assert!(state.command_palette_items().iter().all(|i| i.action != PaletteAction::ToggleSidebar));
    let _ = state.update(Message::CloseCommandPalette);
    assert!(state.sidebar().is_open());

    // Arrow navigation moves selection index across filtered workspace commands.
    let _ = state.update(Message::ToggleCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("split".to_string()));
    let split_items = state.command_palette_items();
    assert!(split_items.len() >= 2);
    assert_eq!(state.command_palette().selected_index(), 0);
    let _ = state.update(Message::CommandPaletteSelectNext);
    assert_eq!(state.command_palette().selected_index(), 1);
    let _ = state.update(Message::CommandPaletteSelectPrevious);
    assert_eq!(state.command_palette().selected_index(), 0);
    let _ = state.update(Message::CloseCommandPalette);
    assert!(!state.command_palette().is_open());
}

#[test]
fn command_palette_quick_connect_parses_targets_and_opens_ssh_session_slot() {
    assert_eq!(
        QuickConnectTarget::parse("alice@db.internal:2222"),
        Some(QuickConnectTarget {
            host: "db.internal".to_string(),
            port: 2222,
            username: "alice".to_string(),
        })
    );
    assert_eq!(
        QuickConnectTarget::parse("ssh ops@web.internal"),
        Some(QuickConnectTarget {
            host: "web.internal".to_string(),
            port: 22,
            username: "ops".to_string(),
        })
    );
    assert_eq!(
        QuickConnectTarget::parse("ops@[2001:db8::1]:2200"),
        Some(QuickConnectTarget {
            host: "2001:db8::1".to_string(),
            port: 2200,
            username: "ops".to_string(),
        })
    );
    let default_target = QuickConnectTarget::parse("bastion.local").expect("host-only target");
    assert_eq!(default_target.host, "bastion.local");
    assert_eq!(default_target.port, 22);
    assert!(!default_target.username.is_empty());

    assert_eq!(QuickConnectTarget::parse(""), None);
    assert_eq!(QuickConnectTarget::parse("@host"), None);
    assert_eq!(QuickConnectTarget::parse("user@"), None);
    assert_eq!(QuickConnectTarget::parse("host:0"), None);
    assert_eq!(QuickConnectTarget::parse("host:abc"), None);
    assert_eq!(QuickConnectTarget::parse("host:99999"), None);

    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        super::common::boot_test_app(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let initial_tabs = state.workspace().tab_count();

    // Entering Quick Connect mode from the palette and confirming opens an SSH Session slot.
    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteEnterQuickConnect);
    assert_eq!(state.command_palette().mode(), CommandPaletteMode::QuickConnect);
    let _ = state.update(Message::CommandPaletteQuickConnectInputChanged(
        "ops@192.168.50.10:2202".to_string(),
    ));
    let _ = state.update(Message::CommandPaletteConfirm);

    assert!(!state.command_palette().is_open());
    assert_eq!(state.workspace().tab_count(), initial_tabs + 1);
    let qc_sid = state.active_session_id().expect("quick connect session");
    let slot = state.ssh_session(qc_sid).expect("quick connect SSH slot");
    assert_eq!(slot.target().username, "ops");
    assert_eq!(slot.target().host, "192.168.50.10");
    assert_eq!(slot.target().port, 2202);
    assert_eq!(slot.transport_status(), &TransportStatus::Connecting);

    // Typing a target directly in Search mode also offers an immediately executable Quick Connect item.
    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged(
        "deploy@api.internal:2222".to_string(),
    ));
    let items = state.command_palette_items();
    assert!(matches!(
        items.first().map(|i| &i.action),
        Some(PaletteAction::QuickConnect(t)) if t.host == "api.internal" && t.port == 2222 && t.username == "deploy"
    ));
    let _ = state.update(Message::CommandPaletteConfirm);
    assert!(!state.command_palette().is_open());
    let qc_sid2 = state.active_session_id().expect("second quick connect session");
    let slot2 = state.ssh_session(qc_sid2).expect("second quick connect slot");
    assert_eq!(slot2.target().host, "api.internal");
}

#[test]
fn status_bar_reflects_active_session_and_extensible_transport_tunnel_transfer_hooks() {
    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        super::common::boot_test_app(ConfigPaths::from_root(temp.path())).expect("boot AppState");
    let local_sid = state.active_session_id().expect("initial local session");

    let initial_snap = state.status_bar_snapshot();
    let active = initial_snap
        .active_session
        .as_ref()
        .expect("active session in status bar");
    assert_eq!(active.session_id, local_sid);
    assert_eq!(active.kind, SessionKindLabel::Local);
    assert_eq!((active.cols, active.rows), (80, 24));
    assert!(!active.zoomed);
    assert!(!active.broadcast);
    assert_eq!(initial_snap.transport_text(), "Transport: Local PTY");
    assert_eq!(initial_snap.tunnels_text(), "Tunnels: 0");
    assert_eq!(initial_snap.transfers_text(), "Transfers: idle");
    assert!(!initial_snap.shows_reconnect_controls());

    // Resizing viewport, zooming, and enabling broadcast reflect immediately in Status Bar.
    let _ = state.update(Message::ViewportResized {
        session_id: local_sid,
        cols: 120,
        rows: 36,
    });
    let _ = state.update(Message::TogglePaneZoom);
    let _ = state.update(Message::ToggleBroadcastInput);
    let updated_local = state.status_bar_snapshot();
    let local_info = updated_local.active_session.as_ref().unwrap();
    assert_eq!((local_info.cols, local_info.rows), (120, 36));
    assert!(local_info.zoomed);
    assert!(local_info.broadcast);
    assert!(updated_local.session_text().contains("120x36"));

    // Open an SSH Session via Quick Connect and drive TransportStatus lifecycle hooks (Ticket 09).
    let _ = state.update(Message::QuickConnect(QuickConnectTarget {
        host: "prod.example.com".to_string(),
        port: 2222,
        username: "ops".to_string(),
    }));
    let ssh_sid = state.active_session_id().expect("ssh session active");
    assert_eq!(
        state.status_bar_snapshot().transport_text(),
        "Transport: Connecting..."
    );

    let _ = state.update(Message::SetSessionTransportStatus {
        session_id: ssh_sid,
        status: TransportStatus::Connected {
            latency_ms: Some(18),
        },
    });
    let conn_snap = state.status_bar_snapshot();
    assert_eq!(conn_snap.active_session.as_ref().unwrap().kind, SessionKindLabel::Ssh);
    assert_eq!(conn_snap.transport_text(), "Transport: Connected (18 ms)");
    assert!(!conn_snap.shows_reconnect_controls());

    let _ = state.update(Message::SetSessionTransportStatus {
        session_id: ssh_sid,
        status: TransportStatus::Reconnecting {
            attempt: 3,
            next_retry_secs: 8,
        },
    });
    let reconn_snap = state.status_bar_snapshot();
    assert_eq!(
        reconn_snap.transport_text(),
        "Transport: Reconnecting #3 (8s)"
    );
    assert!(reconn_snap.shows_reconnect_controls());

    let _ = state.update(Message::StatusBarAction(StatusBarAction::ReconnectSession(
        ssh_sid,
    )));
    assert_eq!(
        state.ssh_session(ssh_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );

    let _ = state.update(Message::StatusBarAction(StatusBarAction::CancelReconnect(
        ssh_sid,
    )));
    assert_eq!(
        state.ssh_session(ssh_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );

    // Tunnel & Transfer hooks update Status Bar summaries and drawer/sidebar toggles.
    let _ = state.update(Message::SetTunnelSummary(TunnelSummaryHook {
        total: 3,
        active: 2,
        holding: 1,
        reconnecting: 0,
        retrying: 0,
        error: 0,
    }));
    let _ = state.update(Message::SetTransferSummary(TransferSummaryHook {
        active: 1,
        queued: 2,
        failed: 0,
        bytes_done: 512,
        bytes_total: 1024,
        bytes_per_sec: Some(256_000),
        drawer_open: false,
    }));
    let hooks_snap = state.status_bar_snapshot();
    assert_eq!(hooks_snap.tunnels_text(), "Tunnels: 2 active, 1 holding");
    assert_eq!(hooks_snap.transfers_text(), "Transfers: 1 active (50%)");

    assert!(!hooks_snap.transfer_drawer_open);
    let _ = state.update(Message::StatusBarAction(
        StatusBarAction::ToggleTransferDrawer,
    ));
    assert!(state.status_bar_snapshot().transfer_drawer_open);

    let _ = state.update(Message::SetSidebarOpen(false));
    assert!(state.sidebar().is_open());
    let _ = state.update(Message::StatusBarAction(StatusBarAction::ToggleTunnelsView));
    assert!(state.sidebar().is_open());
    assert!(!state.sidebar().is_section_collapsed(SidebarSection::ActiveTunnels));

    // Verify Iced Element view construction across Sidebar, Status Bar, and Command Palette modes.
    let win = iced::window::Id::unique();
    let _ = state.view(win);
    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.view(win);
    let _ = state.update(Message::CommandPaletteEnterQuickConnect);
    let _ = state.view(win);
}

#[test]
fn probe_reconnect_all_palette_and_status_bar_handle_degraded_reconnecting_and_mitm_safely() {
    use termewnal_ssh::HostKeyPrompt;
    use termewnal_ui::{MitmAlert, SshUiEvent};

    let temp = TempDir::new().expect("create temp XDG root");
    let mut state =
        super::common::boot_test_app(ConfigPaths::from_root(temp.path())).expect("boot AppState");

    // 1. Command Palette offers "Probe / Reconnect All SSH Sessions".
    let _ = state.update(Message::OpenCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("probe".to_string()));
    let items = state.command_palette_items();
    assert!(matches!(
        items.first().map(|i| &i.action),
        Some(PaletteAction::ProbeReconnectAll)
    ));
    let _ = state.update(Message::CloseCommandPalette);

    // 2. Create 4 SSH sessions in distinct states:
    //    a) Connected (should probe without dropping)
    let _ = state.update(Message::QuickConnect(QuickConnectTarget {
        host: "connected.internal".to_string(),
        port: 22,
        username: "ops".to_string(),
    }));
    let connected_sid = state.active_session_id().unwrap();
    let _ = state.update(Message::SetSessionTransportStatus {
        session_id: connected_sid,
        status: TransportStatus::Connected {
            latency_ms: Some(12),
        },
    });
    assert_eq!(state.ssh_session(connected_sid).unwrap().probe_count(), 0);

    //    b) Degraded (shows reconnect controls + probes)
    let _ = state.update(Message::QuickConnect(QuickConnectTarget {
        host: "degraded.internal".to_string(),
        port: 22,
        username: "ops".to_string(),
    }));
    let degraded_sid = state.active_session_id().unwrap();
    let _ = state.update(Message::SetSessionTransportStatus {
        session_id: degraded_sid,
        status: TransportStatus::Degraded { missed_probes: 2 },
    });
    let degraded_snap = state.status_bar_snapshot();
    assert_eq!(
        degraded_snap.transport_text(),
        "Transport: Degraded (2 missed)"
    );
    assert!(degraded_snap.shows_reconnect_controls());

    //    c) Reconnecting (should immediately transition to Connecting on ProbeReconnectAll)
    let _ = state.update(Message::QuickConnect(QuickConnectTarget {
        host: "reconnecting.internal".to_string(),
        port: 22,
        username: "ops".to_string(),
    }));
    let reconnecting_sid = state.active_session_id().unwrap();
    let _ = state.update(Message::SetSessionTransportStatus {
        session_id: reconnecting_sid,
        status: TransportStatus::Reconnecting {
            attempt: 2,
            next_retry_secs: 4,
        },
    });

    //    d) MITM-blocked (must NEVER auto-reconnect)
    let _ = state.update(Message::QuickConnect(QuickConnectTarget {
        host: "mitm.internal".to_string(),
        port: 22,
        username: "ops".to_string(),
    }));
    let mitm_sid = state.active_session_id().unwrap();
    let mitm_gen = state.ssh_session(mitm_sid).unwrap().ssh_generation();
    let _ = state.update(Message::SshEvent {
        session_id: mitm_sid,
        event: SshUiEvent::MitmBlocked {
            generation: mitm_gen,
            alert: MitmAlert {
                session_id: mitm_sid,
                host: "mitm.internal".to_string(),
                port: 22,
                line: 1,
                key_type: "ssh-ed25519".to_string(),
                expected_fingerprint: "SHA256:good".to_string(),
                received_fingerprint: "SHA256:bad".to_string(),
            },
        },
    });

    //    e) Untouched restored SSH session (generation == 0, Disconnected) must remain Disconnected.
    let restored_prof = Profile::ssh("Restored", "restored.internal", 22, "ops", SshAuthMethod::Agent);
    let _ = state.update(Message::RestoreSshProfile(restored_prof));
    let untouched_sid = state.active_session_id().unwrap();
    assert_eq!(state.ssh_session(untouched_sid).unwrap().ssh_generation(), 0);
    assert_eq!(
        state.ssh_session(untouched_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    let _ = HostKeyPrompt {
        host: String::new(),
        port: 22,
        key_type: String::new(),
        fingerprint_sha256: String::new(),
    };

    // Trigger Probe / Reconnect All via StatusBarAction::ProbeAll.
    let _ = state.update(Message::StatusBarAction(StatusBarAction::ProbeAll));

    assert_eq!(state.ssh_session(connected_sid).unwrap().probe_count(), 1);
    assert_eq!(state.ssh_session(degraded_sid).unwrap().probe_count(), 1);
    assert_eq!(
        state.ssh_session(reconnecting_sid).unwrap().transport_status(),
        &TransportStatus::Connecting
    );
    assert_eq!(
        state.ssh_session(mitm_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
    assert_eq!(
        state.ssh_session(untouched_sid).unwrap().transport_status(),
        &TransportStatus::Disconnected
    );
}

#[test]
fn sidebar_right_click_group_management_and_drag_drop_workflows() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let seeded = seed_profiles(&paths);
    let dev_id = seeded[2].id.clone();

    let mut state = super::common::boot_test_app(paths.clone()).expect("boot AppState");
    let win = iced::window::Id::unique();
    let _ = state.update(Message::WindowOpened(win));

    // Profile sidebar is always open and cannot be collapsed.
    assert!(state.sidebar().is_open());
    let _ = state.update(Message::SetSidebarOpen(false));
    assert!(state.sidebar().is_open());

    // 1. Right-clicking sidebar empty area opens Sidebar context menu and creates a persistent empty group.
    let _ = state.update(Message::OpenSidebarContextMenu {
        group_name: None,
        profile_id: None,
        position: iced::Point::new(80.0, 220.0),
    });
    assert!(matches!(
        state.context_menu().active,
        Some((ContextMenuKind::Sidebar { group_name: None, profile_id: None, .. }, _))
    ));
    drop(state.view(win));

    let _ = state.update(Message::BeginCreateGroup {
        assign_profile_id: None,
    });
    assert!(state.context_menu().is_open());
    let _ = state.update(Message::CreateGroupDraftChanged("Cloud Ops".to_string()));
    drop(state.view(win));
    let _ = state.update(Message::SubmitCreateGroup);
    assert!(!state.context_menu().is_open());

    let persisted = load_profiles(&paths).expect("reload profiles after group create");
    assert_eq!(persisted.groups, vec!["Cloud Ops"]);
    let groups_view = state
        .sidebar()
        .grouped_saved_profiles_with_groups(&state.profiles().profiles, &state.profiles().groups);
    let group_names: Vec<&str> = groups_view.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(
        group_names,
        vec!["Cloud Ops", "Production", "Staging", UNGROUPED_LABEL]
    );
    drop(state.view(win));

    // 2. Group right-click -> New Profile in Group prefills target group.
    let _ = state.update(Message::OpenNewProfileInGroup(Some("Cloud Ops".to_string())));
    assert!(state.profile_editor().is_open());
    assert_eq!(state.profile_editor().group, "Cloud Ops");
    let _ = state.update(Message::CloseProfileEditor);

    // 3. Drag saved profile "Dev Shell" into empty "Cloud Ops" group.
    let _ = state.update(Message::DragStart {
        payload: DragPayload::SavedProfile {
            profile_id: dev_id.clone(),
            label: "Dev Shell".to_string(),
        },
        position: iced::Point::new(60.0, 320.0),
    });
    assert!(state.drag_drop().is_dragging());
    let _ = state.update(Message::DragCursorMoved(iced::Point::new(60.0, 180.0)));
    let _ = state.update(Message::DragHoverChanged(DropTarget::ProfileGroup(Some(
        "Cloud Ops".to_string(),
    ))));
    drop(state.view(win));
    let _ = state.update(Message::DragFinish);
    assert!(!state.drag_drop().is_dragging());

    let after_drop = load_profiles(&paths).expect("reload profiles after drag move");
    let moved_dev = after_drop
        .profiles
        .iter()
        .find(|p| p.id == dev_id)
        .expect("dev profile present");
    assert_eq!(moved_dev.group.as_deref(), Some("Cloud Ops"));

    // 4. Rename and delete group cleanly without losing member profiles.
    let _ = state.update(Message::BeginRenameGroup("Cloud Ops".to_string()));
    let _ = state.update(Message::RenameGroupDraftChanged("Platform".to_string()));
    drop(state.view(win));
    let _ = state.update(Message::SubmitRenameGroup);
    assert_eq!(
        state
            .profiles()
            .profiles
            .iter()
            .find(|p| p.id == dev_id)
            .and_then(|p| p.group.as_deref()),
        Some("Platform")
    );

    let _ = state.update(Message::DeleteProfileGroup("Platform".to_string()));
    let after_delete = load_profiles(&paths).expect("reload profiles after delete group");
    assert!(after_delete.groups.is_empty());
    assert_eq!(
        after_delete
            .profiles
            .iter()
            .find(|p| p.id == dev_id)
            .and_then(|p| p.group.as_deref()),
        None
    );

    // 5. Drag System SSH host into a group clones & assigns it.
    let _ = state.update(Message::SetSystemSshProfiles(vec![SystemSshProfile {
        alias: "edge-ssh".to_string(),
        host_name: Some("10.20.0.5".to_string()),
        port: 22,
        user: Some("deploy".to_string()),
        identity_file: None,
        proxy_jump: Vec::new(),
        forward_agent: None,
        forward_x11: None,
        proxy_command: None,
    }]));
    let _ = state.update(Message::DragStart {
        payload: DragPayload::SystemSsh {
            alias: "edge-ssh".to_string(),
            label: "edge-ssh".to_string(),
        },
        position: iced::Point::new(60.0, 500.0),
    });
    let _ = state.update(Message::DragHoverChanged(DropTarget::ProfileGroup(Some(
        "Production".to_string(),
    ))));
    let _ = state.update(Message::DragFinish);
    assert!(state.profiles().profiles.iter().any(|p| {
        p.name == "edge-ssh" && p.group.as_deref() == Some("Production")
    }));

    // 6. Drag & drop workspace tabs reorders cleanly and supports cancel with Escape.
    let _ = state.update(Message::NewTab);
    let _ = state.update(Message::NewTab);
    let initial_ids: Vec<TabId> = state.workspace().tabs().iter().map(|t| t.id).collect();
    assert_eq!(initial_ids.len(), 3);
    let (t1, t2, t3) = (initial_ids[0], initial_ids[1], initial_ids[2]);

    // Drag T1 after T2 -> [T2, T1, T3]
    let _ = state.update(Message::DragStart {
        payload: DragPayload::Tab {
            tab_id: t1,
            label: "Tab 1".to_string(),
        },
        position: iced::Point::new(260.0, 20.0),
    });
    let _ = state.update(Message::DragHoverChanged(DropTarget::TabSlot {
        tab_id: t2,
        after: true,
    }));
    drop(state.view(win));
    let _ = state.update(Message::DragFinish);
    let order_1: Vec<TabId> = state.workspace().tabs().iter().map(|t| t.id).collect();
    assert_eq!(order_1, vec![t2, t1, t3]);

    // Drag T3 before T2 -> [T3, T2, T1]
    let _ = state.update(Message::DragStart {
        payload: DragPayload::Tab {
            tab_id: t3,
            label: "Tab 3".to_string(),
        },
        position: iced::Point::new(420.0, 20.0),
    });
    let _ = state.update(Message::DragHoverChanged(DropTarget::TabSlot {
        tab_id: t2,
        after: false,
    }));
    let _ = state.update(Message::DragFinish);
    let order_2: Vec<TabId> = state.workspace().tabs().iter().map(|t| t.id).collect();
    assert_eq!(order_2, vec![t3, t2, t1]);

    // Drag T3 to TabStripEnd -> [T2, T1, T3]
    let _ = state.update(Message::DragStart {
        payload: DragPayload::Tab {
            tab_id: t3,
            label: "Tab 3".to_string(),
        },
        position: iced::Point::new(260.0, 20.0),
    });
    let _ = state.update(Message::DragHoverChanged(DropTarget::TabStripEnd));
    let _ = state.update(Message::DragFinish);
    let order_3: Vec<TabId> = state.workspace().tabs().iter().map(|t| t.id).collect();
    assert_eq!(order_3, vec![t2, t1, t3]);

    // Dragging a saved profile onto TabStripEnd opens a new tab; onto WorkspacePane splits right.
    let tabs_before_drop = state.workspace().tab_count();
    let _ = state.update(Message::DragStart {
        payload: DragPayload::SavedProfile {
            profile_id: dev_id.clone(),
            label: "Dev Shell".to_string(),
        },
        position: iced::Point::new(80.0, 300.0),
    });
    let _ = state.update(Message::DragHoverChanged(DropTarget::TabStripEnd));
    let _ = state.update(Message::DragFinish);
    assert_eq!(state.workspace().tab_count(), tabs_before_drop + 1);

    let focused_pane = state.workspace().active_tab().unwrap().focused_pane;
    let panes_before = state.workspace().active_tab().unwrap().root.pane_count();
    let _ = state.update(Message::DragStart {
        payload: DragPayload::SavedProfile {
            profile_id: dev_id,
            label: "Dev Shell".to_string(),
        },
        position: iced::Point::new(80.0, 300.0),
    });
    let _ = state.update(Message::DragHoverChanged(DropTarget::WorkspacePane {
        pane_id: Some(focused_pane),
        split_axis: Some(SplitAxis::Vertical),
    }));
    drop(state.view(win));
    let _ = state.update(Message::DragFinish);
    assert_eq!(
        state.workspace().active_tab().unwrap().root.pane_count(),
        panes_before + 1
    );

    // Escape cancels active drag; external OS file hover renders and clears cleanly.
    let _ = state.update(Message::DragStart {
        payload: DragPayload::Tab {
            tab_id: t1,
            label: "Tab 1".to_string(),
        },
        position: iced::Point::new(260.0, 20.0),
    });
    assert!(state.drag_drop().is_dragging());
    let _ = state.update(Message::EscapePressed);
    assert!(!state.drag_drop().is_dragging());

    let _ = state.update(Message::ExternalFileHovered("/tmp/deploy.tar.gz".into()));
    assert!(state.drag_drop().external_file_hover.is_some());
    drop(state.view(win));
    let _ = state.update(Message::ExternalFilesLeft);
    assert!(state.drag_drop().external_file_hover.is_none());
}
