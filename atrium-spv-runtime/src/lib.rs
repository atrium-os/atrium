//! Runtime helpers called from compiled tier-2 shaders.
//!
//! ## What this crate is
//!
//! The Atrium-Tier-2 software Vulkan renderer compiles
//! each shader to a position-independent native blob (see
//! `atrium-spv-backend-bespoke` + `atrium-spv-blob`).
//! Some IR ops — image sampling, image fetch — are too
//! heavy to inline in every fragment-shader function and
//! lower instead to *calls into this crate*, per the IR
//! `Op::ImageSample*` doc-comment in
//! `atrium-spv-ir`.
//!
//! ## ABI contract
//!
//! Descriptor structs are `#[repr(C)]` and the entry
//! points are `extern "C"`. A backend emits the standard
//! AAPCS64 call sequence (`bl <helper>`) and the loader
//! / JIT-emit blob path patches the helper's address into
//! a function-pointer slot in the blob's header. No deps:
//! the helpers are pure compute over raw byte buffers.
//!
//! ## What this crate is *not*
//!
//! Not a fast-path SIMD sampler. The point of this first
//! cut is correctness + a clean C-ABI for backends to
//! emit against. An inline-NEON bilinear sampler — the
//! "real" perf bar against `clang -O2` on a software
//! sampler — is a separate, later arc (the RUNBOOK
//! "texture/sampler" scoping marks it as a future
//! follow-on once this is wired through to the JIT-emit
//! path).

#![allow(clippy::missing_safety_doc)]

/// Texel formats the helpers understand. Stable wire-form
/// values (don't renumber) — a backend bakes these
/// constants into the loaded blob's descriptor table.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexFormat {
    /// 4×u8 unorm, channel order R,G,B,A.
    Rgba8Unorm = 0,
    /// 4×u8 unorm, channel order B,G,R,A. Matches the
    /// Atrium scanout buffer's hardcoded BGRA layout.
    Bgra8Unorm = 1,
    /// 1×u8 unorm, replicated to R; G=B=0, A=1.
    R8Unorm    = 2,
}

/// Sampler filter modes. Wire-form values are stable.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    Nearest = 0,
    Linear  = 1,
}

/// Texture-coordinate wrap modes at the [0,1] border.
/// Wire-form values are stable.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    /// Clamp to [0, width-1] / [0, height-1].
    ClampToEdge = 0,
    /// Modular wrap.
    Repeat      = 1,
    /// Triangle wave wrap (mirror at each boundary).
    Mirror      = 2,
}

/// A 2D image binding. `data` points at a row-major texel
/// buffer of `height` rows, each `stride_bytes` long; the
/// pixel format determines bytes-per-texel within a row.
/// The shader sees this as a `texture2D` / `image2D`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TexDesc {
    pub data:         *const u8,
    pub width:        u32,
    pub height:       u32,
    pub stride_bytes: u32,
    /// `TexFormat` as `u32` for C-ABI portability.
    pub format:       u32,
}

/// A sampler binding. Independent of any specific image,
/// per the Vulkan combined-image-sampler model.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SamplerDesc {
    /// `FilterMode` as `u32`.
    pub mag_filter: u32,
    /// `FilterMode` as `u32`.
    pub min_filter: u32,
    /// `WrapMode` as `u32`.
    pub wrap_s:     u32,
    /// `WrapMode` as `u32`.
    pub wrap_t:     u32,
}

// ── Helpers ───────────────────────────────────────────────

