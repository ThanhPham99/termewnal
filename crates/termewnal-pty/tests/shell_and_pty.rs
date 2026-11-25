use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use termewnal_pty::{
    PtyEvent, PtySession, PtySpawnConfig, ShellKind, detect_default_shell, resolve_unix_shell,
    resolve_windows_shells,
};

#[test]
fn unix_shell_detection_uses_env_shell_when_present_and_falls_back_to_bin_sh() {
    let valid = resolve_unix_shell(Some("/bin/zsh"), |p| p == Path::new("/bin/zsh"));
    assert_eq!(valid.kind, ShellKind::UnixShell);
    assert_eq!(valid.program, "/bin/zsh");

    let missing = resolve_unix_shell(Some("/nonexistent/shell"), |_| false);
    assert_eq!(missing.program, "/bin/sh");

    let empty = resolve_unix_shell(Some("   "), |_| true);
    assert_eq!(empty.program, "/bin/sh");

    let unset = resolve_unix_shell(None, |_| true);
    assert_eq!(unset.program, "/bin/sh");

    // Real host auto-detection produces a non-empty program path.
    let host_shell = detect_default_shell();
    assert!(!host_shell.program.is_empty());
}

#[test]
fn windows_shell_detection_discovers_pwsh_powershell_cmd_git_bash_and_wsl_in_priority() {
    let available: HashSet<&str> = [
        r"C:\Program Files\PowerShell\7\pwsh.exe",
        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        r"C:\Windows\System32\cmd.exe",
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Windows\System32\wsl.exe",
    ]
    .into_iter()
    .collect();

    let shells = resolve_windows_shells(
        Some(r"C:\Windows\System32\cmd.exe"),
        Some(r"C:\Program Files"),
        Some(r"C:\Windows"),
        None,
        |candidate| {
            let cand = candidate.to_string_lossy();
            available
                .iter()
                .find(|known| {
                    **known == cand.as_ref()
                        || known.rsplit(['\\', '/']).next() == Some(cand.as_ref())
                })
                .map(|s| s.into())
        },
    );

    let kinds: Vec<ShellKind> = shells.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ShellKind::Pwsh,
            ShellKind::WindowsPowerShell,
            ShellKind::Cmd,
            ShellKind::GitBash,
            ShellKind::Wsl,
        ]
    );
    assert_eq!(
        shells[0].program,
        r"C:\Program Files\PowerShell\7\pwsh.exe"
    );
}

#[test]
fn real_pty_spawns_shell_and_captures_echoed_output_in_terminal_grid() {
    let session = PtySession::spawn(PtySpawnConfig::default()).expect("spawn local shell PTY");
    session
        .write_bytes(b"echo TERMEWNAL_SLICE5_OK\r")
        .expect("write command to PTY");

    let found = session.wait_for_output(Duration::from_secs(5), |snap| {
        snap.visible_text().contains("TERMEWNAL_SLICE5_OK")
    });
    assert!(
        found,
        "expected PTY grid to contain TERMEWNAL_SLICE5_OK, got:\n{}",
        session.snapshot().visible_text()
    );
}

#[test]
fn resizing_pty_updates_terminal_grid_and_child_tty_dimensions() {
    let session = PtySession::spawn(PtySpawnConfig::default()).expect("spawn local shell PTY");
    assert_eq!(session.dimensions(), (80, 24));

    session.resize(105, 37).expect("resize PTY and Term grid");
    assert_eq!(session.dimensions(), (105, 37));

    let snap = session.snapshot();
    assert_eq!(snap.cols, 105);
    assert_eq!(snap.rows, 37);
    assert_eq!(snap.cells.len(), 105 * 37);

    #[cfg(unix)]
    {
        session
            .write_bytes(b"stty size\r")
            .expect("query tty size inside shell");
        let saw_size = session.wait_for_output(Duration::from_secs(5), |s| {
            s.visible_text().contains("37 105")
        });
        assert!(
            saw_size,
            "expected kernel PTY stty size '37 105', got:\n{}",
            session.snapshot().visible_text()
        );
    }
}

#[test]
fn exiting_shell_delivers_eof_and_exited_event_without_hanging() {
    let session = PtySession::spawn(PtySpawnConfig::default()).expect("spawn local shell PTY");
    let mut rx = session
        .take_event_receiver()
        .expect("event receiver should be available");

    session.write_bytes(b"exit 0\r").expect("send exit command");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build tokio runtime");

    let exited = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(ev) = rx.recv().await {
                if let PtyEvent::Exited(code) = ev {
                    return Some(code);
                }
            }
            None
        })
        .await
    });

    assert_eq!(exited.expect("timed out waiting for PtyEvent::Exited"), Some(Some(0)));
}
