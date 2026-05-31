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
use atrium_spv_runtime::{TexDesc, UNIFORMS_DESC_BASE};
use rayon::prelude::*;

/// Rasterizer tile size in pixels (R.6+).  8x8 tiles fit 64
/// pixels per tile, comfortably in L1 even with depth +
/// varying + colour state; matches common software-
/// rasterizer choices (Mesa llvmpipe, WARP).  Also the
/// per-stripe height used by R.7's per-stripe parallelism.
const TILE_SIZE: i32 = 8;

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
    /// Tier2ShaderId → raw SPIR-V bytes that produced it.
    /// Used by the daemon to re-compile with
    /// `VkSpecializationInfo`-style overrides at pipeline-
    /// create time without re-uploading the module.
    spirv_by_id: Mutex<HashMap<Tier2ShaderId, Arc<Vec<u8>>>>,
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
            spirv_by_id: Mutex::new(HashMap::new()),
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
        self.register_with_spec_overrides(spirv, &[])
    }

    /// Register a SPIR-V module specialised by `overrides`
    /// (the `VkSpecializationInfo`-style host substitutions
    /// for `OpSpecConstant`s).  Same (spirv, overrides)
    /// returns the same id; different override sets get
    /// distinct ids backed by distinct compiled artifacts.
    pub fn register_with_spec_overrides(
        &self,
        spirv: &[u8],
        overrides: &[(u32, u32)],
    ) -> Result<Tier2ShaderId, LoadError> {
        let hash = ShaderCache::hash_with_spec_overrides(spirv, overrides);
        if let Some(id) = self.by_hash.lock().unwrap().get(&hash).copied() {
            return Ok(id);
        }
        let loaded = self.cache
            .load_or_compile_with_spec_overrides(spirv, overrides)?;
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let id = Tier2ShaderId(*n);
            *n += 1;
            id
        };
        self.by_hash.lock().unwrap().insert(hash, id);
        self.by_id.lock().unwrap().insert(id, loaded);
        self.spirv_by_id.lock().unwrap()
            .insert(id, Arc::new(spirv.to_vec()));
        Ok(id)
    }

    /// Retrieve the SPIR-V bytes that produced this shader
    /// id, if still registered.  Used by the daemon at
    /// pipeline-create time to specialise an already-uploaded
    /// shader against host-supplied spec-constant overrides
    /// without the ICD re-sending the module.
    pub fn get_spirv(&self, id: Tier2ShaderId) -> Option<Arc<Vec<u8>>> {
        self.spirv_by_id.lock().unwrap().get(&id).cloned()
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
            self.spirv_by_id.lock().unwrap().remove(&id);
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
                        1, // gl_FrontFacing: fullscreen FS fill has
                           // no primitive -> treat as front-facing.
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
    /// Tier-2 rasterizer phases R.1 (geometry → pixels) and
    /// R.2 (perspective-correct varying interpolation).  Per
    /// `docs/spec/tier2-renderer.md` §8 and `RUNBOOK.md`
    /// "Next big arc — scoped: tier-2 rasterizer":
    ///
    /// * **Vertex shading** — call `atrium_vs_main` once per
    ///   vertex with that vertex's attribute buffer; collect
    ///   3 clip-space `gl_Position`s.
    /// * **Perspective divide** — NDC = (x/w, y/w, z/w);
    ///   cache `1/w` for perspective-correct interpolation.
    /// * **Viewport mapping** — Vulkan convention (y NOT
    ///   flipped); NDC → screen.
    /// * **Pineda edge functions + bbox**.  Pixel is inside
    ///   iff all three edge values have the same sign
    ///   (handles both windings — we don't bother with a
    ///   winding convention here).
    /// * **Perspective-correct varying interpolation** —
    ///   given the caller's `varyings_per_vertex` buffers
    ///   (an array of `varying_f32_count` `f32` lanes per
    ///   vertex), compute at each inside pixel:
    ///       v(P) = Σ_i b_i * (v_i / w_i)   /   Σ_i b_i * (1 / w_i)
    ///   where `b_i` are normalised barycentric coordinates
    ///   (`edge_i / total_edge_sum`).
    /// * **Fragment shading** — call `atrium_fs_main` with
    ///   the interpolated varying buffer + `gl_FragCoord =
    ///   (cx, cy, interp_z, 1/interp_inv_w)`.
    ///
    /// R.3+ adds depth test, clipping, blending.
    ///
    /// `varyings_per_vertex` is **caller-supplied**, not
    /// captured from the VS's `out_varyings` write.  That's
    /// a temporary scaffolding: the backends today route
    /// every Vertex Output `OpStore` to `out_position`
    /// (cranelift `src/lib.rs:1648`, bespoke `src/lib.rs:2412`)
    /// because the dispatch on `BuiltIn` vs `Location`
    /// decoration is queued as "vertex phase 4+" backend
    /// work.  Once that lands, a production path will
    /// capture each VS invocation's `out_varyings` buffer
    /// and drop the caller-supplied parameter; the
    /// interpolation math here is unchanged.
    pub fn fill_image_triangle(
        &self,
        vs_shader_id: Tier2ShaderId,
        fs_shader_id: Tier2ShaderId,
        draw: &DrawTriangle<'_>,
        width: u32,
        height: u32,
        pixels: &mut [u8],
        mut depth_buffer: Option<&mut [f32]>,
        mut stencil_buffer: Option<&mut [u8]>,
        // MRT: secondary colour attachments (1..N).  Empty for
        // single-attachment draws.  Each is a full RGBA8
        // buffer the same size as `pixels`; the FS writes
        // Location L+1 into `extra_color[L]`.
        extra_color: &mut [&mut [u8]],
    ) -> Result<(), Tier2ExecError> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected_len {
            return Err(Tier2ExecError::BadPixelsLen {
                expected: expected_len,
                got: pixels.len(),
            });
        }
        // R.3 — depth buffer.  Caller-allocated parallel to
        // `pixels`; length is exactly `width * height` f32s.
        // None ⇒ no depth test or write (R.1 / R.2 behaviour).
        let pixel_count = (width as usize) * (height as usize);
        if let Some(db) = depth_buffer.as_deref() {
            if db.len() != pixel_count {
                return Err(Tier2ExecError::BadDepthBufferLen {
                    expected: pixel_count,
                    got: db.len(),
                });
            }
        }
        let varying_bytes = draw.varying_f32_count * 4;
        // Two modes for per-vertex varyings:
        //
        //   (a) Caller supplied non-empty buffers --
        //       varyings_per_vertex[i].len() must match the
        //       declared varying_bytes.  Polygon reads
        //       directly from these.
        //
        //   (b) Caller supplied empty buffers but
        //       varying_f32_count > 0 -- capture VS-write-
        //       through-vary_scratch mode.  fill_image_
        //       triangle below allocates per-vertex scratch
        //       buffers, hands them to vs_main, and uses the
        //       written bytes as the polygon's varyings.
        //       This is the loader-mediated graphics path
        //       since `dispatch_draw` can't allocate &mut
        //       buffers from inside DrawTriangle (no API to
        //       round-trip mut slices through &DrawTriangle).
        //
        // Mode (b) is the common case for real Vulkan apps;
        // mode (a) survives for the original direct callers
        // that pre-baked their varyings.
        let capture_from_vs = varying_bytes > 0
            && draw.varyings_per_vertex.iter().all(|v| v.is_empty());
        if !capture_from_vs {
            for i in 0..3 {
                if draw.varyings_per_vertex[i].len() != varying_bytes {
                    return Err(Tier2ExecError::BadVaryingBufferLen {
                        vertex: i,
                        expected: varying_bytes,
                        got: draw.varyings_per_vertex[i].len(),
                    });
                }
            }
        }
        let vs_loaded = self.get(vs_shader_id)
            .ok_or(Tier2ExecError::UnknownShader(vs_shader_id))?;
        let vs_main = vs_loaded.entry_points.vs_main
            .ok_or(Tier2ExecError::NotAVertexShader)?;
        let fs_loaded = self.get(fs_shader_id)
            .ok_or(Tier2ExecError::UnknownShader(fs_shader_id))?;
        let fs_main = fs_loaded.entry_points.fs_main
            .ok_or(Tier2ExecError::NotAFragmentShader)?;

        let uni_ptr = if draw.uniforms.is_empty() {
            std::ptr::null()
        } else { draw.uniforms.as_ptr() };
        let pc_ptr = if draw.push_constants.is_empty() {
            std::ptr::null()
        } else { draw.push_constants.as_ptr() };

        // ── Step 1: vertex shading.  3 invocations.
        //
        // Per-vertex `vary_scratch` so each VS invocation
        // writes its own varyings.  When `capture_from_vs`
        // is true we read the polygon's varyings from these
        // scratch slots instead of `draw.varyings_per_vertex`.
        let mut clip_positions: [[f32; 4]; 3] = [[0.0; 4]; 3];
        let mut vary_scratch: [[u8; 256]; 3] = [[0u8; 256]; 3];
        let mut clip_dist = [0.0f32; 8];
        for i in 0..3 {
            let attr_ptr = if draw.vertex_attrs[i].is_empty() {
                std::ptr::null()
            } else { draw.vertex_attrs[i].as_ptr() };
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
                    i as u32, draw.instance_index,
                    &mut clip_positions[i] as *mut [f32; 4],
                    vary_scratch[i].as_mut_ptr(),
                    clip_dist.as_mut_ptr(),
                );
            }
        }

        // ── R.4 — clip-space clipping against the near +
        // far planes BEFORE perspective divide.
        //
        // Side planes (left/right/top/bottom) are deferred to
        // R.4 v2; the screen-space bbox clamp later already
        // handles them visibly for "vertex slightly off-
        // screen, w > 0" cases.  Near + far are the
        // must-have planes because perspective divide
        // produces garbage for w ≤ 0 (behind-camera) vertices,
        // and depth-buffer values outside [0, 1] don't have
        // sensible semantics.
        //
        // Algorithm: Sutherland-Hodgman.  Walk the polygon's
        // edges; emit each inside-vertex; at every inside-
        // outside transition, emit an interpolated vertex
        // exactly on the plane.  Then triangulate by fanning
        // from vertex 0 (works for convex polygons, which
        // these always are).
        let n = draw.varying_f32_count;
        // Build initial 3-vertex polygon.  Varyings come from
        // either the per-vertex VS scratch (loader-mediated
        // `dispatch_draw` path) or the caller's pre-supplied
        // `varyings_per_vertex` (older direct callers).
        let mut polygon: Vec<ClipVertex> = (0..3).map(|i| {
            let mut varyings = vec![0.0f32; n];
            let src: &[u8] = if capture_from_vs {
                &vary_scratch[i][..varying_bytes]
            } else {
                draw.varyings_per_vertex[i]
            };
            for k in 0..n {
                let off = k * 4;
                let bytes = [
                    src[off], src[off + 1], src[off + 2], src[off + 3],
                ];
                varyings[k] = f32::from_le_bytes(bytes);
            }
            ClipVertex { pos: clip_positions[i], varyings }
        }).collect();

        // R.4 v1 — near plane: cz >= 0 (Vulkan convention).
        polygon = clip_polygon_plane(&polygon, |v| v.pos[2]);
        if polygon.is_empty() { return Ok(()); }
        // R.4 v1 — far plane: cz <= cw  ⇔  cw - cz >= 0.
        polygon = clip_polygon_plane(&polygon, |v| v.pos[3] - v.pos[2]);
        if polygon.is_empty() { return Ok(()); }

        // R.4 v2 — side planes.  Each side plane has a clip-
        // space signed-distance function vanishing on the
        // plane, positive on the visible side.  Per Vulkan's
        // [-w, w] cube convention:
        //
        //   left   plane: cx >= -cw  ⇔  cx + cw >= 0
        //   right  plane: cx <=  cw  ⇔  cw - cx >= 0
        //   bottom plane: cy >= -cw  ⇔  cy + cw >= 0
        //   top    plane: cy <=  cw  ⇔  cw - cy >= 0
        //
        // Without these the rasterizer relied on screen-space
        // bbox clamping after perspective divide, which is fine
        // for orthographic projection + on-screen geometry but
        // breaks down when a triangle straddles a side edge:
        // the perspective divide and barycentric interpolation
        // produce values outside the [0,1] interpolation range
        // on the clamped side.  R.4 v2 trims the geometry
        // before perspective divide, so every fragment the
        // rasterizer sees has barycentrics in the canonical
        // [0,1] range and per-vertex varyings interpolate
        // correctly.
        polygon = clip_polygon_plane(&polygon, |v| v.pos[0] + v.pos[3]);
        if polygon.is_empty() { return Ok(()); }
        polygon = clip_polygon_plane(&polygon, |v| v.pos[3] - v.pos[0]);
        if polygon.is_empty() { return Ok(()); }
        polygon = clip_polygon_plane(&polygon, |v| v.pos[1] + v.pos[3]);
        if polygon.is_empty() { return Ok(()); }
        polygon = clip_polygon_plane(&polygon, |v| v.pos[3] - v.pos[1]);
        if polygon.is_empty() { return Ok(()); }

        // Polygon needs at least 3 vertices to triangulate.
        if polygon.len() < 3 { return Ok(()); }

        // ── Per-clipped-triangle rasterization.
        // Triangulate by fanning from vertex 0: for an
        // n-vertex polygon, emit (n-2) triangles
        // (verts[0], verts[i], verts[i+1]) for i in 1..n-1.
        let fw = width as f32;
        let fh = height as f32;
        for i in 1..polygon.len() - 1 {
            let tri_verts: [&ClipVertex; 3] = [
                &polygon[0],
                &polygon[i],
                &polygon[i + 1],
            ];

            // Step 2: perspective divide.  Each vertex's clip
            // space (cx, cy, cz, cw) -> NDC (x/w, y/w, z/w).
            // R.4 has already filtered out cw <= 0 (a behind-
            // camera vertex would have been on the outside of
            // the near plane and clipped away), so iw is
            // finite here.  We still guard against w == 0 in
            // case the clip produced an edge-on vertex.
            let mut ndc = [[0.0f32; 3]; 3];
            let mut inv_w = [0.0f32; 3];
            for j in 0..3 {
                let w = tri_verts[j].pos[3];
                let iw = if w == 0.0 { 0.0 } else { 1.0 / w };
                inv_w[j] = iw;
                ndc[j][0] = tri_verts[j].pos[0] * iw;
                ndc[j][1] = tri_verts[j].pos[1] * iw;
                ndc[j][2] = tri_verts[j].pos[2] * iw;
            }

            // Step 3: viewport mapping.  If the caller
            // supplied a viewport (`vkCmdSetViewport`), NDC
            // maps into the (vp.x, vp.y, vp.width, vp.height)
            // sub-rect of the framebuffer; otherwise NDC maps
            // to the full framebuffer (legacy behaviour).
            let (vp_x, vp_y, vp_w, vp_h) = match draw.viewport {
                Some(v) => (v.x, v.y, v.width, v.height),
                None    => (0.0,  0.0,  fw,        fh),
            };
            let screen: [(f32, f32); 3] = [
                (vp_x + (ndc[0][0] + 1.0) * 0.5 * vp_w,
                 vp_y + (ndc[0][1] + 1.0) * 0.5 * vp_h),
                (vp_x + (ndc[1][0] + 1.0) * 0.5 * vp_w,
                 vp_y + (ndc[1][1] + 1.0) * 0.5 * vp_h),
                (vp_x + (ndc[2][0] + 1.0) * 0.5 * vp_w,
                 vp_y + (ndc[2][1] + 1.0) * 0.5 * vp_h),
            ];

            // Step 4: attr/w per varying lane.
            let mut varying_over_w: [Vec<f32>; 3] = [
                vec![0.0f32; n], vec![0.0f32; n], vec![0.0f32; n],
            ];
            for j in 0..3 {
                for k in 0..n {
                    varying_over_w[j][k] = tri_verts[j].varyings[k] * inv_w[j];
                }
            }

            // Step 5: bbox, then intersect with scissor rect.
            // Scissor is in framebuffer-pixel coords and gets
            // clamped to the framebuffer itself in case the
            // app supplied an out-of-range rect.
            let (sx0, sy0, sx1, sy1) = match draw.scissor {
                Some(s) => {
                    let x0 = (s.x as i32).max(0);
                    let y0 = (s.y as i32).max(0);
                    let x1 = ((s.x as i32) + (s.width as i32)).min(width as i32);
                    let y1 = ((s.y as i32) + (s.height as i32)).min(height as i32);
                    (x0, y0, x1, y1)
                }
                None => (0, 0, width as i32, height as i32),
            };
            let min_x = screen.iter().map(|p| p.0)
                .fold(f32::INFINITY, f32::min).floor() as i32;
            let max_x = screen.iter().map(|p| p.0)
                .fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
            let min_y = screen.iter().map(|p| p.1)
                .fold(f32::INFINITY, f32::min).floor() as i32;
            let max_y = screen.iter().map(|p| p.1)
                .fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
            let min_x = min_x.max(sx0);
            let max_x = max_x.min(sx1 - 1);
            let min_y = min_y.max(sy0);
            let max_y = max_y.min(sy1 - 1);
            if min_x > max_x || min_y > max_y { continue; }

            // Step 6: edges.
            let (a, b, c) = (screen[0], screen[1], screen[2]);
            let total_edge = edge_fn(a, b, c);

            // Cull mode.  Screen-space winding sign matches
            // `edge_fn` directly: `total_edge > 0` ⇒ CCW.
            // Vulkan's default front-face is CCW; flipping
            // to CW inverts which sign counts as front.
            if total_edge != 0.0 {
                let is_front = match draw.front_face {
                    FrontFace::CounterClockwise => total_edge > 0.0,
                    FrontFace::Clockwise        => total_edge < 0.0,
                };
                let cull = match draw.cull_mode {
                    CullMode::None         => false,
                    CullMode::Front        =>  is_front,
                    CullMode::Back         => !is_front,
                    CullMode::FrontAndBack => true,
                };
                if cull { continue; }
            }

            // Tile coordinates that overlap the triangle bbox.
            let tile_min_x = min_x / TILE_SIZE;
            let tile_max_x = max_x / TILE_SIZE;
            let tile_min_y = min_y / TILE_SIZE;
            let tile_max_y = max_y / TILE_SIZE;

            // ── R.7: stripe-level parallelism.  Pre-split
            // pixels (and optionally depth) into chunks of
            // TILE_SIZE rows; each chunk is contiguous in
            // row-major memory and disjoint from the others,
            // so `chunks_mut` gives us safe per-stripe
            // `&mut` borrows.  Wrap each chunk + its global
            // stripe index in a `StripeWork`; rayon's
            // `par_iter_mut` distributes tasks across the
            // global thread pool.
            //
            // Triangle setup (a, b, c, total_edge, ndc,
            // inv_w, varying_over_w) is shared via `&`; each
            // task allocates its own `interp_buf` since
            // that's mutated per-pixel.  Function pointers
            // (`fs_main`) and slice references are Send +
            // Sync, so the closure captures them without
            // additional wrapping.
            let (depth_min, depth_max) = match draw.viewport {
                Some(v) => (v.min_depth, v.max_depth),
                None    => (0.0, 1.0),
            };
            let setup = TriangleSetup {
                a, b, c, total_edge,
                ndc, inv_w,
                varying_over_w: &varying_over_w,
                n, varying_bytes,
                min_x, max_x, min_y, max_y,
                tile_min_x, tile_max_x,
                width,
                depth_write: draw.depth_write,
                depth_compare_op: draw.depth_compare_op,
                depth_bounds: draw.depth_bounds,
                depth_min, depth_max,
                depth_bias_offset: compute_depth_bias_offset(
                    draw.depth_bias,
                    // Triangle screen coords + per-vertex
                    // windowed depths.
                    &screen,
                    &[
                        depth_min + ndc[0][2] * (depth_max - depth_min),
                        depth_min + ndc[1][2] * (depth_max - depth_min),
                        depth_min + ndc[2][2] * (depth_max - depth_min),
                    ],
                    total_edge,
                ),
                stencil_face: draw.stencil.map(|s| {
                    // Triangle face = front when screen-space
                    // winding matches the active front_face
                    // convention; otherwise back.  edge_fn
                    // returns positive on CCW, negative on CW.
                    let is_front = match draw.front_face {
                        FrontFace::CounterClockwise => total_edge > 0.0,
                        FrontFace::Clockwise        => total_edge < 0.0,
                    };
                    if is_front { s.front } else { s.back }
                }),
                // gl_FrontFacing source: winding vs front_face.
                // total_edge == 0 (degenerate) never rasterizes a
                // pixel, so the arbitrary `false` is unobservable.
                front_facing: match draw.front_face {
                    FrontFace::CounterClockwise => total_edge > 0.0,
                    FrontFace::Clockwise        => total_edge < 0.0,
                },
            };

            let pixel_stripe_bytes =
                (TILE_SIZE as usize) * (width as usize) * 4;
            let depth_stripe_elems =
                (TILE_SIZE as usize) * (width as usize);
            // Stencil buffer is the same shape as depth but
            // u8 per pixel; same striping math.
            let stencil_stripe_elems = depth_stripe_elems;

            // Build per-stripe tasks.  Skip stripes that
            // don't overlap the triangle bbox (so rayon
            // doesn't get spammed with no-op tasks).  Iterate
            // pixel_chunks + depth_chunks + stencil_chunks
            // in lock-step.
            let pixel_chunks: Vec<&mut [u8]> =
                pixels.chunks_mut(pixel_stripe_bytes).collect();
            let depth_chunks: Vec<Option<&mut [f32]>> =
                match depth_buffer.as_deref_mut() {
                    Some(db) => db
                        .chunks_mut(depth_stripe_elems)
                        .map(Some)
                        .collect(),
                    None => (0..pixel_chunks.len())
                        .map(|_| None)
                        .collect(),
                };
            let stencil_chunks: Vec<Option<&mut [u8]>> =
                match stencil_buffer.as_deref_mut() {
                    Some(sb) => sb
                        .chunks_mut(stencil_stripe_elems)
                        .map(Some)
                        .collect(),
                    None => (0..pixel_chunks.len())
                        .map(|_| None)
                        .collect(),
                };
            // MRT: chunk each extra colour attachment into
            // stripes, then transpose into per-stripe Vecs so
            // each StripeWork owns one slice of every extra
            // attachment.  `extra_iters` holds a ChunksMut per
            // attachment; pulling one `.next()` from each per
            // stripe keeps them in lock-step with the primary
            // pixel stripes.
            let num_stripes = pixel_chunks.len();
            let mut extra_iters: Vec<std::slice::ChunksMut<u8>> =
                extra_color.iter_mut()
                    .map(|buf| buf.chunks_mut(pixel_stripe_bytes))
                    .collect();
            let mut extra_per_stripe: Vec<Vec<&mut [u8]>> =
                (0..num_stripes)
                    .map(|_| extra_iters.iter_mut()
                        .filter_map(|it| it.next())
                        .collect())
                    .collect();
            let mut tasks: Vec<StripeWork> = pixel_chunks
                .into_iter()
                .zip(depth_chunks.into_iter())
                .zip(stencil_chunks.into_iter())
                .zip(extra_per_stripe.drain(..))
                .enumerate()
                .filter(|(s, _)| {
                    let tile_y = *s as i32;
                    tile_y >= tile_min_y && tile_y <= tile_max_y
                })
                .map(|(s, (((px, dp), st), ex))| StripeWork {
                    stripe_y: s as i32,
                    pixels: px,
                    depth: dp,
                    stencil: st,
                    extra_color: ex,
                })
                .collect();

            // Parallel rasterise.  Each task writes only into
            // its own pixel + depth slice — disjoint by
            // construction, so no synchronisation is needed.
            tasks.par_iter_mut().for_each(|task| {
                rasterize_stripe(task, &setup, draw, fs_main);
            });
        }
        Ok(())
    }

    /// Rasterize a `PointList`: one 1x1 fragment per vertex.
    ///
    /// Each vertex runs the VS (capturing its `out_varyings`
    /// scratch), is perspective-divided + viewport-mapped to a
    /// window position, depth-tested, then shaded by a single FS
    /// invocation whose `gl_FragCoord` is the point centre and
    /// whose varyings are that vertex's outputs verbatim (points
    /// have no interpolation).  No 2x2 quad, MRT, stencil, or
    /// MSAA -- screen-space derivatives at a point are zero and
    /// the other features don't apply to 1-pixel primitives.
    /// `gl_FrontFacing` is reported as front (points have no
    /// winding).
    pub fn fill_image_points(
        &self,
        vs_shader_id: Tier2ShaderId,
        fs_shader_id: Tier2ShaderId,
        draw: &DrawPoints<'_>,
        width: u32,
        height: u32,
        pixels: &mut [u8],
        mut depth_buffer: Option<&mut [f32]>,
    ) -> Result<(), Tier2ExecError> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected_len {
            return Err(Tier2ExecError::BadPixelsLen {
                expected: expected_len, got: pixels.len(),
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

        let uni_ptr = if draw.uniforms.is_empty() {
            std::ptr::null()
        } else { draw.uniforms.as_ptr() };
        let pc_ptr = if draw.push_constants.is_empty() {
            std::ptr::null()
        } else { draw.push_constants.as_ptr() };

        let fw = width as f32;
        let fh = height as f32;
        let (vp_x, vp_y, vp_w, vp_h, depth_min, depth_max) = match draw.viewport {
            Some(v) => (v.x, v.y, v.width, v.height, v.min_depth, v.max_depth),
            None    => (0.0, 0.0, fw, fh, 0.0, 1.0),
        };
        let varying_bytes = draw.varying_f32_count * 4;

        if draw.stride == 0 { return Ok(()); }
        let vertex_count = draw.vertices.len() / draw.stride;

        for v in 0..vertex_count {
            let attr = &draw.vertices[v * draw.stride .. (v + 1) * draw.stride];
            let attr_ptr = if attr.is_empty() {
                std::ptr::null()
            } else { attr.as_ptr() };

            let mut clip = [0.0f32; 4];
            let mut vary = vec![0u8; varying_bytes.max(1)];
            let mut clip_dist = [0.0f32; 8];
            // SAFETY: dlopened C-ABI VS; signature checked at open.
            unsafe {
                vs_main(
                    attr_ptr, std::ptr::null(), uni_ptr, pc_ptr,
                    v as u32, draw.instance_index,
                    &mut clip as *mut [f32; 4],
                    vary.as_mut_ptr(),
                    clip_dist.as_mut_ptr(),
                );
            }

            // Near/far + behind-camera clip (Vulkan: 0 <= z <= w).
            let w = clip[3];
            if w <= 0.0 { continue; }
            let iw = 1.0 / w;
            let ndc_x = clip[0] * iw;
            let ndc_y = clip[1] * iw;
            let ndc_z = clip[2] * iw;
            if ndc_z < 0.0 || ndc_z > 1.0 { continue; }

            let sx = vp_x + (ndc_x + 1.0) * 0.5 * vp_w;
            let sy = vp_y + (ndc_y + 1.0) * 0.5 * vp_h;
            let px = sx.floor() as i32;
            let py = sy.floor() as i32;
            if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                continue;
            }
            let lin = (py as usize) * (width as usize) + (px as usize);
            let window_z = depth_min + ndc_z * (depth_max - depth_min);

            // Depth test + write.
            if let Some(db) = depth_buffer.as_deref_mut() {
                if !depth_compare(window_z, db[lin], draw.depth_compare_op) {
                    continue;
                }
                if draw.depth_write { db[lin] = window_z; }
            }

            let mut out_color = [0.0f32; 4];
            let mut out_depth = 0.0f32;
            let vptr = if varying_bytes == 0 {
                std::ptr::null()
            } else { vary.as_ptr() };
            // SAFETY: dlopened C-ABI FS; 11-arg signature.
            unsafe {
                fs_main(
                    vptr, uni_ptr, pc_ptr,
                    sx, sy, window_z, iw,
                    0,
                    out_color.as_mut_ptr(),
                    &mut out_depth,
                    1, // gl_FrontFacing: points are front-facing.
                );
            }

            let idx = lin * 4;
            if idx + 4 > pixels.len() { continue; }
            let abs = &draw.blend_state;
            let am = abs.write_mask;
            let dst = [
                pixels[idx]     as f32 / 255.0,
                pixels[idx + 1] as f32 / 255.0,
                pixels[idx + 2] as f32 / 255.0,
                pixels[idx + 3] as f32 / 255.0,
            ];
            let final_color = if abs.enable {
                apply_blend(abs, out_color, dst)
            } else { out_color };
            if am.r { pixels[idx]     = f32_to_u8(final_color[0]); }
            if am.g { pixels[idx + 1] = f32_to_u8(final_color[1]); }
            if am.b { pixels[idx + 2] = f32_to_u8(final_color[2]); }
            if am.a { pixels[idx + 3] = f32_to_u8(final_color[3]); }
        }
        Ok(())
    }

    /// Rasterize a `LineList`: successive vertex pairs form
    /// independent 1px-wide line segments.
    ///
    /// Each endpoint runs the VS (capturing its `out_varyings`),
    /// is perspective-divided + viewport-mapped, then the segment
    /// is walked by DDA (one fragment per major-axis step).  Per
    /// fragment the parameter `t in [0,1]` drives perspective-
    /// correct varying interpolation (lerp of `varying/w` and of
    /// `1/w`, then divide) and linear window-depth interpolation,
    /// followed by depth-test + FS + blend.  Shares the
    /// [`DrawPoints`] parameter block.  No 2x2 quad / MRT /
    /// stencil / MSAA (a thin line has no area to sample).
    /// `gl_FrontFacing` is reported front (lines have no winding).
    pub fn fill_image_lines(
        &self,
        vs_shader_id: Tier2ShaderId,
        fs_shader_id: Tier2ShaderId,
        draw: &DrawPoints<'_>,
        width: u32,
        height: u32,
        pixels: &mut [u8],
        mut depth_buffer: Option<&mut [f32]>,
    ) -> Result<(), Tier2ExecError> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected_len {
            return Err(Tier2ExecError::BadPixelsLen {
                expected: expected_len, got: pixels.len(),
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

        let uni_ptr = if draw.uniforms.is_empty() {
            std::ptr::null()
        } else { draw.uniforms.as_ptr() };
        let pc_ptr = if draw.push_constants.is_empty() {
            std::ptr::null()
        } else { draw.push_constants.as_ptr() };

        let fw = width as f32;
        let fh = height as f32;
        let (vp_x, vp_y, vp_w, vp_h, depth_min, depth_max) = match draw.viewport {
            Some(v) => (v.x, v.y, v.width, v.height, v.min_depth, v.max_depth),
            None    => (0.0, 0.0, fw, fh, 0.0, 1.0),
        };
        let n = draw.varying_f32_count;
        let varying_bytes = n * 4;

        if draw.stride == 0 { return Ok(()); }
        let vertex_count = draw.vertices.len() / draw.stride;
        let seg_count = vertex_count / 2;

        // Run the VS for one endpoint, returning its clip-space
        // position + decoded f32 varying lanes.
        let run_vs = |v: usize| -> ([f32; 4], Vec<f32>) {
            let attr = &draw.vertices[v * draw.stride .. (v + 1) * draw.stride];
            let attr_ptr = if attr.is_empty() {
                std::ptr::null()
            } else { attr.as_ptr() };
            let mut clip = [0.0f32; 4];
            let mut vary = vec![0u8; varying_bytes.max(1)];
            let mut clip_dist = [0.0f32; 8];
            // SAFETY: dlopened C-ABI VS; signature checked at open.
            unsafe {
                vs_main(
                    attr_ptr, std::ptr::null(), uni_ptr, pc_ptr,
                    v as u32, draw.instance_index,
                    &mut clip as *mut [f32; 4],
                    vary.as_mut_ptr(),
                    clip_dist.as_mut_ptr(),
                );
            }
            let mut lanes = vec![0.0f32; n];
            for k in 0..n {
                lanes[k] = f32::from_le_bytes(
                    vary[k * 4..k * 4 + 4].try_into().unwrap());
            }
            (clip, lanes)
        };

        for s in 0..seg_count {
            let (c0, vary0) = run_vs(2 * s);
            let (c1, vary1) = run_vs(2 * s + 1);

            // Behind-camera reject (full near-plane line clipping
            // is deferred; w<=0 endpoints don't divide sensibly).
            if c0[3] <= 0.0 || c1[3] <= 0.0 { continue; }
            let iw0 = 1.0 / c0[3];
            let iw1 = 1.0 / c1[3];
            let ndc0 = [c0[0] * iw0, c0[1] * iw0, c0[2] * iw0];
            let ndc1 = [c1[0] * iw1, c1[1] * iw1, c1[2] * iw1];

            let sx0 = vp_x + (ndc0[0] + 1.0) * 0.5 * vp_w;
            let sy0 = vp_y + (ndc0[1] + 1.0) * 0.5 * vp_h;
            let sx1 = vp_x + (ndc1[0] + 1.0) * 0.5 * vp_w;
            let sy1 = vp_y + (ndc1[1] + 1.0) * 0.5 * vp_h;
            let wz0 = depth_min + ndc0[2] * (depth_max - depth_min);
            let wz1 = depth_min + ndc1[2] * (depth_max - depth_min);

            // DDA: step along the major axis, one fragment per
            // integer step (Vulkan diamond-exit lines are not
            // modelled; this is a Bresenham-class thin line).
            let dx = sx1 - sx0;
            let dy = sy1 - sy0;
            let steps = dx.abs().max(dy.abs()).ceil().max(1.0) as i32;

            let mut interp = vec![0u8; varying_bytes.max(1)];
            for step in 0..=steps {
                let t = step as f32 / steps as f32;
                let sx = sx0 + dx * t;
                let sy = sy0 + dy * t;
                let px = sx.floor() as i32;
                let py = sy.floor() as i32;
                if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                    continue;
                }
                let lin = (py as usize) * (width as usize) + (px as usize);

                // Perspective-correct interpolation: 1/w is linear
                // in screen space; each varying/w is linear; the
                // attribute is (varying/w)/(1/w).
                let iw = iw0 + (iw1 - iw0) * t;
                let oiw = if iw == 0.0 { 0.0 } else { 1.0 / iw };
                for k in 0..n {
                    let a = vary0[k] * iw0;
                    let b = vary1[k] * iw1;
                    let v = (a + (b - a) * t) * oiw;
                    interp[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
                // Window depth interpolates linearly along the line.
                let window_z = wz0 + (wz1 - wz0) * t;

                if let Some(db) = depth_buffer.as_deref_mut() {
                    if !depth_compare(window_z, db[lin], draw.depth_compare_op) {
                        continue;
                    }
                    if draw.depth_write { db[lin] = window_z; }
                }

                let mut out_color = [0.0f32; 4];
                let mut out_depth = 0.0f32;
                let vptr = if varying_bytes == 0 {
                    std::ptr::null()
                } else { interp.as_ptr() };
                // SAFETY: dlopened C-ABI FS; 11-arg signature.
                unsafe {
                    fs_main(
                        vptr, uni_ptr, pc_ptr,
                        sx, sy, window_z, oiw,
                        0,
                        out_color.as_mut_ptr(),
                        &mut out_depth,
                        1, // gl_FrontFacing: lines are front-facing.
                    );
                }

                let idx = lin * 4;
                if idx + 4 > pixels.len() { continue; }
                let abs = &draw.blend_state;
                let am = abs.write_mask;
                let dst = [
                    pixels[idx]     as f32 / 255.0,
                    pixels[idx + 1] as f32 / 255.0,
                    pixels[idx + 2] as f32 / 255.0,
                    pixels[idx + 3] as f32 / 255.0,
                ];
                let final_color = if abs.enable {
                    apply_blend(abs, out_color, dst)
                } else { out_color };
                if am.r { pixels[idx]     = f32_to_u8(final_color[0]); }
                if am.g { pixels[idx + 1] = f32_to_u8(final_color[1]); }
                if am.b { pixels[idx + 2] = f32_to_u8(final_color[2]); }
                if am.a { pixels[idx + 3] = f32_to_u8(final_color[3]); }
            }
        }
        Ok(())
    }
}

/// Parameters for [`Tier2Registry::fill_image_points`] and
/// [`Tier2Registry::fill_image_lines`].  A lighter sibling of
/// [`DrawTriangle`] -- point and line primitives have no
/// per-fragment culling, stencil, MRT, or MSAA, so only the
/// fields a thin primitive needs are carried.
pub struct DrawPoints<'a> {
    /// Assembled per-vertex attribute bytes: `vertex_count`
    /// records of `stride` bytes, fed to the VS as
    /// `in_attributes` one vertex at a time.
    pub vertices: &'a [u8],
    /// Per-vertex stride in bytes.
    pub stride: usize,
    /// Push-constant bytes (or empty).
    pub push_constants: &'a [u8],
    /// Uniform / descriptor-table bytes (or empty).
    pub uniforms: &'a [u8],
    /// Number of f32 varying lanes the VS writes / FS reads.
    pub varying_f32_count: usize,
    /// Colour blend state for the single attachment.
    pub blend_state: BlendState,
    /// Optional viewport (position + depth range).
    pub viewport: Option<Viewport>,
    /// Depth write enable.
    pub depth_write: bool,
    /// Depth compare op.
    pub depth_compare_op: CompareOp,
    /// `gl_InstanceIndex` value for the VS.
    pub instance_index: u32,
}

/// Compute the depth-bias offset for one triangle per
/// Vulkan's "Depth Bias" rules:
///
///   o = constant * r + slope * m
///   clamped:
///     if clamp > 0:  o = min(o, clamp)
///     if clamp < 0:  o = max(o, clamp)
///     else:          unchanged
///
/// `r` is the minimum representable depth difference; for
/// the f32 depth buffer tier-2 uses, 2^-23 (≈ 1.19e-7) is
/// the spec-permitted choice (matches the mantissa-bit
/// rule).  `m` is the largest of |dz/dx|, |dz/dy| in window
/// space, computed analytically from the triangle's plane
/// equation: with screen vertices (x_i, y_i) and depths
/// z_i, the plane normal is the cross product of two
/// triangle edges and the gradient is -n_xy / n_z, where
/// n_z is `total_edge`.
fn compute_depth_bias_offset(
    bias: Option<(f32, f32, f32)>,
    screen: &[(f32, f32); 3],
    z: &[f32; 3],
    total_edge: f32,
) -> f32 {
    let Some((c, clamp, slope)) = bias else { return 0.0 };
    if total_edge == 0.0 { return 0.0; }
    let (x0, y0) = screen[0];
    let (x1, y1) = screen[1];
    let (x2, y2) = screen[2];
    // n_x / n_z and n_y / n_z give -dz_dx / -dz_dy (signs
    // cancel in the magnitude below).
    let n_x = (y1 - y0) * (z[2] - z[0]) - (z[1] - z[0]) * (y2 - y0);
    let n_y = (z[1] - z[0]) * (x2 - x0) - (x1 - x0) * (z[2] - z[0]);
    let dz_dx = -n_x / total_edge;
    let dz_dy = -n_y / total_edge;
    let m = dz_dx.abs().max(dz_dy.abs());
    let r = (1.0f32 / (1u32 << 23) as f32) as f32;
    let mut o = c * r + slope * m;
    if clamp > 0.0 { o = o.min(clamp); }
    else if clamp < 0.0 { o = o.max(clamp); }
    o
}

/// Vulkan-spec depth compare.  Float NaN behaves correctly
/// out of the box: every comparison with NaN is false, so
/// NaN depths fail every test except `Always`.
fn depth_compare(new: f32, old: f32, op: CompareOp) -> bool {
    match op {
        CompareOp::Never          => false,
        CompareOp::Less           => new <  old,
        CompareOp::Equal          => new == old,
        CompareOp::LessOrEqual    => new <= old,
        CompareOp::Greater        => new >  old,
        CompareOp::NotEqual       => new != old,
        CompareOp::GreaterOrEqual => new >= old,
        CompareOp::Always         => true,
    }
}

/// Pineda edge function: signed 2-area of the triangle
/// formed by points A, B, P.  Positive on one side of
/// directed edge A→B, negative on the other, zero on the
/// edge itself.
#[inline]
fn edge_fn(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Read-only per-triangle setup shared across all
/// parallel stripe tasks (R.7).  All fields are Send +
/// Sync.
struct TriangleSetup<'s> {
    a: (f32, f32),
    b: (f32, f32),
    c: (f32, f32),
    /// Total signed 2-area of the triangle (sum of the
    /// three Pineda edge functions at any point).
    total_edge: f32,
    /// NDC positions for perspective-correct varying
    /// interpolation and gl_FragCoord.z computation.
    ndc: [[f32; 3]; 3],
    /// 1/w per vertex for the perspective denominator.
    inv_w: [f32; 3],
    /// Pre-divided varying lanes: varying_over_w[i][k] =
    /// varying_per_vertex[i][k] / w[i].
    varying_over_w: &'s [Vec<f32>; 3],
    /// Number of f32 varying lanes per vertex.
    n: usize,
    /// Bytes per varying lane group (`n * 4`).
    varying_bytes: usize,
    /// Triangle bbox in screen pixels.
    min_x: i32,
    max_x: i32,
    min_y: i32,
    max_y: i32,
    /// Tile-grid bbox X range (in tile units of
    /// `TILE_SIZE` pixels).  Y range is handled by the
    /// per-stripe task filter at the call site (each task
    /// already knows its own `stripe_y`).
    tile_min_x: i32,
    tile_max_x: i32,
    /// Image width in pixels (for row-major indexing).
    width: u32,
    /// Mirror of `DrawTriangle::depth_write`.  When `false`,
    /// the rasterizer evaluates the depth test for colour-
    /// output gating but leaves the depth buffer unmodified.
    depth_write: bool,
    /// Mirror of `DrawTriangle::depth_compare_op`.
    depth_compare_op: CompareOp,
    /// Mirror of `DrawTriangle::depth_bounds`.
    depth_bounds: Option<(f32, f32)>,
    /// Mirror of `DrawTriangle::stencil`, with the per-face
    /// state already resolved at triangle setup based on
    /// `total_edge` sign + `draw.front_face`.  `None` means
    /// no stencil testing.
    stencil_face: Option<StencilFaceState>,
    /// Precomputed depth-bias offset to add to the windowed
    /// depth at each pixel.  `0.0` when no bias is active.
    depth_bias_offset: f32,
    /// Viewport depth range (`vkCmdSetViewport`'s
    /// `min_depth` / `max_depth`).  Used to remap the
    /// interpolated NDC.z into windowed-depth space before
    /// the depth test + buffer write + FS frag_coord.z
    /// hand-off.  Defaults to `(0.0, 1.0)` (identity) when
    /// the caller didn't supply a viewport.
    depth_min: f32,
    depth_max: f32,
    /// Whether this triangle is front-facing: its screen-space
    /// winding matches the pipeline's `VkFrontFace`.  Passed to
    /// the FS as `gl_FrontFacing` (the trailing `front_facing`
    /// parameter).  Computed once per triangle from
    /// `total_edge`'s sign + `draw.front_face`.
    front_facing: bool,
}

/// One stripe's mutable working set for R.7's per-stripe
/// parallelism.  A stripe is `TILE_SIZE` rows of pixels
/// (and depth, when present) — a contiguous slice in
/// row-major memory.  Different stripes are disjoint, so
/// rayon can hand each task its own `&mut` borrow without
/// synchronisation.
struct StripeWork<'p, 'd> {
    /// Stripe index (= tile row in tile-grid units).
    stripe_y: i32,
    /// `TILE_SIZE * width * 4` bytes (last stripe may be
    /// shorter when height isn't a multiple of TILE_SIZE).
    pixels: &'p mut [u8],
    /// `TILE_SIZE * width` f32 elements, or `None` when
    /// the caller didn't supply a depth buffer.
    depth: Option<&'d mut [f32]>,
    /// `TILE_SIZE * width` u8 elements, or `None` when the
    /// caller didn't supply a stencil buffer.
    stencil: Option<&'d mut [u8]>,
    /// MRT secondary colour attachments: one `TILE_SIZE *
    /// width * 4`-byte stripe slice per extra attachment.
    /// Empty for single-attachment draws.  Index = colour
    /// attachment (L+1); the FS's Location L+1 output lands
    /// in `out_color[(L+1)*4 ..]` and is scattered here.
    extra_color: Vec<&'d mut [u8]>,
}

/// Per-stripe pixel loop.  Walks the tile columns that
/// overlap the triangle within this stripe; for each tile,
/// iterates the (tile ∩ triangle bbox) pixel rectangle and
/// runs the existing edge-function inside test +
/// perspective-correct varying interpolation + depth test
/// + FS call + blend + write-mask path.
///
/// All writes go through `task.pixels` / `task.depth` in
/// **stripe-local** coordinates (`py - stripe_y *
/// TILE_SIZE`) so the slice indexing matches the chunk's
/// length.
///
/// # R.8 deferred -- SIMD pixel quads
///
/// The current loop calls `fs_main` once per pixel.  Real
/// GPUs dispatch the FS in 2x2 pixel quads so derivatives
/// (`dFdx` / `dFdy`) work and lanewise SIMD is natural.
/// R.8 would change this in a coordinated way across four
/// places:
///
///   1. **FS ABI** (atrium-spv-loader::FsMain): take per-
///      input arrays sized 4 instead of scalar values
///      (e.g.  `varyings: *const [u8; 4]` for a 2x2 quad
///      worth of varyings), with a 4-bit lane-mask
///      indicating which lanes are inside the triangle.
///      out_color becomes `[[f32; 4]; 4]`; out_depth
///      becomes `[f32; 4]`.
///   2. **rasterize_stripe**: iterate in 2x2 chunks rather
///      than 1x1; pack the 4 pixel-centre coordinates into
///      the per-quad call; un-pack out_color back into
///      the stripe pixel buffer per lane mask.
///   3. **Cranelift backend** (atrium-spv-backend-cranelift):
///      lift every scalar Op to a 4-lane operation when the
///      stage is Fragment.  Either via Cranelift's first-
///      class SIMD ISel or by manually emitting 4 scalar
///      copies (the conservative path).  AccessChain +
///      Load + Store stay scalar (the per-lane gather/scatter
///      is what produces the SIMD pattern).
///   4. **Bespoke backend** (atrium-spv-backend-bespoke):
///      same lifting in the ARM64 emitter.  NEON's
///      `V0..V31` per-lane registers map naturally.  This
///      is the load-bearing perf win since each FS call
///      goes from "8 instructions per pixel" to "8
///      instructions per quad".
///
/// Together these would roughly 4x throughput on FS-bound
/// scenes (the rasterizer already does tile-level reject,
/// so vector lanes near-fully-utilised inside the triangle).
/// The arc is bounded but cross-cutting -- queued behind
/// the bespoke-compute work since both need the bespoke
/// backend's instruction scheduler to grow new patterns,
/// and doing them together avoids two ABI-break rebuilds.
fn rasterize_stripe(
    task: &mut StripeWork<'_, '_>,
    setup: &TriangleSetup<'_>,
    draw: &DrawTriangle<'_>,
    fs_main: atrium_spv_loader::FsMain,
) {
    let tile_y = task.stripe_y;
    let stripe_pixel_y = tile_y * TILE_SIZE;

    // Reconstruct uniform / push-constant raw pointers
    // inside the task.  Storing them in `TriangleSetup`
    // would require a `Send + Sync` wrapper around the raw
    // `*const u8`; deriving them here from the (Sync)
    // slices is simpler.
    // Implicit-LOD setup.  When enabled + a multi-mip
    // texture is bound at binding 0, clone the uniforms
    // buffer per stripe (so per-pixel descriptor-pointer
    // rewrites don't race other stripes) and read the mip
    // chain off the base TexDesc.  `lod_ctx` carries the
    // descriptor-slot offset + the mip array pointer/count +
    // base texel dims used to scale the gradient into texels.
    let mut uni_local: Vec<u8> = Vec::new();
    let mut lod_ctx: Option<(usize, *const TexDesc, u32, f32, f32)> = None;
    if draw.compute_implicit_lod
        && !draw.uniforms.is_empty()
        && setup.n >= 2
        && draw.uniforms.len() >= UNIFORMS_DESC_BASE + 8
    {
        let desc_off = UNIFORMS_DESC_BASE; // binding 0 tex_desc*
        let tex_ptr = u64::from_le_bytes(
            draw.uniforms[desc_off..desc_off + 8].try_into().unwrap());
        if tex_ptr != 0 {
            // SAFETY: the daemon built this TexDesc + its mip
            // array and keeps them alive for the whole draw
            // (named-bound `tex_descs` / `mip_desc_arrays`).
            let base = unsafe { &*(tex_ptr as *const TexDesc) };
            if base.mip_count > 1 && !base.mip_descs.is_null() {
                uni_local = draw.uniforms.to_vec();
                lod_ctx = Some((
                    desc_off, base.mip_descs, base.mip_count,
                    base.width as f32, base.height as f32,
                ));
            }
        }
    }

    let uni_ptr = if let Some(_) = lod_ctx {
        uni_local.as_ptr()
    } else if draw.uniforms.is_empty() {
        std::ptr::null()
    } else { draw.uniforms.as_ptr() };
    let pc_ptr = if draw.push_constants.is_empty() {
        std::ptr::null()
    } else { draw.push_constants.as_ptr() };

    // Per-stripe scratch for the interpolated varying
    // buffer (mutated per pixel; can't be shared across
    // tasks).
    let mut interp_buf: Vec<u8> = vec![0u8; setup.varying_bytes];
    // Per-stripe scratch for the 2x2-quad probe pass (only
    // touched when `draw.uses_derivatives`).  Holds the
    // interpolated varyings for a single probe lane, kept
    // separate from `interp_buf` so the real pixel's varyings
    // survive the four probe runs.
    let mut probe_buf: Vec<u8> = vec![0u8; setup.varying_bytes];

    let (a, b, c) = (setup.a, setup.b, setup.c);

    for tile_x in setup.tile_min_x..=setup.tile_max_x {
        let t_min_x = (tile_x * TILE_SIZE).max(setup.min_x);
        let t_max_x = ((tile_x + 1) * TILE_SIZE - 1).min(setup.max_x);
        let t_min_y = (tile_y * TILE_SIZE).max(setup.min_y);
        let t_max_y = ((tile_y + 1) * TILE_SIZE - 1).min(setup.max_y);
        if t_min_x > t_max_x || t_min_y > t_max_y { continue; }

        // R.6 v2 — per-tile trivial reject.  Each Pineda edge
        // function is linear in (x, y), so its extrema over an
        // axis-aligned rectangle are attained at the corners.
        // If every corner of the tile is on the outside of
        // some single edge, no pixel in the tile can be inside
        // the triangle → skip the entire per-pixel loop.  This
        // is the standard tile-rasterizer trivial reject; it's
        // a pure perf opt (the per-pixel test that follows is
        // the same one).
        let corners: [(f32, f32); 4] = [
            (t_min_x as f32 + 0.5, t_min_y as f32 + 0.5),
            (t_max_x as f32 + 0.5, t_min_y as f32 + 0.5),
            (t_min_x as f32 + 0.5, t_max_y as f32 + 0.5),
            (t_max_x as f32 + 0.5, t_max_y as f32 + 0.5),
        ];
        let e0: [f32; 4] = corners.map(|p| edge_fn(b, c, p));
        let e1: [f32; 4] = corners.map(|p| edge_fn(c, a, p));
        let e2: [f32; 4] = corners.map(|p| edge_fn(a, b, p));
        // For a CCW triangle (total_edge > 0) a pixel is inside
        // when every edge is >= 0; the tile is trivially out
        // when some edge has max < 0 across all 4 corners.  For
        // CW (total_edge < 0) the inside test flips, so the
        // trivial-reject flips too.
        let tile_rejected = if setup.total_edge > 0.0 {
            e0.iter().copied().fold(f32::NEG_INFINITY, f32::max) < 0.0
            || e1.iter().copied().fold(f32::NEG_INFINITY, f32::max) < 0.0
            || e2.iter().copied().fold(f32::NEG_INFINITY, f32::max) < 0.0
        } else if setup.total_edge < 0.0 {
            e0.iter().copied().fold(f32::INFINITY, f32::min) > 0.0
            || e1.iter().copied().fold(f32::INFINITY, f32::min) > 0.0
            || e2.iter().copied().fold(f32::INFINITY, f32::min) > 0.0
        } else {
            // Degenerate (zero-area) triangle: per-pixel loop
            // already early-outs on total_edge == 0.
            true
        };
        if tile_rejected { continue; }

        for py in t_min_y..=t_max_y {
            for px in t_min_x..=t_max_x {
                let cx = px as f32 + 0.5;
                let cy = py as f32 + 0.5;
                let we0 = edge_fn(b, c, (cx, cy));
                let we1 = edge_fn(c, a, (cx, cy));
                let we2 = edge_fn(a, b, (cx, cy));
                let inside_pos = we0 >= 0.0 && we1 >= 0.0 && we2 >= 0.0;
                let inside_neg = we0 <= 0.0 && we1 <= 0.0 && we2 <= 0.0;
                let center_inside = inside_pos || inside_neg;
                if setup.total_edge == 0.0 { continue; }

                // MSAA coverage: with sample_count > 1, test N
                // sub-pixel sample points and accept the pixel
                // if ANY is covered; `coverage` is the covered
                // fraction, used to blend the fragment colour
                // with the destination (coverage-resolved
                // MSAA).  Single-sample keeps the exact
                // center-in/out test (coverage 1.0 or skip).
                let coverage: f32 = if draw.sample_count > 1 {
                    // Standard 4x sample offsets within the
                    // pixel (Vulkan-ish rotated grid), capped
                    // at 4 samples.
                    const OFFS: [(f32, f32); 4] = [
                        (0.375, 0.125), (0.875, 0.375),
                        (0.125, 0.625), (0.625, 0.875),
                    ];
                    let nsamp = (draw.sample_count as usize).min(4);
                    let mut covered = 0u32;
                    for &(ox, oy) in &OFFS[..nsamp] {
                        let sx = px as f32 + ox;
                        let sy = py as f32 + oy;
                        let s0 = edge_fn(b, c, (sx, sy));
                        let s1 = edge_fn(c, a, (sx, sy));
                        let s2 = edge_fn(a, b, (sx, sy));
                        let inside = (s0 >= 0.0 && s1 >= 0.0 && s2 >= 0.0)
                            || (s0 <= 0.0 && s1 <= 0.0 && s2 <= 0.0);
                        if inside { covered += 1; }
                    }
                    if covered == 0 { continue; }
                    covered as f32 / nsamp as f32
                } else {
                    if !center_inside { continue; }
                    1.0
                };

                let b0 = we0 / setup.total_edge;
                let b1 = we1 / setup.total_edge;
                let b2 = we2 / setup.total_edge;

                let interp_inv_w = b0 * setup.inv_w[0]
                    + b1 * setup.inv_w[1]
                    + b2 * setup.inv_w[2];
                let one_over_interp_inv_w = if interp_inv_w == 0.0 {
                    0.0
                } else { 1.0 / interp_inv_w };
                for k in 0..setup.n {
                    let sow = b0 * setup.varying_over_w[0][k]
                        + b1 * setup.varying_over_w[1][k]
                        + b2 * setup.varying_over_w[2][k];
                    let v = sow * one_over_interp_inv_w;
                    interp_buf[k*4 .. k*4 + 4]
                        .copy_from_slice(&v.to_le_bytes());
                }
                let interp_z = b0 * setup.ndc[0][2]
                    + b1 * setup.ndc[1][2]
                    + b2 * setup.ndc[2][2];
                // Apply the viewport depth-range remap.
                // Vulkan windowed depth = min_depth +
                // ndc.z * (max_depth - min_depth).  When the
                // caller didn't supply a viewport, the
                // identity range (0, 1) preserves the
                // pre-Rung-R behaviour.
                let window_z = setup.depth_min
                    + interp_z * (setup.depth_max - setup.depth_min)
                    + setup.depth_bias_offset;

                // Stripe-local indexing.
                let py_local = (py - stripe_pixel_y) as usize;
                let pixel_lin_local =
                    py_local * (setup.width as usize) + (px as usize);

                // ── Stencil + depth gates ────────────────
                // Per Vulkan spec the stencil test runs
                // first, then the depth bounds test, then
                // the depth compare.  The per-fragment
                // outcome is one of three (stencil fail,
                // depth fail, both pass) and selects the
                // matching face op.
                let stencil_old = task.stencil.as_deref()
                    .map(|s| s[pixel_lin_local]);
                let stencil_pass = if let (Some(face), Some(old)) =
                    (setup.stencil_face, stencil_old)
                {
                    let cmask = face.compare_mask;
                    let r = face.reference & cmask;
                    let b = old & cmask;
                    match face.compare_op {
                        CompareOp::Never          => false,
                        CompareOp::Less           => r <  b,
                        CompareOp::Equal          => r == b,
                        CompareOp::LessOrEqual    => r <= b,
                        CompareOp::Greater        => r >  b,
                        CompareOp::NotEqual       => r != b,
                        CompareOp::GreaterOrEqual => r >= b,
                        CompareOp::Always         => true,
                    }
                } else { true };

                // Depth compare + bounds (only meaningful if
                // a depth buffer is bound).  We compute the
                // pass result but defer the writeback until
                // we know the stencil op outcome too --
                // ordering matters because the stencil
                // depth_fail op fires when stencil passes
                // but depth doesn't.
                let depth_pass = if !stencil_pass {
                    // Vulkan spec: if stencil fails, the
                    // depth test is "not performed"; the
                    // depth-fail op is irrelevant.  Treat as
                    // pass for the outcome selector below
                    // so we don't accidentally route to
                    // depth_fail_op.
                    true
                } else if let Some(db) = task.depth.as_deref() {
                    if let Some((b_min, b_max)) = setup.depth_bounds {
                        let existing = db[pixel_lin_local];
                        if !(existing >= b_min && existing <= b_max) {
                            // Bounds-test failure is treated
                            // identically to a depth-compare
                            // failure for the stencil
                            // outcome selector.
                            false
                        } else {
                            depth_compare(window_z, db[pixel_lin_local],
                                          setup.depth_compare_op)
                        }
                    } else {
                        depth_compare(window_z, db[pixel_lin_local],
                                      setup.depth_compare_op)
                    }
                } else { true };

                // Stencil op selection + writeback.  Runs
                // regardless of whether the fragment will
                // produce colour -- stencil writes persist
                // even when the depth test fails.
                if let (Some(face), Some(stencil_buf), Some(old)) =
                    (setup.stencil_face, task.stencil.as_mut(), stencil_old)
                {
                    let op = if !stencil_pass {
                        face.fail_op
                    } else if !depth_pass {
                        face.depth_fail_op
                    } else {
                        face.pass_op
                    };
                    let new_val: u8 = match op {
                        StencilOp::Keep              => old,
                        StencilOp::Zero              => 0,
                        StencilOp::Replace           => face.reference,
                        StencilOp::IncrementAndClamp => old.saturating_add(1),
                        StencilOp::DecrementAndClamp => old.saturating_sub(1),
                        StencilOp::Invert            => !old,
                        StencilOp::IncrementAndWrap  => old.wrapping_add(1),
                        StencilOp::DecrementAndWrap  => old.wrapping_sub(1),
                    };
                    let written = (old & !face.write_mask)
                                | (new_val & face.write_mask);
                    stencil_buf[pixel_lin_local] = written;
                }

                // Now gate colour output on the combined
                // result.  Stencil fail OR depth fail =
                // discard.
                if !stencil_pass || !depth_pass { continue; }

                // Depth writeback (post-pass) gated on the
                // pipeline / dynamic `depth_write`.
                if let Some(db) = task.depth.as_mut() {
                    if setup.depth_write {
                        db[pixel_lin_local] = window_z;
                    }
                }

                // Implicit-LOD mip selection.  Finite-
                // difference the perspective-correct UV
                // varying (lanes 0,1) across the pixel quad
                // to get screen-space derivatives, scale by
                // the texel dims, take log2 of the larger
                // footprint as the LOD, round to a mip level,
                // and redirect the binding-0 descriptor's
                // tex_desc pointer to that mip in the per-
                // stripe uniforms copy.  Magnified textures
                // (LOD <= 0) keep mip 0, so 1:1 / upscaled
                // rungs are unchanged.
                if let Some((desc_off, mip_descs, mip_count, tw, th)) = lod_ctx {
                    // Perspective-correct (u, v) at an
                    // arbitrary screen point.
                    let uv_at = |x: f32, y: f32| -> (f32, f32) {
                        let g0 = edge_fn(b, c, (x, y)) / setup.total_edge;
                        let g1 = edge_fn(c, a, (x, y)) / setup.total_edge;
                        let g2 = edge_fn(a, b, (x, y)) / setup.total_edge;
                        let iw = g0 * setup.inv_w[0] + g1 * setup.inv_w[1]
                            + g2 * setup.inv_w[2];
                        let oiw = if iw == 0.0 { 0.0 } else { 1.0 / iw };
                        let uu = (g0 * setup.varying_over_w[0][0]
                            + g1 * setup.varying_over_w[1][0]
                            + g2 * setup.varying_over_w[2][0]) * oiw;
                        let vv = (g0 * setup.varying_over_w[0][1]
                            + g1 * setup.varying_over_w[1][1]
                            + g2 * setup.varying_over_w[2][1]) * oiw;
                        (uu, vv)
                    };
                    let (u0, v0) = uv_at(cx, cy);
                    let (ux, vx) = uv_at(cx + 1.0, cy);
                    let (uy, vy) = uv_at(cx, cy + 1.0);
                    let dudx = (ux - u0) * tw;
                    let dvdx = (vx - v0) * th;
                    let dudy = (uy - u0) * tw;
                    let dvdy = (vy - v0) * th;
                    let rho2 = (dudx * dudx + dvdx * dvdx)
                        .max(dudy * dudy + dvdy * dvdy);
                    // lod = log2(sqrt(rho2)) = 0.5*log2(rho2).
                    let lod = if rho2 > 0.0 { 0.5 * rho2.log2() } else { 0.0 };
                    let mip = (lod.round() as i32)
                        .clamp(0, (mip_count as i32) - 1) as usize;
                    // Redirect the descriptor's tex_desc ptr
                    // to mip_descs[mip] (mip 0 = base dup).
                    let chosen = unsafe { mip_descs.add(mip) } as u64;
                    uni_local[desc_off..desc_off + 8]
                        .copy_from_slice(&chosen.to_le_bytes());
                }

                // MRT: out_color holds 4 f32 per colour
                // attachment.  Cap at 8 attachments (Vulkan's
                // common max; spec-min is 4).  The FS writes
                // Location L at out_color[L*4..]; unwritten
                // slots stay 0.
                const MAX_COLOR_ATTACHMENTS: usize = 8;
                let n_color = (1 + task.extra_color.len())
                    .min(MAX_COLOR_ATTACHMENTS);
                let mut out_color = [0.0f32; MAX_COLOR_ATTACHMENTS * 4];
                let mut out_depth = 0.0f32;
                let in_varyings_ptr = if setup.n == 0 {
                    std::ptr::null()
                } else { interp_buf.as_ptr() };

                // 2x2-quad lockstep derivatives.  When the FS
                // uses dFdx/dFdy/fwidth, run a *probe* pass over
                // all four pixels of this pixel's quad: each lane
                // re-runs the FS with its own interpolated
                // varyings + frag_coord, and the runtime records
                // every derivative operand into a thread-local
                // QuadState keyed by op-site.  We then switch to
                // *final* mode and re-run for the real pixel
                // below, where `atrium_deriv` returns the
                // finite-difference between the recorded lanes.
                // Helper lanes (outside the triangle) still run
                // with extrapolated attributes, matching GPU quad
                // semantics.  Gated on `uses_derivatives` so the
                // hot path keeps exactly one FS call per pixel.
                if draw.uses_derivatives {
                    atrium_spv_runtime::quad_probe_begin();
                    let qx = px & !1;
                    let qy = py & !1;
                    let my_lane =
                        ((px & 1) + ((py & 1) << 1)) as usize;
                    for lane in 0..4usize {
                        let lx = qx + (lane as i32 & 1);
                        let ly = qy + ((lane as i32 >> 1) & 1);
                        let lcx = lx as f32 + 0.5;
                        let lcy = ly as f32 + 0.5;
                        let le0 = edge_fn(b, c, (lcx, lcy));
                        let le1 = edge_fn(c, a, (lcx, lcy));
                        let le2 = edge_fn(a, b, (lcx, lcy));
                        let lb0 = le0 / setup.total_edge;
                        let lb1 = le1 / setup.total_edge;
                        let lb2 = le2 / setup.total_edge;
                        let liw = lb0 * setup.inv_w[0]
                            + lb1 * setup.inv_w[1]
                            + lb2 * setup.inv_w[2];
                        let loiw = if liw == 0.0 { 0.0 }
                                   else { 1.0 / liw };
                        for k in 0..setup.n {
                            let sow = lb0 * setup.varying_over_w[0][k]
                                + lb1 * setup.varying_over_w[1][k]
                                + lb2 * setup.varying_over_w[2][k];
                            let v = sow * loiw;
                            probe_buf[k*4 .. k*4 + 4]
                                .copy_from_slice(&v.to_le_bytes());
                        }
                        let lz_ndc = lb0 * setup.ndc[0][2]
                            + lb1 * setup.ndc[1][2]
                            + lb2 * setup.ndc[2][2];
                        let lz = setup.depth_min
                            + lz_ndc
                                * (setup.depth_max - setup.depth_min)
                            + setup.depth_bias_offset;
                        let lane_vptr = if setup.n == 0 {
                            std::ptr::null()
                        } else { probe_buf.as_ptr() };
                        atrium_spv_runtime::quad_set_lane(lane);
                        // SAFETY: same contract as the final
                        // fs_main call below; outputs land in the
                        // scratch out_color / out_depth (discarded
                        // -- only the recorded operands matter).
                        unsafe {
                            fs_main(
                                lane_vptr, uni_ptr, pc_ptr,
                                lcx, lcy, lz, loiw,
                                0,
                                out_color.as_mut_ptr(),
                                &mut out_depth,
                                setup.front_facing as u32,
                            );
                        }
                    }
                    atrium_spv_runtime::quad_final_begin();
                    atrium_spv_runtime::quad_set_lane(my_lane);
                }
                // SAFETY: fs_main is a dlopened C-ABI
                // function whose signature was checked at
                // open time; all pointers are valid for the
                // lifetime of this call.  Disjoint per-stripe
                // pixel ownership means concurrent calls
                // from different tasks don't race.  out_color
                // is sized for the max attachment count, so
                // the FS's per-Location stores never run past
                // it.
                unsafe {
                    fs_main(
                        in_varyings_ptr, uni_ptr, pc_ptr,
                        cx, cy, window_z, one_over_interp_inv_w,
                        0,
                        out_color.as_mut_ptr(),
                        &mut out_depth,
                        setup.front_facing as u32,
                    );
                }
                if draw.uses_derivatives {
                    atrium_spv_runtime::quad_end();
                }
                let idx = pixel_lin_local * 4;

                // Scatter each colour attachment.  Attachment
                // 0 -> task.pixels; attachment k+1 ->
                // task.extra_color[k].  Each attachment uses
                // its own blend + write-mask: attachment 0 ->
                // `draw.blend_state`, attachment L ->
                // `draw.blend_extra[L-1]` (falling back to
                // attachment 0's state when the app didn't
                // supply a per-attachment entry).
                for slot in 0..n_color {
                    let src = [
                        out_color[slot * 4],
                        out_color[slot * 4 + 1],
                        out_color[slot * 4 + 2],
                        out_color[slot * 4 + 3],
                    ];
                    let target: &mut [u8] = if slot == 0 {
                        &mut task.pixels[..]
                    } else {
                        &mut task.extra_color[slot - 1][..]
                    };
                    if idx + 4 > target.len() { continue; }
                    let abs: &BlendState = if slot == 0 {
                        &draw.blend_state
                    } else {
                        draw.blend_extra.get(slot - 1).unwrap_or(&draw.blend_state)
                    };
                    let am = abs.write_mask;
                    let dst = [
                        target[idx]     as f32 / 255.0,
                        target[idx + 1] as f32 / 255.0,
                        target[idx + 2] as f32 / 255.0,
                        target[idx + 3] as f32 / 255.0,
                    ];
                    let mut final_color = if abs.enable {
                        apply_blend(abs, src, dst)
                    } else { src };
                    // MSAA coverage resolve: lerp toward the
                    // destination by (1 - coverage).  Interior
                    // pixels (coverage 1.0) are unchanged;
                    // edge pixels blend with what's already
                    // there (the clear / prior geometry).
                    if coverage < 1.0 {
                        for ch in 0..4 {
                            final_color[ch] = dst[ch] * (1.0 - coverage)
                                + final_color[ch] * coverage;
                        }
                    }
                    if am.r { target[idx]     = f32_to_u8(final_color[0]); }
                    if am.g { target[idx + 1] = f32_to_u8(final_color[1]); }
                    if am.b { target[idx + 2] = f32_to_u8(final_color[2]); }
                    if am.a { target[idx + 3] = f32_to_u8(final_color[3]); }
                }
            }
        }
    }
}

/// A clip-space vertex carried through Sutherland-Hodgman:
/// 4D homogeneous position + the same `varying_f32_count`
/// varying lanes that ride alongside through the clipper.
#[derive(Debug, Clone)]
struct ClipVertex {
    pos: [f32; 4],
    varyings: Vec<f32>,
}

/// Clip a convex polygon against a half-space defined by
/// `signed_distance >= 0`.  Sutherland-Hodgman: walks the
/// polygon edges; emits each inside vertex; at every
/// inside→outside or outside→inside transition emits an
/// interpolated vertex exactly on the plane.
///
/// `dist(v)` returns the signed perpendicular distance from
/// `v` to the plane (positive ⇒ inside half-space).  For
/// clip-space plane equations:
///   near plane (Vulkan):  dist = v.z
///   far  plane:           dist = v.w - v.z
///   left:                 dist = v.w + v.x
///   right:                dist = v.w - v.x
///   bottom:               dist = v.w + v.y
///   top:                  dist = v.w - v.y
///
/// The output polygon has 0..=2*input vertices.  Varyings
/// (and the position itself) interpolate linearly along the
/// cut edge — this is correct for clip-space coordinates
/// (the perspective divide happens AFTER clipping).
fn clip_polygon_plane<F>(
    poly: &[ClipVertex],
    dist: F,
) -> Vec<ClipVertex>
where F: Fn(&ClipVertex) -> f32 {
    if poly.is_empty() { return Vec::new(); }
    let mut out = Vec::with_capacity(poly.len() * 2);
    for i in 0..poly.len() {
        let curr = &poly[i];
        let next = &poly[(i + 1) % poly.len()];
        let d_curr = dist(curr);
        let d_next = dist(next);
        let curr_in = d_curr >= 0.0;
        let next_in = d_next >= 0.0;
        if curr_in {
            out.push(curr.clone());
        }
        if curr_in != next_in {
            // Edge crosses the plane.  Parameter t at which
            // the cut happens: 0 at curr, 1 at next.
            //   t = d_curr / (d_curr - d_next)
            // Division by (d_curr - d_next) is safe: one of
            // d_curr / d_next is strictly positive and the
            // other is strictly negative (otherwise the
            // curr_in != next_in test would have held false),
            // so the denominator is non-zero.
            let denom = d_curr - d_next;
            let t = if denom == 0.0 { 0.0 } else { d_curr / denom };
            let mut interp_pos = [0.0f32; 4];
            for k in 0..4 {
                interp_pos[k] = curr.pos[k] + t * (next.pos[k] - curr.pos[k]);
            }
            let interp_vary: Vec<f32> = (0..curr.varyings.len()).map(|k|
                curr.varyings[k] + t * (next.varyings[k] - curr.varyings[k])
            ).collect();
            out.push(ClipVertex { pos: interp_pos, varyings: interp_vary });
        }
    }
    out
}

/// Draw-call parameters for [`Tier2Registry::fill_image_triangle`].
///
/// Bundling these into a struct so future rasterizer phases
/// (R.3 depth, R.4 clipping, R.5 blending, ...) can grow
/// fields without breaking every caller's argument order.
#[derive(Debug, Clone, Copy)]
pub struct DrawTriangle<'a> {
    /// Per-vertex attribute buffers fed to the vertex shader.
    /// Layout is dictated by the VS's SPIR-V `Input`
    /// variables (typically a 12-byte `vec3` position at
    /// `Location=0`, contiguous f32 lanes for later
    /// locations).  Empty slices map to a null
    /// `in_attributes` parameter into `atrium_vs_main`.
    pub vertex_attrs: [&'a [u8]; 3],

    /// Per-vertex varying buffers consumed by the
    /// rasterizer's interpolator and fed to the FS as
    /// `in_varyings`.  See `fill_image_triangle`'s docstring
    /// for the temporary-scaffolding rationale (the
    /// backends' VS Output dispatch needs to learn the
    /// BuiltIn-vs-Location split before these can be
    /// captured from `out_varyings` directly).  Each slice
    /// must be exactly `varying_f32_count * 4` bytes long.
    pub varyings_per_vertex: [&'a [u8]; 3],

    /// Number of `f32` lanes in each per-vertex varying
    /// buffer.  The interpolator treats the buffer as that
    /// many contiguous little-endian `f32`s; the FS reads
    /// them via its `Input` `Location=N` decorations.
    pub varying_f32_count: usize,

    /// Uniform buffer shared across VS + FS invocations.
    /// Layout per the shaders' `Uniform`-storage
    /// declarations.  Empty → null.
    pub uniforms: &'a [u8],

    /// Push-constant buffer shared across VS + FS
    /// invocations.  Layout per the shaders' `PushConstant`
    /// declarations.  Empty → null.
    pub push_constants: &'a [u8],

    /// Per-attachment blend + colour-write state for colour
    /// attachment 0.  R.5.  Default is `enable: false`
    /// (source replace) + all-channels write mask.
    pub blend_state: BlendState,

    /// Blend state for MRT colour attachments 1..N
    /// (attachment 0 uses `blend_state`).  Empty ⇒ every
    /// extra attachment falls back to `blend_state` (the
    /// shared-state behaviour MRT v1 shipped with).
    pub blend_extra: &'a [BlendState],

    /// Viewport (`vkCmdSetViewport`) to apply during the
    /// NDC -> framebuffer-pixel mapping.  `None` falls back
    /// to a fullscreen viewport spanning the framebuffer
    /// (legacy R.1-R.7 behaviour).  When set, NDC.x in
    /// [-1, 1] maps to pixel range [vp.x, vp.x + vp.width)
    /// and NDC.y similarly to [vp.y, vp.y + vp.height).
    /// `min_depth` / `max_depth` are accepted for wire
    /// completeness but the depth-range remap is deferred
    /// (rasterizer writes raw NDC.z to the depth buffer for
    /// now).
    pub viewport: Option<Viewport>,

    /// Scissor (`vkCmdSetScissor`) to clip rasterised pixels
    /// against, in framebuffer pixel coordinates.  `None`
    /// falls back to a fullscreen scissor spanning the
    /// framebuffer.  Applied alongside the usual triangle-
    /// bbox clamp before the per-pixel walk.
    pub scissor: Option<Scissor>,

    /// Triangle cull mode (`VkCullModeFlags`).  Default
    /// `None` keeps every triangle (the pre-cull behaviour);
    /// `Back` / `Front` skip triangles whose screen-space
    /// winding identifies them as that face.
    pub cull_mode: CullMode,

    /// Winding considered front-facing (`VkFrontFace`).
    /// Defaults to `CounterClockwise` -- Vulkan's spec
    /// default and the convention the rasterizer's
    /// `edge_fn` uses (CCW screen winding ⇒ `total_edge > 0`).
    pub front_face: FrontFace,

    /// When `true`, fragments that pass the depth test
    /// overwrite the depth buffer (the usual default).
    /// When `false`, the depth test still gates colour
    /// output but the depth buffer is left untouched --
    /// matches Vulkan's `VkPipelineDepthStencilStateCreate
    /// Info::depthWriteEnable` when the test is on.  Has
    /// no effect when no depth buffer is bound (`depth ==
    /// None` on `fill_image_triangle`).
    pub depth_write: bool,

    /// Depth compare op (`VkCompareOp` / `Tier2CompareOp`).
    /// Defaults to `Less` for backward compatibility with
    /// the legacy hardcoded rasterizer behaviour.
    pub depth_compare_op: CompareOp,

    /// When `Some((min, max))`, the rasterizer additionally
    /// discards fragments whose destination depth-attachment
    /// value falls outside the inclusive range.  Mirrors
    /// Vulkan's depth bounds test
    /// (`depthBoundsTestEnable=true` + `minDepthBounds` /
    /// `maxDepthBounds`).  `None` means the test is off and
    /// no extra gate is applied.  Has no effect when no
    /// depth attachment is bound (the bounds test reads the
    /// existing buffer value).
    pub depth_bounds: Option<(f32, f32)>,

    /// When `Some(state)`, the rasterizer runs the stencil
    /// test before the depth test using the supplied
    /// per-face ops + compare rule.  `None` skips stencil
    /// entirely (legacy behaviour for rungs that pre-date
    /// stencil support).
    pub stencil: Option<StencilState>,

    /// MSAA rasterization sample count (1 = no MSAA).  When
    /// > 1, the rasterizer tests N sub-pixel sample points
    /// per pixel for coverage and blends the fragment colour
    /// with the destination by the covered fraction
    /// (coverage-resolved MSAA -- the correct resolved
    /// output for opaque single-triangle edges).
    pub sample_count: u32,

    /// When true, the rasterizer computes a per-pixel mip
    /// LOD from the screen-space gradient of the UV varying
    /// (lanes 0,1) and redirects the binding-0 texture
    /// descriptor to the selected mip level for that pixel
    /// (implicit-LOD sampling).  Only takes effect when a
    /// texture with `mip_count > 1` is bound; single-mip
    /// textures are untouched.  Set by the dispatcher when
    /// textures are bound and the VS emits >= 2 varying
    /// lanes (the UV).
    pub compute_implicit_lod: bool,

    /// When `Some((constant, clamp, slope))`, the rasterizer
    /// applies a polygon-offset to the interpolated depth
    /// before the depth test + write + FS frag_coord.z hand-
    /// off.  Mirror of Vulkan's
    /// `depthBiasConstantFactor` / `depthBiasClamp` /
    /// `depthBiasSlopeFactor` triple.  Has no effect when no
    /// depth attachment is bound or the bias triple
    /// evaluates to zero.
    pub depth_bias: Option<(f32, f32, f32)>,

    /// When true, the FS uses screen-space derivatives
    /// (`dFdx`/`dFdy`/`fwidth`).  The rasterizer shades each
    /// covered pixel in 2x2-quad lockstep: a probe pass runs
    /// the FS at all four quad-lane centres (recording each
    /// derivative operand into a thread-local `QuadState`), then
    /// a final pass runs the FS for the real pixel so the
    /// `atrium_deriv` helper returns finite-difference values.
    /// Requires a uniforms buffer carrying the helper table
    /// (the dispatcher guarantees one even with no textures).
    pub uses_derivatives: bool,

    /// Value handed to the vertex shader as `gl_InstanceIndex`
    /// (params[5]).  The dispatcher loops the draw once per
    /// instance (`firstInstance .. firstInstance +
    /// instanceCount`) and stamps this field so the VS can
    /// place each instance independently.  All instances read
    /// the same per-vertex attribute bytes (per-instance vertex
    /// input rate is a separate feature); per-instance variation
    /// comes entirely from the shader reading this index.
    pub instance_index: u32,
}

/// Per-face stencil state passed to `fill_image_triangle`.
/// Mirrors `aqueduct_gpu::Tier2StencilOpState` but uses
/// `CompareOp` and the daemon-local `StencilOp` to keep
/// the registry decoupled from the wire types.
#[derive(Debug, Clone, Copy)]
pub struct StencilFaceState {
    /// Op when the stencil test fails.
    pub fail_op: StencilOp,
    /// Op when both stencil + depth tests pass.
    pub pass_op: StencilOp,
    /// Op when the stencil test passes but depth fails.
    pub depth_fail_op: StencilOp,
    /// Compare rule for the stencil test.
    pub compare_op: CompareOp,
    /// AND mask applied to reference + buffer values before
    /// the stencil compare.
    pub compare_mask: u8,
    /// AND mask applied to the new stencil value before
    /// writing into the buffer.
    pub write_mask: u8,
    /// Reference value compared against the stencil buffer
    /// and substituted by the `Replace` op.
    pub reference: u8,
}

/// Stencil ops applied per-pixel based on the
/// (stencil pass) × (depth pass) outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StencilOp {
    /// Leave the stencil value unchanged.
    #[default]
    Keep,
    /// Set the stencil value to 0.
    Zero,
    /// Set the stencil value to the per-face `reference`.
    Replace,
    /// Increment with saturation at u8::MAX.
    IncrementAndClamp,
    /// Decrement with saturation at 0.
    DecrementAndClamp,
    /// Bitwise NOT.
    Invert,
    /// Increment with wrap-around past u8::MAX.
    IncrementAndWrap,
    /// Decrement with wrap-around past 0.
    DecrementAndWrap,
}

/// Per-draw stencil state passed via `DrawTriangle`.
#[derive(Debug, Clone, Copy)]
pub struct StencilState {
    /// Face state for triangles whose screen-space winding
    /// matches the active `FrontFace` rule.
    pub front: StencilFaceState,
    /// Face state for back-facing triangles.
    pub back: StencilFaceState,
}

impl Default for DrawTriangle<'_> {
    fn default() -> Self {
        DrawTriangle {
            vertex_attrs: [&[], &[], &[]],
            varyings_per_vertex: [&[], &[], &[]],
            varying_f32_count: 0,
            uniforms: &[],
            push_constants: &[],
            blend_state: BlendState::default(),
            blend_extra: &[],
            viewport: None,
            scissor:  None,
            cull_mode: CullMode::default(),
            front_face: FrontFace::default(),
            depth_write: true,
            depth_compare_op: CompareOp::default(),
            depth_bounds: None,
            stencil: None,
            depth_bias: None,
            compute_implicit_lod: false,
            sample_count: 1,
            uses_derivatives: false,
            instance_index: 0,
        }
    }
}

/// Triangle cull mode for the rasterizer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CullMode {
    /// No culling.
    #[default]
    None,
    /// Cull front-facing triangles.
    Front,
    /// Cull back-facing triangles.
    Back,
    /// Cull every triangle.
    FrontAndBack,
}

