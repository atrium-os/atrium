//! Shared pointer-event dispatcher.
//!
//! Source-agnostic: `pointer_uhid_reader` (the only pointer source
//! today) feeds cooked events — cursor position, HID button
//! presses, scroll deltas — into this module. Future readers (real
//! HID descriptor parser, sysmouse, etc.) plug in the same way.
//! Dispatch handles:
//!
//!   - cursor-state update (shared with the render loop's overlay)
//!   - WM intercepts on primary-press (close button → resize edge →
//!     titlebar drag → content click+raise+focus)
//!   - drag/resize state machine (consumes motion + emits
//!     COMP_WINDOW_RESIZED on completion)
//!   - per-window routing (events stamped with target window_id, sent
//!     only to the owning client; unrouted events fall back to broadcast)
//!
//! The wire never carries Linux-shape constants — `hid_button`
//! parameters here are USB HID Usage Page 0x09 button numbers
//! (1=primary, 2=secondary, 3=middle, …); source readers translate
//! their dialect at the boundary.

use crate::cursor::CursorState;
use crate::input_reader::SharedModifiers;

use fresco_server::command::protocol::{
    Completion, COMP_INPUT_MOUSE_BUTTON, COMP_INPUT_MOUSE_MOVE, COMP_INPUT_SCROLL,
    COMP_WINDOW_CLOSE_REQUESTED, COMP_WINDOW_FOCUS, COMP_WINDOW_RESIZED,
};
use fresco_server::window::{
    self as wm_mod, Compositor as WmCompositor, FocusChange, ResizeAnchor,
    MIN_WINDOW_H, MIN_WINDOW_W, RESIZE_EDGE_B, RESIZE_EDGE_L, RESIZE_EDGE_R, RESIZE_EDGE_T,
};

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub type EventSubs = Arc<Mutex<Vec<(u8, Sender<Completion>)>>>;

/// HID Usage Page 0x09 button numbers — the wire-shape values.
pub const HID_BTN_PRIMARY:   u16 = 1;
pub const HID_BTN_SECONDARY: u16 = 2;
pub const HID_BTN_MIDDLE:    u16 = 3;

#[derive(Clone)]
pub struct Dispatcher {
    pub event_subs: EventSubs,
    pub cursor:     Arc<Mutex<CursorState>>,
    pub wm:         Arc<Mutex<WmCompositor>>,
    pub modifiers:  SharedModifiers,
    pub screen_w:   u32,
    pub screen_h:   u32,
}

impl Dispatcher {
    /// Set the cursor to an absolute screen-pixel position. Updates
    /// the shared `CursorState` (so the overlay tracks), runs the
    /// drag/resize state machine if active (mutating `Window.pos` /
    /// `Window.size` in lieu of forwarding), or routes a normal
    /// `COMP_INPUT_MOUSE_MOVE` to the hit window's owner.
    pub fn cursor_to(&self, x: f32, y: f32) {
        let xc = x.clamp(0.0, self.screen_w as f32);
        let yc = y.clamp(0.0, self.screen_h as f32);
        {
            let mut c = self.cursor.lock().unwrap();
            c.x = xc;
            c.y = yc;
            c.visible = true;
        }
        // Drag/resize: mutate window state under WM lock, do NOT
        // forward MOUSE_MOVE — the WM is consuming.
        let consumed = {
            let mut g = self.wm.lock().unwrap();
            if let Some((id, ox, oy)) = g.dragging {
                if let Some(win) = g.windows.get_mut(&id) {
                    win.pos = (xc + ox, yc + oy);
                }
                true
            } else if let Some(anchor) = g.resizing {
                let dx = xc - anchor.start_cursor.0;
                let dy = yc - anchor.start_cursor.1;
                let mut new_x = anchor.start_pos.0;
                let mut new_y = anchor.start_pos.1;
                let mut new_w = anchor.start_size.0;
                let mut new_h = anchor.start_size.1;
                if anchor.edges & RESIZE_EDGE_L != 0 {
                    let nw = (anchor.start_size.0 - dx).max(MIN_WINDOW_W);
                    new_x = anchor.start_pos.0 + (anchor.start_size.0 - nw);
                    new_w = nw;
                }
                if anchor.edges & RESIZE_EDGE_R != 0 {
                    new_w = (anchor.start_size.0 + dx).max(MIN_WINDOW_W);
                }
                if anchor.edges & RESIZE_EDGE_T != 0 {
                    let nh = (anchor.start_size.1 - dy).max(MIN_WINDOW_H);
                    new_y = anchor.start_pos.1 + (anchor.start_size.1 - nh);
                    new_h = nh;
                }
                if anchor.edges & RESIZE_EDGE_B != 0 {
                    new_h = (anchor.start_size.1 + dy).max(MIN_WINDOW_H);
                }
                if let Some(win) = g.windows.get_mut(&anchor.id) {
                    win.pos  = (new_x, new_y);
                    win.size = (new_w, new_h);
                }
                true
            } else {
                false
            }
        };
        if consumed { return; }

        let target = self.hit_window(xc, yc);
        let mut result_hash = [0u8; 32];
        result_hash[0..4].copy_from_slice(&xc.to_le_bytes());
        result_hash[4..8].copy_from_slice(&yc.to_le_bytes());
        self.route(target, Completion {
            comp_type: COMP_INPUT_MOUSE_MOVE,
            status:    0,
            id:        target,
            result_hash,
            _pad:      [0u32; 22],
        });
    }

