//! Bundle reading + validation tests.

use insula_bundle::{Error, InsulaBundle};
use std::fs;
use std::path::Path;

/// Build a minimal valid bundle on disk in `root`.
fn build_minimal_bundle(root: &Path) {
    fs::write(
        root.join("manifest.toml"),
        r#"
[app]
name = "com.example.hello"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/hello"
"#,
    )
    .unwrap();

    fs::create_dir(root.join("bin")).unwrap();
    fs::write(root.join("bin/hello"), b"#!/bin/sh\necho hi\n").unwrap();
    // Make it executable so it looks like a real binary.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(root.join("bin/hello"))
            .unwrap()
            .permissions();
        p.set_mode(0o755);
        fs::set_permissions(root.join("bin/hello"), p).unwrap();
    }
}

#[test]
fn reads_minimal_valid_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    build_minimal_bundle(tmp.path());

    let bundle = InsulaBundle::read(tmp.path())
        .expect("minimal valid bundle should read");

    assert_eq!(bundle.app_id(), "com.example.hello");
    assert_eq!(bundle.manifest.bundle.entry, "bin/hello");
    assert_eq!(bundle.binary_path(), tmp.path().join("bin/hello"));
    assert!(bundle.binary_path().exists());
}

#[test]
fn fails_when_root_is_not_directory() {
    // Path that doesn't exist.
    let result = InsulaBundle::read("/this/path/does/not/exist/atrium-test");
    assert!(matches!(result, Err(Error::BundleRootNotADir(_))));
}

#[test]
fn fails_when_manifest_missing() {
    let tmp = tempfile::tempdir().unwrap();
    // Empty directory — no manifest.toml.
    let result = InsulaBundle::read(tmp.path());
    assert!(matches!(result, Err(Error::ManifestRead { .. })),
            "expected ManifestRead, got {:?}", result);
}

#[test]
fn fails_when_manifest_is_malformed() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("manifest.toml"), "this is not TOML }}}").unwrap();

    let result = InsulaBundle::read(tmp.path());
    assert!(matches!(result, Err(Error::ManifestParse(_))));
}

#[test]
fn fails_when_declared_binary_missing() {
    let tmp = tempfile::tempdir().unwrap();
    // Manifest declares bin/missing but the file isn't there.
    fs::write(
        tmp.path().join("manifest.toml"),
        r#"
[app]
name = "com.example.x"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/missing-binary"
"#,
    )
    .unwrap();

    let result = InsulaBundle::read(tmp.path());
    match result {
        Err(Error::BinaryMissing { entry, .. }) => {
            assert_eq!(entry, "bin/missing-binary");
        }
        other => panic!("expected BinaryMissing, got {:?}", other),
    }
}
