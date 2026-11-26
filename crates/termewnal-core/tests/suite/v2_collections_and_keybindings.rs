use tempfile::TempDir;
use termewnal_core::{
    AppConfig, BellBehavior, ChordMatch, ConfigPaths, DockerRuntime, HighlightRule, KeyAction,
    KeyChord, KeySequence, KeybindingConfig, LoginScriptStep, NewlineMode, Profile, ProfileTarget,
    ProfilesConfig, QuakeEdge, QuickCommand, QuickCommandsConfig, RgbColor, RightClickAction,
    SerialFlowControl, SerialInputMode, SerialOutputMode, SerialParity, SyncProviderKind,
    TabBarPosition, ThemeOverrides, ThemePalette, WorkspaceTemplate, WorkspacesConfig,
    import_tabby_config_file_into_profiles, load_config, load_quick_commands, load_workspaces,
    parse_openssh_config_file, parse_tabby_config_str, save_config, save_profiles,
    save_quick_commands, save_workspaces,
};

#[test]
fn keybindings_parse_resolve_single_and_multi_chord_sequences() {
    let mut cfg = KeybindingConfig::default();
    assert_eq!(
        cfg.bindings_for(KeyAction::NewTab),
        vec!["Ctrl+Shift+T".to_string()]
    );

    cfg.set_bindings(
        KeyAction::NewTab,
        vec!["ctrl+k ctrl+t".to_string(), "Ctrl+Shift+T".to_string()],
    )
    .expect("valid chord bindings");
    assert_eq!(
        cfg.bindings_for(KeyAction::NewTab),
        vec!["Ctrl+K Ctrl+T".to_string(), "Ctrl+Shift+T".to_string()]
    );

    let err = cfg
        .set_bindings(KeyAction::NewTab, vec!["Ctrl+Shift+UnknownKey".to_string()])
        .expect_err("unknown key token must fail");
    assert!(err.contains("Invalid keybinding"));

    let resolver = cfg.resolver();
    let k = KeyChord::parse("Ctrl+K").unwrap();
    let t = KeyChord::parse("Ctrl+T").unwrap();
    assert_eq!(resolver.resolve_step(&[], &k), ChordMatch::PendingPrefix);
    assert_eq!(resolver.resolve_step(&[k], &t), ChordMatch::Matched(KeyAction::NewTab));
    assert_eq!(
        KeySequence::parse("ctrl+alt+space").unwrap().canonical(),
        "Ctrl+Alt+Space"
    );
}

#[test]
fn quick_commands_and_workspaces_persist_atomically_with_backups() {
    let temp = TempDir::new().expect("temp root");
    let paths = ConfigPaths::from_root(temp.path());

    assert!(load_quick_commands(&paths).unwrap().commands.is_empty());
    assert!(load_workspaces(&paths).unwrap().workspaces.is_empty());

    let mut qcfg = QuickCommandsConfig::default();
    let mut qc = QuickCommand::new("Tail Nginx", "tail -f /var/log/nginx/error.log");
    qc.group = Some("Ops".to_string());
    qc.broadcast = true;
    qcfg.commands.push(qc);
    save_quick_commands(&paths, &qcfg).expect("save quick_commands");
    qcfg.commands[0].confirm_before_run = true;
    save_quick_commands(&paths, &qcfg).expect("save quick_commands v2");
    assert!(paths.quick_commands_file().with_file_name("quick_commands.toml.bak.1").is_file());
    assert_eq!(load_quick_commands(&paths).unwrap(), qcfg);

    let mut wcfg = WorkspacesConfig::default();
    wcfg.workspaces.push(WorkspaceTemplate::new("Prod Monitor", Vec::new(), 0));
    save_workspaces(&paths, &wcfg).expect("save workspaces");
    assert_eq!(load_workspaces(&paths).unwrap(), wcfg);
}

