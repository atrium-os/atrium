//! Signing-related CLI subcommands:
//! - `insula keygen <key-id> <output-dir>`
//! - `insula sign <bundle-dir> --key <key-file>`
//! - `insula publishers add <key-id> <pub-file>`
//! - `insula publishers list`
//! - `insula publishers remove <key-id>`

use ed25519_dalek::{SigningKey, VerifyingKey};
use insula_bundle::signing::{self, BundleSignature};
use insula_bundle::InsulaBundle;
use rand::rngs::OsRng;
use std::path::{Path, PathBuf};

pub fn cmd_keygen(args: &[String]) -> Result<(), String> {
    let key_id = args.first().ok_or_else(|| {
        "keygen: missing <key-id> argument".to_string()
    })?;
    let out_dir_str = args.get(1).ok_or_else(|| {
        "keygen: missing <output-dir> argument".to_string()
    })?;
    let out_dir = PathBuf::from(out_dir_str);
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("mkdir {}: {}", out_dir.display(), e))?;

    let sk = SigningKey::generate(&mut OsRng);
    let pk = sk.verifying_key();

    let sk_path = out_dir.join(format!("{}.sk", key_id));
    let pk_path = out_dir.join(format!("{}.pub", key_id));

    std::fs::write(&sk_path, sk.to_bytes())
        .map_err(|e| format!("write {}: {}", sk_path.display(), e))?;
    std::fs::write(&pk_path, pk.to_bytes())
        .map_err(|e| format!("write {}: {}", pk_path.display(), e))?;

    // Tighten secret-key perms.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&sk_path)
            .map_err(|e| e.to_string())?
            .permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(&sk_path, perms);
    }

    println!("generated keypair {}:", key_id);
    println!("  secret: {}", sk_path.display());
    println!("  public: {}", pk_path.display());
    println!();
    println!("To trust this publisher for installs, run:");
    println!("  insula publishers add {} {}", key_id, pk_path.display());
    Ok(())
}

pub fn cmd_sign(args: &[String]) -> Result<(), String> {
    // Parse: <bundle-dir> --key <key-file> [--key-id <id>]
    let bundle_dir = args.first().ok_or_else(|| {
        "sign: missing <bundle-dir> argument".to_string()
    })?;
    let mut key_path: Option<&str> = None;
    let mut key_id: Option<&str> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--key" => {
                key_path = args.get(i + 1).map(|s| s.as_str());
                i += 2;
            }
            "--key-id" => {
                key_id = args.get(i + 1).map(|s| s.as_str());
                i += 2;
            }
            other => return Err(format!("sign: unknown flag '{}'", other)),
        }
    }
    let key_path = key_path.ok_or_else(|| "sign: --key <file> is required".to_string())?;

    // Load secret key.
    let sk_bytes = std::fs::read(key_path)
        .map_err(|e| format!("read {}: {}", key_path, e))?;
    if sk_bytes.len() != 32 {
        return Err(format!(
            "key file {} must be exactly 32 bytes (ed25519 secret), got {}",
            key_path, sk_bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&sk_bytes);
    let sk = SigningKey::from_bytes(&arr);

    // Default key-id: file stem (e.g. "publisher-1.sk" -> "publisher-1").
    let derived_id;
    let key_id = if let Some(id) = key_id {
        id
    } else {
        let stem = Path::new(key_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed-publisher");
        derived_id = stem.to_string();
        &derived_id
    };

    let bundle = InsulaBundle::read(bundle_dir)
        .map_err(|e| format!("read bundle: {}", e))?;
    let sig = signing::sign_bundle(&bundle, key_id, &sk)
        .map_err(|e| format!("sign bundle: {}", e))?;
    signing::write_signature_to_bundle(&bundle.root, &sig)
        .map_err(|e| format!("write signature: {}", e))?;

    println!("signed {} as {}", bundle.app_id(), key_id);
    println!("  signature file: {}/signature", bundle.root.display());
    Ok(())
}

pub fn cmd_publishers(args: &[String], install_root: &Path) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "add" => publishers_add(&args[1..], install_root),
        "list" => publishers_list(install_root),
        "remove" => publishers_remove(&args[1..], install_root),
        other => Err(format!(
            "publishers: unknown subcommand '{}' (use add|list|remove)",
            other
        )),
    }
}

