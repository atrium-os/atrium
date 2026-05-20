//! Tier-2 shader registry.
//!
//! Bridges the wire protocol's per-session shader-upload
//! flow to atrium-spv-loader's AOT compile + dlopen
//! pipeline. When the eventual `Tier2Backend` lands
//! (Phase 2 v5d+), each session will own one of these
//! and call [`Tier2Registry::register`] inside its
//! `handle_shader_upload` path; the resulting
//! `Tier2ShaderId` becomes the daemon-side resource id
//! that pipeline + draw ops look up to find the actual
//! compiled `atrium_fs_main` / `atrium_vs_main` function
//! pointers.
//!
//! This commit lands the registry as a standalone
//! primitive that exercises the full atrium-spv pipeline
//! end-to-end inside the aqueduct-gpu-host crate
//! (frontend → backend → loader → dlopen → call). The
//! plumbing into `Session::handle_shader_upload` lands
//! in a follow-up step once the wire ops for "give me
//! a Tier-2-compiled shader" are finalised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atrium_spv_loader::{LoadError, LoaderConfig, LoadedShader, ShaderCache};

/// Daemon-side id for a registered Tier-2 shader.
///
/// Newtype around a `u64` so we never confuse it with
/// the wire-protocol's `ResourceId` (which is namespaced
/// to the ICD runtime). The registry's internal counter
/// is opaque to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tier2ShaderId(pub u64);

/// Registry of Tier-2-compiled shaders, keyed by content
/// hash. Internally wraps an atrium-spv-loader
/// [`ShaderCache`] (which does the SHA-256 keyed
/// .dylib/.so caching to disk).
pub struct Tier2Registry {
    cache: Arc<ShaderCache>,
    /// hash → assigned Tier2ShaderId. Lets repeat uploads
    /// of the same SPIR-V map to the same id without
    /// re-invoking the compiler.
    by_hash: Mutex<HashMap<String, Tier2ShaderId>>,
    /// Tier2ShaderId → loaded shader. Strong references
    /// keep the dlopened library alive for the registry's
    /// lifetime.
    by_id: Mutex<HashMap<Tier2ShaderId, Arc<LoadedShader>>>,
    /// Monotonic id allocator.
    next_id: Mutex<u64>,
}