/// Depth compare op for the rasterizer (mirror of
/// `Tier2CompareOp` / Vulkan `VkCompareOp`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompareOp {
    /// Test never passes.
    Never,
    /// Pass when new < existing (Vulkan default).
    #[default]
    Less,
    /// Pass when new == existing.
    Equal,
    /// Pass when new <= existing.
    LessOrEqual,
    /// Pass when new > existing.
    Greater,
    /// Pass when new != existing.
    NotEqual,
    /// Pass when new >= existing.
    GreaterOrEqual,
    /// Test always passes.
    Always,
}

/// Front-face winding convention for the rasterizer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FrontFace {
    /// CCW winding (in screen-space pixel coordinates) is
    /// front (Vulkan default).
    #[default]
    CounterClockwise,
    /// CW winding is front.
    Clockwise,
}

/// Vulkan-shaped scissor rect, mirrored from the
/// `SetScissorCmd` wire body.  All coordinates are in
/// framebuffer pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scissor {
    /// Upper-left x in framebuffer pixels.
    pub x: u32,
    /// Upper-left y in framebuffer pixels.
    pub y: u32,
    /// Scissor width in pixels.
    pub width: u32,
    /// Scissor height in pixels.
    pub height: u32,
}

/// Vulkan-shaped viewport, mirrored from the
/// `SetViewportCmd` wire body.  Held in `DrawTriangle` so the
/// rasterizer can produce framebuffer-pixel coordinates
/// without re-deriving them from frame state.
#[derive(Debug, Clone, Copy, Default)]
pub struct Viewport {
    /// Upper-left x in framebuffer pixels.
    pub x: f32,
    /// Upper-left y in framebuffer pixels.
    pub y: f32,
    /// Viewport width in pixels.
    pub width: f32,
    /// Viewport height in pixels.
    pub height: f32,
    /// Minimum depth in the post-NDC depth range
    /// (accepted but not yet honoured by the rasterizer).
    pub min_depth: f32,
    /// Maximum depth in the post-NDC depth range
    /// (accepted but not yet honoured by the rasterizer).
    pub max_depth: f32,
}

