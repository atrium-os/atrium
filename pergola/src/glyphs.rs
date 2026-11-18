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
//! - Colour glyphs (a COLR face: Noto Color Emoji) go to the atlas's RGBA
//!   twin, [`fresco_protocol::COLOR_ATLAS_W`] square, drawn by `atrium-colr`;
//!   their entries say `color`, and the run marks them `GLYPH_COLOR`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// A font face: its bytes and the face index inside them.
#[derive(Clone)]
pub struct Face(Arc<FaceData>);

struct FaceData {
    id: u32,
    bytes: Bytes,
    index: u32,
    /// Has colour glyphs (COLR), found on first use.
    color: std::sync::OnceLock<bool>,
}

/// A face's bytes: owned, or mapped for the process's life (the system font
/// library's files, shared with every other process through the page cache).
enum Bytes {
    Owned(Arc<[u8]>),
    Static(&'static [u8]),
}

static NEXT_FACE: AtomicU32 = AtomicU32::new(1);

impl Face {
    /// Register a face. Call once per font and keep the handle: each call
    /// mints a new id (and so new atlas entries).
    pub fn new(bytes: impl Into<Arc<[u8]>>, index: u32) -> Self {
        Face(Arc::new(FaceData { id: NEXT_FACE.fetch_add(1, Ordering::Relaxed), bytes: Bytes::Owned(bytes.into()), index, color: Default::default() }))
    }
    /// Register a face whose bytes live as long as the process, without
    /// copying them.
    pub fn from_static(bytes: &'static [u8], index: u32) -> Self {
        Face(Arc::new(FaceData { id: NEXT_FACE.fetch_add(1, Ordering::Relaxed), bytes: Bytes::Static(bytes), index, color: Default::default() }))
    }
    pub fn id(&self) -> u32 { self.0.id }
    pub fn bytes(&self) -> &[u8] {
        match &self.0.bytes {
            Bytes::Owned(b) => b,
            Bytes::Static(b) => b,
        }
    }
    pub fn index(&self) -> u32 { self.0.index }
    /// Whether the face draws colour glyphs (a COLR table).
    pub fn is_color(&self) -> bool {
        *self.0.color.get_or_init(|| self.0.index == 0 && atrium_colr::is_color_font(self.bytes()))
    }
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
    /// On the colour atlas (RGBA), not the coverage one.
    pub color: bool,
}

pub const COLOR_W: u32 = fresco_protocol::COLOR_ATLAS_W;
pub const COLOR_H: u32 = fresco_protocol::COLOR_ATLAS_H;

/// Shelf packing: where the next `w`×`h` goes in a `aw`×`ah` page.
#[derive(Default)]
struct Shelf {
    x: u32,
    y: u32,
    row: u32,
}

impl Shelf {
    fn place(&mut self, w: u32, h: u32, aw: u32, ah: u32) -> Option<(u32, u32)> {
        if self.x + w > aw {
            self.x = 0;
            self.y += self.row + 1;
            self.row = 0;
        }
        if w > aw || self.y + h > ah {
            return None;
        }
        let at = (self.x, self.y);
        self.x += w + 1;
        self.row = self.row.max(h);
        Some(at)
    }
}

/// The glyph atlas of one window.
pub struct Atlas {
    data: Vec<u8>,
    shelf: Shelf,
    /// The colour twin (RGBA, straight alpha, sRGB), made on the first
    /// colour glyph; its changed rows.
    color: Option<Vec<u8>>,
    color_shelf: Shelf,
    color_dirty: Option<(u32, u32)>,
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
            shelf: Shelf::default(),
            color: None,
            color_shelf: Shelf::default(),
            color_dirty: None,
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

    fn full(&mut self) -> Option<Entry> {
        self.dropped += 1;
        if self.dropped == 1 {
            log::warn!("glyph atlas full: glyphs are being dropped");
        }
        None
    }

    /// A colour glyph into the RGBA twin; `None` when the face has no
    /// colour version of `gid` (it is then drawn as coverage).
    fn raster_color(&mut self, face: &Face, size: f32, gid: u16) -> Option<Option<Entry>> {
        // Black for palette entries that mean "the text's colour": a
        // document's emoji carry their own colours.
        let bm = atrium_colr::render(face.bytes(), gid as u32, size, [0.0, 0.0, 0.0, 1.0])?;
        let (gw, gh) = (bm.width, bm.height);
        let Some((x, y)) = self.color_shelf.place(gw, gh, COLOR_W, COLOR_H) else { return Some(self.full()) };
        let data = self.color.get_or_insert_with(|| vec![0; (COLOR_W * COLOR_H * 4) as usize]);
        for yy in 0..gh {
            let dst = (((y + yy) * COLOR_W + x) * 4) as usize;
            let src = (yy * gw * 4) as usize;
            data[dst..dst + (gw * 4) as usize].copy_from_slice(&bm.rgba[src..src + (gw * 4) as usize]);
        }
        let (a, b) = self.color_dirty.unwrap_or((y, y + gh));
        self.color_dirty = Some((a.min(y), b.max(y + gh)));
        Some(Some(Entry { u: x, v: y, w: gw, h: gh, left: bm.left as f32, top: bm.top as f32, color: true }))
    }