impl Tier2Registry {
    /// Construct a registry backed by the given loader
    /// config. The config supplies the on-disk cache
    /// directory and the path to the `atrium-spv-compile`
    /// helper binary.
    pub fn new(config: LoaderConfig) -> Self {
        Self {
            cache: Arc::new(ShaderCache::new(config)),
            by_hash: Mutex::new(HashMap::new()),
            by_id: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Register a SPIR-V module. Compiles it via the
    /// underlying loader (cache miss → spawn the compiler
    /// subprocess), dlopens the result, and returns an
    /// opaque id usable for later lookups.
    ///
    /// Idempotent: re-registering the same bytes returns
    /// the same id without recompiling.
    pub fn register(&self, spirv: &[u8]) -> Result<Tier2ShaderId, LoadError> {
        let hash = ShaderCache::hash(spirv);
        if let Some(id) = self.by_hash.lock().unwrap().get(&hash).copied() {
            return Ok(id);
        }
        let loaded = self.cache.load_or_compile(spirv)?;
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let id = Tier2ShaderId(*n);
            *n += 1;
            id
        };
        self.by_hash.lock().unwrap().insert(hash, id);
        self.by_id.lock().unwrap().insert(id, loaded);
        Ok(id)
    }

    /// Look up a registered shader. `None` if the id has
    /// never been issued (or was forgotten).
    pub fn get(&self, id: Tier2ShaderId) -> Option<Arc<LoadedShader>> {
        self.by_id.lock().unwrap().get(&id).cloned()
    }

    /// Forget a registered shader. The dlopened library
    /// stays alive as long as any other Arc clones exist.
    /// Subsequent `register` of the same bytes will
    /// re-issue a fresh id.
    pub fn forget(&self, id: Tier2ShaderId) {
        if let Some(_loaded) = self.by_id.lock().unwrap().remove(&id) {
            // Drop the by_hash mapping too (linear search
            // because we don't index the other direction).
            let mut by_hash = self.by_hash.lock().unwrap();
            by_hash.retain(|_h, v| *v != id);
        }
    }

    /// Render a fragment shader into a flat RGBA8 image
    /// buffer.
    ///
    /// Walks every pixel of a `width × height` image,
    /// invoking `atrium_fs_main` once per pixel with the
    /// supplied push-constant + uniform buffers. The
    /// shader's output `vec4` is converted from float
    /// `[0, 1]` to `u8` (saturating cast) and written into
    /// `pixels` as RGBA bytes in row-major order.
    ///
    /// `pixels.len()` must equal `width * height * 4`.
    ///
    /// This is the simplest possible Tier-2 execution
    /// primitive: no geometry, no interpolated varyings,
    /// no depth. It corresponds exactly to drawing a full-
    /// screen quad with a constant-vertex fragment shader.
    /// Real submit_frame integration with rasterised
    /// geometry lands in a follow-up step.
    pub fn fill_image_fragment(
        &self,
        shader_id: Tier2ShaderId,
        push_constants: &[u8],
        uniforms: &[u8],
        width: u32,
        height: u32,
        pixels: &mut [u8],
    ) -> Result<(), Tier2ExecError> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected_len {
            return Err(Tier2ExecError::BadPixelsLen {
                expected: expected_len, got: pixels.len(),
            });
        }
        let loaded = self.get(shader_id)
            .ok_or(Tier2ExecError::UnknownShader(shader_id))?;
        let fs_main = loaded.entry_points.fs_main
            .ok_or(Tier2ExecError::NotAFragmentShader)?;

        for y in 0..height {
            for x in 0..width {
                let mut out_color = [0.0f32; 4];
                let mut out_depth = 0.0f32;
                // SAFETY: fs_main is a dlopened C ABI
                // function; the ShaderRecord guarantees its
                // signature matches FsMain (checked at
                // dlopen time by atrium-spv-loader). Input
                // pointers are non-null only when their
                // buffers are non-empty.
                unsafe {
                    let pc_ptr = if push_constants.is_empty() {
                        std::ptr::null()
                    } else {
                        push_constants.as_ptr()
                    };
                    let uni_ptr = if uniforms.is_empty() {
                        std::ptr::null()
                    } else {
                        uniforms.as_ptr()
                    };
                    fs_main(
                        std::ptr::null(), uni_ptr, pc_ptr,
                        x as f32 + 0.5, y as f32 + 0.5, 0.0, 1.0,
                        0,
                        out_color.as_mut_ptr(),
                        &mut out_depth,
                    );
                }
                let idx = ((y as usize) * (width as usize) + (x as usize)) * 4;
                pixels[idx    ] = f32_to_u8(out_color[0]);
                pixels[idx + 1] = f32_to_u8(out_color[1]);
                pixels[idx + 2] = f32_to_u8(out_color[2]);
                pixels[idx + 3] = f32_to_u8(out_color[3]);
            }
        }
        Ok(())
    }

