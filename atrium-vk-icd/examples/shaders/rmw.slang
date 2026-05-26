// rmw.slang — read-modify-write compute that increments ssbo[0].
//
// Exercises the input-prefill path that the dispatch_compute
// loop in aqueduct-gpu-host's Tier2Backend uses: before the
// shader runs, the daemon must copy the bound buffer's current
// bytes into compute_input_by_binding so a load from ssbo[0]
// sees what the client wrote (via OP_GPU_BUFFER_WRITE) and not
// zero.  loader_compute_roundtrip with the trivial write-only
// shader can't catch a regression in that pre-fill -- this one
// does: seed = N, expect = N + 1.
//
// Compile (per docs/LANGUAGE-POLICY.md, no `-profile`):
//   external/slang-bin/bin/slangc -target spirv -entry main \
//     -stage compute -o rmw.comp.spv rmw.slang

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = data[0] + 1u;
}
