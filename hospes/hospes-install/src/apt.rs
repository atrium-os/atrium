//! The chain from Google's signing key to one `.deb`: the repository's clear-signed
//! `InRelease`, the `Packages` index it lists by SHA-256, the package the index lists.
use crate::{sha256_hex, Error, GOOGLE_FINGERPRINT, GOOGLE_KEY};
use pgp::composed::{CleartextSignedMessage, Deserializable, SignedPublicKey};
use pgp::types::KeyDetails;

/// One package's entry in a `Packages` index.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
}

fn hex_fpr(f: &[u8]) -> String {
    f.iter().map(|b| format!("{b:02X}")).collect()
}

/// Google's key, checked against the pinned fingerprint and its own self-signatures.
pub fn google_key() -> Result<SignedPublicKey, Error> {
    let (key, _) = SignedPublicKey::from_string(GOOGLE_KEY).map_err(|e| Error::Verify(format!("the shipped key: {e}")))?;
    key.verify_bindings().map_err(|e| Error::Verify(format!("the shipped key's self-signatures: {e}")))?;
    let fpr = hex_fpr(key.fingerprint().as_bytes());
    if fpr != GOOGLE_FINGERPRINT {
        return Err(Error::Verify(format!("the shipped key is {fpr}, not Google's")));
    }
    Ok(key)
}

/// Verify a clear-signed `InRelease` with `key` (its primary key or a signing subkey) and
/// return the signed text.
pub fn verify_inrelease(key: &SignedPublicKey, inrelease: &str) -> Result<String, Error> {
    let (msg, _) = CleartextSignedMessage::from_string(inrelease).map_err(|e| Error::Verify(format!("InRelease: {e}")))?;
    let ok = msg.verify(&key.primary_key).is_ok() || key.public_subkeys.iter().any(|sk| msg.verify(&sk.key).is_ok());
    if !ok {
        return Err(Error::Verify("InRelease is not signed by Google's key".into()));
    }
    Ok(msg.signed_text())
}

/// The SHA-256 a release lists for `path` (e.g. `main/binary-amd64/Packages`).
pub fn release_sha256(release: &str, path: &str) -> Option<(String, u64)> {
    let mut in_sha256 = false;
    for l in release.lines() {
        if !l.starts_with(' ') {
            in_sha256 = l.trim_end() == "SHA256:";
            continue;
        }
        if in_sha256 {
            let mut f = l.split_whitespace();
            let (Some(h), Some(n), Some(p)) = (f.next(), f.next(), f.next()) else { continue };
            if p == path {
                return Some((h.to_ascii_lowercase(), n.parse().ok()?));
            }
        }
    }
    None
}

/// The entry for `name` in a `Packages` index (the highest version if several).
pub fn find_package(packages: &str, name: &str) -> Option<Package> {
    let mut best: Option<Package> = None;
    for stanza in packages.split("\n\n") {
        let field = |k: &str| stanza.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix(": ")).map(str::trim).map(str::to_string));
        if field("Package").as_deref() != Some(name) {
            continue;
        }
        let (Some(version), Some(filename), Some(size), Some(sha256)) = (field("Version"), field("Filename"), field("Size"), field("SHA256")) else { continue };
        let Ok(size) = size.parse() else { continue };
        let p = Package { name: name.into(), version, filename, size, sha256: sha256.to_ascii_lowercase() };
        if best.as_ref().is_none_or(|b| crate::version_cmp(&p.version.replace('-', "."), &b.version.replace('-', ".")).is_gt()) {
            best = Some(p);
        }
    }
    best
}

/// ★ The whole chain: `InRelease` signed by Google, `Packages` as it lists, then the
/// package entry for `name`. The caller fetches `entry.filename` and checks it with
/// [`check_deb`].
pub fn verify_apt(inrelease: &str, packages: &[u8], name: &str) -> Result<Package, Error> {
    verify_apt_for(inrelease, packages, name, crate::platform().0)
}

/// [`verify_apt`] for another machine's index (`amd64` or `arm64`): only to find which
/// package to fetch for it. The machine that installs verifies again with its own.
pub fn verify_apt_for(inrelease: &str, packages: &[u8], name: &str, arch: &str) -> Result<Package, Error> {
    if arch != "amd64" && arch != "arm64" {
        return Err(Error::Verify(format!("no index for {arch}")));
    }
    let key = google_key()?;
    let release = verify_inrelease(&key, inrelease)?;
    let path = format!("main/binary-{arch}/Packages");
    let (want, len) = release_sha256(&release, &path).ok_or_else(|| Error::Verify(format!("the release lists no {path}")))?;
    if packages.len() as u64 != len || sha256_hex(packages) != want {
        return Err(Error::Verify(format!("{path} does not match the signed release")));
    }
    let text = std::str::from_utf8(packages).map_err(|_| Error::Verify("Packages is not text".into()))?;
    find_package(text, name).ok_or_else(|| Error::Package(format!("no {name} in the index")))
}

/// The `.deb` is the one the index names.
pub fn check_deb(entry: &Package, deb: &[u8]) -> Result<(), Error> {
    if deb.len() as u64 != entry.size || sha256_hex(deb) != entry.sha256 {
        return Err(Error::Verify(format!("the package is not {} {} as the index lists it", entry.name, entry.version)));
    }
    Ok(())
}
