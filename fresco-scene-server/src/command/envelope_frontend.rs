//! Aqueduct-envelope command dispatcher for fresco-protocol's
//! CLASS_DISPLAY dictionary.
//!
//! Replaces the legacy 128-byte `Command`/`Completion` `CommandFrontend`
//! at the M2 hard cutover. Built alongside the legacy frontend during
//! M2.6a-d (this milestone); legacy code is deleted in a single
//! atomic cutover commit alongside the demo + socket-lib + frescod
//! rewrites (M2.7).
//!
//! Design (per `docs/spec/fresco-production-rollout.md` M2.6 conversation):
//!
//! - **Per-window scene state** (`WindowSceneState`) lives keyed by
//!   `WindowId`. Each window's `rect_nodes` and `texture_nodes` maps
//!   are direct projections of the wire-format `OP_SCENE_NODE_SET`
//!   stream — no intermediate SceneGraph traversal. This is how the
//!   per-node-delta model maps onto HeadlessRenderer's
//!   `set_rect_nodes(Vec<SceneNode>)` / `set_texture_batches(...)`
//!   API at zero translation cost.
//!
//! - **Compositor + CasStore are shared with the legacy frontend**
//!   during the transition — same `Arc<Mutex<...>>` references. Both
//!   dispatchers see the same window registry, ownership, focus, and
//!   CAS cache. After the M2.7 cutover, legacy frontend is gone and
//!   only EnvelopeFrontend touches them.
//!
//! - **Per-client window ownership** is enforced at every routable
//!   dispatch (matches legacy frontend's pattern at
//!   `command/frontend.rs:119`). Ops carry their target `window_id`
//!   in the envelope's `flags` field; the dispatcher rejects any op
//!   whose target window isn't owned by the dispatching client.
//!
//! - **Scene frame boundaries** (`OP_SCENE_FRAME_BEGIN/END`) bracket
//!   batches of node updates. Renderer reads `WindowSceneState` only
//!   between frames; `in_frame` flag tracks the boundary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aqueduct::{Message, MessageKind};
use fresco_protocol::{
    control, decode, scene_ops,
    SceneNodeSetPayload, SceneNodeClearPayload,
    SlotSetPayload, SlotClearPayload,
    RectParams, TextureParams,
    WindowCreatePayload, WindowDestroyPayload,
    WindowSetTitlePayload, WindowSetHintsPayload,
    WindowRequestClosePayload, WindowPresentPayload,
};

use crate::cas::store::CasStore;
use crate::window::Compositor;

/// Per-window retained-mode scene state, populated by the wire-op
/// stream and consumed by the renderer each frame.
///
/// `rect_nodes` and `texture_nodes` are keyed by the wire-format
/// `node_id` (a `u32` chosen by the client). Last-write-wins on
/// repeated SET to the same node_id; CLEAR removes.
///
/// `in_frame` tracks whether we're between FRAME_BEGIN and FRAME_END.
/// Future: surfacing this to the renderer lets it skip render until
/// the client commits a frame, avoiding partial-state flickers. POC
/// for now treats it as informational only.
#[derive(Default)]
pub struct WindowSceneState {
    pub rect_nodes:    HashMap<u32, RectParams>,
    pub texture_nodes: HashMap<u32, TextureParams>,
    pub slot_table:    HashMap<u32, [u8; 32]>,
    pub in_frame:      bool,
}

impl WindowSceneState {
    pub fn new() -> Self { Self::default() }

    /// Snapshot the rect-op nodes as `fresco_vulkan::SceneNode` in
    /// insertion-order-undefined sequence. Renderer feeds this to
    /// `HeadlessRenderer::set_rect_nodes`.
    ///
    /// Returns a fresh `Vec` each call — the renderer takes ownership.
    /// Cost: O(N_rects) to collect; called per-frame, so this is
    /// expected to be in the µs range for typical scenes.
    pub fn extract_rect_nodes(&self) -> Vec<fresco_vulkan::SceneNode> {
        self.rect_nodes.values().map(|p| fresco_vulkan::SceneNode {
            position: [p.x, p.y],
            size:     [p.w, p.h],
            color:    [p.r, p.g, p.b, p.a],
        }).collect()
    }

