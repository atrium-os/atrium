//! The notification layer (docs/spec/tabula.md §5): who may open a
//! notification surface. The compositor keeps such surfaces above every
//! other window and inside `fresco_protocol::notification_band`.

use std::sync::OnceLock;

/// The uids granted the notification role: FRESCOD_NOTIFY_UIDS (comma
/// separated), else none. Forum's shelf runs under its app's own uid, so
/// the session lists it here, as praecod's `PRAECO_SUBSCRIBERS` does.
pub fn uids() -> &'static [u32] {
    static U: OnceLock<Vec<u32>> = OnceLock::new();
    U.get_or_init(|| {
        let v: Vec<u32> = std::env::var("FRESCOD_NOTIFY_UIDS")
            .map(|s| s.split(',').filter_map(|u| u.trim().parse().ok()).collect())
            .unwrap_or_default();
        eprintln!("frescod: notification layer for uids {v:?}");
        v
    })
}

pub fn may(uid: u32) -> bool {
    uids().contains(&uid)
}

/// The shell's chrome apps: the only app ids the chrome role can go to by
/// registry identity (the Forum bar and dock).
const CHROME_APPS: [&str; 2] = ["org.atrium.forum-bar", "org.atrium.forum-dock"];

/// The shell-chrome role (the Forum bar, which stacks above apps and their
/// pop-ups: fresco-popups.md §2) goes to no app by default. Granted to:
/// - the uids in FRESCOD_CHROME_UIDS (comma separated; a session that runs
///   the bar under a fixed uid, and tests), or
/// - a launched app whose registry binding (FRESCOD_APP_REGISTRY) is one of
///   [`CHROME_APPS`] signed by a publisher in FRESCOD_CHROME_PUBLISHERS
///   (comma-separated publisher ids, hex: Atrium's own key). An unsigned
///   install named like the bar gets nothing.
struct Chrome {
    uids: Vec<u32>,
    publishers: Vec<[u8; 32]>,
    registry: String,
}

fn chrome() -> &'static Chrome {
    static C: OnceLock<Chrome> = OnceLock::new();
    C.get_or_init(|| {
        let list = |k: &str| std::env::var(k).unwrap_or_default();
        let uids: Vec<u32> = list("FRESCOD_CHROME_UIDS").split(',').filter_map(|u| u.trim().parse().ok()).collect();
        let publishers: Vec<[u8; 32]> = list("FRESCOD_CHROME_PUBLISHERS")
            .split(',')
            .filter_map(|h| {
                let h = h.trim();
                (h.len() == 64).then_some(())?;
                let mut out = [0u8; 32];
                for (i, b) in out.iter_mut().enumerate() {
                    *b = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
                }
                Some(out)
            })
            .collect();
        let registry = std::env::var("FRESCOD_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
        eprintln!("frescod: chrome (bar) role for uids {uids:?} and {} publisher(s)", publishers.len());
        Chrome { uids, publishers, registry }
    })
}

/// Whether peer `uid` may open shell chrome.
pub fn chrome_may(uid: u32) -> bool {
    let c = chrome();
    if c.uids.contains(&uid) {
        return true;
    }
    if c.publishers.is_empty() {
        return false;
    }
    let reg = portcullis_peer::AppRegistry::load(&c.registry).unwrap_or_default();
    let app = reg.resolve(uid).map(|(_, app)| app);
    let signed = reg.publisher(uid).flatten();
    matches!((app, signed), (Some(a), Some(p)) if CHROME_APPS.contains(&a) && c.publishers.contains(&p))
}
