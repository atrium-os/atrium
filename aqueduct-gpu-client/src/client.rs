//! `GpuClient` — guest-side wrapper around an [`aqueduct::Connection`]
//! for the `CLASS_GPU` opcode class.
//!
//! Owns the connection, performs the handshake, allocates resource
//! IDs, sends typed envelopes, and demultiplexes async events from
//! request-response replies via an internal event queue.

use std::collections::VecDeque;
use std::time::Duration;

use aqueduct::envelope::flag;
use aqueduct::{Connection, Message};

use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu::opcodes::*;
use aqueduct_gpu::payloads::*;
use aqueduct_gpu::CLASS_GPU;

use crate::error::{GpuClientError, GpuClientResult};
use crate::ids::IdAllocator;

/// The current aqueduct-gpu protocol version this client speaks.
/// Bumped in lockstep with `docs/spec/aqueduct-gpu.md`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Async event surfaced via [`GpuClient::recv_event`]. These are
/// server-initiated messages that arrive out of band of any specific
/// request-response cycle.
#[derive(Debug, Clone)]
pub enum GpuEvent {
    /// A previously-created fence has been signalled.
    FenceSignaled(FenceSignaledEvent),
    /// All resources on this connection are invalid; reconnect.
    DeviceLost(DeviceLostEvent),
    /// A fire-and-forget op failed validation.
    ValidationErr(ValidationErrEvent),
    /// Per-resource validation failure during bundle load.
    BundleLoadErr(BundleLoadErrEvent),
}

/// Guest-side aqueduct-gpu client.
///
/// Owns the underlying [`aqueduct::Connection`]; demultiplexes
/// async events into [`GpuClient::recv_event`] while letting
/// request-response calls (`handshake`, `allocate_memory`, etc.)
/// block transparently for their matching reply.
pub struct GpuClient {
    conn: Connection,
    ids: IdAllocator,
    handshake: Option<HandshakeResponse>,
    /// Events that arrived while we were waiting for a specific
    /// reply. Surfaced FIFO via [`recv_event`](GpuClient::recv_event).
    pending_events: VecDeque<GpuEvent>,
}

