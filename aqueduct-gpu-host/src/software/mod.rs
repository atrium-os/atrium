//! Tier-1 software renderer: tiny-skia rasterisation of
//! Atrium-native bundle operations.
//!
//! Architecture per `docs/spec/aqueduct-gpu.md` §6.5:
//!
//! - Frame command streams arrive as packed `FrameOp` records via
//!   `Backend::submit_frame`.
//! - This module decodes them and dispatches each to a
//!   tiny-skia-based rasteriser.
//! - **No SPIR-V or NIR is interpreted.** The renderer has
//!   hand-coded paths per Atrium-native pipeline (atrium-core's
//!   rect / texture / path; atrium-text's glyph_run). Unknown
//!   pipeline_ids return a rasteriser error that the session
//!   surfaces as `OP_GPU_VALIDATION_ERR`.
//!
//! ## Built-in pipeline IDs
//!
//! Atrium-shipped bundles live in the `IdNamespace::Builtin`
//! namespace (top-4-bit tag 0x0). Tier-1 recognises the following
//! local-IDs by convention; see `aqueduct-core/manifest.json` and
//! `aqueduct-text/manifest.json` for the wire-level declarations.
//!
//! | local-ID | bundle / op       | shader (CPU equivalent)      |
//! |----------|-------------------|------------------------------|
//! | `0x0001` | atrium-core/rect  | fill_rect with push-constants |
//! | `0x0002` | atrium-core/textured-rect | fill_rect with texture sampler |
//! | `0x0003` | atrium-core/path  | fill_path with push-constants |
//! | `0x0010` | atrium-text/glyph_run | composite glyph atlas rects |
//!
//! Future built-ins extend this table; tier-2 (general SW Vulkan)
//! would handle the open-ended case via llvmpipe, deferred.

pub mod renderer;

pub use renderer::{
    BeginRenderPassBody, GlyphInstance, GlyphRunParams, PathCommand,
    PathFillRule, PathOpParams, RectOpParams, RendererError,
    TexturedRectOpParams, TinySkiaRenderer,
};

// Built-in pipeline local-IDs the renderer recognises.

/// Atrium-core's rect op.
pub const BUILTIN_PIPELINE_RECT:           u32 = 0x0001;
/// Atrium-core's textured-rect op.
pub const BUILTIN_PIPELINE_TEXTURED_RECT:  u32 = 0x0002;
/// Atrium-core's path op.
pub const BUILTIN_PIPELINE_PATH:           u32 = 0x0003;
/// Atrium-text's glyph_run op.
pub const BUILTIN_PIPELINE_GLYPH_RUN:      u32 = 0x0010;
