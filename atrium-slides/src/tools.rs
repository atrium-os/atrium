//! Rehearse and Reduce File Size (officina.md §9.3). Both work on their own
//! copy of the deck, on a thread, and report.
//!
//! **Rehearse** decodes every slide's pictures once at the size the screen
//! shows them, one at a time as a slow machine would, and names the slides
//! that may not be ready when the presenter clicks through quickly (a
//! 200 MP photo), before the talk rather than during it.
//!
//! **Reduce File Size** recompresses each picture down to the largest size
//! it is shown at on a screen `longest` px wide (1920 by default: a deck for
//! a projector rarely needs more) and keeps the smaller of the two. Videos
//! are kept as they are.

use std::time::Instant;

use officina_slides::draw::{self, DrawItem, Notes, Paint, Xform};
use officina_slides::model::{Deck, MediaId};
use officina_text::layout::FixedFonts;

/// A slide whose pictures take longer than this to decode may not be ready
/// when the presenter clicks through it quickly.
pub const READY_MS: f64 = 750.0;

/// One slide's pictures, decoded as the show would.
#[derive(Debug, Clone, PartialEq)]
pub struct SlideCost {
    /// Deck index.
    pub slide: usize,
    pub ms: f64,
    pub pictures: usize,
    /// The slowest: (media, ms, natural size).
    pub slowest: Option<(MediaId, f64, (u32, u32))>,
}

/// Every picture slide `i` draws, with the device px it is drawn at on a
/// screen where the slide is `k` px per EMU.
fn pictures(deck: &mut Deck, i: usize, k: f64) -> Vec<(MediaId, u32, u32)> {
    let items = draw::draw_slide(deck, i, &FixedFonts, &mut Notes::default()).unwrap_or_default();
    let mut out = Vec::new();
    let size = |xf: &Xform, w: f64, h: f64| {
        let r = crate::render::bounds(&draw::mul(&[k, 0.0, 0.0, k, 0.0, 0.0], xf), w, h);
        (r.w().ceil().max(1.0) as u32, r.h().ceil().max(1.0) as u32)
    };
    for it in &items {
        match it {
            DrawItem::Picture { xf, w, h, media, .. } => {
                let (pw, ph) = size(xf, *w, *h);
                out.push((*media, pw, ph));
            }
            DrawItem::Path { xf, w, h, fill: Some(Paint::Picture(media)), .. } => {
                let (pw, ph) = size(xf, *w, *h);
                out.push((*media, pw, ph));
            }
            DrawItem::Video { xf, w, h, poster: Some(p), .. } => {
                let (pw, ph) = size(xf, *w, *h);
                out.push((*p, pw, ph));
            }
            _ => {}
        }
    }
    out
}

/// The px per EMU that fits `deck`'s slides on a `screen` (w, h) px.
fn fit(deck: &Deck, screen: (u32, u32)) -> f64 {
    (screen.0 as f64 / deck.size.0.max(1) as f64).min(screen.1 as f64 / deck.size.1.max(1) as f64)
}

/// Decodes every shown slide's pictures at `screen`'s size, one at a time;
/// `progress(done, of)` after each slide. Slides are read and let go one
/// by one, so memory stays flat on any deck.
pub fn rehearse(deck: &mut Deck, screen: (u32, u32), mut progress: impl FnMut(usize, usize)) -> Vec<SlideCost> {
    let k = fit(deck, screen);
    let n = deck.slide_count();
    let mut out = Vec::new();
    for i in 0..n {
        let hidden = deck.slide(i).map(|s| s.hidden).unwrap_or(false);
        if !hidden {
            let mut cost = SlideCost { slide: i, ms: 0.0, pictures: 0, slowest: None };
            for (media, w, h) in pictures(deck, i, k) {
                let t = Instant::now();
                let bytes = deck.media_bytes(media).ok();
                let natural = bytes.as_ref().and_then(|b| crate::decode::natural_size(b)).unwrap_or((0, 0));
                let _ = bytes.and_then(|b| crate::decode::decode(&b, w, h));
                let ms = t.elapsed().as_secs_f64() * 1e3;
                cost.ms += ms;
                cost.pictures += 1;
                if cost.slowest.is_none_or(|s| ms > s.1) {
                    cost.slowest = Some((media, ms, natural));
                }
            }
            out.push(cost);
        }
        deck.unload_slide(i);
        progress(i + 1, n);
    }
    out
}

