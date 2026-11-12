//! Hostile downloads to hospes-install (docs/spec/hospes.md §12): a .deb, a Packages index,
//! a Release text and a library are parsed before (or without) any signature saying they are
//! Google's. Nothing may panic; what is accepted must be self-consistent.
#![no_main]
use hospes_install::{apt, check_library, extract_cdm, manifest_version, version_cmp};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 4 {
        0 => {
            if let Ok(x) = extract_cdm(rest) {
                assert!(!x.library.is_empty());
                let _ = check_library(&x.library);
            }
        }
        1 => {
            if let Ok(i) = check_library(rest) {
                assert_eq!(i.sha256.len(), 64);
            }
        }
        2 => {
            let t = String::from_utf8_lossy(rest);
            let _ = apt::find_package(&t, "google-chrome-stable");
            let _ = apt::release_sha256(&t, "main/binary-amd64/Packages");
            let _ = manifest_version(rest);
        }
        _ => {
            let t = String::from_utf8_lossy(rest);
            let (a, b) = t.split_once('\n').unwrap_or((&t, ""));
            let _ = version_cmp(a, b);
            assert_eq!(version_cmp(a, a), std::cmp::Ordering::Equal);
        }
    }
});
