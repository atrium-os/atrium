//! `TinySkiaRenderer` — decodes a frame command stream and dispatches
//! each `FrameOp` to a tiny-skia `PixmapMut`.
//!
//! Per `docs/spec/aqueduct-gpu.md` §5.1, the frame stream is a packed
//! sequence of records:
//!
//! ```text
//! header (8 bytes) :: body (length - 8 bytes)
//! ```
//!
//! This renderer walks the stream record-by-record via
//! [`FrameDecoder`](aqueduct_gpu::frame::FrameDecoder), maintains a
//! small dispatch state machine (current pipeline, push constants),
//! and rasterises into a borrowed `PixmapMut` whenever a draw op
//! arrives.
//!
//! Phase 1.3c-rect lands `FOP_BIND_PIPELINE` + `FOP_PUSH_CONSTANTS`
//! + `FOP_DRAW` for the rect pipeline. Other pipelines (textured-rect,
//! path, glyph_run) are reserved but return `RendererError::Unsupported`
//! until their dispatch lands.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiny_skia::{Color, FillRule, Paint, Pixmap, PixmapMut, Rect, Transform};

use aqueduct_gpu::frame::{FrameDecodeError, FrameDecoder};
use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu::opcodes::FrameOp;

use super::{
    BUILTIN_PIPELINE_RECT,
    BUILTIN_PIPELINE_TEXTURED_RECT,
    BUILTIN_PIPELINE_PATH,
    BUILTIN_PIPELINE_GLYPH_RUN,
};

/// Body layout for `FOP_BEGIN_RENDERPASS` records in tier-1.
///
/// Wire layout: 8 or 12 bytes, plain little-endian. Newer clients
/// emit 12-byte bodies with the trailing `flags` field; 8-byte
/// bodies are accepted for backward compat (flags = 0).
///
/// ```text
///   offset  size  field
///   0       4     target_image_id   (u32, ResourceId::raw())
///   4       4     clear_color_rgba8 (4×u8: R, G, B, A premultiplied)
///   8       4     flags             (u32, optional; default 0)
/// ```
///
/// Defined flag bits:
///   `BEGIN_RP_FLAG_NO_CLEAR (0x1)` — skip the framebuffer fill at
/// the start of the renderpass. Used by intra-window dirty-rect
/// partial redraw: combined with a `SetScissor`, the pass writes
/// new pixels only inside the scissor, leaving existing pixmap
/// contents intact everywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct BeginRenderPassBody {
    /// Image to render into. Resolved against the backend's image
    /// table; missing target → `RendererError::Unsupported`.
    pub target_image_id: u32,
    /// Premultiplied straight-alpha clear colour, RGBA8.
    pub clear_color_rgba8: [u8; 4],
    /// Flag bits — see `BEGIN_RP_FLAG_*` constants.
    pub flags: u32,
}

/// Skip the framebuffer-clear at renderpass start. Combined with
/// a `SetScissor`, lets a partial redraw write only inside the
/// scissor rect, preserving existing pixmap contents elsewhere.
pub const BEGIN_RP_FLAG_NO_CLEAR: u32 = 0x1;

impl BeginRenderPassBody {
    /// Decode from a FOP record body. Accepts both 8-byte (legacy,
    /// flags=0) and 12-byte (extended) bodies.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RendererError> {
        if bytes.len() < 8 {
            return Err(RendererError::ShortBody {
                op: FrameOp::BeginRenderPass,
                got: bytes.len(),
                want: 8,
            });
        }
        let flags = if bytes.len() >= 12 {
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]])
        } else {
            0
        };
        Ok(Self {
            target_image_id: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            clear_color_rgba8: [bytes[4], bytes[5], bytes[6], bytes[7]],
            flags,
        })
    }

    /// Encode as plain little-endian bytes (12 bytes; emits the
    /// extended form with the flags field).
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[..4].copy_from_slice(&self.target_image_id.to_le_bytes());
        buf[4..8].copy_from_slice(&self.clear_color_rgba8);
        buf[8..12].copy_from_slice(&self.flags.to_le_bytes());
        buf
    }
}

/// Body layout for `FOP_SET_SCISSOR` records in tier-1.
///
/// Restricts subsequent draw calls in the current renderpass to the
/// rectangle `[x..x+w, y..y+h]` in target pixels. Clearing the
/// scissor: pass `w == 0 || h == 0` and the renderer drops the clip.
///
/// Wire layout: 16 bytes, plain little-endian (four u32).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct SetScissorBody {
    /// Top-left X (target pixels).
    pub x: u32,
    /// Top-left Y.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

impl SetScissorBody {
    /// Decode from a FOP record body.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RendererError> {
        if bytes.len() < 16 {
            return Err(RendererError::ShortBody {
                op: FrameOp::SetScissor,
                got: bytes.len(),
                want: 16,
            });
        }
        let u = |o: usize| u32::from_le_bytes([bytes[o], bytes[o+1], bytes[o+2], bytes[o+3]]);
        Ok(Self { x: u(0), y: u(4), w: u(8), h: u(12) })
    }

    /// Encode as plain little-endian bytes (16 bytes).
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[..4].copy_from_slice(&self.x.to_le_bytes());
        buf[4..8].copy_from_slice(&self.y.to_le_bytes());
        buf[8..12].copy_from_slice(&self.w.to_le_bytes());
        buf[12..16].copy_from_slice(&self.h.to_le_bytes());
        buf
    }
}

/// Push-constant layout for the atrium-core path pipeline.
///
/// Carries an RGBA fill colour, optional anti-alias flag, an affine
/// transform applied to all coordinates, and an inline path-command
/// stream. The command stream uses a compact binary encoding
/// matching tiny-skia's path-builder verbs.
///
/// Wire layout (header is 28 bytes, followed by variable-size
/// command stream):
///
/// ```text
///   offset  size  field
///   0       16    color: 4×f32 (R, G, B, A premultiplied)
///   16      1     anti_alias: u8 (0 = off, 1 = on)
///   17      1     fill_rule: u8 (0 = Winding, 1 = EvenOdd)
///   18      2     reserved (must be 0)
///   20      4     command_count: u32
///   24      4     reserved (must be 0)
///   28      …     commands: variable
///
/// Each command record is 1-byte verb + N×f32 args:
///   0x00 MoveTo  (x, y)              — 9 bytes
///   0x01 LineTo  (x, y)              — 9 bytes
///   0x02 QuadTo  (cx, cy, x, y)      — 17 bytes
///   0x03 CubicTo (c1x, c1y, c2x, c2y, x, y) — 25 bytes
///   0x04 Close                       — 1 byte
/// ```
///
/// Designed to fit small paths (rounded rects, icons, simple
/// decorations) inside the FOP_PUSH_CONSTANTS body's 128-byte
/// budget. Paths larger than that require splitting across
/// multiple PUSH_CONSTANTS/DRAW pairs, or a future
/// path-via-buffer mechanism (deferred until concrete demand —
/// most compositor paths are well under the limit).
#[derive(Debug, Clone, PartialEq)]
pub struct PathOpParams {
    /// Fill colour, premultiplied straight-alpha.
    pub color: [f32; 4],
    /// Anti-alias toggle. UI rects use false; vector shapes use
    /// true.
    pub anti_alias: bool,
    /// Fill rule.
    pub fill_rule: PathFillRule,
    /// Path command stream. See module documentation for the
    /// command encoding.
    pub commands: Vec<PathCommand>,
}

