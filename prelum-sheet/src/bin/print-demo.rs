//! prelum-print-demo: an app that prints the way any app does
//! (printing.md §5.1): one `pergola::print::Printing` call opens the print
//! sheet over its window, and it reports how printing ended. Used by the
//! VM check (scripts/prelum-r4-vm.sh), which reads its log.
//!
//!   PRINT_PDF=path     the document (default: a page made in memory)
//!   PRINT_TITLE=text   [Print demo]
//!   PRINT_RELAYOUT=1   the app can lay itself out again: it answers a
//!                      Relayout with a page of the asked size
//!   PRINT_AGAIN=n      open the sheet n times in a row (the rate limit)
//!   PRINT_SECS=n       exit after n seconds
//!   FRESCO_SOCKET, LIMEN_SOCKET as usual.
//!
//! Logs `print-demo: queued`, `print-demo: saved NAME`, `print-demo:
//! cancelled`, `print-demo: failed WHY`, `print-demo: relayout WxH ORIENT`,
//! `print-demo: refused WHY` (limend said no), `print-demo: the app got a
//! press` (a press that reached the app, not the sheet).

use std::os::fd::AsFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pergola::embed::Embeds;
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::print::{Orientation, Outcome, Paper, Printing, Step};
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

const SLOT: u64 = 1;

#[derive(Clone, Default)]
struct Shown {
    printing: bool,
    line: String,
}

struct Demo {
    shown: Mutable<Shown>,
    size: (f32, f32),
}

impl View for Demo {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let s = self.shown.get_cloned();
        let (w, h) = self.size;
        let bg = ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });
        ctx.on_click(bg, || eprintln!("print-demo: the app got a press"));
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        ctx.add(Node::Text { rect: Rect::new(24.0, 18.0, 0.0, 24.0), content: "Report (a demo app)".into(), style: st(type_size::LG, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Text { rect: Rect::new(24.0, 52.0, 0.0, 20.0), content: s.line.clone(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
        if s.printing {
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 1.0, 1.0), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, SLOT);
        }
        ctx.pop();
    }
}

/// A one-page PDF of `w` x `h` points with a frame and a filled square, made
/// here (no library): what an app with nothing paged hands over.
fn page(w: u32, h: u32) -> Vec<u8> {
    let content = format!("0 g 36 36 {} {} re S 0.2 0.4 0.8 rg 72 {} 144 144 re f", w - 72, h - 72, h - 72 - 144);
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents 4 0 R /Resources << >> >>"),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offs {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).bytes());
    out
}

fn main() {
    let title = std::env::var("PRINT_TITLE").unwrap_or_else(|_| "Print demo".into());
    let relayout = std::env::var_os("PRINT_RELAYOUT").is_some();
    let mut again: u32 = std::env::var("PRINT_AGAIN").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let until = std::env::var("PRINT_SECS").ok().and_then(|s| s.parse::<u64>().ok()).map(|s| Instant::now() + Duration::from_secs(s));
    let file = std::env::var("PRINT_PDF").ok().map(|p| std::fs::File::open(&p).unwrap_or_else(|e| {
        eprintln!("print-demo: failed {p}: {e}");
        std::process::exit(1);
    }));

    let mut win = match Window::open(&WindowDesc::new("Report", SizeSpec::Fixed(900, 640))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("print-demo: frescod: {e}");
            std::process::exit(1);
        }
    };
    let embeds = match Embeds::connect() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("print-demo: limend: {e:?}");
            std::process::exit(1);
        }
    };
    win.set_slot_binder(Embeds::binder(&embeds));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown { printing: true, line: "Opening the print sheet…".into() }, dirty.clone());
    let mut app = App::new_with_flag(Demo { shown: shown.clone(), size: (win.width, win.height) }, dirty);
    let _ = win.paint(&mut app);
    let mut printing: Option<Printing> = None;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        if printing.is_none() && again > 0 {
            if let Some(token) = win.embed_token() {
                again -= 1;
                let r = match &file {
                    Some(f) => Printing::start(&embeds, token, SLOT, f.as_fd(), &title, relayout, None),
                    None => Printing::start_bytes(&embeds, token, SLOT, &page(595, 842), &title, relayout, Some(Paper::A4)),
                };
                match r {
                    Ok(p) => {
                        eprintln!("print-demo: asked to print (embed {})", p.embed());
                        shown.set(Shown { printing: true, line: "Printing…".into() });
                        printing = Some(p);
                    }
                    Err(e) => {
                        eprintln!("print-demo: refused {e:?}");
                        shown.set(Shown { printing: false, line: format!("The print sheet didn't open ({e:?})") });
                    }
                }
            }
        }
        let ev = embeds.borrow_mut().poll(Duration::from_millis(1));
        let ev = match ev {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                if until.is_some_and(|u| Instant::now() >= u) {
                    break;
                }
                continue;
            }
            Err(e) => {
                eprintln!("print-demo: failed limend ({e:?})");
                break;
            }
        };
        let Some(p) = printing.as_mut() else { continue };
        match p.handle(&embeds, &ev) {
            Some(Step::Relayout { paper, orientation, .. }) => {
                let (w, h) = match orientation {
                    Orientation::Portrait => (paper.w_pt, paper.h_pt),
                    Orientation::Landscape => (paper.h_pt, paper.w_pt),
                };
                eprintln!("print-demo: relayout {w}x{h} {orientation:?}");
                if let Err(e) = p.document_bytes(&embeds, &page(w, h)) {
                    eprintln!("print-demo: failed to send the new layout ({e:?})");
                }
            }
            Some(Step::Done(o)) => {
                let line = match &o {
                    Outcome::Queued => "queued".to_string(),
                    Outcome::Saved(name) => format!("saved {name}"),
                    Outcome::Cancelled => "cancelled".to_string(),
                    Outcome::Failed(why) => format!("failed {why}"),
                };
                eprintln!("print-demo: {line}");
                shown.set(Shown { printing: false, line });
                printing = None;
            }
            _ => {}
        }
        if until.is_some_and(|u| Instant::now() >= u) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_page_is_a_pdf_folium_reads() {
        let b = super::page(612, 792);
        assert!(b.starts_with(b"%PDF-1.4") && b.ends_with(b"%%EOF\n"));
    }
}