    /// Group texture ops by `slot_id` into `TextureBatch`es. The
    /// renderer issues one dispatch+draw cycle per batch since each
    /// batch uses a different bound texture.
    pub fn extract_texture_batches(&self) -> Vec<fresco_vulkan::TextureBatch> {
        let mut by_slot: HashMap<u32, Vec<fresco_vulkan::TextureNode>> =
            HashMap::new();
        for p in self.texture_nodes.values() {
            by_slot.entry(p.slot_id).or_default().push(fresco_vulkan::TextureNode {
                model: [p.x, p.y, p.w, p.h],
            });
        }
        by_slot.into_iter()
            .map(|(slot_id, nodes)| fresco_vulkan::TextureBatch { slot_id, nodes })
            .collect()
    }
}

// ── Dispatcher ───────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DispatchError {
    /// Op's target window is not owned by the dispatching client.
    NotOwner,
    /// Op references a window that doesn't exist in the compositor.
    UnknownWindow,
    /// Payload failed to decode at the postcard layer.
    BadPayload,
    /// Op-id outside the CLASS_DISPLAY dictionary. Caller should
    /// either dispatch to a different class handler or bounce.
    UnknownOp(u16),
}

/// One outbound message produced by dispatch — either a response to a
/// client-initiated op (e.g. WINDOW_CREATE returning the new window_id)
/// or an async server-driven event (resize, focus). Caller serializes
/// these onto the wire as aqueduct envelopes.
#[derive(Debug)]
pub struct Outbound {
    pub op:      u16,
    pub flags:   u16,
    pub payload: Vec<u8>,
}

pub struct EnvelopeFrontend {
    /// CAS store shared with legacy frontend. Will become solely owned
    /// by EnvelopeFrontend after the M2.7 cutover deletes the legacy
    /// code path.
    cas: Arc<Mutex<CasStore>>,

    /// Window registry, ownership, focus, z-order. Shared with legacy
    /// frontend during transition.
    compositor: Arc<Mutex<Compositor>>,

    /// Per-window retained-mode scene state. Keyed by `WindowId`
    /// (`u16` natively in the compositor; `u32` on the wire to allow
    /// for future expansion). We canonicalise to `u32` here.
    pub per_window: HashMap<u32, WindowSceneState>,

    /// Slot id of the client whose op is currently being dispatched.
    /// Set by `dispatch()`; consulted by handlers for ownership checks.
    current_client: u8,
}

impl EnvelopeFrontend {
    pub fn new(cas: Arc<Mutex<CasStore>>,
               compositor: Arc<Mutex<Compositor>>) -> Self {
        Self {
            cas, compositor,
            per_window: HashMap::new(),
            current_client: 0,
        }
    }

    /// Dispatch one inbound message. Returns any outbound messages
    /// generated (responses + async events).
    ///
    /// Errors are non-fatal — a single misbehaved op shouldn't kill
    /// the connection. The dispatcher returns `Err` and the caller
    /// (frescod's connection thread) decides whether to log + drop
    /// the message or escalate.
    pub fn dispatch(&mut self, msg: &Message, client_id: u8)
        -> Result<Vec<Outbound>, DispatchError>
    {
        self.current_client = client_id;

        /* Routable opcodes carry their target `window_id` in
         * `msg.flags` (lower bits). 0 = "screen" / default-window,
         * non-zero = explicit window. Window-management ops carry
         * their window_id in the payload, not flags. */
        match msg.op {
            // ── Slot / scene / frame ops (routable; window = msg.flags) ──
            control::OP_SLOT_SET           => self.handle_slot_set(msg),
            control::OP_SLOT_CLEAR         => self.handle_slot_clear(msg),
            control::OP_SCENE_FRAME_BEGIN  => self.handle_scene_frame_begin(msg),
            control::OP_SCENE_FRAME_END    => self.handle_scene_frame_end(msg),
            control::OP_SCENE_NODE_SET     => self.handle_scene_node_set(msg),
            control::OP_SCENE_NODE_CLEAR   => self.handle_scene_node_clear(msg),

            // ── Window management ops (window_id is in payload) ──
            control::OP_WINDOW_CREATE        => self.handle_window_create(msg),
            control::OP_WINDOW_DESTROY       => self.handle_window_destroy(msg),
            control::OP_WINDOW_SET_TITLE     => self.handle_window_set_title(msg),
            control::OP_WINDOW_SET_HINTS     => self.handle_window_set_hints(msg),
            control::OP_WINDOW_REQUEST_CLOSE => self.handle_window_request_close(msg),
            control::OP_WINDOW_PRESENT       => self.handle_window_present(msg),

            op => Err(DispatchError::UnknownOp(op)),
        }
    }

