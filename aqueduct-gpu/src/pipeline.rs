//! Pipeline state shapes carried inside
//! `PipelineCreatePayload::state_blob`.
//!
//! `state_blob` is opaque at the envelope layer so different
//! backends can carry different state shapes. The tier-2
//! software renderer reads this module's
//! [`Tier2PipelineStateBlob`] (postcard-encoded) to recover
//! the vertex-input layout the guest's `vkCreateGraphics-
//! Pipelines` call described.

use serde::{Deserialize, Serialize};

/// Vertex attribute scalar/vector format. Subset of `VkFormat`
/// covering the formats the tier-2 software path supports.
/// Extend as ICD coverage grows; unknown values surface as
/// `OP_GPU_VALIDATION_ERR` at pipeline-create time.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VertexFormat {
    /// `VK_FORMAT_R32_SFLOAT` — 1 × f32, 4 bytes.
    R32Sfloat       = 1,
    /// `VK_FORMAT_R32G32_SFLOAT` — 2 × f32, 8 bytes.
    R32g32Sfloat    = 2,
    /// `VK_FORMAT_R32G32B32_SFLOAT` — 3 × f32, 12 bytes.
    R32g32b32Sfloat = 3,
    /// `VK_FORMAT_R32G32B32A32_SFLOAT` — 4 × f32, 16 bytes.
    R32g32b32a32Sfloat = 4,
    /// `VK_FORMAT_R8G8B8A8_UNORM` — 4 × u8 in the source vertex
    /// buffer (4 bytes), expanded to 4 × f32 in the assembled
    /// stream (16 bytes).  Per-channel conversion is
    /// `u8 / 255.0`, the spec-defined UNORM decoding.  Lets
    /// real apps feed packed vertex colours.
    R8g8b8a8Unorm   = 5,
}

impl VertexFormat {
    /// Number of bytes the attribute occupies in the source
    /// vertex buffer.  For SFLOAT formats this equals
    /// `f32_lanes() * 4`; for UNORM / SNORM formats it's
    /// smaller (4 bytes for `R8g8b8a8Unorm`) because the
    /// values are stored quantised.
    pub fn source_byte_size(self) -> usize {
        match self {
            VertexFormat::R32Sfloat            => 4,
            VertexFormat::R32g32Sfloat         => 8,
            VertexFormat::R32g32b32Sfloat      => 12,
            VertexFormat::R32g32b32a32Sfloat   => 16,
            VertexFormat::R8g8b8a8Unorm        => 4,
        }
    }

    /// Number of bytes the attribute occupies in the
    /// assembled vertex stream the VS reads.  Always
    /// `f32_lanes() * 4` because the rasterizer feeds the VS
    /// an f32 stream regardless of source quantisation.
    pub fn byte_size(self) -> usize {
        self.f32_lanes() * 4
    }

    /// Number of f32 lanes in one attribute (in the assembled
    /// stream).
    pub fn f32_lanes(self) -> usize {
        match self {
            VertexFormat::R32Sfloat            => 1,
            VertexFormat::R32g32Sfloat         => 2,
            VertexFormat::R32g32b32Sfloat      => 3,
            VertexFormat::R32g32b32a32Sfloat   => 4,
            VertexFormat::R8g8b8a8Unorm        => 4,
        }
    }
}

/// One vertex-input binding (mirrors
/// `VkVertexInputBindingDescription`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VertexBindingDesc {
    /// Binding slot number; matches `BindVertexBufCmd::binding`.
    pub binding: u32,
    /// Distance in bytes between successive vertex elements.
    pub stride: u32,
    /// Per-vertex (`false`) or per-instance (`true`). Tier-2
    /// today honors only per-vertex; per-instance is logged +
    /// treated as per-vertex during bring-up.
    pub per_instance: bool,
}

/// One vertex-input attribute (mirrors
/// `VkVertexInputAttributeDescription`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VertexAttributeDesc {
    /// Shader input location (matches the SPIR-V `Location`
    /// decoration on the vertex shader's `in` variable).
    pub location: u32,
    /// Source binding slot.
    pub binding: u32,
    /// Attribute format.
    pub format: VertexFormat,
    /// Byte offset within the per-vertex element.
    pub offset: u32,
}

