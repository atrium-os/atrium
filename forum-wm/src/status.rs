//! Status items (docs/spec/forum-status-items.md): the apps' bar items.
//!
//! Apps with `status-item` connect to the status socket and `Set` an item
//! (a glyph, a tooltip, a menu); the bar lists them over forum-ctl and draws
//! them, and reports the user's picks back here, which go to the app's
//! connection. One item per app (named by the launch registry, never by the
//! app); a connection closing takes its item away.

use std::sync::mpsc::Sender;
use std::sync::Mutex;

use forum_ctl::{FromStatus, StatusContent, StatusEntry, StatusItem, StatusValue, ToStatus, STATUS_MAX_LABEL};

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
        ToStatus::Set { glyph, svg, tint, tooltip, content } => {
            let item = StatusItem {
                app_id: app_id.to_string(),
                glyph,
                svg: forum_ctl::status_svg(svg),
                tooltip: tooltip.chars().take(STATUS_MAX_LABEL).collect(),
                tint,
                content: content.bounded(),
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

fn menu(item: &StatusItem) -> &[StatusEntry] {
    match &item.content {
        StatusContent::Menu(m) => m,
        StatusContent::Panel(_) | StatusContent::Drawn { .. } => &[],
    }
}

/// The user picked entry `index` of `app_id`'s menu (an action, check or
/// radio that is enabled).
pub fn pick(app_id: &str, index: u32) -> Result<(), String> {
    let b = BOARD.lock().unwrap();
    let s = b.slots.iter().find(|s| s.item.app_id == app_id).ok_or("no such status item")?;
    match menu(&s.item).get(index as usize) {
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
    match menu(&s.item).get(index as usize) {
        Some(StatusEntry::Slider { .. }) if value.is_finite() => {
            s.tx.send(FromStatus::Slid { index, value: value.clamp(0.0, 1.0) }).map_err(|_| "the app is gone".into())
        }
        _ => Err("not a slider".into()),
    }
}

/// The user worked control `id` of `app_id`'s panel: passed on if that
/// control takes `value` and is enabled.
pub fn input(app_id: &str, id: u32, value: StatusValue) -> Result<(), String> {
    let b = BOARD.lock().unwrap();
    let s = b.slots.iter().find(|s| s.item.app_id == app_id).ok_or("no such status item")?;
    let StatusContent::Panel(p) = &s.item.content else { return Err("not a panel".into()) };
    let value = p.accepts(id, value).ok_or("not a control that takes this")?;
    s.tx.send(FromStatus::Changed { id, value }).map_err(|_| "the app is gone".into())
}

/// The user opened `app_id`'s own panel: the app claims the pop-up the
/// bar's `token` grants.
pub fn open_panel(app_id: &str, token: [u8; 16]) -> Result<(), String> {
    let b = BOARD.lock().unwrap();
    let s = b.slots.iter().find(|s| s.item.app_id == app_id).ok_or("no such status item")?;
    let StatusContent::Drawn { width, height } = s.item.content else { return Err("not a drawn panel".into()) };
    s.tx.send(FromStatus::OpenPanel { token, width, height }).map_err(|_| "the app is gone".into())
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

    /// The board is global: one test at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn items_are_set_picked_and_go_with_their_connection() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let (tx, rx) = channel();
        let menu = vec![
            StatusEntry::Action { label: "Connect".into(), enabled: true },
            StatusEntry::Check { label: "x".repeat(200), on: true, enabled: false },
            StatusEntry::Slider { label: "Level".into(), value: 7.0 },
            StatusEntry::Separator,
        ];
        apply("org.test.vpn", 1, &tx, ToStatus::Set { tint: None, svg: Some("<path d=\"M4 4L20 20\"/>".into()), glyph: '\u{e000}', tooltip: "VPN".into(), content: StatusContent::Menu(menu) });
        let items = list();
        assert_eq!(items.len(), 1);
        let m = super::menu(&items[0]);
        assert_eq!(items[0].svg.as_deref(), Some("<path d=\"M4 4L20 20\"/>"));
        assert!(matches!(&m[1], StatusEntry::Check { label, .. } if label.chars().count() == STATUS_MAX_LABEL));
        assert!(matches!(m[2], StatusEntry::Slider { value, .. } if value == 1.0));
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
        apply("org.test.vpn", 2, &tx2, ToStatus::Set { tint: None, svg: None, glyph: 'x', tooltip: String::new(), content: StatusContent::Menu(Vec::new()) });
        closed(1);
        assert_eq!(list().len(), 1);
        closed(2);
        assert!(list().is_empty());
    }

    #[test]
    fn panels_are_bounded_and_take_only_what_their_controls_take() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        use forum_ctl::{StatusNode, StatusPanel, StatusText, STATUS_MAX_NODES, STATUS_PANEL_MAX_W};
        let (tx, rx) = channel();
        let deep = StatusNode::Row { children: vec![StatusNode::Column { children: vec![StatusNode::Group {
            title: "g".into(),
            children: vec![StatusNode::Row { children: vec![StatusNode::Separator] }],
        }] }] };
        let mut nodes = vec![
            StatusNode::Heading { text: "Clipboard".into() },
            StatusNode::Text { text: "x".repeat(2000), style: StatusText::Body },
            StatusNode::Toggle { id: 1, label: "Paused".into(), on: false, enabled: true },
            StatusNode::Slider { id: 2, label: "History".into(), value: -3.0, enabled: true },
            StatusNode::Choice { id: 3, options: vec!["a".into(), "b".into()], selected: 9, segmented: true, enabled: true },
            StatusNode::Button { id: 4, label: "Clear".into(), primary: false, enabled: false },
            deep,
            StatusNode::TextField { id: 5, label: "Server".into(), placeholder: "host".into(), value: String::new(), enabled: true },
        ];
        nodes.extend((0..100).map(|_| StatusNode::Separator));
        apply("org.test.clip", 7, &tx, ToStatus::Set { tint: None, svg: Some("<path d=\"M1 1\"/> é".into()), glyph: 'c', tooltip: String::new(), content: StatusContent::Panel(StatusPanel { width: 9999, nodes }) });
        let StatusContent::Panel(p) = list().into_iter().find(|i| i.app_id == "org.test.clip").unwrap().content else { panic!() };
        assert_eq!(p.width, STATUS_PANEL_MAX_W);
        assert_eq!(list().into_iter().find(|i| i.app_id == "org.test.clip").unwrap().svg, None, "not ASCII");
        assert!(matches!(&p.nodes[1], StatusNode::Text { text, .. } if text.chars().count() == forum_ctl::STATUS_MAX_TEXT));
        assert!(matches!(p.nodes[3], StatusNode::Slider { value, .. } if value == 0.0));
        assert!(matches!(p.nodes[4], StatusNode::Choice { selected: 1, .. }));
        // Depth 4: the innermost row goes.
        let StatusNode::Row { children } = &p.nodes[6] else { panic!() };
        let StatusNode::Column { children } = &children[0] else { panic!() };
        assert!(matches!(&children[0], StatusNode::Group { children, .. } if children.is_empty()));
        fn count(n: &[StatusNode]) -> usize {
            n.iter().map(|n| 1 + match n {
                StatusNode::Group { children, .. } | StatusNode::Row { children } | StatusNode::Column { children } => count(children),
                _ => 0,
            }).sum()
        }
        assert!(count(&p.nodes) <= STATUS_MAX_NODES);

        input("org.test.clip", 1, StatusValue::On(true)).unwrap();
        assert_eq!(rx.try_recv(), Ok(FromStatus::Changed { id: 1, value: StatusValue::On(true) }));
        input("org.test.clip", 2, StatusValue::Level(4.0)).unwrap();
        assert_eq!(rx.try_recv(), Ok(FromStatus::Changed { id: 2, value: StatusValue::Level(1.0) }));
        input("org.test.clip", 3, StatusValue::Chosen(1)).unwrap();
        assert!(input("org.test.clip", 3, StatusValue::Chosen(2)).is_err(), "no third option");
        assert!(input("org.test.clip", 4, StatusValue::Pressed).is_err(), "disabled");
        assert!(input("org.test.clip", 1, StatusValue::Pressed).is_err(), "a toggle is not pressed");
        assert!(input("org.test.clip", 99, StatusValue::Pressed).is_err());
        while rx.try_recv().is_ok() {}
        input("org.test.clip", 5, StatusValue::Text("y".repeat(900))).unwrap();
        assert!(matches!(rx.try_recv(), Ok(FromStatus::Changed { id: 5, value: StatusValue::Text(t) }) if t.chars().count() == forum_ctl::STATUS_MAX_TEXT));
        assert!(input("org.test.clip", 1, StatusValue::Text("x".into())).is_err(), "a toggle takes no text");
        assert!(pick("org.test.clip", 0).is_err(), "a panel has no menu");
        closed(7);
    }

    #[test]
    fn a_drawn_panel_is_opened_with_the_bars_token() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let (tx, rx) = channel();
        apply("org.test.draw", 7, &tx, ToStatus::Set { glyph: 'x', svg: None, tint: None, tooltip: "t".into(), content: StatusContent::Drawn { width: 900, height: 2 } });
        assert_eq!(list()[0].content, StatusContent::Drawn { width: forum_ctl::STATUS_PANEL_MAX_W, height: forum_ctl::STATUS_DRAWN_MIN_H }, "bounded");
        open_panel("org.test.draw", [4; 16]).unwrap();
        assert_eq!(rx.try_recv(), Ok(FromStatus::OpenPanel { token: [4; 16], width: 400, height: 40 }));
        assert!(open_panel("org.none", [4; 16]).is_err());
        assert!(pick("org.test.draw", 0).is_err(), "no menu");
        apply("org.test.menu", 8, &tx, ToStatus::Set { glyph: 'x', svg: None, tint: None, tooltip: "t".into(), content: StatusContent::Menu(Vec::new()) });
        assert!(open_panel("org.test.menu", [4; 16]).is_err(), "not drawn");
    }
}
