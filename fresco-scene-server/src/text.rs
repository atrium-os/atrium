//! Server-side text: font registry + lazy R8 atlas + shaping.
//!
//! M6.3 moves the text stack from per-app to per-server. Apps no
//! longer link rustybuzz / swash, ship font files, or build their
//! own atlases. They `OP_FONT_OPEN` a font by name and
//! `OP_TEXT_RUN_INSTALL` runs by passing a UTF-8 string; the server
//! shapes, rasterizes lazily into a per-(font, size) atlas, and
//! emits the equivalent `GlyphRunParams` into the target window's
//! scene state.
//!
//! Atlas growth model: each (font_id, size) gets one R8 atlas slot
//! at a fixed reserved id. The atlas is grown by re-uploading whole;
//! steady state has no growth (all printable codepoints land on first
//! exposure). Future M6.4 will add eviction + page splits.
//!
//! Slot id allocation: server-managed slots live in the reserved
//! range `0xF000_0000..=0xFFFF_FFFF`. Client slot ids stay below.
//!
//! Font search path: `/usr/local/share/fonts/`, then
//! `$ATRIUM_FONT_PATH` (colon-separated). The POC ships a tiny
//! built-in registry that maps `"DejaVuSansMono"` and `"system-mono"`
//! to a single discoverable TTF.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use fresco_protocol::{GlyphInstance, GlyphRunParams};
use fresco_text::{shape_and_rasterize, GlyphAtlas};

/// Reserved slot-id range owned by the server's text stack. Client
/// slot ids must stay below this floor; the atlas manager allocates
/// upward from `SERVER_SLOT_BASE`.
pub const SERVER_SLOT_BASE: u32 = 0xF000_0000;

/// Atlas page dimensions. 1024² R8 = 1 MiB per (font, size); enough
/// for ~600 64-px glyphs which covers Latin + Greek + Cyrillic for one
/// font at one size. CJK / emoji will need page splits (M6.4).
const ATLAS_W: u32 = 1024;
const ATLAS_H: u32 = 1024;

#[derive(Debug)]
pub struct FontMetrics {
    pub units_per_em: u32,
    pub ascent_units: i32,
    pub descent_units: i32,
    /// Per-glyph advance in font-design units for monospace fonts;
    /// 0 if the font is proportional. Detected by comparing the
    /// advances of `M`, `i`, `.` — equal => monospace.
    pub mono_advance_units: i32,
}

struct LoadedFont {
    name:     String,
    bytes:    Vec<u8>,
    metrics:  FontMetrics,
    refcount: u32,
}

#[derive(Debug, Clone, Copy)]
struct GlyphCacheEntry {
    atlas_u: u32,
    atlas_v: u32,
    atlas_w: u32,
    atlas_h: u32,
    /// `dx0` from fresco-text — pen-position offset (`pen_x +
    /// glyph_left`). With single-codepoint shaping this collapses to
    /// the glyph's left bearing because `pen_x = 0`.
    bearing_x: f32,
    /// `-dy0` — baseline-to-top, in pixels (positive when the glyph
    /// extends above the baseline).
    bearing_y: f32,
    /// Pen-advance for this glyph in pixels. For monospace fonts this
    /// is the font's `cell_w`; for proportional fonts it varies.
    advance:   f32,
    /// Logical-clock LRU stamp: bumped on every cache hit. Eviction
    /// drops the lowest stamps when the page can't fit a new glyph.
    last_used: u64,
}

struct AtlasPage {
    pixels:  Vec<u8>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    /// `(font_id, size_round, codepoint) → entry`. Keyed by integer
    /// size (px*100 rounded) to dedupe near-equal float sizes.
    glyphs:  HashMap<(u32, u32, u32), GlyphCacheEntry>,
    /// Server slot id this page is bound to.
    slot_id: u32,
    /// Whether the slot's GPU image has been allocated yet. False
    /// until the first PendingAtlasUpload::Full has been drained;
    /// after that, partial-region uploads suffice.
    allocated_on_gpu: bool,
    /// Bbox of pixels modified since the last drain. None means no
    /// changes pending. After a drain the bbox resets.
    dirty_bbox: Option<(u32, u32, u32, u32)>,
    /// Set after `evict_and_compact` rewrites the entire pixels
    /// buffer; forces the next drain to ship a Full upload (every
    /// glyph moved on the GPU side too).
    needs_full_resync: bool,
}