/// What Rehearse says about `costs`, in a sentence or two.
pub fn rehearse_summary(costs: &[SlideCost]) -> String {
    let slow: Vec<&SlideCost> = costs.iter().filter(|c| c.ms > READY_MS).collect();
    let worst = costs.iter().map(|c| c.ms).fold(0.0, f64::max);
    if slow.is_empty() {
        return format!("Rehearsed {} slides: every slide is ready in time (the slowest takes {:.0} ms).", costs.len(), worst);
    }
    let list: Vec<String> = slow.iter().take(8).map(|c| (c.slide + 1).to_string()).collect();
    let more = if slow.len() > 8 { format!(" and {} more", slow.len() - 8) } else { String::new() };
    let big = slow.iter().filter_map(|c| c.slowest).max_by(|a, b| a.1.total_cmp(&b.1)).map(|(_, ms, (w, h))| format!(" The slowest picture is {w} × {h} px ({:.0} ms); Reduce File Size makes such pictures smaller.", ms)).unwrap_or_default();
    format!("Rehearsed {} slides: {} may not be ready if you click through quickly: {}{more}.{big}", costs.len(), slow.len(), list.join(", "))
}

/// What Reduce File Size did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reduced {
    /// Pictures made smaller.
    pub pictures: usize,
    /// Their bytes before and after.
    pub before: u64,
    pub after: u64,
    /// Pictures kept as they are (already small, or not smaller as a JPEG).
    pub kept: usize,
}

/// Makes `deck`'s pictures no bigger than they are shown on a screen
/// `longest` px wide (the slide's aspect kept); `progress(done, of)` per
/// picture. Run on a copy of the deck, then saved as a new file.
pub fn reduce(deck: &mut Deck, longest: u32, mut progress: impl FnMut(usize, usize)) -> Reduced {
    let aspect = deck.size.1 as f64 / deck.size.0.max(1) as f64;
    let screen = (longest, ((longest as f64) * aspect).round() as u32);
    let k = fit(deck, screen);
    // The largest size each picture is shown at, anywhere in the deck.
    let mut need: std::collections::HashMap<MediaId, (u32, u32)> = std::collections::HashMap::new();
    for i in 0..deck.slide_count() {
        for (m, w, h) in pictures(deck, i, k) {
            let e = need.entry(m).or_insert((0, 0));
            *e = (e.0.max(w), e.1.max(h));
        }
        deck.unload_slide(i);
    }
    let mut ids: Vec<MediaId> = need.keys().copied().collect();
    ids.sort_unstable();
    let mut r = Reduced::default();
    for (n, id) in ids.iter().enumerate() {
        progress(n, ids.len());
        let (w, h) = need[id];
        let Ok(bytes) = deck.media_bytes(*id) else { continue };
        let Some((nw, nh)) = crate::decode::natural_size(&bytes) else {
            r.kept += 1;
            continue;
        };
        // Vector pictures are drawn at any size; a GIF may move.
        // The scale a decode for (w, h) takes (it fills the box): close to
        // 1, the picture is already about the size it is shown at.
        let scale = (w as f64 / nw.max(1) as f64).max(h as f64 / nh.max(1) as f64);
        if matches!(officina_text::picture::mime(&bytes), Some("image/svg+xml" | "image/gif")) || scale >= 0.95 {
            r.kept += 1;
            continue;
        }
        let Some((mime, smaller)) = recompress(&bytes, w, h) else {
            r.kept += 1;
            continue;
        };
        if smaller.len() >= bytes.len() {
            r.kept += 1;
            continue;
        }
        r.before += bytes.len() as u64;
        r.after += smaller.len() as u64;
        r.pictures += 1;
        deck.replace_media(*id, mime, &smaller);
    }
    progress(ids.len(), ids.len());
    r
}

