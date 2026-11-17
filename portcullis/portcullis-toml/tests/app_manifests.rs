//! The shipped app manifests (Write, Player, Mail) must stay valid, with no
//! validation errors, and say what they open (opening-files.md §5).

use portcullis_toml::{validate, Manifest, OpenMode};

fn load(rel: &str) -> Manifest {
    let path = format!("{}/../../{}", env!("CARGO_MANIFEST_DIR"), rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let m = Manifest::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e:?}"));
    let r = validate(&m);
    assert!(r.errors.is_empty(), "{rel}: {:?}", r.errors);
    m
}

#[test]
fn write_edits_documents_and_reads_no_folders() {
    let m = load("atrium-write/atrium.toml");
    assert_eq!(m.app.id, "org.atrium.write");
    assert_eq!(m.capabilities.removable_files, None);
    assert!(m.opens.iter().all(|o| o.mode == OpenMode::Edit));
    assert!(m.opens.iter().any(|o| o.types.iter().any(|t| t == "application/vnd.oasis.opendocument.text")));
}

#[test]
fn player_only_views() {
    let m = load("atrium-player/atrium.toml");
    assert_eq!(m.app.id, "org.atrium.player");
    assert!(m.opens.iter().all(|o| o.mode == OpenMode::View));
    assert!(m.opens.iter().any(|o| o.types.iter().any(|t| t == "audio/flac")));
}

#[test]
fn mail_takes_mailto_and_fetches_only_through_nuntius() {
    let m = load("mail/atrium-mail/atrium.toml");
    assert_eq!(m.app.id, "org.atrium.mail");
    assert_eq!(m.capabilities.net_fetch, Some(true));
    assert_eq!(m.capabilities.network.as_ref().map(|n| n.mode()), Some(portcullis_toml::NetworkCap::None));
    assert!(m.links.iter().any(|l| l.schemes.iter().any(|s| s == "mailto")));
}
