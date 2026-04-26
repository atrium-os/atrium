//! Multi-window state — phase B1 of the Fresco compositor protocol.
//!
//! Currently this module is **skeleton only**. The data shapes are
//! defined and the wire opcodes are reserved (see protocol.rs), but
//! the actual per-window dispatch lives in a follow-up. Existing
//! single-window apps continue to use the global SceneGraph + SlotTable.
//!
//! Migration plan:
//!   B1a: refactor frontend.rs so slot/frame ops route through
//!        `Compositor::current_window`; keep one implicit `Window 0`
//!        for backwards-compat with apps that don't call CREATE_WINDOW.
//!   B1b: add server-side window decorations (title bar, close button,
//!        frame border) — drawn by the compositor, not the client.
//!   B1c: input routing by geometry + focus management (click-to-focus,
//!        drag-via-titlebar to move). Mouse events go to the window
//!        under the cursor; keyboard goes to the focused window.

#![allow(dead_code)]   // skeleton: fields will be used as B1 lands

use crate::scene::graph::SceneGraph;
use crate::scene::slots::SlotTable;

/// Server-assigned window identifier. Stable for the window's
/// lifetime, opaque to clients (they only know the ones the server
/// gave them).
///
/// u16 (max 65535 windows) so it fits in `Command::flags` — slot
/// and frame opcodes reuse that field as a window selector. If we
/// ever blow past 65k concurrent windows we'll widen the field.
pub type WindowId = u16;

/// Client identifier — assigned by the kernel module on cdev open()
/// in phase B2. In phase B1 there's only one client (the existing
/// single-process model), so the field exists but is always 0.
pub type ClientId = u32;

/// One renderable window. Owns its scene graph and slot namespace.
pub struct Window {
    pub id:     WindowId,
    pub owner:  ClientId,
    /// Pixel-space rect on the screen. Server controls this; clients
    /// learn it via COMP_WINDOW_RESIZED.
    pub pos:    (f32, f32),
    pub size:   (u32, u32),
    pub title:  String,
    /// Per-window scene root. Slot IDs are scoped to this window —
    /// slot 1 here is unrelated to slot 1 in another window.
    pub slots:  SlotTable,
    pub scene:  SceneGraph,
    pub focus:  bool,
    pub z:      u32,        // z-order; higher = on top
}

impl Window {
    pub fn new(id: WindowId, owner: ClientId, size: (u32, u32)) -> Self {
        Self {
            id,
            owner,
            pos:   (0.0, 0.0),
            size,
            title: String::new(),
            slots: SlotTable::new(),
            scene: SceneGraph::new(),
            focus: false,
            z:     0,
        }
    }
}

/// Server-side compositor state. Holds the windows owned by all
/// clients, dispatches commands to the right window, and tracks
/// global UI state (focus, cursor position, z-order).
pub struct Compositor {
    pub windows:  std::collections::HashMap<WindowId, Window>,
    pub z_order:  Vec<WindowId>,
    pub focus:    Option<WindowId>,
    pub cursor:   (f32, f32),
    pub next_id:  WindowId,
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            windows:  std::collections::HashMap::new(),
            z_order:  Vec::new(),
            focus:    None,
            cursor:   (0.0, 0.0),
            next_id:  1,            // 0 reserved for "implicit window" backcompat
        }
    }

    pub fn create(&mut self, owner: ClientId, size: (u32, u32)) -> WindowId {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut w = Window::new(id, owner, size);
        w.z = self.z_order.len() as u32;
        self.windows.insert(id, w);
        self.z_order.push(id);
        id
    }

    /// Get a mutable reference to a window, auto-creating it if it
    /// doesn't exist. Used during the B1a transition: legacy single-
    /// window apps emit `flags = 0` on every slot/frame op and we
    /// transparently route into an implicit window 0.
    pub fn window_mut(&mut self, id: WindowId, owner: ClientId,
                       default_size: (u32, u32)) -> &mut Window {
        if !self.windows.contains_key(&id) {
            let mut w = Window::new(id, owner, default_size);
            w.z = self.z_order.len() as u32;
            self.windows.insert(id, w);
            self.z_order.push(id);
            if id >= self.next_id { self.next_id = id.saturating_add(1); }
        }
        self.windows.get_mut(&id).unwrap()
    }

    pub fn destroy(&mut self, id: WindowId, by: ClientId) -> bool {
        let allowed = self.windows.get(&id).map(|w| w.owner == by).unwrap_or(false);
        if !allowed { return false; }
        self.windows.remove(&id);
        self.z_order.retain(|&w| w != id);
        if self.focus == Some(id) { self.focus = self.z_order.last().copied(); }
        true
    }

    /// Phase-B1c entry point: which window contains (x, y) at the top
    /// of the z-order? Returns None if none.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<WindowId> {
        for &id in self.z_order.iter().rev() {
            if let Some(w) = self.windows.get(&id) {
                let (wx, wy) = w.pos;
                let (ww, wh) = (w.size.0 as f32, w.size.1 as f32);
                if x >= wx && x < wx + ww && y >= wy && y < wy + wh {
                    return Some(id);
                }
            }
        }
        None
    }
}
