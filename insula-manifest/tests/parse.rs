//! Manifest parsing tests.
//!
//! v0 scope: parses the `[app]` + `[bundle]` sections,
//! preserves unknown sections in `extra`, roundtrips
//! cleanly.

use insula_manifest::{BundleForm, Error, Manifest};

const EXAMPLE_NATIVE: &str = r#"
[app]
name = "com.example.weather"
version = "1.2.3"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-freebsd"]
entry = "bin/weather"
"#;

const EXAMPLE_WASM: &str = r#"
[app]
name = "com.example.app"
version = "0.1.0"
sdk-version = ">=1.0"

[bundle]
form = "wasm"
entry = "main.wasm"
"#;

const EXAMPLE_WITH_EXTRA_SECTIONS: &str = r#"
[app]
name = "com.example.weather"
version = "1.2.3"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-freebsd"]
entry = "bin/weather"

# Sections not yet typed in this v0 — preserved in `extra`.

[render]
fresco = true

[network]
hosts = [
  { name = "api.weather.example.com", port = 443, proto = "tcp" },
]

[storage]
namespace = "com.example.weather"
quota = "100MB"
"#;

#[test]
fn parses_minimal_native_manifest() {
    let m = Manifest::parse(EXAMPLE_NATIVE)
        .expect("minimal native manifest should parse");

    assert_eq!(m.app.name, "com.example.weather");
    assert_eq!(m.app.version, "1.2.3");
    assert_eq!(m.app.sdk_version, "1.x");

    assert_eq!(m.bundle.form, BundleForm::Native);
    assert_eq!(m.bundle.arches, vec!["aarch64-freebsd"]);
    assert_eq!(m.bundle.entry, "bin/weather");

    // No other sections in this minimal example.
    assert!(m.extra.is_empty());
}

#[test]
fn parses_wasm_manifest_without_arches() {
    let m = Manifest::parse(EXAMPLE_WASM)
        .expect("WASM manifest should parse without arches");

    assert_eq!(m.bundle.form, BundleForm::Wasm);
    assert!(m.bundle.arches.is_empty());
    assert_eq!(m.bundle.entry, "main.wasm");
}

#[test]
fn permissive_mode_preserves_unknown_sections() {
    let m = Manifest::parse(EXAMPLE_WITH_EXTRA_SECTIONS)
        .expect("manifest with not-yet-typed sections should parse");

    // The known sections still parse.
    assert_eq!(m.app.name, "com.example.weather");
    assert_eq!(m.bundle.form, BundleForm::Native);

    // The unknown sections are preserved verbatim.
    assert!(m.extra.contains_key("render"));
    assert!(m.extra.contains_key("network"));
    assert!(m.extra.contains_key("storage"));
    assert_eq!(m.extra.len(), 3);
}

#[test]
fn strict_mode_rejects_unknown_sections() {
    let result = Manifest::parse_strict(EXAMPLE_WITH_EXTRA_SECTIONS);
    match result {
        Err(Error::UnknownSections(sections)) => {
            assert!(sections.contains(&"render".to_string()));
            assert!(sections.contains(&"network".to_string()));
            assert!(sections.contains(&"storage".to_string()));
        }
        other => panic!("expected UnknownSections error, got {:?}", other),
    }
}

#[test]
fn roundtrip_native_manifest() {
    let original = Manifest::parse(EXAMPLE_NATIVE).unwrap();
    let serialized = original.serialize().unwrap();
    let reparsed = Manifest::parse(&serialized)
        .expect("serialized output should reparse");

    assert_eq!(reparsed.app.name, original.app.name);
    assert_eq!(reparsed.app.version, original.app.version);
    assert_eq!(reparsed.app.sdk_version, original.app.sdk_version);
    assert_eq!(reparsed.bundle.form, original.bundle.form);
    assert_eq!(reparsed.bundle.arches, original.bundle.arches);
    assert_eq!(reparsed.bundle.entry, original.bundle.entry);
}

#[test]
fn missing_required_app_section_fails() {
    let bad = r#"
[bundle]
form = "native"
arches = ["aarch64-freebsd"]
entry = "bin/x"
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn missing_required_bundle_section_fails() {
    let bad = r#"
[app]
name = "com.example.x"
version = "1.0.0"
sdk-version = "1.x"
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn unknown_bundle_form_fails() {
    let bad = r#"
[app]
name = "com.example.x"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "totally-made-up"
entry = "bin/x"
"#;
    assert!(Manifest::parse(bad).is_err());
}
