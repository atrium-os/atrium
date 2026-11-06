//! forum-wm — the Forum WM **core** (`docs/spec/forum.md`): the privileged
//! window-management component, the only holder of the `window-management`
//! capability. Its job each frame:
//!
//!   surfaces (OP_WM_ENUMERATE)
//!     → `arrange`  : role-driven placement → the atomic OP_WM_DECLARE_LAYOUT
//!     → `rendering_decisions` : occlusion → OP_WM_SET_RENDERING (gate the hidden)
//!
//! This is the production implementation of the policy the gpusim models pinned
//! (`engine/src/forum_layout.rs` placement + `forum_engine.rs` visibility), now
//! against the real `fresco-protocol` wire types. The frescod side (the cap gate +
//! cross-app enumerate/composite mechanism) is the remaining seam; this is the
//! logic that drives it.

use fresco_protocol::{
    WmDeclareLayoutPayload, WmRect, WmRole, WmSetRenderingPayload, WmSlot, WmSurfaceInfo,
};

/// The output region: the screen plus the chrome-reserved edges (a top bar, a
/// bottom dock). Documents get the work area; chrome reserves the edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub rect: WmRect,
    pub bar_h: i32,
    pub dock_h: i32,
}

impl Screen {
    pub fn work_area(&self) -> WmRect {
        WmRect {
            x: self.rect.x,
            y: self.rect.y + self.bar_h,
            w: self.rect.w,
            h: self.rect.h - self.bar_h - self.dock_h,
        }
    }
}

/// Lower layer = closer to the viewer (drawn on top). Reserved roles sit above app
/// content so the shell/system is never coverable by an app.
///
/// Documents split into two layers by focus (see [`slot_layer`]): the *focused*
/// document sits at [`DOC_FOCUSED`] on top of its peers at [`DOC_UNFOCUSED`], so
/// the occlusion pass ([`rendering_decisions`]) render-gates the unfocused ones
/// (they share the work area, fully covered by the focused document). This is the
/// F1 "occluded surface render-gated" tie — the default arrangement is one focused
/// document filling the work area, the rest stacked + gated (forum.md §2.3), not
/// tiled (that's an F2 split/group intent).
const DOC_FOCUSED: i32 = 5;
const DOC_UNFOCUSED: i32 = 6;
/// Picture-in-picture (atrium-player-pip-keys.md §3): above documents and
/// panels, below chrome, HUDs and dialogs, so it never covers a prompt or a
/// credential slot.
pub const PIP_LAYER: i32 = 3;

fn layer(role: WmRole) -> i32 {
    match role {
        // frescod keeps toasts on top whatever the layout says.
        WmRole::Notification => -1,
        WmRole::Dialog => 0,
        WmRole::Hud => 1,
        WmRole::Chrome => 2,
        WmRole::Panel => 4,
        WmRole::Pip => PIP_LAYER,
        WmRole::Document => DOC_FOCUSED,
        WmRole::Background => 7,
    }
}

/// The layer a surface occupies given the resolved focus and tiling mode.
/// Stacked (default): a *document that isn't the focused surface* drops behind
/// the focused one ([`DOC_UNFOCUSED`]) so it gets occluded + render-gated. Tiled
/// (split intent): all documents sit at [`DOC_FOCUSED`] — they're laid out
/// side-by-side (non-overlapping), so none should gate the others.
fn slot_layer(s: &WmSurfaceInfo, focus: u32, tiled: bool) -> i32 {
    match s.role {
        WmRole::Document if !tiled && s.surface_id != focus => DOC_UNFOCUSED,
        r => layer(r),
    }
}

/// The i-th of `n` equal columns across the work area (the split-intent tiling).
/// The last column absorbs any rounding remainder so there's no gap.
fn tile_column(wa: WmRect, i: usize, n: usize) -> WmRect {
    if n <= 1 {
        return wa;
    }
    let w = wa.w / n as i32;
    let x = wa.x + i as i32 * w;
    let width = if i == n - 1 { wa.x + wa.w - x } else { w };
    WmRect { x, y: wa.y, w: width, h: wa.h }
}

/// Resolve which surface holds focus: a dialog grabs it; else the caller's intent
/// (if that surface still exists); else the topmost document; else nothing (0).
fn resolve_focus(surfaces: &[WmSurfaceInfo], focus_intent: Option<u32>) -> u32 {
    surfaces
        .iter()
        .find(|s| s.role == WmRole::Dialog)
        .map(|s| s.surface_id)
        .or_else(|| focus_intent.filter(|f| surfaces.iter().any(|s| s.surface_id == *f)))
        .or_else(|| surfaces.iter().find(|s| s.role == WmRole::Document).map(|s| s.surface_id))
        .unwrap_or(0)
}

/// App surfaces a person works in (what can be focused and moved).
fn click_focusable_role(role: WmRole) -> bool {
    matches!(role, WmRole::Document | WmRole::Dialog | WmRole::Panel)
}

fn rect_for(screen: &Screen, role: WmRole) -> WmRect {
    let wa = screen.work_area();
    match role {
        WmRole::Background => screen.rect,
        WmRole::Document => wa,
        WmRole::Panel => {
            let pw = wa.w / 4;
            WmRect { x: wa.x + wa.w - pw, y: wa.y, w: pw, h: wa.h }
        }
        WmRole::Dialog => {
            let (dw, dh) = (480, 320);
            WmRect { x: wa.x + (wa.w - dw) / 2, y: wa.y + (wa.h - dh) / 2, w: dw, h: dh }
        }
        WmRole::Hud | WmRole::Chrome => {
            WmRect { x: screen.rect.x, y: screen.rect.y, w: screen.rect.w, h: screen.bar_h }
        }
        // Placeholder: a quarter of the work area wide, bottom right, at
        // 16:9, until forum-wm's pip placement.
        WmRole::Pip => {
            let w = wa.w / 4;
            let h = w * 9 / 16;
            WmRect { x: wa.x + wa.w - w, y: wa.y + wa.h - h, w, h }
        }
        // frescod's band for toasts (it clamps them there anyway).
        WmRole::Notification => {
            let top = (wa.y - screen.rect.y).max(0) as u32;
            let [x, y, w, h] = fresco_protocol::notification_band(screen.rect.w.max(0) as u32, screen.rect.h.max(0) as u32, top);
            WmRect { x: screen.rect.x + x, y: screen.rect.y + y, w, h }
        }
    }
}

/// Place the session's surfaces by role → the atomic layout + focus. A dialog
/// grabs focus; else `focus_intent` (if still present); else the topmost document.
///
/// Focus-aware layering: the focused document sits on top ([`DOC_FOCUSED`]) and any
/// other documents drop behind it ([`DOC_UNFOCUSED`]), so [`rendering_decisions`]
/// render-gates the ones the focused document fully covers. The default is one
/// focused document filling the work area with the rest stacked + gated — the
/// intent-managed default (forum.md §2.3), not tiling.
pub fn arrange(
    screen: &Screen,
    surfaces: &[WmSurfaceInfo],
    focus_intent: Option<u32>,
) -> WmDeclareLayoutPayload {
    arrange_for(screen, surfaces, focus_intent, false)
}

/// `arrange` with an explicit tiling mode. `tiled = true` is the split intent
/// (forum.md §2.3): the workspace's documents are laid out side-by-side in equal
/// columns of the work area, all visible. `tiled = false` is the stacked default
/// (one focused document fills the work area, the rest render-gated). Non-document
/// roles (chrome/panel/dialog/background) place identically either way.
pub fn arrange_for(
    screen: &Screen,
    surfaces: &[WmSurfaceInfo],
    focus_intent: Option<u32>,
    tiled: bool,
) -> WmDeclareLayoutPayload {
    let focus = resolve_focus(surfaces, focus_intent);

    // In tiled mode, assign each document a column (stable order by surface_id).
    let mut doc_rect: std::collections::HashMap<u32, WmRect> = std::collections::HashMap::new();
    if tiled {
        let mut doc_ids: Vec<u32> = surfaces
            .iter()
            .filter(|s| s.role == WmRole::Document)
            .map(|s| s.surface_id)
            .collect();
        doc_ids.sort_unstable();
        let n = doc_ids.len();
        for (i, id) in doc_ids.iter().enumerate() {
            doc_rect.insert(*id, tile_column(screen.work_area(), i, n));
        }
    }

    let mut ordered: Vec<&WmSurfaceInfo> = surfaces.iter().collect();
    ordered.sort_by_key(|s| slot_layer(s, focus, tiled));
    let slots: Vec<WmSlot> = ordered
        .iter()
        .map(|s| WmSlot {
            surface_id: s.surface_id,
            rect: doc_rect.get(&s.surface_id).copied().unwrap_or_else(|| rect_for(screen, s.role)),
            layer: slot_layer(s, focus, tiled),
        })
        .collect();

    WmDeclareLayoutPayload { slots, focus, focus_cause: 0 }
}

/// Where the PiP window sits (forum_ctl::Corner), and where it is placed:
/// `aspect` (width:height, from the player's hints; 0 → 16:9) at a quarter
/// of the work area's width, no more than a quarter of its height, at least
/// 160×90, `PIP_MARGIN` from the corner's edges.
pub const PIP_MARGIN: i32 = 16;
pub fn pip_rect(wa: WmRect, aspect: (u32, u32), corner: forum_ctl::Corner) -> WmRect {
    use forum_ctl::Corner;
    let (aw, ah) = if aspect.0 == 0 || aspect.1 == 0 { (16, 9) } else { aspect };
    let ratio = ah as f64 / aw as f64;
    let mut w = wa.w as f64 / 4.0;
    let mut h = w * ratio;
    if h > wa.h as f64 / 4.0 {
        h = wa.h as f64 / 4.0;
        w = h / ratio;
    }
    if w < 160.0 {
        w = 160.0;
        h = w * ratio;
    }
    if h < 90.0 {
        h = 90.0;
        w = h / ratio;
    }
    let (w, h) = (w.round() as i32, h.round() as i32);
    let x = match corner {
        Corner::TopLeft | Corner::BottomLeft => wa.x + PIP_MARGIN,
        Corner::TopRight | Corner::BottomRight => wa.x + wa.w - PIP_MARGIN - w,
    };
    let y = match corner {
        Corner::TopLeft | Corner::TopRight => wa.y + PIP_MARGIN,
        Corner::BottomLeft | Corner::BottomRight => wa.y + wa.h - PIP_MARGIN - h,
    };
    WmRect { x, y, w, h }
}

/// The corner nearest `(x, y)`: where a PiP drag that ends there snaps.
pub fn nearest_corner(wa: WmRect, x: i32, y: i32) -> forum_ctl::Corner {
    use forum_ctl::Corner;
    let right = x >= wa.x + wa.w / 2;
    let bottom = y >= wa.y + wa.h / 2;
    match (right, bottom) {
        (false, false) => Corner::TopLeft,
        (true, false) => Corner::TopRight,
        (false, true) => Corner::BottomLeft,
        (true, true) => Corner::BottomRight,
    }
}

/// A snap target — a half of the work area (the snap intent, forum.md §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap {
    Left,
    Right,
}

/// The rect a [`Snap`] occupies within the work area (left/right half; the right
/// half absorbs the rounding remainder).
pub fn snap_region(wa: WmRect, snap: Snap) -> WmRect {
    let half = wa.w / 2;
    match snap {
        Snap::Left => WmRect { x: wa.x, y: wa.y, w: half, h: wa.h },
        Snap::Right => WmRect { x: wa.x + half, y: wa.y, w: wa.w - half, h: wa.h },
    }
}

/// Snap-mode layout: when any document is snapped, the workspace becomes a manual
/// tile — each snapped document occupies its half (all rendering); unsnapped
/// documents are omitted (the caller render-gates anything not placed). Non-
/// document roles (chrome/panel/dialog/background) place as usual. This is the
/// "float-with-snap, without floating" assist (forum.md §2.3).
pub fn arrange_snapped(
    screen: &Screen,
    surfaces: &[WmSurfaceInfo],
    focus_intent: Option<u32>,
    snapped: &std::collections::HashMap<u32, Snap>,
) -> WmDeclareLayoutPayload {
    let focus = resolve_focus(surfaces, focus_intent);
    let wa = screen.work_area();
    let mut slots: Vec<WmSlot> = Vec::new();
    for s in surfaces {
        match s.role {
            WmRole::Document => {
                if let Some(snap) = snapped.get(&s.surface_id) {
                    slots.push(WmSlot { surface_id: s.surface_id, rect: snap_region(wa, *snap), layer: DOC_FOCUSED });
                }
                // unsnapped documents are omitted → render-gated by the caller.
            }
            r => slots.push(WmSlot { surface_id: s.surface_id, rect: rect_for(screen, r), layer: layer(r) }),
        }
    }
    WmDeclareLayoutPayload { slots, focus, focus_cause: 0 }
}

/// From a declared layout, the occlusion-driven rendering decisions: a surface
/// fully covered by those above it renders nothing → mark it non-rendering so its
/// GPU work stops and the idle blocks power-gate. Partially-visible surfaces stay
/// rendering (Fresco clips); only *full* occlusion gates.
pub fn rendering_decisions(layout: &WmDeclareLayoutPayload) -> Vec<WmSetRenderingPayload> {
    // front-to-back: lowest layer first.
    let mut order: Vec<&WmSlot> = layout.slots.iter().collect();
    order.sort_by_key(|s| s.layer);
    let mut out = Vec::with_capacity(order.len());
    for (i, s) in order.iter().enumerate() {
        let mut region = vec![s.rect];
        for above in &order[..i] {
            region = region.iter().flat_map(|r| subtract(r, &above.rect)).collect();
        }
        let visible: i64 = region.iter().map(area).sum();
        out.push(WmSetRenderingPayload { surface_id: s.surface_id, rendering: visible > 0 });
    }
    out
}

// ── rect helpers (WmRect comes from another crate, so free fns) ──────────────

fn area(r: &WmRect) -> i64 {
    (r.w.max(0) as i64) * (r.h.max(0) as i64)
}
fn right(r: &WmRect) -> i32 {
    r.x + r.w
}
fn bottom(r: &WmRect) -> i32 {
    r.y + r.h
}
fn intersect(a: &WmRect, b: &WmRect) -> Option<WmRect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let r = right(a).min(right(b));
    let bo = bottom(a).min(bottom(b));
    if r > x && bo > y {
        Some(WmRect { x, y, w: r - x, h: bo - y })
    } else {
        None
    }
}
/// `r` minus `cut` → up to four rects (above/below/left/right of the overlap).
fn subtract(r: &WmRect, cut: &WmRect) -> Vec<WmRect> {
    let i = match intersect(r, cut) {
        None => return vec![*r],
        Some(i) => i,
    };
    let mut out = Vec::new();
    if i.y > r.y {
        out.push(WmRect { x: r.x, y: r.y, w: r.w, h: i.y - r.y });
    }
    if bottom(&i) < bottom(r) {
        out.push(WmRect { x: r.x, y: bottom(&i), w: r.w, h: bottom(r) - bottom(&i) });
    }
    if i.x > r.x {
        out.push(WmRect { x: r.x, y: i.y, w: i.x - r.x, h: i.h });
    }
    if right(&i) < right(r) {
        out.push(WmRect { x: right(&i), y: i.y, w: right(r) - right(&i), h: i.h });
    }
    out
}

