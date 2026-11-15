//! A window with sprites ([`crate::pack::Sprite`]) drawn on the CPU: the
//! window under them rastered by its changed tiles into a buffer of its
//! own, each sprite's content rastered once (again only when it changes),
//! and each frame the part of the window that changed made again from the
//! two with [`fresco_compute::compose::over`]. A push, fade, wipe or cover
//! transition then costs a copy and blend of what moved, not a raster of
//! the window. Used by frescod's compute backend and by the split
//! renderer's pixel mode.

use crate::pack::{SceneState, WindowFrame};

/// A sprite's content, rastered, and the drawing that last used it.
struct Content {
    key: u64,
    px: Vec<u32>,
    used: u64,
    /// Rastered lazily: its packed frame while tiles are left to draw,
    /// and which tiles hold the content (a push reveals a slide a strip a
    /// frame, so its first frame rasters only that strip).
    frame: Option<fresco_compute::Frame>,
    valid: Vec<bool>,
    /// The group it was last drawn for, and its tiles' signatures: when
    /// the group's content changes (a build inside a slide), only the
    /// tiles whose signature changed are drawn again.
    group: u32,
    sig: Vec<u64>,
}

/// Contents kept beyond those in use: a transition's outgoing slide is
/// usually the last one's incoming, so it is not rastered again.
const KEEP: usize = 4;

/// How long a window's sprite drawing is kept after its last sprite.
pub const IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// Where a sprite showed in the last drawing: its group, the window rect
/// it covered, and what decided its pixels there.
#[derive(Clone, Copy, PartialEq)]
struct Shown {
    group: u32,
    rect: [u32; 4],
    state: (u64, [i32; 2], u32, [i32; 4], u32),
}

/// One window's sprite drawing. Its first drawing (and the first after
/// [`Sprites::pause`]) is of the whole window. Kept a while after the
/// window's sprites end ([`IDLE`]), so the next animation finds the window
/// under them and recent contents rastered already.
#[derive(Default)]
pub struct Sprites {
    size: (u32, u32),
    base: Vec<u32>,
    base_tiles: Option<Vec<u64>>,
    base_key: Option<u64>,
    content: Vec<Content>,
    shown: Option<Vec<Shown>>,
    draws: u64,
    paused: Option<std::time::Instant>,
    /// When a sprite last animated.
    animated: Option<std::time::Instant>,
}

fn union(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    if a[0] >= a[2] || a[1] >= a[3] {
        return b;
    }
    if b[0] >= b[2] || b[1] >= b[3] {
        return a;
    }
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

fn meet(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])]
}

fn empty(r: [u32; 4]) -> bool {
    r[0] >= r[2] || r[1] >= r[3]
}

/// Threads for the composite: as the raster's (all CPUs but one from
/// three up, `FRESCO_RASTER_THREADS` overrides).
fn threads() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        std::env::var("FRESCO_RASTER_THREADS").ok().and_then(|v| v.parse().ok())
            .unwrap_or(if cpus >= 3 { cpus - 1 } else { cpus })
            .max(1)
    })
}

/// What the last [`Sprites::draw`] cost, for `FRESCOD_RENDER_TIMES`.
#[derive(Default, Clone, Copy, Debug)]
pub struct Cost {
    pub base_tiles: u32,
    pub sprites_rastered: u32,
    /// Tiles of sprite content rastered.
    pub content_tiles: u32,
    /// Tiles of sprite content taken from the window's last drawing.
    pub grabbed: u32,
    pub composite: std::time::Duration,
}