/// Vulkan-shaped per-attachment colour-blend + write-mask
/// state.  Maps onto `VkPipelineColorBlendAttachmentState`
/// minus the rarely-used MIN/MAX/SUBTRACT blend ops (R.5 v1
/// supports ADD only).
#[derive(Debug, Clone, Copy)]
pub struct BlendState {
    /// When `false`, the source colour is written verbatim
    /// (mod the write mask).  The factor / op fields are
    /// ignored.  This is the default — R.1-R.4 behaviour.
    pub enable: bool,
    /// Colour-channel blend factors (RGB).
    pub color: BlendFactorPair,
    /// Alpha-channel blend factors (A).
    pub alpha: BlendFactorPair,
    /// Colour-channel blend op.  R.5 v1: ADD only.
    pub color_op: BlendOp,
    /// Alpha-channel blend op.  R.5 v1: ADD only.
    pub alpha_op: BlendOp,
    /// Per-channel write enable.  When a channel's flag is
    /// `false`, that byte of `pixels` is NOT touched by the
    /// draw.  Independent of blend enable.
    pub write_mask: ColorWriteMask,
}

impl Default for BlendState {
    fn default() -> Self {
        // Source-replace + all-channels write mask: the
        // implicit R.1-R.4 behaviour.
        Self {
            enable: false,
            color: BlendFactorPair { src: BlendFactor::One, dst: BlendFactor::Zero },
            alpha: BlendFactorPair { src: BlendFactor::One, dst: BlendFactor::Zero },
            color_op: BlendOp::Add,
            alpha_op: BlendOp::Add,
            write_mask: ColorWriteMask::ALL,
        }
    }
}