impl AtlasPage {
    fn new(slot_id: u32) -> Self {
        Self {
            pixels:  vec![0u8; (ATLAS_W * ATLAS_H) as usize],
            shelf_x: 1, shelf_y: 1, shelf_h: 0,
            glyphs:  HashMap::new(),
            slot_id,
            allocated_on_gpu: false,
            dirty_bbox: None,
            needs_full_resync: false,
        }
    }

    /// Drop the bottom `evict_pct` of the LRU and re-shelf-pack the
    /// survivors into a freshly-zeroed page. Survivor pixels are
    /// copied from the old atlas *into a side buffer* before we wipe
    /// `pixels`, then written back into the new packing — no
    /// re-rasterization, no per-glyph raster cache.
    ///
    /// Marks the page as needing a Full upload on next drain (every
    /// glyph moved on the GPU side too).
    fn evict_and_compact(&mut self, evict_pct: u32) {
        type GKey = (u32, u32, u32);
        let mut by_lru: Vec<(&GKey, &GlyphCacheEntry)> =
            self.glyphs.iter().collect();
        by_lru.sort_by(|a, b| b.1.last_used.cmp(&a.1.last_used)); /* newest first */
        let keep_n = ((by_lru.len() as u64
                       * (100 - evict_pct as u64) + 99) / 100) as usize;

        /* Capture survivor pixel blocks from the existing atlas
         * before we wipe it. (key, entry, pixel_bytes_or_empty). */
        let mut survivors: Vec<(GKey, GlyphCacheEntry, Vec<u8>)> =
            Vec::with_capacity(keep_n);
        for (k, e) in by_lru.into_iter().take(keep_n) {
            let bytes = if e.atlas_w == 0 || e.atlas_h == 0 {
                Vec::new()
            } else {
                let mut b = Vec::with_capacity((e.atlas_w * e.atlas_h) as usize);
                for row in 0..e.atlas_h {
                    let s = ((e.atlas_v + row) * ATLAS_W + e.atlas_u) as usize;
                    b.extend_from_slice(&self.pixels[s..s + e.atlas_w as usize]);
                }
                b
            };
            survivors.push((*k, *e, bytes));
        }
        let dropped = self.glyphs.len() - survivors.len();

        for px in &mut self.pixels { *px = 0; }
        self.shelf_x = 1; self.shelf_y = 1; self.shelf_h = 0;
        let mut new_map: HashMap<GKey, GlyphCacheEntry> =
            HashMap::with_capacity(survivors.len());

        for (key, mut e, src) in survivors {
            let gw = e.atlas_w; let gh = e.atlas_h;
            if gw == 0 || gh == 0 {
                new_map.insert(key, e);
                continue;
            }
            if self.shelf_x + gw + 1 > ATLAS_W {
                self.shelf_x = 1;
                self.shelf_y += self.shelf_h + 1;
                self.shelf_h = 0;
            }
            if self.shelf_y + gh + 1 > ATLAS_H { break; }
            if gh > self.shelf_h { self.shelf_h = gh; }
            for row in 0..gh {
                let dst = ((self.shelf_y + row) * ATLAS_W
                           + self.shelf_x) as usize;
                let s = (row * gw) as usize;
                self.pixels[dst..dst + gw as usize]
                    .copy_from_slice(&src[s..s + gw as usize]);
            }
            e.atlas_u = self.shelf_x;
            e.atlas_v = self.shelf_y;
            new_map.insert(key, e);
            self.shelf_x += gw + 1;
        }

        self.glyphs = new_map;
        self.needs_full_resync = true;
        self.dirty_bbox = Some((0, 0, ATLAS_W, ATLAS_H));
        log::info!("evicted {dropped} glyph(s) from atlas slot={}; \
                    {} survivors repacked", self.slot_id, self.glyphs.len());
    }

    fn mark_dirty(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let (x1, y1) = (x + w, y + h);
        self.dirty_bbox = Some(match self.dirty_bbox {
            None => (x, y, x1, y1),
            Some((px0, py0, px1, py1)) =>
                (px0.min(x), py0.min(y), px1.max(x1), py1.max(y1)),
        });
    }

