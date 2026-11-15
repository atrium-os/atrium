//! Protected .pptx, .odp and .oslides, saved here and opened as the app
//! does (the foreign ones through the converter process).

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use atrium_slides::doc::Doc;
use atrium_slides::files::{encode, load_file_with, load_with, Format};
use officina_convert::launch::ConvertError;

fn converter() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../officina-convert");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--release", "--bin", "officina-convert"]).current_dir(&dir).status().unwrap();
    assert!(st.success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| dir.join("target"));
    target.join("release/officina-convert")
}

#[test]
fn protected_decks_open_only_with_their_password() {
    std::env::set_var("OFFICINA_CONVERT", converter());
    let mut d = Doc::new("Talk");
    d.set_password(Some("deck pw")).unwrap();
    let dir = std::env::temp_dir().join(format!("atrium-slides-protected-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, f) in [("t.pptx", Format::Pptx), ("t.odp", Format::Odp), ("t.oslides", Format::Native)] {
        let bytes = encode(&mut d, f).unwrap();
        let path = dir.join(name);
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        let open = |pw: Option<&str>| load_file_with(name, &mut std::fs::File::open(&path).unwrap(), 1 << 30, pw);
        assert_eq!(open(None).err(), Some(ConvertError::NeedsPassword), "{name}");
        assert_eq!(open(Some("deck")).err(), Some(ConvertError::WrongPassword), "{name}");
        let back = open(Some("deck pw")).unwrap();
        assert_eq!(back.deck.slide_count(), d.deck.slide_count(), "{name}");
        assert_eq!(back.protection.as_ref().map(|p| p.password.as_str()), Some("deck pw"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ppt_files_open_through_the_converter() {
    std::env::set_var("OFFICINA_CONVERT", converter());
    let text = |d: &mut Doc| {
        let s = d.deck.slide(0).unwrap();
        let mut t: String = s.shapes.iter().filter_map(|x| x.text()).map(|t| t.text()).collect();
        t.push_str(&s.notes.as_ref().map(|n| n.text()).unwrap_or_default());
        t
    };
    let plain = officina_convert::ppt::sample::deck(None);
    let mut d = load_with("old.ppt", plain, None).unwrap();
    let t = text(&mut d);
    assert!(t.contains("Hello from PowerPoint 97") && t.contains("Speaker notes"), "{t:?}");
    assert!(d.protection.is_none());
    let locked = officina_convert::ppt::sample::deck(Some("pa55"));
    assert!(!locked.windows(6).any(|w| w == b"Second"), "the protected file holds the text in the clear");
    assert_eq!(load_with("old.ppt", locked.clone(), None).err(), Some(ConvertError::NeedsPassword));
    assert_eq!(load_with("old.ppt", locked.clone(), Some("pass")).err(), Some(ConvertError::WrongPassword));
    let mut back = load_with("old.ppt", locked, Some("pa55")).unwrap();
    assert!(text(&mut back).contains("Second shape"));
    assert!(back.protection.is_some());
    assert_eq!(atrium_slides::files::format_of("Old.PPS"), Format::Ppt);
}