/// Path fill rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PathFillRule {
    /// Standard non-zero winding rule.
    Winding = 0,
    /// Even-odd rule.
    EvenOdd = 1,
}

/// One verb in a path command stream. Each variant carries the
/// coordinates required for that path operation; all coordinates
/// are in target-image pixel space (origin top-left, +x right,
/// +y down — same as Vulkan / tiny-skia).
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathCommand {
    /// Begin a new subpath at (x, y).
    MoveTo { x: f32, y: f32 },
    /// Add a line segment to (x, y).
    LineTo { x: f32, y: f32 },
    /// Add a quadratic Bezier with control point (cx, cy) ending
    /// at (x, y).
    QuadTo { cx: f32, cy: f32, x: f32, y: f32 },
    /// Add a cubic Bezier with control points (c1x, c1y) and
    /// (c2x, c2y), ending at (x, y).
    CubicTo { c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32 },
    /// Close the current subpath (implicit line back to the
    /// last MoveTo).
    Close,
}

impl PathCommand {
    /// Verb byte for wire encoding.
    pub const fn verb(&self) -> u8 {
        match self {
            PathCommand::MoveTo { .. }  => 0x00,
            PathCommand::LineTo { .. }  => 0x01,
            PathCommand::QuadTo { .. }  => 0x02,
            PathCommand::CubicTo { .. } => 0x03,
            PathCommand::Close          => 0x04,
        }
    }

    /// Wire size in bytes.
    pub const fn wire_size(&self) -> usize {
        match self {
            PathCommand::MoveTo { .. }  => 1 + 2 * 4,
            PathCommand::LineTo { .. }  => 1 + 2 * 4,
            PathCommand::QuadTo { .. }  => 1 + 4 * 4,
            PathCommand::CubicTo { .. } => 1 + 6 * 4,
            PathCommand::Close          => 1,
        }
    }
}

impl PathOpParams {
    /// Header size (color + flags + count = 28 bytes).
    pub const HEADER_LEN: usize = 28;

    /// Encode as plain little-endian bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(
            Self::HEADER_LEN + self.commands.iter().map(|c| c.wire_size()).sum::<usize>(),
        );
        for v in &self.color {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.push(self.anti_alias as u8);
        out.push(self.fill_rule as u8);
        out.extend_from_slice(&0u16.to_le_bytes());  // reserved
        out.extend_from_slice(&(self.commands.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());  // reserved
        for cmd in &self.commands {
            out.push(cmd.verb());
            match *cmd {
                PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => {
                    out.extend_from_slice(&x.to_le_bytes());
                    out.extend_from_slice(&y.to_le_bytes());
                }
                PathCommand::QuadTo { cx, cy, x, y } => {
                    for v in [cx, cy, x, y] { out.extend_from_slice(&v.to_le_bytes()); }
                }
                PathCommand::CubicTo { c1x, c1y, c2x, c2y, x, y } => {
                    for v in [c1x, c1y, c2x, c2y, x, y] {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
                PathCommand::Close => {}
            }
        }
        out
    }

    /// Decode from a push-constants byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RendererError> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(RendererError::ShortPushConstants {
                expected: Self::HEADER_LEN,
                got: bytes.len(),
            });
        }
        let mut color = [0f32; 4];
        for (i, chunk) in bytes[..16].chunks_exact(4).enumerate() {
            color[i] = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        let anti_alias = bytes[16] != 0;
        let fill_rule = match bytes[17] {
            0 => PathFillRule::Winding,
            1 => PathFillRule::EvenOdd,
            other => return Err(RendererError::InvalidPathFillRule(other)),
        };
        // bytes[18..20] reserved
        let command_count = u32::from_le_bytes(
            [bytes[20], bytes[21], bytes[22], bytes[23]],
        ) as usize;
        // bytes[24..28] reserved

        let mut commands = Vec::with_capacity(command_count);
        let mut cursor = Self::HEADER_LEN;
        for _ in 0..command_count {
            if cursor >= bytes.len() {
                return Err(RendererError::ShortPushConstants {
                    expected: cursor + 1,
                    got: bytes.len(),
                });
            }
            let verb = bytes[cursor];
            cursor += 1;
            commands.push(match verb {
                0x00 | 0x01 => {
                    let (x, y) = read_2_f32(bytes, &mut cursor)?;
                    if verb == 0x00 { PathCommand::MoveTo { x, y } }
                    else { PathCommand::LineTo { x, y } }
                }
                0x02 => {
                    let (cx, cy, x, y) = read_4_f32(bytes, &mut cursor)?;
                    PathCommand::QuadTo { cx, cy, x, y }
                }
                0x03 => {
                    let (c1x, c1y, c2x, c2y, x, y) = read_6_f32(bytes, &mut cursor)?;
                    PathCommand::CubicTo { c1x, c1y, c2x, c2y, x, y }
                }
                0x04 => PathCommand::Close,
                other => return Err(RendererError::InvalidPathVerb(other)),
            });
        }
        Ok(Self { color, anti_alias, fill_rule, commands })
    }
}

fn read_2_f32(b: &[u8], c: &mut usize) -> Result<(f32, f32), RendererError> {
    if b.len() < *c + 8 {
        return Err(RendererError::ShortPushConstants {
            expected: *c + 8,
            got: b.len(),
        });
    }
    let x = f32::from_le_bytes([b[*c], b[*c+1], b[*c+2], b[*c+3]]);
    let y = f32::from_le_bytes([b[*c+4], b[*c+5], b[*c+6], b[*c+7]]);
    *c += 8;
    Ok((x, y))
}

fn read_4_f32(b: &[u8], c: &mut usize) -> Result<(f32, f32, f32, f32), RendererError> {
    let (a, b1) = read_2_f32(b, c)?;
    let (c1, d) = read_2_f32(b, c)?;
    Ok((a, b1, c1, d))
}

fn read_6_f32(b: &[u8], c: &mut usize) -> Result<(f32, f32, f32, f32, f32, f32), RendererError> {
    let (a, b1) = read_2_f32(b, c)?;
    let (c1, d) = read_2_f32(b, c)?;
    let (e, f) = read_2_f32(b, c)?;
    Ok((a, b1, c1, d, e, f))
}

/// Push-constant layout for the atrium-core rect pipeline. Fields
/// match what `fresco-protocol::RectParams` carries on the wire, but
/// encoded as plain little-endian bytes (postcard-free, hot path).
///
/// Frescod's renderer populates this struct when it lowers a
/// `fresco-protocol::RectParams` to an aqueduct-gpu frame op.
///
/// Layout: 32 bytes total, no padding. Stable wire format —
/// changes require bumping the pipeline's local-ID (or a
/// pipeline-state hash change for hash-keyed pipeline_ids).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct RectOpParams {
    /// Top-left X in target pixels.
    pub x: f32,
    /// Top-left Y in target pixels.
    pub y: f32,
    /// Width in pixels.
    pub w: f32,
    /// Height in pixels.
    pub h: f32,
    /// Fill colour, premultiplied straight-alpha.
    pub r: f32,
    /// Fill colour green.
    pub g: f32,
    /// Fill colour blue.
    pub b: f32,
    /// Fill alpha.
    pub a: f32,
}

