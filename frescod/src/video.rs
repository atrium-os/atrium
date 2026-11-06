//! Video layers as frescod composes them (docs/spec/fresco-video.md §3–4).
//!
//! Frames arrive already converted to BGRA, at the size of the part of the
//! layer inside its window: from frescod's own frontend in direct mode
//! ([`Direct`]), or copied out of a renderer's video slots by the split
//! broker. Each refresh takes, for every layer, the latest frame due by
//! then, and composes it as a surface of its own just under its window.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::pack::Pixels;

/// Surface ids of video layers start here, clear of window ids (u16) and
/// of the chrome's.
pub const FIRST_ID: u32 = 0x1_0000;

/// Frames a layer may have queued; past that the oldest is dropped.
const MAX_QUEUED: usize = 8;

struct Layer {
    /// Its surface's id.
    id: u32,
    /// Where it shows, `[x, y, w, h]` in its window's pixels; `None`: hidden.
    rect: Option<[i32; 4]>,
    /// Frames waiting for their time: (pixels, seq, present_ns).
    queue: VecDeque<(Arc<Pixels>, u32, u64)>,
    /// The frame on screen.
    current: Option<Arc<Pixels>>,
    /// Frames passed over since the last one shown.
    dropped: u32,
}

/// One video surface to compose this refresh.
pub struct Shown {
    pub id: u32,
    /// Its pixels' position in the window.
    pub at: (i32, i32),
    pub pixels: Arc<Pixels>,
}

#[derive(Default)]
pub struct Videos {
    /// By (global window id, layer id).
    layers: Mutex<HashMap<(u32, u32), Layer>>,
}

/// Every layer frescod composes.
pub static VIDEOS: LazyLock<Videos> = LazyLock::new(Videos::default);

static GEN: AtomicU64 = AtomicU64::new(1);
static NEXT_ID: AtomicU64 = AtomicU64::new(FIRST_ID as u64);

/// Now on `CLOCK_MONOTONIC`, the clock `present_ns` is on.
pub fn monotonic_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: one timespec of ours for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    (t.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(t.tv_nsec as u64)
}

/// Pixels for a frame, with a gen of their own.
pub fn pixels(width: u32, height: u32, px: Vec<u32>, seq: u32) -> Arc<Pixels> {
    Arc::new(Pixels { gen: GEN.fetch_add(1, Ordering::Relaxed), width, height, px, seq, since: Vec::new() })
}

impl Videos {
    fn layer<'a>(m: &'a mut HashMap<(u32, u32), Layer>, key: (u32, u32)) -> &'a mut Layer {
        m.entry(key).or_insert_with(|| {
            // Wraps after 2^32 layers; ids stay above window ids.
            let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let id = FIRST_ID | (n as u32 & 0x7fff_ffff);
            Layer { id, rect: None, queue: VecDeque::new(), current: None, dropped: 0 }
        })
    }

    pub fn place(&self, window: u32, layer: u32, rect: Option<[i32; 4]>) {
        let mut m = self.layers.lock().unwrap();
        let l = Self::layer(&mut m, (window, layer));
        l.rect = rect;
        if rect.is_none() {
            l.dropped += l.queue.len() as u32;
            l.queue.clear();
            l.current = None;
        }
    }

    /// Queue a frame, to show at `present_ns` (0: at once).
    pub fn queue(&self, window: u32, layer: u32, px: Arc<Pixels>, seq: u32, present_ns: u64) {
        let mut m = self.layers.lock().unwrap();
        let l = Self::layer(&mut m, (window, layer));
        if l.rect.is_none() {
            l.dropped += 1;
            return;
        }
        if l.queue.len() >= MAX_QUEUED {
            l.queue.pop_front();
            l.dropped += 1;
        }
        l.queue.push_back((px, seq, present_ns));
    }

    pub fn destroy(&self, window: u32, layer: u32) {
        self.layers.lock().unwrap().remove(&(window, layer));
    }

    pub fn forget_window(&self, window: u32) {
        self.layers.lock().unwrap().retain(|&(w, _), _| w != window);
    }

    /// Whether any layer has a frame waiting, so the frame loop knows to
    /// come back.
    pub fn pending(&self) -> bool {
        self.layers.lock().unwrap().values().any(|l| !l.queue.is_empty())
    }

    /// Advance every layer to the latest frame due at `now_ns`, and return
    /// what to compose, by window.
    pub fn take(&self, now_ns: u64) -> HashMap<u32, Vec<Shown>> {
        let mut out: HashMap<u32, Vec<Shown>> = HashMap::new();
        let mut m = self.layers.lock().unwrap();
        let mut keys: Vec<(u32, u32)> = m.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            let l = m.get_mut(&key).unwrap();
            let mut next = None;
            while l.queue.front().is_some_and(|f| f.2 <= now_ns) {
                if next.is_some() {
                    l.dropped += 1;
                }
                next = l.queue.pop_front();
            }
            if let Some((px, _seq, _)) = next {
                l.current = Some(px);
            }
            if let (Some(r), Some(px)) = (l.rect, &l.current) {
                out.entry(key.0).or_default().push(Shown { id: l.id, at: (r[0], r[1]), pixels: px.clone() });
            }
        }
        out
    }
}

