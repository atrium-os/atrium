//! Golden-file tests for the schema + validator.
//!
//! `tests/accept/*.toml` — must parse + validate without errors.
//!   Warnings are fine; the file may include a `# EXPECT_WARNING: <substr>`
//!   comment that we additionally check.
//!
//! `tests/reject/*.toml` — must produce at least one error containing
//!   the substring named by the `# EXPECT_ERROR: <substr>` comment.
//!   The error may come from either the parse step (TOML syntax /
//!   missing required fields / unknown enum variant) OR the validate
//!   step (semantic rules from spec §3.3).

use std::fs;
use std::path::Path;

fn extract_marker(text: &str, marker: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# ").and_then(|s| s.strip_prefix(marker)) {
            return Some(rest.trim_start_matches(':').trim().to_string());
        }
    }
    None
}

fn run_one_accept(path: &Path) {
    let text = fs::read_to_string(path).unwrap();
    let m = match portcullis_toml::Manifest::from_str(&text) {
        Ok(m) => m,
        Err(e) => panic!("{}: parse failed: {e}", path.display()),
    };
    let report = portcullis_toml::validate(&m);
    if !report.errors.is_empty() {
        panic!("{}: unexpected errors: {:#?}", path.display(), report.errors);
    }
    if let Some(want) = extract_marker(&text, "EXPECT_WARNING") {
        let found = report.warnings.iter().any(|w| w.contains(&want));
        assert!(found,
            "{}: expected warning containing {:?}, got {:#?}",
            path.display(), want, report.warnings);
    }
}

fn run_one_reject(path: &Path) {
    let text = fs::read_to_string(path).unwrap();
    let want = extract_marker(&text, "EXPECT_ERROR")
        .unwrap_or_else(|| panic!("{}: missing # EXPECT_ERROR marker", path.display()));

    /* Parse-step error (TOML syntax / missing field / unknown enum). */
    match portcullis_toml::Manifest::from_str(&text) {
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains(&want),
                "{}: parse error did not contain {:?}: {msg}",
                path.display(), want);
            return;
        }
        Ok(m) => {
            let report = portcullis_toml::validate(&m);
            let found = report.errors.iter().any(|e| e.contains(&want));
            assert!(found,
                "{}: expected error containing {:?}, got {:#?}",
                path.display(), want, report.errors);
        }
    }
}

#[test]
fn accept_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/accept");
    let mut count = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) != Some("toml") { continue; }
        eprintln!("accept: {}", p.display());
        run_one_accept(&p);
        count += 1;
    }
    assert!(count > 0, "no accept fixtures found in {}", dir.display());
}

#[test]
fn reject_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reject");
    let mut count = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) != Some("toml") { continue; }
        eprintln!("reject: {}", p.display());
        run_one_reject(&p);
        count += 1;
    }
    assert!(count > 0, "no reject fixtures found in {}", dir.display());
}