impl RectOpParams {
    /// Decode from a push-constants byte slice. The frame's
    /// `FOP_PUSH_CONSTANTS` body carries the raw bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RendererError> {
        if bytes.len() < std::mem::size_of::<Self>() {
            return Err(RendererError::ShortPushConstants {
                expected: std::mem::size_of::<Self>(),
                got: bytes.len(),
            });
        }
        let mut out = [0f32; 8];
        for (i, chunk) in bytes[..32].chunks_exact(4).enumerate() {
            out[i] = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        Ok(Self {
            x: out[0], y: out[1], w: out[2], h: out[3],
            r: out[4], g: out[5], b: out[6], a: out[7],
        })
    }

    /// Encode as plain little-endian bytes (32 bytes). Used by
    /// clients building rect-op frames.
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut buf = [0u8; 32];
        let vals = [self.x, self.y, self.w, self.h, self.r, self.g, self.b, self.a];
        for (i, v) in vals.iter().enumerate() {
            buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        buf
    }
}

/// Push-constant layout for the atrium-core textured-rect pipeline.
///
/// Samples a sub-rect of a source atlas image into a destination
/// rect, with an RGBA tint multiplier. Wire layout: 52 bytes, plain
/// little-endian.
///
/// ```text
///   offset  size  field
///   0       16    dst rect: 4×f32 (x, y, w, h) in target pixels
///   16      4     atlas_image_id: u32 (ResourceId::raw())
///   20      16    src rect (UV in pixels of the atlas): u0, v0, u1, v1
///   36      16    tint: 4×f32 RGBA premultiplied
/// ```
///
/// UVs are in *pixels of the atlas image*, not normalised [0..1] —
/// matches how tier-1 atlases (icons, glyph atlases) think.
/// `(u1, v1)` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct TexturedRectOpParams {
    /// Destination rect X (target-pixel space).
    pub dst_x: f32,
    /// Destination rect Y.
    pub dst_y: f32,
    /// Destination rect width.
    pub dst_w: f32,
    /// Destination rect height.
    pub dst_h: f32,
    /// Atlas image resource ID (raw u32).
    pub atlas_image_id: u32,
    /// Source U0 (atlas pixels).
    pub src_u0: f32,
    /// Source V0.
    pub src_v0: f32,
    /// Source U1 (exclusive).
    pub src_u1: f32,
    /// Source V1.
    pub src_v1: f32,
    /// Tint R (multiplied into sampled colour, premultiplied alpha).
    pub tint_r: f32,
    /// Tint G.
    pub tint_g: f32,
    /// Tint B.
    pub tint_b: f32,
    /// Tint A.
    pub tint_a: f32,
}

impl TexturedRectOpParams {
    /// Decode from a push-constants byte slice. Requires 52 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RendererError> {
        const SZ: usize = 52;
        if bytes.len() < SZ {
            return Err(RendererError::ShortPushConstants {
                expected: SZ, got: bytes.len(),
            });
        }
        let f = |off: usize| f32::from_le_bytes(
            [bytes[off], bytes[off+1], bytes[off+2], bytes[off+3]]
        );
        let u = |off: usize| u32::from_le_bytes(
            [bytes[off], bytes[off+1], bytes[off+2], bytes[off+3]]
        );
        Ok(Self {
            dst_x: f(0), dst_y: f(4), dst_w: f(8), dst_h: f(12),
            atlas_image_id: u(16),
            src_u0: f(20), src_v0: f(24), src_u1: f(28), src_v1: f(32),
            tint_r: f(36), tint_g: f(40), tint_b: f(44), tint_a: f(48),
        })
    }

    /// Encode as plain little-endian bytes (52 bytes).
    pub fn to_bytes(&self) -> [u8; 52] {
        let mut buf = [0u8; 52];
        let floats = [
            self.dst_x, self.dst_y, self.dst_w, self.dst_h,
        ];
        for (i, v) in floats.iter().enumerate() {
            buf[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
        }
        buf[16..20].copy_from_slice(&self.atlas_image_id.to_le_bytes());
        let uvs = [self.src_u0, self.src_v0, self.src_u1, self.src_v1,
                   self.tint_r, self.tint_g, self.tint_b, self.tint_a];
        for (i, v) in uvs.iter().enumerate() {
            let off = 20 + i*4;
            buf[off..off+4].copy_from_slice(&v.to_le_bytes());
        }
        buf
    }
}

/// One glyph instance in a glyph_run draw.
///
/// Wire layout: 24 bytes, plain little-endian.
///
/// ```text
///   offset  size  field
///   0       8     dst (dx, dy): 2×f32, run-origin-relative
///   8       4     atlas_u: u32 (atlas pixels)
///   12      4     atlas_v: u32
///   16      4     atlas_w: u32
///   20      4     atlas_h: u32
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct GlyphInstance {
    /// Destination X offset from run origin (target pixels).
    pub dx: f32,
    /// Destination Y offset from run origin.
    pub dy: f32,
    /// Atlas-pixel U origin of this glyph's sub-rect.
    pub atlas_u: u32,
    /// Atlas-pixel V origin.
    pub atlas_v: u32,
    /// Width of the glyph's atlas region (pixels).
    pub atlas_w: u32,
    /// Height of the glyph's atlas region.
    pub atlas_h: u32,
}

impl GlyphInstance {
    /// Encode as 24 little-endian bytes.
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..4].copy_from_slice(&self.dx.to_le_bytes());
        b[4..8].copy_from_slice(&self.dy.to_le_bytes());
        b[8..12].copy_from_slice(&self.atlas_u.to_le_bytes());
        b[12..16].copy_from_slice(&self.atlas_v.to_le_bytes());
        b[16..20].copy_from_slice(&self.atlas_w.to_le_bytes());
        b[20..24].copy_from_slice(&self.atlas_h.to_le_bytes());
        b
    }
}

/// Push-constant layout for the atrium-core glyph_run pipeline.
///
/// Batches N glyph quads from one atlas in a single Draw. The atlas
/// is expected to already encode final colour (premultiplied RGBA8) —
/// the client side (atrium-text bundle / fresco-text) is responsible
/// for compositing tint into the atlas at rasterise time. Tier-1 does
/// not apply a per-run RGB tint (deferred to tier-2 SW Vulkan where a
/// fragment shader can multiply).
///
/// Wire layout (header 28 bytes + N × 24-byte [`GlyphInstance`]):
///
/// ```text
///   offset  size  field
///   0       16    color: 4×f32 RGBA (only A used in tier-1; for opacity)
///   16      4     atlas_image_id: u32 (ResourceId::raw())
///   20      4     glyph_count: u32
///   24      8     origin: 2×f32 (run x, y in target pixels)
///   32      ...   glyph_count × GlyphInstance (24 bytes each)
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphRunParams {
    /// Run colour. Tier-1 honours only `color[3]` as run opacity.
    pub color: [f32; 4],
    /// Atlas image holding the rasterised glyphs.
    pub atlas_image_id: u32,
    /// Run origin (target pixels). Per-glyph dx/dy are relative to this.
    pub origin: [f32; 2],
    /// One entry per glyph, in draw order.
    pub glyphs: Vec<GlyphInstance>,
}

impl GlyphRunParams {
    const HEADER_SZ: usize = 32;
    const INSTANCE_SZ: usize = 24;

    /// Decode from a push-constants byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RendererError> {
        if bytes.len() < Self::HEADER_SZ {
            return Err(RendererError::ShortPushConstants {
                expected: Self::HEADER_SZ, got: bytes.len(),
            });
        }
        let f = |off: usize| f32::from_le_bytes(
            [bytes[off], bytes[off+1], bytes[off+2], bytes[off+3]]
        );
        let u = |off: usize| u32::from_le_bytes(
            [bytes[off], bytes[off+1], bytes[off+2], bytes[off+3]]
        );
        let color = [f(0), f(4), f(8), f(12)];
        let atlas_image_id = u(16);
        let glyph_count = u(20) as usize;
        let origin = [f(24), f(28)];

