use crate::command::protocol::*;
use crate::cas::store::CasStore;
use crate::scene::graph::SceneGraph;
use crate::scene::nodes::Transform;
use crate::scene::sharing;
use crate::scene::slots::{SlotTable, TextData};
use crate::window::{Compositor, FocusChange};

/// Append FOCUS completions for a focus shift: blurred for `prev`,
/// focused for `new`. Caller is the destination ring/queue.
pub fn push_focus_completions(out: &mut Vec<Completion>, shift: Option<FocusChange>) {
    let Some(fc) = shift else { return; };
    if let Some(prev) = fc.prev {
        out.push(Completion {
            comp_type:   COMP_WINDOW_FOCUS,
            status:      0, // blurred
            id:          prev as u32,
            result_hash: [0u8; 32],
            _pad:        [0; 22],
        });
    }
    out.push(Completion {
        comp_type:   COMP_WINDOW_FOCUS,
        status:      1, // focused
        id:          fc.new as u32,
        result_hash: [0u8; 32],
        _pad:        [0; 22],
    });
}

use std::sync::{Arc, Mutex};

pub struct CommandFrontend {
    cas: Arc<Mutex<CasStore>>,
    scene: Arc<Mutex<SceneGraph>>,
    /// Currently-bound window's slot table. dispatch() rebinds this
    /// to the target window's Arc for the duration of each routable
    /// handler call, so handlers operate on per-window state without
    /// having to thread a window_id through every call site.
    pub slot_table: Arc<Mutex<SlotTable>>,
    pub compositor: Arc<Mutex<Compositor>>,
    pending_tasks: Vec<PendingTask>,
    /// Extra completions that handlers need to emit beyond the single
    /// dispatch() return value (e.g. FOCUS shifts caused by a
    /// DESTROY_WINDOW). Drained by the caller after each command.
    pub pending_completions: Vec<Completion>,
    pub last_upload_size: u64,
    frame_number: u32,
    in_frame: bool,
    /// Slot id of the client whose command is currently being
    /// dispatched. Used by handlers (e.g. handle_destroy_window) to
    /// enforce window ownership. Set by dispatch(), cleared after.
    current_client: u8,
}

struct PendingTask {
    task_id: u32,
    task_type: u8,
    time_step: f32,
    config_hash: Hash256,
}

impl CommandFrontend {
    pub fn new(cas: Arc<Mutex<CasStore>>,
               scene: Arc<Mutex<SceneGraph>>,
               slot_table: Arc<Mutex<SlotTable>>,
               compositor: Arc<Mutex<Compositor>>) -> Self {
        Self {
            cas,
            scene,
            slot_table,
            compositor,
            pending_tasks: Vec::new(),
            pending_completions: Vec::new(),
            last_upload_size: 0,
            frame_number: 0,
            in_frame: false,
            current_client: 0,
        }
    }


    pub fn reset(&mut self) {
        self.pending_tasks.clear();
        self.last_upload_size = 0;
        // Clear all windows' slot tables — guest reset means fresh
        // start, including any auxiliary windows the previous client
        // had open.
        self.slot_table.lock().unwrap().clear();
        let comp = self.compositor.lock().unwrap();
        for win in comp.windows.values() {
            win.slots.lock().unwrap().clear();
        }
        drop(comp);
        self.frame_number = 0;
        self.in_frame = false;
    }

