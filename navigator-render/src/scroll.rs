//! ★ SCROLL REGIONS (backend §4.9 item 4): a part of the page that scrolls
//! on its own — today, a table wider than its box.
//!
//! NSG had no scroll offset: `overflow: auto` and `scroll` clip like
//! `hidden`. A scroll region is declared, not drawn:
//!
//! ```text
//! scroll c<viewport> x<i>[,<j>…] <content x> <content y> <content w> <content h>
//! ```
//!
//! - `c<viewport>` is the region's clip: what the reader sees of it.
//! - `x<i>,…` are the transforms the region MOVES: its own translation (the
//!   first, identity at offset 0) and every transform nested in it (they are
//!   written composed, so each must be moved too).
//! - the content box, in page units at offset 0: how far the region may move.
//!
//! The worker lays the content out once, at offset 0, and never learns where
//! the reader scrolled it: the UI owns the offset and the compositor applies
//! it (`offset_scene` is the model the review raster uses). The offset is
//! bounded by the content box, so a region can neither be scrolled to show
//! what is not there nor used to move content over the rest of the page:
//! every node a region moves is clipped to its viewport, which the broker
//! checks (`validate`).

use crate::{Scene, U, XF_ONE};

#[derive(Clone, Debug, PartialEq)]
pub struct Scroll {
    /// The viewport clip (an index into `Scene::clips`).
    pub clip: u32,
    /// The region's own translation (an index into `Scene::xforms`).
    pub xform: u32,
    /// The content box at offset 0: x, y, w, h in 1/64 px.
    pub content: (U, U, U, U),
}

/// At most this many scroll regions in one scene.
pub const MAX_SCROLLS: usize = 4096;

/// Every transform the region moves: its own and those nested in it, in
/// index order (a transform's parent always has a lower index).
pub fn moved_xforms(scene: &Scene, s: &Scroll) -> Vec<u32> {
    let mut inside = vec![false; scene.xforms.len()];
    let mut out = vec![];
    for i in 0..scene.xforms.len() {
        let mine = i as u32 == s.xform || scene.xform_parents.get(i).copied().flatten().is_some_and(|p| inside[p as usize]);
        if mine { inside[i] = true; out.push(i as u32) }
    }
    out
}

/// The offsets the region may take: `(dx_min, dx_max, dy_min, dy_max)`, 0
/// always included. Moving content LEFT is a negative dx.
pub fn range(scene: &Scene, s: &Scroll) -> (U, U, U, U) {
    let (vx, vy, vw, vh) = scene.clips[s.clip as usize];
    let (cx, cy, cw, ch) = s.content;
    let lo = |v0: U, vl: U, c0: U, cl: U| ((v0 + vl) - (c0 + cl)).min(0);
    let hi = |v0: U, c0: U| (v0 - c0).max(0);
    (lo(vx, vw, cx, cw), hi(vx, cx), lo(vy, vh, cy, ch), hi(vy, cy))
}

/// ★ The compositor's half, as a model: move region `i` by `(dx, dy)`,
/// clamped to its range — every transform it moves, and the own clip of
/// every path under those transforms. Returns the offset applied.
pub fn offset_scene(scene: &mut Scene, i: usize, dx: U, dy: U) -> (U, U) {
    let s = scene.scrolls[i].clone();
    let (x0, x1, y0, y1) = range(scene, &s);
    let (dx, dy) = (dx.clamp(x0, x1), dy.clamp(y0, y1));
    let moved = moved_xforms(scene, &s);
    for &x in &moved {
        let m = &mut scene.xforms[x as usize];
        m[4] += dx;
        m[5] += dy;
    }
    for (p, a) in scene.paths.iter_mut().zip(&scene.path_attrs) {
        if a.2.is_some_and(|x| moved.contains(&x)) {
            if let Some(k) = p.clip.as_mut() { k[0] += dx; k[2] += dx; k[1] += dy; k[3] += dy }
        }
    }
    (dx, dy)
}

/// ★ THE BROKER'S CHECK of a scene's scroll regions (backend §4.2: the scene
/// is untrusted). Refuses, never clamps:
/// - too many regions; an index that names no clip or transform;
/// - a region whose own transform is not a pure translation of its parent;
/// - a transform moved by two regions (regions do not nest: a table inside
///   one is moved by it and opens none of its own);
/// - a content box with a negative size (a small one is fine: it bounds the
///   offset, and a region whose content fits just cannot move);
/// - ★ a node moved by a region that is NOT clipped to that region's
///   viewport: moving it would paint it over the rest of the page.
pub fn validate(scene: &Scene) -> Result<(), String> {
    if scene.scrolls.len() > MAX_SCROLLS { return Err(format!("{} scroll regions, over {MAX_SCROLLS}", scene.scrolls.len())) }
    let mut owner: Vec<Option<usize>> = vec![None; scene.xforms.len()];
    for (i, s) in scene.scrolls.iter().enumerate() {
        if s.clip as usize >= scene.clips.len() { return Err(format!("scroll {i}: no clip c{}", s.clip)) }
        let Some(m) = scene.xforms.get(s.xform as usize) else { return Err(format!("scroll {i}: no transform x{}", s.xform)) };
        // Composed with its ancestors', the region's own transform must still
        // be its parent's plus a translation: the same linear part.
        let parent = scene.xform_parents.get(s.xform as usize).copied().flatten();
        let lin = parent.map(|p| scene.xforms[p as usize]).unwrap_or([XF_ONE, 0, 0, XF_ONE, 0, 0]);
        if m[..4] != lin[..4] { return Err(format!("scroll {i}: x{} is not a translation", s.xform)) }
        let (_, _, cw, ch) = s.content;
        if cw < 0 || ch < 0 { return Err(format!("scroll {i}: negative content size")) }
        for x in moved_xforms(scene, s) {
            if let Some(o) = owner[x as usize] { return Err(format!("scroll {i}: x{x} is already moved by scroll {o}")) }
            owner[x as usize] = Some(i);
        }
    }
    // Every node under a moved transform is clipped to that region's viewport.
    let regions: Vec<(Vec<u32>, u32)> = scene.scrolls.iter().map(|s| (moved_xforms(scene, s), s.clip)).collect();
    let check = |kind: &str, attrs: &[crate::Attrs]| -> Result<(), String> {
        for (n, a) in attrs.iter().enumerate() {
            let Some(x) = a.2 else { continue };
            for (moved, clip) in &regions {
                if moved.contains(&x) && a.0 != Some(*clip) {
                    return Err(format!("{kind} {n} is moved by a scroll region but not clipped to its viewport c{clip}"));
                }
            }
        }
        Ok(())
    };
    check("rect", &scene.rect_attrs)?;
    check("run", &scene.run_attrs)?;
    check("link", &scene.link_attrs)?;
    check("shadow", &scene.shadow_attrs)?;
    check("grad", &scene.grad_attrs)?;
    check("image", &scene.image_attrs)?;
    check("path", &scene.path_attrs)?;
    Ok(())
}
