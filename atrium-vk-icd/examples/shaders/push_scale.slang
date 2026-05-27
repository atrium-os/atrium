// push_scale.slang -- multiply seed by a push-constant scalar.
//
// First loader-mediated rung that exercises VkPipelineLayout
// push-constant ranges + vkCmdPushConstants + the SPIR-V
// PushConstant storage class.  All previous compute rungs
// fed inputs through the SSBO; this one feeds a single u32
// through the dedicated push-constant path the daemon
// routes via FrameOp::PushConstants -> state.push_constants
// -> the cs_main pc_ptr argument.
//
// Compile (per docs/LANGUAGE-POLICY.md: no -profile):
//   external/slang-bin/bin/slangc -target spirv -entry main \
//     -stage compute -o push_scale.comp.spv push_scale.slang
//
//   ATRIUM_VK_SMOKE_SEED      = 6
//   ATRIUM_VK_SMOKE_PUSH_U32  = 7
//   ATRIUM_VK_SMOKE_EXPECT    = 42  (6 * 7)

[[vk::push_constant]]
cbuffer Params { uint scale; };

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = data[0] * scale;
}
