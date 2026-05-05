//! Server-side rendering helpers reused by the new envelope stack:
//! font loading + glyph metrics + path tessellation. The legacy
//! GpuBackend abstraction + tiny-skia software rasterizer were
//! excised at the M2.7e cutover; the production rasterizer lives in
//! `fresco-vulkan` (HeadlessRenderer + SPIR-V bundle dispatch) and is
//! driven by `frescod` directly, not through this crate.

pub mod font;
pub mod metrics;
pub mod tessellate;
