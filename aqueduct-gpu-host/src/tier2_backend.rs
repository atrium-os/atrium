//! Tier-2 software-execution Backend.
//!
//! Implements the [`Backend`] trait by composition: an
//! internal Tier-2 shader registry plus a per-image
//! framebuffer map keyed by `ResourceId`. The interesting
//! method is [`Tier2Backend::run_fragment_shader_into`],
//! which routes a registered Tier-2 fragment shader's
//! output into a previously-created image.
//!
//! # Phase status
//!
//! **Phase 2 v5d step 2.** This is the scaffolding tier:
//! the Backend trait surface is implemented but
//! `submit_frame` is a stub. Real wire-protocol routing —
//! where a guest's draw call against a Tier-2-bound
//! pipeline kicks off `run_fragment_shader_into`
//! automatically — lands in v5e once the wire ops for
//! "bind a Tier-2 pipeline" are finalised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use aqueduct_gpu::ids::ResourceId;
use aqueduct_gpu::backends::{BackendId, GpuVendor};

use crate::backend::Backend;
use crate::tier2_registry::{Tier2ExecError, Tier2Registry, Tier2ShaderId};

/// Backend that routes draws through Tier-2 compiled
/// fragment shaders. Image storage lives in this backend
/// (one RGBA8 buffer per registered image) so calls to
/// [`Tier2Backend::run_fragment_shader_into`] can write
/// pixels without going through `image_write_pixels`.
pub struct Tier2Backend {
    registry: Arc<Tier2Registry>,
    images:   Mutex<HashMap<u64, ImageStorage>>,
    submissions: AtomicU64,
    presents:    AtomicU64,
}

/// Per-image RGBA8 storage owned by the backend.
struct ImageStorage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Tier2Backend {
    /// Construct a fresh Tier2Backend backed by the given
    /// registry. The registry can be shared across
    /// backends; image storage is per-backend.
    pub fn new(registry: Arc<Tier2Registry>) -> Self {
        Self {
            registry,
            images: Mutex::new(HashMap::new()),
            submissions: AtomicU64::new(0),
            presents:    AtomicU64::new(0),
        }
    }

    /// How many `submit_frame` calls have arrived. Useful
    /// for tests + diagnostics.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }

    /// How many `present` calls have arrived.
    pub fn present_count(&self) -> u64 {
        self.presents.load(Ordering::Relaxed)
    }

    /// Run a compiled fragment shader once per pixel of
    /// `image_id`, writing the result into the backend's
    /// image storage. The shader id and image must both
    /// have been registered (via `Tier2Registry::register`
    /// and `Backend::image_created` respectively).
    pub fn run_fragment_shader_into(
        &self,
        image_id: ResourceId,
        shader_id: Tier2ShaderId,
        push_constants: &[u8],
        uniforms: &[u8],
    ) -> Result<(), Tier2ExecError> {
        let mut images = self.images.lock().unwrap();
        let img = images.get_mut(&(image_id.raw() as u64))
            .ok_or(Tier2ExecError::UnknownShader(shader_id))?;
        self.registry.fill_image_fragment(
            shader_id,
            push_constants, uniforms,
            img.width, img.height,
            &mut img.pixels,
        )
    }

    /// Read back a registered image's RGBA8 pixels.
    /// `None` if the image isn't registered.
    pub fn read_image_pixels(&self, image_id: ResourceId) -> Option<Vec<u8>> {
        let images = self.images.lock().unwrap();
        images.get(&(image_id.raw() as u64)).map(|img| img.pixels.clone())
    }
}

impl Backend for Tier2Backend {
    fn identity(&self) -> BackendId {
        BackendId::new(GpuVendor::Software, 2)
    }
    fn caps(&self) -> u64 { 0 }
    fn max_frame_bytes(&self) -> u32 { 1 << 20 }
    fn max_fences_inflight(&self) -> u32 { 16 }

    fn allocate_memory(&self, _size: u64, _usage: u8) -> [u8; 32] {
        let n = self.submissions.load(Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xC2;
        tok
    }

    fn image_created(&self, image_id: ResourceId, width: u32, height: u32) {
        if width == 0 || height == 0 { return; }
        const MAX_DIM: u32 = 16 * 1024;
        if width > MAX_DIM || height > MAX_DIM { return; }
        let pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        self.images.lock().unwrap().insert(image_id.raw() as u64, ImageStorage {
            width, height, pixels,
        });
    }

    fn image_destroyed(&self, image_id: ResourceId) {
        self.images.lock().unwrap().remove(&(image_id.raw() as u64));
    }

    fn submit_frame(
        &self,
        _fence_id: ResourceId,
        _timeline: u64,
        _frame_buf: &[u8],
    ) -> bool {
        // v5d step 2: submit_frame is a stub. Real wire-
        // protocol routing (draw stream → pipeline lookup →
        // run_fragment_shader_into) lands in v5e.
        self.submissions.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn present(
        &self,
        _image_id: ResourceId,
        _surface_id: u64,
        _frame_id: u64,
    ) {
        self.presents.fetch_add(1, Ordering::Relaxed);
    }
}
