// branch.slang -- if/else with a function-local variable that's
// assigned on each side, exercising OpPhi at the selection-
// merge block (distinct from loop_mul's OpPhi at a loop header).
//
//   if (data[0] > 100)  v = data[0] - 100;
//   else                v = 0;
//   data[0] = v;
//
//   seed   = 200  ->  data[0] = 200 - 100 = 100
//   expect = 100
//
// Depends on Arc 144's spirv-opt --ssa-rewrite to promote the
// function-local `v` into an OpPhi at the merge block.  Without
// it, slang's OpVariable Function would fail compile (Arc 144).

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint v;
    if (data[0] > 100u) {
        v = data[0] - 100u;
    } else {
        v = 0u;
    }
    data[0] = v;
}
