//! `fresco-aqueduct-bridge` — translate fresco-protocol scene nodes
//! into aqueduct-gpu frame command streams.
//!
//! Frescod historically owns a Vulkan renderer (via `fresco-vulkan`
//! and venus passthrough). Phase 1.4 of the aqueduct-gpu rollout
//! replaces that renderer with: build a frame command stream of
//! `FrameOp` records and ship it over aqueduct-gpu's wire. The host
//! (tier-1 SW backend today, tier-3 HW backend on D5+) rasterises.
//!
//! This crate is the **translation layer** — pure functions that
//! take a `fresco_protocol::*Params` value plus a few resource-id
//! handles, and append the equivalent FrameOp records to an
//! `aqueduct_gpu::FrameBuilder`.
//!
//! ## Scope
//!
//! - Rect → `BUILTIN_PIPELINE_RECT` + `RectOpParams`
//! - Path (rotated quad) → `BUILTIN_PIPELINE_PATH` + `PathOpParams`
//! - Texture → `BUILTIN_PIPELINE_TEXTURED_RECT` + `TexturedRectOpParams`
//! - GlyphRun → `BUILTIN_PIPELINE_GLYPH_RUN` + `GlyphRunParams`
//!
//! ## What this crate does NOT do
//!
//! - Walk a fresco scene-graph tree. Frescod still owns traversal /
//!   z-order / visibility; this crate translates one node at a time.
//! - Manage slot-id → image-id mapping. Frescod's slot table is the
//!   source of truth; callers pass already-resolved aqueduct-gpu
//!   resource ids in.
//! - Open or own a wire connection. The caller drives
//!   `aqueduct-gpu-client` directly; this crate only emits records
//!   into the caller's `FrameBuilder`.
//!
//! ## Coordinate convention
//!
//! fresco-protocol coordinates are screen-pixel space with top-left
//! origin. Atrium-native tier-1 rasterisation matches. No flip.
//!
//! ## Wire-format crosswalk
//!
//! The two crates carry near-identical types (e.g. both have a
//! `GlyphRunParams`). They are intentionally distinct: fresco's
//! version is part of the **consumer** protocol (one display server
//! talking to many clients over fresco-protocol); aqueduct-gpu's is
//! part of the **GPU** protocol (frescod talking to the GPU host).
//! The bridge re-shapes one into the other. Future protocol drift
//! is independent on each side.

#![warn(missing_docs)]

use aqueduct_gpu::frame::FrameBuilder;
use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu::opcodes::FrameOp;
use fresco_protocol as fp;

// Built-in pipeline local-IDs — must match aqueduct-gpu-host's
// `software::BUILTIN_PIPELINE_*` constants. Duplicated here so this
// crate doesn't take a build-time dep on the host implementation
// (the wire IDs are the API surface).
const BUILTIN_PIPELINE_RECT:          u32 = 0x0001;
const BUILTIN_PIPELINE_TEXTURED_RECT: u32 = 0x0002;
const BUILTIN_PIPELINE_PATH:          u32 = 0x0003;
const BUILTIN_PIPELINE_GLYPH_RUN:     u32 = 0x0010;

/// Errors a translator function can hit. All currently come from
/// the FrameBuilder running out of buffer space.
#[derive(Debug)]
pub enum BridgeError {
    /// The frame builder rejected a record — usually because the
    /// `max_frame_bytes` cap was exceeded. The caller should split
    /// the scene across multiple frames.
    BuilderFull(aqueduct_gpu::frame::FrameDecodeError),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::BuilderFull(e) => write!(f, "frame builder full: {e}"),
        }
    }
}
impl std::error::Error for BridgeError {}

impl From<aqueduct_gpu::frame::FrameDecodeError> for BridgeError {
    fn from(e: aqueduct_gpu::frame::FrameDecodeError) -> Self {
        BridgeError::BuilderFull(e)
    }
}

/// Emit a `BIND_PIPELINE` for a built-in local-ID. Helper used by
/// every translator below — and exposed so callers that hand-roll
/// frame streams (e.g. compositor-internal optimisations) can
/// reuse the encoding.
pub fn push_bind_builtin_pipeline(
    fb: &mut FrameBuilder,
    builtin_local_id: u32,
) -> Result<(), BridgeError> {
    let pipe = ResourceId::new(IdNamespace::Builtin, builtin_local_id);
    fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes())?;
    Ok(())
}

