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

/// The uids granted the shell-chrome role (the Forum bar, which stacks
/// above apps and their pop-ups: fresco-popups.md §2): FRESCOD_CHROME_UIDS
/// (comma separated). Unset: anyone, with a warning (development).
pub fn chrome_uids() -> Option<&'static [u32]> {
    static U: OnceLock<Option<Vec<u32>>> = OnceLock::new();
    U.get_or_init(|| match std::env::var("FRESCOD_CHROME_UIDS") {
        Ok(s) => {
            let v: Vec<u32> = s.split(',').filter_map(|u| u.trim().parse().ok()).collect();
            eprintln!("frescod: chrome (bar) role for uids {v:?}");
            Some(v)
        }
        Err(_) => {
            eprintln!("frescod: WARNING: FRESCOD_CHROME_UIDS unset: any app may claim the bar's chrome role");
            None
        }
    })
    .as_deref()
}

pub fn chrome_may(uid: u32) -> bool {
    chrome_uids().is_none_or(|u| u.contains(&uid))
}