    fn raster(&mut self, face: &Face, size: f32, gid: u16) -> Option<Entry> {
        use swash::scale::{Render, Source};
        if face.is_color() {
            if let Some(e) = self.raster_color(face, size, gid) {
                return e;
            }
        }
        let font = swash::FontRef::from_index(face.bytes(), face.index() as usize)?;
        let mut s = self.scaler.builder(font).size(size).hint(false).build();
        let img = Render::new(&[Source::Outline]).format(swash::zeno::Format::Alpha).render(&mut s, gid)?;
        let (gw, gh) = (img.placement.width, img.placement.height);
        if gw == 0 || gh == 0 {
            return None;
        }
        let Some((x, y)) = self.shelf.place(gw, gh, ATLAS_W, ATLAS_H) else { return self.full() };
        for yy in 0..gh {
            let dst = ((y + yy) * ATLAS_W + x) as usize;
            let src = (yy * gw) as usize;
            self.data[dst..dst + gw as usize].copy_from_slice(&img.data[src..src + gw as usize]);
        }
        let e = Entry { u: x, v: y, w: gw, h: gh, left: img.placement.left as f32, top: img.placement.top as f32, color: false };
        let (a, b) = self.dirty.unwrap_or((y, y + gh));
        self.dirty = Some((a.min(y), b.max(y + gh)));
        Some(e)
    }

    /// The rows changed since the last call, and their pixels (full width).
    pub fn take_dirty(&mut self) -> Option<(u32, u32, &[u8])> {
        let (a, b) = self.dirty.take()?;
        Some((a, b - a, &self.data[(a * ATLAS_W) as usize..(b * ATLAS_W) as usize]))
    }

    /// The whole atlas (its first upload).
    pub fn pixels(&self) -> &[u8] { &self.data }

    /// The colour twin's rows changed since the last call, and their
    /// pixels (full width, RGBA).
    pub fn take_color_dirty(&mut self) -> Option<(u32, u32, &[u8])> {
        let (a, b) = self.color_dirty.take()?;
        let d = self.color.as_deref()?;
        Some((a, b - a, &d[(a * COLOR_W * 4) as usize..(b * COLOR_W * 4) as usize]))
    }

    /// The whole colour twin, once a colour glyph made it.
    pub fn color_pixels(&self) -> Option<&[u8]> { self.color.as_deref() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plex() -> Face {
        // Built in, not read at run time: a test binary cross-built on one
        // machine and run on another (the dev VM) has no source tree there.
        static PLEX: &[u8] = include_bytes!("../../fonts/ibm-plex/IBMPlexSans.ttf");
        Face::new(PLEX.to_vec(), 0)
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
        assert!(!e.color && a.color_pixels().is_none());
    }

    /// An emoji of Noto Color Emoji lands on the RGBA twin, in colour.
    #[test]
    fn colour_glyphs_go_to_the_colour_atlas() {
        static EMOJI: &[u8] = include_bytes!("../../fonts/noto/NotoColorEmoji-Regular.ttf");
        let f = Face::from_static(EMOJI, 0);
        assert!(f.is_color() && !plex().is_color());
        let mut a = Atlas::new();
        let gid = swash::FontRef::from_index(f.bytes(), 0).unwrap().charmap().map('😀');
        let e = a.glyph(&f, 32.0, gid).expect("drawn");
        assert!(e.color && e.w > 30 && e.h > 30, "{e:?}");
        assert!(a.take_dirty().is_none(), "nothing on the coverage atlas");
        let (y, rows, px) = a.take_color_dirty().unwrap();
        assert_eq!((y, rows, px.len()), (0, e.h, (e.h * COLOR_W * 4) as usize));
        assert!(px.chunks(4).any(|p| p[3] > 200 && p[0] > 200 && p[2] < 120), "yellow");
        assert_eq!(a.glyph(&f, 32.0, gid), Some(e), "cached");
    }
}