    /// Slice the dirty bbox out of `pixels` into a tightly-packed
    /// (no row stride) buffer ready for `upload_texture_region`.
    /// Returns `(dst_x, dst_y, w, h, bytes)`.
    fn extract_dirty(&self) -> Option<(u32, u32, u32, u32, Vec<u8>)> {
        let (x0, y0, x1, y1) = self.dirty_bbox?;
        let w = x1 - x0;
        let h = y1 - y0;
        let mut out = Vec::with_capacity((w * h) as usize);
        for row in y0..y1 {
            let start = (row * ATLAS_W + x0) as usize;
            let end   = start + w as usize;
            out.extend_from_slice(&self.pixels[start..end]);
        }
        Some((x0, y0, w, h, out))
    }

    /// Rasterize `codepoint` from `font_bytes` at `size_px` and pack
    /// into the atlas, returning the cache entry. Idempotent — repeat
    /// calls for the same triple return the cached entry without
    /// re-rasterizing.
    fn ensure(
        &mut self,
        font_id: u32, size_px: f32, codepoint: u32,
        font_bytes: &[u8],
        clock: u64,
    ) -> Option<GlyphCacheEntry> {
        let size_key = (size_px * 100.0) as u32;
        let key = (font_id, size_key, codepoint);
        if let Some(e) = self.glyphs.get_mut(&key) {
            e.last_used = clock;
            return Some(*e);
        }

        let s = char::from_u32(codepoint)?.to_string();
        let atlas: GlyphAtlas = shape_and_rasterize(font_bytes, &s, size_px).ok()?;
        /* Whitespace (space, tab) shapes to a positive advance with no
         * rasterized glyph — `atlas.glyphs` is empty. Still cache the
         * advance so the layout loop progresses correctly. */
        let Some(&q) = atlas.glyphs.first() else {
            let entry = GlyphCacheEntry {
                atlas_u: 0, atlas_v: 0, atlas_w: 0, atlas_h: 0,
                bearing_x: 0.0, bearing_y: 0.0,
                advance: atlas.advance,
                last_used: clock,
            };
            self.glyphs.insert(key, entry);
            return Some(entry);
        };
        let su0 = (q.u0 * atlas.width  as f32).round() as u32;
        let sv0 = (q.v0 * atlas.height as f32).round() as u32;
        let su1 = (q.u1 * atlas.width  as f32).round() as u32;
        let sv1 = (q.v1 * atlas.height as f32).round() as u32;
        let gw = su1.saturating_sub(su0);
        let gh = sv1.saturating_sub(sv0);
        if gw == 0 || gh == 0 {
            /* Whitespace / no-render glyph. Cache an empty entry with
             * just the advance so the layout loop still progresses. */
            let entry = GlyphCacheEntry {
                atlas_u: 0, atlas_v: 0, atlas_w: 0, atlas_h: 0,
                bearing_x: 0.0, bearing_y: 0.0,
                advance: atlas.advance,
                last_used: clock,
            };
            self.glyphs.insert(key, entry);
            return Some(entry);
        }

        if self.shelf_x + gw + 1 > ATLAS_W {
            self.shelf_x = 1;
            self.shelf_y += self.shelf_h + 1;
            self.shelf_h = 0;
        }
        if self.shelf_y + gh + 1 > ATLAS_H {
            /* Page exhausted — evict the oldest 25% of glyphs by LRU
             * stamp, repack, and retry. The new glyph itself is the
             * most-recently-used (clock just bumped before this call)
             * so it can't be evicted by its own miss. */
            self.evict_and_compact(25);
            if self.shelf_x + gw + 1 > ATLAS_W {
                self.shelf_x = 1;
                self.shelf_y += self.shelf_h + 1;
                self.shelf_h = 0;
            }
            if self.shelf_y + gh + 1 > ATLAS_H {
                log::warn!("atlas page slot={} still doesn't fit U+{:04X} \
                            after eviction; dropping",
                           self.slot_id, codepoint);
                return None;
            }
        }
        if gh > self.shelf_h { self.shelf_h = gh; }

        for row in 0..gh {
            for col in 0..gw {
                let src = (((sv0 + row) * atlas.width + (su0 + col)) * 4) as usize;
                let dst = ((self.shelf_y + row) * ATLAS_W
                           + (self.shelf_x + col)) as usize;
                self.pixels[dst] = atlas.pixels[src + 3];
            }
        }

        let entry = GlyphCacheEntry {
            atlas_u: self.shelf_x,
            atlas_v: self.shelf_y,
            atlas_w: gw,
            atlas_h: gh,
            bearing_x: q.dx0,
            bearing_y: -q.dy0,
            advance: atlas.advance,
            last_used: clock,
        };
        self.glyphs.insert(key, entry);
        self.mark_dirty(self.shelf_x, self.shelf_y, gw, gh);
        self.shelf_x += gw + 1;
        Some(entry)
    }
}

