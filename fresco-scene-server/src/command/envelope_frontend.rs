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
    control, decode, encode, scene_ops,
    SceneNodeSetPayload, SceneNodeClearPayload,
    SlotSetPayload, SlotClearPayload, SlotUpdateRegionPayload, SlotKind, TextureFormat,
    RectParams, TextureParams, PathParams, GlyphRunParams,
    WindowCreatePayload, WindowDestroyPayload,
    WindowSetTitlePayload, WindowSetHintsPayload,
    WindowRequestClosePayload, WindowPresentPayload,
    WindowPresentDamagePayload, DamageRect,
    FontOpenPayload, FontOpenResponse, FontClosePayload, TextRunInstallPayload,
    TextMeasurePayload, TextMeasureResponse,
    WmSurfaceInfo, WmEnumerateReply, WmRole, WmRect,
    WmDeclareLayoutPayload, WmSetRenderingPayload,
};
use aqueduct::envelope::flag;
use fresco_vulkan::UploadRequest;
use crate::text::{TextEngine, SharedTextEngine, PendingAtlasUpload};

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
    pub rect_nodes:      HashMap<u32, RectParams>,
    pub texture_nodes:   HashMap<u32, TextureParams>,
    pub path_nodes:      HashMap<u32, PathParams>,
    pub glyph_run_nodes: HashMap<u32, GlyphRunParams>,
    pub slot_table:      HashMap<u32, [u8; 32]>,
    pub in_frame:        bool,

    /// `node_id → client_id` for every live node in this window,
    /// regardless of which op-kind map holds it. Used by
    /// `forget_client_writes` on disconnect to purge only the
    /// disconnecting client's contributions (relevant for the shared
    /// screen window — id 0 — where multiple clients can write).
    pub node_writers: HashMap<u32, u8>,
    /// `slot_id → client_id` for every live slot binding in this
    /// window. Same purpose as `node_writers`.
    pub slot_writers: HashMap<u32, u8>,

    /// PT: damage rectangle from the most recent
    /// `OP_WINDOW_PRESENT_DAMAGE`, in window-local pixels. `Some` means
    /// the next present should recomposite only this region (the
    /// compositor scissors to it); `None` means a whole-surface
    /// present. Consumed by the renderer/compositor each present.
    pub pending_damage: Option<DamageRect>,
}

impl WindowSceneState {
    pub fn new() -> Self { Self::default() }

    /// Snapshot the rect-op nodes as `fresco_vulkan::SceneNode` ordered
    /// by ascending `node_id`. Clients that allocate node ids in
    /// insertion order (Pergola does) get z-order-stable rendering:
    /// the rect with the smallest id is drawn first (background),
    /// later ids paint on top. Without this ordering, `HashMap.values()`
    /// produces a hash-randomized order and same-color rects get
    /// visually clobbered when a later-drawn rect happens to cover them.
    ///
    /// Returns a fresh `Vec` each call — the renderer takes ownership.
    /// Cost: O(N_rects log N_rects) for the sort; still µs-range for
    /// typical scenes.
    pub fn extract_rect_nodes(&self) -> Vec<fresco_vulkan::SceneNode> {
        let mut entries: Vec<(&u32, &RectParams)> = self.rect_nodes.iter().collect();
        entries.sort_by_key(|(id, _)| **id);
        entries.into_iter().map(|(_, p)| fresco_vulkan::SceneNode {
            position: [p.x, p.y],
            size:     [p.w, p.h],
            color:    [p.r, p.g, p.b, p.a],
            radius:   p.radius,
            _pad:     [0.0; 3],
        }).collect()
    }

    /// Snapshot the path-op nodes (rotated quads), ordered by ascending
    /// `node_id` for z-order stability — see `extract_rect_nodes`.
    pub fn extract_path_nodes(&self) -> Vec<fresco_vulkan::PathNode> {
        let mut entries: Vec<(&u32, &PathParams)> = self.path_nodes.iter().collect();
        entries.sort_by_key(|(id, _)| **id);
        entries.into_iter().map(|(_, p)| fresco_vulkan::PathNode {
            model: [p.cx, p.cy, p.length, p.width],
            extra: [p.angle, 0.0, 0.0, 0.0],
            color: [p.r, p.g, p.b, p.a],
        }).collect()
    }

