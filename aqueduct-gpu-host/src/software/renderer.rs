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

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiny_skia::{Color, FillRule, Paint, PixmapMut, Rect, Transform};

use aqueduct_gpu::frame::{FrameDecodeError, FrameDecoder};
use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu::opcodes::FrameOp;

use super::{
    BUILTIN_PIPELINE_RECT,
    BUILTIN_PIPELINE_TEXTURED_RECT,
    BUILTIN_PIPELINE_PATH,
    BUILTIN_PIPELINE_GLYPH_RUN,
};

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

/// Renderer state. Owns a borrowed `PixmapMut` for the duration of
/// `dispatch_frame`. State machine fields (current pipeline, last
/// push-constants) reset between renderpasses.
pub struct TinySkiaRenderer<'a> {
    target: PixmapMut<'a>,
    in_renderpass: bool,
    current_pipeline: Option<ResourceId>,
    push_constants: Vec<u8>,
}

impl<'a> TinySkiaRenderer<'a> {
    /// Wrap a tiny-skia `PixmapMut` as the render target.
    pub fn new(target: PixmapMut<'a>) -> Self {
        Self {
            target,
            in_renderpass: false,
            current_pipeline: None,
            push_constants: Vec::new(),
        }
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

                // Frame ops the tier-1 renderer doesn't yet handle.
                // Caller surfaces these via OP_GPU_VALIDATION_ERR.
                FrameOp::BindDescriptors
                | FrameOp::BindVertexBuf
                | FrameOp::BindIndexBuf
                | FrameOp::SetViewport
                | FrameOp::SetScissor
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
        // Phase 1.3c-rect: body parsing TBD when target-image-id
        // selection lands. For now we just mark the state and use
        // the renderer's preset target. Clear values + multi-target
        // selection arrive when the textured-rect / multi-pass cases
        // land.
        let _ = body;
        self.in_renderpass = true;
        Ok(())
    }

    fn handle_end_renderpass(&mut self) -> Result<(), RendererError> {
        if !self.in_renderpass {
            return Err(RendererError::EndOutsideRenderPass);
        }
        self.in_renderpass = false;
        self.current_pipeline = None;
        self.push_constants.clear();
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
            BUILTIN_PIPELINE_TEXTURED_RECT
            | BUILTIN_PIPELINE_PATH
            | BUILTIN_PIPELINE_GLYPH_RUN => {
                Err(RendererError::Unsupported(
                    "builtin pipeline rasteriser not yet implemented",
                ))
            }
            _ => Err(RendererError::UnknownBuiltinPipeline(local)),
        }
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
        self.target.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::ids::IdNamespace;
    use tiny_skia::Pixmap;

    /// Build a frame command stream that fills the entire 64x64
    /// pixmap with magenta via the rect pipeline.
    fn magenta_rect_frame() -> Vec<u8> {
        let mut fb = FrameBuilder::new(1024);
        fb.push(FrameOp::BeginRenderPass, &[]).unwrap();

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
        let mut r = TinySkiaRenderer::new(pixmap.as_mut());
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
        let mut r = TinySkiaRenderer::new(pixmap.as_mut());
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
        let mut r = TinySkiaRenderer::new(pixmap.as_mut());
        let mut fb = FrameBuilder::new(128);
        fb.push(FrameOp::BeginRenderPass, &[]).unwrap();
        fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
        let err = r.dispatch_frame(&fb.into_buf()).unwrap_err();
        assert!(matches!(err, RendererError::NoPipelineBound));
    }

    #[test]
    fn rejects_unknown_builtin_pipeline() {
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut());
        let mut fb = FrameBuilder::new(128);
        let bogus = ResourceId::new(IdNamespace::Builtin, 0x9999);
        fb.push(FrameOp::BindPipeline, &bogus.raw().to_le_bytes()).unwrap();
        let err = r.dispatch_frame(&fb.into_buf()).unwrap_err();
        assert!(matches!(err, RendererError::UnknownBuiltinPipeline(0x9999)));
    }

    #[test]
    fn rejects_icd_runtime_pipeline_as_tier2() {
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        let mut r = TinySkiaRenderer::new(pixmap.as_mut());
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
    fn multiple_rects_in_one_frame() {
        // Stress the dispatch state machine: rebind push-constants
        // between draws (without rebinding pipeline) and confirm
        // both rects land.
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        pixmap.fill(Color::from_rgba8(0, 0, 0, 255));
        let mut r = TinySkiaRenderer::new(pixmap.as_mut());

        let mut fb = FrameBuilder::new(1024);
        fb.push(FrameOp::BeginRenderPass, &[]).unwrap();
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
