//! `forum-ctl` — a command line for the forum-ctl intent wire, for tests and
//! for switching display modes by hand until a settings panel exists.
//!
//!   forum-ctl surfaces            list the session's surfaces
//!   forum-ctl focus ID            focus a surface
//!   forum-ctl modes               list the monitor's modes (`*` = lit)
//!   forum-ctl mode WxH[@Hz]       switch the display mode (Hz may be 59.94)

use forum_ctl::{default_socket_path, request, Intent, Reply};

fn usage() -> ! {
    eprintln!("usage: forum-ctl surfaces | focus ID | modes | mode WxH[@Hz]");
    std::process::exit(2)
}

fn parse_mode(s: &str) -> Option<(u32, u32, u32)> {
    let (size, hz) = match s.split_once('@') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let (w, h) = size.split_once('x')?;
    let mhz = match hz {
        Some(h) => (h.parse::<f64>().ok()? * 1000.0).round() as u32,
        None => 0,
    };
    Some((w.parse().ok()?, h.parse().ok()?, mhz))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let intent = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["surfaces"] => Intent::ListSurfaces,
        ["focus", id] => Intent::Focus { surface_id: id.parse().unwrap_or_else(|_| usage()) },
        ["modes"] => Intent::ListDisplayModes,
        ["mode", m] => {
            let (width, height, refresh_mhz) = parse_mode(m).unwrap_or_else(|| usage());
            Intent::SetDisplayMode { width, height, refresh_mhz }
        }
        _ => usage(),
    };
    let sock = default_socket_path();
    match request(&sock, &intent) {
        Ok(Reply::Ack) => println!("ok"),
        Ok(Reply::Surfaces { surfaces, focus }) => {
            for s in surfaces {
                let mark = if s.surface_id == focus { '*' } else { ' ' };
                println!("{mark} {:>4} {:?} {} {}x{}+{}+{}", s.surface_id, s.role, s.owner_app,
                         s.rect.w, s.rect.h, s.rect.x, s.rect.y);
            }
        }
        Ok(Reply::DisplayModes { modes, current }) => {
            if modes.is_empty() { println!("no monitor connected"); }
            for (i, m) in modes.iter().enumerate() {
                let mark = if i as u32 == current { '*' } else { ' ' };
                let pref = if m.preferred { " (preferred)" } else { "" };
                println!("{mark} {}x{}@{:.2}{pref}", m.width, m.height, m.refresh_mhz as f64 / 1000.0);
            }
        }
        Ok(Reply::Err { message }) => { eprintln!("forum-ctl: {message}"); std::process::exit(1) }
        Err(e) => { eprintln!("forum-ctl: {}: {e}", sock.display()); std::process::exit(1) }
    }
}
