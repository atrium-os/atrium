// ternary.slang -- C-style conditional expression.  Slang
// typically lowers `cond ? a : b` to OpSelect rather than a
// branch + phi, so this rung tests OpSelect specifically
// (distinct from branch.slang's OpPhi-at-selection-merge path).
//
//   data[0] = (data[0] & 1u) != 0 ? 1u : 0u
//
//   seed   = 7   ->  (7 & 1) != 0 -> true  -> 1
//   expect = 1

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = ((data[0] & 1u) != 0u) ? 1u : 0u;
}
