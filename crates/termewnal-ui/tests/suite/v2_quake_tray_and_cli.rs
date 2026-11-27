use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use iced::Size;
use iced::window;
use tempfile::TempDir;
use termewnal_core::{
    AppConfig, ConfigPaths, Profile, ProfilesConfig, QuakeEdge, save_config, save_profiles,
};
#[cfg(target_os = "linux")]
use termewnal_ui::X11GlobalHotkeyRegistrar;
use termewnal_ui::{
    DaemonLifecycle, GlobalHotkeyRegistrar, HotkeyRegistrationGuard, IpcCommand, LaunchOutcome,
    Message, QuakeBounds, RecordingSystemTray, TrayAction, acquire_or_forward,
    compute_quake_bounds, parse_cli_launch_command, parse_global_hotkey, quake_window_settings,
};

use crate::common::boot_test_app;

fn fast_test_shell() -> Option<String> {
    #[cfg(unix)]
    {
        std::path::Path::new("/bin/sh")
            .exists()
            .then(|| "/bin/sh".to_string())
    }
    #[cfg(windows)]
    {
        Some(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

struct MockHotkeyGuard {
    released: Arc<AtomicBool>,
}

impl HotkeyRegistrationGuard for MockHotkeyGuard {}

impl Drop for MockHotkeyGuard {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

struct MockHotkeyRegistrar {
    registrations: AtomicUsize,
    released: Arc<AtomicBool>,
}

impl GlobalHotkeyRegistrar for MockHotkeyRegistrar {
    fn register(
        &self,
        spec: &str,
        _on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Option<Box<dyn HotkeyRegistrationGuard>>, String> {
        let _ = parse_global_hotkey(spec)?;
        self.registrations.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Box::new(MockHotkeyGuard {
            released: Arc::clone(&self.released),
        })))
    }

    fn backend_name(&self) -> &'static str {
        "mock"
    }
}

#[test]
fn quake_edge_geometry_bounds_and_toggle_lifecycle() {
    let monitor = Size::new(1920.0, 1080.0);
    assert_eq!(
        compute_quake_bounds(QuakeEdge::Top, 50, monitor),
        QuakeBounds {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 540.0,
        }
    );
    assert_eq!(
        compute_quake_bounds(QuakeEdge::Bottom, 50, monitor),
        QuakeBounds {
            x: 0.0,
            y: 540.0,
            width: 1920.0,
            height: 540.0,
        }
    );
    assert_eq!(
        compute_quake_bounds(QuakeEdge::Left, 25, monitor),
        QuakeBounds {
            x: 0.0,
            y: 0.0,
            width: 480.0,
            height: 1080.0,
        }
    );
    assert_eq!(
        compute_quake_bounds(QuakeEdge::Right, 100, monitor),
        QuakeBounds {
            x: 192.0,
            y: 0.0,
            width: 1728.0,
            height: 1080.0,
        }
    );

    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());
    let mut cfg = AppConfig::default();
    cfg.terminal.shell = fast_test_shell();
    cfg.quake.enabled = true;
    cfg.quake.edge = QuakeEdge::Bottom;
    cfg.quake.size_percent = 40;
    cfg.quake.hide_on_blur = true;
    save_config(&paths, &cfg).unwrap();

    let win_settings = quake_window_settings(&cfg.quake, monitor);
    assert!(!win_settings.decorations);
    assert_eq!(win_settings.level, window::Level::AlwaysOnTop);
    assert!(!win_settings.exit_on_close_request);

    let tray = RecordingSystemTray::new();
    let mut state = boot_test_app(paths)
        .unwrap()
        .with_tray_adapter(Arc::clone(&tray) as Arc<dyn termewnal_ui::SystemTrayAdapter>);

    // 1st ToggleQuakeWindow opens Quake window & shell session.
    let _ = state.update(Message::ToggleQuakeWindow);
    let qwin = state.quake().window_id().expect("quake window allocated");
    assert!(state.quake().is_visible());
    assert!(tray.latest().unwrap().quake_visible);
    let quake_sessions = state.window_workspace(qwin).unwrap().session_ids();
    assert_eq!(quake_sessions.len(), 1);
    let q_sid = quake_sessions[0];

    // Monitor size callback updates Quake bounds.
    let _ = state.update(Message::QuakeMonitorSize(
        qwin,
        Some(Size::new(2560.0, 1440.0)),
    ));
    assert_eq!(
        state.quake().current_bounds(&cfg.quake),
        QuakeBounds {
            x: 0.0,
            y: 864.0,
            width: 2560.0,
            height: 576.0,
        }
    );

    // 2nd ToggleQuakeWindow hides without closing live session.
    let _ = state.update(Message::ToggleQuakeWindow);
    assert!(!state.quake().is_visible());
    assert!(!tray.latest().unwrap().quake_visible);
    assert!(state.session(q_sid).is_some());

    // 3rd ToggleQuakeWindow re-shows the SAME Quake window & session.
    let _ = state.update(Message::ToggleQuakeWindow);
    assert_eq!(state.quake().window_id(), Some(qwin));
    assert!(state.quake().is_visible());
    assert!(state.session(q_sid).is_some());

    // Blur auto-hides Quake when hide_on_blur=true, and CloseRequested hides rather than killing.
    let _ = state.update(Message::WindowUnfocused(qwin));
    assert!(!state.quake().is_visible());
    let _ = state.update(Message::ToggleQuakeWindow);
    assert!(state.quake().is_visible());
    let _ = state.update(Message::CloseRequested(qwin));
    assert!(!state.quake().is_visible());
    assert!(state.session(q_sid).is_some());
}

#[test]
fn global_hotkey_parser_mock_guard_and_x11_fallback() {
    let hk1 = parse_global_hotkey("Ctrl+Alt+Space").unwrap();
    assert!(hk1.ctrl && hk1.alt && !hk1.shift && !hk1.super_key);
    assert_eq!(hk1.x11_keysym, 0x0020);

    let hk2 = parse_global_hotkey("Ctrl+`").unwrap();
    assert!(hk2.ctrl && hk2.x11_keysym == 0x0060);

    let hk3 = parse_global_hotkey("Super+Shift+F12").unwrap();
    assert!(hk3.super_key && hk3.shift && hk3.x11_keysym == 0xFFC9);

    assert!(parse_global_hotkey("").is_err());
    assert!(parse_global_hotkey("Ctrl+Alt").is_err());
    assert!(parse_global_hotkey("Ctrl+A+B").is_err());

    // X11GlobalHotkeyRegistrar degrades safely without panicking even if DISPLAY is absent/unreachable.
    #[cfg(target_os = "linux")]
    {
        let x11 = X11GlobalHotkeyRegistrar;
        let res = x11.register("Ctrl+Alt+Space", Arc::new(|| {}));
        assert!(res.is_ok());
    }

    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());
    let mut cfg = AppConfig::default();
    cfg.terminal.shell = fast_test_shell();
    cfg.quake.enabled = true;
    cfg.quake.hotkey = "Ctrl+Alt+Space".to_string();
    save_config(&paths, &cfg).unwrap();

    let released = Arc::new(AtomicBool::new(false));
    let registrar = Arc::new(MockHotkeyRegistrar {
        registrations: AtomicUsize::new(0),
        released: Arc::clone(&released),
    });

    let mut state = boot_test_app(paths)
        .unwrap()
        .with_hotkey_registrar(Arc::clone(&registrar) as Arc<dyn GlobalHotkeyRegistrar>);
    state.sync_global_hotkey().unwrap();
    assert_eq!(registrar.registrations.load(Ordering::SeqCst), 1);
    assert!(state.has_hotkey_guard());

    let _ = state.update(Message::GlobalHotkeyTriggered);
    assert!(state.quake().is_visible());
    let _ = state.update(Message::GlobalHotkeyTriggered);
    assert!(!state.quake().is_visible());

    let _ = state.update(Message::TrayAction(TrayAction::Quit));
    assert!(released.load(Ordering::SeqCst));
}

