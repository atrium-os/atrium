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
    assert_eq!(m.capabilities.embeds.as_deref(), Some(&["picker".to_string()][..]));
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

#[test]
fn sheets_and_slides_convert_in_a_worker_and_read_no_folders() {
    for (rel, id, ty) in [
        ("atrium-sheets/atrium.toml", "org.atrium.sheets", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        ("atrium-slides/atrium.toml", "org.atrium.slides", "application/vnd.oasis.opendocument.presentation"),
    ] {
        let m = load(rel);
        assert_eq!(m.app.id, id);
        assert_eq!(m.capabilities.removable_files, None);
        assert_eq!(m.capabilities.network.as_ref().map(|n| n.mode()), Some(portcullis_toml::NetworkCap::None));
        assert!(m.workers.iter().any(|w| w.name == "convert"), "{rel}");
        assert!(m.opens.iter().any(|o| o.mode == OpenMode::Edit && o.types.iter().any(|t| t == ty)), "{rel}");
        assert_eq!(m.capabilities.embeds.as_ref().and_then(|e| e.first()).map(String::as_str), Some("picker"), "{rel}");
    }
    // Only Slides plays video, in Limen's media player.
    assert_eq!(load("atrium-slides/atrium.toml").capabilities.embeds.as_deref().map(|e| e.len()), Some(2));
    assert_eq!(load("atrium-sheets/atrium.toml").capabilities.embeds.as_deref().map(|e| e.len()), Some(1));
    // Sheets reads .xls but doesn't write it.
    let m = load("atrium-sheets/atrium.toml");
    assert!(m.opens.iter().any(|o| o.mode == OpenMode::View && o.types.iter().any(|t| t == "application/vnd.ms-excel")));
}
