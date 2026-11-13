//! Every strong import of the libraries Hospes hosts resolves in the shim, exactly by
//! (name, version) (spec §6.4, §11.4). The lists are `readelf --dyn-syms` extracts kept in
//! probe/h0-data/ (names and versions only; the libraries themselves are never committed).
//! Weak imports may stay unresolved (the loader binds them to 0).
use std::collections::HashSet;

/// Imports still to come, by step. Each must really be missing (so the list only shrinks).
const PENDING: &[(&str, &str)] = &[];

fn lists() -> &'static [&'static str] {
    if cfg!(target_arch = "x86_64") {
        &["wv-x64.so.imports", "libopenh264-2.6.0-linux64.8.so.imports"]
    } else {
        &["wv-arm64.so.imports", "libopenh264-2.6.0-linux-arm64.8.so.imports"]
    }
}

#[test]
fn every_strong_import_resolves() {
    let have: HashSet<(&str, Option<&str>)> = hospes_glibc::provided().collect();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../probe/h0-data/");
    let mut missing = Vec::new();
    let mut pending_seen = HashSet::new();
    for list in lists() {
        let text: &'static str = std::fs::read_to_string(format!("{dir}{list}")).unwrap().leak();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            let (Some(sym), Some(bind)) = (it.next(), it.next()) else { continue };
            if bind == "WEAK" {
                continue;
            }
            let (name, ver) = match sym.split_once('@') {
                Some((n, v)) => (n, Some(v)),
                None => (sym, None),
            };
            if have.contains(&(name, ver)) {
                assert!(!PENDING.iter().any(|p| p.0 == name), "{name} is provided now: drop it from PENDING");
                continue;
            }
            if PENDING.iter().any(|p| p.0 == name) {
                pending_seen.insert(name);
                continue;
            }
            missing.push(format!("{list}: {sym}"));
        }
    }
    assert!(missing.is_empty(), "unresolved imports:\n{}", missing.join("\n"));
    for (p, _) in PENDING {
        assert!(pending_seen.contains(p), "{p} is in PENDING but no list imports it");
    }
}

/// Sigillum S5 (sigillum.md §16.5), x86_64 only (the vendor ships no other): the InnaIT
/// PKCS#11 module and the real libstdc++ 6.0.28 (Ubuntu focal), loaded as one set. The
/// module's C++ imports (`GLIBCXX_*`, `CXXABI_*`) bind to libstdc++; libgcc_s is not loaded
/// (the shim's unwinder stands in), so every other import of either resolves in the shim.
/// An unversioned import binds to the baseline row, as the resolver does. The PC/SC, udev
/// and curl imports come from the sigillum-p11 host (P2), not from the shim.
#[cfg(target_arch = "x86_64")]
#[test]
fn the_innait_set_resolves() {
    let have: HashSet<(&str, Option<&str>)> = hospes_glibc::provided().collect();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../probe/h0-data/");
    let mut missing = Vec::new();
    for (list, from_libstdcxx) in [("innait-dsc-4.0.7.1-x64.so.imports", true), ("libstdc++.so.6.0.28-focal-x64.so.imports", false)] {
        let text = std::fs::read_to_string(format!("{dir}{list}")).unwrap();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            let (Some(sym), Some(bind)) = (it.next(), it.next()) else { continue };
            let (name, ver) = match sym.split_once('@') {
                Some((n, v)) => (n, Some(v)),
                None => (sym, None),
            };
            if from_libstdcxx && ver.is_some_and(|v| v.starts_with("GLIBCXX_") || v.starts_with("CXXABI_")) {
                continue;
            }
            if ["SCard", "udev_", "curl_"].iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let found = have.contains(&(name, ver)) || (ver.is_none() && have.iter().any(|h| h.0 == name && h.1.is_some_and(|v| v == "GLIBC_2.2.5")));
            if !found && bind != "WEAK" {
                missing.push(format!("{list}: {sym}"));
            }
        }
    }
    assert!(missing.is_empty(), "unresolved imports:\n{}", missing.join("\n"));
}

#[test]
fn no_duplicate_rows() {
    let mut seen = HashSet::new();
    for e in hospes_glibc::provided() {
        assert!(seen.insert(e), "duplicate table row {e:?}");
    }
}