/// The daemon reconcile loop. The WM core's job each time the surface set or focus
/// changes: enumerate → arrange → declare the layout + the rendering decisions.
/// What the session remembers about its monitors across restarts (of
/// frescod, the WM, or the machine): where each set of monitors sat, and the
/// scale set for a monitor. frescod remembers the same while it runs; this
/// is what puts it back after it starts again. Kept per user in
/// `/var/db/atrium/<user>/forum-displays.state` (`$FORUM_DISPLAYS`), a text
/// file of lines
///
/// ```text
/// scale <identity> <thousandths>
/// set <identity>@<x>,<y>:<w>x<h> ... [mirror <identity>=<source> ...]
/// ```
pub mod displays {
    use fresco_protocol::{WmOutput, WmOutputPlacement};
    use std::collections::HashMap;

    /// One monitor's remembered place and the size it had there.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Place {
        pub identity: u64,
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct DisplayMemory {
        /// By set of monitors (sorted identities): where each one sat
        /// (mirrors have no place of their own and are not listed).
        pub sets: HashMap<Vec<u64>, Vec<Place>>,
        /// By set of monitors: which mirrored which, (mirror, source).
        pub mirrors: HashMap<Vec<u64>, Vec<(u64, u64)>>,
        /// Scales the session set, by monitor (thousandths).
        pub scales: HashMap<u64, u32>,
        /// By set of monitors: its span groups (fresco-span.md §3).
        pub spans: HashMap<Vec<u64>, Vec<forum_ctl::SpanGroupInfo>>,
        /// Apps whose zoom stays on one monitor.
        pub no_span: std::collections::BTreeSet<String>,
    }

    /// The set a list of monitors makes, as `sets` keys it: every monitor
    /// on the desktop, present or within its grace.
    pub fn set_of(outputs: &[WmOutput]) -> Vec<u64> {
        let mut v: Vec<u64> = outputs.iter().map(|o| o.identity).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The mirrorings, (mirror, source), sorted.
    pub fn mirrors_of(outputs: &[WmOutput]) -> Vec<(u64, u64)> {
        let mut v: Vec<(u64, u64)> = outputs.iter().filter(|o| o.mirror_of != 0).map(|o| (o.identity, o.mirror_of)).collect();
        v.sort_unstable();
        v
    }

    /// The places of the monitors that aren't mirrors, shifted so the
    /// desktop starts at (0, 0) (as frescod keeps it), in identity order.
    pub fn places_of(outputs: &[WmOutput]) -> Vec<Place> {
        let plain = || outputs.iter().filter(|o| o.mirror_of == 0);
        let mx = plain().map(|o| o.rect.x).min().unwrap_or(0);
        let my = plain().map(|o| o.rect.y).min().unwrap_or(0);
        let mut v: Vec<Place> = plain()
            .map(|o| Place { identity: o.identity, x: o.rect.x - mx, y: o.rect.y - my, w: o.rect.w, h: o.rect.h })
            .collect();
        v.sort_by_key(|p| p.identity);
        v
    }

    impl DisplayMemory {
        /// Remember where this set sits now. Returns whether that changed.
        pub fn remember(&mut self, outputs: &[WmOutput]) -> bool {
            if outputs.is_empty() {
                return false;
            }
            let places = places_of(outputs);
            let mirrors = mirrors_of(outputs);
            let key = set_of(outputs);
            if self.sets.get(&key) == Some(&places) && self.mirrors.get(&key).map_or(mirrors.is_empty(), |m| *m == mirrors) {
                return false;
            }
            self.sets.insert(key.clone(), places);
            if mirrors.is_empty() {
                self.mirrors.remove(&key);
            } else {
                self.mirrors.insert(key, mirrors);
            }
            true
        }

        /// The mirrorings to change so this set mirrors as it was left:
        /// (mirror, source), source 0 to stop. Empty when it already does.
        pub fn restore_mirrors(&self, outputs: &[WmOutput]) -> Vec<(u64, u64)> {
            let key = set_of(outputs);
            if !self.sets.contains_key(&key) {
                return Vec::new();
            }
            let want = self.mirrors.get(&key).cloned().unwrap_or_default();
            let now = mirrors_of(outputs);
            let mut v: Vec<(u64, u64)> = now.iter().filter(|m| !want.iter().any(|w| w.0 == m.0)).map(|m| (m.0, 0)).collect();
            v.extend(want.iter().filter(|w| !now.contains(w)).copied());
            v
        }

        /// The arrangement to ask for so this set sits where it was left:
        /// `None` when it is there already, was never seen, or its monitors
        /// now run at other sizes (the old places would not fit).
        pub fn restore(&self, outputs: &[WmOutput]) -> Option<Vec<WmOutputPlacement>> {
            let saved = self.sets.get(&set_of(outputs))?;
            let now = places_of(outputs);
            if *saved == now {
                return None;
            }
            let sizes_match = saved.iter().all(|p| now.iter().any(|n| n.identity == p.identity && (n.w, n.h) == (p.w, p.h)));
            if !sizes_match {
                return None;
            }
            Some(saved.iter().map(|p| WmOutputPlacement { identity: p.identity, x: p.x, y: p.y }).collect())
        }

        pub fn to_text(&self) -> String {
            let mut lines: Vec<String> = Vec::new();
            let mut scales: Vec<(&u64, &u32)> = self.scales.iter().collect();
            scales.sort();
            for (id, s) in scales {
                lines.push(format!("scale {id:016x} {s}"));
            }
            let mut sets: Vec<(&Vec<u64>, &Vec<Place>)> = self.sets.iter().collect();
            sets.sort_by(|a, b| a.0.cmp(b.0));
            for (key, v) in sets {
                let mut ps: Vec<String> = v.iter().map(|p| format!("{:016x}@{},{}:{}x{}", p.identity, p.x, p.y, p.w, p.h)).collect();
                if let Some(m) = self.mirrors.get(key).filter(|m| !m.is_empty()) {
                    ps.push("mirror".into());
                    ps.extend(m.iter().map(|(a, b)| format!("{a:016x}={b:016x}")));
                }
                lines.push(format!("set {}", ps.join(" ")));
            }
            let hex = |v: &[u64]| v.iter().map(|i| format!("{i:016x}")).collect::<Vec<_>>().join(",");
            let mut spans: Vec<(&Vec<u64>, &Vec<forum_ctl::SpanGroupInfo>)> = self.spans.iter().collect();
            spans.sort_by(|a, b| a.0.cmp(b.0));
            for (key, groups) in spans {
                for g in groups {
                    lines.push(format!("span {} {} {} {}", hex(key), hex(&g.members), g.gap, g.gap_mm));
                }
            }
            for a in &self.no_span {
                lines.push(format!("nospan {a}"));
            }
            let mut t = String::from("# forum-wm: where each set of monitors sat, monitor scales, span groups\n");
            for l in lines {
                t.push_str(&l);
                t.push('\n');
            }
            t
        }

        /// Parse what [`Self::to_text`] wrote; a line it can't read is
        /// skipped (a damaged file loses that line, not the rest).
        pub fn from_text(t: &str) -> Self {
            let mut m = DisplayMemory::default();
            for line in t.lines() {
                let w: Vec<&str> = line.split_whitespace().collect();
                match w.first() {
                    Some(&"scale") if w.len() == 3 => {
                        if let (Ok(id), Ok(s)) = (u64::from_str_radix(w[1], 16), w[2].parse::<u32>()) {
                            m.scales.insert(id, s);
                        }
                    }
                    Some(&"span") if w.len() == 5 => {
                        let ids = |s: &str| -> Option<Vec<u64>> { s.split(',').map(|x| u64::from_str_radix(x, 16).ok()).collect() };
                        if let (Some(key), Some(members), Ok(gap), Ok(gap_mm)) = (ids(w[1]), ids(w[2]), w[3].parse::<u32>(), w[4].parse::<u32>()) {
                            if members.len() >= 2 {
                                m.spans.entry(key).or_default().push(forum_ctl::SpanGroupInfo { members, gap, gap_mm });
                            }
                        }
                    }
                    Some(&"nospan") if w.len() == 2 => {
                        m.no_span.insert(w[1].to_string());
                    }
                    Some(&"set") if w.len() > 1 => {
                        let cut = w.iter().position(|&x| x == "mirror").unwrap_or(w.len());
                        let places: Option<Vec<Place>> = w[1..cut].iter().map(|p| parse_place(p)).collect();
                        let mirrors: Option<Vec<(u64, u64)>> = w[cut.min(w.len())..].iter().skip(1).map(|p| {
                            let (a, b) = p.split_once('=')?;
                            Some((u64::from_str_radix(a, 16).ok()?, u64::from_str_radix(b, 16).ok()?))
                        }).collect();
                        if let (Some(mut v), Some(mut mi)) = (places, mirrors) {
                            v.sort_by_key(|p| p.identity);
                            mi.sort_unstable();
                            let mut key: Vec<u64> = v.iter().map(|p| p.identity).chain(mi.iter().map(|x| x.0)).collect();
                            key.sort_unstable();
                            key.dedup();
                            if !v.is_empty() {
                                if !mi.is_empty() {
                                    m.mirrors.insert(key.clone(), mi);
                                }
                                m.sets.insert(key, v);
                            }
                        }
                    }
                    _ => {}
                }
            }
            m
        }
    }

    fn parse_place(s: &str) -> Option<Place> {
        let (id, rest) = s.split_once('@')?;
        let (xy, wh) = rest.split_once(':')?;
        let (x, y) = xy.split_once(',')?;
        let (w, h) = wh.split_once('x')?;
        Some(Place {
            identity: u64::from_str_radix(id, 16).ok()?,
            x: x.parse().ok()?,
            y: y.parse().ok()?,
            w: w.parse().ok()?,
            h: h.parse().ok()?,
        })
    }

    /// `$FORUM_DISPLAYS` → `/var/db/atrium/<user>/forum-displays.state`.
    pub fn default_path() -> std::path::PathBuf {
        if let Some(p) = std::env::var_os("FORUM_DISPLAYS") {
            return p.into();
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
        format!("/var/db/atrium/{user}/forum-displays.state").into()
    }

    /// Load the memory (empty when there is none or it can't be read).
    pub fn load(path: &std::path::Path) -> DisplayMemory {
        std::fs::read_to_string(path).map(|t| DisplayMemory::from_text(&t)).unwrap_or_default()
    }

    /// Write it whole: a temporary file renamed over the old one, so a
    /// crash leaves the old memory or the new, never half of one.
    pub fn save(path: &std::path::Path, m: &DisplayMemory) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, m.to_text())?;
        std::fs::rename(&tmp, path)
    }
}

pub mod daemon {
    use super::{arrange, arrange_for, arrange_snapped, rendering_decisions, resolve_focus, Screen, Snap};
    use fresco_protocol::{WmDeclareLayoutPayload, WmOutput, WmOutputPlacement, WmRect, WmRole, WmSetRenderingPayload, WmSlot, WmSurfaceInfo};
    use std::cell::RefCell;
    use portcullis_peer::AppRegistry;
    use std::collections::{HashMap, HashSet};
    use std::io;

    /// `mm` between neighbouring pictures in pixels of the monitors `outs`
    /// (fresco-span.md §4): their mean density along one axis, `axis`
    /// giving a monitor's (pixels, millimetres) on it; 0 if none says.
    pub fn bezel_px(mm: u32, outs: &[&WmOutput], axis: impl Fn(&WmOutput) -> (i32, u32)) -> u32 {
        let sized: Vec<(f64, f64)> = outs.iter().map(|o| axis(o)).filter(|&(px, mm)| px > 0 && mm > 0).map(|(px, mm)| (f64::from(px), f64::from(mm))).collect();
        if sized.is_empty() {
            return 0;
        }
        let per_mm = sized.iter().map(|(px, mm)| px / mm).sum::<f64>() / sized.len() as f64;
        (f64::from(mm) * per_mm).round() as u32
    }

    /// Default number of workspaces (virtual desktops) when no per-user config
    /// overrides it. App surfaces live on one workspace each; the user switches
    /// the active workspace (Super+1..N). Chrome + background are global.
    pub const DEFAULT_WORKSPACES: usize = 4;

    /// Whether surface `s` is visible on workspace `active`. Chrome, background,
    /// and hud are *global* — Forum's shell + wallpaper show on every workspace.
    /// App surfaces (document/panel/dialog) show only on the workspace they're
    /// assigned to; an unassigned surface is treated as the active one (a
    /// just-appeared surface is visible until it's explicitly placed elsewhere).
    pub fn shown_in_workspace(
        s: &WmSurfaceInfo, assignment: &HashMap<u32, usize>, active: usize,
    ) -> bool {
        matches!(s.role, WmRole::Chrome | WmRole::Background | WmRole::Hud | WmRole::Notification)
            || assignment.get(&s.surface_id).copied().unwrap_or(active) == active
    }

    /// Roles a pointer click moves keyboard focus to. Clicking chrome (bar /
    /// dock / hud) or the wallpaper must NOT steal focus from the document the
    /// human is working in — only app surfaces (a document, a dialog, a side
    /// panel) are focus targets.
    fn click_focusable(role: WmRole) -> bool {
        matches!(role, WmRole::Document | WmRole::Dialog | WmRole::Panel)
    }

    /// Resolve a pointer click on surface `clicked` to a focus change, if any.
    /// Returns `Some(id)` when the click should move focus there — the clicked
    /// surface is a focusable app surface, present in the set, and not already
    /// the resolved focus — else `None` (chrome/background, an unknown id, or a
    /// click on the already-focused surface, all of which are no-ops).
    pub fn resolve_click_focus(
        surfaces: &[WmSurfaceInfo], clicked: u32, current_focus: Option<u32>,
    ) -> Option<u32> {
        if Some(clicked) == current_focus {
            return None;
        }
        surfaces
            .iter()
            .find(|s| s.surface_id == clicked && click_focusable(s.role))
            .map(|s| s.surface_id)
    }

