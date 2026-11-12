//! A sample status item (docs/spec/forum-status-items.md): puts an icon
//! with a menu in the bar, prints what the user picks and sends the new
//! state back. Run it as a launched app holding `status-item`, or with
//! forum-wm started under `FORUM_STATUS_ANY=1` during development.
//!
//!     cargo run --example status-item

use std::os::unix::net::UnixStream;

use forum_ctl::{decode, encode, read_frame, status_socket_path, write_frame, FromStatus, StatusEntry, ToStatus};

struct State {
    paused: bool,
    level: f32,
    mode: usize,
}

fn menu(s: &State) -> ToStatus {
    let modes = ["Text only", "Everything"];
    let mut menu = vec![
        StatusEntry::Check { label: "Paused".into(), on: s.paused, enabled: true },
        StatusEntry::Separator,
    ];
    for (i, m) in modes.iter().enumerate() {
        menu.push(StatusEntry::Radio { label: (*m).into(), on: s.mode == i, enabled: !s.paused });
    }
    menu.push(StatusEntry::Slider { label: "History".into(), value: s.level });
    menu.push(StatusEntry::Separator);
    menu.push(StatusEntry::Action { label: "Clear history".into(), enabled: true });
    // Phosphor "clipboard".
    ToStatus::Set { glyph: '\u{e196}', tooltip: "Clipboard history".into(), menu }
}

fn send(s: &mut UnixStream, m: &ToStatus) -> std::io::Result<()> {
    let b = encode(m).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_frame(s, &b)
}

fn main() -> std::io::Result<()> {
    let path = status_socket_path();
    let mut s = UnixStream::connect(&path)?;
    let mut st = State { paused: false, level: 0.5, mode: 0 };
    send(&mut s, &menu(&st))?;
    println!("status item up on {}", path.display());
    loop {
        let frame = read_frame(&mut s)?;
        let m: FromStatus = decode(&frame).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        println!("{m:?}");
        match m {
            FromStatus::Picked { index: 0 } => st.paused = !st.paused,
            FromStatus::Picked { index: i @ 2..=3 } => st.mode = i as usize - 2,
            FromStatus::Picked { index: 6 } => println!("history cleared"),
            FromStatus::Slid { index: 4, value } => st.level = value,
            FromStatus::Denied { message } => {
                eprintln!("refused: {message}");
                return Ok(());
            }
            _ => continue,
        }
        send(&mut s, &menu(&st))?;
    }
}
