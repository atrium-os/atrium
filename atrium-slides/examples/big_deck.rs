//! The §9.3 very-large-deck gates (officina.md §16.33 O6e).
//!
//! ```text
//! big_deck gen OUT.oslides [SLIDES] [PHOTOS]   1,000 slides, 1,500 photos (~2 GB)
//! big_deck check OUT.oslides                   open, strip, presenter, memory
//! ```
//!
//! `gen` writes distinct 6 MP photos (JPEG, about 1.3 MB each), two on each
//! of the first slides and one on the rest, with their previews, as the app
//! leaves a deck once it has made them and saved. `check` measures, in this
//! process, what the app does:
//!
//! - **open**: map the file, read the deck, draw the first slide at
//!   1920 × 1080 (under 1 s);
//! - **strip**: every slide's thumbnail drawn as the strip scrolls through
//!   the deck, one new thumbnail at a time (each under a 60 Hz frame);
//! - **presenter**: for each slide, the next one drawn on the main thread
//!   from previews (under a frame) and its full decodes back from the
//!   workers (the time the presenter must stay on a slide before clicking
//!   on shows it sharp);
//! - **memory**: the peak resident size over all of it (under 500 MB).
//!
//! The deck goes in a scratch folder, never in the repository.

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_slides::doc::Doc;
use atrium_slides::render::{self, Place};
use officina_slides::draw::{self, Notes};
use officina_slides::model::{Deck, Rect, Shape, ShapeKind};
use officina_slides::native;
use officina_slides::ops::new_slide;
use officina_text::layout::{FixedFonts, Fonts, PinnedFonts};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => {
            let slides = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(1000);
            let photos = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(1500);
            gen(&args[2], slides, photos)
        }
        Some("check") => check(&args[2]),
        _ => eprintln!("usage: big_deck gen OUT.oslides [SLIDES] [PHOTOS] | check OUT.oslides"),
    }
}

/// A 6 MP photo-like picture, different for each `seed` (gradients, waves
/// and grain, so it compresses like a photo, not like a flat colour).
fn photo(seed: u32) -> Vec<u8> {
    let (w, h) = (3000u32, 2000u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let s = seed.wrapping_mul(2_654_435_761);
    let f = 0.002 + (seed % 17) as f32 * 0.0004;
    for y in 0..h {
        for x in 0..w {
            let mut n = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ s).wrapping_mul(0x9E37_79B1);
            n ^= n >> 15;
            let grain = (n & 31) as f32 - 16.0;
            let wave = ((x as f32 * f + (seed % 7) as f32).sin() * (y as f32 * f * 0.7).cos()) * 60.0;
            let i = ((y * w + x) * 4) as usize;
            px[i] = (90.0 + x as f32 / w as f32 * 120.0 + wave + grain).clamp(0.0, 255.0) as u8;
            px[i + 1] = (70.0 + y as f32 / h as f32 * 110.0 - wave * 0.5 + grain).clamp(0.0, 255.0) as u8;
            px[i + 2] = (60.0 + ((seed * 37) % 120) as f32 + wave * 0.3 + grain).clamp(0.0, 255.0) as u8;
            px[i + 3] = 255;
        }
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 80).encode(&px, w as u16, h as u16, jpeg_encoder::ColorType::Rgba).expect("encode");
    out
}

fn gen(out: &str, slides: usize, photos: usize) {
    let t = Instant::now();
    // Photos on 4 threads.
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let made = Arc::new(std::sync::Mutex::new(vec![None; photos]));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (next, made) = (next.clone(), made.clone());
            std::thread::spawn(move || loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= photos {
                    break;
                }
                let p = photo(i as u32 + 1);
                let pv = atrium_slides::decode::make_preview(&p).expect("preview");
                made.lock().unwrap()[i] = Some((p, pv));
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    let made: Vec<_> = Arc::try_unwrap(made).ok().unwrap().into_inner().unwrap().into_iter().map(|p| p.unwrap()).collect();
    eprintln!("big_deck: {photos} photos in {:.1} s", t.elapsed().as_secs_f64());
    let mut d = Deck::new();
    let layout = d.layout_of_kind("obj").unwrap().id;
    let (sw, sh) = d.size;
    let two = photos.saturating_sub(slides).min(slides);
    let mut p = 0;
    let mut bytes = 0u64;
    for i in 0..slides {
        let mut s = new_slide(&mut d, layout);
        if let Some(t) = s.shapes.iter_mut().find_map(|sh| sh.text_mut()) {
            officina_slides::model::set_plain(&mut t.doc, &format!("Slide {} of {slides}", i + 1));
        }
        let k = if i < two { 2 } else { 1 };
        for j in 0..k {
            if p >= photos {
                break;
            }
            let (img, (mime, w, h, pv)) = &made[p];
            let m = d.add_media("image/jpeg", img);
            d.set_preview(m, mime, *w, *h, pv);
            bytes += img.len() as u64;
            let id = d.new_id();
            let r = if k == 2 { Rect::new(sw / 20 + j as i64 * sw / 2, sh / 4, sw * 2 / 5, sh * 3 / 5) } else { Rect::new(sw / 10, sh / 5, sw * 4 / 5, sh * 7 / 10) };
            s.shapes.push(Shape::new(id, r, ShapeKind::Picture { media: m, crop: [0; 4], line: None }));
            p += 1;
        }
        d.push_slide(s);
    }
    let f = File::create(out).expect("create");
    let f = native::save(&d, std::io::BufWriter::with_capacity(1 << 20, f)).expect("save").into_inner().unwrap();
    f.sync_all().unwrap();
    eprintln!("big_deck: {slides} slides, {p} photos ({:.2} GB of photos), file {:.2} GB, in {:.1} s", bytes as f64 / 1e9, f.metadata().unwrap().len() as f64 / 1e9, t.elapsed().as_secs_f64());
}

fn rss_mb(field: &str) -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines().find(|l| l.starts_with(field)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).map_or(0.0, |kb| kb / 1024.0)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0)
}