/// Push-constants wire shape: 4-byte stage_mask/offset/reserved
/// header, then inline bytes. Matches the renderer's expectations
/// (see `aqueduct-gpu-host` software renderer
/// `handle_push_constants`).
fn push_constants(fb: &mut FrameBuilder, body: &[u8]) -> Result<(), BridgeError> {
    let mut pc = Vec::with_capacity(4 + body.len());
    pc.extend_from_slice(&[0u8; 4]); // stage_mask:u8 offset:u8 reserved:u16
    pc.extend_from_slice(body);
    fb.push(FrameOp::PushConstants, &pc)?;
    Ok(())
}

/// Append a draw record. Body bytes are reserved for future
/// vertex-count / instance-count fields; tier-1 ignores them.
fn push_draw(fb: &mut FrameBuilder) -> Result<(), BridgeError> {
    fb.push(FrameOp::Draw, &[0u8; 16])?;
    Ok(())
}

/// Translate a fresco `RectParams` → BindPipeline + PushConstants +
/// Draw triplet. Colour is straight RGBA in [0,1]; tier-1 takes it
/// as-is (no premultiply step — the rasteriser clamps and stores
/// premultiplied internally via tiny-skia).
pub fn translate_rect(
    fb: &mut FrameBuilder,
    p: &fp::RectParams,
) -> Result<(), BridgeError> {
    push_bind_builtin_pipeline(fb, BUILTIN_PIPELINE_RECT)?;
    // Re-encode into aqueduct-gpu's RectOpParams wire shape (32 B).
    let mut body = [0u8; 32];
    let vals = [p.x, p.y, p.w, p.h, p.r, p.g, p.b, p.a];
    for (i, v) in vals.iter().enumerate() {
        body[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    push_constants(fb, &body)?;
    push_draw(fb)?;
    Ok(())
}

/// Translate a fresco `TextureParams` → textured-rect dispatch.
/// `atlas_image_id` is the aqueduct-gpu `ResourceId` that frescod's
/// slot table resolved from `p.slot_id`. UVs default to the full
/// atlas — fresco's `TextureParams` doesn't carry a sub-rect (the
/// per-glyph path uses GlyphRun for that).
pub fn translate_texture(
    fb: &mut FrameBuilder,
    p: &fp::TextureParams,
    atlas_image_id: ResourceId,
    atlas_w: u32,
    atlas_h: u32,
) -> Result<(), BridgeError> {
    push_bind_builtin_pipeline(fb, BUILTIN_PIPELINE_TEXTURED_RECT)?;
    let mut body = [0u8; 52];
    let dst = [p.x, p.y, p.w, p.h];
    for (i, v) in dst.iter().enumerate() {
        body[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
    }
    body[16..20].copy_from_slice(&atlas_image_id.raw().to_le_bytes());
    let uvs = [0.0_f32, 0.0, atlas_w as f32, atlas_h as f32,
               1.0, 1.0, 1.0, 1.0];
    for (i, v) in uvs.iter().enumerate() {
        let off = 20 + i*4;
        body[off..off+4].copy_from_slice(&v.to_le_bytes());
    }
    push_constants(fb, &body)?;
    push_draw(fb)?;
    Ok(())
}

/// Translate a fresco `PathParams` (rotated coloured quad) → path
/// pipeline dispatch.
///
/// Builds a 4-vertex rotated rect as MoveTo + 3× LineTo + Close.
/// `PathParams` describes a quad centred at `(cx, cy)` with
/// `length × width` extent, rotated by `angle` radians CCW.
pub fn translate_path(
    fb: &mut FrameBuilder,
    p: &fp::PathParams,
) -> Result<(), BridgeError> {
    push_bind_builtin_pipeline(fb, BUILTIN_PIPELINE_PATH)?;

    // Build the four rotated corners.
    let hx = p.length * 0.5;
    let hy = p.width  * 0.5;
    let (s, c) = (p.angle.sin(), p.angle.cos());
    let rot = |lx: f32, ly: f32| -> (f32, f32) {
        (p.cx + lx * c - ly * s, p.cy + lx * s + ly * c)
    };
    let (x0, y0) = rot(-hx, -hy);
    let (x1, y1) = rot( hx, -hy);
    let (x2, y2) = rot( hx,  hy);
    let (x3, y3) = rot(-hx,  hy);

    // Encode PathOpParams wire shape (header = 28 bytes):
    //   0..16   color: 4×f32
    //   16      anti_alias: u8
    //   17      fill_rule:  u8 (0 = Winding, 1 = EvenOdd)
    //   18..20  reserved u16
    //   20..24  command_count: u32
    //   24..28  reserved u32
    //   28..    verb-tagged command stream
    let mut body: Vec<u8> = Vec::with_capacity(28 + 4*(1+8) + 1);
    for v in [p.r, p.g, p.b, p.a] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body.push(1u8);  // anti_alias on for rotated edges
    body.push(0u8);  // winding fill
    body.extend_from_slice(&[0u8; 2]); // reserved u16
    let cmd_count: u32 = 5; // MoveTo + 3× LineTo + Close
    body.extend_from_slice(&cmd_count.to_le_bytes());
    body.extend_from_slice(&[0u8; 4]); // reserved u32

    // Verb encoding (matches PathCommand in renderer):
    //   0x00 MoveTo (x,y)
    //   0x01 LineTo (x,y)
    //   0x04 Close
    let push_xy = |verb: u8, x: f32, y: f32, body: &mut Vec<u8>| {
        body.push(verb);
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
    };
    push_xy(0x00, x0, y0, &mut body);
    push_xy(0x01, x1, y1, &mut body);
    push_xy(0x01, x2, y2, &mut body);
    push_xy(0x01, x3, y3, &mut body);
    body.push(0x04); // Close (no operands)

    push_constants(fb, &body)?;
    push_draw(fb)?;
    Ok(())
}

/// Translate a fresco `GlyphRunParams` → glyph_run dispatch.
/// `atlas_image_id` is frescod's resolved atlas image. The fresco
/// per-glyph `bearing_x`/`bearing_y` are folded into the aqueduct
/// `dx`/`dy` here so the renderer doesn't need to know about font
/// metrics; the renderer just stamps quads.
pub fn translate_glyph_run(
    fb: &mut FrameBuilder,
    p: &fp::GlyphRunParams,
    atlas_image_id: ResourceId,
) -> Result<(), BridgeError> {
    push_bind_builtin_pipeline(fb, BUILTIN_PIPELINE_GLYPH_RUN)?;

    let header_sz = 32usize;
    let instance_sz = 24usize;
    let mut body = Vec::with_capacity(header_sz + p.glyphs.len() * instance_sz);

    // Header: color (16) + atlas_image_id (4) + glyph_count (4) +
    // origin (8).
    for v in [p.r, p.g, p.b, p.a] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body.extend_from_slice(&atlas_image_id.raw().to_le_bytes());
    body.extend_from_slice(&(p.glyphs.len() as u32).to_le_bytes());
    body.extend_from_slice(&p.x.to_le_bytes());
    body.extend_from_slice(&p.y.to_le_bytes());

    // Per-glyph: fresco's dx/dy are pen offsets; bearing_x/y adjust
    // the drawn rect relative to that. tier-1 just wants the final
    // (dx, dy) of the destination rect, so fold them here.
    for g in &p.glyphs {
        let dx = g.dx + g.bearing_x;
        let dy = g.dy - g.bearing_y;
        body.extend_from_slice(&dx.to_le_bytes());
        body.extend_from_slice(&dy.to_le_bytes());
        body.extend_from_slice(&g.atlas_u.to_le_bytes());
        body.extend_from_slice(&g.atlas_v.to_le_bytes());
        body.extend_from_slice(&g.atlas_w.to_le_bytes());
        body.extend_from_slice(&g.atlas_h.to_le_bytes());
    }

    push_constants(fb, &body)?;
    push_draw(fb)?;
    Ok(())
}

/// Flag bits for `begin_renderpass_with_flags`. Mirrors
/// `aqueduct_gpu_host::software::BEGIN_RP_FLAG_*`.
///
/// `BEGIN_RP_FLAG_NO_CLEAR` skips the framebuffer-clear at
/// renderpass start; combined with a scissor it produces an
/// intra-window dirty-rect partial redraw.
pub const BEGIN_RP_FLAG_NO_CLEAR: u32 = 0x1;

/// Emit a `BEGIN_RENDERPASS` record targeting `target_image_id`
/// with the given clear colour. Convenience wrapper — callers
/// could roll this themselves, but every frame needs one.
pub fn begin_renderpass(
    fb: &mut FrameBuilder,
    target_image_id: ResourceId,
    clear_color_rgba8: [u8; 4],
) -> Result<(), BridgeError> {
    begin_renderpass_with_flags(fb, target_image_id, clear_color_rgba8, 0)
}

/// Like `begin_renderpass` but with explicit `flags`. Emits the
/// extended 12-byte body the tier-1 renderer accepts; flags=0 is
/// equivalent to the legacy 8-byte body.
pub fn begin_renderpass_with_flags(
    fb: &mut FrameBuilder,
    target_image_id: ResourceId,
    clear_color_rgba8: [u8; 4],
    flags: u32,
) -> Result<(), BridgeError> {
    let mut body = [0u8; 12];
    body[..4].copy_from_slice(&target_image_id.raw().to_le_bytes());
    body[4..8].copy_from_slice(&clear_color_rgba8);
    body[8..12].copy_from_slice(&flags.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &body)?;
    Ok(())
}

/// Shorthand for the intra-window dirty-rect case: begin a
/// renderpass that preserves existing pixmap contents (no clear).
/// Pair with `set_scissor` for the actual damage rect.
pub fn begin_renderpass_no_clear(
    fb: &mut FrameBuilder,
    target_image_id: ResourceId,
) -> Result<(), BridgeError> {
    begin_renderpass_with_flags(
        fb, target_image_id, [0, 0, 0, 0], BEGIN_RP_FLAG_NO_CLEAR,
    )
}

/// Emit a `SET_SCISSOR` record restricting subsequent draws within
/// the current renderpass to `(x, y, w, h)` in target pixels.
/// Resets at the next `BEGIN_RENDERPASS` / `END_RENDERPASS`.
pub fn set_scissor(
    fb: &mut FrameBuilder,
    x: u32, y: u32, w: u32, h: u32,
) -> Result<(), BridgeError> {
    let mut body = [0u8; 16];
    body[ 0.. 4].copy_from_slice(&x.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&y.to_le_bytes());
    body[ 8..12].copy_from_slice(&w.to_le_bytes());
    body[12..16].copy_from_slice(&h.to_le_bytes());
    fb.push(FrameOp::SetScissor, &body)?;
    Ok(())
}

/// Emit an `END_RENDERPASS` record.
pub fn end_renderpass(fb: &mut FrameBuilder) -> Result<(), BridgeError> {
    fb.push(FrameOp::EndRenderPass, &[])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqueduct_gpu::frame::FrameDecoder;

    #[test]
    fn rect_emits_three_records() {
        let mut fb = FrameBuilder::new(4096);
        let p = fp::RectParams {
            x: 10.0, y: 20.0, w: 30.0, h: 40.0,
            r: 1.0, g: 0.0, b: 1.0, a: 1.0,
        };
        translate_rect(&mut fb, &p).unwrap();
        let buf = fb.into_buf();

        let mut decoder = FrameDecoder::new(&buf);
        let mut ops = Vec::new();
        while let Some((op, body)) = decoder.next().unwrap() {
            ops.push((op, body.to_vec()));
        }
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].0, FrameOp::BindPipeline);
        assert_eq!(ops[1].0, FrameOp::PushConstants);
        assert_eq!(ops[2].0, FrameOp::Draw);

        // Pipeline id should be builtin/0x0001.
        let raw = u32::from_le_bytes([ops[0].1[0], ops[0].1[1], ops[0].1[2], ops[0].1[3]]);
        let id = ResourceId(raw);
        assert_eq!(id.namespace(), Some(IdNamespace::Builtin));
        assert_eq!(id.local_id(), BUILTIN_PIPELINE_RECT);

        // PushConstants body is 4-byte header + 32-byte RectOpParams.
        assert_eq!(ops[1].1.len(), 4 + 32);
        // Float at offset 4 = x = 10.0.
        let x = f32::from_le_bytes([ops[1].1[4], ops[1].1[5], ops[1].1[6], ops[1].1[7]]);
        assert_eq!(x, 10.0);
    }

    #[test]
    fn glyph_run_folds_bearing_into_dx_dy() {
        let mut fb = FrameBuilder::new(4096);
        let p = fp::GlyphRunParams {
            x: 100.0, y: 200.0,
            atlas_slot_id: 7, // ignored — caller resolves to image id
            atlas_width: 256, atlas_height: 256,
            r: 1.0, g: 1.0, b: 1.0, a: 1.0,
            glyphs: vec![fp::GlyphInstance {
                dx: 5.0, dy: 0.0,
                atlas_u: 16, atlas_v: 32, atlas_w: 8, atlas_h: 16,
                bearing_x: 1.0, bearing_y: 14.0,
            }],
        };
        let atlas_id = ResourceId::new(IdNamespace::IcdRuntime, 42);
        translate_glyph_run(&mut fb, &p, atlas_id).unwrap();
        let buf = fb.into_buf();

        let mut decoder = FrameDecoder::new(&buf);
        let mut bind = None;
        let mut pc = None;
        while let Some((op, body)) = decoder.next().unwrap() {
            match op {
                FrameOp::BindPipeline   => bind = Some(body.to_vec()),
                FrameOp::PushConstants  => pc = Some(body.to_vec()),
                _ => {}
            }
        }
        let bind = bind.unwrap();
        let pc = pc.unwrap();

        // Bind targets the glyph_run pipeline.
        let raw = u32::from_le_bytes([bind[0], bind[1], bind[2], bind[3]]);
        assert_eq!(ResourceId(raw).local_id(), BUILTIN_PIPELINE_GLYPH_RUN);

        // Push-constants: 4-byte stage header + 32-byte run header +
        // 24-byte glyph instance.
        assert_eq!(pc.len(), 4 + 32 + 24);
        // Instance dx at offset 4 + 32 = 36; should be 5.0 + 1.0 = 6.0.
        let dx = f32::from_le_bytes([pc[36], pc[37], pc[38], pc[39]]);
        assert_eq!(dx, 6.0);
        // Instance dy at offset 40; should be 0.0 - 14.0 = -14.0.
        let dy = f32::from_le_bytes([pc[40], pc[41], pc[42], pc[43]]);
        assert_eq!(dy, -14.0);
    }

    #[test]
    fn path_emits_rotated_quad_with_close() {
        let mut fb = FrameBuilder::new(4096);
        let p = fp::PathParams {
            cx: 50.0, cy: 50.0,
            length: 20.0, width: 10.0,
            angle: 0.0, // identity rotation for trivial maths
            r: 1.0, g: 1.0, b: 0.0, a: 1.0,
        };
        translate_path(&mut fb, &p).unwrap();
        let buf = fb.into_buf();
        let mut decoder = FrameDecoder::new(&buf);
        let mut pc = None;
        while let Some((op, body)) = decoder.next().unwrap() {
            if op == FrameOp::PushConstants { pc = Some(body.to_vec()); }
        }
        let pc = pc.unwrap();
        // 4-byte stage header + 28-byte PathOpParams header +
        // 4 verbs with (x,y) + 1 Close verb.
        let expected_len = 4 + 28 + 4 * (1 + 8) + 1;
        assert_eq!(pc.len(), expected_len);
        // cmd_count at offset 4+20 = 24.
        let n = u32::from_le_bytes([pc[24], pc[25], pc[26], pc[27]]);
        assert_eq!(n, 5);
        // First verb at offset 4 + 28 = 32 = MoveTo (0x00).
        assert_eq!(pc[32], 0x00);
        // First x = cx - length/2 = 40.0.
        let x0 = f32::from_le_bytes([pc[33], pc[34], pc[35], pc[36]]);
        assert_eq!(x0, 40.0);
    }
}