    /// Rasterise a single triangle through a VS+FS pair into
    /// the supplied RGBA8 image buffer.
    ///
    /// Tier-2 rasterizer **phase R.1** — the minimum viable
    /// pipeline that drives geometric input through the
    /// compiled shaders.  Per `docs/spec/tier2-renderer.md`
    /// §8 and `RUNBOOK.md` "Next big arc — scoped: tier-2
    /// rasterizer":
    ///
    /// * Index/draw walker: 3 indices, hardcoded.
    /// * Vertex shading: call `atrium_vs_main` once per
    ///   vertex with the vertex's attribute buffer; collect
    ///   3 `gl_Position`s.
    /// * Triangle setup: NDC → screen via Vulkan-convention
    ///   viewport mapping (y not flipped); compute bbox.
    /// * Pixel loop: Pineda edge functions at each pixel
    ///   centre `(px+0.5, py+0.5)`; pixel inside iff all
    ///   three edges have the same sign (handles both
    ///   windings).  On inside, call `atrium_fs_main` with
    ///   `gl_FragCoord = (cx, cy, 0, 1)` and *no* varyings
    ///   (R.2 adds perspective-correct interpolation).
    /// * No depth, no clipping, no blend.  R.1's scope.
    ///
    /// The triangle MUST be in NDC (gl_Position.w == 1
    /// effectively); no perspective divide is done.  R.2
    /// will add it.
    ///
    /// `vertex_attrs[i]` is the per-vertex attribute buffer
    /// for vertex `i` (typically 12 bytes for a `vec3`
    /// position attribute; the layout is per the vertex
    /// shader's SPIR-V `Input` variables).  `pixels` is RGBA8
    /// row-major, length `width * height * 4`; pixels
    /// outside the triangle are left untouched, so the
    /// caller controls the background.
    pub fn fill_image_triangle(
        &self,
        vs_shader_id: Tier2ShaderId,
        fs_shader_id: Tier2ShaderId,
        vertex_attrs: [&[u8]; 3],
        uniforms: &[u8],
        push_constants: &[u8],
        width: u32,
        height: u32,
        pixels: &mut [u8],
    ) -> Result<(), Tier2ExecError> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected_len {
            return Err(Tier2ExecError::BadPixelsLen {
                expected: expected_len,
                got: pixels.len(),
            });
        }
        let vs_loaded = self.get(vs_shader_id)
            .ok_or(Tier2ExecError::UnknownShader(vs_shader_id))?;
        let vs_main = vs_loaded.entry_points.vs_main
            .ok_or(Tier2ExecError::NotAVertexShader)?;
        let fs_loaded = self.get(fs_shader_id)
            .ok_or(Tier2ExecError::UnknownShader(fs_shader_id))?;
        let fs_main = fs_loaded.entry_points.fs_main
            .ok_or(Tier2ExecError::NotAFragmentShader)?;

        let uni_ptr = if uniforms.is_empty() {
            std::ptr::null()
        } else { uniforms.as_ptr() };
        let pc_ptr = if push_constants.is_empty() {
            std::ptr::null()
        } else { push_constants.as_ptr() };

        // ── Step 1: vertex shading.  3 invocations.
        // out_varyings / out_clip_distance are R.2+ work; the
        // shader writes into the scratch buffer but R.1 ignores
        // its content (the pixel-loop FS call passes null
        // varyings).
        let mut clip_positions: [[f32; 4]; 3] = [[0.0; 4]; 3];
        let mut vary_scratch = [0u8; 256];
        let mut clip_dist = [0.0f32; 8];
        for i in 0..3 {
            let attr_ptr = if vertex_attrs[i].is_empty() {
                std::ptr::null()
            } else { vertex_attrs[i].as_ptr() };
            // SAFETY: vs_main is a dlopened C-ABI function;
            // the ShaderRecord guarantees its signature
            // matches VsMain (atrium-spv-loader checked it at
            // open time).  All pointers are valid for at least
            // the lifetime of this call.
            unsafe {
                vs_main(
                    attr_ptr,
                    std::ptr::null(),       // in_attr_strides — unused
                    uni_ptr,
                    pc_ptr,
                    i as u32, 0,
                    &mut clip_positions[i] as *mut [f32; 4],
                    vary_scratch.as_mut_ptr(),
                    clip_dist.as_mut_ptr(),
                );
            }
        }

        // ── Step 2: viewport mapping (NDC → screen).
        // R.1 assumes positions are already in NDC (effectively
        // w == 1); R.2 will divide x/y/z by w first.
        // Vulkan convention: y is NOT flipped, so screen y=0
        // corresponds to NDC y=-1.
        let fw = width as f32;
        let fh = height as f32;
        let mut screen: [(f32, f32); 3] = [(0.0, 0.0); 3];
        for i in 0..3 {
            screen[i] = (
                (clip_positions[i][0] + 1.0) * 0.5 * fw,
                (clip_positions[i][1] + 1.0) * 0.5 * fh,
            );
        }