    /// The frescod connection seam — the only I/O. Production wraps a
    /// `fresco_client::Connection` (OP_WM_ENUMERATE → reply, OP_WM_DECLARE_LAYOUT,
    /// OP_WM_SET_RENDERING); tests use a recorder. Splitting it keeps the reconcile
    /// loop pure and testable without a live frescod.
    pub trait FrescoConn {
        fn enumerate(&mut self) -> io::Result<Vec<WmSurfaceInfo>>;
        fn declare_layout(&mut self, layout: &WmDeclareLayoutPayload) -> io::Result<()>;
        fn set_rendering(&mut self, decisions: &[WmSetRenderingPayload]) -> io::Result<()>;
        /// The monitor's modes + the lit one (OP_WM_DISPLAY_MODES).
        fn display_modes(&mut self) -> io::Result<fresco_protocol::WmDisplayModesReply>;
        /// Ask frescod to switch mode (OP_WM_SET_DISPLAY_MODE). It switches
        /// at its next frame and broadcasts EV_DISPLAY_CHANGED, on which the
        /// WM adopts the new screen ([`Wm::set_screen`]).
        fn set_display_mode(&mut self, width: u32, height: u32, refresh_mhz: u32) -> io::Result<()>;
        /// Where each monitor sits on the desktop (OP_WM_OUTPUTS).
        fn outputs(&mut self) -> io::Result<Vec<WmOutput>>;
        /// Move monitors on the desktop (OP_WM_ARRANGE_OUTPUTS); frescod
        /// applies it at its next frame and broadcasts EV_DISPLAY_CHANGED.
        fn arrange_outputs(&mut self, placements: Vec<WmOutputPlacement>) -> io::Result<()>;
        /// Set a monitor's scale, thousandths, 0 = automatic
        /// (OP_WM_SET_OUTPUT_SCALE); EV_DISPLAY_CHANGED follows.
        fn set_output_scale(&mut self, identity: u64, scale_milli: u32) -> io::Result<()>;
        /// Make a monitor mirror another, `of` = 0 stops (OP_WM_MIRROR_OUTPUT);
        /// EV_DISPLAY_CHANGED follows.
        fn mirror_output(&mut self, identity: u64, of: u64) -> io::Result<()>;
        /// Set the system appearance (OP_WM_SET_APPEARANCE); every client
        /// hears EV_APPEARANCE_CHANGED.
        fn set_appearance(&mut self, a: fresco_protocol::AppearancePayload) -> io::Result<()>;
        /// Span window `p.window_id` over monitors `p.members`
        /// (OP_WM_SPAN_GROUP), or end the span (no members); the reply is
        /// the rect the window must cover. EV_DISPLAY_CHANGED follows.
        fn span_group(&mut self, p: fresco_protocol::WmSpanGroupPayload) -> io::Result<WmRect>;
    }