impl GpuClient {
    /// Wrap an already-connected [`aqueduct::Connection`]. Caller
    /// is responsible for connecting; this lets the client integrate
    /// with Portcullis-mediated socket handoff (fd-passed-in
    /// connection from the cap-mediator).
    ///
    /// You must call [`handshake`](Self::handshake) before any other
    /// method — most ops depend on handshake-negotiated capabilities
    /// and the `max_frame_bytes` cap for frame builders.
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            ids: IdAllocator::default(),
            handshake: None,
            pending_events: VecDeque::new(),
        }
    }

    /// Underlying connection (for advanced uses like kqueue
    /// registration via `AsRawFd`, or sharing the connection with
    /// other opcode classes).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Cached handshake response. `None` until
    /// [`handshake`](Self::handshake) succeeds.
    pub fn handshake_response(&self) -> Option<&HandshakeResponse> {
        self.handshake.as_ref()
    }

    /// Construct a [`FrameBuilder`](aqueduct_gpu::FrameBuilder) sized
    /// to the negotiated `max_frame_bytes`. Panics if called before
    /// handshake — the cap is unknown otherwise.
    pub fn frame_builder(&self) -> aqueduct_gpu::FrameBuilder {
        let cap = self
            .handshake
            .as_ref()
            .expect("call handshake() before frame_builder()")
            .max_frame_bytes;
        aqueduct_gpu::FrameBuilder::new(cap)
    }

    // ─── Handshake ─────────────────────────────────────────────────

    /// Perform the initial `OP_GPU_HANDSHAKE` exchange. Caches the
    /// response; subsequent ops use the negotiated caps.
    pub fn handshake(&mut self, kind: ClientKind) -> GpuClientResult<&HandshakeResponse> {
        let req = HandshakePayload {
            protocol_version: PROTOCOL_VERSION,
            client_kind: kind,
        };
        self.send(OP_GPU_HANDSHAKE, flag::RESPONSE_EXPECTED, &req)?;
        let resp: HandshakeResponse = self.recv_response(OP_GPU_HANDSHAKE)?;

        if resp.protocol_version != PROTOCOL_VERSION {
            return Err(GpuClientError::ProtocolMismatch {
                client: PROTOCOL_VERSION,
                host: resp.protocol_version,
            });
        }

        self.handshake = Some(resp);
        Ok(self.handshake.as_ref().unwrap())
    }

    // ─── Memory ────────────────────────────────────────────────────

    /// Allocate a shared-memory region on the host. Returns the
    /// host's response carrying the import token the guest kmod
    /// uses to expose the region to userspace.
    pub fn allocate_memory(
        &mut self,
        size: u64,
        usage: MemoryUsage,
    ) -> GpuClientResult<MemoryCreateResponse> {
        let region_id = self.alloc_id()?;
        let req = MemoryCreatePayload { region_id, size, usage };
        self.send(OP_GPU_MEMORY_CREATE, flag::RESPONSE_EXPECTED, &req)?;
        self.recv_response(OP_GPU_MEMORY_CREATE)
    }

    /// Destroy a memory region. Fire-and-forget; failures surface
    /// via [`recv_event`](Self::recv_event) as `ValidationErr`.
    pub fn destroy_memory(&mut self, region_id: ResourceId) -> GpuClientResult<()> {
        let req = MemoryDestroyPayload { region_id };
        self.send(OP_GPU_MEMORY_DESTROY, 0, &req)
    }

    // ─── Images / buffers / samplers (all fire-and-forget) ────────

    /// Create an image. ID is pre-assigned. Returns it.
    pub fn create_image(&mut self, mut params: ImageCreatePayload) -> GpuClientResult<ResourceId> {
        params.image_id = self.alloc_id()?;
        self.send(OP_GPU_IMAGE_CREATE, 0, &params)?;
        Ok(params.image_id)
    }

    /// Destroy an image.
    pub fn destroy_image(&mut self, image_id: ResourceId) -> GpuClientResult<()> {
        let req = ImageDestroyPayload { image_id };
        self.send(OP_GPU_IMAGE_DESTROY, 0, &req)
    }

    /// Inline upload of pixel data to an image. Fire-and-forget; the
    /// host validates `pixels.len() == row_pitch * height` and surfaces
    /// failures via [`recv_event`](Self::recv_event) as `ValidationErr`.
    ///
    /// This is the tier-1 / debug-path upload route — small textures
    /// and glyph-atlas regions. Large textures should flow through
    /// buffer-staged copies (Phase 1.5+).
    pub fn write_image(
        &mut self,
        image_id: ResourceId,
        row_pitch: u32,
        pixels: Vec<u8>,
    ) -> GpuClientResult<()> {
        let req = ImageWritePayload { image_id, row_pitch, pixels };
        self.send(OP_GPU_IMAGE_WRITE, 0, &req)
    }

    /// Inline sub-region pixel write. Fire-and-forget; failures
    /// surface as `ValidationErr` events. The image must already
    /// exist (prior `create_image` + initial `write_image`). The
    /// pixels outside the declared `[dst_x..dst_x+width,
    /// dst_y..dst_y+height]` rect are untouched.
    ///
    /// Used by atrium-text's server-side glyph rasteriser to ship
    /// only newly-cached glyphs each frame instead of re-uploading
    /// the whole atlas.
    pub fn write_image_region(
        &mut self,
        image_id: ResourceId,
        dst_x: u32, dst_y: u32,
        width: u32, height: u32,
        row_pitch: u32,
        pixels: Vec<u8>,
    ) -> GpuClientResult<()> {
        let req = ImageWriteRegionPayload {
            image_id, dst_x, dst_y, width, height, row_pitch, pixels,
        };
        self.send(OP_GPU_IMAGE_WRITE_REGION, 0, &req)
    }

    /// Create a buffer. ID is pre-assigned. Returns it.
    pub fn create_buffer(&mut self, mut params: BufferCreatePayload) -> GpuClientResult<ResourceId> {
        params.buffer_id = self.alloc_id()?;
        self.send(OP_GPU_BUFFER_CREATE, 0, &params)?;
        Ok(params.buffer_id)
    }

    /// Destroy a buffer.
    pub fn destroy_buffer(&mut self, buffer_id: ResourceId) -> GpuClientResult<()> {
        let req = BufferDestroyPayload { buffer_id };
        self.send(OP_GPU_BUFFER_DESTROY, 0, &req)
    }

    /// Create a sampler. ID is pre-assigned. Returns it.
    pub fn create_sampler(&mut self, mut params: SamplerCreatePayload) -> GpuClientResult<ResourceId> {
        params.sampler_id = self.alloc_id()?;
        self.send(OP_GPU_SAMPLER_CREATE, 0, &params)?;
        Ok(params.sampler_id)
    }

    /// Destroy a sampler.
    pub fn destroy_sampler(&mut self, sampler_id: ResourceId) -> GpuClientResult<()> {
        let req = SamplerDestroyPayload { sampler_id };
        self.send(OP_GPU_SAMPLER_DESTROY, 0, &req)
    }

    /// Create a fence. ID is pre-assigned. Returns it.
    pub fn create_fence(&mut self) -> GpuClientResult<ResourceId> {
        let fence_id = self.alloc_id()?;
        let req = FenceCreatePayload { fence_id };
        self.send(OP_GPU_FENCE_CREATE, 0, &req)?;
        Ok(fence_id)
    }

    /// Destroy a fence.
    pub fn destroy_fence(&mut self, fence_id: ResourceId) -> GpuClientResult<()> {
        let req = FenceDestroyPayload { fence_id };
        self.send(OP_GPU_FENCE_DESTROY, 0, &req)
    }

    // ─── Shaders ───────────────────────────────────────────────────

    /// Resolve a shader by content hash + target backend. Returns
    /// the resolved `shader_id` on cache hit; returns
    /// [`GpuClientError::ShaderResolveMissed`] on miss so the caller
    /// can decide whether to follow up with [`upload_shader`].
    ///
    /// [`upload_shader`]: Self::upload_shader
    pub fn resolve_shader(
        &mut self,
        bytecode_hash: [u8; 32],
        kind: ShaderKind,
        backend: aqueduct_gpu::BackendId,
    ) -> GpuClientResult<ResourceId> {
        let req = ShaderResolvePayload { bytecode_hash, kind, backend };
        self.send(OP_GPU_SHADER_RESOLVE, flag::RESPONSE_EXPECTED, &req)?;
        let resp: ShaderResolveResponse = self.recv_response(OP_GPU_SHADER_RESOLVE)?;

        match resp.status {
            ShaderResolveStatus::Hit => resp
                .shader_id
                .ok_or_else(|| GpuClientError::Validation {
                    opcode: OP_GPU_SHADER_RESOLVE,
                    resource_id: None,
                    diagnostic: "host returned Hit but no shader_id".into(),
                }),
            ShaderResolveStatus::Miss => Err(GpuClientError::shader_miss(bytecode_hash, resp.status)),
        }
    }

    /// Upload bytecode for a previously-missed shader. Cold path —
    /// in steady state shipped apps should always hit the resolve
    /// cache (warmed by atrium-pkg's install hook).
    pub fn upload_shader(
        &mut self,
        bytecode_hash: [u8; 32],
        kind: ShaderKind,
        backend: aqueduct_gpu::BackendId,
        bytecode: Vec<u8>,
    ) -> GpuClientResult<ResourceId> {
        let req = ShaderUploadPayload { bytecode_hash, kind, backend, bytecode };
        self.send(OP_GPU_SHADER_UPLOAD, flag::RESPONSE_EXPECTED, &req)?;
        let resp: ShaderUploadResponse = self.recv_response(OP_GPU_SHADER_UPLOAD)?;
        resp.shader_id.ok_or(GpuClientError::ShaderUploadRejected {
            diagnostic: resp.diagnostic,
        })
    }

    // ─── Pipelines ─────────────────────────────────────────────────

    /// Create a pipeline. ID is pre-assigned. Returns it.
    pub fn create_pipeline(
        &mut self,
        kind: PipelineKind,
        shaders: Vec<ResourceId>,
        state_blob: Vec<u8>,
    ) -> GpuClientResult<ResourceId> {
        let pipeline_id = self.alloc_id()?;
        let req = PipelineCreatePayload { pipeline_id, kind, shaders, state_blob };
        self.send(OP_GPU_PIPELINE_CREATE, 0, &req)?;
        Ok(pipeline_id)
    }

    /// Destroy a pipeline.
    pub fn destroy_pipeline(&mut self, pipeline_id: ResourceId) -> GpuClientResult<()> {
        let req = PipelineDestroyPayload { pipeline_id };
        self.send(OP_GPU_PIPELINE_DESTROY, 0, &req)
    }

    // ─── Frame submit + fence wait ────────────────────────────────

    /// Submit a complete frame command stream. Consumes the
    /// [`FrameBuilder`](aqueduct_gpu::FrameBuilder); the caller can
    /// build a fresh one (via [`frame_builder`](Self::frame_builder))
    /// for the next frame.
    ///
    /// Fire-and-forget; completion is signalled via the fence.
    pub fn submit_frame(
        &mut self,
        fence_id: ResourceId,
        builder: aqueduct_gpu::FrameBuilder,
        timeline: u64,
    ) -> GpuClientResult<()> {
        let req = SubmitFramePayload {
            fence_id,
            timeline,
            command_buf: builder.into_buf(),
        };
        self.send(OP_GPU_SUBMIT_FRAME, 0, &req)
    }

    /// Wait for a fence to signal. Returns `Ok(true)` if signalled,
    /// `Ok(false)` if the timeout expired. `timeout_ns == u64::MAX`
    /// blocks indefinitely.
    pub fn wait_fence(&mut self, fence_id: ResourceId, timeout_ns: u64) -> GpuClientResult<bool> {
        let req = WaitFencePayload { fence_id, timeout_ns };
        self.send(OP_GPU_WAIT_FENCE, flag::RESPONSE_EXPECTED, &req)?;
        let resp: WaitFenceResponse = self.recv_response(OP_GPU_WAIT_FENCE)?;
        Ok(resp.signalled)
    }

    // ─── Surface share ─────────────────────────────────────────────

    /// Hand frescod a reference to an image. Returns the share
    /// token the caller passes via fresco-protocol's
    /// `slot_set_texture`.
    pub fn share_surface(
        &mut self,
        image_id: ResourceId,
        purpose: impl Into<String>,
    ) -> GpuClientResult<[u8; 32]> {
        let req = ShareSurfacePayload { image_id, purpose: purpose.into() };
        self.send(OP_GPU_SHARE_SURFACE, flag::RESPONSE_EXPECTED, &req)?;
        let resp: ShareSurfaceResponse = self.recv_response(OP_GPU_SHARE_SURFACE)?;
        Ok(resp.share_token)
    }

    /// Present a rendered swapchain image to a Fresco surface.
    /// Fire-and-forget; the host endpoint routes the image's
    /// pixels through to its per-window WindowSurface.
    pub fn present(
        &mut self,
        image_id:   ResourceId,
        surface_id: u64,
        frame_id:   u64,
    ) -> GpuClientResult<()> {
        let req = PresentPayload { image_id, surface_id, frame_id };
        self.send(OP_GPU_PRESENT, 0, &req)
    }

    // ─── Bundle lifecycle ──────────────────────────────────────────

    /// Load a third-party scene-graph bundle. Returns the
    /// `bundle_namespace` tag the host assigned for the bundle's
    /// resources, or [`GpuClientError::BundleLoadFailed`] if any
    /// declared resource failed validation.
    pub fn bundle_load(
        &mut self,
        manifest_cas_hash: [u8; 32],
        display_name: impl Into<String>,
    ) -> GpuClientResult<u8> {
        let req = BundleLoadPayload {
            manifest_cas_hash,
            display_name: display_name.into(),
        };
        self.send(OP_GPU_BUNDLE_LOAD, flag::RESPONSE_EXPECTED, &req)?;

        // Bundle load can interleave per-resource error events with
        // the eventual response; collect both until response arrives.
        let mut load_errs: Vec<String> = Vec::new();
        let resp: BundleLoadResponse = loop {
            let m = self.recv_class_message()?;
            match m.op {
                OP_GPU_BUNDLE_LOAD_ERR => {
                    let ev: BundleLoadErrEvent = postcard::from_bytes(&m.payload)?;
                    load_errs.push(format!(
                        "bundle local-id {:#x}: {}",
                        ev.bundle_local_id, ev.diagnostic
                    ));
                }
                OP_GPU_BUNDLE_LOAD if m.flags & flag::IS_RESPONSE != 0 => {
                    break postcard::from_bytes(&m.payload)?;
                }
                _ => self.maybe_queue_event(m)?,
            }
        };

        match resp.bundle_namespace {
            Some(ns) => Ok(ns),
            None => Err(GpuClientError::BundleLoadFailed {
                manifest_cas_hash,
                messages: load_errs,
            }),
        }
    }

    /// Unload a previously-loaded bundle.
    pub fn bundle_unload(&mut self, bundle_namespace: u8) -> GpuClientResult<()> {
        let req = BundleUnloadPayload { bundle_namespace };
        self.send(OP_GPU_BUNDLE_UNLOAD, 0, &req)
    }

    // ─── Event reception ───────────────────────────────────────────

    /// Pull the next async event. With `timeout = None`, blocks
    /// indefinitely; with `Some(Duration)`, waits at most that long.
    /// Returns `Ok(None)` only when a timeout expires.
    ///
    /// Events that arrived during prior request-response calls are
    /// surfaced from an internal queue before reading from the wire.
    pub fn recv_event(&mut self, timeout: Option<Duration>) -> GpuClientResult<Option<GpuEvent>> {
        if let Some(ev) = self.pending_events.pop_front() {
            return Ok(Some(ev));
        }
        let m = match timeout {
            None => self.recv_class_message()?,
            Some(t) => match self.conn.recv_message_or_timeout(t)? {
                None => return Ok(None),
                Some(m) if m.opcode_class == CLASS_GPU => m,
                Some(m) => return Err(GpuClientError::UnexpectedClass(m.opcode_class)),
            },
        };
        Self::message_to_event(m).map(Some)
    }

    // ─── Internals ─────────────────────────────────────────────────

    fn alloc_id(&mut self) -> GpuClientResult<ResourceId> {
        self.ids.next().ok_or(GpuClientError::IdNamespaceExhausted)
    }

    fn send<T: serde::Serialize>(&mut self, op: u16, flags: u16, payload: &T) -> GpuClientResult<()> {
        let bytes = postcard::to_stdvec(payload)?;
        self.conn.send_message(CLASS_GPU, op, flags, &bytes)?;
        Ok(())
    }

    /// Receive the next CLASS_GPU message. Queues any async events
    /// (FENCE_SIGNALED, DEVICE_LOST, VALIDATION_ERR, BUNDLE_LOAD_ERR)
    /// into `pending_events` rather than surfacing them to the
    /// request-response caller. CLASS_GPU is the only acceptable class;
    /// other classes yield `UnexpectedClass`.
    fn recv_class_message(&mut self) -> GpuClientResult<Message> {
        loop {
            let m = self.conn.recv_message()?;
            if m.opcode_class != CLASS_GPU {
                return Err(GpuClientError::UnexpectedClass(m.opcode_class));
            }
            if Self::is_async_event(m.op) {
                let ev = Self::message_to_event(m)?;
                self.pending_events.push_back(ev);
                continue;
            }
            return Ok(m);
        }
    }

    /// Receive the response for a specific opcode. Async events that
    /// interleave are queued; unexpected ops surface as
    /// `UnexpectedOp`.
    fn recv_response<R: serde::de::DeserializeOwned>(&mut self, expected: u16) -> GpuClientResult<R> {
        let m = self.recv_class_message()?;
        if m.op != expected || m.flags & flag::IS_RESPONSE == 0 {
            return Err(GpuClientError::UnexpectedOp { actual: m.op, expected });
        }
        Ok(postcard::from_bytes(&m.payload)?)
    }

    fn is_async_event(op: u16) -> bool {
        matches!(
            op,
            OP_GPU_FENCE_SIGNALED
                | OP_GPU_DEVICE_LOST
                | OP_GPU_VALIDATION_ERR
                | OP_GPU_BUNDLE_LOAD_ERR
        )
    }

    fn message_to_event(m: Message) -> GpuClientResult<GpuEvent> {
        Ok(match m.op {
            OP_GPU_FENCE_SIGNALED => GpuEvent::FenceSignaled(postcard::from_bytes(&m.payload)?),
            OP_GPU_DEVICE_LOST    => GpuEvent::DeviceLost(postcard::from_bytes(&m.payload)?),
            OP_GPU_VALIDATION_ERR => GpuEvent::ValidationErr(postcard::from_bytes(&m.payload)?),
            OP_GPU_BUNDLE_LOAD_ERR => GpuEvent::BundleLoadErr(postcard::from_bytes(&m.payload)?),
            _ => return Err(GpuClientError::UnexpectedOp { actual: m.op, expected: 0 }),
        })
    }

    fn maybe_queue_event(&mut self, m: Message) -> GpuClientResult<()> {
        if Self::is_async_event(m.op) {
            let ev = Self::message_to_event(m)?;
            self.pending_events.push_back(ev);
            Ok(())
        } else {
            Err(GpuClientError::UnexpectedOp { actual: m.op, expected: 0 })
        }
    }
}