/// The full vertex-input state (mirrors
/// `VkPipelineVertexInputStateCreateInfo`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VertexInputState {
    /// One entry per bound vertex buffer slot.
    pub bindings: Vec<VertexBindingDesc>,
    /// One entry per shader-input attribute.
    pub attributes: Vec<VertexAttributeDesc>,
}

impl VertexInputState {
    /// True if any attribute references a binding not present
    /// in `bindings` or reads past the binding's stride.
    pub fn validate(&self) -> Result<(), String> {
        for attr in &self.attributes {
            let bind = self.bindings.iter().find(|b| b.binding == attr.binding)
                .ok_or_else(|| format!(
                    "attribute @location {} references unknown binding {}",
                    attr.location, attr.binding))?;
            // Validate against the SOURCE byte size (bytes the
            // attribute occupies in the buffer), not the packed
            // stream size.  For UNORM formats source_byte_size
            // < byte_size since values are quantised down from
            // f32 to u8 in the vertex buffer.
            let end = (attr.offset as usize) + attr.format.source_byte_size();
            if end > bind.stride as usize {
                return Err(format!(
                    "attribute @location {} (binding {}, offset {}, size {}) \
                     overruns binding stride {}",
                    attr.location, attr.binding, attr.offset,
                    attr.format.byte_size(), bind.stride));
            }
        }
        Ok(())
    }
}

/// Depth-test state (subset of `VkPipelineDepthStencilStateCreateInfo`).
/// D.6 honors `test_enable` + `write_enable`; compare op is
/// hardcoded LESS in the tier-2 rasterizer (fill_image_triangle
/// R.3 spec).
/// Triangle cull mode, mirror of `VkCullModeFlags`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier2CullMode {
    /// No culling (default).
    #[default]
    None,
    /// Cull front-facing triangles.
    Front,
    /// Cull back-facing triangles (the typical default for 3D apps).
    Back,
    /// Cull every triangle.
    FrontAndBack,
}

/// Winding considered front-facing, mirror of `VkFrontFace`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier2FrontFace {
    /// Counter-clockwise winding is front (Vulkan default).
    #[default]
    CounterClockwise,
    /// Clockwise winding is front.
    Clockwise,
}

/// Tier-2 raster state extracted from
/// `VkPipelineRasterizationStateCreateInfo`.
///
/// Honoured by `fill_image_triangle` to skip triangles whose
/// post-viewport-mapping winding matches the active cull mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier2RasterState {
    /// Cull mode.
    pub cull_mode: Tier2CullMode,
    /// Front-face winding.
    pub front_face: Tier2FrontFace,
    /// When true, the rasterizer is bypassed entirely --
    /// vertex stages still execute (for transform-feedback
    /// side effects) but no fragments are produced.  Mirror
    /// of `VkPipelineRasterizationStateCreateInfo::
    /// rasterizerDiscardEnable`.
    #[serde(default)]
    pub rasterizer_discard: bool,
}

/// Tier-2 primitive topology, mirror of
/// `VkPrimitiveTopology`.  Only the two triangle modes are
/// implemented today; the others are reserved on the wire
/// so app pipelines that declare them round-trip, but the
/// daemon falls back to TriangleList rasterization.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier2PrimitiveTopology {
    /// Independent triangles from successive triples of
    /// vertices (the legacy default).
    #[default]
    TriangleList,
    /// Triangle strip; consecutive triangles share an edge.
    /// Vulkan's spec swaps the order of v[i+1] / v[i] for
    /// odd-numbered triangles to keep the winding consistent.
    TriangleStrip,
    /// Reserved / unimplemented (PointList, LineList,
    /// LineStrip, fans, list-with-adjacency, patches).
    /// Daemon treats these as TriangleList for now.
    Other,
}

