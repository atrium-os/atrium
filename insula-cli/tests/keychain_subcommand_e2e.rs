//! `insula keychain pubkey / sign` E2E. Auto-spawns
//! vestibulum-macos, mints/retrieves the keypair,
//! signs a challenge, and verifies the signature
//! against the returned pubkey using ed25519-dalek.

#![cfg(target_os = "macos")]

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use std::path::{Path, PathBuf};
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn build_neighbor(crate_name: &str, bin_name: &str) -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let crate_dir = workspace_root.join(crate_name);
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(crate_dir.join("Cargo.toml"))
        .arg("--bin").arg(bin_name)
        .output()
        .expect("cargo build neighbor");
    assert!(out.status.success(),
            "cargo build {}: {}", crate_name,
            String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn run_insula(install_root: &Path, vest_bin: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_VESTIBULUMD_BIN", vest_bin)
        .env_remove("INSULA_VESTIBULUMD_SOCKET")
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

fn decode_hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn pubkey_then_sign_verify_lifecycle() {
    let install_root = tempfile::tempdir().unwrap();
    let vest_bin = build_neighbor("vestibulum-macos", "vestibulum-macos");

    // pubkey — auto-spawns the daemon, mints the key,
    // prints 64 hex chars.
    let out = run_insula(install_root.path(), &vest_bin,
                         &["keychain", "pubkey", "github.com"]);
    assert!(out.status.success(),
            "pubkey: {}", String::from_utf8_lossy(&out.stderr));
    let pk_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(pk_hex.len(), 64, "pubkey hex should be 64 chars; got {:?}", pk_hex);
    let pk_bytes = decode_hex(&pk_hex);
    let mut pk_arr = [0u8; 32];
    pk_arr.copy_from_slice(&pk_bytes);
    let verifying_key = VerifyingKey::from_bytes(&pk_arr).expect("valid pubkey");

    // sign — signs a 32-byte challenge, returns 64 hex chars.
    let challenge_hex = "00112233445566778899aabbccddeeff\
                        00112233445566778899aabbccddeeff";
    let out = run_insula(install_root.path(), &vest_bin,
                         &["keychain", "sign", "github.com", challenge_hex]);
    assert!(out.status.success(),
            "sign: {}", String::from_utf8_lossy(&out.stderr));
    let sig_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(sig_hex.len(), 128, "sig hex should be 128 chars; got {:?}", sig_hex);
    let sig_bytes = decode_hex(&sig_hex);
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_arr);

    // Verify against the returned pubkey — the
    // load-bearing assertion: the daemon really does
    // hold the private key matching the pubkey it
    // gave us.
    let challenge = decode_hex(challenge_hex);
    verifying_key.verify(&challenge, &signature)
        .expect("signature must verify under the returned pubkey");

    run_insula(install_root.path(), &vest_bin, &["daemons", "down"]);
}

#[test]
fn pubkey_is_stable_across_calls() {
    let install_root = tempfile::tempdir().unwrap();
    let vest_bin = build_neighbor("vestibulum-macos", "vestibulum-macos");

    let a = run_insula(install_root.path(), &vest_bin,
                       &["keychain", "pubkey", "stable.example"]);
    let b = run_insula(install_root.path(), &vest_bin,
                       &["keychain", "pubkey", "stable.example"]);
    let pk_a = String::from_utf8_lossy(&a.stdout).trim().to_string();
    let pk_b = String::from_utf8_lossy(&b.stdout).trim().to_string();
    assert_eq!(pk_a, pk_b,
               "repeated pubkey calls for the same service must agree");

    run_insula(install_root.path(), &vest_bin, &["daemons", "down"]);
}

#[test]
fn sign_with_bad_hex_fails_cleanly() {
    let install_root = tempfile::tempdir().unwrap();
    let vest_bin = build_neighbor("vestibulum-macos", "vestibulum-macos");
    let out = run_insula(install_root.path(), &vest_bin,
                         &["keychain", "sign", "svc", "notvalidhex"]);
    assert!(!out.status.success(), "bad hex should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bad hex challenge") || stderr.contains("hex"),
            "stderr: {}", stderr);
}
