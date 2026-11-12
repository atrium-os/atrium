//! `forum-ctl` — a command line for the forum-ctl intent wire, for tests and
//! for switching display modes by hand until a settings panel exists.
//!
//!   forum-ctl surfaces            list the session's surfaces
//!   forum-ctl focus ID            focus a surface
//!   forum-ctl modes               list the monitor's modes (`*` = lit)
//!   forum-ctl mode WxH[@Hz]       switch the display mode (Hz may be 59.94)
//!   forum-ctl outputs             list the monitors and where they sit
//!   forum-ctl arrange A SIDE B    put output A right-of / left-of / above / below output B
//!   forum-ctl move-monitor        send the focused window to the next monitor
//!   forum-ctl scale N F|auto      show output N at scale F (e.g. 1.5), or the automatic one
//!   forum-ctl mirror A B|off      show output B's picture on output A too, or stop
//!   forum-ctl zoom                zoom the focused window, or un-zoom it
//!   forum-ctl span A B.. [gap PX | bezel MM] make outputs A B.. a span group (a zoom covers them all);
//!                               bezel: millimetres between the pictures, as a ruler measures them
//!   forum-ctl span A              take output A out of its span group
//!   forum-ctl spans               list the span groups and what spans
//!   forum-ctl span-app APP on|off let an app's zoom span a group, or not
//!   forum-ctl pads                list the gamepads and which app holds each
//!   forum-ctl take-back [ID]      take pad ID (default: every held pad) back from its app
//!   forum-ctl status              the apps' bar items and their menus
//!   forum-ctl status-pick APP I   pick entry I of APP's bar menu
//!   forum-ctl status-press APP ID press button ID of APP's bar panel

use forum_ctl::{default_socket_path, request, Intent, Reply};

fn usage() -> ! {
    eprintln!("usage: forum-ctl surfaces | focus ID | modes | mode WxH[@Hz] | outputs \
               | arrange A right-of|left-of|above|below B | move-monitor | scale N F|auto | mirror A B|off \
               | zoom | span A [B..] [gap PX | bezel MM] | spans | span-app APP on|off \
               | pads | take-back [ID] | status | status-pick APP I | status-press APP ID");
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
        ["outputs"] => Intent::ListOutputs,
        ["arrange", a, side, b] => Intent::PlaceOutput {
            output: a.parse().unwrap_or_else(|_| usage()),
            side: side.parse().unwrap_or_else(|e| { eprintln!("forum-ctl: {e}"); usage() }),
            of: b.parse().unwrap_or_else(|_| usage()),
        },
        ["move-monitor"] => Intent::MoveToNextMonitor,
        ["mirror", a, b] => Intent::MirrorOutput {
            output: a.parse().unwrap_or_else(|_| usage()),
            of: if *b == "off" { None } else { Some(b.parse().unwrap_or_else(|_| usage())) },
        },
        ["scale", n, f] => Intent::SetOutputScale {
            output: n.parse().unwrap_or_else(|_| usage()),
            scale_milli: if *f == "auto" { 0 } else {
                (f.parse::<f64>().unwrap_or_else(|_| usage()) * 1000.0).round() as u32
            },
        },
        ["zoom"] => Intent::ToggleZoom,
        ["spans"] => Intent::ListSpanGroups,
        ["span-app", app, on] => Intent::SetAppSpans {
            app: app.to_string(),
            spans: match *on { "on" => true, "off" => false, _ => usage() },
        },
        ["span", rest @ ..] if !rest.is_empty() => {
            let (outs, gap, gap_mm) = match rest {
                [o @ .., "gap", g] => (o, g.parse().unwrap_or_else(|_| usage()), 0),
                [o @ .., "bezel", g] => (o, 0, g.parse().ok().filter(|&m| m > 0).unwrap_or_else(|| usage())),
                o => (o, 0, 0),
            };
            Intent::SetSpanGroup { outputs: outs.iter().map(|n| n.parse().unwrap_or_else(|_| usage())).collect(), gap, gap_mm }
        }
        ["pads"] => Intent::ListPads,
        ["status"] => Intent::ListStatusItems,
        ["status-pick", app, i] => Intent::StatusPick { app_id: app.to_string(), index: i.parse().unwrap_or_else(|_| usage()) },
        ["status-press", app, id] => Intent::StatusInput {
            app_id: app.to_string(),
            id: id.parse().unwrap_or_else(|_| usage()),
            value: forum_ctl::StatusValue::Pressed,
        },
        ["take-back"] => Intent::TakeBackPad { device_id: 0 },
        ["take-back", id] => Intent::TakeBackPad { device_id: id.parse().ok().filter(|&d| d != 0).unwrap_or_else(|| usage()) },
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
        Ok(Reply::Outputs { outputs }) => {
            if outputs.is_empty() { println!("no monitor"); }
            for o in outputs {
                println!("output {} {}x{}@{:.2} at {},{} scale {:.2}{} monitor {:016x}{}", o.output, o.rect.w, o.rect.h,
                         o.refresh_mhz as f64 / 1000.0, o.rect.x, o.rect.y,
                         o.scale_milli as f64 / 1000.0, if o.scale_auto { "" } else { " (set)" }, o.identity,
                         if o.present { "" } else { " (gone, place kept)" });
                if o.width_mm != 0 {
                    println!("  {}x{} mm", o.width_mm, o.height_mm);
                }
                if o.mirror_of != 0 {
                    println!("  mirrors monitor {:016x}", o.mirror_of);
                }
            }
        }
        Ok(Reply::SpanGroups { groups, spanning, no_span }) => {
            if groups.is_empty() { println!("no span groups"); }
            for g in groups {
                let ids: Vec<String> = g.members.iter().map(|m| format!("{m:016x}")).collect();
                if g.gap_mm != 0 {
                    println!("span {} bezel {} mm", ids.join(" "), g.gap_mm);
                } else {
                    println!("span {} gap {}", ids.join(" "), g.gap);
                }
            }
            if spanning != 0 { println!("window {spanning} spans"); }
            for a in no_span { println!("app {a} does not span"); }
        }
        Ok(Reply::StatusItems { items }) => {
            if items.is_empty() { println!("no status items"); }
            for it in items {
                println!("{} {:?} {:?}", it.app_id, it.glyph, it.tooltip);
                match it.content {
                    forum_ctl::StatusContent::Menu(menu) => {
                        for (i, e) in menu.iter().enumerate() {
                            println!("  {i}: {e:?}");
                        }
                    }
                    forum_ctl::StatusContent::Drawn { width, height } => println!("  its own panel, {width}x{height}"),
                    forum_ctl::StatusContent::Panel(p) => {
                        println!("  panel {} px wide", p.width);
                        for n in &p.nodes {
                            println!("  {n:?}");
                        }
                    }
                }
            }
        }
        Ok(Reply::Pads { pads }) => {
            if pads.is_empty() { println!("no gamepads"); }
            for p in pads {
                let to = if p.pad.window_id == 0 { "no window".to_string() } else { format!("window {}", p.pad.window_id) };
                let held = if p.pad.held_by == 0 { String::new() } else { format!(", held by {} (window {})", p.held_by_app, p.pad.held_by) };
                println!("pad {} {:?} {:04x}:{:04x} -> {to}{held}", p.pad.device_id, p.pad.name, p.pad.vendor, p.pad.product);
            }
        }
        Ok(Reply::Err { message }) => { eprintln!("forum-ctl: {message}"); std::process::exit(1) }
        Err(e) => { eprintln!("forum-ctl: {}: {e}", sock.display()); std::process::exit(1) }
    }
}