/// The part of `px` (placed at `at` in a window) inside a `w` x `h`
/// window: as is when all of it is, else a copy of that part, with where
/// it now starts; `None` when none of it is.
pub fn clip(px: &Arc<Pixels>, at: (i32, i32), w: u32, h: u32) -> Option<(Arc<Pixels>, (i32, i32))> {
    let rect = [at.0, at.1, px.width as i32, px.height as i32];
    let v = fresco_scene_server::video::visible(rect, w, h)?;
    if v == rect {
        return Some((px.clone(), at));
    }
    let (ox, oy) = ((v[0] - at.0) as usize, (v[1] - at.1) as usize);
    let (vw, vh) = (v[2] as usize, v[3] as usize);
    let mut out = Vec::with_capacity(vw * vh);
    for y in 0..vh {
        let row = (oy + y) * px.width as usize + ox;
        out.extend_from_slice(&px.px[row..row + vw]);
    }
    Some((pixels(vw as u32, vh as u32, out, px.seq), (v[0], v[1])))
}

/// The direct-mode sink: frescod's own frontend converts, and the frames
/// queue here.
pub struct Direct {
    pub redraw: crate::redraw::Redraw,
}

impl fresco_scene_server::video::VideoSink for Direct {
    fn place(&self, window: u32, layer: u32, rect: Option<[i32; 4]>) {
        VIDEOS.place(window, layer, rect);
        self.redraw.wake();
    }

    fn frame(&self, window: u32, layer: u32, w: u32, h: u32, seq: u32, present_ns: u64, fill: &mut dyn FnMut(&mut [u32])) {
        let mut px = vec![0u32; w as usize * h as usize];
        fill(&mut px);
        VIDEOS.queue(window, layer, pixels(w, h, px, seq), seq, present_ns);
        self.redraw.wake();
    }

    fn destroy(&self, window: u32, layer: u32) {
        VIDEOS.destroy(window, layer);
        self.redraw.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_latest_due_frame_and_counts_the_rest() {
        let v = Videos::default();
        v.place(3, 1, Some([10, 20, 2, 2]));
        for (seq, t) in [(1, 100), (2, 200), (3, 300)] {
            v.queue(3, 1, pixels(2, 2, vec![seq; 4], seq), seq, t);
        }
        assert!(v.take(50).is_empty());
        let s = v.take(250);
        assert_eq!(s[&3][0].pixels.px[0], 2);
        assert_eq!(s[&3][0].at, (10, 20));
        assert_eq!(v.layers.lock().unwrap()[&(3, 1)].dropped, 1);
        // Still shown with nothing new due.
        assert_eq!(v.take(260)[&3][0].pixels.px[0], 2);
        v.forget_window(3);
        assert!(v.take(1000).is_empty());
    }

    #[test]
    fn clips_to_the_window() {
        let px = pixels(4, 2, (0..8).collect(), 1);
        let (c, at) = clip(&px, (-1, 0), 2, 10).unwrap();
        assert_eq!((c.width, c.height, at), (2, 2, (0, 0)));
        assert_eq!(c.px, vec![1, 2, 5, 6]);
        assert!(clip(&px, (5, 0), 2, 10).is_none());
    }
}
