//! Per-session resource tables.
//!
//! Each connecting client gets its own [`ResourceTable`]. The tables
//! are keyed by `ResourceId`'s namespace + local-ID and isolate
//! one connection's resources from another. This is the host-side
//! enforcement counterpart of the kmod's per-fd resource isolation
//! (`aqueduct-gpu.md` §12.3 step 5).
//!
//! Phase 1.3a stores resource metadata only — what the backend
//! makes of them comes in 1.3b. Even at the stub-backend layer, the
//! table lets us validate that:
//!
//! - Resources are addressable only within their issuing connection
//! - Destroyed resources are unaddressable on subsequent ops
//! - Resource leak detection (`unused_local_ids` returns the count
//!   of allocations not paired with destruction).

use std::collections::HashMap;

use aqueduct_gpu::ids::{IdNamespace, ResourceId};

/// Tracks per-connection resources by kind. Each kind has its own
/// table — looking up a buffer_id in the image table is a typed
/// error, not silent confusion.
#[derive(Debug, Default)]
pub struct ResourceTable {
    memories:  HashMap<ResourceId, MemoryRecord>,
    images:    HashMap<ResourceId, ImageRecord>,
    buffers:   HashMap<ResourceId, BufferRecord>,
    samplers:  HashMap<ResourceId, SamplerRecord>,
    fences:    HashMap<ResourceId, FenceRecord>,
    shaders:   HashMap<ResourceId, ShaderRecord>,
    pipelines: HashMap<ResourceId, PipelineRecord>,
}

/// Memory region book-keeping. The actual SHM-fd lives in the
/// backend; this record tracks ownership + size for quota
/// enforcement and address-space accounting.
#[derive(Debug, Clone)]
pub struct MemoryRecord {
    /// Page-aligned size that was actually allocated.
    pub size: u64,
    /// Usage tag from the create payload.
    pub usage: u8,
    /// Token returned to the guest.
    pub atrium_gpu_token: [u8; 32],
}

/// Image book-keeping. Backend handles materialisation; this is the
/// host-endpoint-side identity.
#[derive(Debug, Clone)]
pub struct ImageRecord {
    /// Backing memory region ID.
    pub backing_region: ResourceId,
    /// Width × height × depth, mip levels, array layers.
    pub width:  u32,
    /// Height in pixels.
    pub height: u32,
    /// Depth in pixels (1 for 2D images).
    pub depth:  u32,
    /// Vulkan-encoded format value.
    pub format: u32,
}

/// Buffer book-keeping.
#[derive(Debug, Clone)]
pub struct BufferRecord {
    /// Backing memory region ID.
    pub backing_region: ResourceId,
    /// Buffer size in bytes.
    pub size: u64,
}

/// Sampler book-keeping.
#[derive(Debug, Clone)]
pub struct SamplerRecord {
    /// Marker; actual state lives on the backend.
    pub _placeholder: (),
}

/// Fence state — whether it's been signalled and at what timeline.
#[derive(Debug, Clone, Default)]
pub struct FenceRecord {
    /// Has the host signalled this fence?
    pub signalled: bool,
    /// Timeline of the frame that signalled it (if `signalled`).
    pub timeline: u64,
}

/// Shader book-keeping — references the bytecode hash and the
/// compiled binary (held by the backend's shader cache).
#[derive(Debug, Clone)]
pub struct ShaderRecord {
    /// SHA-256 of the source bytecode.
    pub bytecode_hash: [u8; 32],
}

/// Pipeline book-keeping.
#[derive(Debug, Clone)]
pub struct PipelineRecord {
    /// IDs of the shaders this pipeline references.
    pub shaders: Vec<ResourceId>,
}

impl ResourceTable {
    /// Construct an empty table for a new session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a memory record.
    pub fn insert_memory(&mut self, id: ResourceId, rec: MemoryRecord) {
        self.memories.insert(id, rec);
    }
    /// Remove a memory record. Returns `true` if the ID was present.
    pub fn remove_memory(&mut self, id: ResourceId) -> bool {
        self.memories.remove(&id).is_some()
    }
    /// Look up a memory record.
    pub fn get_memory(&self, id: ResourceId) -> Option<&MemoryRecord> {
        self.memories.get(&id)
    }

    /// Insert an image record.
    pub fn insert_image(&mut self, id: ResourceId, rec: ImageRecord) {
        self.images.insert(id, rec);
    }
    /// Remove an image record.
    pub fn remove_image(&mut self, id: ResourceId) -> bool {
        self.images.remove(&id).is_some()
    }
    /// Look up an image record.
    pub fn get_image(&self, id: ResourceId) -> Option<&ImageRecord> {
        self.images.get(&id)
    }

    /// Insert a buffer record.
    pub fn insert_buffer(&mut self, id: ResourceId, rec: BufferRecord) {
        self.buffers.insert(id, rec);
    }
    /// Remove a buffer record.
    pub fn remove_buffer(&mut self, id: ResourceId) -> bool {
        self.buffers.remove(&id).is_some()
    }
    /// Look up a buffer record.
    pub fn get_buffer(&self, id: ResourceId) -> Option<&BufferRecord> {
        self.buffers.get(&id)
    }

