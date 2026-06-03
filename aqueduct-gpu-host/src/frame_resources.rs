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
    /// Buffer ids (copy dst/src, fill, indirect args, vertex/index/uniform
    /// buffers, SSBOs).
    pub buffers: BTreeSet<u32>,
    /// Sampler ids (combined image-samplers in descriptor sets).
    pub samplers: BTreeSet<u32>,
    /// True iff every resource-referencing op was fully decoded. When
    /// false, the set may be incomplete — the caller must fall back to
    /// whole-world materialisation rather than risk a missing resource.
    pub complete: bool,
}

impl FrameResources {
    /// Total resources referenced (images + pipelines + buffers + samplers).
    pub fn len(&self) -> usize {
        self.images.len() + self.pipelines.len() + self.buffers.len() + self.samplers.len()
    }

    /// Whether no resources are referenced.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every referenced resource id, across all kinds — the set the router
    /// materialises on a tier.
    pub fn all_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.images.iter().chain(&self.pipelines).chain(&self.buffers).chain(&self.samplers).copied()
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
            // Vertex buffer: { binding u32, buffer_id u32, offset u64 }.
            FrameOp::BindVertexBuf => {
                if body.len() >= 8 { r.buffers.insert(le4(&body[4..8])); } else { r.complete = false; }
            }
            // Index buffer: { buffer_id u32, index_type u32, offset u64 }.
            FrameOp::BindIndexBuf => {
                if body.len() >= 4 { r.buffers.insert(le4(body)); } else { r.complete = false; }
            }
            // Descriptor set: header { set u32, write_count u32 } + writes of
            // { binding u32, type u32, buffer_id u32, image_id u32,
            //   sampler_id u32, offset u64, range u64 } = 36 B. Collect every
            // non-zero buffer/image/sampler id referenced.
            FrameOp::BindDescriptors => {
                if !decode_bind_descriptors(body, &mut r) { r.complete = false; }
            }
            // Still-undecoded resource-referencing ops → conservative
            // whole-world materialisation.
            FrameOp::CopyBufToImg
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

/// Bytes per `BindDescriptors` write (see the wire layout in
/// `tier2_backend`'s descriptor parsers).
const DESCRIPTOR_WRITE_BYTES: usize = 36;

/// Collect the buffer/image/sampler ids a `BindDescriptors` body references.
/// Returns `false` if the body is malformed (caller forces whole-world).
fn decode_bind_descriptors(body: &[u8], r: &mut FrameResources) -> bool {
    if body.len() < 8 {
        return false;
    }
    let u32_at = |o: usize| u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
    let write_count = u32_at(4) as usize;
    for w in 0..write_count {
        let off = 8 + w * DESCRIPTOR_WRITE_BYTES;
        if off + DESCRIPTOR_WRITE_BYTES > body.len() {
            return false; // truncated — can't trust the set
        }
        let buffer_id = u32_at(off + 8);
        let image_id = u32_at(off + 12);
        let sampler_id = u32_at(off + 16);
        if buffer_id != 0 { r.buffers.insert(buffer_id); }
        if image_id != 0 { r.images.insert(image_id); }
        if sampler_id != 0 { r.samplers.insert(sampler_id); }
    }
    true
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
    fn a_textured_geometry_draw_introspects_completely() {
        // Vertex + index buffers, and a descriptor set binding a sampled
        // texture (combined image-sampler) + a uniform buffer — all decoded.
        let mut fb = FrameBuilder::new(1024);
        let mut brp = le(0x10).to_vec();
        brp.extend_from_slice(&[0, 0, 0, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &le(0x20)).unwrap();
        // BindVertexBuf { binding, buffer_id=0x40, offset }.
        let mut vb = le(0).to_vec();
        vb.extend_from_slice(&le(0x40));
        vb.extend_from_slice(&0u64.to_le_bytes());
        fb.push(FrameOp::BindVertexBuf, &vb).unwrap();
        // BindIndexBuf { buffer_id=0x41, index_type, offset }.
        let mut ib = le(0x41).to_vec();
        ib.extend_from_slice(&le(0));
        ib.extend_from_slice(&0u64.to_le_bytes());
        fb.push(FrameOp::BindIndexBuf, &ib).unwrap();
        // BindDescriptors: header { set=0, write_count=2 } + 2 writes.
        let mut bd = le(0).to_vec();
        bd.extend_from_slice(&le(2));
        // write 0: combined image-sampler → image 0x50, sampler 0x60.
        let mut w0 = vec![0u8; DESCRIPTOR_WRITE_BYTES];
        w0[8..12].copy_from_slice(&le(0));      // buffer_id = 0
        w0[12..16].copy_from_slice(&le(0x50));  // image_id
        w0[16..20].copy_from_slice(&le(0x60));  // sampler_id
        bd.extend_from_slice(&w0);
        // write 1: uniform buffer → buffer 0x42.
        let mut w1 = vec![0u8; DESCRIPTOR_WRITE_BYTES];
        w1[8..12].copy_from_slice(&le(0x42));   // buffer_id
        bd.extend_from_slice(&w1);
        fb.push(FrameOp::BindDescriptors, &bd).unwrap();

        let r = frame_resources(fb.as_bytes());
        assert!(r.complete, "vertex/index/descriptor ops now decode");
        assert_eq!(r.images, BTreeSet::from([0x10, 0x50]));
        assert_eq!(r.pipelines, BTreeSet::from([0x20]));
        assert_eq!(r.buffers, BTreeSet::from([0x40, 0x41, 0x42]));
        assert_eq!(r.samplers, BTreeSet::from([0x60]));
    }

    #[test]
    fn an_undecoded_op_still_forces_fallback() {
        // A Blit is not yet decoded → conservative whole-world.
        let mut fb = FrameBuilder::new(256);
        fb.push(FrameOp::Blit, &[0u8; 32]).unwrap();
        assert!(!frame_resources(fb.as_bytes()).complete);
    }

    #[test]
    fn truncated_descriptor_body_forces_fallback() {
        let mut fb = FrameBuilder::new(256);
        let mut bd = le(0).to_vec();
        bd.extend_from_slice(&le(4)); // claims 4 writes…
        bd.extend_from_slice(&[0u8; 10]); // …but the body is truncated
        fb.push(FrameOp::BindDescriptors, &bd).unwrap();
        assert!(!frame_resources(fb.as_bytes()).complete);
    }

    #[test]
    fn malformed_short_body_is_treated_as_incomplete() {
        let mut fb = FrameBuilder::new(64);
        fb.push(FrameOp::BindPipeline, &[0u8; 2]).unwrap(); // too short for an id
        let r = frame_resources(fb.as_bytes());
        assert!(!r.complete);
    }
}
