//! Atrium PDF's engine (folium.md §7.1, §8): what the window needs from a
//! document, kept off the window's thread.
//!
//! - **Lazy pages.** Opening reads the page tree and each page's size,
//!   nothing else; a page is interpreted the first time a tile of it is
//!   wanted.
//! - **Display lists, cached.** A page is interpreted once into a display
//!   list, which every tile and every zoom draws from; the most recent
//!   [`DL_CACHE`] are kept.
//! - **Tiles at the painted size.** A page is drawn in [`TILE`]-pixel
//!   tiles at the scale it is shown at, so a zoomed-in page costs only
//!   what the window shows; drawn tiles are kept under
//!   [`TILE_CACHE_BYTES`]. Images inside are decoded at the painted size by
//!   folium-render, under its own budget.
//! - **A worker pool with cancellation.** Tiles are drawn on
//!   [`workers`] threads in the order asked; asking again drops every
//!   queued tile no longer asked for, and a tile being drawn checks its
//!   flag every few hundred items and stops.
//!
//! Vector scene nodes for Fresco (§7.2) come later and replace the raster
//! tiles for pages that fit the node budget.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use folium_content::{interpret, DisplayList, Rect, Shared};
use folium_doc::Doc;
use folium_render::{Renderer, View};

/// Tile edge in device pixels.
pub const TILE: u32 = 512;
/// Drawn tiles kept, in bytes of RGBA.
pub const TILE_CACHE_BYTES: usize = 192 << 20;
/// Display lists kept.
pub const DL_CACHE: usize = 24;
/// Space around and between pages, in window points.
pub const MARGIN: f32 = 16.0;
pub const GAP: f32 = 12.0;
/// The largest device scale a tile is drawn at (px per PDF point): 6,400%
/// at 2x.
pub const SCALE_MAX: f64 = 128.0;

/// How the pages are fitted to the window.
pub use limen::pdf_viewer::Fit;

/// The worker threads: the cores less one for the window, at least one,
/// at most four.
pub fn workers() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get()).saturating_sub(1).clamp(1, 4)
}

/// Where the pages sit in the window's content, in points at zoom 1.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// Window points per PDF point.
    pub scale: f32,
    /// Each page's rect: x, y, w, h.
    pub rects: Vec<[f32; 4]>,
    pub width: f32,
    pub height: f32,
}

impl Layout {
    /// `sizes`: each page's shown size in PDF points (rotation and user
    /// unit applied); `view`: the window's size in points.
    pub fn new(sizes: &[(f64, f64)], fit: Fit, view: (f32, f32)) -> Layout {
        let max_w = sizes.iter().map(|s| s.0).fold(1.0, f64::max) as f32;
        let max_h = sizes.iter().map(|s| s.1).fold(1.0, f64::max) as f32;
        let fit_w = ((view.0 - 2.0 * MARGIN) / max_w).max(0.01);
        let scale = match fit {
            Fit::Width => fit_w,
            Fit::Page => fit_w.min(((view.1 - 2.0 * MARGIN) / max_h).max(0.01)),
            Fit::Percent { zoom_64 } => zoom_64 as f32 / 64.0,
        };
        let width = view.0.max(max_w * scale + 2.0 * MARGIN);
        let mut y = MARGIN;
        let rects = sizes
            .iter()
            .map(|&(w, h)| {
                let (w, h) = (w as f32 * scale, h as f32 * scale);
                let r = [(width - w) / 2.0, y, w, h];
                y += h + GAP;
                r
            })
            .collect();
        Layout { scale, rects, width, height: y - GAP + MARGIN }
    }

    /// The pages any part of which is between `y0` and `y1`.
    pub fn visible(&self, y0: f32, y1: f32) -> Range<usize> {
        let a = self.rects.partition_point(|r| r[1] + r[3] < y0);
        let b = self.rects.partition_point(|r| r[1] <= y1);
        a..b.max(a)
    }

    /// The page at `y` (the last one starting at or above it).
    pub fn page_at(&self, y: f32) -> usize {
        self.rects.partition_point(|r| r[1] <= y).saturating_sub(1)
    }
}

