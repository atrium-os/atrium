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
}

impl VertexFormat {
    /// Size of one attribute in bytes.
    pub fn byte_size(self) -> usize {
        match self {
            VertexFormat::R32Sfloat            => 4,
            VertexFormat::R32g32Sfloat         => 8,
            VertexFormat::R32g32b32Sfloat      => 12,
            VertexFormat::R32g32b32a32Sfloat   => 16,
        }
    }

    /// Number of f32 lanes in one attribute.
    pub fn f32_lanes(self) -> usize {
        match self {
            VertexFormat::R32Sfloat            => 1,
            VertexFormat::R32g32Sfloat         => 2,
            VertexFormat::R32g32b32Sfloat      => 3,
            VertexFormat::R32g32b32a32Sfloat   => 4,
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
            let end = (attr.offset as usize) + attr.format.byte_size();
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
        };
        let bytes = postcard::to_allocvec(&blob).unwrap();
        let back: Tier2PipelineStateBlob = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.vertex_input, blob.vertex_input);
    }
}