impl BlendState {
    /// Convenience constructor for the standard alpha-over
    /// (a.k.a. premultiplied-source-over after a multiply,
    /// or straight-alpha-over otherwise) compositing rule:
    ///   `result.rgb = src.rgb * src.a + dst.rgb * (1 - src.a)`
    ///   `result.a   = src.a + dst.a * (1 - src.a)`
    /// All-channels write mask.
    pub fn alpha_over() -> Self {
        Self {
            enable: true,
            color: BlendFactorPair {
                src: BlendFactor::SrcAlpha,
                dst: BlendFactor::OneMinusSrcAlpha,
            },
            alpha: BlendFactorPair {
                src: BlendFactor::One,
                dst: BlendFactor::OneMinusSrcAlpha,
            },
            color_op: BlendOp::Add,
            alpha_op: BlendOp::Add,
            write_mask: ColorWriteMask::ALL,
        }
    }
}

/// `(src_factor, dst_factor)` pair for one channel group
/// (colour or alpha).
#[derive(Debug, Clone, Copy)]
pub struct BlendFactorPair {
    /// Factor multiplied by the FS-output colour/alpha.
    pub src: BlendFactor,
    /// Factor multiplied by the existing-pixel colour/alpha.
    pub dst: BlendFactor,
}

/// Per-channel blend factor, matching the subset of Vulkan
/// `VkBlendFactor` values that aren't dual-source or
/// constant-colour (those land later if a real shader needs
/// them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendFactor {
    /// 0 (multiplies the operand away)
    Zero,
    /// 1 (multiplies the operand through unchanged)
    One,
    /// (src.r, src.g, src.b)  / src.a  (alpha-side)
    SrcColor,
    /// (1 - src.rgb)  / (1 - src.a)  (alpha-side)
    OneMinusSrcColor,
    /// (dst.r, dst.g, dst.b)  / dst.a  (alpha-side)
    DstColor,
    /// (1 - dst.rgb)  / (1 - dst.a)  (alpha-side)
    OneMinusDstColor,
    /// (src.a, src.a, src.a)  / src.a  (alpha-side)
    SrcAlpha,
    /// (1 - src.a) per channel
    OneMinusSrcAlpha,
    /// (dst.a, dst.a, dst.a)  / dst.a  (alpha-side)
    DstAlpha,
    /// (1 - dst.a) per channel
    OneMinusDstAlpha,
}