/// A tile: page, device scale in 1/256 px per point, column and row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub page: u32,
    pub scale_256: u32,
    pub col: u32,
    pub row: u32,
}

impl Key {
    pub fn scale(&self) -> f64 { self.scale_256 as f64 / 256.0 }
}

/// Quantize a device scale (px per PDF point) for tile keys.
pub fn scale_key(scale: f64) -> u32 { (scale.clamp(1.0 / 64.0, SCALE_MAX) * 256.0).round().max(1.0) as u32 }

/// The tiles of page `page` at device `scale` that meet `area` (device
/// px from the page's top left), nearest the middle of the area first.
pub fn tiles_for(sizes: &[(f64, f64)], page: usize, scale: f64, area: [f64; 4]) -> Vec<Key> {
    let Some(&(w, h)) = sizes.get(page) else { return vec![] };
    let sk = scale_key(scale);
    let s = sk as f64 / 256.0;
    let (pw, ph) = ((w * s).ceil(), (h * s).ceil());
    let t = TILE as f64;
    let (x0, y0) = (area[0].max(0.0), area[1].max(0.0));
    let (x1, y1) = ((area[0] + area[2]).min(pw), (area[1] + area[3]).min(ph));
    if x1 <= x0 || y1 <= y0 {
        return vec![];
    }
    let (c0, c1) = ((x0 / t) as u32, ((x1 / t).ceil() as u32).max(1));
    let (r0, r1) = ((y0 / t) as u32, ((y1 / t).ceil() as u32).max(1));
    let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let mut v: Vec<Key> = (r0..r1).flat_map(|row| (c0..c1).map(move |col| Key { page: page as u32, scale_256: sk, col, row })).collect();
    v.sort_by(|a, b| {
        let d = |k: &Key| ((k.col as f64 + 0.5) * t - mx).powi(2) + ((k.row as f64 + 0.5) * t - my).powi(2);
        d(a).total_cmp(&d(b))
    });
    v
}

/// A drawn tile: its pixels (opaque, so straight and premultiplied are the
/// same) and its size.
pub struct Tile {
    pub key: Key,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A page's display list, made once by whichever worker asks first.
type DlCell = Arc<OnceLock<Option<Arc<DisplayList>>>>;

struct Job {
    key: Key,
    cancel: Arc<AtomicBool>,
}

struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

struct Inner {
    doc: Doc,
    sizes: Vec<(f64, f64)>,
    shared: Shared,
    renderer: Renderer,
    dls: Mutex<(HashMap<u32, (DlCell, u64)>, u64)>,
    queue: Mutex<Queue>,
    wake: Condvar,
    /// Set when the engine goes: interpreting stops too.
    closed: AtomicBool,
}

/// Why a document didn't open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    NeedsPassword,
    Unreadable(String),
}