/// Sample a 2D image at normalised UV coordinates with
/// implicit LOD (LOD computation deferred — this v1
/// always samples mip 0). Writes RGBA32F into
/// `out_rgba[0..4]`.
///
/// # Safety
/// * `tex` and `samp` must be valid pointers.
/// * `tex.data` must point at `>= tex.height * tex.stride_bytes`
///   readable bytes.
/// * `out_rgba` must point at `>= 16` writable bytes.
/// * `tex.format` must be a valid `TexFormat` discriminant.
#[no_mangle]
pub unsafe extern "C" fn atrium_tex_sample_2d(
    tex: *const TexDesc,
    samp: *const SamplerDesc,
    u: f32, v: f32,
    out_rgba: *mut f32,
) {
    let t = &*tex;
    let s = &*samp;
    let rgba = sample_2d_impl(t, s, u, v);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// Fetch a single texel by integer coordinates (no
/// filtering, no wrap — the caller is responsible for
/// keeping `(x, y)` in range). `lod` is ignored in v1.
///
/// # Safety
/// As for `atrium_tex_sample_2d`. Additionally, `x` and
/// `y` must be in `[0, tex.width)` × `[0, tex.height)`.
#[no_mangle]
pub unsafe extern "C" fn atrium_tex_fetch_2d(
    tex: *const TexDesc,
    x: i32, y: i32, _lod: i32,
    out_rgba: *mut f32,
) {
    let t = &*tex;
    let rgba = fetch_texel_impl(t, x as u32, y as u32);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

// ── Implementation (safe Rust, called from the FFI wrappers) ──

fn sample_2d_impl(t: &TexDesc, s: &SamplerDesc, u: f32, v: f32) -> [f32; 4] {
    // The "implicit LOD" decision (mag vs min filter) needs
    // fragment derivatives. v1 doesn't expose them yet, so
    // we always pick mag_filter — the common case for the
    // 1:1-mapped pixel passes the renderer is built for.
    let filter = filter_from_u32(s.mag_filter);
    let wrap_s = wrap_from_u32(s.wrap_s);
    let wrap_t = wrap_from_u32(s.wrap_t);
    // Map normalised UV → texel space. Vulkan/SPIR-V
    // convention: `(0,0)` is the top-left of texel
    // `(0,0)`'s top-left corner, `(1,1)` the bottom-right
    // of the last texel — so sample centres sit at
    // `(i+0.5, j+0.5)` and the continuous coordinate is
    // `u*w - 0.5`.
    let x = u * t.width as f32 - 0.5;
    let y = v * t.height as f32 - 0.5;
    match filter {
        FilterMode::Nearest => {
            let xi = x.round() as i32;
            let yi = y.round() as i32;
            let (xi, yi) = (
                apply_wrap(xi, t.width as i32, wrap_s),
                apply_wrap(yi, t.height as i32, wrap_t),
            );
            fetch_texel_impl(t, xi as u32, yi as u32)
        }
        FilterMode::Linear => {
            // Bilinear: 4 texel taps + bilerp.
            let x0 = x.floor() as i32;
            let y0 = y.floor() as i32;
            let fx = x - x0 as f32;
            let fy = y - y0 as f32;
            let x1 = x0 + 1;
            let y1 = y0 + 1;
            let x0w = apply_wrap(x0, t.width as i32, wrap_s) as u32;
            let x1w = apply_wrap(x1, t.width as i32, wrap_s) as u32;
            let y0w = apply_wrap(y0, t.height as i32, wrap_t) as u32;
            let y1w = apply_wrap(y1, t.height as i32, wrap_t) as u32;
            let t00 = fetch_texel_impl(t, x0w, y0w);
            let t10 = fetch_texel_impl(t, x1w, y0w);
            let t01 = fetch_texel_impl(t, x0w, y1w);
            let t11 = fetch_texel_impl(t, x1w, y1w);
            let mut out = [0.0f32; 4];
            for k in 0..4 {
                let top = t00[k] * (1.0 - fx) + t10[k] * fx;
                let bot = t01[k] * (1.0 - fx) + t11[k] * fx;
                out[k] = top * (1.0 - fy) + bot * fy;
            }
            out
        }
    }
}

fn fetch_texel_impl(t: &TexDesc, x: u32, y: u32) -> [f32; 4] {
    // Caller-clamped: `apply_wrap` already brought (x, y)
    // into [0, w) × [0, h). We treat any out-of-range
    // remnant as edge (defensive — better than UB).
    let xc = x.min(t.width.saturating_sub(1));
    let yc = y.min(t.height.saturating_sub(1));
    let row_off = yc as usize * t.stride_bytes as usize;
    let fmt = format_from_u32(t.format);
    unsafe {
        let row_ptr = t.data.add(row_off);
        match fmt {
            TexFormat::Rgba8Unorm => {
                let px_ptr = row_ptr.add(xc as usize * 4);
                [
                    u8_to_unorm(*px_ptr.add(0)),
                    u8_to_unorm(*px_ptr.add(1)),
                    u8_to_unorm(*px_ptr.add(2)),
                    u8_to_unorm(*px_ptr.add(3)),
                ]
            }
            TexFormat::Bgra8Unorm => {
                let px_ptr = row_ptr.add(xc as usize * 4);
                [
                    u8_to_unorm(*px_ptr.add(2)), // R from byte 2
                    u8_to_unorm(*px_ptr.add(1)), // G
                    u8_to_unorm(*px_ptr.add(0)), // B from byte 0
                    u8_to_unorm(*px_ptr.add(3)), // A
                ]
            }
            TexFormat::R8Unorm => {
                let r = u8_to_unorm(*row_ptr.add(xc as usize));
                [r, 0.0, 0.0, 1.0]
            }
        }
    }
}

#[inline] fn u8_to_unorm(b: u8) -> f32 { b as f32 / 255.0 }

#[inline]
fn apply_wrap(c: i32, n: i32, mode: WrapMode) -> i32 {
    match mode {
        WrapMode::ClampToEdge => c.clamp(0, n - 1),
        WrapMode::Repeat => {
            // Rust's `%` follows the dividend's sign; we
            // want Euclidean mod so negatives wrap forward.
            ((c % n) + n) % n
        }
        WrapMode::Mirror => {
            // Triangle wave with period 2n.
            let period = 2 * n;
            let m = ((c % period) + period) % period;
            if m < n { m } else { period - 1 - m }
        }
    }
}

#[inline]
fn format_from_u32(v: u32) -> TexFormat {
    match v {
        0 => TexFormat::Rgba8Unorm,
        1 => TexFormat::Bgra8Unorm,
        2 => TexFormat::R8Unorm,
        // Defensive — a malformed descriptor falls back
        // to a recognisable garbage value rather than UB.
        _ => TexFormat::Rgba8Unorm,
    }
}

#[inline]
fn filter_from_u32(v: u32) -> FilterMode {
    if v == 1 { FilterMode::Linear } else { FilterMode::Nearest }
}

#[inline]
fn wrap_from_u32(v: u32) -> WrapMode {
    match v {
        1 => WrapMode::Repeat,
        2 => WrapMode::Mirror,
        _ => WrapMode::ClampToEdge,
    }
}

// ── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 checkerboard, RGBA8 unorm: red, green / blue, white.
    fn checker() -> (Vec<u8>, TexDesc) {
        // Row-major, 4 bytes per pixel, stride = 2*4 = 8.
        let pixels: Vec<u8> = vec![
            255,   0,   0, 255,   // (0,0) red
              0, 255,   0, 255,   // (1,0) green
              0,   0, 255, 255,   // (0,1) blue
            255, 255, 255, 255,   // (1,1) white
        ];
        let desc = TexDesc {
            data: pixels.as_ptr(),
            width: 2, height: 2, stride_bytes: 8,
            format: TexFormat::Rgba8Unorm as u32,
        };
        (pixels, desc)
    }

    #[test]
    fn fetch_corners() {
        let (_pixels, desc) = checker();
        // 0,0 → red
        let p = fetch_texel_impl(&desc, 0, 0);
        assert!((p[0] - 1.0).abs() < 1e-6 && p[1] == 0.0 && p[2] == 0.0);
        // 1,1 → white
        let p = fetch_texel_impl(&desc, 1, 1);
        for k in 0..4 { assert!((p[k] - 1.0).abs() < 1e-6); }
    }

    #[test]
    fn nearest_sample_at_centre_is_exact_texel() {
        let (_pixels, desc) = checker();
        let samp = SamplerDesc {
            mag_filter: FilterMode::Nearest as u32,
            min_filter: FilterMode::Nearest as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        };
        // Centre of texel (0,0) sits at u=0.25, v=0.25
        // (texel size 0.5 in normalised coords on a 2x2).
        let p = sample_2d_impl(&desc, &samp, 0.25, 0.25);
        assert!((p[0] - 1.0).abs() < 1e-6); // red
        // Centre of (1,1) at u=0.75, v=0.75 → white.
        let p = sample_2d_impl(&desc, &samp, 0.75, 0.75);
        for k in 0..4 { assert!((p[k] - 1.0).abs() < 1e-6); }
    }

    #[test]
    fn bilinear_at_geometric_centre_averages_four() {
        let (_pixels, desc) = checker();
        let samp = SamplerDesc {
            mag_filter: FilterMode::Linear as u32,
            min_filter: FilterMode::Linear as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        };
        // u=v=0.5 lands exactly at the four-texel meeting
        // point: x = 0.5*2 - 0.5 = 0.5 → fx=0.5, x0=0,x1=1.
        // Same for y. Output should be the equal-weight
        // average of (red, green, blue, white).
        let p = sample_2d_impl(&desc, &samp, 0.5, 0.5);
        // avg R = (1+0+0+1)/4 = 0.5; G = (0+1+0+1)/4 = 0.5;
        // B = (0+0+1+1)/4 = 0.5; A = 1.
        for k in 0..3 { assert!((p[k] - 0.5).abs() < 1e-6,
            "lane {k}: got {}", p[k]); }
        assert!((p[3] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bgra_decodes_swapped() {
        // Same pixels as `checker` but the descriptor says
        // BGRA — fetch should swap R/B.
        let pixels: Vec<u8> = vec![
            255,   0,   0, 255,   // (0,0): BGRA → R from byte2=0, B from byte0=255 → blue
              0, 255,   0, 255,
              0,   0, 255, 255,   // (0,1): BGRA → R=255, B=0 → red
            255, 255, 255, 255,
        ];
        let desc = TexDesc {
            data: pixels.as_ptr(),
            width: 2, height: 2, stride_bytes: 8,
            format: TexFormat::Bgra8Unorm as u32,
        };
        let p0 = fetch_texel_impl(&desc, 0, 0);
        assert!(p0[0] == 0.0 && p0[2] == 1.0, "BGRA swap (0,0): {p0:?}");
        let p2 = fetch_texel_impl(&desc, 0, 1);
        assert!((p2[0] - 1.0).abs() < 1e-6 && p2[2] == 0.0,
                "BGRA swap (0,1): {p2:?}");
    }

    #[test]
    fn wrap_repeat() {
        // c=-1 with n=4, repeat → 3.
        assert_eq!(apply_wrap(-1, 4, WrapMode::Repeat), 3);
        // c=5 with n=4, repeat → 1.
        assert_eq!(apply_wrap(5, 4, WrapMode::Repeat), 1);
    }

    #[test]
    fn wrap_clamp() {
        assert_eq!(apply_wrap(-3, 4, WrapMode::ClampToEdge), 0);
        assert_eq!(apply_wrap(99, 4, WrapMode::ClampToEdge), 3);
    }

    #[test]
    fn wrap_mirror() {
        // n=4: indices flow 0,1,2,3,3,2,1,0,0,1,2,3,...
        // for c = 0..8 (one full mirror period).
        let n = 4;
        let expected = [0, 1, 2, 3, 3, 2, 1, 0];
        for (c, &e) in expected.iter().enumerate() {
            assert_eq!(apply_wrap(c as i32, n, WrapMode::Mirror), e,
                       "c={c}");
        }
        // And negative side: c=-1 → 0, c=-4 → 3, c=-5 → 3.
        assert_eq!(apply_wrap(-1, n, WrapMode::Mirror), 0);
        assert_eq!(apply_wrap(-4, n, WrapMode::Mirror), 3);
    }

    #[test]
    fn r8_replicates_to_red_alpha_one() {
        let pixels: Vec<u8> = vec![128, 200, 50, 255];
        let desc = TexDesc {
            data: pixels.as_ptr(),
            width: 4, height: 1, stride_bytes: 4,
            format: TexFormat::R8Unorm as u32,
        };
        let p = fetch_texel_impl(&desc, 1, 0);
        assert!((p[0] - 200.0 / 255.0).abs() < 1e-6);
        assert!(p[1] == 0.0 && p[2] == 0.0);
        assert!((p[3] - 1.0).abs() < 1e-6);
    }
}
