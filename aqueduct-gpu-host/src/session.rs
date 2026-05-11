//! Per-connection session — owns one aqueduct connection,
//! per-connection resource table, and the dispatch loop that handles
//! incoming envelopes.

use std::sync::Arc;

use anyhow::{Context, Result};
use aqueduct::envelope::flag;
use aqueduct::{Connection, Message};

use aqueduct_gpu::opcodes::*;
use aqueduct_gpu::payloads::*;
use aqueduct_gpu::CLASS_GPU;

use crate::backend::Backend;
use crate::resources::{
    BufferRecord, FenceRecord, ImageRecord, MemoryRecord, PipelineRecord,
    ResourceTable, SamplerRecord, ShaderRecord,
};

/// One connection's worth of state. Constructed by the listener when
/// a guest connects.
pub struct Session {
    conn: Connection,
    table: ResourceTable,
    backend: Arc<dyn Backend>,
    /// Has the client completed the handshake yet? Pre-handshake
    /// clients are limited to OP_GPU_HANDSHAKE; anything else gets
    /// a validation error.
    handshake_done: bool,
    /// Monotonic timeline observed from the client's submits.
    last_timeline: u64,
}

impl Session {
    /// Construct a fresh session.
    pub fn new(conn: Connection, backend: Arc<dyn Backend>) -> Self {
        Self {
            conn,
            table: ResourceTable::new(),
            backend,
            handshake_done: false,
            last_timeline: 0,
        }
    }

    /// Run the dispatch loop until the client disconnects or an
    /// unrecoverable error occurs. Logs each dispatched op at debug.
    pub fn run(mut self) -> Result<()> {
        loop {
            let m = match self.conn.recv_message() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    log::info!("client disconnected (EOF)");
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("recv error: {e}");
                    return Err(e).context("recv_message");
                }
            };

            if m.opcode_class != CLASS_GPU {
                log::warn!("unexpected class {} on aqueduct-gpu socket; closing",
                           m.opcode_class);
                return Ok(());
            }

            log::debug!("dispatch op={:#06x} flags={:#06x} payload_len={}",
                        m.op, m.flags, m.payload.len());

            if !self.handshake_done && m.op != OP_GPU_HANDSHAKE {
                self.send_validation_err(m.op, None,
                    "must complete OP_GPU_HANDSHAKE first")?;
                continue;
            }

