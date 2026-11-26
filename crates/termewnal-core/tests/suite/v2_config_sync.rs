use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use termewnal_core::backup_path;
use termewnal_core::collections::{QuickCommand, QuickCommandsConfig, WorkspacesConfig, save_quick_commands, save_workspaces};
use termewnal_core::config::{AppConfig, ConfigPaths, SyncConfig, SyncProviderKind, load_config, save_config};
use termewnal_core::layout::{LayoutSplitNode, SessionDescriptor, TabLayout};
use termewnal_core::profile::{Profile, ProfilesConfig, SshAuthMethod, save_profiles};
use termewnal_core::sync::{
    ConfigSyncService, SYNC_BUNDLE_SCHEMA_VERSION, SyncBundle, decode_sync_envelope,
    encode_sync_envelope_with_kdf,
};
use termewnal_core::vault::{InMemorySecretStore, SecretStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn populate_sample_configs(paths: &ConfigPaths, theme: &str) {
    let mut cfg = AppConfig::default();
    cfg.terminal.theme = theme.to_string();
    cfg.sync.vault_token_ref = Some("sync/api-token".to_string());
    save_config(paths, &cfg).unwrap();

    let mut profiles = ProfilesConfig::default();
    profiles
        .profiles
        .push(Profile::ssh("Prod Box", "10.0.0.5", 22, "deploy", SshAuthMethod::Agent));
    save_profiles(paths, &profiles).unwrap();

    let mut cmds = QuickCommandsConfig::default();
    cmds.commands.push(QuickCommand::new("Status", "uptime"));
    save_quick_commands(paths, &cmds).unwrap();

    let mut ws = WorkspacesConfig::default();
    ws.workspaces.push(termewnal_core::WorkspaceTemplate::new(
        "Ops",
        vec![TabLayout {
            title: Some("OpsTab".to_string()),
            color: None,
            pinned: false,
            focused_pane: 0,
            zoomed_pane: None,
            root: LayoutSplitNode::Leaf {
                session: SessionDescriptor::Local {
                    profile_id: None,
                    title: None,
                    working_dir: None,
                    shell: None,
                },
            },
        }],
        0,
    ));
    save_workspaces(paths, &ws).unwrap();
}

#[test]
fn sync_bundle_validates_rejects_evil_filenames_and_rotates_backups_without_leaking_vault() {
    let src_dir = TempDir::new().unwrap();
    let src_paths = ConfigPaths::from_root(src_dir.path());
    populate_sample_configs(&src_paths, "TokyoNight");

    // Simulate vault file on disk; sync must never collect it.
    fs::create_dir_all(src_paths.vault_file().parent().unwrap()).unwrap();
    fs::write(src_paths.vault_file(), b"SUPER_SECRET_VAULT_BYTES").unwrap();

    let bundle = SyncBundle::collect_from_paths(&src_paths).unwrap();
    assert_eq!(bundle.schema_version, SYNC_BUNDLE_SCHEMA_VERSION);
    assert_eq!(bundle.files.len(), 4);
    assert!(!bundle.files.keys().any(|k| k.contains("vault")));

    // Reject unknown/path-traversal filenames and broken TOML without modifying dest.
    let dst_dir = TempDir::new().unwrap();
    let dst_paths = ConfigPaths::from_root(dst_dir.path());
    populate_sample_configs(&dst_paths, "Catppuccin");

    let mut evil = bundle.clone();
    evil.files
        .insert("../vault.bin".to_string(), "pwned".to_string());
    assert!(evil.apply_to_paths(&dst_paths).is_err());
    assert_eq!(load_config(&dst_paths).unwrap().terminal.theme, "Catppuccin");

    let mut corrupt = bundle.clone();
    corrupt
        .files
        .insert("config.toml".to_string(), "not valid toml = = [".to_string());
    assert!(corrupt.apply_to_paths(&dst_paths).is_err());
    assert_eq!(load_config(&dst_paths).unwrap().terminal.theme, "Catppuccin");

    // Valid apply rotates `.bak` backup and restores destination cleanly.
    let report = bundle.apply_to_paths(&dst_paths).unwrap();
    assert_eq!(report.applied_files.len(), 4);
    assert_eq!(load_config(&dst_paths).unwrap().terminal.theme, "TokyoNight");

    let bak = backup_path(&dst_paths.config_file(), 1);
    assert!(bak.exists(), "expected config.toml.bak at {}", bak.display());
    let bak_text = fs::read_to_string(bak).unwrap();
    assert!(bak_text.contains("Catppuccin"));
}

#[test]
fn encrypted_sync_envelope_roundtrip_wrong_passphrase_and_tampering() {
    let dir = TempDir::new().unwrap();
    let paths = ConfigPaths::from_root(dir.path());
    populate_sample_configs(&paths, "Dracula");

    let bundle = SyncBundle::collect_from_paths(&paths).unwrap();
    let wire = encode_sync_envelope_with_kdf(&bundle, Some("my-sync-pass"), 64, 1, 1).unwrap();
    let wire_str = std::str::from_utf8(&wire).unwrap();
    assert!(wire_str.contains("\"encrypted\""));
    assert!(!wire_str.contains("Dracula"));
    assert!(!wire_str.contains("Prod Box"));

    assert!(decode_sync_envelope(&wire, None).is_err());
    assert!(decode_sync_envelope(&wire, Some("wrong-pass")).is_err());

    let mut tampered = wire.clone();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0x01;
    assert!(decode_sync_envelope(&tampered, Some("my-sync-pass")).is_err());

    let decoded = decode_sync_envelope(&wire, Some("my-sync-pass")).unwrap();
    assert_eq!(decoded.files, bundle.files);
}

#[derive(Default)]
struct HttpMockServerState {
    files: HashMap<String, String>,
    gists: HashMap<String, String>,
    seen_auth_headers: Vec<(String, String)>,
}

async fn spawn_http_sync_server(state: Arc<Mutex<HttpMockServerState>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let st = Arc::clone(&state);
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = [0_u8; 4096];
                let header_end;
                loop {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = pos + 4;
                        break;
                    }
                }
                let head_str = String::from_utf8_lossy(&raw[..header_end]).into_owned();
                let mut lines = head_str.lines();
                let req_line = lines.next().unwrap_or("");
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("/");

                let mut content_len: usize = 0;
                let mut auth_header: Option<String> = None;
                for line in lines {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse::<usize>().unwrap_or(0);
                        } else if k.eq_ignore_ascii_case("authorization") {
                            auth_header = Some(v.trim().to_string());
                        }
                    }
                }

                while raw.len() < header_end + content_len {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                }
                let body = &raw[header_end..(header_end + content_len).min(raw.len())];

                let (status, resp_body) = {
                    let mut guard = st.lock().unwrap();
                    if let Some(auth) = auth_header {
                        guard
                            .seen_auth_headers
                            .push((path.to_string(), auth));
                    }

                    if path.starts_with("/dav/") {
                        match method {
                            "PUT" => {
                                let text = String::from_utf8_lossy(body).into_owned();
                                guard.files.insert(path.to_string(), text);
                                ("201 Created", "{}".to_string())
                            }
                            "GET" => match guard.files.get(path) {
                                Some(text) => ("200 OK", text.clone()),
                                None => ("404 Not Found", "not found".to_string()),
                            },
                            _ => ("405 Method Not Allowed", String::new()),
                        }
                    } else if path == "/gists" && method == "POST" {
                        let req_json: serde_json::Value = serde_json::from_slice(body).unwrap();
                        let content = req_json["files"]["termewnal-sync.json"]["content"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        let id = "gist-abc-123".to_string();
                        guard.gists.insert(id.clone(), content.clone());
                        let resp = serde_json::json!({
                            "id": id,
                            "files": {
                                "termewnal-sync.json": {
                                    "content": content,
                                    "truncated": false
                                }
                            }
                        });
                        ("201 Created", resp.to_string())
                    } else if let Some(id) = path.strip_prefix("/gists/") {
                        match method {
                            "PATCH" => {
                                let req_json: serde_json::Value =
                                    serde_json::from_slice(body).unwrap();
                                let content = req_json["files"]["termewnal-sync.json"]["content"]
                                    .as_str()
                                    .unwrap()
                                    .to_string();
                                guard.gists.insert(id.to_string(), content.clone());
                                let resp = serde_json::json!({
                                    "id": id,
                                    "files": {
                                        "termewnal-sync.json": {
                                            "content": content,
                                            "truncated": false
                                        }
                                    }
                                });
                                ("200 OK", resp.to_string())
                            }
                            "GET" => match guard.gists.get(id) {
                                Some(content) => {
                                    let resp = serde_json::json!({
                                        "id": id,
                                        "files": {
                                            "termewnal-sync.json": {
                                                "content": content,
                                                "truncated": false
                                            }
                                        }
                                    });
                                    ("200 OK", resp.to_string())
                                }
                                None => ("404 Not Found", "{}".to_string()),
                            },
                            _ => ("405 Method Not Allowed", String::new()),
                        }
                    } else {
                        ("404 Not Found", String::new())
                    }
                };

                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    format!("http://{addr}")
}