/// Tier-2 depth compare op, mirror of `VkCompareOp`.
/// Encodes the rule used to decide whether a fragment's
/// `gl_FragCoord.z` passes the depth test against the
/// existing depth-buffer slot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier2CompareOp {
    /// The test never passes.
    Never,
    /// Pass when new < existing (Vulkan's typical default).
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
    /// The test always passes.
    Always,
}

/// Tier-2 depth-test state, mirror of
/// `VkPipelineDepthStencilStateCreateInfo`'s
/// depth-test/write enables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier2DepthState {
    /// When true, fragments only pass if their depth is less
    /// than the existing depth-buffer value.
    pub test_enable: bool,
    /// When true (and test_enable is true), passing fragments
    /// overwrite the depth-buffer slot.
    pub write_enable: bool,
    /// Compare rule for the test (Vulkan `depthCompareOp`).
    /// Defaults to `Less` so blob round-trips that pre-date
    /// this field still decode with the legacy hardcoded
    /// rasterizer behaviour.
    #[serde(default)]
    pub compare_op: Tier2CompareOp,
}

/// Per-channel blend factor; mirror of the tier-2 rasterizer's
/// internal `BlendFactor`. Subset of Vulkan `VkBlendFactor`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier2BlendFactor {
    /// `VK_BLEND_FACTOR_ZERO`.
    Zero              = 0,
    /// `VK_BLEND_FACTOR_ONE`.
    One               = 1,
    /// `VK_BLEND_FACTOR_SRC_COLOR`.
    SrcColor          = 2,
    /// `VK_BLEND_FACTOR_ONE_MINUS_SRC_COLOR`.
    OneMinusSrcColor  = 3,
    /// `VK_BLEND_FACTOR_DST_COLOR`.
    DstColor          = 4,
    /// `VK_BLEND_FACTOR_ONE_MINUS_DST_COLOR`.
    OneMinusDstColor  = 5,
    /// `VK_BLEND_FACTOR_SRC_ALPHA`.
    SrcAlpha          = 6,
    /// `VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA`.
    OneMinusSrcAlpha  = 7,
    /// `VK_BLEND_FACTOR_DST_ALPHA`.
    DstAlpha          = 8,
    /// `VK_BLEND_FACTOR_ONE_MINUS_DST_ALPHA`.
    OneMinusDstAlpha  = 9,
}

/// Blend equation. R.5 v1 supports `Add` only.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier2BlendOp {
    /// `VK_BLEND_OP_ADD`.
    Add = 0,
}

/// Per-attachment colour-blend + write-mask state (mirror of
/// `VkPipelineColorBlendAttachmentState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier2BlendState {
    /// When false, the source colour replaces the destination
    /// verbatim (modulo write_mask). Factor / op fields ignored.
    pub enable: bool,
    /// Source factor for the colour channels (RGB).
    pub color_src: Tier2BlendFactor,
    /// Destination factor for the colour channels (RGB).
    pub color_dst: Tier2BlendFactor,
    /// Source factor for the alpha channel.
    pub alpha_src: Tier2BlendFactor,
    /// Destination factor for the alpha channel.
    pub alpha_dst: Tier2BlendFactor,
    /// Colour-channel blend op.
    pub color_op: Tier2BlendOp,
    /// Alpha-channel blend op.
    pub alpha_op: Tier2BlendOp,
    /// Per-channel write enables (R, G, B, A).
    pub write_mask_rgba: [bool; 4],
}

impl Default for Tier2BlendState {
    /// Source-replace + all-channel write mask (the implicit
    /// pre-blend behaviour).
    fn default() -> Self {
        Self {
            enable: false,
            color_src: Tier2BlendFactor::One,
            color_dst: Tier2BlendFactor::Zero,
            alpha_src: Tier2BlendFactor::One,
            alpha_dst: Tier2BlendFactor::Zero,
            color_op: Tier2BlendOp::Add,
            alpha_op: Tier2BlendOp::Add,
            write_mask_rgba: [true; 4],
        }
    }
}

