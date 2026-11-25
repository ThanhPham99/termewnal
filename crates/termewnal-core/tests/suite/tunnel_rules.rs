use std::time::Duration;

use tempfile::TempDir;
use termewnal_core::{
    ConfigPaths, DEFAULT_TUNNEL_HOLD_GRACE_SECS, Profile, ProfilesConfig, SshAuthMethod,
    TunnelDirection, TunnelRule, is_public_bind_host, load_profiles, save_profiles,
};

#[test]
fn tunnel_rule_defaults_bind_host_to_loopback_and_roundtrips_resilience_fields() {
    let raw = r#"
schema_version = 1

[[profiles]]
id = "prof-1"
name = "Bastion"

[profiles.target]
kind = "ssh"
host = "bastion.internal"
port = 22
username = "ops"

[[profiles.tunnels]]
id = "tun-local"
name = "Postgres"
direction = "local"
bind_port = 5432
target_host = "127.0.0.1"
target_port = 5432
"#;

    let parsed: ProfilesConfig = toml::from_str(raw).expect("parse profiles TOML with minimal tunnel");
    let rule = &parsed.profiles[0].tunnels[0];
    assert_eq!(rule.bind_host, "127.0.0.1");
    assert_eq!(rule.effective_bind_host(), "127.0.0.1");
    assert!(!rule.standalone);
    assert!(!rule.allow_public_bind);
    assert_eq!(rule.hold_grace_secs, None);
    assert_eq!(DEFAULT_TUNNEL_HOLD_GRACE_SECS, 10);
    assert_eq!(rule.effective_hold_grace(), Duration::from_secs(10));
    assert!(!rule.requires_public_bind_opt_in());

    let temp = TempDir::new().expect("create temp dir");
    let paths = ConfigPaths::from_root(temp.path());
    let mut profile = Profile::ssh("App", "app.internal", 22, "deploy", SshAuthMethod::Agent);
    let mut custom = TunnelRule::dynamic_socks5("SOCKS", 1080);
    custom.bind_host = "0.0.0.0".to_string();
    custom.standalone = true;
    custom.allow_public_bind = true;
    custom.hold_grace_secs = Some(25);
    profile.tunnels.push(custom);

    let cfg = ProfilesConfig {
        schema_version: 1,
        profiles: vec![profile],
    };
    save_profiles(&paths, &cfg).expect("save profiles");
    let loaded = load_profiles(&paths).expect("reload profiles");
    let reloaded_rule = &loaded.profiles[0].tunnels[0];
    assert_eq!(reloaded_rule.direction, TunnelDirection::Dynamic);
    assert_eq!(reloaded_rule.bind_host, "0.0.0.0");
    assert!(reloaded_rule.standalone);
    assert!(reloaded_rule.allow_public_bind);
    assert_eq!(reloaded_rule.hold_grace_secs, Some(25));
    assert_eq!(reloaded_rule.effective_hold_grace(), Duration::from_secs(25));
    assert!(!reloaded_rule.requires_public_bind_opt_in());
}

#[test]
fn public_bind_guard_flags_unspecified_and_non_loopback_hosts_until_opted_in() {
    assert!(!is_public_bind_host("127.0.0.1"));
    assert!(!is_public_bind_host("127.0.0.2"));
    assert!(!is_public_bind_host("::1"));
    assert!(!is_public_bind_host("[::1]"));
    assert!(!is_public_bind_host("localhost"));
    assert!(!is_public_bind_host(""));

    assert!(is_public_bind_host("0.0.0.0"));
    assert!(is_public_bind_host("::"));
    assert!(is_public_bind_host("[::]"));
    assert!(is_public_bind_host("192.168.1.50"));
    assert!(is_public_bind_host("*"));

    let mut rule = TunnelRule::local("DB", 5432, "10.0.0.5", 5432);
    assert_eq!(rule.bind_host, "127.0.0.1");
    assert!(!rule.requires_public_bind_opt_in());

    rule.bind_host = "0.0.0.0".to_string();
    assert!(rule.requires_public_bind_opt_in());

    rule.allow_public_bind = true;
    assert!(!rule.requires_public_bind_opt_in());
}

#[test]
fn remote_tunnel_rule_roundtrips_and_does_not_trigger_local_public_bind_guard() {
    let mut rule = TunnelRule::remote("Remote Web", 8080, "127.0.0.1", 3000);
    assert_eq!(rule.direction, TunnelDirection::Remote);
    assert_eq!(rule.bind_host, "127.0.0.1");
    assert_eq!(rule.bind_port, 8080);
    assert_eq!(rule.target_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(rule.target_port, Some(3000));
    assert!(!rule.requires_public_bind_opt_in());

    rule.bind_host = "0.0.0.0".to_string();
    assert!(!rule.requires_public_bind_opt_in());

    let temp = TempDir::new().expect("create temp dir");
    let paths = ConfigPaths::from_root(temp.path());
    let mut profile = Profile::ssh("App", "app.internal", 22, "deploy", SshAuthMethod::Agent);
    profile.tunnels.push(rule);

    let cfg = ProfilesConfig {
        schema_version: 1,
        profiles: vec![profile],
    };
    save_profiles(&paths, &cfg).expect("save profiles");
    let loaded = load_profiles(&paths).expect("reload profiles");
    let reloaded = &loaded.profiles[0].tunnels[0];
    assert_eq!(reloaded.direction, TunnelDirection::Remote);
    assert_eq!(reloaded.bind_host, "0.0.0.0");
    assert_eq!(reloaded.bind_port, 8080);
    assert_eq!(reloaded.target_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(reloaded.target_port, Some(3000));
    assert!(!reloaded.requires_public_bind_opt_in());
}
