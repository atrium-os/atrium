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

/// The session components that draw shell chrome: the Forum bar and dock
/// (their session.d `app_id`).
const CHROME_COMPONENTS: [&str; 2] = ["org.atrium.forum-bar", "org.atrium.forum-dock"];

/// The shell-chrome role (the Forum bar, which stacks above apps and their
/// pop-ups: fresco-popups.md §2) goes to no app. Granted to:
/// - the bar or dock started as a declared session component: portcullisd
///   records those in the launch registry (FRESCOD_APP_REGISTRY) marked
///   `session`, which an app installed under the same id never is;
/// - the uids in FRESCOD_CHROME_UIDS (comma separated; tests).
struct Chrome {
    uids: Vec<u32>,
    registry: String,
}

fn chrome() -> &'static Chrome {
    static C: OnceLock<Chrome> = OnceLock::new();
    C.get_or_init(|| {
        let uids: Vec<u32> = std::env::var("FRESCOD_CHROME_UIDS").unwrap_or_default()
            .split(',').filter_map(|u| u.trim().parse().ok()).collect();
        let registry = std::env::var("FRESCOD_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
        if !uids.is_empty() {
            eprintln!("frescod: chrome (bar) role also for uids {uids:?}");
        }
        Chrome { uids, registry }
    })
}

/// Whether peer `uid` may open shell chrome.
pub fn chrome_may(uid: u32) -> bool {
    let c = chrome();
    if c.uids.contains(&uid) {
        return true;
    }
    let reg = portcullis_peer::AppRegistry::load(&c.registry).unwrap_or_default();
    reg.is_session(uid) && reg.resolve(uid).is_some_and(|(_, id)| CHROME_COMPONENTS.contains(&id))
}
