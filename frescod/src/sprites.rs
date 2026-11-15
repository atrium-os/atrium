//! A window with sprites ([`crate::pack::Sprite`]) drawn on the CPU: the
//! window under them rastered by its changed tiles into a buffer of its
//! own, each sprite's content rastered once (again only when it changes),
//! and each frame the part of the window that changed made again from the
//! two with [`fresco_compute::compose::over`]. A push, fade, wipe or cover
//! transition then costs a copy and blend of what moved, not a raster of
//! the window. Used by frescod's compute backend and by the split
//! renderer's pixel mode.

use std::collections::HashMap;

use crate::pack::{SceneState, WindowFrame};

/// A sprite's content, rastered.
struct Content {
    key: u64,
    px: Vec<u32>,
}

/// Where a sprite showed in the last drawing: its group, the window rect
/// it covered, and what decided its pixels there.
#[derive(Clone, Copy, PartialEq)]
struct Shown {
    group: u32,
    rect: [u32; 4],
    state: (u64, [i32; 2], u32, [i32; 4]),
}

/// One window's sprite drawing, kept while it has sprites. A new one
/// draws the whole window on its first frame; drop it when the window
/// has none (its surface then holds a drawing the plain path must redraw).
#[derive(Default)]
pub struct Sprites {
    size: (u32, u32),
    base: Vec<u32>,
    base_tiles: Option<Vec<u64>>,
    base_key: Option<u64>,
    content: HashMap<u32, Content>,
    shown: Option<Vec<Shown>>,
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
    pub composite: std::time::Duration,
}

impl Sprites {
    /// Draw `f` (which has sprites) into `out` (`width` x `height`, pitch
    /// `width`): `Ok(Some(rect))`, what changed since the last drawing;
    /// `Ok(None)`, nothing did.
    pub fn draw(
        &mut self,
        t2: &fresco_compute::tier2::Tier2,
        scene: &SceneState,
        f: &WindowFrame,
        out: &mut [u32],
        width: u32,
        height: u32,
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
        let mut shown = Vec::with_capacity(f.sprites.len());
        for (i, s) in f.sprites.iter().enumerate() {
            let key = scene.sprite_key(f, i);
            let c = self.content.entry(s.group).or_insert_with(|| Content { key: !key, px: Vec::new() });
            if c.key != key || c.px.len() != n {
                let (frame, _) = scene.pack_part(f, Some(i), width, height);
                c.px.resize(n, 0);
                t2.render(&frame, &mut c.px).map_err(|e| e.to_string())?;
                c.key = key;
                cost.sprites_rastered += 1;
            }
            let clip = s.clip.map(|v| v.clamp(0, i32::MAX) as u32);
            let at = [s.offset[0] as i64, s.offset[1] as i64];
            let ox = |v: i64, hi: u32| v.clamp(0, hi as i64) as u32;
            let placed = [ox(at[0], width), ox(at[1], height), ox(at[0] + width as i64, width), ox(at[1] + height as i64, height)];
            let rect = meet(meet(clip, placed), whole);
            let rect = if empty(rect) || s.opacity <= 0.0 { [0; 4] } else { rect };
            shown.push(Shown { group: s.group, rect, state: (key, s.offset, s.opacity.to_bits(), s.clip) });
        }
        self.content.retain(|g, _| f.sprites.iter().any(|s| s.group == *g));
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
        for (s, sh) in f.sprites.iter().zip(self.shown.as_ref().unwrap()) {
            let r = meet(sh.rect, damage);
            if empty(r) {
                continue;
            }
            let c = &self.content[&s.group];
            fresco_compute::compose::over(out, width, &c.px, width, (width, height), r, (s.offset[0], s.offset[1]), s.opacity, threads());
        }
        cost.composite += t.elapsed();
        Ok(Some(damage))
    }
}
