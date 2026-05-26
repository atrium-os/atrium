// subgroup_sum.slang -- WaveActiveSum across the subgroup.
// At Atrium's subgroupSize=1 (serial dispatcher; see Arc 80's
// capability allowlist) the sum is just the lane's own value,
// so the result is data[0].  We add 1u afterwards so the
// observable expect != seed when the dispatch ran.
//
//   data[0] = WaveActiveSum(data[0]) + 1u
//
//   seed   = 42  ->  WaveActiveSum(42) = 42 (size=1)
//                ->  data[0] = 42 + 1 = 43
//   expect = 43
//
// Exercises the OpGroupNonUniformIAdd path -- distinct from
// atomic_add.slang's OpAtomicIAdd despite the surface
// similarity.  Subgroup ops are a separate frontend dispatch
// arm (see atrium-spv-frontend/src/functions.rs around the
// GroupNonUniform* opcodes).

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = WaveActiveSum(data[0]) + 1u;
}