/// Pending atlas upload generated by a `shape_text_run` call. The
/// first upload for a (font, size) page is `Full` — it allocates the
/// GPU image and seeds it. Subsequent uploads are `Region` — patch
/// just the rectangle that grew since the last drain.
#[derive(Debug)]
pub enum PendingAtlasUpload {
    Full {
        slot_id: u32,
        width:   u32,
        height:  u32,
        pixels:  Vec<u8>,
    },
    Region {
        slot_id: u32,
        dst_x:   u32,
        dst_y:   u32,
        width:   u32,
        height:  u32,
        pixels:  Vec<u8>,
    },
}

pub struct TextEngine {
    next_font_id: u32,
    next_slot_id: u32,
    fonts:  HashMap<u32, LoadedFont>,
    /// `(font_id, size_key) → AtlasPage`. Each (font, size) gets
    /// its own page; future M6.4 will share pages across sizes via
    /// signed-distance-field rendering.
    pages:  HashMap<(u32, u32), AtlasPage>,
    search_paths: Vec<PathBuf>,
    /// Monotonic LRU clock. Each `ensure()` call bumps it; entries
    /// stamp `last_used` from this. Eviction sorts by `last_used`.
    lru_clock: u64,
}

impl TextEngine {
    pub fn new() -> Self {
        let mut paths: Vec<PathBuf> = vec![
            PathBuf::from("/usr/local/share/fonts"),
            PathBuf::from("/usr/local/share/fonts/ibm-plex"),
            PathBuf::from("/usr/local/share/fonts/dejavu"),
            PathBuf::from("/mnt/host/test-assets"),
        ];
        if let Ok(extra) = std::env::var("ATRIUM_FONT_PATH") {
            paths.extend(extra.split(':').map(PathBuf::from));
        }
        Self {
            next_font_id: 1,
            next_slot_id: SERVER_SLOT_BASE,
            fonts: HashMap::new(),
            pages: HashMap::new(),
            search_paths: paths,
            lru_clock: 0,
        }
    }

    /// Resolve `name` against the server's font registry + search
    /// path. Returns `(font_id, metrics)`. `font_id == 0` means
    /// not found.
    pub fn open(&mut self, name: &str) -> Option<(u32, FontMetrics)> {
        /* Existing-font dedup: bump refcount, reuse id. */
        if let Some((&fid, _)) = self.fonts.iter().find(|(_, f)| f.name == name) {
            self.fonts.get_mut(&fid).unwrap().refcount += 1;
            let m = &self.fonts[&fid].metrics;
            return Some((fid, FontMetrics {
                units_per_em: m.units_per_em,
                ascent_units: m.ascent_units,
                descent_units: m.descent_units,
                mono_advance_units: m.mono_advance_units,
            }));
        }

        let bytes = self.read_font(name)?;
        let face  = ttf_parser::Face::parse(&bytes, 0).ok()?;
        /* Monospace detection: compare advances of three glyphs with
         * very different widths in proportional fonts (M, i, .). If
         * all three match, treat as monospace and report the advance.
         * Falls back to 0 (= "proportional") if any glyph is missing. */
        let advance_for = |c: char| -> Option<i32> {
            let gid = face.glyph_index(c)?;
            face.glyph_hor_advance(gid).map(|a| a as i32)
        };
        let mono_advance_units = match (advance_for('M'), advance_for('i'), advance_for('.')) {
            (Some(m), Some(i), Some(d)) if m == i && i == d => m,
            _ => 0,
        };
        let metrics = FontMetrics {
            units_per_em: face.units_per_em() as u32,
            ascent_units: face.ascender() as i32,
            descent_units: face.descender() as i32,
            mono_advance_units,
        };
        let fid = self.next_font_id;
        self.next_font_id += 1;
        self.fonts.insert(fid, LoadedFont {
            name: name.to_string(),
            bytes,
            metrics: FontMetrics {
                units_per_em: metrics.units_per_em,
                ascent_units: metrics.ascent_units,
                descent_units: metrics.descent_units,
                mono_advance_units: metrics.mono_advance_units,
            },
            refcount: 1,
        });
        log::info!("font {} '{}': units_per_em={}", fid, name, metrics.units_per_em);
        Some((fid, metrics))
    }

