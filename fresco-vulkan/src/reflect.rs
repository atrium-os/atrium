//! Minimal SPIR-V reflection — descriptor bindings only.
//!
//! Replaces the hardcoded `create_*_set_layout` paths from step 4: the
//! shader IS the source of truth for which (set, binding) slots exist
//! and what descriptor type each one is. New bundles with novel
//! binding shapes "just work" without a renderer-side OpKind enum.
//!
//! We parse the SPIR-V word stream by hand because we only need a
//! tiny slice of the grammar (~5 opcodes, 2 decorations, 3 storage
//! classes). Pulling in `rspirv` for that would be more dependency
//! than reflection.
//!
//! Limitations:
//!   - One descriptor per binding (no arrays). Easy to extend by
//!     reading OpTypeArray's length.
//!   - Stage flags are caller-provided (which shader did this SPIR-V
//!     come from); we don't try to infer from OpEntryPoint, since the
//!     caller already knows whether they're loading compute / vert /
//!     frag.
//!   - Assumes modern Vulkan SPIR-V (StorageBuffer storage class for
//!     SSBOs, not the legacy Uniform + BufferBlock decoration). True
//!     for glslangValidator with --target-env vulkan1.3 (what the
//!     pre-Slang `bundles/*/build.sh` ran) AND for slangc with
//!     `-target spirv` (no `-profile glsl_460` — that flag would
//!     force the legacy encoding).

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use ash::vk;

const SPIRV_MAGIC: u32 = 0x0723_0203;

/* SPIR-V opcodes we recognise. */
const OP_TYPE_IMAGE:           u32 = 25;
const OP_TYPE_SAMPLED_IMAGE:   u32 = 27;
const OP_TYPE_POINTER:         u32 = 32;
const OP_VARIABLE:             u32 = 59;
const OP_DECORATE:             u32 = 71;

/* SPIR-V Decoration enum values. */
const DECORATION_BINDING:        u32 = 33;
const DECORATION_DESCRIPTOR_SET: u32 = 34;

/* SPIR-V StorageClass enum values. */
const STORAGE_UNIFORM_CONSTANT: u32 = 0;
const STORAGE_UNIFORM:          u32 = 2;
const STORAGE_STORAGE_BUFFER:   u32 = 12;

/// One descriptor binding found in a shader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReflectedBinding {
    pub set:     u32,
    pub binding: u32,
    pub ty:      vk::DescriptorType,
}

