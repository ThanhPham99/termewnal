use std::sync::Mutex;

use termewnal_core::ConfigPaths;
use termewnal_ui::AppState;
use termewnal_ui::ipc::{LaunchOutcome, acquire_or_forward, parse_cli_launch_command};
use termewnal_ui::ui_theme::{FONT_BODY, ui_font};
use tracing::warn;

fn main() -> iced::Result {
    let paths = ConfigPaths::detect_xdg()
        .unwrap_or_else(|_| ConfigPaths::from_root(std::env::temp_dir().join("termewnal")));
    let launch_cmd = parse_cli_launch_command(std::env::args().skip(1));

    let ipc_handle = match acquire_or_forward(&paths, launch_cmd.clone()) {
        Ok(LaunchOutcome::SecondaryForwarded) => {
            return Ok(());
        }
        Ok(LaunchOutcome::Primary(handle)) => Some(handle),
        Err(err) => {
            warn!(error = %err, "single-instance IPC unavailable; continuing standalone");
            None
        }
    };

    let boot_slot = Mutex::new(Some((paths, ipc_handle, launch_cmd)));
    iced::daemon(
        move || {
            let (paths, ipc_handle, initial_cmd) = boot_slot
                .lock()
                .expect("boot slot poisoned")
                .take()
                .expect("iced daemon boot invoked more than once");
            AppState::boot_daemon_with_ipc_and_initial_command(paths, ipc_handle, initial_cmd)
        },
        AppState::update,
        AppState::view,
    )
    .settings(iced::Settings {
        id: Some("termewnal".to_string()),
        default_font: ui_font(),
        default_text_size: iced::Pixels(FONT_BODY),
        antialiasing: true,
        ..Default::default()
    })
    .title(AppState::title)
    .theme(AppState::theme)
    .subscription(AppState::subscription)
    .run()
}
