// write_42.slang — minimal slang compute that writes 42 to an
// SSBO at descriptor (set=0, binding=0).  Used by the loader-
// mediated compute round-trip to prove that real slangc output
// flows through the entire stack (atrium-vk-icd -> aqueduct-
// gpu-host -> atrium-spv-compile -> atrium-spv-frontend ->
// IR -> bespoke backend) end-to-end.
//
// Compile to SPIR-V:
//   slangc -profile spirv_1_3 -target spirv -entry main \
//          -o write_42.comp.spv write_42.slang

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = 42u;
}