    /// Button press/release at the current cursor position. `hid_button`
    /// is a USB HID Usage Page 0x09 button number — translate from the
    /// source dialect before calling. Implements the same intercept
    /// priority as `main_macos.rs::handle_wm_event`.
    pub fn button(&self, hid_button: u16, pressed: bool) {
        let (cx, cy) = {
            let c = self.cursor.lock().unwrap();
            (c.x, c.y)
        };

        if hid_button == HID_BTN_PRIMARY && pressed {
            let mut g = self.wm.lock().unwrap();
            if let Some(close_id) = wm_mod::hit_close_button(&g, cx, cy) {
                let owner = g.windows.get(&close_id)
                    .map(|w| w.owner as u8).unwrap_or(0);
                drop(g);
                self.send_to_owner(owner, Completion {
                    comp_type:   COMP_WINDOW_CLOSE_REQUESTED,
                    status:      0,
                    id:          close_id as u32,
                    result_hash: [0u8; 32],
                    _pad:        [0u32; 22],
                });
                return;
            }
            if let Some((id, edges)) = g.hit_resize_edge(cx, cy) {
                let win = &g.windows[&id];
                g.resizing = Some(ResizeAnchor {
                    id, edges,
                    start_cursor: (cx, cy),
                    start_pos:    win.pos,
                    start_size:   win.size,
                });
                let change = g.raise(id);
                drop(g);
                self.announce_focus(change);
                return;
            }
            if let Some(id) = g.hit_titlebar(cx, cy) {
                let win = &g.windows[&id];
                let offset = (win.pos.0 - cx, win.pos.1 - cy);
                g.dragging = Some((id, offset.0, offset.1));
                let change = g.raise(id);
                drop(g);
                self.announce_focus(change);
                return;
            }
            if let Some(id) = g.hit_content(cx, cy) {
                let change = g.raise(id);
                drop(g);
                self.announce_focus(change);
                // fall through: forward as MOUSE_BUTTON to the owner
            }
        }

        if hid_button == HID_BTN_PRIMARY && !pressed {
            let mut g = self.wm.lock().unwrap();
            if g.dragging.take().is_some() {
                return;
            }
            if let Some(anchor) = g.resizing.take() {
                let (w_px, h_px, owner) = g.windows.get(&anchor.id)
                    .map(|w| (w.size.0 as u32, w.size.1 as u32, w.owner as u8))
                    .unwrap_or((0, 0, 0));
                drop(g);
                if w_px > 0 && h_px > 0 {
                    let mut rh = [0u8; 32];
                    rh[0..4].copy_from_slice(&w_px.to_le_bytes());
                    rh[4..8].copy_from_slice(&h_px.to_le_bytes());
                    self.send_to_owner(owner, Completion {
                        comp_type:   COMP_WINDOW_RESIZED,
                        status:      0,
                        id:          anchor.id as u32,
                        result_hash: rh,
                        _pad:        [0u32; 22],
                    });
                }
                return;
            }
        }

        let target = self.hit_window(cx, cy);
        let mods = *self.modifiers.lock().unwrap();
        let mut result_hash = [0u8; 32];
        result_hash[0..2].copy_from_slice(&hid_button.to_le_bytes());
        result_hash[2] = mods;
        result_hash[4..8].copy_from_slice(&cx.to_le_bytes());
        result_hash[8..12].copy_from_slice(&cy.to_le_bytes());
        self.route(target, Completion {
            comp_type: COMP_INPUT_MOUSE_BUTTON,
            status:    if pressed { 1 } else { 0 },
            id:        target,
            result_hash,
            _pad:      [0u32; 22],
        });
    }

    pub fn scroll(&self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 { return; }
        let (cx, cy) = {
            let c = self.cursor.lock().unwrap();
            (c.x, c.y)
        };
        let target = self.hit_window(cx, cy);
        let mut result_hash = [0u8; 32];
        result_hash[0..4].copy_from_slice(&dx.to_le_bytes());
        result_hash[4..8].copy_from_slice(&dy.to_le_bytes());
        self.route(target, Completion {
            comp_type: COMP_INPUT_SCROLL,
            status:    0,
            id:        target,
            result_hash,
            _pad:      [0u32; 22],
        });
    }

    // ── routing helpers ──────────────────────────────────────────

    fn hit_window(&self, x: f32, y: f32) -> u32 {
        self.wm.lock().unwrap().hit_test(x, y).map(|w| w as u32).unwrap_or(0)
    }

    fn route(&self, window_id: u32, comp: Completion) {
        if window_id == 0 {
            self.broadcast(comp);
            return;
        }
        let owner = match self.wm.lock().unwrap().windows.get(&(window_id as u16)) {
            Some(w) => w.owner as u8,
            None => return,
        };
        let mut subs = self.event_subs.lock().unwrap();
        subs.retain(|(client_id, tx)| {
            if *client_id == owner { tx.send(comp).is_ok() } else { true }
        });
    }

    fn broadcast(&self, comp: Completion) {
        let mut subs = self.event_subs.lock().unwrap();
        subs.retain(|(_, tx)| tx.send(comp).is_ok());
    }

    fn send_to_owner(&self, owner: u8, comp: Completion) {
        let mut subs = self.event_subs.lock().unwrap();
        subs.retain(|(client_id, tx)| {
            if *client_id == owner { tx.send(comp).is_ok() } else { true }
        });
    }

    fn announce_focus(&self, change: Option<FocusChange>) {
        let Some(fc) = change else { return; };
        if let Some(prev) = fc.prev {
            self.broadcast(Completion {
                comp_type:   COMP_WINDOW_FOCUS,
                status:      0,
                id:          prev as u32,
                result_hash: [0u8; 32],
                _pad:        [0u32; 22],
            });
        }
        self.broadcast(Completion {
            comp_type:   COMP_WINDOW_FOCUS,
            status:      1,
            id:          fc.new as u32,
            result_hash: [0u8; 32],
            _pad:        [0u32; 22],
        });
    }
}
