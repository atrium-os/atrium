// per_thread.slang -- write data[tid.x] = tid.x * 2u, with
// dispatch(N, 1, 1).  First slang rung that exercises:
//
//   * SV_DispatchThreadID propagation through to the lane-id
//     in the bespoke backend's cs_main signature
//   * Indexed SSBO writes (OpAccessChain with a runtime
//     index, not a literal 0)
//   * The Tier2Backend's workgroup-parallel dispatcher walking
//     more than one workgroup
//
//   For dispatch(8, 1, 1) the per-slot writes are
//      data[0] = 0, data[1] = 2, ..., data[7] = 14
//   sum = 0+2+4+6+8+10+12+14 = 56.
//
// The example asserts against the sum for BUFFER_U32S > 1.
//
//   ATRIUM_VK_SMOKE_BUFFER_U32S = 8
//   ATRIUM_VK_SMOKE_DISPATCH_X  = 8
//   ATRIUM_VK_SMOKE_SEED        = 0   (any; gets overwritten)
//   ATRIUM_VK_SMOKE_EXPECT      = 56

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[tid.x] = tid.x * 2u;
}
