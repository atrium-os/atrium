//! The shipped Forum manifests must stay valid against the schema, with the
//! caps the desktop's trust model depends on (forum-wm = window-management; the
//! chrome apps = forum-control, never window-management). These live in the
//! forum-* crate dirs; this guards them from schema drift.

use portcullis_toml::Manifest;

fn load(rel: &str) -> Manifest {
    let path = format!("{}/../../{}", env!("CARGO_MANIFEST_DIR"), rel);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {path}: {e}"));
    Manifest::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e:?}"))
}

#[test]
fn forum_wm_is_the_sole_window_manager() {
    let m = load("forum-wm/atrium.toml");
    assert_eq!(m.app.id, "org.atrium.forum-wm");
    assert_eq!(m.app.entry, "bin/forum-wm");
    assert_eq!(m.capabilities.graphics.as_deref(), Some("fresco"));
    assert_eq!(m.capabilities.window_management, Some(true));
    // The shell never holds forum-control — it SERVES it, it doesn't drive itself.
    assert_eq!(m.capabilities.forum_control, None);
}

#[test]
fn chrome_apps_drive_but_never_manage() {
    for (rel, id) in [
        ("forum-bar/atrium.toml", "org.atrium.forum-bar"),
        ("forum-dock/atrium.toml", "org.atrium.forum-dock"),
        ("forum-overview/atrium.toml", "org.atrium.forum-overview"),
    ] {
        let m = load(rel);
        assert_eq!(m.app.id, id, "{rel}");
        assert_eq!(m.capabilities.graphics.as_deref(), Some("fresco"), "{rel}");
        assert_eq!(m.capabilities.forum_control, Some(true), "{rel}");
        // Structural: a chrome app must NOT be able to manage windows directly.
        assert_eq!(m.capabilities.window_management, None, "{rel}");
    }
}
