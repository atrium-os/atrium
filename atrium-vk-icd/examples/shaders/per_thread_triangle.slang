// per_thread_triangle.slang -- each lane computes the
// triangular sum 1+2+...+tid.x.  Combines several paths
// the existing rungs cover only individually:
//
//   * Per-thread dispatch (per_thread.slang's territory):
//     dispatch_x = 8, each lane writes data[tid.x].
//   * Per-thread loop with a thread-id-dependent trip count
//     (sum_loop's structure but with the bound coming from
//     SV_DispatchThreadID, not the SSBO).
//   * Edge cases at lane 0 (zero iterations) and lane 1
//     (one iteration), both produced by the same compiled
//     code path via spirv-opt's OpPhi.
//
// Output:
//   data[0] = 0   (empty loop)
//   data[1] = 1
//   data[2] = 1+2 = 3
//   data[3] = 1+2+3 = 6
//   data[4] = 10
//   data[5] = 15
//   data[6] = 21
//   data[7] = 28
//   sum = 0+1+3+6+10+15+21+28 = 84
//
// Run with BUFFER_U32S=8, DISPATCH_X=8, SEED=0, EXPECT=84.

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint s = 0u;
    [unroll(0)]
    for (uint i = 1u; i <= tid.x; i = i + 1u) {
        s = s + i;
    }
    data[tid.x] = s;
}