    pub fn dispatch(&mut self, cmd: &Command, client_id: u8) -> Option<Completion> {
        self.current_client = client_id;
        // Routable opcodes (slot/frame/scene ops) carry the target
        // window_id in cmd.flags. We bind self.slot_table / self.scene
        // to that window's Arcs for the duration of the handler, then
        // restore. Window 0 is the default "screen" window created at
        // startup; clients target other windows via CREATE_WINDOW.
        let restore = if self.is_routable(cmd.opcode) {
            let saved = (self.slot_table.clone(), self.scene.clone());
            let comp = self.compositor.lock().unwrap();
            let Some(win) = comp.windows.get(&cmd.flags) else {
                log::warn!("dispatch op 0x{:04x}: window {} doesn't exist; dropping",
                    cmd.opcode, cmd.flags);
                return None;
            };
            // Reject routable ops on windows the requester doesn't
            // own. Window 0 (the screen) is special-cased: clients
            // can render into the screen scene from any slot — the
            // OS-style isolation only applies to client-created
            // windows.
            if cmd.flags != 0 && win.owner != client_id as u32 {
                log::warn!("dispatch op 0x{:04x}: window {} owner={} but requester={}; dropping",
                    cmd.opcode, cmd.flags, win.owner, client_id);
                return None;
            }
            let win_slots = win.slots.clone();
            let win_scene = win.scene.clone();
            drop(comp);
            log::trace!("dispatch op 0x{:04x} routed to window {}", cmd.opcode, cmd.flags);
            self.slot_table = win_slots;
            self.scene = win_scene;
            Some(saved)
        } else {
            None
        };

        let result = self.dispatch_inner(cmd);

        if let Some((slots, scene)) = restore {
            self.slot_table = slots;
            self.scene = scene;
        }
        result
    }

    /// Opcodes whose handlers operate on per-window scene/slot state.
    /// Other opcodes (CAS uploads, query, fence, window lifecycle) are
    /// global and don't get routed.
    fn is_routable(&self, opcode: u16) -> bool {
        matches!(opcode,
            CMD_SET_ROOT | CMD_SET_CAMERA | CMD_ADD_NODE | CMD_REMOVE_NODE
            | CMD_UPDATE_TRANSFORM | CMD_UPDATE_TRANSFORM_INLINE
            | CMD_UPDATE_MATERIAL | CMD_ADD_LIGHT | CMD_UPDATE_LIGHT
            | CMD_SLOT_ALLOC | CMD_SLOT_FREE | CMD_SLOT_SET_XFORM
            | CMD_SLOT_SET_CONTENT | CMD_SLOT_SET_CHILDREN | CMD_SLOT_SET_FLAGS
            | CMD_SLOT_SET_ROOT | CMD_SLOT_SET_TEXT | CMD_SLOT_SET_CAS_CHILDREN
            | CMD_RENDER | CMD_FRAME_BEGIN | CMD_FRAME_END
        )
    }

    fn dispatch_inner(&mut self, cmd: &Command) -> Option<Completion> {
        match cmd.opcode {
            CMD_UPLOAD_BEGIN => self.handle_upload_begin(cmd),
            CMD_UPLOAD_DATA => self.handle_upload_data(cmd),
            CMD_UPLOAD_FINISH => self.handle_upload_finish(cmd),
            CMD_UPLOAD_DMA => self.handle_upload_dma(cmd),

            CMD_SET_ROOT => self.handle_set_root(cmd),
            CMD_SET_CAMERA => self.handle_set_camera(cmd),
            CMD_ADD_NODE => self.handle_add_node(cmd),
            CMD_REMOVE_NODE => self.handle_remove_node(cmd),
            CMD_UPDATE_TRANSFORM => self.handle_update_transform(cmd),
            CMD_UPDATE_TRANSFORM_INLINE => self.handle_update_transform_inline(cmd),
            CMD_UPDATE_MATERIAL => self.handle_update_material(cmd),
            CMD_ADD_LIGHT => self.handle_add_light(cmd),
            CMD_UPDATE_LIGHT => self.handle_update_light(cmd),

            CMD_SPAWN_TASK => self.handle_spawn_task(cmd),
            CMD_SPAWN_TASK_TARGET => self.handle_spawn_task_target(cmd),
            CMD_STOP_TASK => self.handle_stop_task(cmd),

            CMD_SLOT_ALLOC => self.handle_slot_alloc(cmd),
            CMD_SLOT_FREE => self.handle_slot_free(cmd),
            CMD_SLOT_SET_XFORM => self.handle_slot_set_xform(cmd),
            CMD_SLOT_SET_CONTENT => self.handle_slot_set_content(cmd),
            CMD_SLOT_SET_CHILDREN => self.handle_slot_set_children(cmd),
            CMD_SLOT_SET_FLAGS => self.handle_slot_set_flags(cmd),
            CMD_SLOT_SET_ROOT => self.handle_slot_set_root(cmd),
            CMD_SLOT_SET_TEXT => self.handle_slot_set_text(cmd),
            CMD_SLOT_SET_CAS_CHILDREN => self.handle_slot_set_cas_children(cmd),

            CMD_RENDER => self.handle_render(cmd),
            CMD_FENCE => self.handle_fence(cmd),
            CMD_QUERY_HASH => self.handle_query_hash(cmd),
            CMD_FRAME_BEGIN => self.handle_frame_begin(cmd),
            CMD_FRAME_END => self.handle_frame_end(cmd),

            // ── Multi-window lifecycle ─────────────────────────
            CMD_CREATE_WINDOW    => self.handle_create_window(cmd),
            CMD_DESTROY_WINDOW   => self.handle_destroy_window(cmd),
            CMD_WINDOW_SET_TITLE => self.handle_window_set_title(cmd),
            CMD_WINDOW_SET_POS   => self.handle_window_set_pos(cmd),
            CMD_WINDOW_SET_SIZE  => self.handle_window_set_size(cmd),
            CMD_WINDOW_SET_ROOT  => { log::info!("WINDOW_SET_ROOT (stub): win={}", cmd.u32_at(8)); None }
            CMD_WINDOW_PRESENT   => { log::info!("WINDOW_PRESENT (stub): win={}", cmd.u32_at(8)); None }

            _ => {
                log::warn!("unknown command opcode: 0x{:04x}", cmd.opcode);
                Some(Completion::error(cmd.sequence_id, STATUS_INVALID_HASH))
            }
        }
    }

