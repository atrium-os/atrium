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
    /// The content's rate in milli-Hz (0: not said), for VRR pacing.
    hz_milli: u32,
}

/// A frame newly on screen, to announce once its flip has landed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Presented {
    pub window: u32,
    pub layer: u32,
    pub seq: u32,
    pub dropped: u32,
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
    /// Frames composed into a frame whose flip has not landed yet.
    awaiting: Mutex<Vec<Presented>>,
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
            Layer { id, rect: None, queue: VecDeque::new(), current: None, dropped: 0, hz_milli: 0 }
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

    pub fn rate(&self, window: u32, layer: u32, hz_milli: u32) {
        let mut m = self.layers.lock().unwrap();
        if let Some(l) = m.get_mut(&(window, layer)) {
            l.hz_milli = hz_milli;
        }
    }

    /// When the next queued frame is due (CLOCK_MONOTONIC ns), if any
    /// layer that shows has one waiting.
    pub fn next_due(&self) -> Option<u64> {
        self.layers.lock().unwrap().values()
            .filter(|l| l.rect.is_some())
            .filter_map(|l| l.queue.front().map(|f| f.2))
            .min()
    }

    /// With variable refresh down to `lo_mhz`: how often to present again
    /// so the shown layer with the slowest content keeps each frame up for
    /// a whole number of refreshes (24p on a 48–144 Hz panel: every
    /// 1/48 s). `None` when no shown layer said its rate, or none needs it.
    pub fn repeat_ns(&self, lo_mhz: u32) -> Option<u64> {
        if lo_mhz == 0 {
            return None;
        }
        self.layers.lock().unwrap().values()
            .filter(|l| l.rect.is_some() && l.current.is_some() && l.hz_milli > 0)
            .filter_map(|l| {
                let k = lo_mhz.div_ceil(l.hz_milli).max(1);
                (k > 1).then(|| 1_000_000_000_000u64 / (u64::from(l.hz_milli) * u64::from(k)))
            })
            .min()
    }

    /// Advance every layer of a window in `composed` to the latest frame
    /// due at `now_ns`, and return what to compose, by window. A layer
    /// whose window is not composed (off screen, or hidden in its slot)
    /// skips its frames and gets no `PRESENTED` (fresco-video.md §4).
    /// Frames newly shown wait in `awaiting` for [`Videos::landed`].
    pub fn take(&self, now_ns: u64, composed: &std::collections::HashSet<u32>) -> HashMap<u32, Vec<Shown>> {
        let mut out: HashMap<u32, Vec<Shown>> = HashMap::new();
        let mut newly = Vec::new();
        let mut m = self.layers.lock().unwrap();
        let mut keys: Vec<(u32, u32)> = m.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            let l = m.get_mut(&key).unwrap();
            if !composed.contains(&key.0) {
                l.dropped += l.queue.len() as u32;
                l.queue.clear();
                continue;
            }
            let mut next = None;
            while l.queue.front().is_some_and(|f| f.2 <= now_ns) {
                if next.is_some() {
                    l.dropped += 1;
                }
                next = l.queue.pop_front();
            }
            if let Some((px, seq, _)) = next {
                l.current = Some(px);
                if l.rect.is_some() {
                    newly.push(Presented { window: key.0, layer: key.1, seq, dropped: std::mem::take(&mut l.dropped) });
                }
            }
            if let (Some(r), Some(px)) = (l.rect, &l.current) {
                out.entry(key.0).or_default().push(Shown { id: l.id, at: (r[0], r[1]), pixels: px.clone() });
            }
        }
        drop(m);
        self.awaiting.lock().unwrap().extend(newly);
        out
    }

    /// Whether frames wait for their flip to land.
    pub fn awaiting(&self) -> bool {
        !self.awaiting.lock().unwrap().is_empty()
    }

    /// The flip carrying the frames composed so far is on screen, from
    /// `vblank_ns`: tell their owners (`EV_VIDEO_PRESENTED`).
    pub fn landed(&self, vblank_ns: u64, comp: &std::sync::Mutex<fresco_scene_server::window::Compositor>) {
        let done = std::mem::take(&mut *self.awaiting.lock().unwrap());
        if done.is_empty() {
            return;
        }
        let c = comp.lock().unwrap();
        for p in done {
            c.emit(fresco_scene_server::window::DisplayEvent::VideoPresented {
                window_id: p.window, layer: p.layer, seq: p.seq, vblank_ns, dropped: p.dropped,
            });
        }
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

    fn rate(&self, window: u32, layer: u32, hz_milli: u32) {
        VIDEOS.rate(window, layer, hz_milli);
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
        let on: std::collections::HashSet<u32> = [3].into();
        assert_eq!(v.next_due(), Some(100));
        assert!(v.take(50, &on).is_empty());
        let s = v.take(250, &on);
        assert_eq!(s[&3][0].pixels.px[0], 2);
        assert_eq!(s[&3][0].at, (10, 20));
        assert_eq!(*v.awaiting.lock().unwrap(), vec![Presented { window: 3, layer: 1, seq: 2, dropped: 1 }]);
        // Still shown with nothing new due, and nothing new to announce.
        v.awaiting.lock().unwrap().clear();
        assert_eq!(v.take(260, &on)[&3][0].pixels.px[0], 2);
        assert!(!v.awaiting());
        // Off screen: frames are skipped, not announced.
        v.queue(3, 1, pixels(2, 2, vec![9; 4], 9), 9, 270);
        assert!(v.take(280, &Default::default()).is_empty());
        assert!(!v.awaiting() && v.next_due().is_none());
        v.forget_window(3);
        assert!(v.take(1000, &on).is_empty());
    }

    #[test]
    fn repeats_slow_content_on_variable_refresh() {
        let v = Videos::default();
        v.place(1, 1, Some([0, 0, 2, 2]));
        v.queue(1, 1, pixels(2, 2, vec![0; 4], 1), 1, 0);
        v.take(1, &[1].into());
        assert_eq!(v.repeat_ns(48_000), None); // rate not said
        v.rate(1, 1, 24_000);
        assert_eq!(v.repeat_ns(48_000), Some(20_833_333)); // twice per frame
        assert_eq!(v.repeat_ns(20_000), None); // the panel holds 24 Hz itself
        v.rate(1, 1, 23_976);
        assert_eq!(v.repeat_ns(40_000), Some(20_854_187));
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
