//! Decoded pictures for the pages on screen (officina.md §7.4: layout needs
//! sizes, not pixels).
//!
//! A picture is decoded the first time a page showing it is drawn, at about
//! the size it is drawn (never above its natural size), by the Navigator's
//! memory-safe decoders with their ceilings checked on the header. Decoded
//! pixels stay in a least-recently-used cache with a fixed budget; a picture
//! that can't be decoded draws a placeholder in its frame, and since layout
//! used only the frame, nothing moves either way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use navigator_image::image::{decode_unaddressed, Ceilings};
use officina_text::model::ImageData;
use pergola::node::Image;

/// Decoded bytes kept across all documents in this process.
pub const BUDGET: usize = 256 << 20;

struct Entry {
    source: Weak<[u8]>,
    image: Option<Image>,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<(usize, u32, u32), Entry>,
    bytes: usize,
    tick: u64,
    pub decodes: u64,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Rounds a drawn size up, so small zoom changes reuse the decode.
fn bucket(v: f64) -> u32 {
    let v = v.ceil().max(1.0) as u32;
    v.div_ceil(64) * 64
}

/// `data` decoded for drawing `w` × `h` device pixels; None when it can't be.
pub fn picture(data: &ImageData, w: f64, h: f64) -> Option<Image> {
    let target = (bucket(w).min(data.width.max(1)), bucket(h).min(data.height.max(1)));
    let key = (Arc::as_ptr(&data.bytes) as *const u8 as usize, target.0, target.1);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.tick += 1;
        let tick = c.tick;
        if let Some(e) = c.map.get_mut(&key) {
            if e.source.upgrade().is_some_and(|s| Arc::ptr_eq(&s, &data.bytes)) {
                e.used = tick;
                return e.image.clone();
            }
        }
        if let Some(old) = c.map.remove(&key) {
            c.bytes -= old.bytes;
        }
        c.decodes += 1;
        let ceil = Ceilings { target: Some(target), ..Ceilings::default() };
        let image = decode_unaddressed(&data.bytes, ceil).ok().map(|px| Image::new(px.width, px.height, px.rgba));
        let bytes = image.as_ref().map_or(0, |i| i.data().rgba.len());
        // Make room, least recently used first.
        while c.bytes + bytes > BUDGET && !c.map.is_empty() {
            let (&k, _) = c.map.iter().min_by_key(|(_, e)| e.used).expect("not empty");
            let e = c.map.remove(&k).expect("present");
            c.bytes -= e.bytes;
        }
        c.bytes += bytes;
        c.map.insert(key, Entry { source: Arc::downgrade(&data.bytes), image: image.clone(), bytes, used: tick });
        image
    })
}

/// Decodes so far, for tests.
pub fn decodes() -> u64 {
    CACHE.with(|c| c.borrow().decodes)
}
