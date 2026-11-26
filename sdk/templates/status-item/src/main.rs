//! {{title}}: an item in the bar with a menu (template: status-item).
//!
//! The bar draws the icon and the menu; this process only says what is in
//! the menu and hears what the person picked. Quit ends the process with
//! exit 0, which is a clean exit: it is not restarted until the next
//! sign-in. Any other ending (a crash) is restarted.

use std::os::unix::net::UnixStream;

use forum_ctl::{decode, encode, read_frame, status_socket_path, write_frame, FromStatus, StatusContent, StatusEntry, ToStatus};

/// Phosphor "bell": the icon shown in the bar.
const GLYPH: char = '\u{e0ce}';

/// The menu, by index: 0 Paused, 1 a separator, 2 Quit.
const PAUSED: u32 = 0;
const QUIT: u32 = 2;

fn send(sock: &mut UnixStream, paused: bool) -> std::io::Result<()> {
    let content = StatusContent::Menu(vec![
        StatusEntry::Check { label: "Paused".into(), on: paused, enabled: true },
        StatusEntry::Separator,
        StatusEntry::Action { label: "Quit {{title}}".into(), enabled: true },
    ]);
    let m = ToStatus::Set { glyph: GLYPH, tint: None, svg: None, tooltip: "{{title}}".into(), content };
    let b = encode(&m).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_frame(sock, &b)
}

fn main() {
    let mut sock = match UnixStream::connect(status_socket_path()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{{name}}: no bar to join: {e}");
            std::process::exit(1);
        }
    };
    let mut paused = false;
    if let Err(e) = send(&mut sock, paused) {
        eprintln!("{{name}}: {e}");
        std::process::exit(1);
    }
    loop {
        let m: FromStatus = match read_frame(&mut sock).map(|f| decode(&f)) {
            Ok(Ok(m)) => m,
            // The bar went away: a non-zero exit, so it is started again.
            _ => std::process::exit(1),
        };
        match m {
            FromStatus::Picked { index: PAUSED } => paused = !paused,
            FromStatus::Picked { index: QUIT } => std::process::exit(0),
            FromStatus::Denied { message } => {
                eprintln!("{{name}}: the bar refused the item: {message}");
                std::process::exit(1);
            }
            _ => continue,
        }
        if send(&mut sock, paused).is_err() {
            std::process::exit(1);
        }
    }
}
