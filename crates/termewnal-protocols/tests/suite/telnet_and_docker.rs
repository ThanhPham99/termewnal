use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use termewnal_core::{DockerRuntime, LoginScriptStep, Profile, ProfileTarget};
use termewnal_protocols::docker::{
    CommandOutputRunner, ContainerExecConfig, DockerDiscovery, build_exec_shell_spec,
};
use termewnal_protocols::telnet::{
    DO, IAC, OPT_ECHO, OPT_NAWS, OPT_SGA, OPT_TTYPE, SB, SE, TelnetConnectConfig, TelnetEvent,
    TelnetParser, TelnetSession, WILL, build_naws_frame, build_ttype_is_frame,
    escape_telnet_outgoing,
};
use termewnal_protocols::{ProtocolError, ProtocolStatus};
use termewnal_pty::TerminalBuffer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// Avoid unused warning if unused re-exports aren't needed; verify constants explicitly.
#[test]
fn telnet_parser_handles_split_iac_subnegotiations_and_escaped_ff() {
    let mut parser = TelnetParser::new();

    let c1 = parser.feed(&[b'A', IAC]);
    assert_eq!(c1.payload, b"A");
    assert!(c1.events.is_empty());

    let c2 = parser.feed(&[DO, OPT_NAWS, IAC, IAC, b'B', IAC, SB, OPT_TTYPE]);
    assert_eq!(c2.payload, vec![IAC, b'B']);
    assert_eq!(
        c2.events,
        vec![TelnetEvent::Negotiation {
            command: DO,
            option: OPT_NAWS,
        }]
    );

    let c3 = parser.feed(&[1, IAC, SE, b'C']);
    assert_eq!(c3.payload, b"C");
    assert_eq!(
        c3.events,
        vec![TelnetEvent::Subnegotiation {
            option: OPT_TTYPE,
            payload: vec![1],
        }]
    );

    assert_eq!(
        escape_telnet_outgoing(&[b'x', IAC, b'y']),
        vec![b'x', IAC, IAC, b'y']
    );
    assert_eq!(OPT_ECHO, 1);
    assert_eq!(OPT_SGA, 3);
    assert_eq!(build_ttype_is_frame("xterm"), vec![
        IAC, SB, OPT_TTYPE, 0, b'x', b't', b'e', b'r', b'm', IAC, SE
    ]);
}