#[test]
fn system_tray_hide_show_tunnel_telemetry_profile_launch_and_quit() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());
    let mut profiles = ProfilesConfig::default();
    let p1 = Profile::local_shell("Ops Shell", fast_test_shell());
    let p1_id = p1.id.clone();
    profiles.profiles.push(p1);
    save_profiles(&paths, &profiles).unwrap();

    let tray = RecordingSystemTray::new();
    let mut state = boot_test_app(paths)
        .unwrap()
        .with_tray_adapter(Arc::clone(&tray) as Arc<dyn termewnal_ui::SystemTrayAdapter>);
    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));

    let snap = tray.latest().expect("tray state recorded");
    assert!(snap.window_visible);
    assert!(!snap.quake_visible);
    assert_eq!(snap.quick_profiles.len(), 1);
    assert_eq!(snap.quick_profiles[0].1, "Ops Shell");
    let labels: Vec<String> = snap.menu_entries().into_iter().map(|i| i.label).collect();
    assert!(labels.contains(&"Hide Window".to_string()));
    assert!(labels.contains(&"Toggle Quake Console".to_string()));
    assert!(labels.contains(&"Active Tunnels: 0".to_string()));
    assert!(labels.contains(&"Launch: Ops Shell".to_string()));
    assert!(labels.contains(&"Quit Termewnal".to_string()));

    // Clicking Show/Hide hides window into BackgroundRunning without dropping sessions.
    let tabs_before = state.window_workspace(w1).unwrap().tab_count();
    let sids_before = state.window_workspace(w1).unwrap().session_ids();
    let _ = state.update(Message::TrayAction(TrayAction::ShowOrHideWindow));
    assert!(state.is_minimized_to_tray());
    assert_eq!(state.lifecycle(), DaemonLifecycle::BackgroundRunning);
    assert!(!tray.latest().unwrap().window_visible);
    assert_eq!(state.window_workspace(w1).unwrap().tab_count(), tabs_before);
    assert!(state.session(sids_before[0]).is_some());

    // Clicking Show/Hide again restores window visibility.
    let _ = state.update(Message::TrayAction(TrayAction::ShowOrHideWindow));
    assert!(!state.is_minimized_to_tray());
    assert_eq!(state.lifecycle(), DaemonLifecycle::Active);
    assert!(tray.latest().unwrap().window_visible);

    // Clicking quick profile launches new tab for that profile.
    let _ = state.update(Message::TrayAction(TrayAction::LaunchProfile(p1_id)));
    assert_eq!(
        state.window_workspace(w1).unwrap().tab_count(),
        tabs_before + 1
    );

    // Tray Quit tears down cleanly.
    let _ = state.update(Message::TrayAction(TrayAction::Quit));
    assert_eq!(state.lifecycle(), DaemonLifecycle::Exiting);
}