// `IdNamespace` is referenced in doc-comments above; bring it into
// scope so rustdoc resolves it.
#[allow(unused_imports)]
use IdNamespace as _;

#[cfg(test)]
mod tests {
    use super::*;

    // The client's wire-talking paths can't be tested without a
    // server peer; that's covered by host-endpoint integration tests
    // (Phase 1.3). What CAN be tested here is the pure-state
    // helpers: ID allocation logic via the client's interface,
    // event-vs-response classification, etc.

    #[test]
    fn async_event_classification() {
        assert!(GpuClient::is_async_event(OP_GPU_FENCE_SIGNALED));
        assert!(GpuClient::is_async_event(OP_GPU_DEVICE_LOST));
        assert!(GpuClient::is_async_event(OP_GPU_VALIDATION_ERR));
        assert!(GpuClient::is_async_event(OP_GPU_BUNDLE_LOAD_ERR));

        // Request-response ops are NOT async events.
        assert!(!GpuClient::is_async_event(OP_GPU_HANDSHAKE));
        assert!(!GpuClient::is_async_event(OP_GPU_MEMORY_CREATE));
        assert!(!GpuClient::is_async_event(OP_GPU_WAIT_FENCE));
        assert!(!GpuClient::is_async_event(OP_GPU_SUBMIT_FRAME));
    }

    #[test]
    fn protocol_version_constant_is_one() {
        // Locks the version; bump in lockstep with the spec.
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
