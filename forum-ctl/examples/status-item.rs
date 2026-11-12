//! A sample status item (docs/spec/forum-status-items.md): puts an icon
//! in the bar with a panel hanging from it (or, with `--menu`, a menu),
//! prints what the user does and sends the new state back. Run it as a
//! launched app holding `status-item`, or with forum-wm started under
//! `FORUM_STATUS_ANY=1` during development.
//!
//!     cargo run --example status-item [-- --menu]

use std::os::unix::net::UnixStream;

use forum_ctl::{
    decode, encode, read_frame, status_socket_path, write_frame, FromStatus, StatusContent, StatusEntry, StatusNode,
    StatusPanel, StatusText, StatusValue, ToStatus,
};

/// Phosphor "clipboard": shown if the vector icon is refused.
const GLYPH: char = '\u{e196}';
/// The app's colour: its icon lights in it while its panel is open.
const TINT: [u8; 3] = [0x7a, 0x4c, 0xd9];
/// The app's own vector icons, from its bundle: one per state.
const ICON: &str = include_str!("status-item-icons/clipboard.svg");
const ICON_PAUSED: &str = include_str!("status-item-icons/clipboard-paused.svg");
const MODES: [&str; 2] = ["Text only", "Everything"];

struct State {
    paused: bool,
    level: f32,
    mode: u32,
    kept: u32,
    search: String,
}

fn panel(s: &State) -> StatusContent {
    StatusContent::Panel(StatusPanel {
        width: 300,
        nodes: vec![
            StatusNode::Heading { text: "Clipboard history".into() },
            StatusNode::Text {
                text: format!("{} items kept. Copied passwords are never kept.", s.kept),
                style: StatusText::Secondary,
            },
            StatusNode::Group {
                title: "Keep".into(),
                children: vec![
                    StatusNode::Toggle { id: 1, label: "Paused".into(), on: s.paused, enabled: true },
                    StatusNode::Choice {
                        id: 2,
                        options: MODES.iter().map(|m| m.to_string()).collect(),
                        selected: s.mode,
                        segmented: true,
                        enabled: !s.paused,
                    },
                    StatusNode::Slider { id: 3, label: format!("History: {} items", (s.level * 100.0).round()), value: s.level, enabled: true },
                ],
            },
            StatusNode::TextField {
                id: 6,
                label: "Search history".into(),
                placeholder: "Type and press Enter".into(),
                value: s.search.clone(),
                enabled: true,
            },
            StatusNode::Progress { label: "Storage used".into(), value: s.kept as f32 / 100.0 },
            StatusNode::Separator,
            StatusNode::Row {
                children: vec![
                    StatusNode::Button { id: 4, label: "Clear".into(), primary: false, enabled: s.kept > 0 },
                    StatusNode::Button { id: 5, label: "Open".into(), primary: true, enabled: true },
                ],
            },
            StatusNode::Text { text: "last copy 2 min ago".into(), style: StatusText::Caption },
        ],
    })
}

fn menu(s: &State) -> StatusContent {
    let mut menu = vec![StatusEntry::Check { label: "Paused".into(), on: s.paused, enabled: true }, StatusEntry::Separator];
    for (i, m) in MODES.iter().enumerate() {
        menu.push(StatusEntry::Radio { label: (*m).into(), on: s.mode == i as u32, enabled: !s.paused });
    }
    menu.push(StatusEntry::Slider { label: "History".into(), value: s.level });
    menu.push(StatusEntry::Separator);
    menu.push(StatusEntry::Action { label: "Clear history".into(), enabled: s.kept > 0 });
    StatusContent::Menu(menu)
}

fn send(sock: &mut UnixStream, s: &State, as_menu: bool) -> std::io::Result<()> {
    let content = if as_menu { menu(s) } else { panel(s) };
    let svg = Some(if s.paused { ICON_PAUSED } else { ICON }.to_string());
    let m = ToStatus::Set { glyph: GLYPH, tint: Some(TINT), svg, tooltip: "Clipboard history".into(), content };
    let b = encode(&m).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_frame(sock, &b)
}

fn main() -> std::io::Result<()> {
    let as_menu = std::env::args().any(|a| a == "--menu");
    let path = status_socket_path();
    let mut sock = UnixStream::connect(&path)?;
    let mut st = State { paused: false, level: 0.5, mode: 0, kept: 12, search: String::new() };
    send(&mut sock, &st, as_menu)?;
    println!("status item up on {}", path.display());
    loop {
        let frame = read_frame(&mut sock)?;
        let m: FromStatus = decode(&frame).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        println!("{m:?}");
        match m {
            // The menu's entries, by index.
            FromStatus::Picked { index: 0 } => st.paused = !st.paused,
            FromStatus::Picked { index: i @ 2..=3 } => st.mode = i - 2,
            FromStatus::Picked { index: 6 } => st.kept = 0,
            FromStatus::Slid { index: 4, value } => st.level = value,
            // The panel's controls, by id.
            FromStatus::Changed { id: 1, value: StatusValue::On(on) } => st.paused = on,
            FromStatus::Changed { id: 2, value: StatusValue::Chosen(i) } => st.mode = i,
            FromStatus::Changed { id: 3, value: StatusValue::Level(v) } => st.level = v,
            FromStatus::Changed { id: 4, value: StatusValue::Pressed } => st.kept = 0,
            FromStatus::Changed { id: 5, value: StatusValue::Pressed } => println!("open the history window"),
            FromStatus::Changed { id: 6, value: StatusValue::Text(t) } => st.search = t,
            FromStatus::Denied { message } => {
                eprintln!("refused: {message}");
                return Ok(());
            }
            _ => continue,
        }
        send(&mut sock, &st, as_menu)?;
    }
}
