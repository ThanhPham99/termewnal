use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{Algorithm, HashAlg, PrivateKey};
use sha1::Sha1;
use tempfile::TempDir;
use termewnal_ssh::{
    KnownHostStatus, fingerprint_sha256, record_known_host_path, verify_known_host_path,
};

fn random_ed25519_pubkey() -> russh::keys::PublicKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .expect("generate ed25519 key")
        .public_key()
        .clone()
}

fn random_rsa_pubkey() -> russh::keys::PublicKey {
    let kp = russh::keys::ssh_key::private::RsaKeypair::random(&mut rand::rng(), 2048)
        .expect("generate 2048-bit rsa key");
    PrivateKey::from(kp).public_key().clone()
}

fn hash_host_entry(host_token: &str, salt: &[u8]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(salt).expect("hmac key");
    mac.update(host_token.as_bytes());
    let digest = mac.finalize().into_bytes();
    format!("|1|{}|{}", BASE64.encode(salt), BASE64.encode(&digest))
}

#[test]
fn unknown_host_is_detected_and_recording_is_idempotent_with_0600_perms() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join(".ssh").join("known_hosts");
    let pubkey = random_ed25519_pubkey();
    let expected_fp = pubkey.fingerprint(HashAlg::Sha256).to_string();
    assert_eq!(fingerprint_sha256(&pubkey), expected_fp);
    assert!(expected_fp.starts_with("SHA256:"));

    // Missing file -> Unknown.
    let status = verify_known_host_path("bastion.example.com", 22, &pubkey, &known_hosts).unwrap();
    assert_eq!(
        status,
        KnownHostStatus::Unknown {
            host: "bastion.example.com".to_string(),
            port: 22,
            key_type: "ssh-ed25519".to_string(),
            fingerprint_sha256: expected_fp.clone(),
        }
    );

    // Record standard port 22 and custom port 2222.
    record_known_host_path("bastion.example.com", 22, &pubkey, &known_hosts).unwrap();
    record_known_host_path("bastion.example.com", 22, &pubkey, &known_hosts).unwrap();
    record_known_host_path("bastion.example.com", 2222, &pubkey, &known_hosts).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&known_hosts).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let content = std::fs::read_to_string(&known_hosts).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2, "idempotent write should produce 2 lines:\n{content}");
    assert!(lines[0].starts_with("bastion.example.com ssh-ed25519 "));
    assert!(lines[1].starts_with("[bastion.example.com]:2222 ssh-ed25519 "));

    assert_eq!(
        verify_known_host_path("bastion.example.com", 22, &pubkey, &known_hosts).unwrap(),
        KnownHostStatus::Trusted
    );
    assert_eq!(
        verify_known_host_path("bastion.example.com", 2222, &pubkey, &known_hosts).unwrap(),
        KnownHostStatus::Trusted
    );
    assert!(matches!(
        verify_known_host_path("bastion.example.com", 2223, &pubkey, &known_hosts).unwrap(),
        KnownHostStatus::Unknown { .. }
    ));
}

#[test]
fn parses_comments_tabs_wildcards_hashed_entries_and_reports_exact_mismatch_line() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");

    let key_tab = random_ed25519_pubkey();
    let key_hashed = random_ed25519_pubkey();
    let key_rsa = random_rsa_pubkey();
    let key_attacker = random_ed25519_pubkey();

    let hashed_token = hash_host_entry("[hashed.example.net]:2200", b"0123456789abcdef");

    let contents = format!(
        "# line 1 comment\n\
         \n\
         *.corp.local,!blocked.corp.local\t{}\t# trailing comment\n\
         {} {}\n\
         prod.example.com {}\n",
        key_tab.to_openssh().unwrap(),
        hashed_token,
        key_hashed.to_openssh().unwrap(),
        key_rsa.to_openssh().unwrap(),
    );
    std::fs::write(&known_hosts, contents).unwrap();

    // Wildcard + tab-separated entry matches app.corp.local, excludes blocked.corp.local.
    assert_eq!(
        verify_known_host_path("app.corp.local", 22, &key_tab, &known_hosts).unwrap(),
        KnownHostStatus::Trusted
    );
    assert!(matches!(
        verify_known_host_path("blocked.corp.local", 22, &key_tab, &known_hosts).unwrap(),
        KnownHostStatus::Unknown { .. }
    ));

    // Hashed |1| entry matches [hashed.example.net]:2200.
    assert_eq!(
        verify_known_host_path("hashed.example.net", 2200, &key_hashed, &known_hosts).unwrap(),
        KnownHostStatus::Trusted
    );

    // Mismatch on line 3 (remember comment on line 1 + blank line 2 must be counted!).
    let mismatch = verify_known_host_path("app.corp.local", 22, &key_attacker, &known_hosts).unwrap();
    assert_eq!(
        mismatch,
        KnownHostStatus::Mismatch {
            host: "app.corp.local".to_string(),
            port: 22,
            line: 3,
            recorded_fingerprint_sha256: fingerprint_sha256(&key_tab),
            offered_fingerprint_sha256: fingerprint_sha256(&key_attacker),
            key_type: "ssh-ed25519".to_string(),
        }
    );

    // Host with only ssh-rsa recorded returns Unknown when ssh-ed25519 is offered, Trusted for ssh-rsa.
    assert_eq!(
        verify_known_host_path("prod.example.com", 22, &key_rsa, &known_hosts).unwrap(),
        KnownHostStatus::Trusted
    );
    assert!(matches!(
        verify_known_host_path("prod.example.com", 22, &key_attacker, &known_hosts).unwrap(),
        KnownHostStatus::Unknown { .. }
    ));
}

#[test]
fn revoked_host_key_entry_blocks_as_mismatch() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let revoked_key = random_ed25519_pubkey();

    let contents = format!(
        "@revoked compromised.example.com {}\n",
        revoked_key.to_openssh().unwrap()
    );
    std::fs::write(&known_hosts, contents).unwrap();

    let status = verify_known_host_path("compromised.example.com", 22, &revoked_key, &known_hosts)
        .unwrap();
    assert!(matches!(
        status,
        KnownHostStatus::Mismatch { line: 1, .. }
    ));
}
