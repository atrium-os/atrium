//! forum-overview — Forum's window switcher, an ordinary graphics-only app.
//!
//! It holds NO window-management capability. It cannot touch another app's surface
//! directly; it asks the WM core (forum-wm) over forum-ctl to list the session's
//! surfaces and to focus one. The core authorizes and carries out each intent. This
//! is the decomposed-shell contract (docs/spec/forum.md §3) made concrete: the
//! powerful capability lives only in the small core; the chrome is unprivileged.
//!
//! Usage:
//!   forum-overview            # list the session's surfaces
//!   forum-overview focus <id> # ask the core to focus surface <id>
//!
//! Socket: FORUM_CTL_SOCKET (default /tmp/forum-ctl.sock).

use forum_ctl::{Intent, Reply};

fn socket() -> String {
    std::env::var("FORUM_CTL_SOCKET").unwrap_or_else(|_| "/tmp/forum-ctl.sock".into())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sock = socket();

    let intent = match args.first().map(String::as_str) {
        None => Intent::ListSurfaces,
        Some("focus") => {
            let id: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                eprintln!("usage: forum-overview focus <surface_id>");
                std::process::exit(2);
            });
            Intent::Focus { surface_id: id }
        }
        Some(other) => {
            eprintln!("forum-overview: unknown command '{other}' (try: focus <id>)");
            std::process::exit(2);
        }
    };

    match forum_ctl::request(&sock, &intent)? {
        Reply::Surfaces { surfaces, focus } => {
            println!("forum-overview: {} surface(s) (focus={})", surfaces.len(), focus);
            for s in surfaces {
                let star = if s.surface_id == focus { '*' } else { ' ' };
                println!(
                    "  {star} #{:<3} {:<24} {:?}  {}x{}",
                    s.surface_id, s.owner_app, s.role, s.rect.w, s.rect.h
                );
            }
        }
        Reply::Ack => println!("forum-overview: focus applied"),
        Reply::Err { message } => {
            eprintln!("forum-overview: core refused: {message}");
            std::process::exit(1);
        }
    }
    Ok(())
}
