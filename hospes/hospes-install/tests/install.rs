//! The install route (hospes.md §12): Google's signed repository index as fetched on
//! 2026-11-12 (tests/data, public data; the package itself is never committed), tampering
//! caught at each link, a synthetic package's CDM taken out, and the library checks.
use hospes_install::*;

const INRELEASE: &str = include_str!("data/InRelease");

fn packages() -> Vec<u8> {
    let (arch, _) = platform();
    std::fs::read(format!("{}/tests/data/Packages-{arch}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn the_signed_index_names_the_package() {
    let p = apt::verify_apt(INRELEASE, &packages(), "google-chrome-stable").unwrap();
    assert!(p.filename.starts_with("pool/main/g/google-chrome-stable/google-chrome-stable_"), "{p:?}");
    assert_eq!(p.sha256.len(), 64);
    assert!(p.size > 50 << 20);
}

#[test]
fn tampering_breaks_the_chain() {
    // A changed release line: the signature no longer holds.
    let bad = INRELEASE.replacen("Suite: stable", "Suite: stablf", 1);
    assert!(matches!(apt::verify_apt(&bad, &packages(), "google-chrome-stable"), Err(Error::Verify(_))));
    // A changed index: its hash is not the signed one.
    let mut p = packages();
    let at = p.len() / 2;
    p[at] ^= 1;
    assert!(matches!(apt::verify_apt(INRELEASE, &p, "google-chrome-stable"), Err(Error::Verify(_))));
    // A package that is not the one listed.
    let entry = apt::verify_apt(INRELEASE, &packages(), "google-chrome-stable").unwrap();
    assert!(matches!(apt::check_deb(&entry, b"not the package"), Err(Error::Verify(_))));
    // A package the index does not have.
    assert!(matches!(apt::verify_apt(INRELEASE, &packages(), "no-such-package"), Err(Error::Package(_))));
}

#[test]
fn the_shipped_key_is_googles() {
    let k = apt::google_key().unwrap();
    use pgp::types::KeyDetails;
    let f: String = k.fingerprint().as_bytes().iter().map(|b| format!("{b:02X}")).collect();
    assert_eq!(f, GOOGLE_FINGERPRINT);
}

fn test_cdm() -> Vec<u8> {
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    std::fs::read(format!("{}/../test-cdm/bin/{arch}-clearkey.so", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// A .deb shaped like Chrome's, holding `lib` and a manifest.
fn fake_deb(lib: &[u8], manifest: &[u8]) -> Vec<u8> {
    let (_, plat) = platform();
    let mut t = tar::Builder::new(Vec::new());
    let mut add = |path: &str, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        t.append_data(&mut h, path, data).unwrap();
    };
    add("./opt/google/chrome/chrome", b"not taken");
    add(&format!("./opt/google/chrome/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so"), lib);
    add("./opt/google/chrome/WidevineCdm/manifest.json", manifest);
    let tarb = t.into_inner().unwrap();
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut &tarb[..], &mut xz).unwrap();
    let mut out = Vec::new();
    {
        let mut a = ar::Builder::new(&mut out);
        for (name, data) in [("debian-binary", &b"2.0\n"[..]), ("control.tar.xz", &b""[..]), ("data.tar.xz", &xz[..])] {
            a.append(&ar::Header::new(name.as_bytes().to_vec(), data.len() as u64), data).unwrap();
        }
    }
    out
}

#[test]
fn only_the_cdm_and_manifest_come_out_and_install() {
    let lib = test_cdm();
    let manifest = br#"{"name":"WidevineCdm","version":"4.10.3112.0","x-cdm-module-versions":"4"}"#;
    let x = extract_cdm(&fake_deb(&lib, manifest)).unwrap();
    assert_eq!(x.library, lib);
    assert_eq!(x.manifest, manifest);
    let dest = std::env::temp_dir().join(format!("hospes-install-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let dir = install(&dest, &x.library, Some(&x.manifest), false).unwrap();
    assert_eq!(dir, dest.join("4.10.3112.0"));
    assert_eq!(std::fs::read(dest.join("current/libwidevinecdm.so")).unwrap(), lib);
    let older = br#"{"version":"4.10.2000.0"}"#;
    assert!(matches!(install(&dest, &lib, Some(older), false), Err(Error::Downgrade { .. })));
    let newer = br#"{"version":"4.10.3200.1"}"#;
    install(&dest, &lib, Some(newer), false).unwrap();
    assert_eq!(std::fs::read_link(dest.join("current")).unwrap().to_string_lossy(), "4.10.3200.1");
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_library_hospes_cannot_host_is_refused() {
    let info = check_library(&test_cdm()).unwrap();
    assert!(info.imports > 0);
    // Not a CDM: a fixture without the entry points.
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let fx = std::fs::read(format!("{}/../fixtures/bin/{arch}/glibc.so", env!("CARGO_MANIFEST_DIR"))).unwrap();
    assert!(matches!(check_library(&fx), Err(Error::Library(m)) if m.contains("not a CDM")));
    assert!(matches!(check_library(b"\x7fELF nonsense"), Err(Error::Library(_))));
}

/// A real Chrome package, if one is at `HOSPES_INSTALL_REAL_DEB` (never committed): the CDM
/// and manifest come out, and every import of the CDM is provided. Nothing is run.
#[test]
fn a_real_chrome_package_if_given() {
    let Some(p) = std::env::var_os("HOSPES_INSTALL_REAL_DEB") else { return };
    let x = extract_cdm(&std::fs::read(p).unwrap()).unwrap();
    let v = manifest_version(&x.manifest).unwrap();
    let info = check_library(&x.library).unwrap();
    eprintln!("CDM {v}: {} imports, all provided; sha256 {}", info.imports, info.sha256);
}

#[test]
fn manifest_versions() {
    assert_eq!(manifest_version(br#"{"version": "4.10.3112.0"}"#).as_deref(), Some("4.10.3112.0"));
    assert_eq!(manifest_version(br#"{"version":"../x"}"#), None);
    assert!(version_cmp("4.10.3112.0", "4.10.999.9").is_gt());
}