impl Sprites {
    /// Draw `f` (which has sprites) into `out` (`width` x `height`, pitch
    /// `width`): `Ok(Some(rect))`, what changed since the last drawing;
    /// `Ok(None)`, nothing did. `plain`: `out` holds the window's last
    /// drawing, made without sprites (an outgoing sprite's pixels are then
    /// taken from it rather than rastered).
    pub fn draw(
        &mut self,
        t2: &fresco_compute::tier2::Tier2,
        scene: &SceneState,
        f: &WindowFrame,
        out: &mut [u32],
        width: u32,
        height: u32,
        plain: bool,
        cost: &mut Cost,
    ) -> Result<Option<[u32; 4]>, String> {
        use fresco_compute::{damage, TILE};
        let n = (width * height) as usize;
        if out.len() < n {
            return Err("sprites: surface too small".into());
        }
        let whole = [0, 0, width, height];
        if self.size != (width, height) || self.base.len() != n {
            *self = Sprites { size: (width, height), base: vec![0; n], ..Default::default() };
        }
        let mut damage = [0u32; 4];
        // The window under the sprites.
        let key = scene.base_key(f);
        if self.base_key != Some(key) {
            let (frame, _) = scene.pack_part(f, None, width, height);
            let sig = damage::tile_signatures(&frame);
            let h = &frame.header;
            match &self.base_tiles {
                Some(held) if held.len() == sig.len() => {
                    let dirty = damage::dirty(&sig, Some(held));
                    let tiles = damage::rects(&dirty, h.tiles_x, h.tiles_y, fresco_compute::tier2::Tier2::MAX_TILE_RECTS);
                    if !tiles.is_empty() {
                        t2.render_tiles(&frame, &mut self.base, &tiles).map_err(|e| e.to_string())?;
                    }
                    for t in &tiles {
                        cost.base_tiles += (t[2] - t[0]) * (t[3] - t[1]);
                        damage = union(damage, [t[0] * TILE, t[1] * TILE, (t[2] * TILE).min(width), (t[3] * TILE).min(height)]);
                    }
                }
                _ => {
                    self.base_tiles = None;
                    t2.render(&frame, &mut self.base).map_err(|e| e.to_string())?;
                    cost.base_tiles += h.tiles_x * h.tiles_y;
                    damage = whole;
                }
            }
            self.base_tiles = Some(sig);
            self.base_key = Some(key);
        }
        // Each sprite's content, and where it shows.
        let moving = f.sprites.iter().any(|s| s.animated);
        if moving {
            self.animated = Some(std::time::Instant::now());
        }
        self.paused = None;
        self.draws += 1;
        let mut shown = Vec::with_capacity(f.sprites.len());
        let mut using = Vec::with_capacity(f.sprites.len());
        let first = self.shown.is_none();
        for (i, s) in f.sprites.iter().enumerate() {
            let key = scene.sprite_key(f, i);
            let clip = s.clip.map(|v| v.clamp(0, i32::MAX) as u32);
            let off = [s.offset[0] as i64, s.offset[1] as i64];
            let ox = |v: i64, hi: u32| v.clamp(0, hi as i64) as u32;
            let k = s.scale as f64;
            let (ew, eh) = ((width as f64 * k).ceil() as i64, (height as f64 * k).ceil() as i64);
            let placed = [ox(off[0], width), ox(off[1], height), ox(off[0] + ew, width), ox(off[1] + eh, height)];
            let rect = meet(meet(clip, placed), whole);
            let rect = if empty(rect) || s.opacity <= 0.0 { [0; 4] } else { rect };
            // `rect` in the content's own pixels.
            let back = |v: u32, o: i64, hi: u32, up: bool| {
                let l = (v as i64 - o) as f64 / k;
                (if up { l.ceil() + 1.0 } else { l.floor() - 1.0 }).clamp(0.0, hi as f64) as u32
            };
            let local = [back(rect[0], off[0], width, false), back(rect[1], off[1], height, false), back(rect[2], off[0], width, true), back(rect[3], off[1], height, true)];
            let local = if k == 1.0 {
                [
                    (rect[0] as i64 - off[0]).clamp(0, width as i64) as u32,
                    (rect[1] as i64 - off[1]).clamp(0, height as i64) as u32,
                    (rect[2] as i64 - off[0]).clamp(0, width as i64) as u32,
                    (rect[3] as i64 - off[1]).clamp(0, height as i64) as u32,
                ]
            } else {
                local
            };
            let found = self.content.iter().position(|c| c.key == key && c.px.len() == n);
            // Not held: the group's own content changed, drawn again by
            // the tiles whose signatures changed (unless that content is
            // in use this frame, or that is most of it).
            let same = found.is_none().then(|| self.content.iter().position(|c| c.group == s.group && c.px.len() == n && !c.sig.is_empty())).flatten().filter(|j| !using.contains(j));
            let at = match (found, same) {
                (Some(at), _) => at,
                (None, Some(at)) => {
                    let (frame, _) = scene.pack_part(f, Some(i), width, height);
                    let sig = damage::tile_signatures(&frame);
                    let c = &mut self.content[at];
                    if sig.len() == c.sig.len() {
                        for (k, v) in c.valid.iter_mut().enumerate() {
                            *v &= sig[k] == c.sig[k];
                        }
                    } else {
                        c.valid = vec![false; sig.len()];
                    }
                    c.sig = sig;
                    c.frame = Some(frame);
                    c.key = key;
                    at
                }
                (None, None) => {
                    // A new raster: into the least recently used content
                    // not in use, once more than KEEP are held.
                    let spare = (self.content.len() >= KEEP + f.sprites.len())
                        .then(|| (0..self.content.len()).filter(|j| !using.contains(j)).min_by_key(|&j| self.content[j].used))
                        .flatten();
                    let at = spare.unwrap_or_else(|| {
                        self.content.push(Content { key: !key, px: Vec::new(), used: 0, frame: None, valid: Vec::new(), group: s.group, sig: Vec::new() });
                        self.content.len() - 1
                    });
                    let (frame, _) = scene.pack_part(f, Some(i), width, height);
                    let c = &mut self.content[at];
                    c.px.resize(n, 0);
                    c.valid = vec![false; (frame.header.tiles_x * frame.header.tiles_y) as usize];
                    c.sig = damage::tile_signatures(&frame);
                    c.group = s.group;
                    if first && std::env::var_os("FRESCOD_RENDER_TIMES").is_some() {
                        eprintln!("frescod: sprite {} new: plain {plain} outgoing {} opaque {} rect {rect:?}", s.group, s.outgoing, s.opaque);
                    }
                    if plain && first && s.outgoing && s.opaque && k == 1.0 && !empty(rect) {
                        cost.grabbed += take_from_window(t2, &frame, c, out, width, local, (off[0] as i32, off[1] as i32))?;
                    }
                    c.frame = Some(frame);
                    c.key = key;
                    cost.sprites_rastered += 1;
                    at
                }
            };
            self.content[at].used = self.draws;
            using.push(at);
            // The tiles of the content that show now, rastered if not yet;
            // while nothing animates, all of them (a hidden slide's, so it
            // is ready when it comes in).
            let c = &mut self.content[at];
            let all = !moving;
            if let (Some(frame), true) = (&c.frame, all || !empty(rect)) {
                let h = &frame.header;
                let mut need = vec![false; c.valid.len()];
                let mut any = false;
                let span = if all { [0, 0, width, height] } else { local };
                for ty in span[1] / TILE..span[3].div_ceil(TILE).min(h.tiles_y) {
                    for tx in span[0] / TILE..span[2].div_ceil(TILE).min(h.tiles_x) {
                        let k = (ty * h.tiles_x + tx) as usize;
                        if !c.valid[k] {
                            need[k] = true;
                            any = true;
                        }
                    }
                }
                if any {
                    let tiles = damage::rects(&need, h.tiles_x, h.tiles_y, fresco_compute::tier2::Tier2::MAX_TILE_RECTS);
                    t2.render_tiles(frame, &mut c.px, &tiles).map_err(|e| e.to_string())?;
                    // What a rect list covers (it may round up past `need`).
                    for t in &tiles {
                        for ty in t[1]..t[3].min(h.tiles_y) {
                            for tx in t[0]..t[2].min(h.tiles_x) {
                                c.valid[(ty * h.tiles_x + tx) as usize] = true;
                            }
                        }
                    }
                    cost.content_tiles += tiles.iter().map(|t| (t[2] - t[0]) * (t[3] - t[1])).sum::<u32>();
                }
                if c.valid.iter().all(|&v| v) {
                    c.frame = None;
                }
            }
            shown.push(Shown { group: s.group, rect, state: (key, s.offset, s.opacity.to_bits(), s.clip, s.scale.to_bits()) });
        }
        match &self.shown {
            Some(prev) if prev.len() == shown.len() && prev.iter().zip(&shown).all(|(a, b)| a.group == b.group) => {
                for (a, b) in prev.iter().zip(&shown) {
                    if a != b {
                        damage = union(damage, union(a.rect, b.rect));
                    }
                }
            }
            _ => damage = whole,
        }
        self.shown = Some(shown);
        let damage = meet(damage, whole);
        if empty(damage) {
            return Ok(None);
        }
        // The changed part again: the window under, then each sprite.
        let t = std::time::Instant::now();
        let w = width as usize;
        for y in damage[1] as usize..damage[3] as usize {
            let (a, b) = (y * w + damage[0] as usize, y * w + damage[2] as usize);
            out[a..b].copy_from_slice(&self.base[a..b]);
        }
        for ((s, sh), &at) in f.sprites.iter().zip(self.shown.as_ref().unwrap()).zip(&using) {
            let r = meet(sh.rect, damage);
            if empty(r) {
                continue;
            }
            let c = &self.content[at];
            if s.scale == 1.0 {
                fresco_compute::compose::over(out, width, &c.px, width, (width, height), r, (s.offset[0], s.offset[1]), s.opacity, threads());
            } else {
                let at = (s.offset[0] as f32, s.offset[1] as f32);
                fresco_compute::compose::over_scaled(out, width, &c.px, width, (width, height), r, at, s.scale, s.opacity, threads());
            }
        }
        cost.composite += t.elapsed();
        Ok(Some(damage))
    }