    /// A window spanning a group of monitors, as frescod granted it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SpanUp {
        pub window: u32,
        pub members: Vec<u64>,
        /// Hidden pixels between columns and between rows.
        pub gaps: (u32, u32),
        /// The desktop rect the window covers.
        pub rect: WmRect,
    }

    /// The WM core state: output geometry, the human's focus intent, and the
    /// workspace model (N virtual desktops + which is active + each app
    /// surface's workspace assignment).
    pub struct Wm {
        pub screen: Screen,
        pub focus_intent: Option<u32>,
        /// What moved the focus since the last layout
        /// (`fresco_protocol::focus_cause`), declared with it.
        focus_cause: std::cell::Cell<u8>,
        /// Number of workspaces (virtual desktops).
        pub workspaces: usize,
        /// The currently-visible workspace (0-based).
        pub active: usize,
        /// surface_id → workspace. Unlisted surfaces default to the active
        /// workspace (see [`shown_in_workspace`]); chrome/background ignore this.
        pub assignment: HashMap<u32, usize>,
        /// Per-app landing rules from config: app-id → workspace. Applied to a
        /// new surface by its `owner_app` (inert until frescod reports real
        /// app-ids; then a configured app opens on its assigned workspace).
        pub assign_rules: HashMap<String, usize>,
        /// Optional workspace names (cosmetic; from config). Indexed by workspace.
        pub names: Vec<String>,
        /// The launch registry (uid → app-id), loaded by the binary from
        /// portcullis-peer. Used to resolve a surface's `owner_uid` to its
        /// app-id so [`assign_rules`](Self::assign_rules) (keyed by app-id) apply.
        pub registry: AppRegistry,
        /// Workspaces currently in *split* (tiled) mode — the split intent
        /// (forum.md §2.3). A tiled workspace lays its documents side-by-side in
        /// columns (all visible); a non-tiled one stacks them (one focused +
        /// fills, the rest render-gated — the default).
        pub tiled_workspaces: HashSet<usize>,
        /// The surface currently *zoomed* to fill the whole screen (the zoom
        /// intent, forum.md §2.3) — a true fullscreen that even covers chrome;
        /// everything else render-gates. `None` = the normal arrangement.
        pub zoomed: Option<u32>,
        /// Documents snapped to a half of the work area (the snap intent). When
        /// any document on the active workspace is snapped, it enters snap mode
        /// (manual tile: snapped docs show in their halves, the rest gate).
        pub snapped: HashMap<u32, Snap>,
        /// Where the picture-in-picture window sits (`Intent::PipCorner`).
        pub pip_corner: forum_ctl::Corner,
        /// The monitors (display 3.0), as frescod places them on the desktop.
        /// With fewer than two present the WM lays out on `screen` alone.
        pub outputs: Vec<WmOutput>,
        /// surface_id → the monitor (identity) it lives on. A window whose
        /// monitor is gone shows on the primary one meanwhile (rescued) and
        /// goes back when that monitor returns, on whatever output it is
        /// plugged into (restored): the home is kept, not rewritten.
        pub home: HashMap<u32, u64>,
        /// The document last on top of each monitor, so focusing a window on
        /// one monitor doesn't change what another one shows.
        pub top: RefCell<HashMap<u64, u32>>,
        /// Where each set of monitors sat and the scales set, kept across
        /// restarts in `display_store` (None: in memory only, as in tests).
        pub displays: super::displays::DisplayMemory,
        pub display_store: Option<std::path::PathBuf>,
        /// Mirrorings were asked for to put a set back: when they land
        /// (same set), its arrangement is put back too, not overwritten.
        pub restoring: bool,
        /// The window spanning a group of monitors now (fresco-span.md).
        pub span_up: RefCell<Option<SpanUp>>,
        /// The last span frescod refused (window, members): not asked
        /// again until something changes, so a refusal doesn't loop.
        span_refused: RefCell<Option<(u32, Vec<u64>)>>,
        /// Notifications for the user, (title, body), that the binary posts
        /// to Praeco: a span member held out for flapping (fresco-span.md §6).
        pub notices: Vec<(String, String)>,
    }

    pub use forum_ctl::Side;

    impl Wm {
        pub fn new(screen: Screen) -> Self {
            Wm {
                screen,
                focus_intent: None,
                focus_cause: std::cell::Cell::new(0),
                workspaces: DEFAULT_WORKSPACES,
                active: 0,
                assignment: HashMap::new(),
                assign_rules: HashMap::new(),
                names: Vec::new(),
                registry: AppRegistry::default(),
                tiled_workspaces: HashSet::new(),
                zoomed: None,
                snapped: HashMap::new(),
                pip_corner: forum_ctl::Corner::default(),
                outputs: Vec::new(),
                home: HashMap::new(),
                top: RefCell::new(HashMap::new()),
                displays: Default::default(),
                display_store: None,
                restoring: false,
                span_up: RefCell::new(None),
                span_refused: RefCell::new(None),
                notices: Vec::new(),
            }
        }

        /// The monitors to lay out on: the present ones, primary first. The
        /// primary is the top-left one (top row, then leftmost); it holds the
        /// bar and the dock, and takes the windows whose monitor is gone.
        pub fn monitors(&self) -> Vec<WmOutput> {
            let mut v: Vec<WmOutput> = self.outputs.iter().filter(|o| o.present && o.mirror_of == 0).copied().collect();
            v.sort_by_key(|o| (o.rect.y, o.rect.x, o.output));
            v
        }

        /// The monitor surface `s` shows on: chrome, hud and the wallpaper on
        /// the primary; an app surface on its home if that monitor is here,
        /// else on the primary.
        pub fn monitor_of(&self, s: &WmSurfaceInfo, mons: &[WmOutput]) -> u64 {
            let primary = mons.first().map_or(0, |m| m.identity);
            if matches!(s.role, WmRole::Chrome | WmRole::Hud | WmRole::Background | WmRole::Notification) {
                return primary;
            }
            self.home
                .get(&s.surface_id)
                .copied()
                .filter(|h| mons.iter().any(|m| m.identity == *h))
                .unwrap_or(primary)
        }

        /// Monitor `m`'s screen: its part of the desktop below frescod's
        /// trusted strip (the strip runs along the desktop's top), with the
        /// bar and dock reserved on the primary only.
        fn screen_of(&self, m: &WmOutput, primary: bool) -> Screen {
            let top = self.screen.rect.y;
            let y = m.rect.y.max(top);
            let h = m.rect.h - (y - m.rect.y);
            Screen {
                rect: WmRect { x: m.rect.x, y, w: m.rect.w, h },
                bar_h: if primary { self.screen.bar_h } else { 0 },
                dock_h: if primary { self.screen.dock_h } else { 0 },
            }
        }

        /// Adopt frescod's monitors (after EV_DISPLAY_CHANGED) and re-lay out
        /// if they moved, came or went. Returns the new layout if it changed.
        pub fn set_outputs(&mut self, conn: &mut impl FrescoConn, outputs: Vec<WmOutput>)
            -> io::Result<Option<WmDeclareLayoutPayload>>
        {
            if outputs == self.outputs {
                return Ok(None);
            }
            // A span member newly held out for flapping: say which.
            for o in outputs.iter().filter(|o| o.flapping) {
                if !self.outputs.iter().any(|p| p.identity == o.identity && p.flapping) {
                    self.notices.push((
                        "A monitor keeps disconnecting".into(),
                        format!(
                            "The monitor on output {} dropped out 3 times in a minute. Its part of the \
                             game stays dark until it has stayed connected for 10 seconds; check its cable.",
                            o.output
                        ),
                    ));
                }
            }
            // The same monitors as before, moved: the session moved them, so
            // remember it. Another set: put it back where it was last left
            // (frescod knows only what it has seen since it started); what
            // it was then lands with the next EV_DISPLAY_CHANGED.
            let same_set = super::displays::set_of(&outputs) == super::displays::set_of(&self.outputs);
            let restore = !same_set || std::mem::take(&mut self.restoring);
            self.apply_remembered_displays(conn, &outputs, restore)?;
            // Windows with no home yet (opened while there was one monitor)
            // belong to the monitor they are on now, so a new monitor
            // becoming the primary doesn't pull them over to it.
            let mons = self.monitors();
            if !mons.is_empty() {
                for s in conn.enumerate()? {
                    if super::click_focusable_role(s.role) && !self.home.contains_key(&s.surface_id) {
                        let at = self.monitor_of(&s, &mons);
                        self.home.insert(s.surface_id, at);
                    }
                }
            }
            self.outputs = outputs;
            self.reconcile(conn).map(Some)
        }

        /// Bring monitors back as the session left them: a remembered scale
        /// for each one frescod has on automatic, and, with `restore`, the
        /// remembered arrangement for this set. Otherwise (or when it is
        /// already in place) remember the arrangement as it is.
        pub fn apply_remembered_displays(&mut self, conn: &mut impl FrescoConn, outputs: &[WmOutput], restore: bool)
            -> io::Result<()>
        {
            for o in outputs {
                match self.displays.scales.get(&o.identity) {
                    Some(&s) if o.scale_auto || o.scale_milli != s => conn.set_output_scale(o.identity, s)?,
                    _ => {}
                }
            }
            /* A span moves its monitors apart for the bezels: that is not
             * an arrangement to keep, nor one to put back. */
            if self.span_up.borrow().is_some() {
                return Ok(());
            }
            if restore {
                let mirrors = self.displays.restore_mirrors(outputs);
                for &(m, src) in &mirrors {
                    conn.mirror_output(m, src)?;
                }
                if !mirrors.is_empty() {
                    eprintln!("forum-wm: mirroring as this set was left");
                    self.restoring = true; // the arrangement follows once they land
                    return Ok(());
                }
                if let Some(p) = self.displays.restore(outputs) {
                    eprintln!("forum-wm: putting {} monitor(s) back where they were", p.len());
                    return conn.arrange_outputs(p);
                }
            }
            if self.displays.remember(outputs) {
                self.save_displays();
            }
            Ok(())
        }

        /// Make output `which` mirror output `of` (None: stop). frescod
        /// checks it; the WM re-lays out on the EV_DISPLAY_CHANGED that
        /// follows, which also remembers it.
        pub fn mirror(&mut self, conn: &mut impl FrescoConn, which: u32, of: Option<u32>) -> io::Result<()> {
            let find = |n: u32| self.outputs.iter().find(|o| o.output == n).copied();
            let Some(a) = find(which) else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such output"));
            };
            let src = match of {
                None => 0,
                Some(n) => find(n).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such output"))?.identity,
            };
            conn.mirror_output(a.identity, src)
        }

        /// Set output `which`'s scale (thousandths; 0 = automatic) and
        /// remember it for that monitor.
        pub fn set_scale(&mut self, conn: &mut impl FrescoConn, which: u32, scale_milli: u32) -> io::Result<()> {
            let Some(o) = self.outputs.iter().find(|o| o.output == which).copied() else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such output"));
            };
            conn.set_output_scale(o.identity, scale_milli)?;
            if scale_milli == 0 {
                self.displays.scales.remove(&o.identity);
            } else {
                self.displays.scales.insert(o.identity, scale_milli);
            }
            self.save_displays();
            Ok(())
        }

        fn save_displays(&self) {
            if let Some(p) = &self.display_store {
                if let Err(e) = super::displays::save(p, &self.displays) {
                    eprintln!("forum-wm: {}: {e}", p.display());
                }
            }
        }

        /// Send the focused window to the next monitor (in the primary-first
        /// order, wrapping) and re-declare; it becomes that window's home.
        /// Returns the monitor it went to, `None` with one monitor or nothing
        /// focusable.
        pub fn move_focused_to_next_monitor(&mut self, conn: &mut impl FrescoConn)
            -> io::Result<Option<u64>>
        {
            let mons = self.monitors();
            if mons.len() < 2 {
                return Ok(None);
            }
            let surfaces = conn.enumerate()?;
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            let focus = resolve_focus(&active, self.focus_intent);
            let Some(s) = active.iter().find(|s| s.surface_id == focus && super::click_focusable_role(s.role)) else {
                return Ok(None);
            };
            let here = self.monitor_of(s, &mons);
            let i = mons.iter().position(|m| m.identity == here).unwrap_or(0);
            let to = mons[(i + 1) % mons.len()].identity;
            self.home.insert(focus, to);
            self.snapped.remove(&focus); // a half of the old monitor means nothing on the new one
            self.focus_intent = Some(focus);
            self.declare(conn, &surfaces)?;
            Ok(Some(to))
        }

        /// Put output `which` beside output `of` (by output number), keeping
        /// the top (or left) edges aligned, and ask frescod to move it. The
        /// other monitors stay where they are; frescod refuses an arrangement
        /// that leaves a monitor detached or overlapping.
        pub fn place_output(&mut self, conn: &mut impl FrescoConn, which: u32, side: Side, of: u32)
            -> io::Result<()>
        {
            let find = |n: u32| self.outputs.iter().find(|o| o.output == n).copied();
            let (Some(a), Some(b)) = (find(which), find(of)) else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such output"));
            };
            if which == of {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "an output can't sit beside itself"));
            }
            let (x, y) = match side {
                Side::RightOf => (b.rect.x + b.rect.w, b.rect.y),
                Side::LeftOf => (b.rect.x - a.rect.w, b.rect.y),
                Side::Above => (b.rect.x, b.rect.y - a.rect.h),
                Side::Below => (b.rect.x, b.rect.y + b.rect.h),
            };
            conn.arrange_outputs(vec![WmOutputPlacement { identity: a.identity, x, y }])
        }

        /// A human-facing label for workspace `i`: its configured name, else
        /// `"workspace N"`.
        pub fn workspace_label(&self, i: usize) -> String {
            self.names.get(i).cloned().unwrap_or_else(|| format!("workspace {i}"))
        }

        /// The human focuses a surface (an intent); the next reconcile applies it.
        pub fn focus(&mut self, surface_id: u32) {
            self.focus_intent = Some(surface_id);
        }

        /// Assign a (new) surface to the active workspace — the default landing
        /// rule: a window opens on the workspace you're looking at.
        pub fn assign_to_active(&mut self, surface_id: u32) {
            self.assignment.insert(surface_id, self.active);
        }

        /// Switch the active workspace. The previous workspace's app surfaces
        /// fall out of the active set → render-gated (a whole desktop stops
        /// compositing); the new workspace's surfaces are arranged + composited.
        /// Focus resets so `arrange` picks the topmost document there. No-op
        /// (returns `None`) for an out-of-range or already-active workspace.
        pub fn switch_workspace(&mut self, conn: &mut impl FrescoConn, ws: usize)
            -> io::Result<Option<usize>>
        {
            if ws >= self.workspaces || ws == self.active {
                return Ok(None);
            }
            self.active = ws;
            self.focus_intent = None; // resolve focus within the newly-active workspace
            self.zoomed = None;       // a zoom belonged to the workspace we left
            let surfaces = conn.enumerate()?;
            self.declare(conn, &surfaces)?;
            Ok(Some(ws))
        }

        /// Move the focused surface to another workspace (the "send to desktop"
        /// intent) and re-declare, staying on the current workspace. The moved
        /// surface leaves the active set → render-gates; focus re-resolves among
        /// what remains. No-op (returns `None`) for an out-of-range / current
        /// workspace or when nothing focusable is on screen. Returns the moved
        /// surface id.
        pub fn move_focused_to_workspace(&mut self, conn: &mut impl FrescoConn, ws: usize)
            -> io::Result<Option<u32>>
        {
            if ws >= self.workspaces || ws == self.active {
                return Ok(None);
            }
            let surfaces = conn.enumerate()?;
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            let focus = arrange(&self.screen, &active, self.focus_intent).focus;
            if focus == 0 {
                return Ok(None); // nothing to move
            }
            self.assignment.insert(focus, ws);
            // Remember the placement by app-id so it persists: future launches of
            // this app (and the next reboot) land it here. The binary writes
            // `assign_rules` out to the state file after the move.
            if let Some((_user, app)) = active
                .iter()
                .find(|s| s.surface_id == focus)
                .and_then(|s| self.registry.resolve(s.owner_uid))
            {
                self.assign_rules.insert(app.to_string(), ws);
            }
            self.zoomed = self.zoomed.filter(|z| *z != focus); // a moved window can't stay zoomed here
            self.focus_intent = None; // it left this workspace; re-resolve focus among the rest
            self.declare(conn, &surfaces)?;
            Ok(Some(focus))
        }

        /// Toggle the split (tiled) intent for the active workspace and re-declare
        /// (forum.md §2.3). Tiled → documents side-by-side in columns, all
        /// visible; un-tiled → back to the stacked default (one focused fills,
        /// the rest render-gated). Returns the new tiled state.
        pub fn toggle_split(&mut self, conn: &mut impl FrescoConn) -> io::Result<bool> {
            let now_tiled = if self.tiled_workspaces.remove(&self.active) {
                false
            } else {
                self.tiled_workspaces.insert(self.active);
                true
            };
            let surfaces = conn.enumerate()?;
            self.declare(conn, &surfaces)?;
            Ok(now_tiled)
        }

        /// Toggle *zoom* (fullscreen) for the focused surface and re-declare
        /// (forum.md §2.3). Zoom on → that surface fills the screen, all else
        /// gates; zoom off → back to the arrangement. Returns the new zoom state.
        /// No-op (returns `false`) when there's no focusable surface.
        /// Move the PiP window to `corner` and lay out again.
        pub fn set_pip_corner(&mut self, conn: &mut impl FrescoConn, corner: forum_ctl::Corner) -> io::Result<()> {
            self.pip_corner = corner;
            let surfaces = conn.enumerate()?;
            self.declare(conn, &surfaces).map(|_| ())
        }

        pub fn toggle_zoom(&mut self, conn: &mut impl FrescoConn) -> io::Result<bool> {
            let surfaces = conn.enumerate()?;
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            let focus = arrange(&self.screen, &active, self.focus_intent).focus;
            let now_zoomed = if self.zoomed == Some(focus) {
                self.zoomed = None;
                false
            } else if focus != 0 {
                self.zoomed = Some(focus);
                true
            } else {
                false // nothing to zoom
            };
            self.declare(conn, &surfaces)?;
            Ok(now_zoomed)
        }

        /// The span the zoom wants now: the zoomed window, if its monitor is
        /// in a span group of this set and the app may span, over the
        /// group's monitors still on the desktop (present, or within their
        /// grace: frescod keeps a member's place while it is briefly gone).
        fn wanted_span(&self, active: &[WmSurfaceInfo]) -> Option<(u32, Vec<u64>, (u32, u32))> {
            let z = self.zoomed?;
            let s = active.iter().find(|s| s.surface_id == z)?;
            if self.displays.no_span.contains(&s.owner_app) {
                return None;
            }
            let mons = self.monitors();
            let home = self.monitor_of(s, &mons);
            // This set's group; else (a member gone for good makes another
            // set) the remembered group with the most monitors still here.
            let here = |g: &&forum_ctl::SpanGroupInfo| g.members.iter().filter(|id| self.outputs.iter().any(|o| o.identity == **id)).count();
            let g = match self.displays.spans.get(&super::displays::set_of(&self.outputs)) {
                Some(groups) => groups.iter().find(|g| g.members.contains(&home))?,
                None => self.displays.spans.values().flatten().filter(|g| g.members.contains(&home)).max_by_key(here)?,
            };
            let members: Vec<u64> = g
                .members
                .iter()
                .copied()
                .filter(|id| self.outputs.iter().any(|o| o.identity == *id && o.mirror_of == 0))
                .collect();
            if members.len() < 2 {
                return None;
            }
            let gaps = if g.gap_mm == 0 {
                (g.gap, g.gap)
            } else {
                let outs: Vec<&WmOutput> = members.iter().filter_map(|id| self.outputs.iter().find(|o| o.identity == *id)).collect();
                (bezel_px(g.gap_mm, &outs, |o| (o.rect.w, o.width_mm)), bezel_px(g.gap_mm, &outs, |o| (o.rect.h, o.height_mm)))
            };
            Some((z, members, gaps))
        }

        /// Ask frescod for the span the zoom wants, or end the one up.
        fn sync_span(&self, conn: &mut impl FrescoConn, active: &[WmSurfaceInfo]) -> io::Result<()> {
            let want = self.wanted_span(active);
            let now = self.span_up.borrow().as_ref().map(|s| (s.window, s.members.clone(), s.gaps));
            if want == now {
                return Ok(());
            }
            if let Some((w, m, _)) = &want {
                if self.span_refused.borrow().as_ref() == Some(&(*w, m.clone())) {
                    // Refused before: zoom stays on one monitor.
                    if now.is_none() {
                        return Ok(());
                    }
                }
            }
            if now.is_some() && want.is_none() {
                let ended = conn.span_group(fresco_protocol::WmSpanGroupPayload {
                    window_id: 0, members: Vec::new(), gaps_x: Vec::new(), gaps_y: Vec::new(),
                });
                *self.span_up.borrow_mut() = None;
                eprintln!("forum-wm: span ended");
                return ended.map(|_| ());
            }
            let Some((window, members, gaps)) = want else { return Ok(()) };
            let rects: Vec<WmRect> = members.iter().filter_map(|id| self.outputs.iter().find(|o| o.identity == *id)).map(|o| o.rect).collect();
            let distinct = |f: fn(&WmRect) -> i32| {
                let mut v: Vec<i32> = rects.iter().map(f).collect();
                v.sort_unstable();
                v.dedup();
                v.len()
            };
            let (cols, rows) = (distinct(|r| r.x), distinct(|r| r.y));
            let between = |n: usize, gap: u32| if gap == 0 { Vec::new() } else { vec![gap; n.saturating_sub(1)] };
            let p = fresco_protocol::WmSpanGroupPayload { window_id: window, members: members.clone(), gaps_x: between(cols, gaps.0), gaps_y: between(rows, gaps.1) };
            match conn.span_group(p) {
                Ok(rect) => {
                    eprintln!("forum-wm: window {window} spans {} monitors ({}x{})", members.len(), rect.w, rect.h);
                    *self.span_up.borrow_mut() = Some(SpanUp { window, members, gaps, rect });
                    *self.span_refused.borrow_mut() = None;
                }
                Err(e) => {
                    eprintln!("forum-wm: span refused ({e}); the zoom stays on one monitor");
                    *self.span_refused.borrow_mut() = Some((window, members));
                    if now.is_some() {
                        *self.span_up.borrow_mut() = None;
                        let _ = conn.span_group(fresco_protocol::WmSpanGroupPayload {
                            window_id: 0, members: Vec::new(), gaps_x: Vec::new(), gaps_y: Vec::new(),
                        });
                    }
                }
            }
            Ok(())
        }

        /// Make outputs `outs` a span group of this set of monitors, with
        /// `gap` hidden pixels between neighbours (fresco-span.md §3, §4):
        /// two or more monitors, all at one mode, none a mirror. They
        /// leave any group they were in; one output alone just leaves.
        /// Remembered; a zoom on one of them now covers them all.
        pub fn set_span_group(&mut self, conn: &mut impl FrescoConn, outs: &[u32], gap: u32, gap_mm: u32) -> io::Result<()> {
            let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_string());
            let mut members = Vec::new();
            for &n in outs {
                let o = self.outputs.iter().find(|o| o.output == n).ok_or_else(|| bad("no such output"))?;
                if o.mirror_of != 0 {
                    return Err(bad("a mirror can't be in a span group"));
                }
                if members.contains(&o.identity) {
                    return Err(bad("an output named twice"));
                }
                members.push(o.identity);
            }
            if members.len() >= 2 {
                let first = self.outputs.iter().find(|o| o.identity == members[0]).unwrap();
                let one_mode = members.iter().all(|id| {
                    self.outputs.iter().any(|o| o.identity == *id && (o.rect.w, o.rect.h, o.refresh_mhz) == (first.rect.w, first.rect.h, first.refresh_mhz))
                });
                if !one_mode {
                    return Err(bad("the monitors of a span group must run one mode"));
                }
                if first.rect.w < first.rect.h {
                    return Err(bad("a span is landscape only"));
                }
                if gap > 1000 {
                    return Err(bad("a bezel gap of more than 1000 pixels"));
                }
                if gap_mm > 200 {
                    return Err(bad("a bezel of more than 200 mm"));
                }
                if gap_mm != 0 && members.iter().any(|id| self.outputs.iter().any(|o| o.identity == *id && o.width_mm == 0)) {
                    return Err(bad("a monitor doesn't give its physical size: give the gap in pixels"));
                }
            }
            let key = super::displays::set_of(&self.outputs);
            let groups = self.displays.spans.entry(key.clone()).or_default();
            for g in groups.iter_mut() {
                g.members.retain(|id| !members.contains(id));
            }
            groups.retain(|g| g.members.len() >= 2);
            if members.len() >= 2 {
                groups.push(forum_ctl::SpanGroupInfo { members, gap: if gap_mm != 0 { 0 } else { gap }, gap_mm });
            }
            if groups.is_empty() {
                self.displays.spans.remove(&key);
            }
            self.save_displays();
            *self.span_refused.borrow_mut() = None;
            self.reconcile(conn).map(|_| ())
        }

        /// This set's span groups, and the window spanning now (0: none).
        pub fn span_groups(&self) -> (Vec<forum_ctl::SpanGroupInfo>, u32) {
            let g = self.displays.spans.get(&super::displays::set_of(&self.outputs)).cloned().unwrap_or_default();
            (g, self.span_up.borrow().as_ref().map_or(0, |s| s.window))
        }

        /// Let app `app` span (or keep its zoom on one monitor); remembered.
        pub fn set_app_spans(&mut self, conn: &mut impl FrescoConn, app: &str, spans: bool) -> io::Result<()> {
            if app.is_empty() || app.len() > 255 || app.chars().any(char::is_whitespace) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "an app id, without spaces"));
            }
            if spans {
                self.displays.no_span.remove(app);
            } else {
                self.displays.no_span.insert(app.to_string());
            }
            self.save_displays();
            self.reconcile(conn).map(|_| ())
        }

        /// Snap the focused document to a half of the work area (the snap intent,
        /// forum.md §2.3) and re-declare. Entering snap mode manually tiles the
        /// snapped docs; unsnapped docs render-gate. Only documents snap (chrome /
        /// dialogs aren't snap targets). No-op (`None`) otherwise. Returns the
        /// snapped surface.
        pub fn snap_focused(&mut self, conn: &mut impl FrescoConn, snap: Snap)
            -> io::Result<Option<u32>>
        {
            let surfaces = conn.enumerate()?;
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            let focus = arrange(&self.screen, &active, self.focus_intent).focus;
            let is_doc = active.iter().any(|s| s.surface_id == focus && s.role == WmRole::Document);
            if focus == 0 || !is_doc {
                return Ok(None);
            }
            self.snapped.insert(focus, snap);
            self.zoomed = self.zoomed.filter(|z| *z != focus); // snap supersedes zoom for this surface
            self.declare(conn, &surfaces)?;
            Ok(Some(focus))
        }

        /// Un-snap the focused document (back toward the normal arrangement) and
        /// re-declare. Returns the surface if it had been snapped.
        pub fn unsnap_focused(&mut self, conn: &mut impl FrescoConn) -> io::Result<Option<u32>> {
            let surfaces = conn.enumerate()?;
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            let focus = arrange(&self.screen, &active, self.focus_intent).focus;
            let removed = self.snapped.remove(&focus).is_some();
            self.declare(conn, &surfaces)?;
            Ok(removed.then_some(focus))
        }

        /// Pick the layout for the active workspace's visible surfaces, applying
        /// the active intent: zoom (one surface fills the screen) > snap (manual
        /// tile of the snapped docs) > the workspace's split/stacked arrangement.
        ///
        /// With several monitors each one is laid out on its own, for the
        /// surfaces that show on it ([`Self::monitor_of`]): the intents apply
        /// per monitor (zoom fills the zoomed window's monitor, split tiles
        /// each monitor's documents), and a monitor without the focused window
        /// keeps showing the document last on top of it.
        fn layout_active(&self, active: &[WmSurfaceInfo]) -> WmDeclareLayoutPayload {
            let mons = self.monitors();
            // A span: its window covers the group's monitors; the others
            // are laid out as usual.
            if let Some(su) = self.span_up.borrow().clone().filter(|su| active.iter().any(|s| s.surface_id == su.window)) {
                let mut slots = vec![WmSlot { surface_id: su.window, rect: su.rect, layer: 0 }];
                for (i, m) in mons.iter().enumerate().filter(|(_, m)| !su.members.contains(&m.identity)) {
                    let sub: Vec<WmSurfaceInfo> = active
                        .iter()
                        .filter(|s| s.surface_id != su.window && self.monitor_of(s, &mons) == m.identity)
                        .cloned()
                        .collect();
                    if sub.is_empty() {
                        continue;
                    }
                    let f = self.top.borrow().get(&m.identity).copied().filter(|t| sub.iter().any(|s| s.surface_id == *t));
                    slots.extend(self.layout_on(&self.screen_of(m, i == 0), &sub, f).slots);
                }
                return WmDeclareLayoutPayload { slots, focus: su.window, focus_cause: 0 };
            }
            match mons.as_slice() {
                [] => return self.layout_on(&self.screen, active, self.focus_intent),
                // One monitor left (the others gone, or within their grace):
                // everything on it, sized to it rather than to the desktop.
                [m] => return self.layout_on(&self.screen_of(m, true), active, self.focus_intent),
                _ => {}
            }
            let focus = resolve_focus(active, self.focus_intent);
            let mut slots = Vec::new();
            for (i, m) in mons.iter().enumerate() {
                let sub: Vec<WmSurfaceInfo> =
                    active.iter().filter(|s| self.monitor_of(s, &mons) == m.identity).cloned().collect();
                if sub.is_empty() {
                    continue;
                }
                let has = |id: u32| sub.iter().any(|s| s.surface_id == id);
                let f = if has(focus) {
                    Some(focus)
                } else {
                    self.top.borrow().get(&m.identity).copied().filter(|t| has(*t))
                };
                let l = self.layout_on(&self.screen_of(m, i == 0), &sub, f);
                if l.focus != 0 && sub.iter().any(|s| s.surface_id == l.focus && s.role == WmRole::Document) {
                    self.top.borrow_mut().insert(m.identity, l.focus);
                }
                slots.extend(l.slots);
            }
            WmDeclareLayoutPayload { slots, focus, focus_cause: 0 }
        }

        /// [`Self::layout_active`] on one screen.
        fn layout_on(&self, screen: &Screen, active: &[WmSurfaceInfo], focus_intent: Option<u32>)
            -> WmDeclareLayoutPayload
        {
            // Zoom wins: the zoomed surface alone fills the whole screen.
            if let Some(zid) = self.zoomed.filter(|z| active.iter().any(|s| s.surface_id == *z)) {
                return WmDeclareLayoutPayload {
                    slots: vec![WmSlot { surface_id: zid, rect: screen.rect, layer: 0 }],
                    focus: zid,
                    focus_cause: 0,
                };
            }
            // Snap: if any visible document is snapped, enter snap mode.
            if active.iter().any(|s| s.role == WmRole::Document && self.snapped.contains_key(&s.surface_id)) {
                return arrange_snapped(screen, active, focus_intent, &self.snapped);
            }
            // Otherwise the workspace's split (tiled) or stacked arrangement.
            let tiled = self.tiled_workspaces.contains(&self.active);
            arrange_for(screen, active, focus_intent, tiled)
        }

        /// The workspace-aware placement step shared by every reconcile path:
        /// choose the active-workspace layout, declare it, then render-gate every
        /// surface NOT placed in it — apps on inactive workspaces, and (in zoom /
        /// snap mode) the active-workspace surfaces the intent excluded. So an
        /// inactive workspace, or a hidden window, fully stops compositing.
        fn declare(&self, conn: &mut impl FrescoConn, surfaces: &[WmSurfaceInfo])
            -> io::Result<WmDeclareLayoutPayload>
        {
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            self.sync_span(conn, &active)?;
            let mut layout = self.layout_active(&active);
            layout.focus_cause = self.focus_cause.take();
            conn.declare_layout(&layout)?;

            let placed: HashSet<u32> = layout.slots.iter().map(|s| s.surface_id).collect();
            let mut decisions = rendering_decisions(&layout); // occlusion among placed
            for s in surfaces.iter().filter(|s| !placed.contains(&s.surface_id)) {
                decisions.push(WmSetRenderingPayload { surface_id: s.surface_id, rendering: false });
            }
            conn.set_rendering(&decisions)?;
            Ok(layout)
        }

        /// Re-derive and push the layout for the active workspace: enumerate,
        /// arrange the visible surfaces, declare the atomic layout, gate the
        /// occluded ones + every inactive-workspace surface. Returns the
        /// declared (active-workspace) layout.
        /// The output changed size (a mode switch or a monitor swap): adopt the
        /// new screen and re-lay out every surface for it. A screen of zero
        /// area (monitor unplugged) keeps the old layout: there is nothing to
        /// place windows on, and the next plug brings a real size.
        pub fn set_screen(&mut self, conn: &mut impl FrescoConn, screen: Screen)
            -> io::Result<Option<WmDeclareLayoutPayload>>
        {
            if screen.rect.w <= 0 || screen.rect.h <= 0 || screen == self.screen {
                return Ok(None);
            }
            self.screen = screen;
            self.reconcile(conn).map(Some)
        }

        pub fn reconcile(&self, conn: &mut impl FrescoConn) -> io::Result<WmDeclareLayoutPayload> {
            let surfaces = conn.enumerate()?;
            self.declare(conn, &surfaces)
        }

        /// Focus-follows-click: react to a pointer-button press frescod reported
        /// over surface `clicked` (the compositor tags the event with the
        /// hit-test target, so it's the surface *under the cursor*, not the
        /// currently-focused one). If that surface is a focusable app surface
        /// that isn't already focused, move focus to it and re-declare the
        /// layout. Returns the new focus when it changed, `None` otherwise.
        ///
        /// One enumerate serves both the current-focus check and the re-arrange,
        /// so a click that doesn't change focus costs a single round-trip and no
        /// layout push.
        pub fn focus_click(&mut self, conn: &mut impl FrescoConn, clicked: u32)
            -> io::Result<Option<u32>>
        {
            let surfaces = conn.enumerate()?;
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            let resolved = arrange(&self.screen, &active, self.focus_intent).focus;
            match resolve_click_focus(&active, clicked, Some(resolved)) {
                Some(id) => {
                    self.focus_intent = Some(id);
                    self.focus_cause.set(fresco_protocol::focus_cause::POINTER);
                    let layout = self.declare(conn, &surfaces)?;
                    Ok(Some(layout.focus))
                }
                None => Ok(None),
            }
        }

        /// A newly-created surface. Per forum.md §2.3 a new *document* opens
        /// focused; other roles don't steal focus (a dialog grabs it via
        /// `arrange` already; panels/chrome/background never do). Reconciles
        /// either way to place the newcomer. Returns the declared layout.
        pub fn focus_new(&mut self, conn: &mut impl FrescoConn, new_id: u32)
            -> io::Result<WmDeclareLayoutPayload>
        {
            let surfaces = conn.enumerate()?;
            if let Some(s) = surfaces.iter().find(|s| s.surface_id == new_id) {
                // Land the newcomer: resolve its owning uid → app-id via the
                // launch registry, and if config has a rule for that app-id use
                // its workspace; else the active workspace (the default flow).
                let ws = self
                    .registry
                    .resolve(s.owner_uid)
                    .and_then(|(_user, app)| self.assign_rules.get(app).copied())
                    .filter(|w| *w < self.workspaces)
                    .unwrap_or(self.active);
                self.assignment.insert(new_id, ws);
                // With several monitors a new window opens on the monitor of
                // the window you're working in (the primary if none).
                let mons = self.monitors();
                if mons.len() > 1 && !self.home.contains_key(&new_id) {
                    let shown: Vec<WmSurfaceInfo> = surfaces
                        .iter()
                        .filter(|t| t.surface_id != new_id && shown_in_workspace(t, &self.assignment, self.active))
                        .cloned()
                        .collect();
                    let cur = resolve_focus(&shown, self.focus_intent);
                    let at = shown
                        .iter()
                        .find(|t| t.surface_id == cur)
                        .map_or(mons[0].identity, |t| self.monitor_of(t, &mons));
                    self.home.insert(new_id, at);
                }
                // A new document opens focused — but only if it landed on the
                // workspace you're looking at (a rule may send it elsewhere).
                if s.role == WmRole::Document && ws == self.active {
                    self.focus_intent = Some(new_id);
                    self.focus_cause.set(fresco_protocol::focus_cause::PROGRAM);
                }
            }
            self.declare(conn, &surfaces)
        }

        /// The switcher (forum.md §2.3): cycle keyboard focus to the next
        /// document. Re-declares the layout so render-gating follows — the
        /// newly-focused document composites, the prior one drops behind and
        /// gates. No-op (returns `None`) with fewer than two documents.
        pub fn cycle_focus(&mut self, conn: &mut impl FrescoConn)
            -> io::Result<Option<u32>>
        {
            let surfaces = conn.enumerate()?;
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();
            let mut docs: Vec<u32> = active
                .iter()
                .filter(|s| s.role == WmRole::Document)
                .map(|s| s.surface_id)
                .collect();
            docs.sort_unstable();
            let current = arrange(&self.screen, &active, self.focus_intent).focus;
            match next_document(&docs, current) {
                Some(next) if next != current => {
                    self.focus_intent = Some(next);
                    self.focus_cause.set(fresco_protocol::focus_cause::KEYBOARD);
                    let layout = self.declare(conn, &surfaces)?;
                    Ok(Some(layout.focus))
                }
                _ => Ok(None),
            }
        }
    }

    /// The document focused after `current` in a forward cycle through `docs`
    /// (ascending surface_id). Wraps around; `None` if there are no documents;
    /// starts at the first if `current` isn't itself a document.
    pub fn next_document(docs: &[u32], current: u32) -> Option<u32> {
        if docs.is_empty() {
            return None;
        }
        match docs.iter().position(|&d| d == current) {
            Some(i) => Some(docs[(i + 1) % docs.len()]),
            None => Some(docs[0]),
        }
    }
}