        let want_total = Self::HEADER_SZ + glyph_count * Self::INSTANCE_SZ;
        if bytes.len() < want_total {
            return Err(RendererError::ShortPushConstants {
                expected: want_total, got: bytes.len(),
            });
        }
        let mut glyphs = Vec::with_capacity(glyph_count);
        for i in 0..glyph_count {
            let off = Self::HEADER_SZ + i * Self::INSTANCE_SZ;
            glyphs.push(GlyphInstance {
                dx: f(off),
                dy: f(off + 4),
                atlas_u: u(off + 8),
                atlas_v: u(off + 12),
                atlas_w: u(off + 16),
                atlas_h: u(off + 20),
            });
        }
        Ok(Self { color, atlas_image_id, origin, glyphs })
    }

    /// Encode to a length-prefix-free byte buffer suitable for push-constants.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_SZ + self.glyphs.len() * Self::INSTANCE_SZ);
        for v in &self.color {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.atlas_image_id.to_le_bytes());
        out.extend_from_slice(&(self.glyphs.len() as u32).to_le_bytes());
        for v in &self.origin {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for g in &self.glyphs {
            out.extend_from_slice(&g.to_bytes());
        }
        out
    }
}

/// Renderer state. Owns a borrowed `PixmapMut` for the duration of
/// `dispatch_frame`. State machine fields (current pipeline, last
/// push-constants) reset between renderpasses.
pub struct TinySkiaRenderer<'a> {
    target: PixmapMut<'a>,
    /// Source images keyed by `ResourceId::raw() as u64`. Read-only
    /// access for textured-rect / glyph_run rasterisers. Excludes the
    /// target image — caller (SoftwareBackend::submit_frame) removes
    /// it from the map before constructing the renderer.
    sources: &'a HashMap<u64, Pixmap>,
    in_renderpass: bool,
    current_pipeline: Option<ResourceId>,
    push_constants: Vec<u8>,
    /// Current scissor rect in target pixels. `None` = no clip
    /// (full target). Built into a tiny-skia `Mask` lazily on
    /// first draw after `SetScissor` so single-renderpass code that
    /// never sets a scissor pays no extra cost.
    scissor: Option<(u32, u32, u32, u32)>,
    scissor_mask: Option<tiny_skia::Mask>,
}

impl<'a> TinySkiaRenderer<'a> {
    /// Wrap a tiny-skia `PixmapMut` as the render target plus a
    /// read-only sources map for textured-rect / glyph_run.
    pub fn new(target: PixmapMut<'a>, sources: &'a HashMap<u64, Pixmap>) -> Self {
        Self {
            target,
            sources,
            in_renderpass: false,
            current_pipeline: None,
            push_constants: Vec::new(),
            scissor: None,
            scissor_mask: None,
        }
    }

    /// Lazily build / cache a `Mask` covering the current scissor
    /// rect. After this returns, `self.scissor_mask` is `Some` if
    /// a scissor is active, `None` otherwise. Split out of the
    /// mask-read site to satisfy Rust's borrow checker: rasterisers
    /// need to pass `self.scissor_mask.as_ref()` alongside a
    /// `&mut self.target.fill_path(...)` call, so the mask field
    /// must already be populated before the rasteriser borrows
    /// `&mut self.target`.
    fn ensure_scissor_mask(&mut self) {
        let Some((x, y, w, h)) = self.scissor else { return; };
        if self.scissor_mask.is_some() { return; }
        let tw = self.target.width();
        let th = self.target.height();
        let Some(mut mask) = tiny_skia::Mask::new(tw, th) else { return; };
        if w > 0 && h > 0 {
            if let Some(rect) = tiny_skia::Rect::from_xywh(
                x as f32, y as f32, w as f32, h as f32,
            ) {
                let path = tiny_skia::PathBuilder::from_rect(rect);
                mask.fill_path(
                    &path,
                    tiny_skia::FillRule::Winding,
                    true,
                    tiny_skia::Transform::identity(),
                );
            }
        }
        self.scissor_mask = Some(mask);
    }

    /// Dispatch all records in a frame command stream. Returns the
    /// number of draw / dispatch ops actually executed (for
    /// telemetry).
    pub fn dispatch_frame(&mut self, frame_buf: &[u8]) -> Result<u32, RendererError> {
        let mut decoder = FrameDecoder::new(frame_buf);
        let mut draws = 0u32;
        while let Some((op, body)) = decoder.next()? {
            match op {
                FrameOp::BeginRenderPass => self.handle_begin_renderpass(body)?,
                FrameOp::EndRenderPass   => self.handle_end_renderpass()?,
                FrameOp::BindPipeline    => self.handle_bind_pipeline(body)?,
                FrameOp::PushConstants   => self.handle_push_constants(body)?,
                FrameOp::Draw => {
                    self.handle_draw()?;
                    draws += 1;
                }
                FrameOp::SetScissor => self.handle_set_scissor(body)?,

                // Frame ops the tier-1 renderer doesn't yet handle.
                // Caller surfaces these via OP_GPU_VALIDATION_ERR.
                FrameOp::BindDescriptors
                | FrameOp::BindVertexBuf
                | FrameOp::BindIndexBuf
                | FrameOp::BindDepthAttachment
                | FrameOp::SetViewport
                | FrameOp::SetCullMode
                | FrameOp::SetFrontFace
                | FrameOp::SetDepthTestEnable
                | FrameOp::SetDepthWriteEnable
                | FrameOp::DrawIndexed
                | FrameOp::DrawIndirect
                | FrameOp::Dispatch
                | FrameOp::DispatchIndirect
                | FrameOp::CopyBufToImg
                | FrameOp::CopyImgToBuf
                | FrameOp::Blit
                | FrameOp::PipelineBarrier => {
                    // Phase 1.3c-rect: not yet implemented. Subsequent
                    // commits add these in priority order: SetScissor
                    // (compositor uses it for partial redraw),
                    // BindDescriptors + DrawIndexed (textured-rect),
                    // glyph_run (atrium-text).
                    return Err(RendererError::UnsupportedFrameOp(op));
                }
            }
        }
        Ok(draws)
    }

    // ─── FrameOp handlers ─────────────────────────────────────────

    fn handle_begin_renderpass(&mut self, body: &[u8]) -> Result<(), RendererError> {
        if self.in_renderpass {
            return Err(RendererError::NestedRenderPass);
        }
        let p = BeginRenderPassBody::from_bytes(body)?;
        // Honour the clear-colour unless the caller set the
        // BEGIN_RP_FLAG_NO_CLEAR flag (intra-window dirty-rect
        // partial redraw: combined with a SetScissor, the pass
        // touches only inside-scissor pixels and existing pixmap
        // contents persist everywhere else).
        if p.flags & BEGIN_RP_FLAG_NO_CLEAR == 0 {
            let [r, g, b, a] = p.clear_color_rgba8;
            self.target.fill(Color::from_rgba8(r, g, b, a));
        }
        self.in_renderpass = true;
        // Scissor state is per-renderpass; new pass starts unclipped.
        self.scissor = None;
        self.scissor_mask = None;
        Ok(())
    }

