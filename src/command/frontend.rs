use crate::command::protocol::*;
use crate::cas::store::CasStore;
use crate::scene::graph::SceneGraph;
use crate::scene::nodes::Transform;
use crate::scene::sharing;

use std::sync::{Arc, Mutex};

pub struct CommandFrontend {
    cas: Arc<Mutex<CasStore>>,
    scene: Arc<Mutex<SceneGraph>>,
    pending_tasks: Vec<PendingTask>,
    pub last_upload_size: u64,
}

struct PendingTask {
    task_id: u32,
    task_type: u8,
    time_step: f32,
    config_hash: Hash256,
}

impl CommandFrontend {
    pub fn new(cas: Arc<Mutex<CasStore>>, scene: Arc<Mutex<SceneGraph>>) -> Self {
        Self {
            cas,
            scene,
            pending_tasks: Vec::new(),
            last_upload_size: 0,
        }
    }

    pub fn dispatch(&mut self, cmd: &Command) -> Option<Completion> {
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

            CMD_RENDER => self.handle_render(cmd),
            CMD_FENCE => self.handle_fence(cmd),
            CMD_QUERY_HASH => self.handle_query_hash(cmd),

            _ => {
                log::warn!("unknown command opcode: 0x{:04x}", cmd.opcode);
                Some(Completion::error(cmd.sequence_id, STATUS_INVALID_HASH))
            }
        }
    }

    fn handle_upload_begin(&mut self, cmd: &Command) -> Option<Completion> {
        let total_size = cmd.u32_at(8) as usize;
        let bytes = cmd.payload_bytes();
        let available = bytes.len() - 8;
        let data = &bytes[8..8 + available.min(total_size)];

        let mut cas = self.cas.lock().unwrap();
        cas.begin_upload(cmd.sequence_id, total_size, data);
        None
    }

    fn handle_upload_data(&mut self, cmd: &Command) -> Option<Completion> {
        let _offset = cmd.u32_at(8);
        let bytes = cmd.payload_bytes();
        let data = &bytes[4..bytes.len().min(120)]; // up to 116 bytes of data

        let mut cas = self.cas.lock().unwrap();
        cas.append_upload(cmd.sequence_id, data);
        None
    }

    fn handle_upload_finish(&mut self, cmd: &Command) -> Option<Completion> {
        let upload_id = cmd.u32_at(40);

        let mut cas = self.cas.lock().unwrap();
        match cas.finish_upload(cmd.sequence_id) {
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

    fn handle_upload_dma(&mut self, _cmd: &Command) -> Option<Completion> {
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

        // Arc-style ref management: inc new tree, dec old tree
        cas.inc_ref_tree(&root_hash);
        if old_root != NULL_HASH {
            cas.dec_ref_tree(&old_root);
        }

        log::trace!("set root: {:02x}{:02x}.. (freed {} blobs, {} KB)",
            root_hash[0], root_hash[1],
            cas.gc_freed_blobs, cas.gc_freed_bytes / 1024);
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

    fn handle_update_light(&mut self, _cmd: &Command) -> Option<Completion> {
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

    fn handle_render(&mut self, _cmd: &Command) -> Option<Completion> {
        let mut scene = self.scene.lock().unwrap();
        let cas = self.cas.lock().unwrap();
        log::trace!("CMD_RENDER: traversing root {:02x}{:02x}.. (CAS blobs: {})",
            scene.root_hash[0], scene.root_hash[1], cas.blob_count());
        scene.traverse(&cas);
        log::trace!("CMD_RENDER: render_list={} lights={}",
            scene.render_list().len(), scene.light_list().len());
        None
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
}