    /// Insert a sampler record.
    pub fn insert_sampler(&mut self, id: ResourceId, rec: SamplerRecord) {
        self.samplers.insert(id, rec);
    }
    /// Remove a sampler record.
    pub fn remove_sampler(&mut self, id: ResourceId) -> bool {
        self.samplers.remove(&id).is_some()
    }

    /// Insert a fence record (initially unsignalled).
    pub fn insert_fence(&mut self, id: ResourceId) {
        self.fences.insert(id, FenceRecord::default());
    }
    /// Mark a fence as signalled at the given timeline.
    pub fn signal_fence(&mut self, id: ResourceId, timeline: u64) -> bool {
        match self.fences.get_mut(&id) {
            Some(rec) => { rec.signalled = true; rec.timeline = timeline; true }
            None => false,
        }
    }
    /// Look up a fence's signal state. Missing fence → `None`.
    pub fn fence_signalled(&self, id: ResourceId) -> Option<bool> {
        self.fences.get(&id).map(|r| r.signalled)
    }
    /// Remove a fence record.
    pub fn remove_fence(&mut self, id: ResourceId) -> bool {
        self.fences.remove(&id).is_some()
    }

    /// Insert a shader record.
    pub fn insert_shader(&mut self, id: ResourceId, rec: ShaderRecord) {
        self.shaders.insert(id, rec);
    }
    /// Look up a shader record.
    pub fn get_shader(&self, id: ResourceId) -> Option<&ShaderRecord> {
        self.shaders.get(&id)
    }

    /// Insert a pipeline record.
    pub fn insert_pipeline(&mut self, id: ResourceId, rec: PipelineRecord) {
        self.pipelines.insert(id, rec);
    }
    /// Remove a pipeline record.
    pub fn remove_pipeline(&mut self, id: ResourceId) -> bool {
        self.pipelines.remove(&id).is_some()
    }
    /// Look up a pipeline record.
    pub fn get_pipeline(&self, id: ResourceId) -> Option<&PipelineRecord> {
        self.pipelines.get(&id)
    }

    /// Total live resource count (for telemetry / leak detection).
    pub fn live_count(&self) -> usize {
        self.memories.len()
            + self.images.len()
            + self.buffers.len()
            + self.samplers.len()
            + self.fences.len()
            + self.shaders.len()
            + self.pipelines.len()
    }

    /// Validate that an ID's namespace is consumable on this
    /// connection. Currently accepts `IcdRuntime` (client-allocated)
    /// and `Bundle(_)` (loaded bundles); rejects `Builtin` from
    /// untrusted clients (only Atrium-internal callers reference
    /// built-ins).
    pub fn validate_namespace(id: ResourceId) -> Result<IdNamespace, &'static str> {
        match id.namespace() {
            Some(IdNamespace::IcdRuntime) => Ok(IdNamespace::IcdRuntime),
            Some(IdNamespace::Bundle(n))  => Ok(IdNamespace::Bundle(n)),
            Some(IdNamespace::Builtin)    => Err("builtin namespace reserved for Atrium internals"),
            None => Err("invalid namespace tag"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> ResourceId {
        ResourceId::new(IdNamespace::IcdRuntime, n)
    }

    #[test]
    fn insert_remove_roundtrip() {
        let mut t = ResourceTable::new();
        let i = id(0x10);
        assert!(t.get_buffer(i).is_none());
        t.insert_buffer(i, BufferRecord { backing_region: id(0x1), size: 64 });
        assert!(t.get_buffer(i).is_some());
        assert!(t.remove_buffer(i));
        assert!(!t.remove_buffer(i), "double-remove returns false");
    }

    #[test]
    fn fence_signal_state() {
        let mut t = ResourceTable::new();
        let f = id(0x100);
        t.insert_fence(f);
        assert_eq!(t.fence_signalled(f), Some(false));
        assert!(t.signal_fence(f, 42));
        assert_eq!(t.fence_signalled(f), Some(true));
        assert!(!t.signal_fence(id(0x999), 0), "signaling unknown fence returns false");
    }

    #[test]
    fn live_count_sums_all_kinds() {
        let mut t = ResourceTable::new();
        t.insert_memory(id(1), MemoryRecord { size: 4096, usage: 1, atrium_gpu_token: [0; 32] });
        t.insert_buffer(id(2), BufferRecord { backing_region: id(1), size: 64 });
        t.insert_fence(id(3));
        assert_eq!(t.live_count(), 3);
    }

    #[test]
    fn validate_namespace_rejects_builtin() {
        let builtin = ResourceId::new(IdNamespace::Builtin, 1);
        assert!(ResourceTable::validate_namespace(builtin).is_err());
        let icd = ResourceId::new(IdNamespace::IcdRuntime, 1);
        assert!(ResourceTable::validate_namespace(icd).is_ok());
        let bundle = ResourceId::new(IdNamespace::Bundle(0x3), 1);
        assert!(ResourceTable::validate_namespace(bundle).is_ok());
    }
}
