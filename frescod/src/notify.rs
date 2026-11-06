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