    /// Returns the current per-window state (read-only) for the
    /// renderer to extract render input. `None` if `window_id` doesn't
    /// have any state yet (no SCENE_NODE_SET seen).
    pub fn window_state(&self, window_id: u32) -> Option<&WindowSceneState> {
        self.per_window.get(&window_id)
    }

    /// Mutable variant for code paths that need to touch state outside
    /// the dispatch() flow (cleanup on client disconnect, etc).
    pub fn window_state_mut(&mut self, window_id: u32) -> Option<&mut WindowSceneState> {
        self.per_window.get_mut(&window_id)
    }

    /// Drop all state for a window (called on WINDOW_DESTROY or on
    /// client disconnect).
    pub fn forget_window(&mut self, window_id: u32) {
        self.per_window.remove(&window_id);
    }

    // ── Routable-op ownership check ──────────────────────────────────

    /// Resolve `msg.flags` into a target window_id and verify the
    /// current client owns it. Returns `(window_id, owner)` on success.
    fn check_routable(&self, msg: &Message) -> Result<u32, DispatchError> {
        let win_id = msg.flags as u32;  /* 0 = screen / default */
        if win_id == 0 {
            /* Window 0 is the implicit "screen" window — always
             * dispatchable without ownership check. */
            return Ok(0);
        }
        let comp = self.compositor.lock().unwrap();
        let win = comp.windows.get(&(win_id as u16))
            .ok_or(DispatchError::UnknownWindow)?;
        if win.owner != self.current_client as u32 {
            return Err(DispatchError::NotOwner);
        }
        Ok(win_id)
    }

    /// Get-or-create per-window state map entry for `window_id`.
    fn ensure_window_state(&mut self, window_id: u32) -> &mut WindowSceneState {
        self.per_window.entry(window_id).or_default()
    }

    // ── Slot handlers ────────────────────────────────────────────────

    fn handle_slot_set(&mut self, msg: &Message) -> Result<Vec<Outbound>, DispatchError> {
        let win_id = self.check_routable(msg)?;
        let p: SlotSetPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        self.ensure_window_state(win_id)
            .slot_table.insert(p.slot_id, p.hash);
        /* `kind` (TextureDesc / future MeshDesc) is consumed by the
         * renderer's resource-table allocator, not stored here.
         * EnvelopeFrontend just records the hash binding; M2.7 frescod
         * integration will plumb the kind through to HeadlessRenderer's
         * process_uploads at frame boundary. */
        Ok(Vec::new())
    }

    fn handle_slot_clear(&mut self, msg: &Message) -> Result<Vec<Outbound>, DispatchError> {
        let win_id = self.check_routable(msg)?;
        let p: SlotClearPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        if let Some(s) = self.per_window.get_mut(&win_id) {
            s.slot_table.remove(&p.slot_id);
        }
        Ok(Vec::new())
    }

    // ── Frame boundary handlers ──────────────────────────────────────

    fn handle_scene_frame_begin(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let win_id = self.check_routable(msg)?;
        self.ensure_window_state(win_id).in_frame = true;
        Ok(Vec::new())
    }

    fn handle_scene_frame_end(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let win_id = self.check_routable(msg)?;
        if let Some(s) = self.per_window.get_mut(&win_id) {
            s.in_frame = false;
        }
        Ok(Vec::new())
    }

    // ── Scene node handlers ──────────────────────────────────────────