#[test]
fn system_bar_close_minimizes_last_window_to_tray_and_ipc_restores_without_duplicating() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());

    let tray = RecordingSystemTray::new();
    let mut state = boot_test_app(paths.clone())
        .unwrap()
        .with_tray_adapter(Arc::clone(&tray) as Arc<dyn termewnal_ui::SystemTrayAdapter>);
    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));
    let _ = state.update(Message::NewWindow);
    assert_eq!(state.windows().len(), 2);
    let w2 = *state.windows().iter().find(|&&id| id != w1).unwrap();

    // Closing a non-last window closes only that window rather than hiding remaining windows.
    let _ = state.update(Message::CloseRequested(w1));
    assert!(!state.is_minimized_to_tray());
    let _ = state.update(Message::WindowClosed(w1));
    assert_eq!(state.windows().len(), 1);
    assert!(state.windows().contains(&w2));

    // Even with active background work, closing the last window minimizes to tray by default.
    let tracker = state.background_work_tracker().clone();
    tracker.set_active_standalone_tunnels(1);
    tracker.set_active_transfers(1);
    let sid = state.window_workspace(w2).unwrap().session_ids()[0];

    let _ = state.update(Message::CloseRequested(w2));
    assert!(state.is_minimized_to_tray());
    assert_eq!(state.lifecycle(), DaemonLifecycle::BackgroundRunning);
    assert_eq!(state.close_guard(), termewnal_ui::CloseGuardState::Idle);
    assert!(state.windows().contains(&w2));
    assert!(state.session(sid).is_some());
    assert!(!tracker.stop_requested());
    assert!(!tray.latest().unwrap().window_visible);
    assert!(
        tray.latest()
            .unwrap()
            .menu_entries()
            .iter()
            .any(|item| item.label == "Show Window")
    );

    // Relaunching via IPC OpenWindow restores the hidden window instead of spawning a second one.
    let _ = state.update(Message::IpcCommand(IpcCommand::OpenWindow));
    assert!(!state.is_minimized_to_tray());
    assert_eq!(state.lifecycle(), DaemonLifecycle::Active);
    assert_eq!(state.windows().len(), 1);
    assert!(state.windows().contains(&w2));
    assert!(tray.latest().unwrap().window_visible);

    // Minimize again via CloseRequested, then open a new tab via IPC -> unhides and adds tab to w2.
    let _ = state.update(Message::CloseRequested(w2));
    assert!(state.is_minimized_to_tray());
    let tabs_before = state.window_workspace(w2).unwrap().tab_count();
    let _ = state.update(Message::IpcCommand(IpcCommand::OpenTab {
        profile_id: None,
        cwd: None,
    }));
    assert!(!state.is_minimized_to_tray());
    assert_eq!(state.windows().len(), 1);
    assert_eq!(state.window_workspace(w2).unwrap().tab_count(), tabs_before + 1);

    // Explicit Quit from Command Palette triggers Close Guard when background work is active.
    let _ = state.update(Message::CommandPaletteExecuteItem(
        termewnal_ui::command_palette::PaletteAction::QuitApp,
    ));
    assert!(matches!(
        state.close_guard(),
        termewnal_ui::CloseGuardState::Confirming { window_id, .. } if window_id == w2
    ));
    let _ = state.update(Message::CloseGuardStopAndQuit);
    assert_eq!(state.lifecycle(), DaemonLifecycle::Exiting);
    assert!(tracker.stop_requested());
}

