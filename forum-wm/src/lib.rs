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
///
/// Documents split into two layers by focus (see [`slot_layer`]): the *focused*
/// document sits at [`DOC_FOCUSED`] on top of its peers at [`DOC_UNFOCUSED`], so
/// the occlusion pass ([`rendering_decisions`]) render-gates the unfocused ones
/// (they share the work area, fully covered by the focused document). This is the
/// F1 "occluded surface render-gated" tie — the default arrangement is one focused
/// document filling the work area, the rest stacked + gated (forum.md §2.3), not
/// tiled (that's an F2 split/group intent).
const DOC_FOCUSED: i32 = 4;
const DOC_UNFOCUSED: i32 = 5;

fn layer(role: WmRole) -> i32 {
    match role {
        WmRole::Dialog => 0,
        WmRole::Hud => 1,
        WmRole::Chrome => 2,
        WmRole::Panel => 3,
        WmRole::Document => DOC_FOCUSED,
        WmRole::Background => 6,
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
    use super::{arrange, arrange_for, rendering_decisions, Screen};
    use fresco_protocol::{WmDeclareLayoutPayload, WmRole, WmSetRenderingPayload, WmSlot, WmSurfaceInfo};
    use portcullis_peer::AppRegistry;
    use std::collections::{HashMap, HashSet};
    use std::io;

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
        matches!(s.role, WmRole::Chrome | WmRole::Background | WmRole::Hud)
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
    }

    /// The WM core state: output geometry, the human's focus intent, and the
    /// workspace model (N virtual desktops + which is active + each app
    /// surface's workspace assignment).
    pub struct Wm {
        pub screen: Screen,
        pub focus_intent: Option<u32>,
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
    }

    impl Wm {
        pub fn new(screen: Screen) -> Self {
            Wm {
                screen,
                focus_intent: None,
                workspaces: DEFAULT_WORKSPACES,
                active: 0,
                assignment: HashMap::new(),
                assign_rules: HashMap::new(),
                names: Vec::new(),
                registry: AppRegistry::default(),
                tiled_workspaces: HashSet::new(),
                zoomed: None,
            }
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

        /// The workspace-aware placement step shared by every reconcile path:
        /// arrange the surfaces *visible on the active workspace*, declare that
        /// layout, then mark every other surface (apps on inactive workspaces)
        /// non-rendering — so an inactive workspace fully stops compositing.
        fn declare(&self, conn: &mut impl FrescoConn, surfaces: &[WmSurfaceInfo])
            -> io::Result<WmDeclareLayoutPayload>
        {
            let active: Vec<WmSurfaceInfo> = surfaces
                .iter()
                .filter(|s| shown_in_workspace(s, &self.assignment, self.active))
                .cloned()
                .collect();

            // Zoom (fullscreen) short-circuits the normal layout: if a surface on
            // the active workspace is zoomed, it alone fills the whole screen
            // (covering chrome) and everything else render-gates.
            if let Some(zid) = self.zoomed.filter(|z| active.iter().any(|s| s.surface_id == *z)) {
                let layout = WmDeclareLayoutPayload {
                    slots: vec![WmSlot { surface_id: zid, rect: self.screen.rect, layer: 0 }],
                    focus: zid,
                };
                conn.declare_layout(&layout)?;
                let decisions: Vec<WmSetRenderingPayload> = surfaces
                    .iter()
                    .map(|s| WmSetRenderingPayload { surface_id: s.surface_id, rendering: s.surface_id == zid })
                    .collect();
                conn.set_rendering(&decisions)?;
                return Ok(layout);
            }

            let tiled = self.tiled_workspaces.contains(&self.active);
            let layout = arrange_for(&self.screen, &active, self.focus_intent, tiled);
            conn.declare_layout(&layout)?;
            let mut decisions = rendering_decisions(&layout);
            for s in surfaces
                .iter()
                .filter(|s| !shown_in_workspace(s, &self.assignment, self.active))
            {
                decisions.push(WmSetRenderingPayload { surface_id: s.surface_id, rendering: false });
            }
            conn.set_rendering(&decisions)?;
            Ok(layout)
        }

        /// Re-derive and push the layout for the active workspace: enumerate,
        /// arrange the visible surfaces, declare the atomic layout, gate the
        /// occluded ones + every inactive-workspace surface. Returns the
        /// declared (active-workspace) layout.
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
                // A new document opens focused — but only if it landed on the
                // workspace you're looking at (a rule may send it elsewhere).
                if s.role == WmRole::Document && ws == self.active {
                    self.focus_intent = Some(new_id);
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
}
