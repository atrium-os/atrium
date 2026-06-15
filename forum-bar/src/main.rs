//! forum-bar — Forum's statusbar, an ordinary graphics-only app.
//!
//! Holds no window-management capability. It asks the WM core over forum-ctl for the
//! session's surfaces and renders a status summary. Today it prints the line; once
//! the display path is up it draws into its reserved top-edge `chrome` surface via
//! Pergola (docs/spec/forum.md §3).
//!
//! Socket: FORUM_CTL_SOCKET (default /tmp/forum-ctl.sock).

use forum_ctl::{Intent, Reply};

fn socket() -> String {
    std::env::var("FORUM_CTL_SOCKET").unwrap_or_else(|_| "/tmp/forum-ctl.sock".into())
}

fn main() -> std::io::Result<()> {
    match forum_ctl::request(&socket(), &Intent::ListSurfaces)? {
        Reply::Surfaces { surfaces, focus } => {
            println!("{}", forum_bar::status_line(&surfaces, focus));
            Ok(())
        }
        Reply::Err { message } => {
            eprintln!("forum-bar: core refused: {message}");
            std::process::exit(1);
        }
        Reply::Ack => {
            eprintln!("forum-bar: unexpected Ack to ListSurfaces");
            std::process::exit(1);
        }
    }
}