    fn handle_scene_node_set(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let win_id = self.check_routable(msg)?;
        let p: SceneNodeSetPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let s = self.ensure_window_state(win_id);
        match p.op_id {
            scene_ops::ATRIUM_CORE_RECT => {
                let inner: RectParams = decode(&p.params)
                    .map_err(|_| DispatchError::BadPayload)?;
                s.rect_nodes.insert(p.node_id, inner);
                /* Repeat-node-id replaces — but if the previous binding
                 * was a texture op, we need to evict from texture_nodes
                 * to maintain the invariant "node_id appears in exactly
                 * one map." */
                s.texture_nodes.remove(&p.node_id);
            }
            scene_ops::ATRIUM_CORE_TEXTURE => {
                let inner: TextureParams = decode(&p.params)
                    .map_err(|_| DispatchError::BadPayload)?;
                s.texture_nodes.insert(p.node_id, inner);
                s.rect_nodes.remove(&p.node_id);
            }
            other => {
                /* Unknown / unbundled op-id. The closed registry per
                 * §3.4 ensures stable assignment; an unknown op-id
                 * means either a future bundle that frescod doesn't
                 * have loaded, or a malformed client. Log + skip. */
                log::debug!("SCENE_NODE_SET op_id={:#x} unhandled (no bundle loaded)", other);
            }
        }
        Ok(Vec::new())
    }

    fn handle_scene_node_clear(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let win_id = self.check_routable(msg)?;
        let p: SceneNodeClearPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        if let Some(s) = self.per_window.get_mut(&win_id) {
            s.rect_nodes.remove(&p.node_id);
            s.texture_nodes.remove(&p.node_id);
        }
        Ok(Vec::new())
    }

    // ── Window-management handlers ───────────────────────────────────

    fn handle_window_create(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: WindowCreatePayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let mut comp = self.compositor.lock().unwrap();
        let new_id = comp.create(self.current_client as u32, (p.width, p.height));
        /* Apply title + hints. Compositor's title is set via
         * `rebuild_window_title(cas)`; for M2.6 we just store the
         * literal text in Window::title and let the cutover wire the
         * glyph-baking path. */
        if let Some(win) = comp.windows.get_mut(&new_id) {
            win.title = p.title;
            /* Hints are advisory; for now we record but don't act on
             * server_decorations / min_size / max_size / initial_position.
             * M2.6d's compositor-side event hooks will respect them. */
            let _ = p.hints;
            let _ = p.parent_window_id;
        }

        /* Reply with the assigned window_id. Wire encoding mirrors the
         * payload format (postcard). For a server-assigned u32 the
         * payload is just `window_id`. */
        let reply_payload = fresco_protocol::encode(&(new_id as u32))
            .map_err(|_| DispatchError::BadPayload)?;
        Ok(vec![Outbound {
            op:      control::OP_WINDOW_CREATE,
            flags:   aqueduct::envelope::flag::IS_RESPONSE,
            payload: reply_payload,
        }])
    }

    fn handle_window_destroy(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: WindowDestroyPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let win_id = p.window_id as u16;
        let mut comp = self.compositor.lock().unwrap();
        let ok = comp.destroy(win_id, self.current_client as u32);
        if ok {
            drop(comp);
            self.forget_window(p.window_id);
        }
        /* M2.6d adds a focus-shift event here when the destroyed window
         * was focused — the legacy frontend's destroy_with_focus_shift
         * path computes it; this handler will adopt that mechanism. */
        Ok(Vec::new())
    }

    fn handle_window_set_title(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: WindowSetTitlePayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let win_id = p.window_id as u16;
        let mut comp = self.compositor.lock().unwrap();
        let win = comp.windows.get_mut(&win_id)
            .ok_or(DispatchError::UnknownWindow)?;
        if win.owner != self.current_client as u32 {
            return Err(DispatchError::NotOwner);
        }
        win.title = p.title;
        /* M2.7 cutover wires `comp.rebuild_window_title(cas)` here so
         * the title-glyph cache regenerates. For now title is just
         * stored. */
        Ok(Vec::new())
    }

    fn handle_window_set_hints(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: WindowSetHintsPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let win_id = p.window_id as u16;
        let comp = self.compositor.lock().unwrap();
        let win = comp.windows.get(&win_id)
            .ok_or(DispatchError::UnknownWindow)?;
        if win.owner != self.current_client as u32 {
            return Err(DispatchError::NotOwner);
        }
        /* Hints currently advisory; M2.6d's compositor hooks will act
         * on them (min_size enforcement on resize, modal z-shifting). */
        let _ = p.hints;
        Ok(Vec::new())
    }

