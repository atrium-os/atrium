//! Shaped text drawn by the app (pergola review step 4, pergola.md §10.4):
//! a run of glyph ids already positioned by a shaper (the document engine,
//! or Pergola's own once `Text` moves client-side), rastered here into an
//! R8 atlas and sent as Fresco glyph runs. frescod never shapes these.
//!
//! - [`Face`]: a font's bytes, registered once and named by a process-wide
//!   id, so a node carries a cheap handle and two nodes compare by face.
//! - [`Atlas`]: one 2048² R8 texture per window, shelf-packed, each glyph
//!   rastered at its device size on first sight (whole device pixels, no
//!   sub-pixel variants yet). New glyphs reach frescod as one region update
//!   just before the run that needs them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// A font face: its bytes and the face index inside them.
#[derive(Clone)]
pub struct Face(Arc<FaceData>);

struct FaceData {
    id: u32,
    bytes: Arc<[u8]>,
    index: u32,
}

static NEXT_FACE: AtomicU32 = AtomicU32::new(1);

impl Face {
    /// Register a face. Call once per font and keep the handle: each call
    /// mints a new id (and so new atlas entries).
    pub fn new(bytes: impl Into<Arc<[u8]>>, index: u32) -> Self {
        Face(Arc::new(FaceData { id: NEXT_FACE.fetch_add(1, Ordering::Relaxed), bytes: bytes.into(), index }))
    }
    pub fn id(&self) -> u32 { self.0.id }
    pub fn bytes(&self) -> &[u8] { &self.0.bytes }
    pub fn index(&self) -> u32 { self.0.index }
}

impl PartialEq for Face {
    fn eq(&self, o: &Self) -> bool { self.0.id == o.0.id }
}

impl std::fmt::Debug for Face {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "Face#{}", self.0.id) }
}

/// One positioned glyph: its id and its pen offset from the run's origin
/// (logical px; y down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub id: u16,
    pub dx: f32,
    pub dy: f32,
}

pub const ATLAS_W: u32 = 2048;
pub const ATLAS_H: u32 = 2048;

/// Where a glyph sits in the atlas and how it hangs off the pen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    pub u: u32,
    pub v: u32,
    pub w: u32,
    pub h: u32,
    /// Left of the pen, device px.
    pub left: f32,
    /// Above the baseline, device px.
    pub top: f32,
}

/// The glyph atlas of one window.
pub struct Atlas {
    data: Vec<u8>,
    x: u32,
    y: u32,
    row: u32,
    /// (face, size in 1/64 device px, glyph) → entry; `None`: blank or
    /// didn't fit.
    slots: HashMap<(u32, u32, u16), Option<Entry>>,
    /// Rows `dirty.0..dirty.1` changed since the last [`Atlas::take_dirty`].
    dirty: Option<(u32, u32)>,
    /// Glyphs dropped because the atlas was full.
    pub dropped: u64,
    scaler: swash::scale::ScaleContext,
}

impl Default for Atlas {
    fn default() -> Self { Self::new() }
}

impl Atlas {
    pub fn new() -> Self {
        Atlas {
            data: vec![0; (ATLAS_W * ATLAS_H) as usize],
            x: 0, y: 0, row: 0,
            slots: HashMap::new(),
            dirty: None,
            dropped: 0,
            scaler: swash::scale::ScaleContext::new(),
        }
    }

    /// The glyph `gid` of `face` at `size` device px, rastered on first use.
    pub fn glyph(&mut self, face: &Face, size: f32, gid: u16) -> Option<Entry> {
        let q = (size * 64.0).round().max(1.0) as u32;
        let key = (face.id(), q, gid);
        if let Some(e) = self.slots.get(&key) {
            return *e;
        }
        let e = self.raster(face, q as f32 / 64.0, gid);
        self.slots.insert(key, e);
        e
    }

    fn raster(&mut self, face: &Face, size: f32, gid: u16) -> Option<Entry> {
        use swash::scale::{Render, Source};
        let font = swash::FontRef::from_index(face.bytes(), face.index() as usize)?;
        let mut s = self.scaler.builder(font).size(size).hint(false).build();
        let img = Render::new(&[Source::Outline]).format(swash::zeno::Format::Alpha).render(&mut s, gid)?;
        let (gw, gh) = (img.placement.width, img.placement.height);
        if gw == 0 || gh == 0 {
            return None;
        }
        if self.x + gw > ATLAS_W {
            self.x = 0;
            self.y += self.row + 1;
            self.row = 0;
        }
        if gw > ATLAS_W || self.y + gh > ATLAS_H {
            self.dropped += 1;
            if self.dropped == 1 {
                log::warn!("glyph atlas full: glyphs are being dropped");
            }
            return None;
        }
        for yy in 0..gh {
            let dst = ((self.y + yy) * ATLAS_W + self.x) as usize;
            let src = (yy * gw) as usize;
            self.data[dst..dst + gw as usize].copy_from_slice(&img.data[src..src + gw as usize]);
        }
        let e = Entry { u: self.x, v: self.y, w: gw, h: gh, left: img.placement.left as f32, top: img.placement.top as f32 };
        let (a, b) = self.dirty.unwrap_or((self.y, self.y + gh));
        self.dirty = Some((a.min(self.y), b.max(self.y + gh)));
        self.x += gw + 1;
        self.row = self.row.max(gh);
        Some(e)
    }

    /// The rows changed since the last call, and their pixels (full width).
    pub fn take_dirty(&mut self) -> Option<(u32, u32, &[u8])> {
        let (a, b) = self.dirty.take()?;
        Some((a, b - a, &self.data[(a * ATLAS_W) as usize..(b * ATLAS_W) as usize]))
    }

    /// The whole atlas (its first upload).
    pub fn pixels(&self) -> &[u8] { &self.data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plex() -> Face {
        Face::new(std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../fonts/ibm-plex/IBMPlexSans.ttf")).unwrap(), 0)
    }

    #[test]
    fn glyphs_raster_once_and_mark_their_rows() {
        let f = plex();
        let mut a = Atlas::new();
        let gid = swash::FontRef::from_index(f.bytes(), 0).unwrap().charmap().map('A');
        let e = a.glyph(&f, 16.0, gid).expect("A has ink");
        assert!(e.w > 4 && e.h > 8 && e.top > 8.0, "{e:?}");
        let (y, rows, px) = a.take_dirty().unwrap();
        assert_eq!((y, rows), (0, e.h));
        assert!(px.iter().any(|&p| p > 200), "coverage was written");
        assert_eq!(a.glyph(&f, 16.0, gid), Some(e), "cached");
        assert!(a.take_dirty().is_none(), "nothing new");
        let space = swash::FontRef::from_index(f.bytes(), 0).unwrap().charmap().map(' ');
        assert_eq!(a.glyph(&f, 16.0, space), None, "blank glyphs take no room");
        let big = a.glyph(&f, 32.0, gid).unwrap();
        assert!(big.h > e.h && big.u > e.u, "another size is another entry");
    }
}