#[tokio::test]
async fn telnet_session_negotiates_options_handles_resize_naws_login_script_and_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_rx_wire = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let conn_count = Arc::new(AtomicUsize::new(0));
    let (drop_first_tx, drop_first_rx) = oneshot::channel::<()>();
    let drop_first_tx = Arc::new(Mutex::new(Some(drop_first_tx)));
    let drop_first_rx = Arc::new(Mutex::new(Some(drop_first_rx)));

    let wire_srv = Arc::clone(&server_rx_wire);
    let cnt_srv = Arc::clone(&conn_count);
    let drop_rx_srv = Arc::clone(&drop_first_rx);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let idx = cnt_srv.fetch_add(1, Ordering::SeqCst);
            let wire_log = Arc::clone(&wire_srv);
            let drop_rx = if idx == 0 {
                drop_rx_srv.lock().unwrap().take()
            } else {
                None
            };

            tokio::spawn(async move {
                // Server initiates Telnet option negotiation + TTYPE query + login prompt.
                let mut init = vec![
                    IAC, DO, OPT_NAWS, IAC, DO, OPT_TTYPE, IAC, WILL, OPT_ECHO, IAC, SB, OPT_TTYPE,
                    1, IAC, SE,
                ];
                if idx == 0 {
                    init.extend_from_slice(b"login: ");
                } else {
                    init.extend_from_slice(b"SESSION_2_READY\r\n");
                }
                let _ = sock.write_all(&init).await;

                let mut drop_rx_opt = drop_rx;
                let mut buf = [0_u8; 4096];
                let mut text_parser = TelnetParser::new();
                let mut line_acc = String::new();

                loop {
                    let read_fut = sock.read(&mut buf);
                    let res = if let Some(rx) = drop_rx_opt.as_mut() {
                        tokio::select! {
                            _ = rx => break,
                            r = read_fut => r,
                        }
                    } else {
                        read_fut.await
                    };

                    match res {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            wire_log.lock().unwrap().push(buf[..n].to_vec());
                            let parsed = text_parser.feed(&buf[..n]);
                            if !parsed.payload.is_empty() {
                                line_acc.push_str(&String::from_utf8_lossy(&parsed.payload));
                                if line_acc.contains("ops\r") || line_acc.contains("ops\n") {
                                    line_acc.clear();
                                    let _ = sock.write_all(b"TELNET_AUTH_OK\r\n").await;
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    let buffer = Arc::new(TerminalBuffer::new(80, 24, 2_000));
    let profile = Profile::telnet("Router", "127.0.0.1", addr.port());
    let mut cfg = TelnetConnectConfig::from_profile_target(&profile.target, 80, 24).unwrap();
    cfg.auto_reconnect = true;
    cfg.reconnect_delay = Duration::from_millis(25);
    cfg.login_scripts = vec![LoginScriptStep {
        expect: "login:".to_string(),
        send: "ops".to_string(),
        is_regex: false,
        secret_ref: None,
        delay_ms: 0,
        append_newline: true,
    }];

    let session = TelnetSession::connect_on_buffer(cfg, Arc::clone(&buffer));

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("TELNET_AUTH_OK")
            })
            .await,
        "Telnet LoginScript auth output missing: {}",
        buffer.export_scrollback_text()
    );
    assert_eq!(session.status(), ProtocolStatus::Connected);

    // Wait briefly so NAWS negotiation flag settles cleanly and test resize NAWS frame.
    let deadline_naws = Instant::now() + Duration::from_secs(2);
    while !session.is_naws_enabled() && Instant::now() < deadline_naws {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(session.is_naws_enabled());

    session.resize(132, 43).unwrap();
    let expected_resize_naws = build_naws_frame(132, 43);
    let expected_ttype = build_ttype_is_frame("xterm-256color");

    let deadline_wire = Instant::now() + Duration::from_secs(2);
    let mut saw_naws_resize = false;
    let mut saw_ttype = false;
    let mut saw_do_echo = false;
    while Instant::now() < deadline_wire {
        let joined: Vec<u8> = server_rx_wire
            .lock()
            .unwrap()
            .iter()
            .flat_map(|c| c.iter().copied())
            .collect();
        saw_naws_resize = joined
            .windows(expected_resize_naws.len())
            .any(|w| w == expected_resize_naws.as_slice());
        saw_ttype = joined
            .windows(expected_ttype.len())
            .any(|w| w == expected_ttype.as_slice());
        saw_do_echo = joined.windows(3).any(|w| w == [IAC, DO, OPT_ECHO]);
        if saw_naws_resize && saw_ttype && saw_do_echo {
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(saw_naws_resize, "missing resize NAWS(132, 43) frame");
    assert!(saw_ttype, "missing TTYPE IS frame");
    assert!(saw_do_echo, "missing IAC DO ECHO response");

    // Simulate TCP connection drop and verify auto-reconnect keeps prior scrollback!
    if let Some(tx) = drop_first_tx.lock().unwrap().take() {
        let _ = tx.send(());
    }

    assert!(
        buffer
            .wait_for_output_async(Duration::from_secs(3), |s| {
                s.visible_text().contains("SESSION_2_READY")
            })
            .await,
        "Telnet did not reconnect cleanly: {}",
        buffer.export_scrollback_text()
    );
    let full = buffer.export_scrollback_text();
    assert!(full.contains("TELNET_AUTH_OK"));
    assert!(full.contains("Telnet reconnected"));

    session.disconnect();
}

struct MockCliRunner;

impl CommandOutputRunner for MockCliRunner {
    fn run_stdout(&self, program: &str, args: &[&str]) -> Result<String, ProtocolError> {
        if program == "docker" {
            assert_eq!(
                args,
                ["ps", "--format", "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}"]
            );
            Ok("c101\tapi-gateway\tnginx:1.27\tUp 2 hours\nc202\tworker\talpine:latest\tUp 5 minutes\n".to_string())
        } else if program == "podman" {
            Err(ProtocolError::Docker("podman not installed".to_string()))
        } else {
            unreachable!()
        }
    }
}

#[test]
fn docker_discovery_parses_containers_and_builds_exec_shell_spec() {
    let discovery = DockerDiscovery::new(Box::new(MockCliRunner));
    let containers = discovery.list_all_containers(false);
    assert_eq!(containers.len(), 2);
    assert_eq!(containers[0].runtime, DockerRuntime::Docker);
    assert_eq!(containers[0].name, "api-gateway");
    assert_eq!(containers[1].image, "alpine:latest");

    let profile = Profile::docker("API Shell", DockerRuntime::Podman, "web-1");
    let mut cfg = ContainerExecConfig::from_profile_target(&profile.target).unwrap();
    cfg.user = Some("www-data".to_string());
    cfg.working_dir = Some("/srv/app".to_string());
    cfg.shell = Some("/bin/bash".to_string());

    let spec = build_exec_shell_spec(&cfg).unwrap();
    assert_eq!(spec.program, "podman");
    assert_eq!(
        spec.args,
        vec![
            "exec",
            "-it",
            "-u",
            "www-data",
            "-w",
            "/srv/app",
            "web-1",
            "/bin/bash",
        ]
    );
    assert_eq!(spec.display_name, "podman: web-1");

    // Default shell probing fallback for minimal containers.
    cfg.shell = None;
    let fallback_spec = build_exec_shell_spec(&cfg).unwrap();
    assert!(fallback_spec.args.iter().any(|a| a == "/bin/sh"));

    // Empty container rejected.
    let bad = ContainerExecConfig {
        runtime: DockerRuntime::Docker,
        container: "   ".to_string(),
        shell: None,
        user: None,
        working_dir: None,
    };
    assert!(build_exec_shell_spec(&bad).is_err());

    let unused_check = ProfileTarget::Docker {
        runtime: DockerRuntime::Docker,
        container: "x".to_string(),
        shell: None,
        user: None,
        working_dir: None,
    };
    assert!(ContainerExecConfig::from_profile_target(&unused_check).is_some());
}