    fn handle_end_renderpass(&mut self) -> Result<(), RendererError> {
        if !self.in_renderpass {
            return Err(RendererError::EndOutsideRenderPass);
        }
        self.in_renderpass = false;
        self.current_pipeline = None;
        self.push_constants.clear();
        self.scissor = None;
        self.scissor_mask = None;
        Ok(())
    }

    fn handle_set_scissor(&mut self, body: &[u8]) -> Result<(), RendererError> {
        let s = SetScissorBody::from_bytes(body)?;
        if s.w == 0 || s.h == 0 {
            // Clear scissor.
            self.scissor = None;
        } else {
            self.scissor = Some((s.x, s.y, s.w, s.h));
        }
        // Force mask rebuild on next draw.
        self.scissor_mask = None;
        Ok(())
    }

    fn handle_bind_pipeline(&mut self, body: &[u8]) -> Result<(), RendererError> {
        if body.len() < 4 {
            return Err(RendererError::ShortBody {
                op: FrameOp::BindPipeline,
                got: body.len(),
                want: 4,
            });
        }
        let raw = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let id = ResourceId(raw);

        // Tier-1 only knows the built-in namespace. Bundle
        // namespaces require bundle-load support (deferred). ICD-
        // runtime pipelines from third-party SPIR-V are tier-2
        // territory and return Unsupported.
        match id.namespace() {
            Some(IdNamespace::Builtin) => {
                let local = id.local_id();
                if !matches!(
                    local,
                    BUILTIN_PIPELINE_RECT
                        | BUILTIN_PIPELINE_TEXTURED_RECT
                        | BUILTIN_PIPELINE_PATH
                        | BUILTIN_PIPELINE_GLYPH_RUN
                ) {
                    return Err(RendererError::UnknownBuiltinPipeline(local));
                }
                self.current_pipeline = Some(id);
                Ok(())
            }
            Some(IdNamespace::Bundle(_)) => {
                Err(RendererError::Unsupported("bundle pipelines"))
            }
            Some(IdNamespace::IcdRuntime) => {
                Err(RendererError::Unsupported(
                    "ICD-runtime pipelines (third-party SPIR-V) — tier-2 territory"
                ))
            }
            None => Err(RendererError::InvalidPipelineId(raw)),
        }
    }

    fn handle_push_constants(&mut self, body: &[u8]) -> Result<(), RendererError> {
        // Frame-op body: stage_mask:u8 + offset:u8 + reserved:u16 +
        // inline_bytes (variable). Phase 1.3c-rect simplifies by
        // ignoring stage_mask / offset and treating the entire body
        // (after the 4-byte header) as the push-constant payload.
        if body.len() < 4 {
            return Err(RendererError::ShortBody {
                op: FrameOp::PushConstants,
                got: body.len(),
                want: 4,
            });
        }
        self.push_constants.clear();
        self.push_constants.extend_from_slice(&body[4..]);
        Ok(())
    }

    fn handle_draw(&mut self) -> Result<(), RendererError> {
        if !self.in_renderpass {
            return Err(RendererError::DrawOutsideRenderPass);
        }
        let pipeline = self.current_pipeline.ok_or(RendererError::NoPipelineBound)?;
        let local = pipeline.local_id();

        match local {
            BUILTIN_PIPELINE_RECT => self.rasterise_rect(),
            BUILTIN_PIPELINE_PATH => self.rasterise_path(),
            BUILTIN_PIPELINE_TEXTURED_RECT => self.rasterise_textured_rect(),
            BUILTIN_PIPELINE_GLYPH_RUN => self.rasterise_glyph_run(),
            _ => Err(RendererError::UnknownBuiltinPipeline(local)),
        }
    }

    fn rasterise_textured_rect(&mut self) -> Result<(), RendererError> {
        let params = TexturedRectOpParams::from_bytes(&self.push_constants)?;

        let atlas = self.sources
            .get(&(params.atlas_image_id as u64))
            .ok_or(RendererError::AtlasNotRegistered(params.atlas_image_id))?;

        // Sub-pixmap UV range sanity. We allow exclusive u1/v1 == w/h.
        let aw = atlas.width()  as f32;
        let ah = atlas.height() as f32;
        if params.src_u0 < 0.0 || params.src_v0 < 0.0
            || params.src_u1 > aw || params.src_v1 > ah
            || params.src_u1 <= params.src_u0
            || params.src_v1 <= params.src_v0
        {
            return Err(RendererError::InvalidUv {
                u0: params.src_u0, v0: params.src_v0,
                u1: params.src_u1, v1: params.src_v1,
                atlas_w: aw as u32, atlas_h: ah as u32,
            });
        }

        let dst = Rect::from_xywh(params.dst_x, params.dst_y, params.dst_w, params.dst_h)
            .ok_or(RendererError::InvalidRect)?;

        // tiny-skia's `Pattern.transform` is the pattern-to-world
        // mapping: for a pattern-space point P, the corresponding
        // target-space (world) point is `transform * P`. To sample
        // atlas point A at target point T:
        //   T = scale_inv · A + (dst_xy - scale_inv · atlas_uv0)
        // where scale_inv = dst_size / atlas_uv_size (i.e., the
        // pattern-space → world-space scale).
        let scale_inv_x = params.dst_w / (params.src_u1 - params.src_u0);
        let scale_inv_y = params.dst_h / (params.src_v1 - params.src_v0);
        let tx = params.dst_x - params.src_u0 * scale_inv_x;
        let ty = params.dst_y - params.src_v0 * scale_inv_y;
        let pattern_xform = Transform::from_row(scale_inv_x, 0.0, 0.0, scale_inv_y, tx, ty);

        let pattern = tiny_skia::Pattern::new(
            atlas.as_ref(),
            tiny_skia::SpreadMode::Pad,
            tiny_skia::FilterQuality::Bilinear,
            params.tint_a.clamp(0.0, 1.0),
            pattern_xform,
        );

        let mut paint = Paint::default();
        paint.shader = pattern;
        paint.anti_alias = false;

        let path = tiny_skia::PathBuilder::from_rect(dst);
        self.ensure_scissor_mask();
        self.target.fill_path(
            &path, &paint, FillRule::Winding, Transform::identity(),
            self.scissor_mask.as_ref(),
        );
        // Tint RGB ignored for tier-1 (Pattern doesn't directly
        // accept an RGB multiplier). The alpha channel is honoured
        // via the `opacity` arg above; glyph-tinting will arrive
        // alongside glyph_run by way of a per-glyph colour modulator
        // baked into the atlas, or a shader-side multiply when
        // tier-2 lands.
        let _ = (params.tint_r, params.tint_g, params.tint_b);
        Ok(())
    }