/// The forum-ctl side: the WM core answering chrome apps' intents
/// (`docs/spec/forum.md` §3-4). The chrome apps (dock/bar/shelf/overview) hold no
/// window-management cap; they ask the core, which holds it, to act on their behalf.
pub mod control {
    use super::daemon::{FrescoConn, Wm};
    use forum_ctl::{Intent, Reply};

    /// Carry out one chrome intent against Fresco. The core is the authority: a
    /// read intent (`ListSurfaces`) answers from the Fresco enumerate; an action
    /// intent (`Focus`) updates the focus and re-declares the layout. Errors talking
    /// to Fresco become `Reply::Err` rather than tearing down the control session.
    pub fn handle_intent(wm: &mut Wm, conn: &mut impl FrescoConn, intent: Intent) -> Reply {
        match intent {
            Intent::ListSurfaces => match conn.enumerate() {
                Ok(surfaces) => {
                    // Report the focus the WM actually resolves for this surface set
                    // (a dialog grabs it; else the intent; else the topmost
                    // document) — not the raw intent, which is None until a focus.
                    let focus = super::arrange(&wm.screen, &surfaces, wm.focus_intent).focus;
                    Reply::Surfaces { surfaces, focus }
                }
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::Focus { surface_id } => {
                wm.focus(surface_id);
                match wm.reconcile(conn) {
                    Ok(layout) => {
                        // Report what actually got focus (a dialog can override the
                        // request), so the chrome's highlight matches the screen.
                        if layout.focus == surface_id { Reply::Ack }
                        else { Reply::Surfaces { surfaces: Vec::new(), focus: layout.focus } }
                    }
                    Err(e) => Reply::Err { message: e.to_string() },
                }
            }
            Intent::ListDisplayModes => match conn.display_modes() {
                Ok(r) => Reply::DisplayModes { modes: r.modes, current: r.current },
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::ListOutputs => match conn.outputs() {
                Ok(outputs) => Reply::Outputs { outputs },
                Err(e) => Reply::Err { message: e.to_string() },
            },
            // frescod checks the geometry and moves the monitor at its next
            // frame; the WM re-lays out on the EV_DISPLAY_CHANGED that follows.
            Intent::PlaceOutput { output, side, of } => match wm.place_output(conn, output, side, of) {
                Ok(()) => Reply::Ack,
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::MirrorOutput { output, of } => match wm.mirror(conn, output, of) {
                Ok(()) => Reply::Ack,
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::SetOutputScale { output, scale_milli } => match wm.set_scale(conn, output, scale_milli) {
                Ok(()) => Reply::Ack,
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::SetAppearance { dark } => {
                let scheme = if dark { fresco_protocol::APPEARANCE_DARK } else { fresco_protocol::APPEARANCE_LIGHT };
                match conn.set_appearance(fresco_protocol::AppearancePayload { scheme, high_contrast: false, accent: 0 }) {
                    Ok(()) => Reply::Ack,
                    Err(e) => Reply::Err { message: e.to_string() },
                }
            }
            Intent::ToggleZoom => match wm.toggle_zoom(conn) {
                Ok(_) => Reply::Ack,
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::SetSpanGroup { outputs, gap, gap_mm } => match wm.set_span_group(conn, &outputs, gap, gap_mm) {
                Ok(()) => Reply::Ack,
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::ListSpanGroups => {
                let (groups, spanning) = wm.span_groups();
                Reply::SpanGroups { groups, spanning, no_span: wm.displays.no_span.iter().cloned().collect() }
            }
            Intent::PipCorner { corner } => match wm.set_pip_corner(conn, corner) {
                Ok(()) => Reply::Ack,
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::SetAppSpans { app, spans } => match wm.set_app_spans(conn, &app, spans) {
                Ok(()) => Reply::Ack,
                Err(e) => Reply::Err { message: e.to_string() },
            },
            Intent::MoveToNextMonitor => match wm.move_focused_to_next_monitor(conn) {
                Ok(Some(_)) => Reply::Ack,
                Ok(None) => Reply::Err { message: "nothing to move, or only one monitor".into() },
                Err(e) => Reply::Err { message: e.to_string() },
            },
            // frescod validates the mode against the monitor's list; the WM
            // re-lays out when frescod's EV_DISPLAY_CHANGED arrives, not here,
            // so a refused or failed switch leaves the layout alone.
            Intent::SetDisplayMode { width, height, refresh_mhz } =>
                match conn.set_display_mode(width, height, refresh_mhz) {
                    Ok(()) => Reply::Ack,
                    Err(e) => Reply::Err { message: e.to_string() },
                },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Screen {
        Screen { rect: WmRect { x: 0, y: 0, w: 1920, h: 1080 }, bar_h: 24, dock_h: 48 }
    }
    fn surf(id: u32, role: WmRole) -> WmSurfaceInfo {
        WmSurfaceInfo { surface_id: id, owner_app: format!("org.atrium.app{id}"), owner_uid: 0, role, rect: WmRect { x: 0, y: 0, w: 0, h: 0 } }
    }

    #[test]
    fn click_focuses_an_app_surface_but_not_chrome_or_the_focused_one() {
        use super::daemon::resolve_click_focus;
        let surfaces = [
            surf(1, WmRole::Document),
            surf(2, WmRole::Document),
            surf(9, WmRole::Chrome),     // the bar/dock — never a focus target
        ];
        // Clicking the unfocused document moves focus there.
        assert_eq!(resolve_click_focus(&surfaces, 2, Some(1)), Some(2));
        // Clicking chrome is a no-op (focus stays on the document).
        assert_eq!(resolve_click_focus(&surfaces, 9, Some(1)), None);
        // Clicking the already-focused surface is a no-op (no redundant push).
        assert_eq!(resolve_click_focus(&surfaces, 1, Some(1)), None);
        // Clicking an unknown surface id is a no-op.
        assert_eq!(resolve_click_focus(&surfaces, 99, Some(1)), None);
    }

    #[test]
    fn a_document_fills_the_work_area_and_is_focused() {
        let l = arrange(&screen(), &[surf(1, WmRole::Document)], None);
        assert_eq!(l.focus, 1);
        let s = &l.slots[0];
        assert_eq!(s.rect, screen().work_area());
        assert_eq!(s.rect.y, 24); // below the bar
    }

    /// frescod's trusted strip (F4d) holds the top rows: main.rs starts the
    /// screen below it, and nothing the WM places reaches into it.
    #[test]
    fn nothing_is_placed_in_frescods_reserved_strip() {
        let s = Screen { rect: WmRect { x: 0, y: 24, w: 1920, h: 1056 }, bar_h: 24, dock_h: 48 };
        for role in [WmRole::Background, WmRole::Document, WmRole::Panel, WmRole::Dialog, WmRole::Chrome] {
            let r = rect_for(&s, role);
            assert!(r.y >= 24 && r.y + r.h <= 1080, "{role:?} at {r:?}");
        }
        assert_eq!(rect_for(&s, WmRole::Chrome).y, 24, "the bar sits just below the strip");
        assert_eq!(rect_for(&s, WmRole::Document), WmRect { x: 0, y: 48, w: 1920, h: 1080 - 48 - 48 });
    }

    #[test]
    fn a_dialog_grabs_focus_and_sits_on_top() {
        let l = arrange(&screen(), &[surf(1, WmRole::Document), surf(2, WmRole::Dialog)], None);
        assert_eq!(l.focus, 2, "dialog grabs focus");
        // the dialog slot is the lowest layer (topmost).
        let dialog = l.slots.iter().find(|s| s.surface_id == 2).unwrap();
        let doc = l.slots.iter().find(|s| s.surface_id == 1).unwrap();
        assert!(dialog.layer < doc.layer, "dialog over document");
    }

    #[test]
    fn a_fully_covered_surface_is_marked_non_rendering() {
        // a full-screen document under a full-screen document → the back one gates.
        let mut l = arrange(&screen(), &[surf(1, WmRole::Document)], None);
        // add a second full-area document on top (same work-area rect, lower layer
        // by declaration order won't differ — force a covering slot manually):
        l.slots.push(WmSlot { surface_id: 2, rect: screen().rect, layer: -1 }); // covers all, topmost
        let r = rendering_decisions(&l);
        let back = r.iter().find(|x| x.surface_id == 1).unwrap();
        assert!(!back.rendering, "fully occluded → non-rendering → power-gate");
        let front = r.iter().find(|x| x.surface_id == 2).unwrap();
        assert!(front.rendering, "the top surface renders");
    }

    #[test]
    fn a_partially_covered_surface_keeps_rendering() {
        let layout = WmDeclareLayoutPayload {
            slots: vec![
                WmSlot { surface_id: 1, rect: WmRect { x: 0, y: 0, w: 1000, h: 1080 }, layer: 4 }, // back
                WmSlot { surface_id: 2, rect: WmRect { x: 0, y: 0, w: 500, h: 1080 }, layer: 0 },   // covers left half
            ],
            focus: 1,
            focus_cause: 0,
        };
        let r = rendering_decisions(&layout);
        assert!(r.iter().find(|x| x.surface_id == 1).unwrap().rendering, "partial occlusion still renders (Fresco clips)");
    }

    #[test]
    fn focused_document_renders_and_gates_its_peers() {
        // The F1 default: two documents share the work area; only the focused one
        // is on top + rendering, the other is fully occluded → render-gated.
        let surfaces = [surf(1, WmRole::Document), surf(2, WmRole::Document)];
        let l = arrange(&screen(), &surfaces, Some(2));
        assert_eq!(l.focus, 2);
        let d1 = l.slots.iter().find(|s| s.surface_id == 1).unwrap();
        let d2 = l.slots.iter().find(|s| s.surface_id == 2).unwrap();
        assert!(d2.layer < d1.layer, "focused document sits on top of its peers");
        let r = rendering_decisions(&l);
        assert!(r.iter().find(|x| x.surface_id == 2).unwrap().rendering, "focused document renders");
        assert!(!r.iter().find(|x| x.surface_id == 1).unwrap().rendering, "occluded peer is render-gated");
    }

    #[test]
    fn switching_focus_flips_which_document_is_gated() {
        let surfaces = [surf(1, WmRole::Document), surf(2, WmRole::Document)];
        for (focus, gated) in [(1u32, 2u32), (2, 1)] {
            let r = rendering_decisions(&arrange(&screen(), &surfaces, Some(focus)));
            assert!(r.iter().find(|x| x.surface_id == focus).unwrap().rendering, "focus={focus} renders");
            assert!(!r.iter().find(|x| x.surface_id == gated).unwrap().rendering, "focus={focus}: {gated} gated");
        }
    }

    #[test]
    fn next_document_cycles_and_wraps() {
        use super::daemon::next_document;
        let docs = [1u32, 2, 3];
        assert_eq!(next_document(&docs, 1), Some(2));
        assert_eq!(next_document(&docs, 2), Some(3));
        assert_eq!(next_document(&docs, 3), Some(1)); // wrap
        assert_eq!(next_document(&docs, 9), Some(1)); // current not a document → first
        assert_eq!(next_document(&[], 1), None);      // nothing to cycle
        assert_eq!(next_document(&[5], 5), Some(5));   // single doc → itself (caller no-ops)
    }

    #[test]
    fn workspaces_partition_apps_but_chrome_is_global() {
        use super::daemon::shown_in_workspace;
        use std::collections::HashMap;
        let mut a = HashMap::new();
        a.insert(1u32, 0usize); // doc 1 on workspace 0
        a.insert(2u32, 1usize); // doc 2 on workspace 1
        let (doc1, doc2, bar) = (surf(1, WmRole::Document), surf(2, WmRole::Document), surf(9, WmRole::Chrome));
        // active = 0: doc1 shown, doc2 hidden, chrome global.
        assert!(shown_in_workspace(&doc1, &a, 0));
        assert!(!shown_in_workspace(&doc2, &a, 0));
        assert!(shown_in_workspace(&bar, &a, 0));
        // active = 1: doc2 shown, doc1 hidden, chrome still global.
        assert!(!shown_in_workspace(&doc1, &a, 1));
        assert!(shown_in_workspace(&doc2, &a, 1));
        assert!(shown_in_workspace(&bar, &a, 1));
        // an unassigned app surface defaults to the active workspace (visible).
        assert!(shown_in_workspace(&surf(5, WmRole::Document), &a, 0));
    }

    #[test]
    fn a_panel_docks_beside_the_document_so_both_render() {
        // A panel gets the right-quarter dock (not the full work area), so it
        // doesn't cover the document — both stay visible + rendering.
        let surfaces = [surf(1, WmRole::Document), surf(2, WmRole::Panel)];
        let l = arrange(&screen(), &surfaces, Some(1));
        let wa = screen().work_area();
        let panel = l.slots.iter().find(|s| s.surface_id == 2).unwrap();
        assert!(panel.rect.w < wa.w, "panel is narrower than the work area");
        assert_eq!(panel.rect.x + panel.rect.w, wa.x + wa.w, "panel docked at the right edge");
        let r = rendering_decisions(&l);
        assert!(r.iter().find(|x| x.surface_id == 1).unwrap().rendering, "document still visible beside the panel");
        assert!(r.iter().find(|x| x.surface_id == 2).unwrap().rendering, "panel renders");
    }

    #[test]
    fn split_tiles_documents_side_by_side_all_rendering() {
        // The split intent: two documents become two columns of the work area,
        // both visible (nothing gated) — vs the stacked default.
        let surfaces = [surf(1, WmRole::Document), surf(2, WmRole::Document)];
        let l = arrange_for(&screen(), &surfaces, Some(1), true);
        let wa = screen().work_area();
        let d1 = l.slots.iter().find(|s| s.surface_id == 1).unwrap();
        let d2 = l.slots.iter().find(|s| s.surface_id == 2).unwrap();
        assert_eq!(d1.rect.x, wa.x);
        assert_eq!(d2.rect.x, wa.x + wa.w / 2);
        assert_eq!(d1.rect.x + d1.rect.w, d2.rect.x, "columns abut — no overlap");
        assert_eq!(d1.layer, d2.layer, "tiled docs share a layer (no occlusion)");
        let r = rendering_decisions(&l);
        assert!(r.iter().all(|x| x.rendering), "every tiled document renders");
    }

    // ── the daemon reconcile loop ────────────────────────────────────────────

    use crate::daemon::{FrescoConn, Side, Wm};
    use std::io;

    /// A recorder standing in for the live frescod connection.
    #[derive(Default)]
    struct MockConn {
        surfaces: Vec<WmSurfaceInfo>,
        declared: Option<WmDeclareLayoutPayload>,
        rendering: Vec<WmSetRenderingPayload>,
        modes: Vec<fresco_protocol::WmDisplayMode>,
        mode_set: Option<(u32, u32, u32)>,
        outputs: Vec<fresco_protocol::WmOutput>,
        arranged: Option<Vec<fresco_protocol::WmOutputPlacement>>,
        scaled: Vec<(u64, u32)>,
        mirrored: Vec<(u64, u64)>,
        appearance: Option<fresco_protocol::AppearancePayload>,
        spans: Vec<fresco_protocol::WmSpanGroupPayload>,
    }
    impl FrescoConn for MockConn {
        fn span_group(&mut self, p: fresco_protocol::WmSpanGroupPayload) -> io::Result<fresco_protocol::WmRect> {
            let rects: Vec<fresco_protocol::WmRect> =
                p.members.iter().filter_map(|id| self.outputs.iter().find(|o| o.identity == *id)).map(|o| o.rect).collect();
            let r = if rects.is_empty() {
                fresco_protocol::WmRect { x: 0, y: 0, w: 0, h: 0 }
            } else {
                let x0 = rects.iter().map(|r| r.x).min().unwrap();
                let y0 = rects.iter().map(|r| r.y).min().unwrap();
                let x1 = rects.iter().map(|r| r.x + r.w).max().unwrap() + p.gaps_x.iter().sum::<u32>() as i32;
                let y1 = rects.iter().map(|r| r.y + r.h).max().unwrap() + p.gaps_y.iter().sum::<u32>() as i32;
                fresco_protocol::WmRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
            };
            self.spans.push(p);
            Ok(r)
        }
        fn set_appearance(&mut self, a: fresco_protocol::AppearancePayload) -> io::Result<()> {
            self.appearance = Some(a);
            Ok(())
        }
        fn enumerate(&mut self) -> io::Result<Vec<WmSurfaceInfo>> {
            Ok(self.surfaces.clone())
        }
        fn declare_layout(&mut self, layout: &WmDeclareLayoutPayload) -> io::Result<()> {
            self.declared = Some(layout.clone());
            Ok(())
        }
        fn set_rendering(&mut self, decisions: &[WmSetRenderingPayload]) -> io::Result<()> {
            self.rendering = decisions.to_vec();
            Ok(())
        }
        fn display_modes(&mut self) -> io::Result<fresco_protocol::WmDisplayModesReply> {
            Ok(fresco_protocol::WmDisplayModesReply { modes: self.modes.clone(), current: 0 })
        }
        fn set_display_mode(&mut self, w: u32, h: u32, r: u32) -> io::Result<()> {
            if !self.modes.iter().any(|m| m.width == w && m.height == h) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "no such mode"));
            }
            self.mode_set = Some((w, h, r));
            Ok(())
        }
        fn outputs(&mut self) -> io::Result<Vec<fresco_protocol::WmOutput>> {
            Ok(self.outputs.clone())
        }
        fn arrange_outputs(&mut self, p: Vec<fresco_protocol::WmOutputPlacement>) -> io::Result<()> {
            self.arranged = Some(p);
            Ok(())
        }
        fn set_output_scale(&mut self, identity: u64, scale_milli: u32) -> io::Result<()> {
            self.scaled.push((identity, scale_milli));
            Ok(())
        }
        fn mirror_output(&mut self, identity: u64, of: u64) -> io::Result<()> {
            self.mirrored.push((identity, of));
            Ok(())
        }
    }

    #[test]
    fn display_mode_intents_reach_fresco_and_a_new_screen_relays_out() {
        use fresco_protocol::WmDisplayMode;
        let mut conn = MockConn {
            surfaces: vec![surf(1, WmRole::Document)],
            modes: vec![
                WmDisplayMode { width: 1920, height: 1080, refresh_mhz: 60000, preferred: true },
                WmDisplayMode { width: 1280, height: 720, refresh_mhz: 60000, preferred: false },
            ],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        match handle_intent(&mut wm, &mut conn, Intent::ListDisplayModes) {
            Reply::DisplayModes { modes, current } => { assert_eq!(modes.len(), 2); assert_eq!(current, 0); }
            r => panic!("{r:?}"),
        }
        let r = handle_intent(&mut wm, &mut conn, Intent::SetDisplayMode { width: 1280, height: 720, refresh_mhz: 0 });
        assert_eq!(r, Reply::Ack);
        assert_eq!(conn.mode_set, Some((1280, 720, 0)));
        assert!(conn.declared.is_none(), "the WM waits for frescod's event before re-laying out");
        let r = handle_intent(&mut wm, &mut conn, Intent::SetDisplayMode { width: 800, height: 600, refresh_mhz: 0 });
        assert!(matches!(r, Reply::Err { .. }));

        // frescod's EV_DISPLAY_CHANGED → the WM adopts the screen and re-places.
        let small = Screen { rect: WmRect { x: 0, y: 0, w: 1280, h: 720 }, bar_h: 24, dock_h: 48 };
        let l = wm.set_screen(&mut conn, small).unwrap().expect("relaid out");
        let doc = &l.slots[0];
        assert!(doc.rect.x + doc.rect.w <= 1280 && doc.rect.y + doc.rect.h <= 720, "{:?}", doc.rect);
        // The same screen again, or an unplugged (empty) one, changes nothing.
        assert!(wm.set_screen(&mut conn, small).unwrap().is_none());
        let none = Screen { rect: WmRect { x: 0, y: 0, w: 0, h: 0 }, bar_h: 24, dock_h: 48 };
        assert!(wm.set_screen(&mut conn, none).unwrap().is_none());
        assert_eq!(wm.screen, small);
    }

    #[test]
    fn reconcile_enumerates_arranges_and_declares() {
        let mut conn = MockConn {
            surfaces: vec![surf(1, WmRole::Document), surf(2, WmRole::Dialog)],
            ..Default::default()
        };
        let wm = Wm::new(screen());
        let layout = wm.reconcile(&mut conn).unwrap();

        // it declared exactly what arrange produced, and pushed rendering decisions.
        assert_eq!(conn.declared.as_ref().unwrap().focus, 2, "dialog focused");
        assert_eq!(layout.slots.len(), 2);
        assert_eq!(conn.rendering.len(), 2, "a rendering decision per surface");
    }

    #[test]
    fn focus_intent_steers_the_next_reconcile() {
        // two documents; the human focuses the second.
        let mut conn = MockConn {
            surfaces: vec![surf(1, WmRole::Document), surf(2, WmRole::Document)],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        wm.reconcile(&mut conn).unwrap(); // focus falls to the first document
        assert_eq!(conn.declared.as_ref().unwrap().focus, 1);

        wm.focus(2);
        wm.reconcile(&mut conn).unwrap();
        assert_eq!(conn.declared.as_ref().unwrap().focus, 2, "focus intent honored");
    }

    // ── forum-ctl intents (the chrome-app side) ──────────────────────────────

    use crate::control::handle_intent;
    use forum_ctl::{Intent, Reply};

    #[test]
    fn list_surfaces_intent_returns_the_session_surfaces() {
        let mut conn = MockConn {
            surfaces: vec![surf(1, WmRole::Document), surf(2, WmRole::Panel)],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        match handle_intent(&mut wm, &mut conn, Intent::ListSurfaces) {
            Reply::Surfaces { surfaces, .. } => assert_eq!(surfaces.len(), 2),
            other => panic!("expected Surfaces, got {other:?}"),
        }
    }

    #[test]
    fn focus_intent_from_chrome_refocuses_and_redeclares() {
        // two documents; the dock asks the core to focus #2.
        let mut conn = MockConn {
            surfaces: vec![surf(1, WmRole::Document), surf(2, WmRole::Document)],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        let reply = handle_intent(&mut wm, &mut conn, Intent::Focus { surface_id: 2 });
        assert_eq!(reply, Reply::Ack, "the requested surface got focus");
        // the core re-declared the layout to Fresco with #2 focused.
        assert_eq!(conn.declared.as_ref().unwrap().focus, 2);
    }

    #[test]
    fn switching_workspace_gates_the_other_apps_but_keeps_chrome() {
        let mut conn = MockConn {
            surfaces: vec![
                surf(1, WmRole::Document),
                surf(2, WmRole::Document),
                surf(9, WmRole::Chrome),
            ],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        wm.assign_to_active(1);                       // doc 1 → workspace 0
        wm.switch_workspace(&mut conn, 1).unwrap();   // go to workspace 1
        wm.assign_to_active(2);                        // doc 2 → workspace 1
        wm.reconcile(&mut conn).unwrap();

        // On workspace 1: doc 2 + chrome are declared (active); doc 1 is gated.
        let declared: Vec<u32> =
            conn.declared.as_ref().unwrap().slots.iter().map(|s| s.surface_id).collect();
        assert!(declared.contains(&2) && declared.contains(&9), "active doc + chrome placed");
        assert!(!declared.contains(&1), "the other workspace's doc isn't in the active layout");
        let gated: Vec<u32> =
            conn.rendering.iter().filter(|r| !r.rendering).map(|r| r.surface_id).collect();
        assert_eq!(gated, vec![1], "the inactive workspace's app is render-gated");

        // Switch back to workspace 0 → doc 1 visible, doc 2 now gated, chrome stays.
        wm.switch_workspace(&mut conn, 0).unwrap();
        let gated: Vec<u32> =
            conn.rendering.iter().filter(|r| !r.rendering).map(|r| r.surface_id).collect();
        assert_eq!(gated, vec![2], "switching flips which workspace's apps are gated");
        let declared: Vec<u32> =
            conn.declared.as_ref().unwrap().slots.iter().map(|s| s.surface_id).collect();
        assert!(declared.contains(&9), "chrome is global — present on every workspace");
    }

    #[test]
    fn config_rule_lands_a_known_app_on_its_workspace() {
        use portcullis_peer::AppRegistry;
        // A surface owned by uid 1001, which the launch registry maps to the
        // navigator app; config assigns that app to workspace 1.
        let mut conn = MockConn {
            surfaces: vec![WmSurfaceInfo {
                surface_id: 7,
                owner_app: "client:1".into(),
                owner_uid: 1001,
                role: WmRole::Document,
                rect: WmRect { x: 0, y: 0, w: 0, h: 0 },
            }],
            ..Default::default()
        };
        let mut wm = Wm::new(screen()); // active workspace is 0
        wm.registry = AppRegistry::parse("1001 alice org.atrium.navigator");
        wm.assign_rules.insert("org.atrium.navigator".into(), 1);

        wm.focus_new(&mut conn, 7).unwrap();
        assert_eq!(wm.assignment.get(&7), Some(&1), "the app lands on its configured workspace");
        // It didn't open on the active workspace, so it isn't auto-focused.
        assert_ne!(wm.focus_intent, Some(7));
        // An unknown uid (no registry entry / no rule) falls back to active.
        let mut conn2 = MockConn {
            surfaces: vec![surf(8, WmRole::Document)], // owner_uid 0
            ..Default::default()
        };
        wm.focus_new(&mut conn2, 8).unwrap();
        assert_eq!(wm.assignment.get(&8), Some(&0), "unconfigured app opens on the active workspace");
    }

    #[test]
    fn toggle_split_switches_between_stacked_and_tiled() {
        let mut conn = MockConn {
            surfaces: vec![surf(1, WmRole::Document), surf(2, WmRole::Document)],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        // Stacked default: the unfocused document is render-gated.
        wm.reconcile(&mut conn).unwrap();
        assert_eq!(conn.rendering.iter().filter(|r| !r.rendering).count(), 1, "stacked: 1 gated");
        // Split ON → both documents tile + render.
        assert!(wm.toggle_split(&mut conn).unwrap(), "now tiled");
        assert_eq!(conn.rendering.iter().filter(|r| !r.rendering).count(), 0, "tiled: none gated");
        // Split OFF → back to stacked (1 gated again).
        assert!(!wm.toggle_split(&mut conn).unwrap(), "back to stacked");
        assert_eq!(conn.rendering.iter().filter(|r| !r.rendering).count(), 1, "stacked again");
    }

    #[test]
    fn zoom_fills_the_screen_and_gates_everything_else() {
        let mut conn = MockConn {
            surfaces: vec![
                surf(1, WmRole::Document),
                surf(2, WmRole::Document),
                surf(9, WmRole::Chrome),
            ],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        wm.focus_intent = Some(1);
        // Zoom the focused document.
        assert!(wm.toggle_zoom(&mut conn).unwrap(), "zoomed on");
        let layout = conn.declared.as_ref().unwrap();
        assert_eq!(layout.slots.len(), 1, "only the zoomed surface is laid out");
        assert_eq!(layout.slots[0].surface_id, 1);
        assert_eq!(layout.slots[0].rect, screen().rect, "fills the whole screen (covers chrome)");
        // Everything except the zoomed surface is gated — even global chrome.
        let rendered: Vec<u32> = conn.rendering.iter().filter(|r| r.rendering).map(|r| r.surface_id).collect();
        assert_eq!(rendered, vec![1], "only the zoomed surface renders");
        // Toggle off → back to the normal arrangement (chrome + a doc render again).
        assert!(!wm.toggle_zoom(&mut conn).unwrap(), "zoomed off");
        assert!(conn.declared.as_ref().unwrap().slots.len() > 1, "normal arrangement restored");
    }

    #[test]
    fn move_sends_the_focused_window_to_another_workspace() {
        let mut conn = MockConn {
            surfaces: vec![surf(1, WmRole::Document), surf(2, WmRole::Document)],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        wm.assign_to_active(1); // both docs created on workspace 0
        wm.assign_to_active(2);
        wm.focus_intent = Some(2); // focus doc 2 on workspace 0
        let moved = wm.move_focused_to_workspace(&mut conn, 1).unwrap();
        assert_eq!(moved, Some(2), "the focused doc moved");
        assert_eq!(wm.assignment.get(&2), Some(&1), "doc 2 now belongs to workspace 1");
        assert_eq!(wm.active, 0, "we stay on the current workspace");
        // Doc 2 left workspace 0 → gated; only doc 1 is active.
        let declared: Vec<u32> =
            conn.declared.as_ref().unwrap().slots.iter().map(|s| s.surface_id).collect();
        assert_eq!(declared, vec![1], "only doc 1 remains on the active workspace");
        // Following it to workspace 1 shows doc 2.
        wm.switch_workspace(&mut conn, 1).unwrap();
        let declared: Vec<u32> =
            conn.declared.as_ref().unwrap().slots.iter().map(|s| s.surface_id).collect();
        assert_eq!(declared, vec![2], "doc 2 is on workspace 1");
    }

    #[test]
    fn snap_tiles_two_docs_to_halves_and_gates_the_rest() {
        let mut conn = MockConn {
            surfaces: vec![
                surf(1, WmRole::Document),
                surf(2, WmRole::Document),
                surf(3, WmRole::Document),
            ],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        let wa = screen().work_area();
        // Snap doc 1 left, doc 2 right.
        wm.focus_intent = Some(1);
        wm.snap_focused(&mut conn, Snap::Left).unwrap();
        wm.focus_intent = Some(2);
        wm.snap_focused(&mut conn, Snap::Right).unwrap();
        let layout = conn.declared.as_ref().unwrap();
        let d1 = layout.slots.iter().find(|s| s.surface_id == 1).unwrap();
        let d2 = layout.slots.iter().find(|s| s.surface_id == 2).unwrap();
        assert_eq!(d1.rect.x, wa.x);
        assert_eq!(d2.rect.x, wa.x + wa.w / 2, "right-snapped at the half line");
        assert!(layout.slots.iter().all(|s| s.surface_id != 3), "the un-snapped doc isn't placed");
        // Doc 3 (un-snapped) is render-gated; 1 and 2 render.
        let gated: Vec<u32> = conn.rendering.iter().filter(|r| !r.rendering).map(|r| r.surface_id).collect();
        assert_eq!(gated, vec![3]);
        // Un-snapping doc 2 leaves only doc 1 snapped (still snap mode).
        wm.unsnap_focused(&mut conn).unwrap(); // focus is doc 2
        assert!(!wm.snapped.contains_key(&2));
        assert!(wm.snapped.contains_key(&1));
    }

    #[test]
    fn moving_a_known_app_records_a_persistent_rule() {
        use portcullis_peer::AppRegistry;
        let mut conn = MockConn {
            surfaces: vec![WmSurfaceInfo {
                surface_id: 5,
                owner_app: "client:1".into(),
                owner_uid: 1001,
                role: WmRole::Document,
                rect: WmRect { x: 0, y: 0, w: 0, h: 0 },
            }],
            ..Default::default()
        };
        let mut wm = Wm::new(screen());
        wm.assign_to_active(5);
        wm.registry = AppRegistry::parse("1001 alice org.atrium.term");
        wm.focus_intent = Some(5);
        wm.move_focused_to_workspace(&mut conn, 2).unwrap();
        // The binary persists assign_rules to the state file → survives reboot.
        assert_eq!(
            wm.assign_rules.get("org.atrium.term"),
            Some(&2),
            "moving a known app records its placement for future launches + persistence",
        );
    }

    // ── several monitors ────────────────────────────────────────────────

    fn mon(output: u32, identity: u64, x: i32, w: i32, h: i32) -> fresco_protocol::WmOutput {
        fresco_protocol::WmOutput {
            output, identity, rect: WmRect { x, y: 0, w, h }, refresh_mhz: 60000, present: true,
            scale_milli: 1000, scale_auto: true, mirror_of: 0,
            // A 24" 1080p monitor's 0.275 mm pixels, at any size.
            width_mm: (w as u32 * 11).div_ceil(40), height_mm: (h as u32 * 11).div_ceil(40), flapping: false,
        }
    }

    fn slot(l: &WmDeclareLayoutPayload, id: u32) -> Option<WmRect> {
        l.slots.iter().find(|s| s.surface_id == id).map(|s| s.rect)
    }

    fn two_monitors() -> (Wm, MockConn) {
        let mut conn = MockConn { surfaces: vec![surf(1, WmRole::Document)], ..Default::default() };
        let mut wm = Wm::new(Screen { rect: WmRect { x: 0, y: 0, w: 4480, h: 1440 }, bar_h: 24, dock_h: 48 });
        wm.set_outputs(&mut conn, vec![mon(0, 10, 0, 1920, 1080), mon(1, 20, 1920, 2560, 1440)]).unwrap();
        (wm, conn)
    }

    #[test]
    fn display_memory_round_trips_and_skips_damaged_lines() {
        use crate::displays::{DisplayMemory, Place};
        let mut m = DisplayMemory::default();
        m.remember(&[mon(0, 10, 1920, 1920, 1080), mon(1, 20, 0, 1920, 1080)]);
        m.scales.insert(20, 1500);
        let t = m.to_text();
        assert_eq!(DisplayMemory::from_text(&t), m);
        assert_eq!(m.sets[&vec![10, 20]][0], Place { identity: 10, x: 1920, y: 0, w: 1920, h: 1080 });
        let damaged = format!("{t}set zz@1,2:3x4\nscale 14\ngarbage\n");
        assert_eq!(DisplayMemory::from_text(&damaged), m);
    }

    #[test]
    fn a_set_comes_back_as_it_was_left_even_after_a_restart() {
        // Monitor 20 was put left of 10, then the WM (and frescod) restarted:
        // frescod lays them out left to right again; the WM puts them back.
        let mut wm = Wm::new(screen());
        wm.displays.remember(&[mon(0, 10, 1920, 1920, 1080), mon(1, 20, 0, 1920, 1080)]);
        wm.displays.scales.insert(20, 1500);
        let mut conn = MockConn { surfaces: vec![surf(1, WmRole::Document)], ..Default::default() };
        let fresh = vec![mon(0, 10, 0, 1920, 1080), mon(1, 20, 1920, 1920, 1080)];
        wm.apply_remembered_displays(&mut conn, &fresh, true).unwrap();
        let p = conn.arranged.take().expect("asked to put them back");
        assert!(p.iter().any(|p| p.identity == 20 && p.x == 0) && p.iter().any(|p| p.identity == 10 && p.x == 1920));
        assert_eq!(conn.scaled, vec![(20, 1500)], "the scale set for 20 comes back");
        // Once in place nothing more is asked, and moving them by hand (same
        // set) is remembered, not undone.
        wm.outputs = fresh;
        let moved = vec![mon(0, 10, 0, 1920, 1080), mon(1, 20, 1920, 1920, 1080)];
        wm.set_outputs(&mut conn, vec![mon(0, 10, 1920, 1920, 1080), mon(1, 20, 0, 1920, 1080)]).unwrap();
        assert!(conn.arranged.is_none());
        wm.set_outputs(&mut conn, moved).unwrap();
        assert!(conn.arranged.is_none(), "a move of the same set is the user's: kept");
        assert_eq!(wm.displays.sets[&vec![10, 20]][0].x, 0, "and remembered");
        // A monitor running another size now: its old place may not fit, so
        // frescod's layout stands.
        let other = vec![mon(0, 10, 0, 1920, 1080), mon(1, 20, 1920, 2560, 1440)];
        wm.displays.remember(&[mon(0, 10, 2560, 1920, 1080), mon(1, 20, 0, 1920, 1080)]);
        assert!(wm.displays.restore(&other).is_none());
    }

    #[test]
    fn a_mirror_is_no_monitor_of_its_own_and_comes_back_as_left() {
        use crate::displays::DisplayMemory;
        let mirror = |mut o: fresco_protocol::WmOutput, of| { o.mirror_of = of; o.rect.x = 0; o };
        let (mut wm, mut conn) = two_monitors();
        wm.mirror(&mut conn, 1, Some(0)).unwrap();
        assert_eq!(conn.mirrored, vec![(20, 10)]);
        // frescod applies it: monitor 20 shows 10's part and lays nothing out.
        let both = vec![mon(0, 10, 0, 1920, 1080), mirror(mon(1, 20, 0, 1920, 1080), 10)];
        wm.set_outputs(&mut conn, both.clone()).unwrap();
        assert_eq!(wm.monitors().len(), 1, "a mirror is not a monitor to lay out on");
        let l = wm.reconcile(&mut conn).unwrap();
        assert!(l.slots.iter().all(|s| s.rect.x < 1920), "everything on the source");
        // Remembered with the set, through the file, and put back after a
        // restart that brought the set up unmirrored.
        let m = DisplayMemory::from_text(&wm.displays.to_text());
        assert_eq!(m, wm.displays);
        assert_eq!(m.mirrors[&vec![10, 20]], vec![(20, 10)]);
        let mut wm2 = Wm::new(screen());
        wm2.displays = m;
        let mut conn2 = MockConn::default();
        wm2.outputs = vec![mon(0, 10, 0, 1920, 1080), mon(1, 20, 1920, 1920, 1080)];
        let unmirrored = wm2.outputs.clone();
        wm2.apply_remembered_displays(&mut conn2, &unmirrored, true).unwrap();
        assert_eq!(conn2.mirrored, vec![(20, 10)]);
        assert!(wm2.restoring);
        // It lands (same set): not remembered over, and nothing more asked.
        wm2.set_outputs(&mut conn2, both.clone()).unwrap();
        assert!(!wm2.restoring && conn2.mirrored.len() == 1 && conn2.arranged.is_none());
        // Stopping: told to frescod, remembered once it lands.
        wm.mirror(&mut conn, 1, None).unwrap();
        assert_eq!(conn.mirrored.last(), Some(&(20, 0)));
        wm.set_outputs(&mut conn, vec![mon(0, 10, 0, 1920, 1080), mon(1, 20, 1920, 1920, 1080)]).unwrap();
        assert!(wm.displays.mirrors.get(&vec![10, 20]).is_none());
        assert_eq!(wm.monitors().len(), 2);
    }

    #[test]
    fn setting_a_scale_reaches_fresco_and_is_remembered() {
        let (mut wm, mut conn) = two_monitors();
        wm.set_scale(&mut conn, 1, 1750).unwrap();
        assert_eq!(conn.scaled, vec![(20, 1750)]);
        assert_eq!(wm.displays.scales.get(&20), Some(&1750));
        wm.set_scale(&mut conn, 1, 0).unwrap();
        assert_eq!(wm.displays.scales.get(&20), None, "automatic again");
        assert!(wm.set_scale(&mut conn, 7, 1000).is_err());
    }

    #[test]
    fn each_monitor_shows_its_own_windows_and_chrome_stays_on_the_primary() {
        let (mut wm, mut conn) = two_monitors();
        conn.surfaces.push(surf(9, WmRole::Chrome));
        wm.focus_new(&mut conn, 1).unwrap();
        // Window 1 opened on the primary; send it to the other monitor.
        assert_eq!(wm.move_focused_to_next_monitor(&mut conn).unwrap(), Some(20));
        // A second window opens where you're working: monitor 20.
        conn.surfaces.push(surf(2, WmRole::Document));
        let l = wm.focus_new(&mut conn, 2).unwrap();
        let r2 = slot(&l, 2).unwrap();
        assert!(r2.x >= 1920 && r2.y == 0 && r2.h == 1440, "{r2:?}: no bar or dock off the primary");
        let r9 = slot(&l, 9).unwrap();
        assert!(r9.x == 0 && r9.w == 1920, "{r9:?}: the bar sits on the primary");
        // Focus moves to a window on the primary: monitor 20 keeps showing 2.
        conn.surfaces.push(surf(3, WmRole::Document));
        wm.home.insert(3, 10);
        wm.focus(3);
        let l = wm.reconcile(&mut conn).unwrap();
        assert_eq!(l.focus, 3);
        let on20 = l.slots.iter().filter(|s| s.rect.x >= 1920 && s.layer == DOC_FOCUSED).count();
        assert_eq!(on20, 1, "monitor 20 still has a document on top: {l:?}");
    }

    #[test]
    fn a_window_on_a_monitor_that_left_is_rescued_and_goes_back_when_it_returns() {
        let (mut wm, mut conn) = two_monitors();
        wm.focus_new(&mut conn, 1).unwrap();
        wm.move_focused_to_next_monitor(&mut conn).unwrap();
        let on = |wm: &mut Wm, conn: &mut MockConn| slot(&wm.reconcile(conn).unwrap(), 1).unwrap();
        assert!(on(&mut wm, &mut conn).x >= 1920);
        // Monitor 20 unplugged (frescod drops it after its grace).
        wm.set_outputs(&mut conn, vec![mon(0, 10, 0, 1920, 1080)]).unwrap();
        let r = on(&mut wm, &mut conn);
        assert!(r.x < 1920 && r.x + r.w <= 1920, "rescued onto the primary: {r:?}");
        // It comes back, on another output and to the left this time.
        wm.set_outputs(&mut conn, vec![mon(2, 20, 0, 2560, 1440), mon(0, 10, 2560, 1920, 1080)]).unwrap();
        let r = on(&mut wm, &mut conn);
        assert!(r.x < 2560 && r.w <= 2560, "back on its monitor: {r:?}");
        assert_eq!(wm.home.get(&1), Some(&20));
    }

    #[test]
    fn a_monitor_in_its_grace_shows_nothing_and_keeps_its_windows() {
        let (mut wm, mut conn) = two_monitors();
        wm.focus_new(&mut conn, 1).unwrap();
        wm.move_focused_to_next_monitor(&mut conn).unwrap();
        let mut gone = mon(1, 20, 1920, 2560, 1440);
        gone.present = false;
        wm.set_outputs(&mut conn, vec![mon(0, 10, 0, 1920, 1080), gone]).unwrap();
        let r = slot(&wm.reconcile(&mut conn).unwrap(), 1).unwrap();
        assert!(r.x + r.w <= 1920, "shown on the primary meanwhile: {r:?}");
        assert_eq!(wm.home.get(&1), Some(&20), "and still at home on 20");
    }

    #[test]
    fn zoom_fills_only_its_own_monitor() {
        let (mut wm, mut conn) = two_monitors();
        wm.focus_new(&mut conn, 1).unwrap();
        wm.move_focused_to_next_monitor(&mut conn).unwrap();
        wm.toggle_zoom(&mut conn).unwrap();
        let l = conn.declared.clone().unwrap();
        assert_eq!(slot(&l, 1), Some(WmRect { x: 1920, y: 0, w: 2560, h: 1440 }));
    }

    #[test]
    fn zoom_on_a_span_group_covers_the_group_and_ends_with_the_zoom() {
        let mut conn = MockConn { surfaces: vec![surf(1, WmRole::Document), surf(2, WmRole::Document)], ..Default::default() };
        let mut wm = Wm::new(Screen { rect: WmRect { x: 0, y: 0, w: 5760, h: 1080 }, bar_h: 24, dock_h: 48 });
        let mons = vec![mon(0, 10, 0, 1920, 1080), mon(1, 11, 1920, 1920, 1080), mon(2, 12, 3840, 1920, 1080), mon(3, 13, 5760, 2560, 1440)];
        conn.outputs = mons.clone();
        wm.set_outputs(&mut conn, mons).unwrap();
        // Refused: mixed modes, a mirror-less unknown output.
        assert!(wm.set_span_group(&mut conn, &[2, 3], 0, 0).is_err(), "1080p and 1440p");
        assert!(wm.set_span_group(&mut conn, &[0, 9], 0, 0).is_err(), "no output 9");
        wm.set_span_group(&mut conn, &[0, 1, 2], 40, 0).unwrap();
        assert!(conn.spans.is_empty(), "a group alone spans nothing");
        assert_eq!(wm.span_groups().0, vec![forum_ctl::SpanGroupInfo { members: vec![10, 11, 12], gap: 40, gap_mm: 0 }]);

        wm.focus_new(&mut conn, 1).unwrap();
        wm.toggle_zoom(&mut conn).unwrap();
        let p = conn.spans.last().unwrap().clone();
        assert_eq!((p.window_id, p.members.clone(), p.gaps_x.clone(), p.gaps_y.clone()), (1, vec![10, 11, 12], vec![40, 40], vec![]));
        let l = conn.declared.clone().unwrap();
        assert_eq!(slot(&l, 1), Some(WmRect { x: 0, y: 0, w: 5840, h: 1080 }), "the window covers the group and its bezels");
        assert_eq!(slot(&l, 2), None, "nothing else on the group");
        assert_eq!(wm.span_groups().1, 1);
        // A re-declare asks nothing more.
        let n = conn.spans.len();
        wm.reconcile(&mut conn).unwrap();
        assert_eq!(conn.spans.len(), n);

        // Un-zoom ends it.
        wm.toggle_zoom(&mut conn).unwrap();
        assert_eq!(conn.spans.last().unwrap().members, Vec::<u64>::new());
        assert_eq!(wm.span_groups().1, 0);

        // An app that doesn't span zooms on its monitor alone.
        wm.set_app_spans(&mut conn, "org.atrium.app1", false).unwrap();
        let n = conn.spans.len();
        wm.toggle_zoom(&mut conn).unwrap();
        assert_eq!(conn.spans.len(), n);
        assert_eq!(slot(&conn.declared.clone().unwrap(), 1).map(|r| r.w), Some(1920));
        wm.toggle_zoom(&mut conn).unwrap();
        wm.set_app_spans(&mut conn, "org.atrium.app1", true).unwrap();

        // A member gone for good (no longer on the desktop): the span goes
        // on over the rest, as one new request.
        wm.toggle_zoom(&mut conn).unwrap();
        let fewer = vec![mon(0, 10, 0, 1920, 1080), mon(1, 11, 1920, 1920, 1080), mon(3, 13, 3840, 2560, 1440)];
        conn.outputs = fewer.clone();
        wm.set_outputs(&mut conn, fewer.clone()).unwrap();
        assert_eq!(conn.spans.last().unwrap().members, vec![10, 11]);
        // The group is per set of monitors: the memory round-trips it.
        let m = crate::displays::DisplayMemory::from_text(&wm.displays.to_text());
        assert_eq!(m, wm.displays);
        // A member held out for flapping: one notice naming its output.
        let mut flap = conn.outputs.clone();
        flap[1].flapping = true;
        conn.outputs = flap.clone();
        wm.set_outputs(&mut conn, flap.clone()).unwrap();
        let mut again = flap.clone();
        again[2].present = false;
        wm.set_outputs(&mut conn, again).unwrap();
        assert_eq!(wm.notices.len(), 1, "told once while it stays held");
        assert!(wm.notices[0].1.contains("output 1"), "{:?}", wm.notices);
        wm.notices.clear();
        conn.outputs = fewer.clone();
        wm.set_outputs(&mut conn, fewer.clone()).unwrap();
        // A bezel in millimetres: 22 mm at 0.275 mm a pixel is 80 pixels,
        // asked again at once for the span up.
        wm.set_span_group(&mut conn, &[0, 1], 0, 22).unwrap();
        assert_eq!(wm.span_groups().0[0].gap_mm, 22);
        assert_eq!(conn.spans.last().unwrap().gaps_x, vec![80]);
        let m = crate::displays::DisplayMemory::from_text(&wm.displays.to_text());
        assert_eq!(m, wm.displays);
        // One output alone takes it out of the group.
        wm.set_span_group(&mut conn, &[0, 1, 2], 0, 0).ok();
    }

    #[test]
    fn placing_an_output_asks_frescod_for_its_new_corner() {
        let (mut wm, mut conn) = two_monitors();
        wm.place_output(&mut conn, 1, Side::LeftOf, 0).unwrap();
        assert_eq!(conn.arranged, Some(vec![fresco_protocol::WmOutputPlacement { identity: 20, x: -2560, y: 0 }]));
        wm.place_output(&mut conn, 1, Side::Below, 0).unwrap();
        assert_eq!(conn.arranged, Some(vec![fresco_protocol::WmOutputPlacement { identity: 20, x: 0, y: 1080 }]));
        assert!(wm.place_output(&mut conn, 1, Side::Below, 1).is_err());
        assert!(wm.place_output(&mut conn, 5, Side::Below, 0).is_err());
    }

    #[test]
    fn windows_stay_on_their_monitor_when_a_new_one_becomes_the_primary() {
        let mut conn = MockConn { surfaces: vec![surf(1, WmRole::Document)], ..Default::default() };
        let mut wm = Wm::new(Screen { rect: WmRect { x: 0, y: 0, w: 1920, h: 1080 }, bar_h: 24, dock_h: 48 });
        wm.set_outputs(&mut conn, vec![mon(0, 10, 0, 1920, 1080)]).unwrap();
        wm.focus_new(&mut conn, 1).unwrap();
        // A second monitor arrives to the left: it is now the top-left one.
        wm.set_outputs(&mut conn, vec![mon(1, 20, 0, 2560, 1440), mon(0, 10, 2560, 1920, 1080)]).unwrap();
        let r = slot(&wm.reconcile(&mut conn).unwrap(), 1).unwrap();
        assert!(r.x >= 2560, "window 1 stayed on monitor 10: {r:?}");
    }

    #[test]
    fn the_bar_switch_sets_the_system_appearance() {
        let mut wm = Wm::new(screen());
        let mut conn = MockConn::default();
        assert_eq!(handle_intent(&mut wm, &mut conn, Intent::SetAppearance { dark: true }), Reply::Ack);
        assert_eq!(conn.appearance.map(|a| a.scheme), Some(fresco_protocol::APPEARANCE_DARK));
        handle_intent(&mut wm, &mut conn, Intent::SetAppearance { dark: false });
        assert_eq!(conn.appearance.map(|a| a.scheme), Some(fresco_protocol::APPEARANCE_LIGHT));
    }

#[cfg(test)]
mod pip_tests {
    use super::*;
    use forum_ctl::Corner;

    #[test]
    fn pip_sits_in_its_corner_at_a_quarter_width_and_snaps_to_the_nearest() {
        let wa = WmRect { x: 0, y: 48, w: 1920, h: 984 };
        let r = pip_rect(wa, (16, 9), Corner::BottomRight);
        assert_eq!((r.w, r.h), (437, 246), "height capped at a quarter of the work area");
        assert_eq!((r.x + r.w, r.y + r.h), (1920 - PIP_MARGIN, 48 + 984 - PIP_MARGIN));
        let r = pip_rect(wa, (0, 0), Corner::TopLeft);
        assert_eq!((r.x, r.y), (PIP_MARGIN, 48 + PIP_MARGIN), "no aspect: 16:9");
        // A small screen still gets 160×90 at least.
        let r = pip_rect(WmRect { x: 0, y: 0, w: 400, h: 300 }, (16, 9), Corner::TopLeft);
        assert!(r.w >= 160 && r.h >= 90);
        assert_eq!(nearest_corner(wa, 100, 900), Corner::BottomLeft);
        assert_eq!(nearest_corner(wa, 1800, 100), Corner::TopRight);
        // Above documents and panels, below chrome, HUDs and dialogs.
        assert!(PIP_LAYER < layer(WmRole::Panel) && PIP_LAYER < DOC_FOCUSED);
        assert!(PIP_LAYER > layer(WmRole::Chrome) && PIP_LAYER > layer(WmRole::Dialog) && PIP_LAYER > layer(WmRole::Hud));
    }
}

}