    /// Compose a CAS upload-staging key from the current client's
    /// id and the command's sequence_id. Each client has its own seq
    /// space, so namespacing with client_id makes the key unique
    /// across clients — no more interleave when two apps upload at
    /// the same time.
    fn upload_key(&self, seq: u32) -> u64 {
        ((self.current_client as u64) << 32) | (seq as u64)
    }

    fn handle_upload_begin(&mut self, cmd: &Command) -> Option<Completion> {
        let total_size = cmd.u32_at(8) as usize;
        let bytes = cmd.payload_bytes();
        let available = bytes.len() - 8;
        let data = &bytes[8..8 + available.min(total_size)];

        let key = self.upload_key(cmd.sequence_id);
        let mut cas = self.cas.lock().unwrap();
        cas.begin_upload(key, total_size, data);
        None
    }

    fn handle_upload_data(&mut self, cmd: &Command) -> Option<Completion> {
        let _offset = cmd.u32_at(8);
        let bytes = cmd.payload_bytes();
        let data = &bytes[4..bytes.len().min(120)]; // up to 116 bytes of data

        let key = self.upload_key(cmd.sequence_id);
        let mut cas = self.cas.lock().unwrap();
        cas.append_upload(key, data);
        None
    }

    fn handle_upload_finish(&mut self, cmd: &Command) -> Option<Completion> {
        let upload_id = cmd.u32_at(40);

        let key = self.upload_key(cmd.sequence_id);
        let mut cas = self.cas.lock().unwrap();
        match cas.finish_upload(key) {
            Some(hash) => {
                let size = cas.load(&hash).map(|d| d.len()).unwrap_or(0);
                log::trace!("upload complete: id={} hash={:02x}{:02x}.. size={}",
                    upload_id, hash[0], hash[1], size);
                self.last_upload_size = size as u64;
                Some(Completion::upload_complete(upload_id, hash))
            }
            None => Some(Completion::error(upload_id, STATUS_INVALID_HASH)),
        }
    }

    fn handle_upload_dma(&mut self, cmd: &Command) -> Option<Completion> {
        // staging area upload — read from ivshmem staging region
        // TODO: implement when staging area is mapped
        log::warn!("CMD_UPLOAD_DMA not yet implemented");
        None
    }

    fn handle_set_root(&mut self, cmd: &Command) -> Option<Completion> {
        let root_hash = cmd.hash_at(8);
        let mut cas = self.cas.lock().unwrap();
        let mut scene = self.scene.lock().unwrap();

        let old_root = scene.root_hash;
        scene.set_root(root_hash);

        // No GC on set_root — blobs are retained until guest reset.
        // GC was freeing shared blobs due to async SET_ROOT/upload ordering:
        // the WM's SET_ROOT arrives interleaved with the next frame's uploads,
        // so inc_ref_tree can't walk children that haven't been uploaded yet,
        // leaving them unprotected when dec_ref_tree runs.
        log::trace!("set root: {:02x}{:02x}.. (CAS: {} blobs)",
            root_hash[0], root_hash[1], cas.blob_count());
        None
    }

