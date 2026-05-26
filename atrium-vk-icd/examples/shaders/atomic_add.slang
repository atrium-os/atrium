// atomic_add.slang — atomic increment of ssbo[0].
//
// Same observable effect as rmw.slang (`data[0] += 1`) but the
// SPIR-V code path is entirely different: Slang emits
// OpAtomicIAdd against a StorageBuffer pointer, which the
// frontend lowers to atrium-spv-ir's Op::AtomicIAdd, which the
// bespoke backend lowers to an ARMv8.1 LSE `ldaddal`
// instruction (see atrium-spv-backend-bespoke/tests/
// differential_compute.rs for the 16+ unit tests proving
// correctness against the cranelift oracle).
//
// This rung is the first time those atomic paths get exercised
// end-to-end through the Khronos Vulkan loader.
//
// Compile (per docs/LANGUAGE-POLICY.md, no `-profile`):
//   external/slang-bin/bin/slangc -target spirv -entry main \
//     -stage compute -o atomic_add.comp.spv atomic_add.slang

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint old;
    InterlockedAdd(data[0], 1u, old);
}