    fn rasterise_glyph_run(&mut self) -> Result<(), RendererError> {
        self.ensure_scissor_mask();
        let params = GlyphRunParams::from_bytes(&self.push_constants)?;
        if params.glyphs.is_empty() {
            return Ok(());
        }
        let atlas = self.sources
            .get(&(params.atlas_image_id as u64))
            .ok_or(RendererError::AtlasNotRegistered(params.atlas_image_id))?;

        let aw = atlas.width();
        let ah = atlas.height();

        let opacity = params.color[3].clamp(0.0, 1.0);

        for g in &params.glyphs {
            if g.atlas_w == 0 || g.atlas_h == 0 { continue; }

            // Bounds-check the atlas sub-rect. Reject the whole run
            // on malformed input — tier-1's "validate or refuse"
            // posture (no clamping, no silent dropping).
            if g.atlas_u.saturating_add(g.atlas_w) > aw
                || g.atlas_v.saturating_add(g.atlas_h) > ah
            {
                return Err(RendererError::InvalidUv {
                    u0: g.atlas_u as f32,
                    v0: g.atlas_v as f32,
                    u1: (g.atlas_u + g.atlas_w) as f32,
                    v1: (g.atlas_v + g.atlas_h) as f32,
                    atlas_w: aw, atlas_h: ah,
                });
            }

            let dst_x = params.origin[0] + g.dx;
            let dst_y = params.origin[1] + g.dy;
            let dst_w = g.atlas_w as f32;
            let dst_h = g.atlas_h as f32;

            let dst = Rect::from_xywh(dst_x, dst_y, dst_w, dst_h)
                .ok_or(RendererError::InvalidRect)?;

            // tiny-skia Pattern transform is pattern-to-world.
            // 1:1 scale: target_p = atlas_p + (dst_xy - atlas_uv0).
            let tx = dst_x - g.atlas_u as f32;
            let ty = dst_y - g.atlas_v as f32;
            let xform = Transform::from_row(1.0, 0.0, 0.0, 1.0, tx, ty);

            // Nearest-neighbour for crisp glyph bitmaps; switch to
            // Bilinear once we have subpixel positioning (post-Phase-1).
            let pattern = tiny_skia::Pattern::new(
                atlas.as_ref(),
                tiny_skia::SpreadMode::Pad,
                tiny_skia::FilterQuality::Nearest,
                opacity,
                xform,
            );

            let mut paint = Paint::default();
            paint.shader = pattern;
            paint.anti_alias = false;
            // tiny-skia's default blend is SourceOver, which is what
            // we want for glyph composition over a cleared target.

            let path = tiny_skia::PathBuilder::from_rect(dst);
            self.target.fill_path(
                &path, &paint, FillRule::Winding, Transform::identity(),
                self.scissor_mask.as_ref(),
            );
        }

        // RGB tint is reserved (atlas is expected to bake colour).
        // See GlyphRunParams docs.
        let _ = (params.color[0], params.color[1], params.color[2]);
        Ok(())
    }

    fn rasterise_path(&mut self) -> Result<(), RendererError> {
        let params = PathOpParams::from_bytes(&self.push_constants)?;
        if params.commands.is_empty() {
            // Empty path = nothing to draw, but not an error.
            return Ok(());
        }

        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba(
            params.color[0].clamp(0.0, 1.0),
            params.color[1].clamp(0.0, 1.0),
            params.color[2].clamp(0.0, 1.0),
            params.color[3].clamp(0.0, 1.0),
        ).ok_or(RendererError::InvalidColor)?);
        paint.anti_alias = params.anti_alias;

        let mut pb = tiny_skia::PathBuilder::new();
        for cmd in &params.commands {
            match *cmd {
                PathCommand::MoveTo { x, y } => pb.move_to(x, y),
                PathCommand::LineTo { x, y } => pb.line_to(x, y),
                PathCommand::QuadTo { cx, cy, x, y } => pb.quad_to(cx, cy, x, y),
                PathCommand::CubicTo { c1x, c1y, c2x, c2y, x, y } =>
                    pb.cubic_to(c1x, c1y, c2x, c2y, x, y),
                PathCommand::Close => pb.close(),
            }
        }
        let path = pb.finish().ok_or(RendererError::DegeneratePath)?;

        let fill_rule = match params.fill_rule {
            PathFillRule::Winding => FillRule::Winding,
            PathFillRule::EvenOdd => FillRule::EvenOdd,
        };
        self.ensure_scissor_mask();
        self.target.fill_path(&path, &paint, fill_rule, Transform::identity(),
            self.scissor_mask.as_ref());
        Ok(())
    }

    fn rasterise_rect(&mut self) -> Result<(), RendererError> {
        let params = RectOpParams::from_bytes(&self.push_constants)?;

        let mut paint = Paint::default();
        // tiny-skia expects 0..1 floats. Clamp to be safe against
        // bad client data; the universal-sandbox principle (§11) is
        // about preventing untrusted shader execution, not
        // preventing arithmetic clamping.
        paint.set_color(Color::from_rgba(
            params.r.clamp(0.0, 1.0),
            params.g.clamp(0.0, 1.0),
            params.b.clamp(0.0, 1.0),
            params.a.clamp(0.0, 1.0),
        ).ok_or(RendererError::InvalidColor)?);
        paint.anti_alias = false; // crisp UI rects; antialias for path later

        let rect = Rect::from_xywh(params.x, params.y, params.w, params.h)
            .ok_or(RendererError::InvalidRect)?;

        let path = tiny_skia::PathBuilder::from_rect(rect);
        self.ensure_scissor_mask();
        self.target.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            self.scissor_mask.as_ref(),
        );
        Ok(())
    }

    /// Borrow the underlying pixmap (for tests / post-render readback).
    pub fn target(&self) -> &PixmapMut<'a> {
        &self.target
    }
}

