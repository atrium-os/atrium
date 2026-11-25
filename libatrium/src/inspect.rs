//! The inspector for C apps (dev-tools.md §3.6, D23–D25): what a frame
//! drew, for the IDE that started a dev build. Each `atrium_window_frame_*`
//! call is one node; `atrium.h` wraps those calls in macros that first hand
//! over `__FILE__` and `__LINE__` (`atrium__site`), so a node knows the
//! app's line that drew it.
//!
//! Nothing is recorded unless the process was started with the IDE's
//! socket at `ATRIUM_INSPECT_FD` (portcullisd does that for a Run from
//! Artifex, for dev builds only). The IDE is a peer: the wire is
//! `atrium-inspect`'s, bounded both ways.

use std::cell::Cell;
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use atrium_inspect::{Ask, Item, Link, Site, Tell, MAX_BOUNDS, MAX_ITEMS};

/// Node ids the outlines are drawn with, above any an app picks in
/// practice; outlines are never listed or picked.
pub const MARK_BASE: u32 = 0xFFFF_0000;
/// Longest source path kept, in bytes.
const MAX_PATH: usize = 1024;

/// One node of a frame, in the window's device pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Drawn {
    /// The app's node id.
    pub id: u32,
    /// `Rect`, `Path`, `Texture` or `Glyphs`.
    pub kind: &'static str,
    /// x, y, w, h.
    pub rect: [f32; 4],
    /// Its colour and the like, in a line.
    pub detail: String,
    /// The app's file and line that drew it.
    pub site: Option<(String, u32)>,
}

struct State {
    link: Link,
    /// The frame being built.
    building: Vec<Drawn>,
    /// The frame last ended: what the IDE sees.
    shown: Vec<Drawn>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// The socket was looked for (once per process).
static LOOKED: AtomicBool = AtomicBool::new(false);
/// A link is open: recording is on.
static ON: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The line `atrium__site` named for the next frame call.
    static NEXT_SITE: Cell<Option<(*const c_char, u32)>> = const { Cell::new(None) };
}

/// Called by `atrium.h`'s macros just before a frame call: the app's line
/// that makes the next node.
///
/// # Safety
///
/// `file` is NULL or a NUL-terminated string that outlives the next frame
/// call (`__FILE__` is static).
#[no_mangle]
pub unsafe extern "C" fn atrium__site(file: *const c_char, line: u32) {
    if ON.load(Ordering::Relaxed) || !LOOKED.load(Ordering::Relaxed) {
        NEXT_SITE.with(|s| s.set((!file.is_null()).then_some((file, line))));
    }
}

/// Take the line named for this call, as text.
fn take_site() -> Option<(String, u32)> {
    let (file, line) = NEXT_SITE.with(|s| s.take())?;
    // SAFETY: `atrium__site`'s contract: NUL-terminated and alive until
    // this call; strnlen keeps the read within MAX_PATH bytes either way.
    let n = unsafe { libc::strnlen(file, MAX_PATH) };
    if n == MAX_PATH {
        return None;
    }
    // SAFETY: n bytes then a NUL, as strnlen found.
    let s = unsafe { CStr::from_ptr(file) }.to_string_lossy().into_owned();
    Some((s, line))
}

/// Open the IDE's socket if this process was given one (once).
pub fn attach() {
    if LOOKED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(link) = Link::from_env() {
        *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(State { link, building: Vec::new(), shown: Vec::new() });
        ON.store(true, Ordering::Release);
    } else {
        NEXT_SITE.with(|s| s.set(None));
    }
}

/// Note a node the app drew (only while an IDE listens).
pub fn record(id: u32, kind: &'static str, rect: [f32; 4], detail: impl FnOnce() -> String) {
    let site = take_site();
    if !ON.load(Ordering::Acquire) {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = g.as_mut() {
        if st.building.len() < MAX_ITEMS && rect.iter().all(|v| v.is_finite()) {
            st.building.push(Drawn { id, kind, rect, detail: detail(), site });
        }
    }
}

/// The frame starts: forget the last one's nodes being built.
pub fn begin() {
    attach();
    NEXT_SITE.with(|s| s.set(None));
    if let Some(st) = STATE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        st.building.clear();
    }
}

/// The outlines to draw before the frame ends: (rect, colour, line width).
pub fn marks() -> Vec<([f32; 4], [f32; 4], f32)> {
    let g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_ref() else { return Vec::new() };
    outlines(&st.building, st.link.highlight, st.link.bounds)
}

/// What `marks` draws for `nodes`.
pub fn outlines(nodes: &[Drawn], highlight: Option<u32>, bounds: bool) -> Vec<([f32; 4], [f32; 4], f32)> {
    let mut out = Vec::new();
    if bounds {
        out.extend(nodes.iter().take(MAX_BOUNDS).map(|d| (d.rect, [0.15, 0.45, 0.95, 0.55], 1.0)));
    }
    if let Some(d) = highlight.and_then(|h| nodes.iter().rev().find(|d| d.id == h)) {
        out.push((d.rect, [0.88, 0.16, 0.55, 1.0], 2.0));
    }
    out
}

