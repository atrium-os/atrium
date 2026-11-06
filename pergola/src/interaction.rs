//! Handlers attached to nodes during a render pass.
//!
//! Each `View::render` rebuilds the handler table from scratch,
//! parallel to the `NodeTree`. The App keeps the most recent table
//! and uses it to dispatch events.
//!
//! Phase-3 ships the `on_click` handler — fired on `PointerUp` when
//! `PointerDown` happened on the same node. Keyboard handlers
//! (`on_key`, `on_char`) and focus management arrive in phase-3.5.

use std::collections::HashMap;

use crate::event::{Event, Key, Modifiers};
use crate::geom::Point;
use crate::node::{Node, NodeId};

pub type ClickHandler = Box<dyn Fn() + 'static>;
/// Receives the full key event so handlers can match by kind, key,
/// modifiers, and consume `chars`. TextField uses this to append
/// text input.
pub type KeyHandler = Box<dyn Fn(&Event) + 'static>;

/// What a scroll view is asked to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollKind {
    /// A wheel or trackpad delta (px) with the pointer at `at`.
    Wheel { at: Option<Point>, dx: f32, dy: f32, modifiers: Modifiers },
    /// A navigation key: arrows, Page Up/Down, Home/End, Space, and with
    /// Ctrl the zoom keys (`+`, `-`, `0` arrive as `Key::Char` + `chars`).
    Key { key: Key, chars: char, modifiers: Modifiers },
    /// A press inside the view (catches a fling).
    Press,
}

/// A scroll input, with the clock and the view's node as last laid out
/// (its box and `content_h`).
pub struct ScrollInput<'a> {
    pub kind: ScrollKind,
    /// App time, ms.
    pub t: f64,
    pub node: &'a Node,
}

pub type ScrollHandler = Box<dyn Fn(&ScrollInput) + 'static>;
/// Called once per frame while the app animates, with the app time (ms)
/// and the node as last laid out. Returns whether it still needs frames.
pub type FrameHandler = Box<dyn Fn(f64, &Node) -> bool + 'static>;

#[derive(Default)]
pub struct Handlers {
    pub on_click: Option<ClickHandler>,
    pub on_key: Option<KeyHandler>,
    /// When `true`, this node can take keyboard focus. Pointer-down
    /// on a focusable node sets focus; Tab navigates among focusables.
    pub focusable: bool,
    /// Wheel, navigation keys and presses for a scroll view.
    pub on_scroll: Option<ScrollHandler>,
    /// A per-frame step (a fling, an eased jump, a zoom settling).
    pub on_frame: Option<FrameHandler>,
    /// A drop from another window landed on this node (tabula.md §4).
    pub on_drop: Option<ClickHandler>,
    /// This node is where another process's surface is embedded (a Limen
    /// slot, e.g. Arca's credential field): the App reports its window
    /// rect to the binder whenever it changes ([`crate::slot`]).
    pub slot: Option<crate::slot::SlotKey>,
}

impl Handlers {
    pub fn new() -> Self { Self::default() }
}

/// The handler set for a single render pass — keyed by NodeId.
#[derive(Default)]
pub struct Interactions {
    pub by_node: HashMap<NodeId, Handlers>,
}

impl Interactions {
    pub fn new() -> Self { Self::default() }

    /// Get-or-insert the Handlers for `id`. Used by `Ctx::on_click`
    /// and the (future) `on_key` to attach handlers without
    /// requiring nodes to know they're interactive at construction
    /// time.
    pub fn entry(&mut self, id: NodeId) -> &mut Handlers {
        self.by_node.entry(id).or_default()
    }

    pub fn get(&self, id: NodeId) -> Option<&Handlers> {
        self.by_node.get(&id)
    }
}