            if let Err(e) = self.dispatch(m) {
                log::warn!("dispatch error: {e}");
            }
        }
    }

    fn dispatch(&mut self, m: Message) -> Result<()> {
        match m.op {
            OP_GPU_HANDSHAKE          => self.handle_handshake(m),
            OP_GPU_MEMORY_CREATE      => self.handle_memory_create(m),
            OP_GPU_MEMORY_DESTROY     => self.handle_memory_destroy(m),
            OP_GPU_IMAGE_CREATE       => self.handle_image_create(m),
            OP_GPU_IMAGE_DESTROY      => self.handle_image_destroy(m),
            OP_GPU_BUFFER_CREATE      => self.handle_buffer_create(m),
            OP_GPU_BUFFER_DESTROY     => self.handle_buffer_destroy(m),
            OP_GPU_SAMPLER_CREATE     => self.handle_sampler_create(m),
            OP_GPU_SAMPLER_DESTROY    => self.handle_sampler_destroy(m),
            OP_GPU_SHADER_RESOLVE     => self.handle_shader_resolve(m),
            OP_GPU_SHADER_UPLOAD      => self.handle_shader_upload(m),
            OP_GPU_PIPELINE_CREATE    => self.handle_pipeline_create(m),
            OP_GPU_PIPELINE_DESTROY   => self.handle_pipeline_destroy(m),
            OP_GPU_FENCE_CREATE       => self.handle_fence_create(m),
            OP_GPU_FENCE_DESTROY      => self.handle_fence_destroy(m),
            OP_GPU_SUBMIT_FRAME       => self.handle_submit_frame(m),
            OP_GPU_WAIT_FENCE         => self.handle_wait_fence(m),
            OP_GPU_SHARE_SURFACE      => self.handle_share_surface(m),
            OP_GPU_BUNDLE_LOAD        => self.handle_bundle_load(m),
            OP_GPU_BUNDLE_UNLOAD      => self.handle_bundle_unload(m),
            other => {
                self.send_validation_err(other, None, "unknown op")?;
                Ok(())
            }
        }
    }

    // ─── Op handlers ──────────────────────────────────────────────

    fn handle_handshake(&mut self, m: Message) -> Result<()> {
        let req: HandshakePayload = postcard::from_bytes(&m.payload)?;
        let resp = HandshakeResponse {
            protocol_version: req.protocol_version, // accept whatever the client asks
            backend: self.backend.identity(),
            caps: self.backend.caps(),
            max_frame_bytes: self.backend.max_frame_bytes(),
            max_fences_inflight: self.backend.max_fences_inflight(),
        };
        self.send_response(OP_GPU_HANDSHAKE, &resp)?;
        self.handshake_done = true;
        log::info!("handshake complete: client_kind={:?}, backend={}",
                   req.client_kind, self.backend.identity());
        Ok(())
    }

    fn handle_memory_create(&mut self, m: Message) -> Result<()> {
        let req: MemoryCreatePayload = postcard::from_bytes(&m.payload)?;
        if let Err(why) = ResourceTable::validate_namespace(req.region_id) {
            self.send_validation_err(OP_GPU_MEMORY_CREATE, Some(req.region_id), why)?;
            return Ok(());
        }
        let token = self.backend.allocate_memory(req.size, req.usage as u8);
        self.table.insert_memory(req.region_id, MemoryRecord {
            size: req.size, usage: req.usage as u8, atrium_gpu_token: token,
        });
        let resp = MemoryCreateResponse {
            region_id: req.region_id,
            size: req.size,
            host_va_hint: 0,
            atrium_gpu_token: token,
        };
        self.send_response(OP_GPU_MEMORY_CREATE, &resp)
    }

    fn handle_memory_destroy(&mut self, m: Message) -> Result<()> {
        let req: MemoryDestroyPayload = postcard::from_bytes(&m.payload)?;
        self.table.remove_memory(req.region_id);
        Ok(())
    }

    fn handle_image_create(&mut self, m: Message) -> Result<()> {
        let req: ImageCreatePayload = postcard::from_bytes(&m.payload)?;
        if let Err(why) = ResourceTable::validate_namespace(req.image_id) {
            self.send_validation_err(OP_GPU_IMAGE_CREATE, Some(req.image_id), why)?;
            return Ok(());
        }
        self.table.insert_image(req.image_id, ImageRecord {
            backing_region: req.backing_region,
            width: req.width, height: req.height, depth: req.depth,
            format: req.format,
        });
        Ok(())
    }

    fn handle_image_destroy(&mut self, m: Message) -> Result<()> {
        let req: ImageDestroyPayload = postcard::from_bytes(&m.payload)?;
        self.table.remove_image(req.image_id);
        Ok(())
    }

    fn handle_buffer_create(&mut self, m: Message) -> Result<()> {
        let req: BufferCreatePayload = postcard::from_bytes(&m.payload)?;
        if let Err(why) = ResourceTable::validate_namespace(req.buffer_id) {
            self.send_validation_err(OP_GPU_BUFFER_CREATE, Some(req.buffer_id), why)?;
            return Ok(());
        }
        self.table.insert_buffer(req.buffer_id, BufferRecord {
            backing_region: req.backing_region, size: req.size,
        });
        Ok(())
    }

    fn handle_buffer_destroy(&mut self, m: Message) -> Result<()> {
        let req: BufferDestroyPayload = postcard::from_bytes(&m.payload)?;
        self.table.remove_buffer(req.buffer_id);
        Ok(())
    }

    fn handle_sampler_create(&mut self, m: Message) -> Result<()> {
        let req: SamplerCreatePayload = postcard::from_bytes(&m.payload)?;
        if let Err(why) = ResourceTable::validate_namespace(req.sampler_id) {
            self.send_validation_err(OP_GPU_SAMPLER_CREATE, Some(req.sampler_id), why)?;
            return Ok(());
        }
        self.table.insert_sampler(req.sampler_id, SamplerRecord { _placeholder: () });
        Ok(())
    }

    fn handle_sampler_destroy(&mut self, m: Message) -> Result<()> {
        let req: SamplerDestroyPayload = postcard::from_bytes(&m.payload)?;
        self.table.remove_sampler(req.sampler_id);
        Ok(())
    }

    fn handle_shader_resolve(&mut self, m: Message) -> Result<()> {
        let req: ShaderResolvePayload = postcard::from_bytes(&m.payload)?;
        // Stub backend: every shader is a Miss. Real backends look
        // up `(hash, backend, version)` in Tessera CAS.
        let resp = ShaderResolveResponse {
            bytecode_hash: req.bytecode_hash,
            status: ShaderResolveStatus::Miss,
            shader_id: None,
        };
        self.send_response(OP_GPU_SHADER_RESOLVE, &resp)
    }

    fn handle_shader_upload(&mut self, m: Message) -> Result<()> {
        let req: ShaderUploadPayload = postcard::from_bytes(&m.payload)?;
        // Stub backend: accept everything. Real backends run
        // spirv-val + bounded-loop analysis + backend codegen.
        let shader_id = aqueduct_gpu::ids::ResourceId::new(
            aqueduct_gpu::ids::IdNamespace::IcdRuntime,
            (req.bytecode_hash[0] as u32) << 16 | (req.bytecode_hash[1] as u32) << 8 | 1,
        );
        self.table.insert_shader(shader_id, ShaderRecord {
            bytecode_hash: req.bytecode_hash,
        });
        let resp = ShaderUploadResponse {
            bytecode_hash: req.bytecode_hash,
            shader_id: Some(shader_id),
            diagnostic: String::new(),
        };
        self.send_response(OP_GPU_SHADER_UPLOAD, &resp)
    }

    fn handle_pipeline_create(&mut self, m: Message) -> Result<()> {
        let req: PipelineCreatePayload = postcard::from_bytes(&m.payload)?;
        if let Err(why) = ResourceTable::validate_namespace(req.pipeline_id) {
            self.send_validation_err(OP_GPU_PIPELINE_CREATE, Some(req.pipeline_id), why)?;
            return Ok(());
        }
        self.table.insert_pipeline(req.pipeline_id, PipelineRecord {
            shaders: req.shaders.clone(),
        });
        Ok(())
    }

    fn handle_pipeline_destroy(&mut self, m: Message) -> Result<()> {
        let req: PipelineDestroyPayload = postcard::from_bytes(&m.payload)?;
        self.table.remove_pipeline(req.pipeline_id);
        Ok(())
    }

    fn handle_fence_create(&mut self, m: Message) -> Result<()> {
        let req: FenceCreatePayload = postcard::from_bytes(&m.payload)?;
        if let Err(why) = ResourceTable::validate_namespace(req.fence_id) {
            self.send_validation_err(OP_GPU_FENCE_CREATE, Some(req.fence_id), why)?;
            return Ok(());
        }
        self.table.insert_fence(req.fence_id);
        Ok(())
    }

    fn handle_fence_destroy(&mut self, m: Message) -> Result<()> {
        let req: FenceDestroyPayload = postcard::from_bytes(&m.payload)?;
        self.table.remove_fence(req.fence_id);
        Ok(())
    }

    fn handle_submit_frame(&mut self, m: Message) -> Result<()> {
        let req: SubmitFramePayload = postcard::from_bytes(&m.payload)?;
        if req.timeline <= self.last_timeline && self.last_timeline > 0 {
            log::warn!("non-monotonic timeline: {} <= {}",
                       req.timeline, self.last_timeline);
        }
        self.last_timeline = req.timeline;
        let signal_now = self.backend.submit_frame(
            req.fence_id, req.timeline, &req.command_buf,
        );
        if signal_now {
            if self.table.signal_fence(req.fence_id, req.timeline) {
                self.send_event(OP_GPU_FENCE_SIGNALED, &FenceSignaledEvent {
                    fence_id: req.fence_id,
                    timeline: req.timeline,
                })?;
            } else {
                self.send_validation_err(
                    OP_GPU_SUBMIT_FRAME,
                    Some(req.fence_id),
                    "fence_id not created",
                )?;
            }
        }
        Ok(())
    }

    fn handle_wait_fence(&mut self, m: Message) -> Result<()> {
        let req: WaitFencePayload = postcard::from_bytes(&m.payload)?;
        let signalled = self.table.fence_signalled(req.fence_id).unwrap_or(false);
        let resp = WaitFenceResponse {
            fence_id: req.fence_id,
            signalled,
        };
        self.send_response(OP_GPU_WAIT_FENCE, &resp)
    }

    fn handle_share_surface(&mut self, m: Message) -> Result<()> {
        let req: ShareSurfacePayload = postcard::from_bytes(&m.payload)?;
        // Stub: produce a deterministic share token. Real backends
        // hand off the underlying image to frescod's surface-import path.
        let mut token = [0u8; 32];
        token[0] = 0xC0;
        token[1] = 0xDE;
        token[2..6].copy_from_slice(&req.image_id.raw().to_le_bytes());
        let resp = ShareSurfaceResponse { share_token: token };
        log::info!("share_surface image={} purpose={:?}",
                   req.image_id, req.purpose);
        self.send_response(OP_GPU_SHARE_SURFACE, &resp)
    }

    fn handle_bundle_load(&mut self, m: Message) -> Result<()> {
        let req: BundleLoadPayload = postcard::from_bytes(&m.payload)?;
        // Stub: reject all bundle loads. Real impl walks the
        // manifest CAS, materialises declared resources, returns a
        // bundle_namespace tag.
        let resp = BundleLoadResponse {
            manifest_cas_hash: req.manifest_cas_hash,
            bundle_namespace: None,
        };
        self.send_event(OP_GPU_BUNDLE_LOAD_ERR, &BundleLoadErrEvent {
            manifest_cas_hash: req.manifest_cas_hash,
            bundle_local_id: 0,
            diagnostic: "stub backend does not materialise bundles yet".into(),
        })?;
        self.send_response(OP_GPU_BUNDLE_LOAD, &resp)
    }

    fn handle_bundle_unload(&mut self, _m: Message) -> Result<()> {
        // No bundles loaded by the stub.
        Ok(())
    }

    // ─── Wire helpers ─────────────────────────────────────────────

    fn send_response<T: serde::Serialize>(&mut self, op: u16, payload: &T) -> Result<()> {
        let bytes = postcard::to_stdvec(payload)?;
        self.conn.send_message(CLASS_GPU, op, flag::IS_RESPONSE, &bytes)?;
        Ok(())
    }

    fn send_event<T: serde::Serialize>(&mut self, op: u16, payload: &T) -> Result<()> {
        let bytes = postcard::to_stdvec(payload)?;
        self.conn.send_message(CLASS_GPU, op, flag::ASYNC_EVENT, &bytes)?;
        Ok(())
    }

    fn send_validation_err(
        &mut self,
        opcode: u16,
        resource_id: Option<aqueduct_gpu::ids::ResourceId>,
        diagnostic: &str,
    ) -> Result<()> {
        self.send_event(OP_GPU_VALIDATION_ERR, &ValidationErrEvent {
            opcode,
            resource_id,
            diagnostic: diagnostic.to_string(),
        })
    }
}

/// Workaround for unused-field lint on `FenceRecord` (re-export so
/// the symbol is referenced).
#[allow(dead_code)]
pub(crate) const _PIN_FENCE_RECORD: Option<FenceRecord> = None;