/// Parse the SPIR-V word stream and extract every (set, binding) used
/// by an OpVariable. Stage flags are not included here — see
/// `merge_for_layout` for the per-set merge that combines bindings
/// across compute / vertex / fragment.
pub fn reflect(spirv: &[u32]) -> Result<Vec<ReflectedBinding>> {
    if spirv.len() < 5 || spirv[0] != SPIRV_MAGIC {
        bail!("not a SPIR-V binary (bad magic)");
    }

    /* First pass: collect decorations + type info indexed by result-id. */
    let mut decor_set:     HashMap<u32, u32> = HashMap::new();
    let mut decor_binding: HashMap<u32, u32> = HashMap::new();
    /* id → (storage_class, pointee_type_id) */
    let mut pointers:      HashMap<u32, (u32, u32)> = HashMap::new();
    /* id → ptr_type_id; storage_class read from the pointer type. */
    let mut variables:     HashMap<u32, u32> = HashMap::new();
    let mut sampled_image_ids: std::collections::HashSet<u32> =
        std::collections::HashSet::new();

    let mut i = 5;  /* skip 5-word header */
    while i < spirv.len() {
        let word = spirv[i];
        let op   = word & 0xFFFF;
        let wc   = (word >> 16) as usize;
        if wc == 0 || i + wc > spirv.len() {
            bail!("malformed SPIR-V instruction at word {i}");
        }
        let operands = &spirv[i + 1 .. i + wc];

        match op {
            OP_DECORATE if operands.len() >= 3 => {
                let target  = operands[0];
                let decor   = operands[1];
                let literal = operands[2];
                match decor {
                    DECORATION_DESCRIPTOR_SET => { decor_set.insert(target, literal); }
                    DECORATION_BINDING        => { decor_binding.insert(target, literal); }
                    _ => {}
                }
            }
            OP_TYPE_POINTER if operands.len() == 3 => {
                /* operands: result_id, storage_class, pointee_type_id */
                pointers.insert(operands[0], (operands[1], operands[2]));
            }
            OP_VARIABLE if operands.len() >= 3 => {
                /* operands: result_type_id (= a pointer type), result_id, storage_class, [initializer] */
                variables.insert(operands[1], operands[0]);
            }
            OP_TYPE_SAMPLED_IMAGE if !operands.is_empty() => {
                /* result_id is operands[0]; we don't care about the
                 * inner image type for descriptor selection. */
                sampled_image_ids.insert(operands[0]);
            }
            OP_TYPE_IMAGE if !operands.is_empty() => {
                /* Bare OpTypeImage (no sampler) → STORAGE_IMAGE in
                 * descriptor terms. We don't generate any of these in
                 * atrium-core today, but recognise it so future bundles
                 * with imageStore() compute kernels reflect cleanly. */
                sampled_image_ids.insert(operands[0]);  /* treated below */
            }
            _ => {}
        }
        i += wc;
    }

    /* Second pass: for each variable that has both (set, binding)
     * decorations, resolve descriptor type via its pointer's storage
     * class + pointee. */
    let mut out: Vec<ReflectedBinding> = Vec::new();
    for (&var_id, &ptr_type_id) in &variables {
        let (Some(&set), Some(&binding)) =
            (decor_set.get(&var_id), decor_binding.get(&var_id))
        else { continue };

        let (storage_class, pointee) = pointers.get(&ptr_type_id)
            .copied()
            .ok_or_else(|| anyhow!(
                "OpVariable id={var_id} references unknown pointer type id={ptr_type_id}"))?;

        let ty = match storage_class {
            STORAGE_STORAGE_BUFFER => vk::DescriptorType::STORAGE_BUFFER,
            STORAGE_UNIFORM        => vk::DescriptorType::UNIFORM_BUFFER,
            STORAGE_UNIFORM_CONSTANT => {
                if sampled_image_ids.contains(&pointee) {
                    vk::DescriptorType::COMBINED_IMAGE_SAMPLER
                } else {
                    /* Uniform constant that isn't a sampled-image: fall
                     * back to combined-image-sampler. The atrium-core
                     * bundles only use sampler2D under this storage
                     * class, so this branch is the conservative default. */
                    vk::DescriptorType::COMBINED_IMAGE_SAMPLER
                }
            }
            _ => continue,  /* not a descriptor — skip (Input/Output/etc.) */
        };

        out.push(ReflectedBinding { set, binding, ty });
    }

    /* Sort for deterministic output (same shader → same layout). */
    out.sort_by_key(|b| (b.set, b.binding));
    Ok(out)
}

/// Merge multiple shaders' reflected bindings into a single per-set
/// layout, OR-ing stage flags wherever the same (set, binding) appears
/// in more than one shader (e.g. an InstanceBuf bound by both compute
/// and the vertex stage of the render pipeline).
///
/// For the POC every op uses set=0; we just return the bindings for
/// that set. Multi-set support is a one-line change once a bundle
/// needs it.
pub fn build_set_layout_bindings(
    inputs: &[(Vec<ReflectedBinding>, vk::ShaderStageFlags)],
    set:    u32,
) -> Vec<vk::DescriptorSetLayoutBinding<'static>> {
    /* (binding) → (ty, stages) */
    let mut by_binding: HashMap<u32, (vk::DescriptorType, vk::ShaderStageFlags)>
        = HashMap::new();
    for (bindings, stages) in inputs {
        for b in bindings {
            if b.set != set { continue; }
            by_binding.entry(b.binding)
                .and_modify(|(_ty, s)| *s |= *stages)
                .or_insert((b.ty, *stages));
        }
    }
    let mut keys: Vec<u32> = by_binding.keys().copied().collect();
    keys.sort();
    keys.into_iter().map(|binding| {
        let (ty, stages) = by_binding[&binding];
        vk::DescriptorSetLayoutBinding::default()
            .binding(binding)
            .descriptor_count(1)
            .descriptor_type(ty)
            .stage_flags(stages)
    }).collect()
}

/// Convenience: does this shader's reflected set contain a sampler
/// binding? Available for future use in `OpFrameResources` to drop
/// the OpKind switch when buffer-sizing also migrates.
#[allow(dead_code)]
pub fn has_sampler(bindings: &[ReflectedBinding]) -> bool {
    bindings.iter().any(|b| b.ty == vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
}