#[test]
fn cli_flags_parse_and_ipc_cwd_profile_and_toggle_quake_dispatch() {
    assert_eq!(
        parse_cli_launch_command(Vec::<&str>::new()),
        IpcCommand::OpenWindow
    );
    assert_eq!(
        parse_cli_launch_command(["--toggle-quake"]),
        IpcCommand::ToggleQuake
    );
    assert_eq!(
        parse_cli_launch_command(["--new-tab"]),
        IpcCommand::OpenTab {
            profile_id: None,
            cwd: None,
        }
    );
    assert_eq!(
        parse_cli_launch_command(["--profile", "Prod SSH"]),
        IpcCommand::OpenTab {
            profile_id: Some("Prod SSH".to_string()),
            cwd: None,
        }
    );
    assert_eq!(
        parse_cli_launch_command(["--new-tab", "--profile=web", "--cwd=/srv/app"]),
        IpcCommand::OpenTab {
            profile_id: Some("web".to_string()),
            cwd: Some(PathBuf::from("/srv/app")),
        }
    );

    let dir = TempDir::new().unwrap();
    let custom_cwd = dir.path().join("workspace_cwd");
    std::fs::create_dir_all(&custom_cwd).unwrap();
    let paths = ConfigPaths::from_root(dir.path().to_path_buf());

    let mut profiles = ProfilesConfig::default();
    let mut prof = Profile::local_shell("Custom Dev", fast_test_shell());
    prof.id = "custom-dev-id".to_string();
    profiles.profiles.push(prof);
    save_profiles(&paths, &profiles).unwrap();

    // Verify IPC roundtrip for ToggleQuake and OpenTab with profile name + cwd.
    let primary = match acquire_or_forward(&paths, IpcCommand::OpenWindow).unwrap() {
        LaunchOutcome::Primary(h) => h,
        LaunchOutcome::SecondaryForwarded => panic!("expected primary lock"),
    };
    let mut rx = primary.take_receiver().unwrap();
    assert!(matches!(
        acquire_or_forward(&paths, IpcCommand::ToggleQuake).unwrap(),
        LaunchOutcome::SecondaryForwarded
    ));
    let cmd_open = parse_cli_launch_command([
        "--new-tab",
        "--profile",
        "custom dev",
        "--cwd",
        custom_cwd.to_str().unwrap(),
    ]);
    assert!(matches!(
        acquire_or_forward(&paths, cmd_open.clone()).unwrap(),
        LaunchOutcome::SecondaryForwarded
    ));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let (recv1, recv2) = rt.block_on(async { (rx.recv().await.unwrap(), rx.recv().await.unwrap()) });
    assert_eq!(recv1, IpcCommand::ToggleQuake);
    assert_eq!(recv2, cmd_open);
    drop(primary);

    // Apply IPC commands into AppState & verify real PTY spawns inside custom_cwd.
    let mut state = boot_test_app(paths).unwrap();
    let w1 = window::Id::unique();
    let _ = state.update(Message::WindowOpened(w1));

    let _ = state.update(Message::IpcCommand(recv2));
    assert_eq!(state.window_workspace(w1).unwrap().tab_count(), 2);
    let sid = state.active_session_id().unwrap();
    assert_eq!(state.session(sid).unwrap().profile_id(), Some("custom-dev-id"));
    let pty = Arc::clone(state.session(sid).unwrap().pty());
    #[cfg(unix)]
    pty.write_bytes(b"pwd\r").unwrap();
    #[cfg(windows)]
    pty.write_bytes(b"cd\r").unwrap();
    assert!(pty.wait_for_output(Duration::from_secs(3), |s| s
        .visible_text()
        .contains("workspace_cwd")));

    // IPC ToggleQuake opens & toggles Quake window cleanly.
    let _ = state.update(Message::IpcCommand(IpcCommand::ToggleQuake));
    assert!(state.quake().is_visible());
    let _ = state.update(Message::IpcCommand(IpcCommand::ToggleQuake));
    assert!(!state.quake().is_visible());
}

