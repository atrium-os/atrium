// multi_binding.slang -- read from binding 1, write to binding 0.
// First loader-mediated rung that exercises >1 SSBO descriptor
// binding.  All previous rungs used a single binding-0 SSBO;
// this one validates:
//
//   * Tier2Backend's compute_buffer_by_binding map for >1
//     entry (previously only the binding=0 entry was tested)
//   * The Vulkan descriptor-set update path (vkUpdateDescriptorSets)
//     with multiple writes in one call
//   * The pipeline-layout / descriptor-set-layout binding
//     count > 1 path
//
//   data[0] = src[0] + data[0]
//
//   seed (binding 0) = 10
//   second_seed (binding 1) = 32
//   expect = 10 + 32 = 42

[[vk::binding(0, 0)]] RWStructuredBuffer<uint> data;
[[vk::binding(1, 0)]] RWStructuredBuffer<uint> src;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = src[0] + data[0];
}