    fn handle_set_camera(&mut self, cmd: &Command) -> Option<Completion> {
        let camera_hash = cmd.hash_at(8);
        let mut scene = self.scene.lock().unwrap();
        scene.set_camera(camera_hash);
        None
    }

    fn handle_add_node(&mut self, cmd: &Command) -> Option<Completion> {
        let parent_hash = cmd.hash_at(8);
        let node_hash = cmd.hash_at(40);

        let mut cas = self.cas.lock().unwrap();
        let mut scene = self.scene.lock().unwrap();

        if let Some(new_root) = sharing::add_node_to_parent(
            &mut cas, &scene.root_hash, &parent_hash, &node_hash,
        ) {
            log::trace!("add_node: new root {:02x}{:02x}...", new_root[0], new_root[1]);
            scene.set_root(new_root);
        } else {
            log::warn!("add_node: parent {:02x}{:02x}... not found", parent_hash[0], parent_hash[1]);
        }
        None
    }

    fn handle_remove_node(&mut self, cmd: &Command) -> Option<Completion> {
        let parent_hash = cmd.hash_at(8);
        let node_hash = cmd.hash_at(40);

        let mut cas = self.cas.lock().unwrap();
        let mut scene = self.scene.lock().unwrap();

        if let Some(new_root) = sharing::remove_node_from_parent(
            &mut cas, &scene.root_hash, &parent_hash, &node_hash,
        ) {
            scene.set_root(new_root);
        }
        None
    }

    fn handle_update_transform(&mut self, cmd: &Command) -> Option<Completion> {
        let node_hash = cmd.hash_at(8);
        let new_xform_hash = cmd.hash_at(40);

        let mut cas = self.cas.lock().unwrap();
        let mut scene = self.scene.lock().unwrap();

        if let Some(new_root) = sharing::update_scene_node_transform(
            &mut cas, &scene.root_hash, &node_hash, &new_xform_hash,
        ) {
            scene.set_root(new_root);
        }
        None
    }

    fn handle_update_transform_inline(&mut self, cmd: &Command) -> Option<Completion> {
        let pos = [cmd.f32_at(8), cmd.f32_at(12), cmd.f32_at(16)];
        let rot = [cmd.f32_at(20), cmd.f32_at(24), cmd.f32_at(28), cmd.f32_at(32)];
        let scale = cmd.f32_at(36);
        let node_hash = cmd.hash_at(40);

        let transform = Transform::from_trs(pos, rot, scale);
        let xform_buf = sharing::serialize_transform(
            transform.flags, transform.scale_hint, &transform.matrix,
        );

        let mut cas = self.cas.lock().unwrap();
        let xform_hash = cas.store(&xform_buf);

        let mut scene = self.scene.lock().unwrap();

        if let Some(new_root) = sharing::update_scene_node_transform(
            &mut cas, &scene.root_hash, &node_hash, &xform_hash,
        ) {
            scene.set_root(new_root);
        }
        None
    }

    fn handle_update_material(&mut self, cmd: &Command) -> Option<Completion> {
        let node_hash = cmd.hash_at(8);
        let material_hash = cmd.hash_at(40);

        let mut cas = self.cas.lock().unwrap();
        let mut scene = self.scene.lock().unwrap();

        if let Some(new_root) = sharing::update_scene_node_material(
            &mut cas, &scene.root_hash, &node_hash, &material_hash,
        ) {
            scene.set_root(new_root);
        }
        None
    }

    fn handle_add_light(&mut self, cmd: &Command) -> Option<Completion> {
        let parent_hash = cmd.hash_at(8);
        let light_hash = cmd.hash_at(40);

        let mut cas = self.cas.lock().unwrap();
        let mut scene = self.scene.lock().unwrap();

        if let Some(new_root) = sharing::add_node_to_parent(
            &mut cas, &scene.root_hash, &parent_hash, &light_hash,
        ) {
            scene.set_root(new_root);
        }
        None
    }