    /// Group glyph_run nodes by atlas slot into
    /// `fresco_vulkan::GlyphRunBatch`es. Each batch carries its
    /// scene nodes (one per text run sharing this atlas) and a
    /// concatenated glyphs vector with adjusted per-node
    /// `meta[1]` (glyph_offset) so the GPU kernel reads each run's
    /// glyphs from the right slice.
    ///
    /// The runs themselves are not pre-translated by window
    /// position here — the merge step in frescod's render loop
    /// adds the window offset, the same way it does for rect and
    /// path nodes.
    pub fn extract_glyph_run_batches(&self)
        -> Vec<fresco_vulkan::GlyphRunBatch>
    {
        /* Same id-ordering trick as extract_rect_nodes — preserves the
         * client's insertion order so text rendered later overlaps
         * text rendered earlier. Within a single batch (one atlas
         * slot), glyph runs are appended in id order. */
        let mut entries: Vec<(&u32, &GlyphRunParams)>
            = self.glyph_run_nodes.iter().collect();
        entries.sort_by_key(|(id, _)| **id);
        let mut by_slot:
            HashMap<u32, fresco_vulkan::GlyphRunBatch> = HashMap::new();
        for (_, run) in entries {
            let entry = by_slot.entry(run.atlas_slot_id)
                .or_insert_with(|| fresco_vulkan::GlyphRunBatch {
                    atlas_slot_id: run.atlas_slot_id,
                    nodes:  Vec::new(),
                    glyphs: Vec::new(),
                });
            let glyph_offset = entry.glyphs.len() as i32;
            entry.nodes.push(fresco_vulkan::GlyphRunNode {
                origin:    [run.x, run.y, 0.0, 0.0],
                atlas_dim: [run.atlas_width  as f32,
                            run.atlas_height as f32,
                            0.0, 0.0],
                color:     [run.r, run.g, run.b, run.a],
                meta:      [run.glyphs.len() as i32, glyph_offset, 0, 0],
            });
            for g in &run.glyphs {
                entry.glyphs.push(fresco_vulkan::GlyphInstance {
                    d_offset: [g.dx, g.dy],
                    atlas_uv: [g.atlas_u as f32, g.atlas_v as f32,
                               g.atlas_w as f32, g.atlas_h as f32],
                    bearing:  [g.bearing_x, g.bearing_y],
                    ..Default::default()
                });
            }
        }
        by_slot.into_values().collect()
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
    /// The dispatching client lacks the capability the op requires
    /// (e.g. a cross-app window-management op from a client that does
    /// not hold the `window-management` capability). Default-deny.
    Forbidden,
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

    /// Slot SETs accumulated since the last `take_pending_uploads()`.
    /// Each entry is `(slot_id, hash, kind)`; renderer drains by hash
    /// → CAS bytes → format-aware `UploadRequest`.
    pending_slot_sets: Vec<(u32, [u8; 32], SlotKind)>,
    /// Slot CLEARs accumulated since the last drain.
    pending_slot_clears: Vec<u32>,
    /// Atlas pages produced by server-side text shaping that need to
    /// be re-uploaded before the next render.
    pending_atlas_uploads: Vec<PendingAtlasUpload>,
    /// PT: in-app sub-rectangle texture updates from
    /// `OP_SLOT_UPDATE_REGION`. Each is `(slot_id, dst_x, dst_y,
    /// width, height, bytes)` — bytes are carried inline on the wire
    /// (no CAS round-trip), drained straight into an
    /// `UploadRequest::TextureRegion`.
    pending_region_uploads: Vec<(u32, u32, u32, u32, u32, Vec<u8>)>,

    /// Server-side text engine: font registry + lazy atlases. Shared
    /// across all clients so DejaVuSansMono-16 atlas is built once and
    /// reused regardless of which client first asked.
    text: SharedTextEngine,

    /// Clients granted the `window-management` capability — the
    /// privileged shell (Forum), and only it. The connection-accept
    /// layer verifies the peer's manifest (getpeereid → registry →
    /// caps, the Choragus/`audio_monitor` pattern) and calls
    /// `grant_window_management` for a qualifying client. The cross-app
    /// `OP_WM_*` ops are default-deny: a client not in this set gets
    /// `Forbidden`, so an ordinary app can never enumerate or place
    /// another app's surfaces.
    wm_capable: std::collections::HashSet<u8>,
}

impl EnvelopeFrontend {
    pub fn new(cas: Arc<Mutex<CasStore>>,
               compositor: Arc<Mutex<Compositor>>) -> Self {
        Self::new_with_text(cas, compositor,
            Arc::new(std::sync::RwLock::new(TextEngine::new())))
    }

    pub fn new_with_text(
        cas: Arc<Mutex<CasStore>>,
        compositor: Arc<Mutex<Compositor>>,
        text: SharedTextEngine,
    ) -> Self {
        Self {
            cas, compositor, text,
            per_window: HashMap::new(),
            current_client: 0,
            pending_slot_sets: Vec::new(),
            pending_slot_clears: Vec::new(),
            pending_atlas_uploads: Vec::new(),
            pending_region_uploads: Vec::new(),
            wm_capable: std::collections::HashSet::new(),
        }
    }

    /// Grant a client the `window-management` capability. Called by the
    /// connection-accept layer after it verifies the peer's manifest
    /// holds `window-management` (getpeereid → registry → caps). Until
    /// that plumbing lands, frescod's launcher can grant the known
    /// shell client here. Default state is ungranted (deny).
    pub fn grant_window_management(&mut self, client_id: u8) {
        self.wm_capable.insert(client_id);
    }

    /// Drop a client's grants (on disconnect). Cheap no-op if absent.
    pub fn revoke_client_caps(&mut self, client_id: u8) {
        self.wm_capable.remove(&client_id);
    }

    /// Stash a blob into the scene-server CasStore. The aqueduct
    /// substrate auto-handles UPLOAD_BEGIN/DATA/FINISH into its
    /// per-connection cache; the connection thread is responsible
    /// for pulling those bytes out of `conn.cache_get(&hash)` and
    /// handing them here so the upload pump can resolve hashes
    /// referenced by SLOT_SET into renderer uploads.
    pub fn ingest_blob(&self, bytes: &[u8]) {
        self.cas.lock().unwrap().store(bytes);
    }