#[tokio::test]
async fn config_sync_service_syncs_local_webdav_and_github_gist_end_to_end() {
    let srv_state = Arc::new(Mutex::new(HttpMockServerState::default()));
    let base_url = spawn_http_sync_server(Arc::clone(&srv_state)).await;

    let service = ConfigSyncService::new()
        .with_github_api_base(&base_url)
        .with_kdf_params(64, 1, 1);

    let src_dir = TempDir::new().unwrap();
    let src_paths = ConfigPaths::from_root(src_dir.path());
    populate_sample_configs(&src_paths, "Kanagawa");

    let vault = InMemorySecretStore::new();
    vault.set_secret("sync/api-token", "pat-secret-xyz").unwrap();

    // 1. LocalDirectory encrypted push + pull into dst_paths.
    let local_sync_dir = TempDir::new().unwrap();
    let local_cfg = SyncConfig {
        provider: SyncProviderKind::LocalDirectory,
        local_dir: Some(local_sync_dir.path().to_path_buf()),
        encrypt_bundle: true,
        ..SyncConfig::default()
    };
    let local_push = service
        .push(&src_paths, &local_cfg, Some(&vault), Some("local-pw"))
        .await
        .unwrap();
    assert!(local_push.bytes_uploaded > 0);

    let dst1 = TempDir::new().unwrap();
    let dst1_paths = ConfigPaths::from_root(dst1.path());
    let local_pull = service
        .pull(&dst1_paths, &local_cfg, Some(&vault), Some("local-pw"))
        .await
        .unwrap();
    assert_eq!(local_pull.apply_report.applied_files.len(), 4);
    assert_eq!(load_config(&dst1_paths).unwrap().terminal.theme, "Kanagawa");

    // 2. WebDAV push + pull via real ReqwestSyncHttpClient over HTTP loopback.
    let webdav_cfg = SyncConfig {
        provider: SyncProviderKind::WebDav,
        webdav_url: Some(format!("{base_url}/dav")),
        webdav_username: Some("alice".to_string()),
        vault_token_ref: Some("sync/api-token".to_string()),
        encrypt_bundle: false,
        ..SyncConfig::default()
    };
    let dav_push = service
        .push(&src_paths, &webdav_cfg, Some(&vault), None)
        .await
        .unwrap();
    assert!(dav_push.target_location.ends_with("/dav/termewnal-sync.json"));

    let dst2 = TempDir::new().unwrap();
    let dst2_paths = ConfigPaths::from_root(dst2.path());
    service
        .pull(&dst2_paths, &webdav_cfg, Some(&vault), None)
        .await
        .unwrap();
    assert_eq!(load_config(&dst2_paths).unwrap().terminal.theme, "Kanagawa");

    // 3. GitHub Gist create (POST) -> update (PATCH) -> encrypted pull into dst3.
    let mut gist_cfg = SyncConfig {
        provider: SyncProviderKind::GitHubGist,
        gist_id: None,
        vault_token_ref: Some("sync/api-token".to_string()),
        encrypt_bundle: true,
        ..SyncConfig::default()
    };
    let created = service
        .push(&src_paths, &gist_cfg, Some(&vault), Some("gist-pass"))
        .await
        .unwrap();
    assert_eq!(created.created_gist_id.as_deref(), Some("gist-abc-123"));

    // Modify config locally and PATCH the existing Gist.
    gist_cfg.gist_id = created.created_gist_id.clone();
    populate_sample_configs(&src_paths, "Synthwave84");
    let patched = service
        .push(&src_paths, &gist_cfg, Some(&vault), Some("gist-pass"))
        .await
        .unwrap();
    assert!(patched.created_gist_id.is_none());

    let dst3 = TempDir::new().unwrap();
    let dst3_paths = ConfigPaths::from_root(dst3.path());
    service
        .pull(&dst3_paths, &gist_cfg, Some(&vault), Some("gist-pass"))
        .await
        .unwrap();
    assert_eq!(load_config(&dst3_paths).unwrap().terminal.theme, "Synthwave84");

    // Confirm Vault token stayed inside Authorization headers and NEVER appeared in synced config.toml.
    let synced_cfg_raw = fs::read_to_string(dst3_paths.config_file()).unwrap();
    assert!(!synced_cfg_raw.contains("pat-secret-xyz"));
    let auths = &srv_state.lock().unwrap().seen_auth_headers;
    assert!(auths.iter().any(|(p, h)| p.starts_with("/dav/") && h.starts_with("Basic ")));
    assert!(auths.iter().any(|(p, h)| p.starts_with("/gists") && h == "Bearer pat-secret-xyz"));
}
