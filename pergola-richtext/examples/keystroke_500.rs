//! O3b gate (officina.md §6, §16.11): keystroke latency in a 500-page document.
//! Each key is handled and the scene diff built (what a frame costs the app),
//! with the pinned fonts' shaping; typing, Enter and Backspace on page 250.
//!
//! cargo run --release --example keystroke_500

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use officina_text::layout::PinnedFonts;
use officina_text::model::TextDoc;
use pergola::app::App;
use pergola::event::{Event, Key, KeyEventKind, Modifiers};
use pergola::geom::Rect;
use pergola::reactive::Mutable;
use pergola::view::{Ctx, View};
use pergola::widgets::scroll_view::ScrollState;
use pergola_richtext::{Editor, Pos, RichTextView};

struct Host(Rc<RefCell<Editor>>, Mutable<ScrollState>, Mutable<u64>);
impl View for Host {
    fn render(&self, ctx: &mut Ctx) {
        RichTextView::new(self.0.clone(), self.1.clone(), self.2.clone(), Rect::new(0.0, 0.0, 1200.0, 900.0)).emit(ctx);
    }
}

const WORDS: &[&str] = &["the", "layout", "of", "a", "page", "depends", "on", "every", "paragraph", "before", "it", "and", "pagination", "keeps", "checkpoints", "so", "that", "an", "edit", "repaginates", "only", "what", "changed", "naïve", "café"];

fn main() {
    let pages_wanted = 500usize;
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let mut x: u64 = 0x1234_5679;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let h1 = d.styles.by_name("Heading 1").unwrap();
    for i in 0..pages_wanted * 11 / 2 {
        if i % 30 == 0 {
            d.push_para(&format!("Chapter {}", i / 30 + 1), h1, vec![]);
        }
        let n = 60 + rnd(80) as usize;
        let s: Vec<&str> = (0..n).map(|_| WORDS[rnd(WORDS.len() as u64) as usize]).collect();
        d.push_para(&(s.join(" ") + "."), 0, vec![]);
    }
    let fonts = Rc::new(PinnedFonts::new().expect("fonts"));
    let t = Instant::now();
    let mut e = Editor::new(d, fonts);
    while !e.paginate(Duration::from_secs(10)) {}
    let pages = e.pages.pages().len();
    println!("{pages} pages, paginated in {:.2?}", t.elapsed());
    let mid = e.pages.pages()[pages / 2].slices[1].block;
    let ed = Rc::new(RefCell::new(e));
    let dirty = Arc::new(AtomicBool::new(true));
    let scroll = Mutable::with_dirty(ScrollState::default(), dirty.clone());
    let rev = Mutable::with_dirty(0u64, dirty.clone());
    let mut app = App::new_with_flag(Host(ed.clone(), scroll.clone(), rev), dirty);
    app.tick();
    // Bring the caret into view (as a click there would) and focus.
    let key = |app: &mut App<Host>, k: Key, c: &str| {
        let t = Instant::now();
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: k, modifiers: Modifiers::default(), chars: c.into() });
        let d = app.tick();
        (t.elapsed(), d.len())
    };
    // Focus the view (a press on it), then put the caret on the middle page;
    // the first key brings it into view.
    app.handle_event(Event::PointerDown { at: pergola::geom::Point::new(600.0, 400.0) });
    app.handle_event(Event::PointerUp { at: pergola::geom::Point::new(600.0, 400.0) });
    app.tick();
    ed.borrow_mut().set_caret(Pos::new(mid, 10), false);
    key(&mut app, Key::Right, "");
    let caret = ed.borrow().caret;
    println!("caret at block {} (page {})", caret.block, ed.borrow_mut().spot(caret).page);
    let mut times: Vec<(Duration, usize, &str)> = Vec::new();
    let text = "The quick brown fox jumps over the lazy dog while pagination keeps up. ";
    for _ in 0..3 {
        for c in text.chars() {
            let (t, n) = key(&mut app, Key::Char, &c.to_string());
            times.push((t, n, "type"));
        }
        let (t, n) = key(&mut app, Key::Enter, "");
        times.push((t, n, "enter"));
        for _ in 0..10 {
            let (t, n) = key(&mut app, Key::Backspace, "");
            times.push((t, n, "backspace"));
        }
        for _ in 0..5 {
            let (t, n) = key(&mut app, Key::Down, "");
            times.push((t, n, "down"));
        }
    }
    let mut v: Vec<Duration> = times.iter().map(|x| x.0).collect();
    v.sort();
    let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    let worst = times.iter().max_by_key(|x| x.0).unwrap();
    println!("{} keys: p50 {:.2?}, p99 {:.2?}, max {:.2?} ({}, {} node changes)", v.len(), p(0.5), p(0.99), worst.0, worst.2, worst.1);
    let deltas: Vec<usize> = times.iter().map(|x| x.1).collect();
    println!("node changes per key: max {}, mean {:.0}", deltas.iter().max().unwrap(), deltas.iter().sum::<usize>() as f64 / deltas.len() as f64);
    println!("pages now {} (done: {}); frame budget 8.3 ms at 120 Hz", ed.borrow().pages.pages().len(), ed.borrow().pages.done());
}