/// `bytes` decoded to `w` × `h` and encoded again: JPEG (quality 85), or PNG
/// when it has transparency.
pub fn recompress(bytes: &[u8], w: u32, h: u32) -> Option<(&'static str, Vec<u8>)> {
    let px = crate::decode::decode(bytes, w, h)?;
    if px.width > u16::MAX as u32 || px.height > u16::MAX as u32 {
        return None;
    }
    let mut out = Vec::new();
    if px.rgba.chunks_exact(4).all(|p| p[3] == 255) {
        jpeg_encoder::Encoder::new(&mut out, 85).encode(&px.rgba, px.width as u16, px.height as u16, jpeg_encoder::ColorType::Rgba).ok()?;
        Some(("image/jpeg", out))
    } else {
        Some(("image/png", crate::decode::encode_png(px.width, px.height, &px.rgba)?))
    }
}

/// What Reduce File Size says.
pub fn reduce_summary(r: &Reduced) -> String {
    if r.pictures == 0 {
        return format!("No picture is bigger than it is shown ({} kept as they are).", r.kept);
    }
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    format!("{} pictures made smaller: {:.1} MB → {:.1} MB ({} kept as they are).", r.pictures, mb(r.before), mb(r.after), r.kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_slides::model::{Rect, Shape, ShapeKind};
    use officina_slides::ops::new_slide;

    fn photo(w: u32, h: u32) -> Vec<u8> {
        let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % w) as u8, (i / w % 256) as u8, (i * 7 % 256) as u8, 255]).collect();
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 95).encode(&px, w as u16, h as u16, jpeg_encoder::ColorType::Rgba).unwrap();
        out
    }

    fn deck(pic: &[u8]) -> (Deck, MediaId) {
        let mut d = Deck::new();
        let l = d.layout_of_kind("obj").unwrap().id;
        let m = d.add_media("image/jpeg", pic);
        for i in 0..3 {
            let mut s = new_slide(&mut d, l);
            s.hidden = i == 2;
            let id = d.new_id();
            // A quarter of the slide's width.
            s.shapes.push(Shape::new(id, Rect::new(0, 0, d.size.0 / 4, d.size.1 / 4), ShapeKind::Picture { media: m, crop: [0; 4], line: None }));
            d.push_slide(s);
        }
        (d, m)
    }

    #[test]
    fn rehearse_times_each_shown_slide() {
        let (d, m) = deck(&photo(2400, 1600));
        // From a file, as an opened deck is.
        let mut d = officina_slides::native::load(std::sync::Arc::new(officina_slides::native::save_to_vec(&d))).unwrap();
        let mut seen = 0;
        let costs = rehearse(&mut d, (1920, 1080), |done, of| {
            assert!(done <= of);
            seen = done;
        });
        assert_eq!(seen, 3);
        assert_eq!(costs.len(), 2, "the hidden slide is skipped");
        assert!(costs.iter().all(|c| c.pictures == 1 && c.slowest.is_some_and(|s| s.0 == m && s.2 == (2400, 1600))));
        assert_eq!(d.loaded_count(), 0, "slides let go as it goes");
        assert!(rehearse_summary(&costs).starts_with("Rehearsed 2 slides"));
    }

    #[test]
    fn reduce_shrinks_pictures_to_their_shown_size() {
        let big = photo(2400, 1600);
        let (mut d, m) = deck(&big);
        let r = reduce(&mut d, 1920, |_, _| {});
        assert_eq!((r.pictures, r.kept), (1, 0));
        let b = d.media_bytes(m).unwrap();
        let (_, w, h) = navigator_image::image::natural_size(&b).unwrap();
        // Shown 480 px wide on a 1920 px screen (buckets round up a little).
        assert!((480..=500).contains(&w) && h < 340, "{w} × {h}");
        assert!(r.after < r.before && b.len() as u64 == r.after);
        // Again: nothing left to do.
        let r = reduce(&mut d, 1920, |_, _| {});
        assert_eq!((r.pictures, r.kept), (0, 1));
    }
}