/// Tier-2 compute pipeline state blob — postcard-encoded
/// inside `PipelineCreatePayload::state_blob` when the
/// pipeline kind is `Compute`. Carries the SPIR-V
/// `LocalSize` (workgroup dimensions) since the Tier-2
/// dispatcher needs it to drive the (groupCount ×
/// local_size) invocation loop without re-parsing SPIR-V.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier2ComputeStateBlob {
    /// `local_size_x` from the SPIR-V `LocalSize` execution
    /// mode (or `LocalSizeId` for spec-constant sizing).
    pub local_size_x: u32,
    /// `local_size_y`.
    pub local_size_y: u32,
    /// `local_size_z`.
    pub local_size_z: u32,
    /// Number of distinct StorageBuffer bindings the shader
    /// declares (across set 0; multi-set isn't modeled yet).
    /// When `<= 1`, the dispatcher passes a single SSBO
    /// pointer in X2 (legacy path).  When `>= 2`, X2 becomes
    /// a descriptor-table base: an array of `ssbo_binding_count`
    /// u64 pointers, one per binding.  Populated by the ICD
    /// at vkCreateComputePipelines from a SPIR-V scan.
    pub ssbo_binding_count: u32,
    /// Total byte size of the per-workgroup scratch buffer the
    /// shader needs for `StorageClass::Workgroup` variables
    /// (0 if it declares none).  The dispatcher allocates a
    /// buffer of this size per worker thread and passes its
    /// base pointer as the 10th cs_main argument.
    pub workgroup_size: u32,
    /// `VkSpecializationInfo` overrides for this pipeline
    /// stage: each `(SpecId, value)` substitutes the host's
    /// 32-bit bit pattern for the SPIR-V `OpSpecConstant`'s
    /// declared default.  Empty = no specialisation (the
    /// SPIR-V defaults apply).  The daemon routes a non-
    /// empty list through `Tier2Registry::register_with_spec_overrides`,
    /// which produces a distinct compiled artifact + ID per
    /// `(spirv, overrides)` pair.
    pub spec_overrides: Vec<(u32, u32)>,
    /// `true` when the SPIR-V contains at least one
    /// `OpControlBarrier` (opcode 224).  Tells the Tier-2
    /// dispatcher to use Arc-150's parallel-lane mode (one
    /// OS thread per invocation, sharing an
    /// `Arc<std::sync::Barrier>`).  When `false` and
    /// `lx * ly * lz > 1`, the dispatcher MUST stay serial:
    /// without a barrier the lanes are observably-equivalent
    /// to executing in any order, so the cheaper serial
    /// loop is the right choice -- AND it gives a stable
    /// "last-writer wins" order that pre-150 shaders may
    /// (incorrectly but historically) rely on.  Populated
    /// by the ICD at vkCreateComputePipelines via a SPIR-V
    /// scan.
    pub uses_barrier: bool,
}

impl Default for Tier2ComputeStateBlob {
    /// Vulkan's spec default: 1x1x1 if a shader doesn't
    /// declare `LocalSize`. Apps should override.
    fn default() -> Self {
        Self {
            local_size_x: 1, local_size_y: 1, local_size_z: 1,
            ssbo_binding_count: 0,
            workgroup_size: 0,
            spec_overrides: Vec::new(),
            uses_barrier: false,
        }
    }
}

