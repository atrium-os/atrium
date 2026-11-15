//! Protected .docx and .odt, saved here and opened through the converter
//! process, as the app does.

use std::path::PathBuf;
use std::process::Command;

use atrium_write::doc::Doc;
use atrium_write::files::{encode, format_of, load_with, Format};
use officina_convert::launch::ConvertError;

fn converter() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../officina-convert");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--release", "--bin", "officina-convert"]).current_dir(&dir).status().unwrap();
    assert!(st.success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| dir.join("target"));
    target.join("release/officina-convert")
}

#[test]
fn protected_files_open_only_with_their_password() {
    std::env::set_var("OFFICINA_CONVERT", converter());
    let mut d = Doc::from_text("plan.txt", b"The plan\nStep one\n").unwrap();
    d.set_password(Some("s3cret")).unwrap();
    for (name, f) in [("plan.docx", Format::Docx), ("plan.odt", Format::Odt), ("plan.owrite", Format::Native)] {
        let file = encode(&d, f);
        assert!(!file.windows(8).any(|w| w == b"Step one"), "{name} holds the text in the clear");
        assert_eq!(load_with(name, &file, None).err(), Some(ConvertError::NeedsPassword), "{name}");
        assert_eq!(load_with(name, &file, Some("S3cret")).err(), Some(ConvertError::WrongPassword), "{name}");
        let back = load_with(name, &file, Some("s3cret")).unwrap();
        assert_eq!(back.plain_text(), d.plain_text(), "{name}");
        // Saved again, it stays protected with the same password.
        assert_eq!(back.protection.as_ref().map(|p| p.password.as_str()), Some("s3cret"));
    }
    // Unprotected, the same document opens without one.
    d.set_password(None).unwrap();
    let file = encode(&d, Format::Docx);
    assert!(load_with("plan.docx", &file, None).unwrap().protection.is_none());
}

#[test]
fn doc_files_open_through_the_converter() {
    std::env::set_var("OFFICINA_CONVERT", converter());
    let plain = officina_convert::doc::sample::document(None);
    let d = load_with("old.doc", &plain, None).unwrap();
    assert!(d.plain_text().contains("Hello from Word 97"));
    assert!(d.protection.is_none());
    let locked = officina_convert::doc::sample::document(Some("pa55"));
    assert!(!locked.windows(6).any(|w| w == b"Second"), "the protected file holds the text in the clear");
    assert_eq!(load_with("old.doc", &locked, None).err(), Some(ConvertError::NeedsPassword));
    assert_eq!(load_with("old.doc", &locked, Some("pass")).err(), Some(ConvertError::WrongPassword));
    let back = load_with("old.doc", &locked, Some("pa55")).unwrap();
    assert!(back.plain_text().contains("Second paragraph"));
    assert!(back.protection.is_some());
    assert_eq!(format_of("Old.DOC"), Format::Doc);
    assert_eq!(format_of("letter.docx"), Format::Docx);
}