fn trusted_dir(install_root: &Path) -> PathBuf {
    install_root.join("trusted-publishers")
}

fn publishers_add(args: &[String], install_root: &Path) -> Result<(), String> {
    let key_id = args.first().ok_or_else(|| {
        "publishers add: missing <key-id> argument".to_string()
    })?;
    let pub_path = args.get(1).ok_or_else(|| {
        "publishers add: missing <pub-file> argument".to_string()
    })?;
    let pub_bytes = std::fs::read(pub_path)
        .map_err(|e| format!("read {}: {}", pub_path, e))?;
    if pub_bytes.len() != 32 {
        return Err(format!(
            "pubkey file must be 32 bytes (ed25519), got {}",
            pub_bytes.len()
        ));
    }
    let dir = trusted_dir(install_root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("mkdir {}: {}", dir.display(), e))?;
    let dst = dir.join(format!("{}.pub", key_id));
    std::fs::write(&dst, &pub_bytes)
        .map_err(|e| format!("write {}: {}", dst.display(), e))?;
    println!("trusted publisher {} added at {}", key_id, dst.display());
    Ok(())
}

fn publishers_list(install_root: &Path) -> Result<(), String> {
    let dir = trusted_dir(install_root);
    if !dir.is_dir() {
        println!("(no trusted publishers)");
        return Ok(());
    }
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().and_then(|s| s.to_str()) == Some("pub")
        })
        .filter_map(|e| {
            e.path().file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
        })
        .collect();
    if entries.is_empty() {
        println!("(no trusted publishers)");
        return Ok(());
    }
    entries.sort();
    for id in entries {
        println!("{}", id);
    }
    Ok(())
}

fn publishers_remove(args: &[String], install_root: &Path) -> Result<(), String> {
    let key_id = args.first().ok_or_else(|| {
        "publishers remove: missing <key-id> argument".to_string()
    })?;
    let dst = trusted_dir(install_root).join(format!("{}.pub", key_id));
    if !dst.exists() {
        return Err(format!("no trusted publisher with id {}", key_id));
    }
    std::fs::remove_file(&dst).map_err(|e| e.to_string())?;
    println!("removed trusted publisher {}", key_id);
    Ok(())
}

/// Load a trusted publisher's pubkey from the install
/// root's trusted-publishers store.
pub fn load_trusted_publisher(
    install_root: &Path,
    key_id: &str,
) -> Result<VerifyingKey, String> {
    let path = trusted_dir(install_root).join(format!("{}.pub", key_id));
    let bytes = std::fs::read(&path)
        .map_err(|_| format!("no trusted publisher with id {}", key_id))?;
    if bytes.len() != 32 {
        return Err(format!(
            "trusted publisher {} pubkey file is malformed",
            key_id
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr).map_err(|e| format!("decode pubkey: {}", e))
}

/// Verify a bundle's signature against the install
/// root's trusted-publishers store. Returns Ok(()) if
/// signed and verifying; an error message otherwise.
pub fn verify_bundle_signature(
    bundle: &InsulaBundle,
    install_root: &Path,
) -> Result<BundleSignature, String> {
    let sig = signing::read_signature(&bundle.root)
        .map_err(|e| format!("read signature: {}", e))?;
    let trusted = load_trusted_publisher(install_root, &sig.key_id)?;
    signing::verify_bundle(bundle, &trusted)
        .map_err(|e| format!("verify signature: {}", e))?;
    Ok(sig)
}
