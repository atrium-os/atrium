//! The manifest TRUST gate — shared by **every** user-app launch vector so the
//! check is uniform, not copied per path. A manifest's capabilities are honoured
//! only if a trusted publisher signed it (keyed Sigstore, `portcullis-sig`): an
//! unsigned / tampered / untrusted manifest is refused before any cap is granted
//! or any jail created. Applies to *any* app under `/var/lib/atrium/apps`,
//! whichever way it is launched (`Request::Launch`, the session/stdio path, …).
//!
//! System services (`/etc/atrium/services.d`) are a *different* trust root — the
//! operator installed them into the base — and are intentionally out of scope for
//! third-party publisher signatures.

use std::path::Path;

/// The trusted-publisher set lives here (`*.pem`). Which publishers Portcullis
/// will honour is Atrium policy.
pub const PUBLISHERS_DIR: &str = "/etc/atrium/publishers";

/// Load every trusted-publisher public key (`*.pem`) from `dir`. Empty (missing
/// dir or no keys) = trust not yet configured.
pub fn load_trusted_publishers(dir: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("pem") {
                if let Ok(pem) = std::fs::read_to_string(&p) {
                    keys.push(pem);
                }
            }
        }
    }
    keys
}

/// Read the manifest signature, accepting a DER signature (openssl/cosign) or
/// base64 text (cosign's on-disk form), auto-detected.
pub fn manifest_signature(sig_path: &Path) -> Vec<u8> {
    let raw = std::fs::read(sig_path).unwrap_or_default();
    if let Ok(s) = std::str::from_utf8(&raw) {
        if let Ok(der) = portcullis_sig::sig_from_base64(s) {
            return der;
        }
    }
    raw
}

/// Verify the app tree's manifest signature. `Ok(())` = a trusted publisher
/// signed it (launch may proceed). `Err(reason)` = refuse. If no publishers are
/// configured, allow with an explicit warning — the gate's absence is auditable,
/// never silent; enforcement turns on the moment the first key is installed.
pub fn verify(tree: &Path, manifest_text: &str) -> Result<(), String> {
    let publishers = load_trusted_publishers(PUBLISHERS_DIR);
    if publishers.is_empty() {
        eprintln!(
            "portcullisd: WARNING manifest trust not configured ({PUBLISHERS_DIR} empty); allowing UNSIGNED {}",
            tree.display()
        );
        return Ok(());
    }
    let sig = manifest_signature(&tree.join("atrium.toml.sig"));
    portcullis_sig::verify_trusted(manifest_text.as_bytes(), &sig, &publishers)
        .map(|()| eprintln!("portcullisd: {} manifest signature verified (trusted publisher)", tree.display()))
        .map_err(|e| format!("manifest not signed by a trusted publisher ({e:?})"))
}
