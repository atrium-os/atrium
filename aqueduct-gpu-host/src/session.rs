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
use crate::shader_cache::{CacheKey, LookupResult, ShaderCache};
use crate::tier2_registry::Tier2Registry;

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
    /// Optional warm-path shader cache. When `None`, every resolve
    /// misses and uploads aren't persisted; when `Some`, the cache
    /// is the source of truth.
    shader_cache: Option<Arc<ShaderCache>>,
    /// Compiler-version tag for cache keys. Currently 0; bump in
    /// lockstep with backend-shader-translator changes.
    compiler_version: u32,
    /// Optional Tier-2 software-shader registry. When attached,
    /// every successful SPIR-V upload also gets compiled through
    /// atrium-spv-loader so subsequent pipeline + draw ops can
    /// invoke the resulting `atrium_fs_main` / `atrium_vs_main`
    /// function pointer. Tier-2 failures are non-fatal at
    /// upload time — the protocol-level upload still succeeds and
    /// the ShaderRecord just records `tier2_id = None`.
    tier2_registry: Option<Arc<Tier2Registry>>,
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
            shader_cache: None,
            compiler_version: 0,
            tier2_registry: None,
        }
    }

    /// Attach a process-wide shader cache. Called by the listener
    /// before [`run`](Self::run) when the daemon was configured
    /// with one.
    pub fn set_shader_cache(&mut self, cache: Arc<ShaderCache>) {
        self.shader_cache = Some(cache);
    }

    /// Attach a process-wide Tier-2 software-shader registry.
    /// Every successful SPIR-V upload thereafter will also be
    /// compiled through atrium-spv-loader; the registry id lands
    /// on the [`ShaderRecord`] for downstream lookup.
    pub fn set_tier2_registry(&mut self, registry: Arc<Tier2Registry>) {
        self.tier2_registry = Some(registry);
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
            OP_GPU_IMAGE_WRITE        => self.handle_image_write(m),
            OP_GPU_IMAGE_WRITE_REGION => self.handle_image_write_region(m),
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
            OP_GPU_PRESENT            => self.handle_present(m),
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
        // Hand to the backend so it can allocate its per-image
        // state (SW backend: tiny_skia::Pixmap; GPU backend:
        // typically no-op, allocation happens via vkBindImageMemory
        // equivalents).
        self.backend.image_created(req.image_id, req.width, req.height);
        Ok(())
    }

    fn handle_image_destroy(&mut self, m: Message) -> Result<()> {
        let req: ImageDestroyPayload = postcard::from_bytes(&m.payload)?;
        self.table.remove_image(req.image_id);
        self.backend.image_destroyed(req.image_id);
        Ok(())
    }

    fn handle_image_write(&mut self, m: Message) -> Result<()> {
        let req: ImageWritePayload = postcard::from_bytes(&m.payload)?;
        if self.table.get_image(req.image_id).is_none() {
            self.send_validation_err(
                OP_GPU_IMAGE_WRITE, Some(req.image_id),
                "image not created on this session",
            )?;
            return Ok(());
        }
        if let Err(diag) = self.backend.image_write_pixels(
            req.image_id, req.row_pitch, &req.pixels,
        ) {
            self.send_validation_err(OP_GPU_IMAGE_WRITE, Some(req.image_id), &diag)?;
        }
        Ok(())
    }

    fn handle_image_write_region(&mut self, m: Message) -> Result<()> {
        let req: ImageWriteRegionPayload = postcard::from_bytes(&m.payload)?;
        if self.table.get_image(req.image_id).is_none() {
            self.send_validation_err(
                OP_GPU_IMAGE_WRITE_REGION, Some(req.image_id),
                "image not created on this session",
            )?;
            return Ok(());
        }
        if let Err(diag) = self.backend.image_write_region_pixels(
            req.image_id,
            req.dst_x, req.dst_y, req.width, req.height,
            req.row_pitch, &req.pixels,
        ) {
            self.send_validation_err(
                OP_GPU_IMAGE_WRITE_REGION, Some(req.image_id), &diag,
            )?;
        }
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

        let (status, shader_id) = match &self.shader_cache {
            None => (ShaderResolveStatus::Miss, None),
            Some(cache) => {
                let key = CacheKey {
                    bytecode_hash: req.bytecode_hash,
                    backend: req.backend,
                    compiler_version: self.compiler_version,
                    kind: req.kind,
                };
                match cache.lookup(&key) {
                    LookupResult::Hit(_) => {
                        // Bytes are cached; materialise a resource-id
                        // so the client can reference the shader.
                        // ID derivation: lower 24 bits of the hash.
                        let local = ((req.bytecode_hash[0] as u32) << 16)
                            | ((req.bytecode_hash[1] as u32) << 8)
                            |  (req.bytecode_hash[2] as u32);
                        let id = aqueduct_gpu::ids::ResourceId::new(
                            aqueduct_gpu::ids::IdNamespace::IcdRuntime,
                            local | 0x1,
                        );
                        self.table.insert_shader(id, ShaderRecord {
                            bytecode_hash: req.bytecode_hash,
                            // Cache-hit path doesn't see the
                            // raw SPIR-V bytes; Tier-2 compile
                            // requires UPLOAD to seed it.
                            // Future: extend the cache to
                            // record the Tier2 .so path too,
                            // so a RESOLVE hit can fast-path
                            // into dlopen without recompile.
                            tier2_id: None,
                        });
                        (ShaderResolveStatus::Hit, Some(id))
                    }
                    LookupResult::Miss => (ShaderResolveStatus::Miss, None),
                    LookupResult::Error(e) => {
                        log::warn!("shader cache lookup error: {e}");
                        (ShaderResolveStatus::Miss, None)
                    }
                }
            }
        };

        let resp = ShaderResolveResponse {
            bytecode_hash: req.bytecode_hash,
            status,
            shader_id,
        };
        self.send_response(OP_GPU_SHADER_RESOLVE, &resp)
    }

    fn handle_shader_upload(&mut self, m: Message) -> Result<()> {
        let req: ShaderUploadPayload = postcard::from_bytes(&m.payload)?;

        // Phase 2.0 gate: structural + policy validation. Applies to
        // SPIR-V only; NIR bypasses (atrium-mesa emits trusted NIR).
        if matches!(req.kind, ShaderKind::SpirV) {
            if let Err(e) = crate::shader_validator::validate_spirv(&req.bytecode) {
                let diag = format!("shader validator rejected upload: {e}");
                log::warn!("{diag}");
                let resp = ShaderUploadResponse {
                    bytecode_hash: req.bytecode_hash,
                    shader_id: None,
                    diagnostic: diag,
                };
                return self.send_response(OP_GPU_SHADER_UPLOAD, &resp);
            }
        }

        // Stub backend: accept everything that passed validation.
        // Real backends additionally run spirv-val (Phase 2.4) +
        // backend codegen. After validation, persist into the
        // shader cache so a future RESOLVE with the same key Hits.
        if let Some(cache) = &self.shader_cache {
            let key = CacheKey {
                bytecode_hash: req.bytecode_hash,
                backend: req.backend,
                compiler_version: self.compiler_version,
                kind: req.kind,
            };
            if let Err(e) = cache.insert(&key, &req.bytecode) {
                // Cache write failure is non-fatal — the upload still
                // succeeded for this session; only the warm-path
                // optimisation degrades.
                log::warn!("shader cache insert failed (warm path disabled for {:?}): {e}",
                           req.bytecode_hash);
            }
        }

        // Tier-2 compile path. If a Tier2Registry is attached and
        // this is SPIR-V, kick off (or hit) the atrium-spv-compile
        // pipeline. Failures are non-fatal: log + record
        // tier2_id=None. This means a client can mix Tier-2-
        // capable shaders with shaders we can't AOT compile yet
        // (vec3 perf paths, uncommon opcodes, etc.) without
        // breaking the upload contract.
        let tier2_id = if matches!(req.kind, ShaderKind::SpirV) {
            match &self.tier2_registry {
                Some(reg) => match reg.register(&req.bytecode) {
                    Ok(id) => Some(id),
                    Err(e) => {
                        log::warn!("Tier-2 compile failed (shader still accepted): {e}");
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };

        let shader_id = aqueduct_gpu::ids::ResourceId::new(
            aqueduct_gpu::ids::IdNamespace::IcdRuntime,
            (req.bytecode_hash[0] as u32) << 16 | (req.bytecode_hash[1] as u32) << 8 | 1,
        );
        self.table.insert_shader(shader_id, ShaderRecord {
            bytecode_hash: req.bytecode_hash,
            tier2_id,
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

    /// `OP_GPU_PRESENT` — forward to the backend's `present` hook.
    /// Backends route to their actual surface→window map (today
    /// just bumps a counter; frescod-aqueduct's installed
    /// backend can override to drive its per-window WindowSurface).
    fn handle_present(&mut self, m: Message) -> Result<()> {
        let req: PresentPayload = postcard::from_bytes(&m.payload)?;
        log::debug!("present image={} surface={} frame={}",
                   req.image_id, req.surface_id, req.frame_id);
        self.backend.present(req.image_id, req.surface_id, req.frame_id);
        Ok(())
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