    fn handle_update_light(&mut self, cmd: &Command) -> Option<Completion> {
        let mut scene = self.scene.lock().unwrap();
        scene.mark_dirty();
        None
    }

    fn handle_spawn_task(&mut self, cmd: &Command) -> Option<Completion> {
        let task_type = cmd.payload_bytes()[0];
        let time_step = cmd.f32_at(12);
        let task_id = cmd.u32_at(16);
        let config_hash = cmd.hash_at(20);

        self.pending_tasks.push(PendingTask {
            task_id,
            task_type,
            time_step,
            config_hash,
        });

        log::trace!("spawn task: id={} type={}", task_id, task_type);
        None
    }

    fn handle_spawn_task_target(&mut self, cmd: &Command) -> Option<Completion> {
        let task_id = cmd.u32_at(8);
        let _target_hash = cmd.hash_at(12);

        if let Some(idx) = self.pending_tasks.iter().position(|t| t.task_id == task_id) {
            let _task = self.pending_tasks.remove(idx);
            // TODO: register autonomous task with the task runner
            log::trace!("task {} target bound", task_id);
        }
        None
    }

    fn handle_stop_task(&mut self, cmd: &Command) -> Option<Completion> {
        let task_id = cmd.u32_at(8);
        // TODO: stop autonomous task
        log::trace!("stop task: id={}", task_id);
        None
    }

    fn handle_render(&mut self, cmd: &Command) -> Option<Completion> {
        let mut scene = self.scene.lock().unwrap();
        let mut cas = self.cas.lock().unwrap();
        cas.advance_generation();
        log::trace!("CMD_RENDER: traversing root {:02x}{:02x}.. (CAS blobs: {} gen={})",
            scene.root_hash[0], scene.root_hash[1], cas.blob_count(), cas.generation);
        scene.traverse(&mut cas);
        log::trace!("CMD_RENDER: render_list={} lights={}",
            scene.render_list().len(), scene.light_list().len());
        None
    }

    pub fn maybe_sweep(&self) {
        // Sweep disabled: guest upload cache has no way to know the GPU server
        // freed a blob, so it skips re-uploading. CAS grows until guest reset.
        // Proper fix: blob existence query protocol or server→guest invalidation.
    }

    fn handle_fence(&mut self, cmd: &Command) -> Option<Completion> {
        let fence_value = cmd.u32_at(8);
        Some(Completion::fence(fence_value))
    }

    fn handle_query_hash(&mut self, cmd: &Command) -> Option<Completion> {
        let query_hash = cmd.hash_at(8);
        let query_id = cmd.u32_at(40);
        let cas = self.cas.lock().unwrap();
        let exists = cas.exists(&query_hash);
        Some(Completion::query_result(query_id, exists))
    }

    // ── Slot-based scene graph (v2) ────────────────────────────

    fn handle_slot_alloc(&mut self, cmd: &Command) -> Option<Completion> {
        let bytes = cmd.payload_bytes();
        let slot_id = u16::from_le_bytes([bytes[0], bytes[1]]);
        let node_type = u16::from_le_bytes([bytes[2], bytes[3]]);
        let flags = cmd.u32_at(12);
        let xform_hash = cmd.hash_at(16);
        let rend_hash = cmd.hash_at(48);
        let child_count = u16::from_le_bytes([bytes[72], bytes[73]]) as usize;

        let mut st = self.slot_table.lock().unwrap();
        st.alloc(slot_id, node_type, flags);

        if xform_hash != NULL_HASH {
            let cas = self.cas.lock().unwrap();
            if let Some(data) = cas.load(&xform_hash) {
                if let Some(crate::scene::nodes::NodeData::Transform(t)) = crate::scene::nodes::NodeData::parse(data) {
                    st.set_transform_inline(slot_id, t.matrix);
                }
            }
        }
        if rend_hash != NULL_HASH {
            st.set_content(slot_id, rend_hash);
        }
        if child_count > 0 {
            let max = child_count.min(22); // (120 - 76) / 2 = 22 u16 slots
            let mut children = Vec::with_capacity(max);
            for i in 0..max {
                let off = 76 + i * 2;
                if off + 2 > bytes.len() { break; }
                children.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
            }
            st.set_children(slot_id, children);
        }

        log::trace!("SLOT_ALLOC: id={} type={} flags=0x{:x} children={}",
            slot_id, node_type, flags, child_count);
        None
    }