#[test]
fn v2_app_config_and_profiles_roundtrip_and_overrides_work() {
    let temp = TempDir::new().expect("temp root");
    let paths = ConfigPaths::from_root(temp.path());

    let mut cfg = AppConfig::default();
    cfg.terminal.tab_bar_position = TabBarPosition::Left;
    cfg.terminal.right_click_action = RightClickAction::Paste;
    cfg.terminal.bell_behavior = BellBehavior::Visual;
    cfg.terminal.cursor_blink = true;
    cfg.terminal.window_opacity = 0.92;
    cfg.terminal.theme = "Solarized Dark".to_string();
    cfg.terminal.theme_overrides = ThemeOverrides {
        background: Some("#112233".to_string()),
        ui_accent: Some("#ff8800".to_string()),
        ..ThemeOverrides::default()
    };
    cfg.terminal.highlight_rules.push(HighlightRule {
        id: "h1".to_string(),
        name: "Errors".to_string(),
        pattern: "ERROR|FATAL".to_string(),
        foreground: Some("#ff5555".to_string()),
        background: None,
        bold: true,
        enabled: true,
        case_insensitive: false,
    });
    cfg.quake.enabled = true;
    cfg.quake.edge = QuakeEdge::Bottom;
    cfg.sync.provider = SyncProviderKind::LocalDirectory;
    save_config(&paths, &cfg).expect("save v2 config");

    let reloaded = load_config(&paths).expect("load v2 config");
    assert_eq!(reloaded, cfg);

    let pal = ThemePalette::for_name_with_overrides(
        &reloaded.terminal.theme,
        &reloaded.terminal.theme_overrides,
    );
    assert_eq!(pal.background, RgbColor::new(0x11, 0x22, 0x33));
    assert_eq!(pal.ui_accent, RgbColor::new(0xff, 0x88, 0x00));

    let mut profiles = ProfilesConfig::default();
    let mut serial = Profile::serial("ESP32", "/dev/ttyUSB0", 921_600);
    if let ProfileTarget::Serial {
        parity,
        flow_control,
        input_mode,
        output_mode,
        newline_mode,
        ..
    } = &mut serial.target
    {
        *parity = SerialParity::Even;
        *flow_control = SerialFlowControl::Hardware;
        *input_mode = SerialInputMode::HexBytes;
        *output_mode = SerialOutputMode::Hexdump;
        *newline_mode = NewlineMode::CrLf;
    }
    let mut telnet = Profile::telnet("Core Switch", "10.0.0.1", 23);
    if let ProfileTarget::Telnet { login_scripts, .. } = &mut telnet.target {
        login_scripts.push(LoginScriptStep {
            expect: "login:".to_string(),
            send: "admin".to_string(),
            is_regex: false,
            append_newline: true,
            delay_ms: 50,
            secret_ref: None,
        });
    }
    let docker = Profile::docker("App Container", DockerRuntime::Podman, "web-1");
    profiles.profiles = vec![serial, telnet, docker];
    save_profiles(&paths, &profiles).expect("save v2 profiles");
    let loaded_profiles = termewnal_core::load_profiles(&paths).expect("load v2 profiles");
    assert_eq!(loaded_profiles, profiles);
}

#[test]
fn openssh_parses_forward_x11_proxy_command_and_tabby_imports_v2_fields() {
    let temp = TempDir::new().expect("temp root");
    let ssh_dir = temp.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    let cfg_path = ssh_dir.join("config");
    std::fs::write(
        &cfg_path,
        "\
Host tunnelbox
    HostName 10.5.5.5
    User ops
    ForwardAgent yes
    ForwardX11 yes
    ProxyCommand ssh -W %h:%p bastion
",
    )
    .unwrap();
    let hosts = parse_openssh_config_file(&cfg_path, temp.path()).unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].forward_agent, Some(true));
    assert_eq!(hosts[0].forward_x11, Some(true));
    assert_eq!(
        hosts[0].proxy_command.as_deref(),
        Some("ssh -W %h:%p bastion")
    );

    let tabby_yaml = r#"
profiles:
  - type: ssh
    name: Bastion X11
    options:
      host: x11.example.com
      user: alice
      agentForward: true
      x11: true
      proxyCommand: nc -X connect -x 127.0.0.1:8080 %h %p
      loginScripts:
        - expect: "MFA:"
          send: "continue"
  - type: ssh
    name: Corp HTTP Proxy
    options:
      host: internal.example.com
      user: ops
      httpProxyHost: proxy.corp.local
      httpProxyPort: 3128
  - type: serial
    name: Sensor UART
    options:
      port: /dev/ttyACM0
      baudrate: 115200
      parity: odd
      rtscts: true
      inputMode: readline
      outputMode: hexdump
  - type: telnet
    name: Router
    options:
      host: 192.168.10.1
      port: 2323
  - type: docker
    name: Redis
    options:
      container: redis-dev
      runtime: podman
      command: /bin/sh
quickCmds:
  - name: Disk Usage
    text: df -h
    group: Diagnostics
    appendCR: true
"#;
    let report = parse_tabby_config_str(tabby_yaml, "fallback").unwrap();
    assert_eq!(report.imported_ssh_count, 2);
    assert_eq!(report.imported_serial_count, 1);
    assert_eq!(report.imported_telnet_count, 1);
    assert_eq!(report.imported_docker_count, 1);
    assert_eq!(report.quick_commands.len(), 1);
    assert_eq!(report.quick_commands[0].name, "Disk Usage");

    let corp_http = report
        .profiles
        .iter()
        .find(|p| p.name == "Corp HTTP Proxy")
        .expect("Corp HTTP Proxy imported");
    match &corp_http.target {
        termewnal_core::ProfileTarget::Ssh {
            proxy: Some(proxy), ..
        } => {
            assert_eq!(proxy.kind, termewnal_core::SshProxyKind::HttpConnect);
            assert_eq!(proxy.host, "proxy.corp.local");
            assert_eq!(proxy.port, 3128);
        }
        other => panic!("expected HTTP CONNECT proxy target, got {other:?}"),
    }

    let paths = ConfigPaths::from_root(temp.path());
    let yaml_path = temp.path().join("tabby.yaml");
    std::fs::write(&yaml_path, tabby_yaml).unwrap();
    let outcome = import_tabby_config_file_into_profiles(&yaml_path, &paths, "fallback", None)
        .expect("import v2 tabby");
    assert_eq!(outcome.added_count, 5);
    assert_eq!(outcome.added_quick_commands_count, 1);
    assert_eq!(load_quick_commands(&paths).unwrap().commands.len(), 1);
}