/// Errors emitted by the tier-1 renderer. The session translates
/// these into `OP_GPU_VALIDATION_ERR` async events to the client.
#[derive(Debug, Error)]
pub enum RendererError {
    /// Frame decoder rejected the byte stream.
    #[error("frame stream decode error: {0}")]
    FrameDecode(#[from] FrameDecodeError),

    /// `FrameOp` recognised but tier-1 doesn't handle it yet.
    #[error("frame op {0:?} not supported by tier-1 software renderer")]
    UnsupportedFrameOp(FrameOp),

    /// Higher-level feature not supported in tier-1.
    #[error("tier-1 software renderer cannot handle {0}")]
    Unsupported(&'static str),

    /// `FOP_BIND_PIPELINE` referenced a built-in we don't know.
    #[error("unknown built-in pipeline local-ID {0:#x}")]
    UnknownBuiltinPipeline(u32),

    /// `FOP_BIND_PIPELINE` body was malformed.
    #[error("invalid pipeline_id {0:#010x}")]
    InvalidPipelineId(u32),

    /// `FOP_BEGIN_RENDERPASS` while already inside one.
    #[error("nested render pass")]
    NestedRenderPass,

    /// `FOP_END_RENDERPASS` without a matching begin.
    #[error("END_RENDERPASS without an active render pass")]
    EndOutsideRenderPass,

    /// `FOP_DRAW` outside an active render pass.
    #[error("DRAW outside an active render pass")]
    DrawOutsideRenderPass,

    /// `FOP_DRAW` without a prior `FOP_BIND_PIPELINE`.
    #[error("DRAW with no pipeline bound")]
    NoPipelineBound,

    /// Push-constants buffer is shorter than the active op needs.
    #[error("short push constants: have {got} bytes, op needs {expected}")]
    ShortPushConstants {
        /// What we needed.
        expected: usize,
        /// What we got.
        got: usize,
    },

    /// A frame-op record's body is shorter than the dispatcher expects.
    #[error("op {op:?} body too short: have {got}, want {want}")]
    ShortBody {
        /// Which op.
        op: FrameOp,
        /// What we got.
        got: usize,
        /// What we needed.
        want: usize,
    },

    /// Push-constant rect colour was malformed (NaN, etc.).
    #[error("invalid colour in push constants (non-finite or out of range)")]
    InvalidColor,

    /// Push-constant rect dims were malformed (NaN, negative w/h).
    #[error("invalid rect in push constants (non-finite or negative dimensions)")]
    InvalidRect,

    /// PathOpParams carried a verb byte we don't recognise.
    #[error("invalid path verb byte {0:#x} (expected 0x00..=0x04)")]
    InvalidPathVerb(u8),

    /// PathOpParams carried an unrecognised fill rule.
    #[error("invalid path fill rule {0} (expected 0 = Winding, 1 = EvenOdd)")]
    InvalidPathFillRule(u8),

    /// Path construction produced no usable path (e.g. zero
    /// commands after the header, or only Close commands).
    #[error("path is degenerate after command-stream decode")]
    DegeneratePath,

    /// Textured-rect referenced an atlas image that hasn't been
    /// registered via `image_created` (created on this session).
    #[error("textured-rect atlas image {0:#010x} not registered")]
    AtlasNotRegistered(u32),

    /// Textured-rect UV rect was malformed (out of atlas bounds,
    /// zero/negative area, or non-finite values).
    #[error("invalid textured-rect UV ({u0},{v0})-({u1},{v1}) over atlas {atlas_w}x{atlas_h}")]
    InvalidUv {
        /// Source u0.
        u0: f32,
        /// Source v0.
        v0: f32,
        /// Source u1 (exclusive).
        u1: f32,
        /// Source v1 (exclusive).
        v1: f32,
        /// Atlas width.
        atlas_w: u32,
        /// Atlas height.
        atlas_h: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::ids::IdNamespace;
    use tiny_skia::Pixmap;

    fn rp_body(target_image_local: u32, clear: [u8; 4]) -> [u8; 12] {
        BeginRenderPassBody {
            target_image_id: ResourceId::new(IdNamespace::IcdRuntime, target_image_local).raw(),
            clear_color_rgba8: clear,
            flags: 0,
        }.to_bytes()
    }

    /// Build a frame command stream that fills the entire 64x64
    /// pixmap with magenta via the rect pipeline.
    fn magenta_rect_frame() -> Vec<u8> {
        let mut fb = FrameBuilder::new(1024);
        // target image id is irrelevant in the in-module test
        // (we hand the renderer an explicit PixmapMut), but the
        // body schema requires 8 bytes; clear colour stays black
        // so the test asserts on the actual rect rasterisation.
        fb.push(FrameOp::BeginRenderPass, &rp_body(1, [0, 0, 0, 255])).unwrap();

        let rect_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_RECT);
        fb.push(FrameOp::BindPipeline, &rect_pipe.raw().to_le_bytes()).unwrap();

        let params = RectOpParams {
            x: 0.0, y: 0.0, w: 64.0, h: 64.0,
            r: 1.0, g: 0.0, b: 1.0, a: 1.0,
        };
        let mut pc_body = vec![0u8; 4]; // stage_mask + offset + reserved
        pc_body.extend_from_slice(&params.to_bytes());
        fb.push(FrameOp::PushConstants, &pc_body).unwrap();

        // FOP_DRAW body: vertex_count, instance_count, first_vertex,
        // first_instance — tier-1 ignores these for the rect pipeline.
        fb.push(FrameOp::Draw, &[4u32.to_le_bytes(), 1u32.to_le_bytes(),
                                 0u32.to_le_bytes(), 0u32.to_le_bytes()].concat()).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        fb.into_buf()
    }

    #[test]
    fn rasterises_magenta_rect() {
        let mut pixmap = Pixmap::new(64, 64).unwrap();
        // Fill with non-magenta to verify the renderer actually wrote.
        pixmap.fill(Color::from_rgba8(0, 0, 0, 255));
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);
        let draws = r.dispatch_frame(&magenta_rect_frame()).unwrap();
        assert_eq!(draws, 1);

        // Centre pixel should be magenta now. tiny-skia premultiplies
        // by default; with a=1.0 the stored RGBA matches input.
        let center = pixmap.pixel(32, 32).unwrap();
        assert_eq!(center.red(),   255);
        assert_eq!(center.green(),   0);
        assert_eq!(center.blue(),  255);
        assert_eq!(center.alpha(), 255);
    }

    #[test]
    fn rejects_draw_outside_renderpass() {
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);
        let mut fb = FrameBuilder::new(128);
        let rect_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_RECT);
        fb.push(FrameOp::BindPipeline, &rect_pipe.raw().to_le_bytes()).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
        let err = r.dispatch_frame(&fb.into_buf()).unwrap_err();
        assert!(matches!(err, RendererError::DrawOutsideRenderPass));
    }

    #[test]
    fn rejects_draw_without_pipeline() {
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);
        let mut fb = FrameBuilder::new(128);
        fb.push(FrameOp::BeginRenderPass, &rp_body(1, [0, 0, 0, 255])).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
        let err = r.dispatch_frame(&fb.into_buf()).unwrap_err();
        assert!(matches!(err, RendererError::NoPipelineBound));
    }

    #[test]
    fn rejects_unknown_builtin_pipeline() {
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);
        let mut fb = FrameBuilder::new(128);
        let bogus = ResourceId::new(IdNamespace::Builtin, 0x9999);
        fb.push(FrameOp::BindPipeline, &bogus.raw().to_le_bytes()).unwrap();
        let err = r.dispatch_frame(&fb.into_buf()).unwrap_err();
        assert!(matches!(err, RendererError::UnknownBuiltinPipeline(0x9999)));
    }

    #[test]
    fn rejects_icd_runtime_pipeline_as_tier2() {
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);
        let mut fb = FrameBuilder::new(128);
        // ICD-runtime pipelines (top tag 0xF) require shader
        // execution — tier-1 rejects with "Unsupported".
        let icd_pipe = ResourceId::new(IdNamespace::IcdRuntime, 0x1);
        fb.push(FrameOp::BindPipeline, &icd_pipe.raw().to_le_bytes()).unwrap();
        let err = r.dispatch_frame(&fb.into_buf()).unwrap_err();
        assert!(matches!(err, RendererError::Unsupported(_)),
                "expected Unsupported, got {err:?}");
    }

    #[test]
    fn rect_params_byte_roundtrip() {
        let p = RectOpParams {
            x: 1.0, y: 2.0, w: 3.0, h: 4.0,
            r: 0.5, g: 0.25, b: 0.125, a: 1.0,
        };
        let bytes = p.to_bytes();
        assert_eq!(bytes.len(), 32);
        let back = RectOpParams::from_bytes(&bytes).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn from_bytes_rejects_short_buffer() {
        let err = RectOpParams::from_bytes(&[0u8; 20]).unwrap_err();
        assert!(matches!(err, RendererError::ShortPushConstants { .. }));
    }

    #[test]
    fn path_params_byte_roundtrip() {
        let p = PathOpParams {
            color: [0.5, 0.25, 0.125, 1.0],
            anti_alias: true,
            fill_rule: PathFillRule::EvenOdd,
            commands: vec![
                PathCommand::MoveTo { x: 10.0, y: 10.0 },
                PathCommand::LineTo { x: 20.0, y: 10.0 },
                PathCommand::QuadTo { cx: 25.0, cy: 15.0, x: 20.0, y: 20.0 },
                PathCommand::CubicTo {
                    c1x: 18.0, c1y: 18.0, c2x: 12.0, c2y: 18.0, x: 10.0, y: 20.0
                },
                PathCommand::Close,
            ],
        };
        let bytes = p.to_bytes();
        let back = PathOpParams::from_bytes(&bytes).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn rasterises_triangle_path() {
        // Fill a triangle with vertices (4,4), (12,4), (8,14) onto
        // a 16x16 black pixmap; the pixel at the triangle's
        // centroid should be the fill colour, and a pixel well
        // outside should still be black.
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        pixmap.fill(Color::from_rgba8(0, 0, 0, 255));
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);

        let path_params = PathOpParams {
            color: [1.0, 1.0, 0.0, 1.0], // yellow
            anti_alias: false,
            fill_rule: PathFillRule::Winding,
            commands: vec![
                PathCommand::MoveTo { x: 4.0,  y: 4.0 },
                PathCommand::LineTo { x: 12.0, y: 4.0 },
                PathCommand::LineTo { x: 8.0,  y: 14.0 },
                PathCommand::Close,
            ],
        };

        let mut fb = FrameBuilder::new(2048);
        fb.push(FrameOp::BeginRenderPass, &rp_body(1, [0, 0, 0, 255])).unwrap();
        let path_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_PATH);
        fb.push(FrameOp::BindPipeline, &path_pipe.raw().to_le_bytes()).unwrap();
        let mut body = vec![0u8; 4]; // stage_mask + offset + reserved
        body.extend_from_slice(&path_params.to_bytes());
        fb.push(FrameOp::PushConstants, &body).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        r.dispatch_frame(&fb.into_buf()).unwrap();

        // Centroid of the triangle (~ (8, 7.3)) should be yellow.
        let inside = pixmap.pixel(8, 7).unwrap();
        assert_eq!(inside.red(),   255);
        assert_eq!(inside.green(), 255);
        assert_eq!(inside.blue(),    0);

        // Top-left corner should be untouched (black).
        let outside = pixmap.pixel(0, 0).unwrap();
        assert_eq!(outside.red(),   0);
        assert_eq!(outside.green(), 0);
        assert_eq!(outside.blue(),  0);
    }

    #[test]
    fn rasterises_quad_and_cubic_curves() {
        // Confirm the QuadTo and CubicTo verbs are wired up — we
        // build a small closed path mixing both kinds of curves
        // and just assert the dispatch succeeded with a non-zero
        // covered area (centroid pixel is filled).
        let mut pixmap = Pixmap::new(32, 32).unwrap();
        pixmap.fill(Color::from_rgba8(0, 0, 0, 255));
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);

        let params = PathOpParams {
            color: [0.0, 1.0, 0.0, 1.0], // green
            anti_alias: true,
            fill_rule: PathFillRule::Winding,
            commands: vec![
                PathCommand::MoveTo  { x: 8.0,  y: 16.0 },
                PathCommand::QuadTo  { cx: 16.0, cy: 4.0,  x: 24.0, y: 16.0 },
                PathCommand::CubicTo { c1x: 22.0, c1y: 22.0,
                                       c2x: 10.0, c2y: 22.0,
                                       x: 8.0,  y: 16.0 },
                PathCommand::Close,
            ],
        };

        let mut fb = FrameBuilder::new(2048);
        fb.push(FrameOp::BeginRenderPass, &rp_body(1, [0, 0, 0, 255])).unwrap();
        let path_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_PATH);
        fb.push(FrameOp::BindPipeline, &path_pipe.raw().to_le_bytes()).unwrap();
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&params.to_bytes());
        fb.push(FrameOp::PushConstants, &body).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        r.dispatch_frame(&fb.into_buf()).unwrap();

        // Centre of the curve should be green-ish.
        let centre = pixmap.pixel(16, 16).unwrap();
        assert!(centre.green() > 200, "expected green-dominant centre, got {centre:?}");
    }

    #[test]
    fn rejects_invalid_path_verb() {
        // Construct a path payload with a bogus verb byte.
        let mut body = vec![0u8; 4]; // stage_mask + offset + reserved
        let mut payload: Vec<u8> = Vec::new();
        // 4 floats colour
        for _ in 0..4 { payload.extend_from_slice(&1.0f32.to_le_bytes()); }
        payload.push(0); // anti_alias = off
        payload.push(0); // fill_rule = Winding
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes()); // command_count = 1
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(0x99); // bogus verb
        body.extend_from_slice(&payload);

        let mut pixmap = Pixmap::new(8, 8).unwrap();
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);
        let mut fb = FrameBuilder::new(256);
        fb.push(FrameOp::BeginRenderPass, &rp_body(1, [0, 0, 0, 255])).unwrap();
        let path_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_PATH);
        fb.push(FrameOp::BindPipeline, &path_pipe.raw().to_le_bytes()).unwrap();
        fb.push(FrameOp::PushConstants, &body).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
        let err = r.dispatch_frame(&fb.into_buf()).unwrap_err();
        assert!(matches!(err, RendererError::InvalidPathVerb(0x99)));
    }

    #[test]
    fn empty_path_is_no_op() {
        // command_count = 0 should not be an error; just no draw.
        let params = PathOpParams {
            color: [1.0, 0.0, 0.0, 1.0],
            anti_alias: false,
            fill_rule: PathFillRule::Winding,
            commands: vec![],
        };
        let mut pixmap = Pixmap::new(8, 8).unwrap();
        pixmap.fill(Color::from_rgba8(0, 0, 0, 255));
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);
        let mut fb = FrameBuilder::new(256);
        fb.push(FrameOp::BeginRenderPass, &rp_body(1, [0, 0, 0, 255])).unwrap();
        let path_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_PATH);
        fb.push(FrameOp::BindPipeline, &path_pipe.raw().to_le_bytes()).unwrap();
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&params.to_bytes());
        fb.push(FrameOp::PushConstants, &body).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        r.dispatch_frame(&fb.into_buf()).unwrap();

        // Pixmap should still be entirely black.
        let p = pixmap.pixel(4, 4).unwrap();
        assert_eq!((p.red(), p.green(), p.blue()), (0, 0, 0));
    }

    #[test]
    fn multiple_rects_in_one_frame() {
        // Stress the dispatch state machine: rebind push-constants
        // between draws (without rebinding pipeline) and confirm
        // both rects land.
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        pixmap.fill(Color::from_rgba8(0, 0, 0, 255));
        let sources = HashMap::new();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut(), &sources);

        let mut fb = FrameBuilder::new(1024);
        fb.push(FrameOp::BeginRenderPass, &rp_body(1, [0, 0, 0, 255])).unwrap();
        let rect_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_RECT);
        fb.push(FrameOp::BindPipeline, &rect_pipe.raw().to_le_bytes()).unwrap();

        // Red rect, top-left 4x4.
        let red = RectOpParams { x: 0.0, y: 0.0, w: 4.0, h: 4.0,
                                  r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&red.to_bytes());
        fb.push(FrameOp::PushConstants, &body).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();

        // Green rect, bottom-right 4x4.
        let green = RectOpParams { x: 12.0, y: 12.0, w: 4.0, h: 4.0,
                                    r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
        body.clear();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&green.to_bytes());
        fb.push(FrameOp::PushConstants, &body).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();

        fb.push(FrameOp::EndRenderPass, &[]).unwrap();

        let draws = r.dispatch_frame(&fb.into_buf()).unwrap();
        assert_eq!(draws, 2);

        let red_px   = pixmap.pixel(2, 2).unwrap();
        assert_eq!((red_px.red(), red_px.green(), red_px.blue()), (255, 0, 0));
        let green_px = pixmap.pixel(14, 14).unwrap();
        assert_eq!((green_px.red(), green_px.green(), green_px.blue()), (0, 255, 0));
    }
}