    pub fn close(&mut self, font_id: u32) {
        let mut should_drop = false;
        if let Some(f) = self.fonts.get_mut(&font_id) {
            f.refcount = f.refcount.saturating_sub(1);
            if f.refcount == 0 { should_drop = true; }
        }
        if should_drop {
            /* Drop pages owned by this font too. The slots they
             * occupy stay bound until the next text install reclaims
             * them; a future M6.4 will explicitly free. */
            self.pages.retain(|(fid, _), _| *fid != font_id);
            self.fonts.remove(&font_id);
            log::info!("font {} dropped", font_id);
        }
    }

    /// Shape `text` with `font_id` at `size_px`, ensure every glyph
    /// is in the atlas, and return a fully-formed `GlyphRunParams`
    /// the caller can install into the per-window scene state.
    /// Also returns a `PendingAtlasUpload` if the atlas grew during
    /// this call (caller is responsible for handing it to the GPU
    /// upload pump before the next render).
    pub fn shape_text_run(
        &mut self,
        font_id: u32,
        size_px: f32,
        x: f32, y: f32,
        color: [f32; 4],
        text: &str,
    ) -> Option<(GlyphRunParams, Option<PendingAtlasUpload>)> {
        /* Wire convention: `y` is the top of the text em-box (so apps
         * compute it as e.g. `field_y + (field_h - text_h)/2` for
         * vertical centering, without needing font metrics). The
         * compute kernel expects `origin.y` to be the baseline (it
         * does `origin.y - bearing.y` to derive each glyph's top).
         * Add the font's ascender here so the on-wire bbox-top
         * convention resolves to a baseline before reaching the GPU. */
        let font = self.fonts.get(&font_id)?;
        let upe = font.metrics.units_per_em as f32;
        let ascender_px =
            font.metrics.ascent_units as f32 * size_px / upe;
        let y = y + ascender_px;
        let font_bytes = font.bytes.clone();

        let size_key = (size_px * 100.0) as u32;
        let page_key = (font_id, size_key);
        let slot_id = if let Some(p) = self.pages.get(&page_key) {
            p.slot_id
        } else {
            let id = self.next_slot_id;
            self.next_slot_id += 1;
            self.pages.insert(page_key, AtlasPage::new(id));
            id
        };
        let mut clock = self.lru_clock;
        let page = self.pages.get_mut(&page_key).unwrap();

        /* Walk codepoints in text — POC shaping = one glyph per char
         * with monospace pen advance using the font's per-glyph
         * advance from rustybuzz. Real shaping (ligatures, kerning,
         * complex scripts) lands when we move from char-by-char to
         * shape_and_rasterize on the whole string at once + cache by
         * (font, size, glyph_id). For now this matches what the
         * client-side MonoAtlas did. */
        let mut glyphs: Vec<GlyphInstance> = Vec::with_capacity(text.len());
        let mut pen_x: f32 = 0.0;
        for ch in text.chars() {
            let cp = ch as u32;
            /* A text run is a single shaped baseline. Line breaks,
             * tabs, and other C0/C1 control codes have no defined
             * meaning here — apps split into multiple runs and
             * compute tab stops themselves. Skip silently rather
             * than let the shaper emit a `.notdef` box. */
            if cp < 0x20 || cp == 0x7f { continue; }
            clock += 1;
            let entry = match page.ensure(font_id, size_px, cp, &font_bytes, clock) {
                Some(e) => e,
                None    => continue,
            };
            if entry.atlas_w > 0 && entry.atlas_h > 0 {
                glyphs.push(GlyphInstance {
                    dx: pen_x,
                    dy: 0.0,
                    atlas_u: entry.atlas_u,
                    atlas_v: entry.atlas_v,
                    atlas_w: entry.atlas_w,
                    atlas_h: entry.atlas_h,
                    bearing_x: entry.bearing_x,
                    bearing_y: entry.bearing_y,
                });
            }
            pen_x += entry.advance;
        }
        self.lru_clock = clock;
        let page = self.pages.get_mut(&page_key).unwrap();

        let pending = if page.dirty_bbox.is_some() {
            if !page.allocated_on_gpu || page.needs_full_resync {
                /* First upload for this slot — or a post-eviction
                 * resync where every glyph moved on the GPU side.
                 * Either way ship the whole atlas. Happens exactly
                 * once per (font, size) per process lifetime in the
                 * common case (no eviction). */
                page.allocated_on_gpu = true;
                page.needs_full_resync = false;
                page.dirty_bbox = None;
                Some(PendingAtlasUpload::Full {
                    slot_id: page.slot_id,
                    width:   ATLAS_W,
                    height:  ATLAS_H,
                    pixels:  page.pixels.clone(),
                })
            } else {
                let (dst_x, dst_y, w, h, bytes) = page.extract_dirty().unwrap();
                page.dirty_bbox = None;
                Some(PendingAtlasUpload::Region {
                    slot_id: page.slot_id,
                    dst_x, dst_y,
                    width:  w, height: h,
                    pixels: bytes,
                })
            }
        } else { None };

        let run = GlyphRunParams {
            x, y,
            atlas_slot_id: slot_id,
            atlas_width:   ATLAS_W,
            atlas_height:  ATLAS_H,
            r: color[0], g: color[1], b: color[2], a: color[3],
            glyphs,
        };
        Some((run, pending))
    }