        // ── Step 3: screen-space bbox, clamped to viewport.
        let min_x = screen.iter().map(|p| p.0)
            .fold(f32::INFINITY, f32::min).max(0.0).floor() as i32;
        let max_x = screen.iter().map(|p| p.0)
            .fold(f32::NEG_INFINITY, f32::max).min(fw - 1.0).ceil() as i32;
        let min_y = screen.iter().map(|p| p.1)
            .fold(f32::INFINITY, f32::min).max(0.0).floor() as i32;
        let max_y = screen.iter().map(|p| p.1)
            .fold(f32::NEG_INFINITY, f32::max).min(fh - 1.0).ceil() as i32;
        if min_x > max_x || min_y > max_y {
            return Ok(());     // triangle fully off-screen
        }

        // ── Step 4: Pineda edge functions.
        //   e(A, B, P) > 0  ⇔  P is on one specific side of
        //                      directed edge A → B.
        // Pixel is inside the triangle iff the three edge
        // values have the same sign (handles both windings —
        // we don't bother with a winding convention in R.1).
        let (a, b, c) = (screen[0], screen[1], screen[2]);
        let edge = |ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32| -> f32 {
            (bx - ax) * (py - ay) - (by - ay) * (px - ax)
        };

        // ── Step 5: pixel loop.
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let cx = px as f32 + 0.5;
                let cy = py as f32 + 0.5;
                let w0 = edge(b.0, b.1, c.0, c.1, cx, cy);
                let w1 = edge(c.0, c.1, a.0, a.1, cx, cy);
                let w2 = edge(a.0, a.1, b.0, b.1, cx, cy);
                let inside_pos = w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0;
                let inside_neg = w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0;
                if !(inside_pos || inside_neg) { continue; }

                let mut out_color = [0.0f32; 4];
                let mut out_depth = 0.0f32;
                // SAFETY: same as VS above.  R.1 passes null
                // varyings; R.2 will pass an interpolated buffer.
                unsafe {
                    fs_main(
                        std::ptr::null(),       // in_varyings
                        uni_ptr,
                        pc_ptr,
                        cx, cy, 0.0, 1.0,
                        0,
                        out_color.as_mut_ptr(),
                        &mut out_depth,
                    );
                }
                let idx = ((py as usize) * (width as usize)
                    + (px as usize)) * 4;
                pixels[idx]     = f32_to_u8(out_color[0]);
                pixels[idx + 1] = f32_to_u8(out_color[1]);
                pixels[idx + 2] = f32_to_u8(out_color[2]);
                pixels[idx + 3] = f32_to_u8(out_color[3]);
            }
        }
        Ok(())
    }
}

/// Saturating float-to-u8 conversion matching the standard
/// sRGB framebuffer convention.
fn f32_to_u8(v: f32) -> u8 {
    if v.is_nan() { 0 }
    else if v <= 0.0 { 0 }
    else if v >= 1.0 { 255 }
    else { (v * 255.0 + 0.5) as u8 }
}

/// Errors from [`Tier2Registry::fill_image_fragment`].
#[derive(Debug, thiserror::Error)]
pub enum Tier2ExecError {
    /// `pixels.len()` didn't match `width * height * 4`.
    #[error("pixels buffer length {got} doesn't match expected {expected}")]
    BadPixelsLen {
        /// Required length.
        expected: usize,
        /// Caller-provided length.
        got: usize,
    },
    /// `shader_id` not in the registry (never registered or
    /// forgotten).
    #[error("Tier-2 shader id {0:?} not in registry")]
    UnknownShader(Tier2ShaderId),
    /// The shader doesn't export `atrium_fs_main` (e.g.
    /// it's a vertex or compute shader).
    #[error("shader has no atrium_fs_main entry point")]
    NotAFragmentShader,
    /// The shader doesn't export `atrium_vs_main` (e.g.
    /// it's a fragment or compute shader).
    #[error("shader has no atrium_vs_main entry point")]
    NotAVertexShader,
}