    /// Drain accumulated slot-bind / slot-clear ops into renderer-side
    /// upload + free requests. Looks each `slot_set` hash up in CAS;
    /// missing-blob entries are dropped with a warning. Caller passes
    /// the returned vectors to `HeadlessRenderer::process_uploads`
    /// before the next render.
    pub fn take_pending_uploads(&mut self) -> (Vec<UploadRequest>, Vec<u32>) {
        let sets = std::mem::take(&mut self.pending_slot_sets);
        let clears = std::mem::take(&mut self.pending_slot_clears);
        let atlas_uploads = std::mem::take(&mut self.pending_atlas_uploads);
        let cas = self.cas.lock().unwrap();
        let mut uploads = Vec::with_capacity(sets.len() + atlas_uploads.len());
        for (slot_id, hash, kind) in sets {
            let Some(bytes) = cas.load(&hash) else {
                log::warn!("slot_set slot={slot_id}: CAS miss for hash; \
                            drop pending upload");
                continue;
            };
            match kind {
                SlotKind::Texture(desc) => {
                    let format = match desc.format {
                        TextureFormat::Rgba8UnormSrgb =>
                            ash::vk::Format::R8G8B8A8_SRGB,
                        TextureFormat::R8Unorm =>
                            ash::vk::Format::R8_UNORM,
                    };
                    uploads.push(UploadRequest::Texture {
                        slot_id,
                        bytes:  bytes.to_vec(),
                        width:  desc.width,
                        height: desc.height,
                        format,
                    });
                }
            }
        }
        /* Server-managed text atlases. Bytes already in hand from
         * the text engine — bypass CAS, go straight to UploadRequest.
         * First upload of each (font, size) page goes via Full
         * (allocates the GPU image); subsequent grow events ship
         * just the dirty rectangle via TextureRegion. */
        for a in atlas_uploads {
            match a {
                PendingAtlasUpload::Full {
                    slot_id, width, height, pixels,
                } => uploads.push(UploadRequest::Texture {
                    slot_id, bytes: pixels, width, height,
                    format: ash::vk::Format::R8_UNORM,
                }),
                PendingAtlasUpload::Region {
                    slot_id, dst_x, dst_y, width, height, pixels,
                } => uploads.push(UploadRequest::TextureRegion {
                    slot_id, bytes: pixels,
                    dst_x, dst_y, width, height,
                }),
            }
        }
        /* PT: app-driven in-place sub-rectangle updates
         * (OP_SLOT_UPDATE_REGION). Bytes already inline — straight to
         * the same device-side region write the text atlas uses. */
        for (slot_id, dst_x, dst_y, width, height, bytes)
            in std::mem::take(&mut self.pending_region_uploads)
        {
            uploads.push(UploadRequest::TextureRegion {
                slot_id, bytes, dst_x, dst_y, width, height,
            });
        }
        (uploads, clears)
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
            control::OP_SLOT_UPDATE_REGION => self.handle_slot_update_region(msg),
            control::OP_SCENE_FRAME_BEGIN  => self.handle_scene_frame_begin(msg),
            control::OP_SCENE_FRAME_END    => self.handle_scene_frame_end(msg),
            control::OP_SCENE_NODE_SET     => self.handle_scene_node_set(msg),
            control::OP_SCENE_NODE_CLEAR   => self.handle_scene_node_clear(msg),

            // Server-side text (M6.3 + M6.5)
            control::OP_FONT_OPEN          => self.handle_font_open(msg),
            control::OP_FONT_CLOSE         => self.handle_font_close(msg),
            control::OP_TEXT_RUN_INSTALL   => self.handle_text_run_install(msg),
            control::OP_TEXT_MEASURE       => self.handle_text_measure(msg),

            // ── Window management ops (window_id is in payload) ──
            control::OP_WINDOW_CREATE        => self.handle_window_create(msg),
            control::OP_WINDOW_DESTROY       => self.handle_window_destroy(msg),
            control::OP_WINDOW_SET_TITLE     => self.handle_window_set_title(msg),
            control::OP_WINDOW_SET_HINTS     => self.handle_window_set_hints(msg),
            control::OP_WINDOW_REQUEST_CLOSE => self.handle_window_request_close(msg),
            control::OP_WINDOW_PRESENT       => self.handle_window_present(msg),
            control::OP_WINDOW_PRESENT_DAMAGE => self.handle_window_present_damage(msg),

            // ── Cross-app window management (gated by window-management) ──
            control::OP_WM_ENUMERATE         => self.handle_wm_enumerate(msg),
            control::OP_WM_DECLARE_LAYOUT    => self.handle_wm_declare_layout(msg),
            control::OP_WM_SET_RENDERING     => self.handle_wm_set_rendering(msg),

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

    /// Walk every per-window scene state and purge nodes + slot
    /// bindings written by `client_id`. Used for the shared screen
    /// window (id 0) where multiple clients can write — the per-
    /// window state survives the disconnecting client and would
    /// otherwise leak that client's last frame indefinitely.
    /// Slot purges are queued through `pending_slot_clears` so the
    /// renderer's next `process_uploads` cycle frees the GPU side.
    pub fn forget_client_writes(&mut self, client_id: u8) {
        for s in self.per_window.values_mut() {
            let mut dead_nodes: Vec<u32> = Vec::new();
            for (&id, &writer) in &s.node_writers {
                if writer == client_id { dead_nodes.push(id); }
            }
            for id in dead_nodes {
                s.rect_nodes.remove(&id);
                s.texture_nodes.remove(&id);
                s.path_nodes.remove(&id);
                s.glyph_run_nodes.remove(&id);
                s.node_writers.remove(&id);
            }

            let mut dead_slots: Vec<u32> = Vec::new();
            for (&id, &writer) in &s.slot_writers {
                if writer == client_id { dead_slots.push(id); }
            }
            for id in dead_slots {
                s.slot_table.remove(&id);
                s.slot_writers.remove(&id);
                self.pending_slot_clears.push(id);
            }
        }
    }

    /// Borrow the compositor `Arc` so callers can manipulate windows
    /// outside the dispatch flow (client-disconnect cleanup, frescod's
    /// per-frame render walking the window list, etc).
    pub fn compositor_arc(&self) -> &Arc<Mutex<Compositor>> {
        &self.compositor
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
        let writer = self.current_client;
        let s = self.ensure_window_state(win_id);
        s.slot_table.insert(p.slot_id, p.hash);
        s.slot_writers.insert(p.slot_id, writer);
        self.pending_slot_sets.push((p.slot_id, p.hash, p.kind));
        Ok(Vec::new())
    }

    fn handle_slot_clear(&mut self, msg: &Message) -> Result<Vec<Outbound>, DispatchError> {
        let win_id = self.check_routable(msg)?;
        let p: SlotClearPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        if let Some(s) = self.per_window.get_mut(&win_id) {
            s.slot_table.remove(&p.slot_id);
            s.slot_writers.remove(&p.slot_id);
        }
        self.pending_slot_clears.push(p.slot_id);
        Ok(Vec::new())
    }

    /// PT: `OP_SLOT_UPDATE_REGION` — overwrite a sub-rectangle of an
    /// already-bound texture slot in place. The slot's binding
    /// (dimensions/format) is unchanged — only `pixels[region]` are
    /// replaced — so this neither touches `slot_table` nor re-uploads
    /// the whole blob; it queues a device-side region write.
    fn handle_slot_update_region(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let win_id = self.check_routable(msg)?;
        let p: SlotUpdateRegionPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        // Reject a region update to a slot this window never bound:
        // there's no texture to patch, and silently allocating one
        // would mask a client bug.
        let bound = self.per_window.get(&win_id)
            .map(|s| s.slot_table.contains_key(&p.slot_id))
            .unwrap_or(false);
        if !bound {
            log::warn!("slot_update_region win={win_id} slot={}: slot not \
                        bound (no prior OP_SLOT_SET); dropping", p.slot_id);
            return Ok(Vec::new());
        }
        self.pending_region_uploads.push((
            p.slot_id, p.dst_x, p.dst_y, p.width, p.height, p.bytes,
        ));
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
        let writer = self.current_client;
        let s = self.ensure_window_state(win_id);
        s.node_writers.insert(p.node_id, writer);
        match p.op_id {
            scene_ops::ATRIUM_CORE_RECT => {
                let inner: RectParams = decode(&p.params)
                    .map_err(|_| DispatchError::BadPayload)?;
                s.rect_nodes.insert(p.node_id, inner);
                /* Repeat-node-id replaces — evict from sibling op-kind
                 * maps to maintain the invariant "node_id appears in
                 * exactly one map." */
                s.texture_nodes.remove(&p.node_id);
                s.path_nodes.remove(&p.node_id);
                s.glyph_run_nodes.remove(&p.node_id);
            }
            scene_ops::ATRIUM_CORE_TEXTURE => {
                let inner: TextureParams = decode(&p.params)
                    .map_err(|_| DispatchError::BadPayload)?;
                s.texture_nodes.insert(p.node_id, inner);
                s.rect_nodes.remove(&p.node_id);
                s.path_nodes.remove(&p.node_id);
                s.glyph_run_nodes.remove(&p.node_id);
            }
            scene_ops::ATRIUM_CORE_PATH => {
                let inner: PathParams = decode(&p.params)
                    .map_err(|_| DispatchError::BadPayload)?;
                s.path_nodes.insert(p.node_id, inner);
                s.rect_nodes.remove(&p.node_id);
                s.texture_nodes.remove(&p.node_id);
                s.glyph_run_nodes.remove(&p.node_id);
            }
            scene_ops::ATRIUM_TEXT_GLYPH_RUN => {
                let inner: GlyphRunParams = decode(&p.params)
                    .map_err(|_| DispatchError::BadPayload)?;
                s.glyph_run_nodes.insert(p.node_id, inner);
                s.rect_nodes.remove(&p.node_id);
                s.texture_nodes.remove(&p.node_id);
                s.path_nodes.remove(&p.node_id);
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
            s.path_nodes.remove(&p.node_id);
            s.glyph_run_nodes.remove(&p.node_id);
            s.node_writers.remove(&p.node_id);
        }
        Ok(Vec::new())
    }

    // ── Server-side text handlers (M6.3) ─────────────────────────────

    fn handle_font_open(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: FontOpenPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let mut text = self.text.write().unwrap();
        let resp = match text.open(&p.name) {
            Some((font_id, m)) => FontOpenResponse {
                font_id,
                units_per_em: m.units_per_em,
                ascent_units: m.ascent_units,
                descent_units: m.descent_units,
                mono_advance_units: m.mono_advance_units,
            },
            None => {
                log::warn!("FONT_OPEN '{}' failed — not found in search path",
                           p.name);
                FontOpenResponse {
                    font_id: 0,
                    units_per_em: 0, ascent_units: 0, descent_units: 0,
                    mono_advance_units: 0,
                }
            }
        };
        let payload = encode(&resp).map_err(|_| DispatchError::BadPayload)?;
        Ok(vec![Outbound {
            op:      control::OP_FONT_OPEN,
            flags:   flag::IS_RESPONSE,
            payload,
        }])
    }

    fn handle_font_close(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: FontClosePayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        self.text.write().unwrap().close(p.font_id);
        Ok(Vec::new())
    }

    fn handle_text_measure(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: TextMeasurePayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let resp = match self.text.write().unwrap().measure(p.font_id, p.size_px, &p.text) {
            Some((w, a, d)) => TextMeasureResponse {
                width_px: w, ascent_px: a, descent_px: d,
            },
            None => TextMeasureResponse {
                width_px: 0.0, ascent_px: 0.0, descent_px: 0.0,
            },
        };
        let payload = encode(&resp).map_err(|_| DispatchError::BadPayload)?;
        Ok(vec![Outbound {
            op:      control::OP_TEXT_MEASURE,
            flags:   flag::IS_RESPONSE,
            payload,
        }])
    }

    fn handle_text_run_install(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let win_id = self.check_routable(msg)?;
        let p: TextRunInstallPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;

        let (run, pending) = {
            let mut text = self.text.write().unwrap();
            text.shape_text_run(
                p.font_id, p.size_px, p.x, p.y,
                [p.r, p.g, p.b, p.a], &p.text, p.weight,
            ).ok_or(DispatchError::BadPayload)?
        };
        if let Some(up) = pending {
            self.pending_atlas_uploads.push(up);
        }
        let writer = self.current_client;
        let s = self.ensure_window_state(win_id);
        s.glyph_run_nodes.insert(p.node_id, run);
        s.rect_nodes.remove(&p.node_id);
        s.texture_nodes.remove(&p.node_id);
        s.path_nodes.remove(&p.node_id);
        s.node_writers.insert(p.node_id, writer);
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
            /* `initial_position` is the only hint the server currently
             * acts on — it lets multi-window apps spread out instead
             * of stacking at the WM's default origin. min_size /
             * max_size / server_decorations / modal are still advisory
             * (no enforcement / chrome path on the M3 stack). */
            if let Some((x, y)) = p.hints.initial_position {
                win.pos = (x as f32, y as f32);
            }
            /* Record the declared role so WM_ENUMERATE reports it and the
             * WM can place/layer by role (forum.md §2.2). `hud` is a
             * Forum-reserved layer no app may claim (overlay-safety): clamp
             * it to Document. Capability-gating the other privileged roles
             * (e.g. chrome only for shell apps) is future work — today the
             * hint is trusted for everything except hud. */
            win.role = match p.hints.role {
                Some(fresco_protocol::WmRole::Hud) | None => fresco_protocol::WmRole::Document,
                Some(r) => r,
            };
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

    // ── Cross-app window management (the privileged shell side) ──────────

    /// `OP_WM_ENUMERATE` — report every surface in this session to the WM.
    ///
    /// Default-deny: only a client holding `window-management` (in
    /// `wm_capable`) may see across app boundaries; everyone else gets
    /// `Forbidden`. frescod is per-session, so "every window in the
    /// compositor" *is* exactly this seat's surfaces — cross-user
    /// isolation is structural (bob's windows live in bob's frescod).
    ///
    /// `role` is the role the client declared at create time
    /// (`WindowHints.role`, defaulting to `Document`); `owner_app` is
    /// still synthesised from the owning client id (the real app-id
    /// arrives with the peer-cred → registry binding). The shell gets the
    /// geometry + role it needs to place + layer surfaces.
    fn handle_wm_enumerate(&mut self, _msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        if !self.wm_capable.contains(&self.current_client) {
            return Err(DispatchError::Forbidden);
        }
        let comp = self.compositor.lock().unwrap();
        let mut surfaces: Vec<WmSurfaceInfo> = comp.windows.values()
            .filter(|w| w.id != 0) // window 0 is the implicit screen, not an app surface
            .map(|w| WmSurfaceInfo {
                surface_id: w.id as u32,
                owner_app:  format!("client:{}", w.owner),
                role:       w.role,
                rect: WmRect {
                    x: w.pos.0 as i32,
                    y: w.pos.1 as i32,
                    w: w.size.0 as i32,
                    h: w.size.1 as i32,
                },
            })
            .collect();
        // Stable order (by surface id) so the WM sees a deterministic set.
        surfaces.sort_by_key(|s| s.surface_id);

        let reply_payload = encode(&WmEnumerateReply { surfaces })
            .map_err(|_| DispatchError::BadPayload)?;
        Ok(vec![Outbound {
            op:      control::OP_WM_ENUMERATE,
            flags:   aqueduct::envelope::flag::IS_RESPONSE,
            payload: reply_payload,
        }])
    }

    /// `OP_WM_DECLARE_LAYOUT` — apply the WM's atomic placement of every
    /// surface: per-surface rect (pos + size), the z-order implied by the
    /// declared layers, and which surface holds focus. This is the state
    /// the WM owns; the renderer reads pos/size/z_order to compose.
    ///
    /// Layer convention follows forum-wm: a *lower* layer is closer to the
    /// viewer (topmost). frescod's `z_order` is bottom→top (last = top), so
    /// we lay the declared surfaces highest-layer-first. The screen window
    /// (id 0) stays pinned at the bottom; any surface created in the race
    /// between enumerate and declare (not named in the layout) is kept on
    /// top so a brand-new window is never silently hidden.
    fn handle_wm_declare_layout(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        if !self.wm_capable.contains(&self.current_client) {
            return Err(DispatchError::Forbidden);
        }
        let layout: WmDeclareLayoutPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let mut comp = self.compositor.lock().unwrap();

        // 1. geometry — apply each declared rect to the surface it names.
        for slot in &layout.slots {
            if let Some(w) = comp.windows.get_mut(&(slot.surface_id as u16)) {
                w.pos  = (slot.rect.x as f32, slot.rect.y as f32);
                w.size = (slot.rect.w as f32, slot.rect.h as f32);
            }
        }

        // 2. z-order — screen at the bottom, then declared surfaces
        //    highest-layer-first (so the lowest layer lands on top), then
        //    any unnamed surviving surface on top of all.
        let mut named: Vec<&fresco_protocol::WmSlot> = layout.slots.iter()
            .filter(|s| comp.windows.contains_key(&(s.surface_id as u16)))
            .collect();
        named.sort_by(|a, b| b.layer.cmp(&a.layer)); // highest layer = bottom
        let named_ids: std::collections::HashSet<u16> =
            named.iter().map(|s| s.surface_id as u16).collect();

        let mut new_z: Vec<u16> = Vec::with_capacity(comp.windows.len());
        if comp.windows.contains_key(&0) { new_z.push(0); }
        for s in &named { new_z.push(s.surface_id as u16); }
        // unnamed survivors (excluding screen + already-placed) on top.
        let mut unnamed: Vec<u16> = comp.windows.keys().copied()
            .filter(|&id| id != 0 && !named_ids.contains(&id))
            .collect();
        unnamed.sort();
        new_z.extend(unnamed);

        // write the new order + keep each Window.z consistent with it.
        for (i, &id) in new_z.iter().enumerate() {
            if let Some(w) = comp.windows.get_mut(&id) { w.z = i as u32; }
        }
        comp.z_order = new_z;

        // 3. focus — set the declared focus (0 = leave unfocused). z is set
        //    above from roles, so focus here must not raise.
        let new_focus = (layout.focus != 0
            && comp.windows.contains_key(&(layout.focus as u16)))
            .then_some(layout.focus as u16);
        comp.set_focus_no_raise(new_focus);

        Ok(Vec::new()) // atomic apply; no reply
    }

    /// `OP_WM_SET_RENDERING` — record that a surface is (not) rendering. The
    /// WM sets this false for a fully-occluded surface so its GPU work can
    /// stop and the idle blocks power-gate. For now it records the decision
    /// on the surface; the renderer honoring it (skip-compose) and the
    /// power-gate signal are the follow-up.
    fn handle_wm_set_rendering(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        if !self.wm_capable.contains(&self.current_client) {
            return Err(DispatchError::Forbidden);
        }
        let p: WmSetRenderingPayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let mut comp = self.compositor.lock().unwrap();
        if let Some(w) = comp.windows.get_mut(&(p.surface_id as u16)) {
            w.rendering = p.rendering;
            Ok(Vec::new())
        } else {
            Err(DispatchError::UnknownWindow)
        }
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

    /// PT: `OP_WINDOW_PRESENT_DAMAGE` — per-window present carrying a
    /// dirty rectangle. Records the damage on the window's state so the
    /// renderer/compositor can scissor its recomposite to that region
    /// (frescod's render loop consumes `pending_damage`). An empty rect
    /// is treated as no-damage (cleared to `None`). Otherwise identical
    /// to `OP_WINDOW_PRESENT`.
    fn handle_window_present_damage(&mut self, msg: &Message)
        -> Result<Vec<Outbound>, DispatchError>
    {
        let p: WindowPresentDamagePayload = decode(&msg.payload)
            .map_err(|_| DispatchError::BadPayload)?;
        let damage = if p.damage.w == 0 || p.damage.h == 0 {
            None
        } else {
            Some(p.damage)
        };
        let s = self.ensure_window_state(p.window_id);
        s.pending_damage = damage;
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
            r: 0.5, g: 0.25, b: 1.0, a: 1.0, radius: 0.0,
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
        let inner = RectParams { x: 0.0, y: 0.0, w: 1.0, h: 1.0, r: 0.0, g: 0.0, b: 0.0, a: 1.0, radius: 0.0 };
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
    fn slot_update_region_queues_texture_region_upload() {
        let mut f = fixture();
        // Bind the slot first (region update requires an existing slot).
        let set = SlotSetPayload {
            slot_id: 5, hash: [0xCD; 32],
            kind: SlotKind::Texture(TextureDesc {
                width: 64, height: 64, format: TextureFormat::Rgba8UnormSrgb,
            }),
        };
        f.dispatch(&msg(control::OP_SLOT_SET, 0, encode(&set).unwrap()), 1)
            .expect("slot set");

        let upd = SlotUpdateRegionPayload {
            slot_id: 5, dst_x: 8, dst_y: 4, width: 2, height: 3,
            bytes: vec![0x11; 2 * 3 * 4],
        };
        f.dispatch(&msg(control::OP_SLOT_UPDATE_REGION, 0, encode(&upd).unwrap()), 1)
            .expect("slot update region");

        let (uploads, _clears) = f.take_pending_uploads();
        let region = uploads.iter().find_map(|u| match u {
            UploadRequest::TextureRegion { slot_id, dst_x, dst_y, width, height, bytes }
                if *slot_id == 5 => Some((*dst_x, *dst_y, *width, *height, bytes.len())),
            _ => None,
        });
        assert_eq!(region, Some((8, 4, 2, 3, 24)));
    }

    #[test]
    fn slot_update_region_unbound_slot_dropped() {
        let mut f = fixture();
        // No prior OP_SLOT_SET for slot 9.
        let upd = SlotUpdateRegionPayload {
            slot_id: 9, dst_x: 0, dst_y: 0, width: 1, height: 1,
            bytes: vec![0u8; 4],
        };
        f.dispatch(&msg(control::OP_SLOT_UPDATE_REGION, 0, encode(&upd).unwrap()), 1)
            .expect("dispatch (dropped, not errored)");
        let (uploads, _) = f.take_pending_uploads();
        assert!(!uploads.iter().any(|u|
            matches!(u, UploadRequest::TextureRegion { slot_id: 9, .. })));
    }

    #[test]
    fn window_present_damage_records_rect() {
        let mut f = fixture();
        let p = WindowPresentDamagePayload {
            window_id: 0,
            damage: DamageRect { x: 12, y: 24, w: 40, h: 30 },
        };
        f.dispatch(&msg(control::OP_WINDOW_PRESENT_DAMAGE, 0, encode(&p).unwrap()), 1)
            .expect("present damage");
        assert_eq!(f.window_state(0).unwrap().pending_damage,
                   Some(DamageRect { x: 12, y: 24, w: 40, h: 30 }));

        // An empty rect clears damage to None (whole-surface present).
        let empty = WindowPresentDamagePayload {
            window_id: 0, damage: DamageRect { x: 0, y: 0, w: 0, h: 0 },
        };
        f.dispatch(&msg(control::OP_WINDOW_PRESENT_DAMAGE, 0, encode(&empty).unwrap()), 1)
            .expect("present no-damage");
        assert_eq!(f.window_state(0).unwrap().pending_damage, None);
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
    fn wm_enumerate_is_default_deny() {
        let mut f = fixture();
        // An ordinary app (client 1) creates a surface...
        let p = WindowCreatePayload {
            width: 800, height: 600, title: "app".into(),
            hints: WindowHints::default(), parent_window_id: 0,
        };
        f.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&p).unwrap()), 1)
            .expect("create");
        // ...and is refused the cross-app enumerate (no window-management cap).
        let r = f.dispatch(&msg(control::OP_WM_ENUMERATE, 0, Vec::new()), 1);
        assert!(matches!(r, Err(DispatchError::Forbidden)),
            "an app without the cap cannot see across app boundaries");
    }

    #[test]
    fn wm_enumerate_reports_session_surfaces_for_the_shell() {
        let mut f = fixture();
        let p = WindowCreatePayload {
            width: 800, height: 600, title: "app".into(),
            hints: WindowHints::default(), parent_window_id: 0,
        };
        let out = f.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&p).unwrap()), 1)
            .expect("create");
        let app_win: u32 = decode(&out[0].payload).unwrap();

        // The shell (client 9) holds window-management → it enumerates.
        f.grant_window_management(9);
        let out = f.dispatch(&msg(control::OP_WM_ENUMERATE, 0, Vec::new()), 9)
            .expect("enumerate");
        let reply: WmEnumerateReply = decode(&out[0].payload).unwrap();
        assert!(reply.surfaces.iter().any(|s| s.surface_id == app_win),
            "the shell sees the app's surface");
        assert!(!reply.surfaces.iter().any(|s| s.surface_id == 0),
            "window 0 (the screen) is not an app surface");
    }

