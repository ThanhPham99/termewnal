use std::path::PathBuf;

use tempfile::TempDir;
use termewnal_core::{
    CURRENT_SCHEMA_VERSION, ConfigPaths, CoreError, LayoutSplitAxis, LayoutSplitNode, Profile,
    ProfilesConfig, SessionDescriptor, SshAuthMethod, TabLayout, WindowLayout, WorkspaceLayout,
    backup_path, load_layout, load_profiles, save_layout, save_profiles,
};

#[test]
fn layout_round_trip_preserves_windows_tabs_nested_splits_and_session_descriptors() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    // Missing layout.toml loads cleanly as empty default with schema_version = 1.
    let initial = load_layout(&paths).expect("load missing layout");
    assert_eq!(initial.schema_version, CURRENT_SCHEMA_VERSION);
    assert!(initial.windows.is_empty());
    assert_eq!(
        paths.layout_file().file_name().and_then(|n| n.to_str()),
        Some("layout.toml")
    );

    let layout = WorkspaceLayout {
        schema_version: CURRENT_SCHEMA_VERSION,
        windows: vec![
            WindowLayout {
                width: 1440,
                height: 900,
                active_tab: 1,
                tabs: vec![
                    TabLayout {
                        title: Some("Dev".to_string()),
                        focused_pane: 1,
                        zoomed_pane: Some(1),
                        root: LayoutSplitNode::Split {
                            axis: LayoutSplitAxis::Vertical,
                            ratio: 0.35,
                            first: Box::new(LayoutSplitNode::Leaf {
                                session: SessionDescriptor::Local {
                                    profile_id: None,
                                    title: Some("zsh".to_string()),
                                    working_dir: Some(PathBuf::from("/tmp/project-a")),
                                    shell: Some("/bin/zsh".to_string()),
                                },
                            }),
                            second: Box::new(LayoutSplitNode::Split {
                                axis: LayoutSplitAxis::Horizontal,
                                ratio: 0.65,
                                first: Box::new(LayoutSplitNode::Leaf {
                                    session: SessionDescriptor::Remote {
                                        profile_id: Some("prof-prod".to_string()),
                                        host: "prod.example.com".to_string(),
                                        port: 2222,
                                        username: Some("deploy".to_string()),
                                        title: Some("prod".to_string()),
                                        auto_connect_on_startup: true,
                                    },
                                }),
                                second: Box::new(LayoutSplitNode::Leaf {
                                    session: SessionDescriptor::Local {
                                        profile_id: None,
                                        title: Some("bash".to_string()),
                                        working_dir: None,
                                        shell: None,
                                    },
                                }),
                            }),
                        },
                    },
                    TabLayout {
                        title: None,
                        focused_pane: 0,
                        zoomed_pane: None,
                        root: LayoutSplitNode::Leaf {
                            session: SessionDescriptor::Remote {
                                profile_id: Some("prof-staging".to_string()),
                                host: "staging.example.com".to_string(),
                                port: 22,
                                username: Some("ops".to_string()),
                                title: None,
                                auto_connect_on_startup: false,
                            },
                        },
                    },
                ],
            },
            WindowLayout {
                width: 1024,
                height: 768,
                active_tab: 0,
                tabs: vec![TabLayout {
                    title: Some("Logs".to_string()),
                    focused_pane: 0,
                    zoomed_pane: None,
                    root: LayoutSplitNode::Leaf {
                        session: SessionDescriptor::Local {
                            profile_id: None,
                            title: None,
                            working_dir: Some(PathBuf::from("/var/log")),
                            shell: None,
                        },
                    },
                }],
            },
        ],
    };

    save_layout(&paths, &layout).expect("save layout v1");
    let loaded = load_layout(&paths).expect("reload layout");
    assert_eq!(loaded, layout);

    let raw = std::fs::read_to_string(paths.layout_file()).expect("read layout.toml");
    assert!(raw.contains("schema_version = 1"));
    assert!(raw.contains("/tmp/project-a"));
    assert!(raw.contains("prod.example.com"));

    // Second atomic save rotates previous layout to .bak.1.
    let mut updated = layout.clone();
    updated.windows.pop();
    save_layout(&paths, &updated).expect("save layout v2");
    assert_eq!(load_layout(&paths).expect("reload v2"), updated);
    assert!(backup_path(&paths.layout_file(), 1).is_file());

    // Unsupported schema_version and malformed TOML fail cleanly.
    std::fs::write(paths.layout_file(), "schema_version = 42\n").expect("write bad version");
    assert!(matches!(
        load_layout(&paths),
        Err(CoreError::UnsupportedSchemaVersion {
            found: 42,
            expected: 1,
            ..
        })
    ));

    std::fs::write(paths.layout_file(), "schema_version = [broken").expect("write malformed");
    assert!(matches!(
        load_layout(&paths),
        Err(CoreError::TomlDeserialize { span: Some(_), .. })
    ));
}

#[test]
fn profile_auto_connect_on_startup_defaults_false_and_round_trips_in_profiles_toml() {
    let temp = TempDir::new().expect("create temp root");
    let paths = ConfigPaths::from_root(temp.path());

    let local = Profile::local_shell("Local", None);
    assert!(!local.auto_connect_on_startup());

    let mut ssh_auto = Profile::ssh("AutoSSH", "auto.example.com", 22, "alice", SshAuthMethod::Agent);
    assert!(!ssh_auto.auto_connect_on_startup());
    ssh_auto.set_auto_connect_on_startup(true);
    assert!(ssh_auto.auto_connect_on_startup());

    let mut profiles = ProfilesConfig::default();
    profiles.profiles.push(local);
    profiles.profiles.push(ssh_auto);
    save_profiles(&paths, &profiles).expect("save profiles");

    let reloaded = load_profiles(&paths).expect("reload profiles");
    assert!(!reloaded.profiles[0].auto_connect_on_startup());
    assert!(reloaded.profiles[1].auto_connect_on_startup());
}
