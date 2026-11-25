use std::sync::Mutex;

use termewnal_core::ConfigPaths;
use termewnal_ui::AppState;
use termewnal_ui::ipc::{IpcCommand, LaunchOutcome, acquire_or_forward};
use tracing::warn;

fn parse_launch_command<I, S>(args: I) -> IpcCommand
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut open_tab = false;
    let mut profile_id = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_ref() {
            "--new-tab" => open_tab = true,
            "--profile" => {
                if let Some(id) = iter.next() {
                    profile_id = Some(id.as_ref().to_string());
                    open_tab = true;
                }
            }
            _ => {}
        }
    }

    if open_tab {
        IpcCommand::OpenTab { profile_id }
    } else {
        IpcCommand::OpenWindow
    }
}

fn main() -> iced::Result {
    let paths = ConfigPaths::detect_xdg()
        .unwrap_or_else(|_| ConfigPaths::from_root(std::env::temp_dir().join("termewnal")));
    let launch_cmd = parse_launch_command(std::env::args().skip(1));

    let ipc_handle = match acquire_or_forward(&paths, launch_cmd) {
        Ok(LaunchOutcome::SecondaryForwarded) => {
            return Ok(());
        }
        Ok(LaunchOutcome::Primary(handle)) => Some(handle),
        Err(err) => {
            warn!(error = %err, "single-instance IPC unavailable; continuing standalone");
            None
        }
    };

    let boot_slot = Mutex::new(Some((paths, ipc_handle)));
    iced::daemon(
        move || {
            let (paths, ipc_handle) = boot_slot
                .lock()
                .expect("boot slot poisoned")
                .take()
                .expect("iced daemon boot invoked more than once");
            AppState::boot_daemon_with_ipc(paths, ipc_handle)
        },
        AppState::update,
        AppState::view,
    )
    .title(AppState::title)
    .theme(AppState::theme)
    .subscription(AppState::subscription)
    .run()
}
