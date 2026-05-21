//! `insula --version` / `insula version` coverage.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(insula_binary()).args(args).output().expect("insula")
}

const EXPECTED_VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn dash_dash_version_prints_terse_version() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    assert_eq!(trimmed, format!("insula {}", EXPECTED_VERSION),
               "got: {:?}", trimmed);
}

#[test]
fn dash_capital_v_prints_terse_version() {
    let out = run(&["-V"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&format!("insula {}", EXPECTED_VERSION)));
}

#[test]
fn version_subcommand_prints_terse_version() {
    let out = run(&["version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    assert_eq!(trimmed, format!("insula {}", EXPECTED_VERSION));
}

#[test]
fn version_verbose_lists_build_info_and_abi_families() {
    let out = run(&["version", "--verbose"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.contains(EXPECTED_VERSION),
            "expected version string; got: {}", stdout);
    assert!(stdout.contains("build profile"));
    assert!(stdout.contains("target"));
    assert!(stdout.contains("sdk-version"));
    assert!(stdout.contains("libatrium surfaces"));

    // ABI-family enumeration — the breadth claim. Catches
    // regressions where someone adds an ABI and forgets to
    // mention it in the version blurb.
    for surface in [
        "init", "log", "exit", "storage", "keychain",
        "network", "notify", "tabellarius", "window",
        "fill", "path", "texture", "glyph_run", "poll_event",
    ] {
        assert!(stdout.contains(surface),
                "version --verbose should mention '{}'; got: {}",
                surface, stdout);
    }
}