    fn handle_slot_free(&mut self, cmd: &Command) -> Option<Completion> {
        let slot_id = cmd.u16_at(8);
        self.slot_table.lock().unwrap().free(slot_id);
        log::trace!("SLOT_FREE: id={}", slot_id);
        None
    }

    fn handle_slot_set_xform(&mut self, cmd: &Command) -> Option<Completion> {
        let slot_id = cmd.u16_at(8);
        let mode = cmd.u16_at(10);
        let mut st = self.slot_table.lock().unwrap();

        match mode {
            0 => {
                // hash reference
                let hash = cmd.hash_at(12);
                let cas = self.cas.lock().unwrap();
                if let Some(data) = cas.load(&hash) {
                    if let Some(crate::scene::nodes::NodeData::Transform(t)) = crate::scene::nodes::NodeData::parse(data) {
                        st.set_transform_inline(slot_id, t.matrix);
                    }
                }
            }
            1 => {
                // inline 4x4 matrix (64 bytes at offset 12)
                let bytes = cmd.payload_bytes();
                let mut matrix = [0.0f32; 16];
                for i in 0..16 {
                    let off = 4 + i * 4; // payload offset (12 - 8 = 4)
                    matrix[i] = f32::from_le_bytes([
                        bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3],
                    ]);
                }
                st.set_transform_inline(slot_id, matrix);
            }
            2 => {
                // translate only (12 bytes: x, y, z)
                let x = cmd.f32_at(12);
                let y = cmd.f32_at(16);
                let z = cmd.f32_at(20);
                st.set_transform_translate(slot_id, x, y, z);
            }
            _ => {}
        }
        None
    }

    fn handle_slot_set_content(&mut self, cmd: &Command) -> Option<Completion> {
        let slot_id = cmd.u16_at(8);
        let hash = cmd.hash_at(10);
        self.slot_table.lock().unwrap().set_content(slot_id, hash);
        None
    }

    fn handle_slot_set_children(&mut self, cmd: &Command) -> Option<Completion> {
        let bytes = cmd.payload_bytes();
        let slot_id = u16::from_le_bytes([bytes[0], bytes[1]]);
        let count = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
        let _flags = u16::from_le_bytes([bytes[4], bytes[5]]);

        let max = count.min(56); // (120 - 8) / 2 = 56 u16 slots
        let mut children = Vec::with_capacity(max);
        for i in 0..max {
            let off = 6 + i * 2;
            if off + 2 > bytes.len() { break; }
            children.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
        }

        self.slot_table.lock().unwrap().set_children(slot_id, children);
        None
    }

    fn handle_slot_set_flags(&mut self, cmd: &Command) -> Option<Completion> {
        let slot_id = cmd.u16_at(8);
        let flags = cmd.u32_at(10);
        let clip = [
            cmd.f32_at(14),
            cmd.f32_at(18),
            cmd.f32_at(22),
            cmd.f32_at(26),
        ];
        self.slot_table.lock().unwrap().set_flags(slot_id, flags, clip);
        None
    }

    fn handle_slot_set_root(&mut self, cmd: &Command) -> Option<Completion> {
        let slot_id = cmd.u16_at(8);
        self.slot_table.lock().unwrap().set_root(slot_id);
        log::trace!("SLOT_SET_ROOT: slot={}", slot_id);
        None
    }

    fn handle_slot_set_text(&mut self, cmd: &Command) -> Option<Completion> {
        let slot_id = cmd.u16_at(8);
        let size = cmd.f32_at(10);
        let color = cmd.u32_at(14);
        let font_hash = cmd.hash_at(18);
        let bytes = cmd.payload_bytes();
        let text_start = 50 - 8; // offset 50 in command = 42 in payload
        let text_end = bytes.len().min(text_start + 74);
        let mut text = [0u8; 80];
        let text_len = text_end - text_start;
        text[..text_len].copy_from_slice(&bytes[text_start..text_end]);

        self.slot_table.lock().unwrap().set_text(slot_id, TextData {
            size, color, font_hash, text, text_len,
        });
        None
    }

    fn handle_slot_set_cas_children(&mut self, cmd: &Command) -> Option<Completion> {
        let slot_id = cmd.u16_at(8);
        let hash = cmd.hash_at(10);
        self.slot_table.lock().unwrap().set_cas_subtree(slot_id, hash);
        log::trace!("SLOT_SET_CAS_CHILDREN: slot={} hash={:02x}{:02x}..",
            slot_id, hash[0], hash[1]);
        None
    }

    // ── Frame control ──────────────────────────────────────────

    fn handle_frame_begin(&mut self, cmd: &Command) -> Option<Completion> {
        self.frame_number = cmd.u32_at(8);
        self.in_frame = true;
        log::trace!("FRAME_BEGIN: frame={}", self.frame_number);
        None
    }

    fn handle_frame_end(&mut self, cmd: &Command) -> Option<Completion> {
        self.in_frame = false;
        let st = self.slot_table.lock().unwrap();
        if st.is_active() {
            let mut cas = self.cas.lock().unwrap();
            cas.advance_generation();
            let (mut render_list, mut camera_hash, cas_subtrees) = st.traverse(&mut cas);
            let mut scene = self.scene.lock().unwrap();
            if camera_hash == NULL_HASH {
                camera_hash = scene.camera_hash;
            }
            // Process CAS subtrees with SceneGraph's font_cache + resolve_mesh
            for (list_hash, matrix, clip) in &cas_subtrees {
                scene.traverse_cas_into(
                    &mut cas, list_hash, matrix, *clip, &mut render_list,
                );
            }
            scene.set_slot_render_list(render_list, camera_hash);
            log::info!("FRAME_END: frame={} slots={} cas_subtrees={} render_items={} cas_blobs={}",
                self.frame_number, st.slot_count(), cas_subtrees.len(),
                scene.render_list().len(), cas.blob_count());
        } else {
            // No slots active — fall back to CAS traversal (backward compat)
            let mut scene = self.scene.lock().unwrap();
            let mut cas = self.cas.lock().unwrap();
            cas.advance_generation();
            scene.traverse(&mut cas);
            log::trace!("FRAME_END: frame={} (CAS mode) render_items={}",
                self.frame_number, scene.render_list().len());
        }
        None
    }

    // ── Multi-window lifecycle handlers ─────────────────────────────
    //
    // CREATE_WINDOW payload (post-cmd-header at offset 8):
    //   [0..4]   width  (u32)
    //   [4..8]   height (u32)
    //   [8..12]  flags  (u32, reserved, must be 0)
    //   [12..28] short title bytes (utf-8, NUL-terminated, optional)

    fn handle_create_window(&mut self, cmd: &Command) -> Option<Completion> {
        let w = cmd.u32_at(8);
        let h = cmd.u32_at(12);
        let _flags = cmd.u32_at(16);
        let bytes = cmd.payload_bytes();
        // Short title: 16 bytes at payload offset 12 (cmd offset 20). NUL-terminated.
        let title_buf = &bytes[12..28.min(bytes.len())];
        let title_end = title_buf.iter().position(|&b| b == 0).unwrap_or(title_buf.len());
        let title = String::from_utf8_lossy(&title_buf[..title_end]).into_owned();

        let mut comp = self.compositor.lock().unwrap();
        let win_id = comp.create(self.current_client as u32, (w.max(1), h.max(1)));
        if !title.is_empty() {
            if let Some(win) = comp.windows.get_mut(&win_id) {
                win.title = title.clone();
            }
            let mut cas = self.cas.lock().unwrap();
            comp.rebuild_window_title(win_id, &mut cas);
        }
        log::info!("CREATE_WINDOW: id={} {}x{} title={:?}", win_id, w, h, title);

        // Echo the request's sequence_id in result_hash[0..4] so the
        // client can correlate this completion with its request even
        // if multiple CREATE_WINDOWs are in flight. comp.id is the
        // server-assigned window_id.
        let mut result_hash = [0u8; 32];
        result_hash[..4].copy_from_slice(&cmd.sequence_id.to_le_bytes());
        Some(Completion {
            comp_type:   COMP_WINDOW_CREATED,
            status:      STATUS_OK,
            id:          win_id as u32,
            result_hash,
            _pad:        [0; 22],
        })
    }

    fn handle_destroy_window(&mut self, cmd: &Command) -> Option<Completion> {
        let win_id = cmd.u32_at(8) as u16;
        let mut comp = self.compositor.lock().unwrap();
        let (ok, shift) = comp.destroy_with_focus_shift(win_id, self.current_client as u32);
        drop(comp);
        log::info!("DESTROY_WINDOW: id={} ok={}", win_id, ok);
        if let Some(fc) = shift {
            push_focus_completions(&mut self.pending_completions, Some(fc));
        }
        None
    }

    fn handle_window_set_size(&mut self, cmd: &Command) -> Option<Completion> {
        let win_id = cmd.u32_at(8) as u16;
        let w = cmd.u32_at(12);
        let h = cmd.u32_at(16);
        if w == 0 || h == 0 {
            log::warn!("WINDOW_SET_SIZE: id={} ignored zero dimension {}x{}", win_id, w, h);
            return None;
        }
        let mut comp = self.compositor.lock().unwrap();
        let owner = comp.windows.get(&win_id).map(|w| w.owner);
        if owner != Some(self.current_client as u32) {
            log::warn!("WINDOW_SET_SIZE: id={} rejected (owner={:?}, requester={})",
                win_id, owner, self.current_client);
            return None;
        }
        comp.windows.get_mut(&win_id).unwrap().size = (w as f32, h as f32);
        // Re-layout title for the new width — ellipsis tracks size.
        let mut cas = self.cas.lock().unwrap();
        comp.rebuild_window_title(win_id, &mut cas);
        drop(cas);
        drop(comp);
        log::info!("WINDOW_SET_SIZE: id={} {}x{}", win_id, w, h);

        // Emit RESIZED completion. Width/height packed into
        // result_hash[0..8] little-endian — mirrors libfresco's
        // fresco_winevt_try_enqueue decoding.
        let mut rh = [0u8; 32];
        rh[0..4].copy_from_slice(&w.to_le_bytes());
        rh[4..8].copy_from_slice(&h.to_le_bytes());
        self.pending_completions.push(Completion {
            comp_type:   COMP_WINDOW_RESIZED,
            status:      0,
            id:          win_id as u32,
            result_hash: rh,
            _pad:        [0; 22],
        });
        None
    }

    fn handle_window_set_pos(&mut self, cmd: &Command) -> Option<Completion> {
        let win_id = cmd.u32_at(8) as u16;
        let x = cmd.f32_at(12);
        let y = cmd.f32_at(16);
        let mut comp = self.compositor.lock().unwrap();
        let owner = comp.windows.get(&win_id).map(|w| w.owner);
        if owner != Some(self.current_client as u32) {
            log::warn!("WINDOW_SET_POS: id={} rejected (owner={:?}, requester={})",
                win_id, owner, self.current_client);
            return None;
        }
        comp.windows.get_mut(&win_id).unwrap().pos = (x, y);
        log::info!("WINDOW_SET_POS: id={} ({}, {})", win_id, x, y);
        None
    }

    fn handle_window_set_title(&mut self, cmd: &Command) -> Option<Completion> {
        let win_id = cmd.u32_at(8) as u16;
        let bytes = cmd.payload_bytes();
        let title_buf = &bytes[4..];
        let end = title_buf.iter().position(|&b| b == 0).unwrap_or(title_buf.len());
        let title = String::from_utf8_lossy(&title_buf[..end]).into_owned();

        let mut comp = self.compositor.lock().unwrap();
        let owner = comp.windows.get(&win_id).map(|w| w.owner);
        if owner != Some(self.current_client as u32) {
            log::warn!("WINDOW_SET_TITLE: id={} rejected (owner={:?}, requester={})",
                win_id, owner, self.current_client);
            return None;
        }
        comp.windows.get_mut(&win_id).unwrap().title = title.clone();
        let mut cas = self.cas.lock().unwrap();
        comp.rebuild_window_title(win_id, &mut cas);
        log::info!("WINDOW_SET_TITLE: id={} title={:?}", win_id, title);
        None
    }
}