/// The frame ended: it is what the IDE sees now.
pub fn end() {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_mut() else { return };
    let built = std::mem::take(&mut st.building);
    if built != st.shown {
        st.shown = built;
        if std::mem::take(&mut st.link.following) {
            st.link.send(&Tell::Changed);
        }
    }
    drop(g);
    serve();
}

/// Answer what the IDE asked. A broken or gone IDE is dropped.
pub fn serve() {
    if !ON.load(Ordering::Acquire) {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_mut() else { return };
    let asks = match st.link.poll() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("libatrium: inspector: {e}; it is gone");
            *g = None;
            ON.store(false, Ordering::Release);
            return;
        }
    };
    for a in asks {
        match a {
            Ask::Tree => {
                st.link.following = true;
                let items = items(&st.shown);
                st.link.send(&Tell::Tree(items));
            }
            Ask::Pick(on) => st.link.picking = on,
            Ask::Highlight(id) => st.link.highlight = id,
            Ask::Bounds(on) => st.link.bounds = on,
        }
    }
}

/// A press at (`x`, `y`) in device pixels: the inspector's when it waits
/// for one (then the app's events never see it, as for any press).
pub fn press(x: f32, y: f32) {
    if !ON.load(Ordering::Acquire) {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_mut() else { return };
    if !std::mem::take(&mut st.link.picking) {
        return;
    }
    if let Some(id) = pick(&st.shown, x, y) {
        st.link.highlight = Some(id);
        st.link.send(&Tell::Picked(id));
    }
}

/// The last-drawn node under (`x`, `y`).
pub fn pick(nodes: &[Drawn], x: f32, y: f32) -> Option<u32> {
    nodes.iter().rev().find(|d| {
        let [rx, ry, w, h] = d.rect;
        x >= rx && y >= ry && x < rx + w && y < ry + h
    }).map(|d| d.id)
}

/// The frame as the IDE lists it: flat, in drawing order.
pub fn items(nodes: &[Drawn]) -> Vec<Item> {
    nodes.iter().take(MAX_ITEMS).map(|d| Item {
        id: d.id,
        parent: None,
        depth: 0,
        kind: d.kind.into(),
        rect: (d.rect[2] > 0.0 && d.rect[3] > 0.0).then_some(d.rect),
        detail: d.detail.clone(),
        site: d.site.as_ref().map(|(f, l)| Site { file: f.clone(), line: *l, col: 0 }),
    }).collect()
}

/// `#rrggbbaa` of a straight RGBA colour in [0, 1].
pub fn hex(r: f32, g: f32, b: f32, a: f32) -> String {
    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", c(r), c(g), c(b), c(a))
}

/// The box of a rotated quad (`atrium_window_frame_path`).
pub fn quad_box(cx: f32, cy: f32, length: f32, width: f32, angle: f32) -> [f32; 4] {
    let (s, c) = angle.sin_cos();
    let (hl, hw) = (length / 2.0, width / 2.0);
    let ex = (hl * c).abs() + (hw * s).abs();
    let ey = (hl * s).abs() + (hw * c).abs();
    [cx - ex, cy - ey, 2.0 * ex, 2.0 * ey]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: u32, rect: [f32; 4]) -> Drawn {
        Drawn { id, kind: "Rect", rect, detail: String::new(), site: Some(("app.c".into(), id)) }
    }

    #[test]
    fn a_press_picks_the_last_drawn_node_under_it() {
        let nodes = vec![d(1, [0.0, 0.0, 100.0, 100.0]), d(2, [10.0, 10.0, 20.0, 20.0]), d(3, [200.0, 0.0, 5.0, 5.0])];
        assert_eq!(pick(&nodes, 15.0, 15.0), Some(2));
        assert_eq!(pick(&nodes, 50.0, 50.0), Some(1));
        assert_eq!(pick(&nodes, 150.0, 50.0), None);
        let items = items(&nodes);
        assert_eq!(items[1].site.as_ref().map(|s| (s.file.as_str(), s.line)), Some(("app.c", 2)));
        assert!(items.iter().all(|i| i.depth == 0 && i.parent.is_none()));
    }

    #[test]
    fn outlines_follow_bounds_and_the_highlight() {
        let nodes = vec![d(1, [0.0, 0.0, 10.0, 10.0]), d(2, [5.0, 5.0, 2.0, 2.0])];
        assert!(outlines(&nodes, None, false).is_empty());
        assert_eq!(outlines(&nodes, None, true).len(), 2);
        let h = outlines(&nodes, Some(2), false);
        assert_eq!((h.len(), h[0].0), (1, [5.0, 5.0, 2.0, 2.0]));
        assert!(outlines(&nodes, Some(9), false).is_empty(), "an id not drawn outlines nothing");
    }

    #[test]
    fn a_turned_quad_is_boxed() {
        let b = quad_box(10.0, 10.0, 20.0, 2.0, std::f32::consts::FRAC_PI_2);
        assert!((b[2] - 2.0).abs() < 1e-4 && (b[3] - 20.0).abs() < 1e-4, "{b:?}");
        assert_eq!(hex(1.0, 0.0, 0.5, 1.0), "#ff0080ff");
    }
}