/// One open document and its workers.
pub struct Engine {
    inner: Arc<Inner>,
    done: Receiver<Option<Tile>>,
    pending: HashMap<Key, Arc<AtomicBool>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Engine {
    pub fn open(src: Box<dyn folium_syntax::Source>, password: &[u8]) -> Result<Engine, OpenError> {
        let doc = Doc::open(src, password).map_err(|e| match e {
            folium_syntax::Error::NeedsPassword => OpenError::NeedsPassword,
            e => OpenError::Unreadable(format!("{e:?}")),
        })?;
        // A page whose dictionary is damaged is shown at the first page's
        // size (or Letter), blank.
        let mut sizes = Vec::with_capacity(doc.page_count());
        for i in 0..doc.page_count() {
            let s = doc.page(i).ok().map(|p| p.size()).filter(|s| s.0 >= 1.0 && s.1 >= 1.0 && s.0 < 1e6 && s.1 < 1e6);
            sizes.push(s.unwrap_or_else(|| sizes.first().copied().unwrap_or((612.0, 792.0))));
        }
        let inner = Arc::new(Inner {
            doc,
            sizes,
            shared: Shared::new(),
            renderer: Renderer::new(),
            dls: Mutex::new((HashMap::new(), 0)),
            queue: Mutex::new(Queue { jobs: VecDeque::new(), closed: false }),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let (tx, done) = channel();
        let threads = (0..workers())
            .map(|n| {
                let (inner, tx) = (inner.clone(), tx.clone());
                std::thread::Builder::new().name(format!("folium-{n}")).spawn(move || work(&inner, &tx)).expect("worker thread")
            })
            .collect();
        Ok(Engine { inner, done, pending: HashMap::new(), threads })
    }

    pub fn page_count(&self) -> usize { self.inner.sizes.len() }
    pub fn sizes(&self) -> &[(f64, f64)] { &self.inner.sizes }
    pub fn title(&self) -> String { self.inner.doc.info().title.unwrap_or_default() }

    /// The tiles of page `page` at device `scale` that meet `area`.
    pub fn tiles_for(&self, page: usize, scale: f64, area: [f64; 4]) -> Vec<Key> { tiles_for(&self.inner.sizes, page, scale, area) }

    /// Ask for these tiles, most wanted first: queued tiles not among them
    /// are dropped and running ones told to stop; those already asked for
    /// stay where they are in the queue only if still wanted.
    pub fn want(&mut self, keys: &[Key]) {
        let wanted: std::collections::HashSet<Key> = keys.iter().copied().collect();
        self.pending.retain(|k, c| {
            let keep = wanted.contains(k);
            if !keep {
                c.store(true, Ordering::Relaxed);
            }
            keep
        });
        let mut q = self.inner.queue.lock().unwrap();
        q.jobs.retain(|j| !j.cancel.load(Ordering::Relaxed));
        let mut fresh = VecDeque::new();
        for &k in keys {
            // Queued already, or being drawn.
            if self.pending.contains_key(&k) {
                continue;
            }
            let cancel = Arc::new(AtomicBool::new(false));
            self.pending.insert(k, cancel.clone());
            fresh.push_back(Job { key: k, cancel });
        }
        // Newly wanted tiles go in the order asked, ahead of older ones
        // (those are still wanted but were asked for less recently).
        let rest = std::mem::take(&mut q.jobs);
        q.jobs = fresh;
        q.jobs.extend(rest);
        drop(q);
        self.inner.wake.notify_all();
    }

    /// Tiles drawn since the last call; `None` entries were stopped.
    pub fn drawn(&mut self) -> Vec<Tile> {
        let mut out = Vec::new();
        while let Ok(t) = self.done.try_recv() {
            if let Some(t) = t {
                if self.pending.remove(&t.key).is_some() {
                    out.push(t);
                }
            }
        }
        out
    }

    /// Wait up to `d` for a drawn tile (tests and the bench).
    pub fn wait(&mut self, d: std::time::Duration) -> Vec<Tile> {
        let mut out = Vec::new();
        if let Ok(Some(t)) = self.done.recv_timeout(d) {
            if self.pending.remove(&t.key).is_some() {
                out.push(t);
            }
        }
        out.extend(self.drawn());
        out
    }

    pub fn busy(&self) -> bool { !self.pending.is_empty() }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        for c in self.pending.values() {
            c.store(true, Ordering::Relaxed);
        }
        self.inner.queue.lock().unwrap().closed = true;
        self.inner.wake.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn work(inner: &Inner, tx: &Sender<Option<Tile>>) {
    loop {
        let job = {
            let mut q = inner.queue.lock().unwrap();
            loop {
                if q.closed {
                    return;
                }
                if let Some(j) = q.jobs.pop_front() {
                    break j;
                }
                q = inner.wake.wait(q).unwrap();
            }
        };
        if job.cancel.load(Ordering::Relaxed) {
            let _ = tx.send(None);
            continue;
        }
        let t = draw(inner, &job);
        if tx.send(t).is_err() {
            return;
        }
    }
}

/// The display list of page `n`, made by the first worker to ask.
fn display_list(inner: &Inner, n: u32) -> Option<Arc<DisplayList>> {
    let cell = {
        let mut g = inner.dls.lock().unwrap();
        let (map, tick) = &mut *g;
        *tick += 1;
        let t = *tick;
        let cell = map.entry(n).or_insert_with(|| (Arc::new(OnceLock::new()), 0));
        cell.1 = t;
        let cell = cell.0.clone();
        while map.len() > DL_CACHE {
            let old = map.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k).unwrap();
            map.remove(&old);
        }
        cell
    };
    cell.get_or_init(|| {
        let page = inner.doc.page(n as usize).ok()?;
        let contents = inner.doc.contents(&page).ok()?;
        let crop = Rect::new(page.crop_box.x0, page.crop_box.y0, page.crop_box.x1, page.crop_box.y1);
        let closed = || inner.closed.load(Ordering::Relaxed);
        match interpret(&inner.doc.syntax, &inner.shared, &contents, &page.resources, Some(crop), &closed) {
            Ok(dl) => Some(Arc::new(dl)),
            Err(e) => {
                eprintln!("atrium-pdf: page {}: {e:?}", n + 1);
                None
            }
        }
    })
    .clone()
}

fn draw(inner: &Inner, job: &Job) -> Option<Tile> {
    let k = job.key;
    let page = inner.doc.page(k.page as usize).ok();
    let (w, h) = inner.sizes[k.page as usize];
    let s = k.scale();
    let (pw, ph) = ((w * s).ceil() as u32, (h * s).ceil() as u32);
    let (x, y) = (k.col * TILE, k.row * TILE);
    if x >= pw || y >= ph {
        return None;
    }
    let (tw, th) = (TILE.min(pw - x), TILE.min(ph - y));
    let cancel = || job.cancel.load(Ordering::Relaxed) || inner.closed.load(Ordering::Relaxed);
    let rgba = match (page, display_list(inner, k.page)) {
        (Some(page), Some(dl)) if !cancel() => {
            let crop = Rect::new(page.crop_box.x0, page.crop_box.y0, page.crop_box.x1, page.crop_box.y1);
            let view = View { crop, rotate: page.rotate, scale: s * page.user_unit };
            let (pm, _) = inner.renderer.render(&inner.doc.syntax, &dl, &view, (x as i32, y as i32, tw, th), &cancel);
            pm.take()
        }
        // Damaged or empty: a white tile.
        (_, None) | (None, _) => vec![255; (tw * th * 4) as usize],
        _ => return None,
    };
    if cancel() {
        return None;
    }
    Some(Tile { key: k, width: tw, height: th, rgba })
}

/// Drawn tiles kept by the window, under [`TILE_CACHE_BYTES`], least
/// recently shown first out.
pub struct TileCache<I> {
    map: HashMap<Key, (I, usize, u64)>,
    bytes: usize,
    tick: u64,
    budget: usize,
}

impl<I: Clone> TileCache<I> {
    pub fn new(budget: usize) -> Self { TileCache { map: HashMap::new(), bytes: 0, tick: 0, budget } }

    pub fn insert(&mut self, k: Key, image: I, bytes: usize) {
        self.tick += 1;
        if let Some((_, b, _)) = self.map.insert(k, (image, bytes, self.tick)) {
            self.bytes -= b;
        }
        self.bytes += bytes;
        while self.bytes > self.budget && self.map.len() > 1 {
            let old = self.map.iter().min_by_key(|(_, (_, _, t))| *t).map(|(k, _)| *k).unwrap();
            let (_, b, _) = self.map.remove(&old).unwrap();
            self.bytes -= b;
        }
    }

    /// The tile, marked as just shown.
    pub fn get(&mut self, k: &Key) -> Option<I> {
        self.tick += 1;
        let t = self.tick;
        self.map.get_mut(k).map(|e| {
            e.2 = t;
            e.0.clone()
        })
    }

    pub fn contains(&self, k: &Key) -> bool { self.map.contains_key(k) }

    /// Tiles of `page` at other scales than `scale_256`, the nearest scale
    /// last (to be drawn under the current ones while those come).
    pub fn stand_ins(&self, page: u32, scale_256: u32) -> Vec<(Key, I)> {
        let mut v: Vec<(Key, I)> = self.map.iter().filter(|(k, _)| k.page == page && k.scale_256 != scale_256).map(|(k, e)| (*k, e.0.clone())).collect();
        v.sort_by_key(|(k, _)| std::cmp::Reverse((k.scale_256 as i64 - scale_256 as i64).abs()));
        v
    }

    pub fn bytes(&self) -> usize { self.bytes }
    pub fn len(&self) -> usize { self.map.len() }
    pub fn is_empty(&self) -> bool { self.map.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_fits_and_finds_pages() {
        let sizes = vec![(612.0, 792.0), (792.0, 612.0), (612.0, 792.0)];
        let l = Layout::new(&sizes, Fit::Width, (824.0, 600.0));
        assert!((l.scale - 1.0).abs() < 1e-6, "{}", l.scale);
        assert_eq!(l.rects[0], [(824.0 - 612.0) / 2.0, MARGIN, 612.0, 792.0]);
        assert_eq!(l.rects[1][1], MARGIN + 792.0 + GAP);
        assert_eq!(l.visible(0.0, 100.0), 0..1);
        assert_eq!(l.visible(800.0, 820.0), 0..2);
        assert_eq!(l.visible(1e6, 2e6), 3..3);
        assert_eq!(l.page_at(900.0), 1);
        let p = Layout::new(&sizes, Fit::Page, (824.0, 424.0));
        assert!((p.scale - 392.0 / 792.0).abs() < 1e-6, "{}", p.scale);
        let z = Layout::new(&sizes, Fit::Percent { zoom_64: 128 }, (300.0, 300.0));
        assert_eq!(z.scale, 2.0);
        assert!(z.width > 300.0, "wider than the window scrolls across");
    }

    #[test]
    fn tile_cache_keeps_budget() {
        let mut c = TileCache::new(100);
        let k = |n| Key { page: n, scale_256: 256, col: 0, row: 0 };
        for n in 0..5 {
            c.insert(k(n), n, 40);
        }
        assert_eq!(c.len(), 2);
        assert!(c.bytes() <= 100);
        assert!(c.contains(&k(4)) && c.contains(&k(3)));
        c.insert(Key { scale_256: 512, ..k(4) }, 9, 10);
        assert_eq!(c.stand_ins(4, 512).len(), 1);
    }

    /// A small two-page PDF written here.
    fn pdf() -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let content = b"0 0 1 rg 100 100 200 200 re f";
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 400 400] >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << >> >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << >> /MediaBox [0 0 800 400] >>".to_vec(),
            [format!("<< /Length {} >>\nstream\n", content.len()).into_bytes(), content.to_vec(), b"\nendstream".to_vec()].concat(),
        ];
        let mut offs = vec![];
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend(format!("{} 0 obj\n", i + 1).bytes());
            out.extend(o);
            out.extend(b"\nendobj\n");
        }
        let x = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for o in offs {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).bytes());
        out
    }

    #[test]
    fn engine_draws_tiles_and_drops_unwanted() {
        let mut e = Engine::open(Box::new(folium_syntax::Bytes(pdf().into())), b"").unwrap();
        assert_eq!(e.page_count(), 2);
        assert_eq!(e.sizes()[1], (800.0, 400.0));
        let keys = e.tiles_for(0, 2.0, [0.0, 0.0, 800.0, 800.0]);
        assert_eq!(keys.len(), 4, "800 px square in 512 px tiles");
        // Ask for page 1's tiles, then change our mind before they're all drawn.
        let far = e.tiles_for(1, 4.0, [0.0, 0.0, 3200.0, 1600.0]);
        e.want(&far);
        e.want(&keys);
        let mut got = vec![];
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while e.busy() && std::time::Instant::now() < until {
            got.extend(e.wait(std::time::Duration::from_millis(100)));
        }
        assert!(!e.busy());
        assert!(got.iter().all(|t| t.key.page == 0), "dropped tiles are not delivered");
        assert_eq!(got.len(), 4);
        // The blue square (100..300 pt from the bottom left; page 400 pt
        // high) at 2 px/pt: device (200..600, 200..600); tile (0,0) holds
        // its top-left part from (200, 200).
        let t = got.iter().find(|t| t.key.col == 0 && t.key.row == 0).unwrap();
        let px = |x: u32, y: u32| &t.rgba[((y * t.width + x) * 4) as usize..][..4];
        assert_eq!(px(10, 10), [255, 255, 255, 255]);
        assert_eq!(px(300, 300), [0, 0, 255, 255]);
    }
}
pub mod sanitized;
