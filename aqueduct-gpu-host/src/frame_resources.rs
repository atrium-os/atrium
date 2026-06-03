//! Frame resource-set introspection — which resources a frame references.
//!
//! The gating piece for **per-resource residency** (single-homed routing,
//! `docs/spec/energy-policy.md` §"Single-homed resource residency"): to
//! materialise only a frame's resources on its dispatch tier — rather than
//! mirroring every upload to both backends — the router must know exactly
//! which resources the frame touches.
//!
//! A frame references resources through many ops: the render-target image
//! (`BeginRenderPass`, `BindColorAttachments`, `BindDepthAttachment`), the
//! pipeline (`BindPipeline`), copy/fill buffers and images, and —
//! *indirectly* — sampled textures + bound geometry buffers
//! (`BindDescriptors`, `BindVertexBuf`, `BindIndexBuf`). This pass decodes
//! the ones whose body format is known and **conservatively flags
//! incompleteness** when it meets a resource-referencing op it does not yet
//! decode. A caller that sees `complete == false` must materialise the
//! whole resource world on the dispatch tier: an undecoded reference could
//! be a sampled texture, and a missing texture is a wrong pixel — the one
//! thing routing must never produce.
//!
//! As coverage of the indirect ops grows, fewer frames fall back, and the
//! per-resource discrete upload win widens. Flat-UI frames (no textures, no
//! vertex buffers) introspect completely today.

use std::collections::BTreeSet;

use aqueduct_gpu::frame::FrameDecoder;
use aqueduct_gpu::opcodes::FrameOp;

/// The set of resources a frame references, by raw id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameResources {
    /// Image ids (render targets, copy/blit images).
    pub images: BTreeSet<u32>,
    /// Pipeline ids.
    pub pipelines: BTreeSet<u32>,
    /// Buffer ids (copy dst/src, fill, indirect args).
    pub buffers: BTreeSet<u32>,
    /// True iff every resource-referencing op was fully decoded. When
    /// false, the set may be incomplete — the caller must fall back to
    /// whole-world materialisation rather than risk a missing resource.
    pub complete: bool,
}

impl FrameResources {
    /// Total resources referenced (images + pipelines + buffers).
    pub fn len(&self) -> usize {
        self.images.len() + self.pipelines.len() + self.buffers.len()
    }

    /// Whether no resources are referenced.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.pipelines.is_empty() && self.buffers.is_empty()
    }
}

/// Walk a frame and collect the resources it references. See module docs
/// for the `complete` flag's meaning.
pub fn frame_resources(frame_buf: &[u8]) -> FrameResources {
    let le4 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let mut r = FrameResources { complete: true, ..Default::default() };
    let mut dec = FrameDecoder::new(frame_buf);
    while let Ok(Some((op, body))) = dec.next() {
        match op {
            // Render target = primary colour attachment (body[0..4]).
            FrameOp::BeginRenderPass | FrameOp::BindDepthAttachment => {
                if body.len() >= 4 { r.images.insert(le4(body)); } else { r.complete = false; }
            }
            // Secondary colour attachments: count u32 + count × image_id.
            FrameOp::BindColorAttachments => {
                if body.len() >= 4 {
                    let count = le4(body) as usize;
                    for i in 0..count {
                        let o = 4 + i * 4;
                        if o + 4 <= body.len() { r.images.insert(le4(&body[o..o + 4])); }
                        else { r.complete = false; break; }
                    }
                } else { r.complete = false; }
            }
            FrameOp::BindPipeline => {
                if body.len() >= 4 { r.pipelines.insert(le4(body)); } else { r.complete = false; }
            }
            // src image (0..4) + dst buffer (4..8).
            FrameOp::CopyImgToBuf => {
                if body.len() >= 8 {
                    r.images.insert(le4(&body[0..4]));
                    r.buffers.insert(le4(&body[4..8]));
                } else { r.complete = false; }
            }
            FrameOp::FillBuffer => {
                if body.len() >= 4 { r.buffers.insert(le4(body)); } else { r.complete = false; }
            }
            // Resource-referencing ops whose body we do not yet decode.
            // Conservatively force whole-world materialisation: any of these
            // could pull in a texture / vertex buffer we'd otherwise miss.
            FrameOp::BindDescriptors
            | FrameOp::BindVertexBuf
            | FrameOp::BindIndexBuf
            | FrameOp::CopyBufToImg
            | FrameOp::Blit
            | FrameOp::DrawIndirect
            | FrameOp::DispatchIndirect => {
                r.complete = false;
            }
            // No resource reference (Draw, Set*, PushConstants, barriers, …).
            _ => {}
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqueduct_gpu::frame::FrameBuilder;

    fn le(v: u32) -> [u8; 4] { v.to_le_bytes() }

    #[test]
    fn flat_frame_introspects_completely() {
        let mut fb = FrameBuilder::new(1024);
        let mut brp = le(0x10).to_vec();
        brp.extend_from_slice(&[0, 0, 0, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &le(0x20)).unwrap();
        let mut draw = le(3).to_vec();
        draw.extend_from_slice(&le(1));
        draw.extend_from_slice(&[0u8; 8]);
        fb.push(FrameOp::Draw, &draw).unwrap();
        let mut cib = le(0x10).to_vec();
        cib.extend_from_slice(&le(0x30));
        cib.extend_from_slice(&[0u8; 8]);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();

        let r = frame_resources(fb.as_bytes());
        assert!(r.complete, "no undecoded resource ops");
        assert_eq!(r.images, BTreeSet::from([0x10]));
        assert_eq!(r.pipelines, BTreeSet::from([0x20]));
        assert_eq!(r.buffers, BTreeSet::from([0x30]));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn mrt_color_attachments_are_collected() {
        let mut fb = FrameBuilder::new(1024);
        let mut brp = le(0x10).to_vec();
        brp.extend_from_slice(&[0, 0, 0, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        let mut bca = le(2).to_vec(); // count
        bca.extend_from_slice(&le(0x11));
        bca.extend_from_slice(&le(0x12));
        fb.push(FrameOp::BindColorAttachments, &bca).unwrap();
        let r = frame_resources(fb.as_bytes());
        assert!(r.complete);
        assert_eq!(r.images, BTreeSet::from([0x10, 0x11, 0x12]));
    }

    #[test]
    fn a_textured_draw_forces_conservative_fallback() {
        // BindDescriptors (a sampled texture) is not yet decoded → complete
        // must be false so the caller materialises the whole world.
        let mut fb = FrameBuilder::new(1024);
        let mut brp = le(0x10).to_vec();
        brp.extend_from_slice(&[0, 0, 0, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &le(0x20)).unwrap();
        fb.push(FrameOp::BindDescriptors, &[0u8; 16]).unwrap();
        let r = frame_resources(fb.as_bytes());
        assert!(!r.complete, "an undecoded descriptor bind forces whole-world");
        // What we *could* decode is still collected.
        assert_eq!(r.pipelines, BTreeSet::from([0x20]));
    }

    #[test]
    fn malformed_short_body_is_treated_as_incomplete() {
        let mut fb = FrameBuilder::new(64);
        fb.push(FrameOp::BindPipeline, &[0u8; 2]).unwrap(); // too short for an id
        let r = frame_resources(fb.as_bytes());
        assert!(!r.complete);
    }
}
