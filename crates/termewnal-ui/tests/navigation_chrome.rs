use std::time::Duration;

use iced::keyboard::{Key, Modifiers, key::Named};
use tempfile::TempDir;
use termewnal_core::{
    ConfigPaths, Profile, ProfileTarget, ProfilesConfig, SshAuthMethod, TunnelDirection,
    save_profiles,
};
use termewnal_ssh::TransportStatus;
use termewnal_ui::command_palette::{CommandPaletteMode, PaletteAction, QuickConnectTarget};
use termewnal_ui::sidebar::{
    ActiveTunnelEntry, SidebarSection, SystemSshProfile, TunnelRuntimeState, UNGROUPED_LABEL,
};
use termewnal_ui::status_bar::{
    SessionKindLabel, StatusBarAction, TransferSummaryHook, TunnelSummaryHook,
};
use termewnal_ui::{
    AppState, LaunchDisposition, Message, SessionId, SplitAxis, TabId, shortcut_for_key_event,
    shortcut_for_key_event_with_palette,
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
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("b".into()), Modifiers::CTRL, tab),
        Some(Message::ToggleSidebar)
    ));
    assert!(matches!(
        shortcut_for_key_event(&Key::Character("B".into()), Modifiers::CTRL, tab),
        Some(Message::ToggleSidebar)
    ));

    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    seed_profiles(&paths);

    let mut state = AppState::boot_with_paths(paths).expect("boot AppState");
    assert!(!state.sidebar().is_open());

    let _ = state.update(Message::ToggleSidebar);
    assert!(state.sidebar().is_open());

    let _ = state.update(Message::ToggleSidebar);
    assert!(!state.sidebar().is_open());

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
    }]));
    assert_eq!(state.sidebar().active_tunnels().len(), 1);
    assert_eq!(state.sidebar().active_tunnels()[0].name, "Postgres Local");
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
        auto_connect_on_startup: true,
        tmux_reattach: Some("main".to_string()),
    };

    let mut cfg = ProfilesConfig::default();
    cfg.profiles = vec![local_profile, ssh_profile];
    save_profiles(&paths, &cfg).expect("save profiles");

    let mut state = AppState::boot_with_paths(paths).expect("boot AppState");
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

    let mut state = AppState::boot_with_paths(paths).expect("boot AppState");
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

    // Workspace actions (Toggle Broadcast Input, Toggle Sidebar, Split Vertically) execute via palette.
    let _ = state.update(Message::ToggleCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("broadcast".to_string()));
    assert!(!state.is_broadcast_banner_visible());
    let _ = state.update(Message::CommandPaletteConfirm);
    assert!(state.is_broadcast_banner_visible());

    let _ = state.update(Message::ToggleCommandPalette);
    let _ = state.update(Message::CommandPaletteQueryChanged("sidebar".to_string()));
    assert!(!state.sidebar().is_open());
    let _ = state.update(Message::CommandPaletteConfirm);
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
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");
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
        AppState::boot_with_paths(ConfigPaths::from_root(temp.path())).expect("boot AppState");
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

    assert!(!state.sidebar().is_open());
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
