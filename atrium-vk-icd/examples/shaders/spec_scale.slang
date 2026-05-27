// spec_scale.slang -- multiply seed by a SPECIALIZATION
// CONSTANT (vs push_scale.slang's PUSH CONSTANT).
//
// Spec constants are baked into the SPIR-V at JIT-compile
// time -- the host supplies a value via VkSpecializationInfo
// on vkCreateComputePipelines, and the ICD re-keys the
// shader hash so a different value produces a different
// compiled .so.  Push constants are runtime-mutable per-
// dispatch; spec constants are immutable post-compile.
//
//   ATRIUM_VK_SMOKE_SEED      = 6
//   ATRIUM_VK_SMOKE_SPEC_U32  = 7
//   ATRIUM_VK_SMOKE_EXPECT    = 42  (6 * 7)
//
// Internally Slang lowers `[[vk::constant_id(0)]] const uint`
// to an OpSpecConstant with a SpecId=0 decoration; the
// frontend reads the decoration to find which override slot
// to apply.

[[vk::constant_id(0)]]
const uint scale = 1;

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = data[0] * scale;
}