    /// Shape `text` with `font_id` at `size_px` and return the run's
    /// pixel-space metrics. Reuses `shape_text_run`'s machinery — the
    /// caching side-effect is intentional, so a measure followed by an
    /// install in the same frame doesn't pay double.
    pub fn measure(
        &mut self,
        font_id: u32,
        size_px: f32,
        text: &str,
    ) -> Option<(f32, f32, f32)> {
        let f = self.fonts.get(&font_id)?;
        let upe = f.metrics.units_per_em as f32;
        let ascent_px  = f.metrics.ascent_units  as f32 * size_px / upe;
        let descent_px = -(f.metrics.descent_units as f32) * size_px / upe;

        let font_bytes = f.bytes.clone();
        let size_key = (size_px * 100.0) as u32;
        let page_key = (font_id, size_key);
        if !self.pages.contains_key(&page_key) {
            let id = self.next_slot_id;
            self.next_slot_id += 1;
            self.pages.insert(page_key, AtlasPage::new(id));
        }
        let mut clock = self.lru_clock;
        let page = self.pages.get_mut(&page_key).unwrap();
        let mut width = 0.0_f32;
        for ch in text.chars() {
            let cp = ch as u32;
            if cp < 0x20 || cp == 0x7f { continue; }
            clock += 1;
            let entry = match page.ensure(font_id, size_px, cp, &font_bytes, clock) {
                Some(e) => e,
                None    => continue,
            };
            width += entry.advance;
        }
        self.lru_clock = clock;
        Some((width, ascent_px, descent_px))
    }

    fn read_font(&self, name: &str) -> Option<Vec<u8>> {
        /* Built-in name aliases first. */
        let candidates: &[&str] = match name {
            // Atrium system faces = IBM Plex (visual-language §2); DejaVu kept as a
            // fallback so a system without Plex installed still renders text.
            "IBMPlexMono" | "DejaVuSansMono" | "system-mono" =>
                &["IBMPlexMono-Regular.ttf", "DejaVuSansMono.ttf", "DejaVuSansMono-Bold.ttf"],
            "IBMPlexSans" | "DejaVuSans" | "system-sans" =>
                &["IBMPlexSans.ttf", "DejaVuSans.ttf"],
            "DejaVuSerif" | "system-serif" =>
                &["DejaVuSerif.ttf"],
            other => {
                /* Fall through to literal-name lookup so callers can
                 * pass a basename like "MyFont.ttf" directly. */
                return self.read_font_file(Path::new(other));
            }
        };
        for c in candidates {
            for dir in &self.search_paths {
                let p = dir.join(c);
                if let Some(b) = self.read_font_file(&p) {
                    log::info!("font '{}' resolved to {}", name, p.display());
                    return Some(b);
                }
            }
        }
        None
    }

    fn read_font_file(&self, p: &Path) -> Option<Vec<u8>> {
        if !p.is_file() { return None; }
        std::fs::read(p).ok()
    }
}

impl Default for TextEngine {
    fn default() -> Self { Self::new() }
}

/// `Arc<RwLock<TextEngine>>` shared between EnvelopeFrontend and
/// the dispatcher.
pub type SharedTextEngine = Arc<RwLock<TextEngine>>;