    /// The groups to keep as sprites ([`crate::pack::window_frame_promoted`]):
    /// those of the last drawing, while a sprite animated within [`IDLE`].
    pub fn keep(&self) -> std::collections::HashSet<u32> {
        match (&self.shown, self.animated) {
            (Some(shown), Some(t)) if t.elapsed() < IDLE => shown.iter().map(|s| s.group).collect(),
            _ => Default::default(),
        }
    }

    /// The window has no sprites now (it is drawn the plain way): the next
    /// drawing here is whole. True once it has had none for [`IDLE`]
    /// (then drop it).
    pub fn pause(&mut self) -> bool {
        self.shown = None;
        let since = *self.paused.get_or_insert_with(std::time::Instant::now);
        since.elapsed() >= IDLE
    }
}

/// An outgoing sprite's content (`c`, packed as `frame`) where it shows
/// now (`local`, its own pixels; at `off` in the window), taken from the
/// window's last drawing `out`: the group was drawn there just so, and
/// its pixels hide the window under it. A few tiles are rastered to check
/// that; if any differs nothing is taken. Returns the tiles taken (each
/// wholly inside `local`, now valid).
fn take_from_window(
    t2: &fresco_compute::tier2::Tier2,
    frame: &fresco_compute::Frame,
    c: &mut Content,
    out: &[u32],
    width: u32,
    local: [u32; 4],
    off: (i32, i32),
) -> Result<u32, String> {
    use fresco_compute::TILE;
    let h = &frame.header;
    let w = width as usize;
    let (tx0, ty0) = (local[0].div_ceil(TILE), local[1].div_ceil(TILE));
    let (tx1, ty1) = ((local[2] / TILE).min(h.tiles_x), (local[3] / TILE).min(h.tiles_y));
    if tx0 >= tx1 || ty0 >= ty1 {
        return Ok(0);
    }
    for y in local[1] as usize..local[3] as usize {
        let sy = (y as i64 + off.1 as i64) as usize;
        let (a, b) = (local[0] as usize, local[2] as usize);
        let sx = (a as i64 + off.0 as i64) as usize;
        c.px[y * w + a..y * w + b].copy_from_slice(&out[sy * w + sx..sy * w + sx + (b - a)]);
    }
    // Check: a few tiles spread over it, rastered over what was taken.
    let all: Vec<(u32, u32)> = (ty0..ty1).flat_map(|ty| (tx0..tx1).map(move |tx| (tx, ty))).collect();
    let step = all.len().div_ceil(12).max(1);
    let sample: Vec<(u32, u32)> = all.iter().copied().step_by(step).collect();
    let tile_px = |px: &[u32], (tx, ty): (u32, u32)| -> Vec<u32> {
        (ty * TILE..((ty + 1) * TILE).min(h.height))
            .flat_map(|y| px[(y * width + tx * TILE) as usize..(y * width + ((tx + 1) * TILE).min(width)) as usize].to_vec())
            .collect()
    };
    let taken: Vec<Vec<u32>> = sample.iter().map(|&t| tile_px(&c.px, t)).collect();
    let rects: Vec<[u32; 4]> = sample.iter().map(|&(tx, ty)| [tx, ty, tx + 1, ty + 1]).collect();
    t2.render_tiles(frame, &mut c.px, &rects).map_err(|e| e.to_string())?;
    for (k, &t) in sample.iter().enumerate() {
        c.valid[(t.1 * h.tiles_x + t.0) as usize] = true;
        if tile_px(&c.px, t) != taken[k] {
            return Ok(0);
        }
    }
    for &(tx, ty) in &all {
        c.valid[(ty * h.tiles_x + tx) as usize] = true;
    }
    Ok(all.len() as u32)
}