    fn handle_window_request_close(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: WindowRequestClosePayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        /* Toolkit-initiated close = same effect as DESTROY. (User-
         * initiated close goes the other direction: server emits
         * EV_WINDOW_CLOSE_REQUESTED, toolkit decides whether to send
         * DESTROY or REQUEST_CLOSE in response. M2.6d wires that.) */
        let mut comp = self.compositor.lock().unwrap();
        let ok = comp.destroy(p.window_id as u16, self.current_client as u32);
        if ok {
            drop(comp);
            self.forget_window(p.window_id);
        }
        Ok(Vec::new())
    }

    fn handle_window_present(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: WindowPresentPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        /* Per-window frame commit. Single-window apps can stick with
         * SCENE_FRAME_END; multi-window apps use this to commit one
         * window's frame at a time. M2.7 wires the present trigger
         * into frescod's render loop. */
        let _ = p.window_id;
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresco_protocol::*;

    fn fixture() -> EnvelopeFrontend {
        let cas = Arc::new(Mutex::new(CasStore::new()));
        let scene = Arc::new(Mutex::new(crate::scene::graph::SceneGraph::new()));
        let slots = Arc::new(Mutex::new(crate::scene::slots::SlotTable::new()));
        let comp = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
        EnvelopeFrontend::new(cas, comp)
    }

    fn msg(op: u16, flags: u16, payload: Vec<u8>) -> Message {
        Message {
            opcode_class: CLASS_DISPLAY,
            op,
            flags,
            kind: MessageKind::Request,
            payload,
        }
    }

    #[test]
    fn window0_routable_no_owner_check() {
        let mut f = fixture();
        let payload = encode(&SceneFrameBeginPayload::default()).unwrap();
        let m = msg(control::OP_SCENE_FRAME_BEGIN, /*window_id=*/ 0, payload);
        let out = f.dispatch(&m, /*client_id=*/ 1).expect("dispatch");
        assert!(out.is_empty());
        assert!(f.window_state(0).unwrap().in_frame);
    }

    #[test]
    fn scene_node_set_records_rect() {
        let mut f = fixture();

        /* Begin frame on window 0. */
        let payload = encode(&SceneFrameBeginPayload::default()).unwrap();
        f.dispatch(&msg(control::OP_SCENE_FRAME_BEGIN, 0, payload), 1)
            .expect("frame begin");

        /* Set node 7 = a rect. */
        let inner = RectParams {
            x: 100.0, y: 200.0, w: 64.0, h: 48.0,
            r: 0.5, g: 0.25, b: 1.0, a: 1.0,
        };
        let inner_bytes = encode(&inner).unwrap();
        let outer = SceneNodeSetPayload {
            node_id: 7,
            op_id:   scene_ops::ATRIUM_CORE_RECT,
            params:  inner_bytes,
        };
        let outer_bytes = encode(&outer).unwrap();
        f.dispatch(&msg(control::OP_SCENE_NODE_SET, 0, outer_bytes), 1)
            .expect("node set");

        /* Verify state. */
        let s = f.window_state(0).unwrap();
        assert_eq!(s.rect_nodes.len(), 1);
        let stored = &s.rect_nodes[&7];
        assert_eq!(stored.x, 100.0);
        assert_eq!(stored.b, 1.0);

        /* Extract → renderer-shape SceneNode. */
        let nodes = s.extract_rect_nodes();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].position, [100.0, 200.0]);
        assert_eq!(nodes[0].size,     [64.0, 48.0]);
        assert_eq!(nodes[0].color,    [0.5, 0.25, 1.0, 1.0]);
    }

    #[test]
    fn texture_nodes_grouped_by_slot() {
        let mut f = fixture();

        for (node_id, slot) in [(1, 1), (2, 1), (3, 2), (4, 1)] {
            let inner = TextureParams { x: 0.0, y: 0.0, w: 10.0, h: 10.0, slot_id: slot };
            let outer = SceneNodeSetPayload {
                node_id,
                op_id:   scene_ops::ATRIUM_CORE_TEXTURE,
                params:  encode(&inner).unwrap(),
            };
            f.dispatch(&msg(control::OP_SCENE_NODE_SET, 0, encode(&outer).unwrap()), 1)
                .expect("texture set");
        }

        let s = f.window_state(0).unwrap();
        let batches = s.extract_texture_batches();
        let slot_counts: std::collections::HashMap<u32, usize> =
            batches.iter().map(|b| (b.slot_id, b.nodes.len())).collect();
        assert_eq!(slot_counts.get(&1), Some(&3));
        assert_eq!(slot_counts.get(&2), Some(&1));
    }

    #[test]
    fn scene_node_clear_removes() {
        let mut f = fixture();
        let inner = RectParams { x: 0.0, y: 0.0, w: 1.0, h: 1.0, r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let outer = SceneNodeSetPayload {
            node_id: 5,
            op_id:   scene_ops::ATRIUM_CORE_RECT,
            params:  encode(&inner).unwrap(),
        };
        f.dispatch(&msg(control::OP_SCENE_NODE_SET, 0, encode(&outer).unwrap()), 1)
            .expect("set");

        let clear = SceneNodeClearPayload { node_id: 5 };
        f.dispatch(&msg(control::OP_SCENE_NODE_CLEAR, 0, encode(&clear).unwrap()), 1)
            .expect("clear");

        assert!(f.window_state(0).unwrap().rect_nodes.is_empty());
    }

    #[test]
    fn slot_set_and_clear() {
        let mut f = fixture();
        let p = SlotSetPayload {
            slot_id: 3,
            hash:    [0xAB; 32],
            kind:    SlotKind::Texture(TextureDesc {
                width: 256, height: 256,
                format: TextureFormat::Rgba8UnormSrgb,
            }),
        };
        f.dispatch(&msg(control::OP_SLOT_SET, 0, encode(&p).unwrap()), 1)
            .expect("slot set");
        assert_eq!(f.window_state(0).unwrap().slot_table.get(&3),
                   Some(&[0xAB; 32]));

        let cl = SlotClearPayload { slot_id: 3 };
        f.dispatch(&msg(control::OP_SLOT_CLEAR, 0, encode(&cl).unwrap()), 1)
            .expect("slot clear");
        assert!(f.window_state(0).unwrap().slot_table.is_empty());
    }

    #[test]
    fn window_create_returns_id() {
        let mut f = fixture();
        let p = WindowCreatePayload {
            width: 1280, height: 720, title: "test".into(),
            hints: WindowHints::default(),
            parent_window_id: 0,
        };
        let out = f.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&p).unwrap()), 1)
            .expect("window create");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].op, control::OP_WINDOW_CREATE);
        assert_eq!(out[0].flags & aqueduct::envelope::flag::IS_RESPONSE,
                   aqueduct::envelope::flag::IS_RESPONSE);

        /* Decoded reply is a u32 window_id; should be > 0 (window 0
         * is the screen). */
        let new_id: u32 = decode(&out[0].payload).expect("decode reply");
        assert!(new_id > 0);
    }

    #[test]
    fn unknown_op_returns_unknown_op_error() {
        let mut f = fixture();
        let m = msg(/*op=*/ 0xDEAD, 0, Vec::new());
        let r = f.dispatch(&m, 1);
        match r {
            Err(DispatchError::UnknownOp(0xDEAD)) => {}
            other => panic!("expected UnknownOp(0xDEAD), got {other:?}"),
        }
    }

    #[test]
    fn unowned_window_op_rejected() {
        let mut f = fixture();
        /* Client 1 creates a window. */
        let create = WindowCreatePayload {
            width: 100, height: 100, title: "".into(),
            hints: WindowHints::default(),
            parent_window_id: 0,
        };
        let out = f.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&create).unwrap()), 1)
            .expect("create");
        let new_id: u32 = decode(&out[0].payload).unwrap();

        /* Client 2 tries to set title — must fail. */
        let st = WindowSetTitlePayload { window_id: new_id, title: "hijack".into() };
        let r = f.dispatch(&msg(control::OP_WINDOW_SET_TITLE, 0, encode(&st).unwrap()), 2);
        match r {
            Err(DispatchError::NotOwner) => {}
            other => panic!("expected NotOwner, got {other:?}"),
        }
    }
}