    fn make_window(f: &mut EnvelopeFrontend, client: u8, w: u32, h: u32) -> u32 {
        let p = WindowCreatePayload {
            width: w, height: h, title: "app".into(),
            hints: WindowHints::default(), parent_window_id: 0,
        };
        let out = f.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&p).unwrap()), client)
            .expect("create");
        decode(&out[0].payload).unwrap()
    }

    #[test]
    fn wm_declare_layout_applies_geometry_z_and_focus() {
        use fresco_protocol::{WmSlot, WmRect};
        let mut f = fixture();
        let a = make_window(&mut f, 1, 100, 100); // a document
        let b = make_window(&mut f, 2, 100, 100); // a dialog (lower layer = on top)

        f.grant_window_management(9);
        let layout = WmDeclareLayoutPayload {
            slots: vec![
                WmSlot { surface_id: a, rect: WmRect { x: 0, y: 24, w: 1920, h: 1000 }, layer: 4 },
                WmSlot { surface_id: b, rect: WmRect { x: 720, y: 380, w: 480, h: 320 }, layer: 0 },
            ],
            focus: b,
        };
        f.dispatch(&msg(control::OP_WM_DECLARE_LAYOUT, 0, encode(&layout).unwrap()), 9)
            .expect("declare layout");

        let comp = f.compositor_arc().lock().unwrap();
        // geometry applied.
        let wa = comp.windows.get(&(a as u16)).unwrap();
        assert_eq!((wa.pos, wa.size), ((0.0, 24.0), (1920.0, 1000.0)));
        // z-order: screen at bottom, then the higher-layer doc, then the dialog on top.
        assert_eq!(comp.z_order.first(), Some(&0u16), "screen pinned at the bottom");
        assert_eq!(comp.z_order.last(), Some(&(b as u16)), "lowest layer (dialog) on top");
        // focus on the dialog, without it being raised by a separate raise().
        assert_eq!(comp.focus, Some(b as u16));
        assert!(comp.windows.get(&(b as u16)).unwrap().focus);
        assert!(!comp.windows.get(&(a as u16)).unwrap().focus);
    }

    #[test]
    fn wm_declare_layout_is_gated() {
        use fresco_protocol::{WmSlot, WmRect};
        let mut f = fixture();
        let a = make_window(&mut f, 1, 100, 100);
        let layout = WmDeclareLayoutPayload {
            slots: vec![WmSlot { surface_id: a, rect: WmRect { x: 0, y: 0, w: 10, h: 10 }, layer: 4 }],
            focus: a,
        };
        // client 1 (not the shell) cannot declare a layout.
        let r = f.dispatch(&msg(control::OP_WM_DECLARE_LAYOUT, 0, encode(&layout).unwrap()), 1);
        assert!(matches!(r, Err(DispatchError::Forbidden)));
    }

    #[test]
    fn wm_set_rendering_records_the_decision() {
        let mut f = fixture();
        let a = make_window(&mut f, 1, 100, 100);
        f.grant_window_management(9);

        // default: rendering.
        assert!(f.compositor_arc().lock().unwrap().windows.get(&(a as u16)).unwrap().rendering);

        let p = WmSetRenderingPayload { surface_id: a, rendering: false };
        f.dispatch(&msg(control::OP_WM_SET_RENDERING, 0, encode(&p).unwrap()), 9)
            .expect("set rendering");
        assert!(!f.compositor_arc().lock().unwrap().windows.get(&(a as u16)).unwrap().rendering,
            "the WM gated the fully-occluded surface");

        // ungranted client is refused.
        let r = f.dispatch(&msg(control::OP_WM_SET_RENDERING, 0, encode(&p).unwrap()), 1);
        assert!(matches!(r, Err(DispatchError::Forbidden)));
    }

    // ── F0 end-to-end: the REAL forum_wm daemon loop vs the REAL handlers ──────
    //
    // The forum-wm binary talks to frescod over a socket via fresco-client; here we
    // splice the same `forum_wm::daemon::Wm::reconcile` loop directly onto the
    // frontend's dispatch (the shell client), so the policy crate and the server
    // handlers are exercised together without a VM or socket. Only the wire
    // transport + the uid gate are left for the in-VM run.

    use forum_wm::daemon::{FrescoConn, Wm};
    use forum_wm::Screen;

    struct FrontendConn<'a> {
        fe: &'a mut EnvelopeFrontend,
        shell: u8,
    }
    impl FrontendConn<'_> {
        fn dispatch(&mut self, op: u16, payload: Vec<u8>) -> std::io::Result<Vec<Outbound>> {
            self.fe.dispatch(&msg(op, 0, payload), self.shell)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}")))
        }
    }
    impl FrescoConn for FrontendConn<'_> {
        fn enumerate(&mut self) -> std::io::Result<Vec<WmSurfaceInfo>> {
            let out = self.dispatch(control::OP_WM_ENUMERATE, Vec::new())?;
            let reply: WmEnumerateReply = decode(&out[0].payload).unwrap();
            Ok(reply.surfaces)
        }
        fn declare_layout(&mut self, layout: &WmDeclareLayoutPayload) -> std::io::Result<()> {
            self.dispatch(control::OP_WM_DECLARE_LAYOUT, encode(layout).unwrap())?;
            Ok(())
        }
        fn set_rendering(&mut self, decisions: &[WmSetRenderingPayload]) -> std::io::Result<()> {
            for d in decisions {
                self.dispatch(control::OP_WM_SET_RENDERING, encode(d).unwrap())?;
            }
            Ok(())
        }
    }

    #[test]
    fn f0_forum_wm_reconcile_against_frescod() {
        let mut f = fixture();
        // two apps create surfaces; the shell (client 9) holds the cap.
        let a = make_window(&mut f, 1, 400, 300);
        let b = make_window(&mut f, 2, 400, 300);
        f.grant_window_management(9);

        // run the REAL forum-wm daemon loop against the REAL handlers.
        let wm = Wm::new(Screen {
            rect: WmRect { x: 0, y: 0, w: 1920, h: 1080 },
            bar_h: 24, dock_h: 48,
        });
        {
            let mut conn = FrontendConn { fe: &mut f, shell: 9 };
            let layout = wm.reconcile(&mut conn).expect("reconcile");
            assert_eq!(layout.slots.len(), 2, "both surfaces placed");
        }

        // the compositor now reflects the WM's declared layout.
        let comp = f.compositor_arc().lock().unwrap();
        // both app windows were repositioned to fill the work area (below the bar).
        for id in [a, b] {
            let w = comp.windows.get(&(id as u16)).unwrap();
            assert_eq!(w.pos.1, 24.0, "placed below the 24px bar");
            assert_eq!(w.size, (1920.0, 1008.0), "document fills the work area (1080-24-48)");
        }
        // a focus was chosen; the screen (window 0) stays at the bottom of z-order.
        assert!(comp.focus.is_some(), "the WM picked a focus");
        assert_eq!(comp.z_order.first(), Some(&0u16), "screen pinned at the bottom");
        // two identical full-area documents → the lower is fully occluded → gated.
        let gated = [a, b].iter()
            .filter(|&&id| !comp.windows.get(&(id as u16)).unwrap().rendering)
            .count();
        assert_eq!(gated, 1, "one of two stacked documents is occluded → render-gated");
    }

    #[test]
    fn window_create_honors_initial_position() {
        let mut f = fixture();
        let p = WindowCreatePayload {
            width: 480, height: 480, title: "clock".into(),
            hints: WindowHints {
                initial_position: Some((380, 80)),
                ..Default::default()
            },
            parent_window_id: 0,
        };
        let out = f.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&p).unwrap()), 1)
            .expect("window create");
        let new_id: u32 = decode(&out[0].payload).unwrap();

        /* Compositor's window.pos should reflect the hint. */
        let comp = f.compositor_arc().lock().unwrap();
        let win = comp.windows.get(&(new_id as u16)).expect("window exists");
        assert_eq!(win.pos, (380.0, 80.0));
    }

    #[test]
    fn window_create_default_position_is_origin() {
        let mut f = fixture();
        let p = WindowCreatePayload {
            width: 100, height: 100, title: "".into(),
            hints: WindowHints::default(), /* no initial_position */
            parent_window_id: 0,
        };
        let out = f.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&p).unwrap()), 1)
            .expect("create");
        let new_id: u32 = decode(&out[0].payload).unwrap();
        let comp = f.compositor_arc().lock().unwrap();
        let win = comp.windows.get(&(new_id as u16)).expect("window exists");
        assert_eq!(win.pos, (0.0, 0.0));
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
