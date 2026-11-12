//! A status item that draws its own panel (docs/spec/forum-status-items.md
//! §4): an icon in the bar; a press on it opens a pop-up this app draws
//! with Pergola (recent clips, copies by hour, a Clear button), hanging
//! from the icon by the bar's grant. Run it as a launched app holding
//! `status-item`, or with forum-wm under `FORUM_STATUS_ANY=1`.
//!
//!     cargo run --example status_drawn

use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use forum_ctl::{decode, encode, read_frame, status_socket_path, write_frame, FromStatus, StatusContent, ToStatus};
use pergola::geom::Rect;
use pergola::node::{Node, TextStyle};
use pergola::theme::{font, radius, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::{Window, WindowDesc};
use pergola::{App, Button, Mutable};

const GLYPH: char = '\u{e196}';
const ICON: &str = include_str!("../../forum-ctl/examples/status-item-icons/clipboard.svg");
const W: u16 = 300;
const H: u16 = 300;

/// What the app keeps between openings.
#[derive(Clone)]
struct Clips {
    recent: Vec<String>,
    per_hour: [u8; 12],
}

struct Panel {
    clips: Mutable<Clips>,
    w: f32,
    h: f32,
}

fn style(color: pergola::Color, size: f32, weight: Weight) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// Text laid out by its top at (`x`, `y`).
fn text(ctx: &mut Ctx, s: &str, x: f32, y: f32, st: TextStyle) {
    let m = Node::measure_text(s, &st);
    ctx.add(Node::Text { rect: Rect::new(x, y, m.w, m.h), content: s.into(), style: st });
}

impl View for Panel {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let c = self.clips.get_cloned();
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, self.w, self.h), fill: t.bg_elevated(), radius: radius::MD });
        let (x, w) = (14.0, self.w - 28.0);
        text(ctx, "Recent clips", x, 12.0, style(t.text_primary(), type_size::MD, Weight::Semibold));
        // Each clip a row: hovered rows light; a click picks it.
        let mut y = 38.0;
        for (i, s) in c.recent.iter().take(4).enumerate() {
            let row = ctx.add(Node::Rect { rect: Rect::new(x - 6.0, y, w + 12.0, 26.0), fill: pergola::Color::TRANSPARENT, radius: radius::SM });
            if ctx.is_hovered(row) {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(row) {
                    *fill = t.state_hover();
                }
            }
            // A real app would copy it back to the clipboard.
            ctx.on_click(row, move || println!("picked clip {i}"));
            let mut line: String = s.chars().take(34).collect();
            if s.chars().count() > 34 {
                line.push('…');
            }
            text(ctx, &line, x, y + 5.0, style(t.text_secondary(), type_size::SM, Weight::Regular));
            y += 28.0;
        }
        if c.recent.is_empty() {
            text(ctx, "Nothing kept yet.", x, y + 5.0, style(t.text_tertiary(), type_size::SM, Weight::Regular));
            y += 28.0;
        }
        // Copies by hour: a bar chart no described panel could show.
        y += 8.0;
        text(ctx, "Copies, last 12 hours", x, y, style(t.text_tertiary(), type_size::XS, Weight::Medium));
        y += 20.0;
        let top = c.per_hour.iter().copied().max().unwrap_or(1).max(1) as f32;
        let bw = w / 12.0;
        for (i, n) in c.per_hour.iter().enumerate() {
            let bh = 60.0 * *n as f32 / top;
            ctx.add(Node::Rect { rect: Rect::new(x + i as f32 * bw + 2.0, y + 60.0 - bh, bw - 4.0, bh.max(2.0)), fill: t.accent_fill(), radius: 2.0 });
        }
        y += 74.0;
        let clips = self.clips.clone();
        Button::secondary("Clear")
            .at(x, y)
            .width(w)
            .on_click(move || {
                let mut c = clips.get_cloned();
                c.recent.clear();
                c.per_hour = [0; 12];
                clips.set(c);
            })
            .render(ctx);
    }
}

fn set(sock: &mut UnixStream) -> std::io::Result<()> {
    let m = ToStatus::Set { glyph: GLYPH, svg: Some(ICON.into()), tooltip: "Clipboard history".into(), content: StatusContent::Drawn { width: W, height: H } };
    write_frame(sock, &encode(&m).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?)
}

/// Open the panel the bar's `token` grants and run it until frescod takes
/// it down (a press outside, focus elsewhere) or Escape closes it.
fn open_panel(token: [u8; 16], width: u16, height: u16, kept: &mut Clips) -> std::io::Result<()> {
    let window = Window::open(&WindowDesc::hosted(token, width as u32, height as u32))?;
    let dirty = Arc::new(AtomicBool::new(true));
    let clips = Mutable::with_dirty(kept.clone(), dirty.clone());
    let view = Panel { clips: clips.clone(), w: window.width, h: window.height };
    let mut app = App::new_with_flag(view, dirty);
    println!("panel open, {}x{}", window.width, window.height);
    window.run(&mut app, |ev, _, flow| {
        // Escape closes it (HID usage 0x29); the window goes with it.
        if matches!(ev, fresco_client::Event::Key { hid_usage: 0x29, pressed: true, .. }) {
            flow.exit();
        }
    })?;
    *kept = clips.get_cloned();
    println!("panel closed");
    Ok(())
}

fn main() -> std::io::Result<()> {
    env_logger::init();
    let mut sock = UnixStream::connect(status_socket_path())?;
    set(&mut sock)?;
    println!("status item up");
    let mut kept = Clips {
        recent: vec!["git push -u origin main".into(), "https://atrium.example/docs/spec".into(), "Meeting at 3, room 2".into(), "cargo test -q".into()],
        per_hour: [1, 0, 3, 5, 2, 0, 0, 4, 7, 3, 1, 2],
    };
    loop {
        let frame = read_frame(&mut sock)?;
        let m: FromStatus = decode(&frame).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        println!("{m:?}");
        match m {
            FromStatus::OpenPanel { token, width, height } => {
                if let Err(e) = open_panel(token, width, height, &mut kept) {
                    eprintln!("panel: {e}");
                }
            }
            FromStatus::Denied { message } => {
                eprintln!("refused: {message}");
                return Ok(());
            }
            _ => {}
        }
    }
}