/// Tier-2 pipeline state blob — postcard-encoded inside
/// [`super::payloads::PipelineCreatePayload::state_blob`] when
/// the target backend is the tier-2 software renderer.
///
/// Other backends are free to encode their own state shape;
/// the session decodes opportunistically (failure to decode
/// means the pipeline isn't tier-2-shaped, not an error).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tier2PipelineStateBlob {
    /// Vertex-input layout for the bound vertex shader.
    pub vertex_input: VertexInputState,
    /// Depth-test state. `None` means "disabled" -- the draw
    /// runs with no depth attachment.
    #[serde(default)]
    pub depth: Option<Tier2DepthState>,
    /// Colour-blend state. `None` is equivalent to the default
    /// (source replace + all-channel write mask).
    #[serde(default)]
    pub blend: Option<Tier2BlendState>,
    /// Cull mode + front-face winding extracted from
    /// `VkPipelineRasterizationStateCreateInfo`. `None`
    /// preserves the legacy "no culling" behaviour.
    #[serde(default)]
    pub raster: Option<Tier2RasterState>,
    /// Primitive topology from
    /// `VkPipelineInputAssemblyStateCreateInfo`.  Defaults
    /// to `TriangleList` for backward compatibility with
    /// blobs that pre-date this field.
    #[serde(default)]
    pub topology: Tier2PrimitiveTopology,
    /// Total bytes the VS writes through Location-decorated
    /// `Output`-storage variables.  Lets the dispatcher size
    /// the per-vertex varying capture buffer fed to
    /// `fill_image_triangle::DrawTriangle::varyings_per_vertex`.
    /// 0 = no varyings (the VS only emits `BuiltIn` outputs);
    /// dispatcher skips capture + passes `null` for
    /// `in_varyings_ptr` to the FS.  Populated by the ICD at
    /// vkCreateGraphicsPipelines via a SPIR-V scan of the VS
    /// module.
    #[serde(default)]
    pub vs_varying_bytes: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_input_state_validates_referenced_bindings() {
        let st = VertexInputState {
            bindings: vec![
                VertexBindingDesc { binding: 0, stride: 12, per_instance: false },
            ],
            attributes: vec![
                VertexAttributeDesc {
                    location: 0, binding: 0,
                    format: VertexFormat::R32g32b32Sfloat, offset: 0,
                },
            ],
        };
        assert!(st.validate().is_ok());
    }

    #[test]
    fn vertex_input_state_rejects_unknown_binding() {
        let st = VertexInputState {
            bindings: vec![],
            attributes: vec![VertexAttributeDesc {
                location: 0, binding: 7,
                format: VertexFormat::R32Sfloat, offset: 0,
            }],
        };
        assert!(st.validate().is_err());
    }

    #[test]
    fn vertex_input_state_rejects_overrun() {
        let st = VertexInputState {
            bindings: vec![VertexBindingDesc {
                binding: 0, stride: 8, per_instance: false,
            }],
            attributes: vec![VertexAttributeDesc {
                location: 0, binding: 0,
                format: VertexFormat::R32g32b32Sfloat, offset: 0,
            }],
        };
        let err = st.validate().unwrap_err();
        assert!(err.contains("overruns"), "{err}");
    }

    #[test]
    fn tier2_pipeline_state_blob_postcard_roundtrip() {
        let blob = Tier2PipelineStateBlob {
            vertex_input: VertexInputState {
                bindings: vec![VertexBindingDesc {
                    binding: 0, stride: 20, per_instance: false,
                }],
                attributes: vec![
                    VertexAttributeDesc {
                        location: 0, binding: 0,
                        format: VertexFormat::R32g32b32Sfloat, offset: 0,
                    },
                    VertexAttributeDesc {
                        location: 1, binding: 0,
                        format: VertexFormat::R32g32Sfloat, offset: 12,
                    },
                ],
            },
            depth: None,
            blend: None,
        };
        let bytes = postcard::to_allocvec(&blob).unwrap();
        let back: Tier2PipelineStateBlob = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.vertex_input, blob.vertex_input);
        assert_eq!(back.depth, blob.depth);
    }

    #[test]
    fn tier2_pipeline_state_blob_with_depth_and_blend_roundtrip() {
        let blob = Tier2PipelineStateBlob {
            vertex_input: VertexInputState::default(),
            depth: Some(Tier2DepthState {
                test_enable: true, write_enable: true,
            }),
            blend: Some(Tier2BlendState {
                enable: true,
                color_src: Tier2BlendFactor::SrcAlpha,
                color_dst: Tier2BlendFactor::OneMinusSrcAlpha,
                alpha_src: Tier2BlendFactor::One,
                alpha_dst: Tier2BlendFactor::Zero,
                color_op: Tier2BlendOp::Add,
                alpha_op: Tier2BlendOp::Add,
                write_mask_rgba: [true, true, true, false],
            }),
        };
        let bytes = postcard::to_allocvec(&blob).unwrap();
        let back: Tier2PipelineStateBlob = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.depth, blob.depth);
        assert_eq!(back.blend, blob.blend);
    }
}