fn check(path: &str) {
    // The deck's own memory is anonymous memory; the mapped file's pages
    // (RssFile) are the page cache, shared and given back under pressure.
    let peak_anon = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let p = peak_anon.clone();
        std::thread::spawn(move || loop {
            let kb = (rss_mb("RssAnon:") * 1024.0) as u64;
            p.fetch_max(kb, std::sync::atomic::Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(5));
        });
    }
    let fonts: Box<dyn Fonts> = match PinnedFonts::new() {
        Ok(f) => Box::new(f),
        Err(_) => Box::new(FixedFonts),
    };
    let screen = (1920.0f64, 1080.0f64);
    // Open: map, read the deck, draw the first slide.
    let t = Instant::now();
    let f = File::open(path).expect("open");
    let mut doc = Doc::from_file("big.oslides", &f).expect("load");
    let deck_key = 1;
    let k = (screen.0 / doc.deck.size.0 as f64).min(screen.1 / doc.deck.size.1 as f64);
    let at = Place { k, x: 0.0, y: 0.0 };
    let items = draw::draw_slide(&mut doc.deck, 0, &*fonts, &mut Notes::default()).unwrap();
    let mut out = Vec::new();
    render::nodes(&doc.deck, deck_key, &items, at, 1.0, &mut out);
    let open = t.elapsed();
    let n = doc.deck.slide_count();
    println!("open: {n} slides, {} media, first slide drawn in {:.0} ms (gate 1000 ms), RSS {:.0} MB", doc.deck.media_count(), open.as_secs_f64() * 1e3, rss_mb("VmRSS:"));

    // The strip: one new thumbnail at a time (200 px wide, device scale 2).
    let tk = 200.0 / doc.deck.size.0 as f64;
    let mut strip = Vec::with_capacity(n);
    for i in 0..n {
        let t = Instant::now();
        let items = draw::draw_slide(&mut doc.deck, i, &*fonts, &mut Notes::default()).unwrap_or_default();
        let mut out = Vec::new();
        render::nodes(&doc.deck, deck_key, &items, Place { k: tk, x: 0.0, y: 0.0 }, 2.0, &mut out);
        strip.push(t.elapsed().as_secs_f64() * 1e3);
        // The strip lets slides far from the view go back to the file.
        if i >= 24 {
            doc.deck.unload_slide(i - 24);
        }
    }
    let worst = strip.iter().cloned().fold(0.0, f64::max);
    println!("strip: {n} thumbnails, median {:.1} ms, p99 {:.1} ms, worst {:.1} ms (gate 16.7 ms a frame), decodes asked {}, RSS {:.0} MB", pct(&mut strip.clone(), 0.5), pct(&mut strip.clone(), 0.99), worst, render::decoding(), rss_mb("VmRSS:"));

    // The presenter: on slide i, slide i + 1 is drawn (from previews at
    // once) and its full decodes come back from the workers.
    let mut draw_next = Vec::with_capacity(n);
    let mut sharp = Vec::with_capacity(n);
    for i in 0..n.saturating_sub(1) {
        let t = Instant::now();
        let items = draw::draw_slide(&mut doc.deck, i + 1, &*fonts, &mut Notes::default()).unwrap_or_default();
        let mut out = Vec::new();
        render::nodes(&doc.deck, deck_key, &items, at, 1.0, &mut out);
        draw_next.push(t.elapsed().as_secs_f64() * 1e3);
        while render::decoding() > 0 {
            render::pump(atrium_slides::decode::take_decoded());
            if t.elapsed() > Duration::from_secs(30) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        sharp.push(t.elapsed().as_secs_f64() * 1e3);
        if i >= 2 {
            doc.deck.unload_slide(i - 2);
        }
    }
    println!(
        "presenter: next slide drawn from previews median {:.1} ms, p99 {:.1} ms, worst {:.1} ms (gate 16.7 ms); sharp after median {:.0} ms, p99 {:.0} ms, worst {:.0} ms",
        pct(&mut draw_next.clone(), 0.5),
        pct(&mut draw_next.clone(), 0.99),
        draw_next.iter().cloned().fold(0.0, f64::max),
        pct(&mut sharp.clone(), 0.5),
        pct(&mut sharp.clone(), 0.99),
        sharp.iter().cloned().fold(0.0, f64::max)
    );

    // Rehearse.
    let t = Instant::now();
    let mut copy = doc.deck.clone();
    let costs = atrium_slides::tools::rehearse(&mut copy, (1920, 1080), |_, _| {});
    println!("rehearse: {:.1} s; {}", t.elapsed().as_secs_f64(), atrium_slides::tools::rehearse_summary(&costs));
    println!(
        "memory: peak anonymous {:.0} MB (gate 500 MB); peak RSS {:.0} MB with the mapped file's pages ({:.0} MB of them now)",
        peak_anon.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1024.0,
        rss_mb("VmHWM:"),
        rss_mb("RssFile:")
    );
}
