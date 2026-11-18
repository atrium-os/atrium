//! Picture work off the window's thread (officina.md §9.3): decoding a
//! picture at the size it is drawn, and making a picture's preview (a small
//! JPEG or PNG stored in the deck, [`officina_slides::model::Preview`]).
//!
//! A few worker threads share two queues; decodes go before previews, so the
//! slide being looked at sharpens before the rest of the deck gets previews.
//! Results come back through [`take`], on the window's thread, which puts
//! them where they are drawn from and asks for a paint.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

pub use officina_text::picture::Pixels;
use officina_slides::model::{MediaId, PREVIEW_SIDE};

/// What a job is for: (deck key, media, width, height) of a decode.
pub type Key = (u64, MediaId, u32, u32);

pub enum Done {
    /// A picture decoded for drawing at `key`'s size (`None`: it didn't).
    Decoded(Key, Option<Pixels>),
    /// A preview made for (deck key, media): (mime, width, height, file).
    Preview(u64, MediaId, Option<(&'static str, u32, u32, Vec<u8>)>),
}

enum Job {
    Decode(Key, Arc<[u8]>),
    Preview(u64, MediaId, Arc<[u8]>),
}

#[derive(Default)]
struct Queues {
    decode: VecDeque<Job>,
    preview: VecDeque<Job>,
    done: Vec<Done>,
    /// Jobs taken and not done.
    running: usize,
}

struct Pool {
    q: Mutex<Queues>,
    cv: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let p: &'static Pool = Box::leak(Box::new(Pool { q: Mutex::new(Queues::default()), cv: Condvar::new() }));
        // One core stays with the window.
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).clamp(1, 4);
        for i in 0..n {
            let _ = std::thread::Builder::new().name(format!("slides decode {i}")).spawn(move || work(p));
        }
        p
    })
}

fn work(p: &'static Pool) {
    loop {
        let job = {
            let mut q = p.q.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(j) = q.decode.pop_front().or_else(|| q.preview.pop_front()) {
                    q.running += 1;
                    break j;
                }
                q = p.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let done = match job {
            Job::Decode(key, bytes) => Done::Decoded(key, decode(&bytes, key.2, key.3)),
            Job::Preview(deck, media, bytes) => Done::Preview(deck, media, make_preview(&bytes)),
        };
        let mut q = p.q.lock().unwrap_or_else(|e| e.into_inner());
        q.running -= 1;
        q.done.push(done);
    }
}

/// `bytes` decoded to at most `w` × `h` px (one scale for both axes): by
/// the convert worker (the installed officina_text::picture decoder), or,
/// in tests and with `local-decode`, here.
pub fn decode(bytes: &[u8], w: u32, h: u32) -> Option<Pixels> {
    if let Some(d) = officina_text::picture::decoder() {
        return d.decode(bytes, Some((w, h)));
    }
    #[cfg(any(test, feature = "local-decode"))]
    {
        use navigator_image::image::{decode_unaddressed, Ceilings};
        let px = decode_unaddressed(bytes, Ceilings { target: Some((w, h)), ..Ceilings::default() }).ok()?;
        Some(Pixels { width: px.width, height: px.height, rgba: px.rgba })
    }
    #[cfg(not(any(test, feature = "local-decode")))]
    None
}

/// A picture's natural size from its header (None for SVG, which has none).
pub fn natural_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some(d) = officina_text::picture::decoder() {
        return d.size(bytes);
    }
    #[cfg(any(test, feature = "local-decode"))]
    {
        navigator_image::image::natural_size(bytes).ok().map(|(_, w, h)| (w, h))
    }
    #[cfg(not(any(test, feature = "local-decode")))]
    None
}

/// A picture's preview: decoded to [`PREVIEW_SIDE`] on its longer side,
/// then a JPEG (quality 82), or a PNG when it has transparency.
pub fn make_preview(bytes: &[u8]) -> Option<(&'static str, u32, u32, Vec<u8>)> {
    // The decoder fills its target box (one scale, the larger), so ask for
    // the box the longer side makes.
    let (nw, nh) = natural_size(bytes)?;
    let k = (PREVIEW_SIDE as f64 / nw.max(nh).max(1) as f64).min(1.0);
    let px = decode(bytes, ((nw as f64 * k).round() as u32).max(1), ((nh as f64 * k).round() as u32).max(1))?;
    let (w, h) = (px.width, px.height);
    if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
        return None;
    }
    if px.rgba.chunks_exact(4).all(|p| p[3] == 255) {
        let mut out = Vec::new();
        let enc = jpeg_encoder::Encoder::new(&mut out, 82);
        enc.encode(&px.rgba, w as u16, h as u16, jpeg_encoder::ColorType::Rgba).ok()?;
        Some(("image/jpeg", w, h, out))
    } else {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().ok()?;
            wr.write_image_data(&px.rgba).ok()?;
        }
        Some(("image/png", w, h, out))
    }
}

/// Decodes `bytes` for `key` on a worker.
pub fn want_decode(key: Key, bytes: Arc<[u8]>) {
    let p = pool();
    p.q.lock().unwrap_or_else(|e| e.into_inner()).decode.push_back(Job::Decode(key, bytes));
    p.cv.notify_one();
}

/// Makes media `media`'s preview (of deck `deck`) on a worker.
pub fn want_preview(deck: u64, media: MediaId, bytes: Arc<[u8]>) {
    let p = pool();
    p.q.lock().unwrap_or_else(|e| e.into_inner()).preview.push_back(Job::Preview(deck, media, bytes));
    p.cv.notify_one();
}

/// Jobs queued or running: (decodes and previews waiting, running).
pub fn pending() -> (usize, usize) {
    let q = pool().q.lock().unwrap_or_else(|e| e.into_inner());
    (q.decode.len() + q.preview.len(), q.running)
}

/// The results ready since the last call.
pub fn take() -> Vec<Done> {
    std::mem::take(&mut pool().q.lock().unwrap_or_else(|e| e.into_inner()).done)
}

/// The decodes ready since the last call (previews stay for [`take`]).
pub fn take_decoded() -> Vec<Done> {
    let mut q = pool().q.lock().unwrap_or_else(|e| e.into_inner());
    let (dec, rest): (Vec<Done>, Vec<Done>) = std::mem::take(&mut q.done).into_iter().partition(|d| matches!(d, Done::Decoded(..)));
    q.done = rest;
    dec
}

/// Drops every queued job (a deck closed); running ones finish.
pub fn clear() {
    let p = pool();
    let mut q = p.q.lock().unwrap_or_else(|e| e.into_inner());
    q.decode.clear();
    q.preview.clear();
}
