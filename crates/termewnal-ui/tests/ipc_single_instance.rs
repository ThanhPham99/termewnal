use std::time::Duration;

use tempfile::TempDir;
use termewnal_core::ConfigPaths;
use termewnal_ui::ipc::{IpcCommand, LaunchOutcome, acquire_or_forward};

#[test]
fn secondary_launch_forwards_open_window_and_open_tab_to_primary_over_local_ipc() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());

    let primary = match acquire_or_forward(&paths, IpcCommand::OpenWindow)
        .expect("first launch becomes primary")
    {
        LaunchOutcome::Primary(handle) => handle,
        LaunchOutcome::SecondaryForwarded => panic!("expected first launch to be primary"),
    };

    let mut rx = primary
        .take_receiver()
        .expect("primary IPC handle provides command receiver");

    match acquire_or_forward(&paths, IpcCommand::OpenWindow)
        .expect("second launch forwards OpenWindow")
    {
        LaunchOutcome::SecondaryForwarded => {}
        LaunchOutcome::Primary(_) => panic!("second launch must NOT start a competing primary"),
    }

    match acquire_or_forward(
        &paths,
        IpcCommand::OpenTab {
            profile_id: Some("prod-bastion".to_string()),
        },
    )
    .expect("third launch forwards OpenTab")
    {
        LaunchOutcome::SecondaryForwarded => {}
        LaunchOutcome::Primary(_) => panic!("third launch must NOT start a competing primary"),
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build tokio runtime");

    let received = rt.block_on(async {
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("receive first IPC command")
            .expect("IPC channel open");
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("receive second IPC command")
            .expect("IPC channel open");
        (first, second)
    });

    assert_eq!(received.0, IpcCommand::OpenWindow);
    assert_eq!(
        received.1,
        IpcCommand::OpenTab {
            profile_id: Some("prod-bastion".to_string()),
        }
    );

    primary.shutdown();
}

#[cfg(unix)]
#[test]
fn primary_probes_and_reclaims_stale_socket_without_clobbering_live_primary() {
    use std::os::unix::net::UnixListener;

    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());
    let sock_path = paths.ipc_socket_file();
    std::fs::create_dir_all(sock_path.parent().unwrap()).expect("create runtime dir");

    // Simulate a crashed process leaving a dead Unix domain socket file behind.
    {
        let _dead_listener = UnixListener::bind(&sock_path).expect("bind dead socket");
    }
    assert!(sock_path.exists(), "stale socket file should remain on disk");

    let primary = match acquire_or_forward(&paths, IpcCommand::OpenWindow)
        .expect("stale socket must be reclaimed by new primary")
    {
        LaunchOutcome::Primary(handle) => handle,
        LaunchOutcome::SecondaryForwarded => {
            panic!("dead socket must not falsely trigger secondary forwarding")
        }
    };

    let mut rx = primary.take_receiver().expect("receiver available");

    // While primary is live, a new launch must forward instead of reclaiming the live socket.
    match acquire_or_forward(&paths, IpcCommand::OpenTab { profile_id: None })
        .expect("live primary receives forwarded OpenTab")
    {
        LaunchOutcome::SecondaryForwarded => {}
        LaunchOutcome::Primary(_) => panic!("must not clobber live primary socket"),
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build tokio runtime");
    let cmd = rt
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        })
        .expect("receive OpenTab")
        .expect("channel open");
    assert_eq!(cmd, IpcCommand::OpenTab { profile_id: None });

    drop(primary);

    // After clean primary drop, a new process can acquire primary cleanly again.
    let next_primary = match acquire_or_forward(&paths, IpcCommand::OpenWindow)
        .expect("re-acquire primary after clean shutdown")
    {
        LaunchOutcome::Primary(handle) => handle,
        LaunchOutcome::SecondaryForwarded => {
            panic!("expected new primary after previous primary shut down")
        }
    };
    next_primary.shutdown();
}
