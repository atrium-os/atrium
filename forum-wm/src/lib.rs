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
#[derive(Debug, Clone, Copy)]
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
fn layer(role: WmRole) -> i32 {
    match role {
        WmRole::Dialog => 0,
        WmRole::Hud => 1,
        WmRole::Chrome => 2,
        WmRole::Panel => 3,
        WmRole::Document => 4,
        WmRole::Background => 5,
    }
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
    }
}

/// Place the session's surfaces by role → the atomic layout + focus. A dialog
/// grabs focus; else `focus_intent` (if still present); else the topmost document.
pub fn arrange(
    screen: &Screen,
    surfaces: &[WmSurfaceInfo],
    focus_intent: Option<u32>,
) -> WmDeclareLayoutPayload {
    let mut ordered: Vec<&WmSurfaceInfo> = surfaces.iter().collect();
    ordered.sort_by_key(|s| layer(s.role));
    let slots: Vec<WmSlot> = ordered
        .iter()
        .map(|s| WmSlot { surface_id: s.surface_id, rect: rect_for(screen, s.role), layer: layer(s.role) })
        .collect();

    let focus = surfaces
        .iter()
        .find(|s| s.role == WmRole::Dialog)
        .map(|s| s.surface_id)
        .or_else(|| focus_intent.filter(|f| surfaces.iter().any(|s| s.surface_id == *f)))
        .or_else(|| surfaces.iter().find(|s| s.role == WmRole::Document).map(|s| s.surface_id))
        .unwrap_or(0);

    WmDeclareLayoutPayload { slots, focus }
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
pub mod daemon {
    use super::{arrange, rendering_decisions, Screen};
    use fresco_protocol::{WmDeclareLayoutPayload, WmSetRenderingPayload, WmSurfaceInfo};
    use std::io;

    /// The frescod connection seam — the only I/O. Production wraps a
    /// `fresco_client::Connection` (OP_WM_ENUMERATE → reply, OP_WM_DECLARE_LAYOUT,
    /// OP_WM_SET_RENDERING); tests use a recorder. Splitting it keeps the reconcile
    /// loop pure and testable without a live frescod.
    pub trait FrescoConn {
        fn enumerate(&mut self) -> io::Result<Vec<WmSurfaceInfo>>;
        fn declare_layout(&mut self, layout: &WmDeclareLayoutPayload) -> io::Result<()>;
        fn set_rendering(&mut self, decisions: &[WmSetRenderingPayload]) -> io::Result<()>;
    }

    /// The WM core state: the output geometry + the human's focus intent.
    pub struct Wm {
        pub screen: Screen,
        pub focus_intent: Option<u32>,
    }

    impl Wm {
        pub fn new(screen: Screen) -> Self {
            Wm { screen, focus_intent: None }
        }

        /// The human focuses a surface (an intent); the next reconcile applies it.
        pub fn focus(&mut self, surface_id: u32) {
            self.focus_intent = Some(surface_id);
        }

        /// Re-derive and push the layout: enumerate the session's surfaces, arrange
        /// them by role, declare the atomic layout, and gate the fully-occluded
        /// ones. Returns the layout it declared (so the caller can track focus).
        pub fn reconcile(&self, conn: &mut impl FrescoConn) -> io::Result<WmDeclareLayoutPayload> {
            let surfaces = conn.enumerate()?;
            let layout = arrange(&self.screen, &surfaces, self.focus_intent);
            conn.declare_layout(&layout)?;
            conn.set_rendering(&rendering_decisions(&layout))?;
            Ok(layout)
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
        WmSurfaceInfo { surface_id: id, owner_app: format!("org.atrium.app{id}"), role, rect: WmRect { x: 0, y: 0, w: 0, h: 0 } }
    }

    #[test]
    fn a_document_fills_the_work_area_and_is_focused() {
        let l = arrange(&screen(), &[surf(1, WmRole::Document)], None);
        assert_eq!(l.focus, 1);
        let s = &l.slots[0];
        assert_eq!(s.rect, screen().work_area());
        assert_eq!(s.rect.y, 24); // below the bar
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
        };
        let r = rendering_decisions(&layout);
        assert!(r.iter().find(|x| x.surface_id == 1).unwrap().rendering, "partial occlusion still renders (Fresco clips)");
    }

    // ── the daemon reconcile loop ────────────────────────────────────────────

    use crate::daemon::{FrescoConn, Wm};
    use std::io;

    /// A recorder standing in for the live frescod connection.
    #[derive(Default)]
    struct MockConn {
        surfaces: Vec<WmSurfaceInfo>,
        declared: Option<WmDeclareLayoutPayload>,
        rendering: Vec<WmSetRenderingPayload>,
    }
    impl FrescoConn for MockConn {
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
}
