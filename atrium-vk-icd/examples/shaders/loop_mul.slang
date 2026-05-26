// loop_mul.slang -- multiply ssbo[0] by 2 five times via a
// for-loop.  Exercises the SPIR-V structured-loop CFG:
//   OpLoopMerge, OpBranch (back-edge), OpBranchConditional
//   (loop-exit test), plus OpIMul for the body.
//
// The frontend's cfg.rs structured-CFG recovery is the path
// being validated here -- previously only covered by the
// differential_compute tests at the bespoke backend level;
// this is the first time it runs through the Khronos loader.
//
// data[0] *= 2; repeated 5 times
//   seed   = 1  -> 1, 2, 4, 8, 16, 32
//   expect = 32
//
// Compile:
//   external/slang-bin/bin/slangc -target spirv -entry main \
//     -stage compute -o loop_mul.comp.spv loop_mul.slang

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint v = data[0];
    [unroll(0)]  // tell slang not to unroll: we want the loop.
    for (uint i = 0u; i < 5u; i = i + 1u) {
        v = v * 2u;
    }
    data[0] = v;
}