#[test]
fn quake_window_ignores_hide_on_blur_while_os_dialog_in_flight() {
    let temp = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(temp.path());
    let mut cfg = AppConfig::default();
    cfg.quake.hide_on_blur = true;
    save_config(&paths, &cfg).unwrap();

    let mut state = boot_test_app(paths).unwrap();
    let _ = state.update(Message::IpcCommand(IpcCommand::ToggleQuake));
    let quake_win = state.quake().window_id().expect("quake win opened");
    let _ = state.update(Message::WindowOpened(quake_win));
    assert!(state.quake().is_visible());
    assert_eq!(state.in_flight_os_dialogs(), 0);

    // Launch an OS file picker dialog from Quake: in-flight guard blocks blur auto-hide.
    let _ = state.update(Message::SettingsBrowseSftpDownloadDir);
    assert_eq!(state.in_flight_os_dialogs(), 1);
    let _ = state.update(Message::WindowUnfocused(quake_win));
    assert!(state.quake().is_visible());

    // Canceling or picking decrements in_flight_os_dialogs back to 0, re-enabling hide_on_blur.
    let _ = state.update(Message::SettingsSftpDownloadDirPicked(None));
    assert_eq!(state.in_flight_os_dialogs(), 0);
    let _ = state.update(Message::WindowUnfocused(quake_win));
    assert!(!state.quake().is_visible());
}