/// Blend equation: `result = factor_op(src*src_factor,
/// dst*dst_factor)`.  R.5 v1 supports `Add` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendOp {
    /// `a + b`
    Add,
}

/// Per-channel write enable.  When all 4 flags are `true`
/// (the default), the entire RGBA pixel is written; when a
/// flag is `false`, that channel's byte in `pixels` is left
/// untouched even when blending says it should change.
#[derive(Debug, Clone, Copy)]
pub struct ColorWriteMask {
    /// Write enable for the red channel.
    pub r: bool,
    /// Write enable for the green channel.
    pub g: bool,
    /// Write enable for the blue channel.
    pub b: bool,
    /// Write enable for the alpha channel.
    pub a: bool,
}

impl ColorWriteMask {
    /// All 4 channels writable (the default).
    pub const ALL: Self = Self { r: true, g: true, b: true, a: true };
    /// No channel writable (effectively skips colour write
    /// altogether — depth/stencil are still updated per
    /// their own flags).
    pub const NONE: Self = Self { r: false, g: false, b: false, a: false };
}

/// Evaluate a colour-side `BlendFactor` against the given
/// `src` and `dst` RGBA values.  Returns a 3-vector of
/// per-channel weights matching the Vulkan spec.
fn color_factor(f: BlendFactor, src: [f32; 4], dst: [f32; 4]) -> [f32; 3] {
    match f {
        BlendFactor::Zero =>
            [0.0; 3],
        BlendFactor::One =>
            [1.0; 3],
        BlendFactor::SrcColor =>
            [src[0], src[1], src[2]],
        BlendFactor::OneMinusSrcColor =>
            [1.0 - src[0], 1.0 - src[1], 1.0 - src[2]],
        BlendFactor::DstColor =>
            [dst[0], dst[1], dst[2]],
        BlendFactor::OneMinusDstColor =>
            [1.0 - dst[0], 1.0 - dst[1], 1.0 - dst[2]],
        BlendFactor::SrcAlpha =>
            [src[3]; 3],
        BlendFactor::OneMinusSrcAlpha =>
            [1.0 - src[3]; 3],
        BlendFactor::DstAlpha =>
            [dst[3]; 3],
        BlendFactor::OneMinusDstAlpha =>
            [1.0 - dst[3]; 3],
    }
}

