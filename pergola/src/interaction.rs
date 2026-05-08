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

use crate::event::Event;
use crate::node::NodeId;

pub type ClickHandler = Box<dyn Fn() + 'static>;
/// Receives the full key event so handlers can match by kind, key,
/// modifiers, and consume `chars`. TextField uses this to append
/// text input.
pub type KeyHandler = Box<dyn Fn(&Event) + 'static>;

#[derive(Default)]
pub struct Handlers {
    pub on_click: Option<ClickHandler>,
    pub on_key: Option<KeyHandler>,
    /// When `true`, this node can take keyboard focus. Pointer-down
    /// on a focusable node sets focus; Tab navigates among focusables.
    pub focusable: bool,
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
