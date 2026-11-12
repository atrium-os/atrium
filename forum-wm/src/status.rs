//! Status items (docs/spec/forum-status-items.md): the apps' bar items.
//!
//! Apps with `status-item` connect to the status socket and `Set` an item
//! (a glyph, a tooltip, a menu); the bar lists them over forum-ctl and draws
//! them, and reports the user's picks back here, which go to the app's
//! connection. One item per app (named by the launch registry, never by the
//! app); a connection closing takes its item away.

use std::sync::mpsc::Sender;
use std::sync::Mutex;

use forum_ctl::{FromStatus, StatusEntry, StatusItem, ToStatus, STATUS_MAX_ENTRIES, STATUS_MAX_LABEL};

struct Slot {
    item: StatusItem,
    /// The connection that set it, and where its events go.
    conn: u64,
    tx: Sender<FromStatus>,
}

struct Board {
    slots: Vec<Slot>,
    generation: u64,
}

static BOARD: Mutex<Board> = Mutex::new(Board { slots: Vec::new(), generation: 0 });

/// Connection `conn` of app `app_id` (its events to `tx`) sent `m`.
pub fn apply(app_id: &str, conn: u64, tx: &Sender<FromStatus>, m: ToStatus) {
    let mut b = BOARD.lock().unwrap();
    b.generation += 1;
    let generation = b.generation;
    match m {
        ToStatus::Set { glyph, tooltip, menu } => {
            let item = StatusItem {
                app_id: app_id.to_string(),
                glyph,
                tooltip: tooltip.chars().take(STATUS_MAX_LABEL).collect(),
                menu: menu.into_iter().take(STATUS_MAX_ENTRIES).map(StatusEntry::bounded).collect(),
                generation,
            };
            let slot = Slot { item, conn, tx: tx.clone() };
            match b.slots.iter_mut().find(|s| s.item.app_id == app_id) {
                Some(s) => *s = slot,
                None => b.slots.push(slot),
            }
        }
        ToStatus::Remove => b.slots.retain(|s| s.item.app_id != app_id),
    }
}

/// Connection `conn` closed: its item goes (not one a newer connection of
/// the same app set since).
pub fn closed(conn: u64) {
    BOARD.lock().unwrap().slots.retain(|s| s.conn != conn);
}

/// The items, in the order they were added.
pub fn list() -> Vec<StatusItem> {
    BOARD.lock().unwrap().slots.iter().map(|s| s.item.clone()).collect()
}

/// The user picked entry `index` of `app_id`'s menu (an action, check or
/// radio that is enabled).
pub fn pick(app_id: &str, index: u32) -> Result<(), String> {
    let b = BOARD.lock().unwrap();
    let s = b.slots.iter().find(|s| s.item.app_id == app_id).ok_or("no such status item")?;
    match s.item.menu.get(index as usize) {
        Some(StatusEntry::Action { enabled: true, .. } | StatusEntry::Check { enabled: true, .. } | StatusEntry::Radio { enabled: true, .. }) => {
            s.tx.send(FromStatus::Picked { index }).map_err(|_| "the app is gone".into())
        }
        _ => Err("not a pickable entry".into()),
    }
}

/// The user moved slider entry `index` of `app_id`'s menu to `value`.
pub fn slide(app_id: &str, index: u32, value: f32) -> Result<(), String> {
    let b = BOARD.lock().unwrap();
    let s = b.slots.iter().find(|s| s.item.app_id == app_id).ok_or("no such status item")?;
    match s.item.menu.get(index as usize) {
        Some(StatusEntry::Slider { .. }) if value.is_finite() => {
            s.tx.send(FromStatus::Slid { index, value: value.clamp(0.0, 1.0) }).map_err(|_| "the app is gone".into())
        }
        _ => Err("not a slider".into()),
    }
}

#[cfg(test)]
pub(crate) fn reset() {
    let mut b = BOARD.lock().unwrap();
    b.slots.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn items_are_set_picked_and_go_with_their_connection() {
        reset();
        let (tx, rx) = channel();
        let menu = vec![
            StatusEntry::Action { label: "Connect".into(), enabled: true },
            StatusEntry::Check { label: "x".repeat(200), on: true, enabled: false },
            StatusEntry::Slider { label: "Level".into(), value: 7.0 },
            StatusEntry::Separator,
        ];
        apply("org.test.vpn", 1, &tx, ToStatus::Set { glyph: '\u{e000}', tooltip: "VPN".into(), menu });
        let items = list();
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0].menu[1], StatusEntry::Check { label, .. } if label.chars().count() == STATUS_MAX_LABEL));
        assert!(matches!(items[0].menu[2], StatusEntry::Slider { value, .. } if value == 1.0));
        pick("org.test.vpn", 0).unwrap();
        assert_eq!(rx.try_recv(), Ok(FromStatus::Picked { index: 0 }));
        assert!(pick("org.test.vpn", 1).is_err(), "disabled");
        assert!(pick("org.test.vpn", 3).is_err(), "separator");
        assert!(pick("org.other", 0).is_err());
        slide("org.test.vpn", 2, 0.25).unwrap();
        assert_eq!(rx.try_recv(), Ok(FromStatus::Slid { index: 2, value: 0.25 }));
        assert!(slide("org.test.vpn", 0, 0.5).is_err());
        // A newer connection of the same app replaces it; the old one closing
        // leaves it.
        let (tx2, _rx2) = channel();
        apply("org.test.vpn", 2, &tx2, ToStatus::Set { glyph: 'x', tooltip: String::new(), menu: Vec::new() });
        closed(1);
        assert_eq!(list().len(), 1);
        closed(2);
        assert!(list().is_empty());
    }
}