/// Evaluate an alpha-side `BlendFactor`.  Returns a scalar
/// — Vulkan reduces colour-typed factors to their alpha
/// component when applied to the alpha channel.
fn alpha_factor(f: BlendFactor, src: [f32; 4], dst: [f32; 4]) -> f32 {
    match f {
        BlendFactor::Zero => 0.0,
        BlendFactor::One => 1.0,
        BlendFactor::SrcColor | BlendFactor::SrcAlpha => src[3],
        BlendFactor::OneMinusSrcColor | BlendFactor::OneMinusSrcAlpha =>
            1.0 - src[3],
        BlendFactor::DstColor | BlendFactor::DstAlpha => dst[3],
        BlendFactor::OneMinusDstColor | BlendFactor::OneMinusDstAlpha =>
            1.0 - dst[3],
    }
}

/// Apply a `BlendOp` to a pair of weighted operands.
fn blend_op(op: BlendOp, a: f32, b: f32) -> f32 {
    match op {
        BlendOp::Add => a + b,
    }
}

/// Full blend equation for one pixel.  `src` = FS output,
/// `dst` = existing pixel; returns the post-blend colour.
fn apply_blend(state: &BlendState, src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
    let sc = color_factor(state.color.src, src, dst);
    let dc = color_factor(state.color.dst, src, dst);
    let sa = alpha_factor(state.alpha.src, src, dst);
    let da = alpha_factor(state.alpha.dst, src, dst);
    [
        blend_op(state.color_op, sc[0] * src[0], dc[0] * dst[0]),
        blend_op(state.color_op, sc[1] * src[1], dc[1] * dst[1]),
        blend_op(state.color_op, sc[2] * src[2], dc[2] * dst[2]),
        blend_op(state.alpha_op, sa    * src[3], da    * dst[3]),
    ]
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
    /// A per-vertex varying buffer's byte length didn't
    /// match `varying_f32_count * 4`.
    #[error("varyings_per_vertex[{vertex}] length {got} != expected {expected}")]
    BadVaryingBufferLen {
        /// Which of the 3 vertex buffers was wrong.
        vertex: usize,
        /// Required length (`varying_f32_count * 4`).
        expected: usize,
        /// Caller-supplied length.
        got: usize,
    },
    /// The supplied depth buffer's length didn't match
    /// `width * height` (one `f32` per pixel).
    #[error("depth_buffer length {got} != expected {expected}")]
    BadDepthBufferLen {
        /// Required length (`width * height`).
        expected: usize,
        /// Caller-supplied length.
        got: usize,
    },
}
