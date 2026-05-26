// groupshared_xor.slang -- workgroup-shared memory test that
// SURFACED a real architectural constraint: NOT a passing rung.
//
// [numthreads(8,1,1)] makes Slang emit the cache as a genuine
// `OpVariable Workgroup` (the [numthreads(1,1,1)] case in
// groupshared_var was folded away by Slang's optimizer).  The
// SPIR-V is well-formed; the daemon validates and dispatches.
//
// But the shader is observably incorrect on Atrium tier-2.
// Atrium's Tier2Backend::dispatch_compute runs invocations
// within a workgroup SERIALLY on one worker thread (workgroups
// parallelise across threads, lanes within a workgroup do not
// -- see tier2_backend.rs ~line 1297, "Within a workgroup the
// lz/ly/lx loop stays serial so shared-memory and ControlBarrier
// semantics are preserved per workgroup").  OpControlBarrier
// is therefore a no-op, and cross-lane shared-memory reads
// only see writes from lanes with STRICTLY LOWER tid.x.
//
// For this shader at dispatch(1,1,1) with [numthreads(8,1,1)]:
//   lane 0 writes cache[0]=0; reads cache[1]=0 (lane 1 hasn't run)
//   lane 1 writes cache[1]=1; reads cache[2]=0 (lane 2 hasn't run)
//   ...
//   lane 7 writes cache[7]=7; reads cache[0]=0 (lane 0 has run,
//                                              wrote 0)
// Result: data[i] = i ^ 0 = i, NOT the intended XOR-with-neighbor.
// Sum = 0+1+2+...+7 = 28 (vs the "real barrier" expected 24).
//
// A passing version would require multi-pass dispatcher execution
// (run-until-barrier across all lanes, sync, continue) -- a real
// change to Tier2Backend::dispatch_compute, separate arc.
//
// Kept in tree as a doc + repro of the architectural finding.
// NOT in EXTRA_SHADERS.
//
// Each lane writes its index into cache, syncs the workgroup,
// then reads its neighbour's value and XORs the two:
//
//   cache[tid.x] = tid.x;
//   barrier;
//   data[tid.x] = cache[tid.x] ^ cache[(tid.x + 1) % 8];
//
// Output:
//   data[0] = 0 ^ 1 = 1
//   data[1] = 1 ^ 2 = 3
//   data[2] = 2 ^ 3 = 1
//   data[3] = 3 ^ 4 = 7
//   data[4] = 4 ^ 5 = 1
//   data[5] = 5 ^ 6 = 3
//   data[6] = 6 ^ 7 = 1
//   data[7] = 7 ^ 0 = 7
//   sum     = 1+3+1+7+1+3+1+7 = 24
//
// Run with BUFFER_U32S=8, DISPATCH_X=1 (one workgroup of 8
// threads), SEED=0, EXPECT=24.
//
// Exercises:
//   * StorageClass Workgroup (cache lives in the dispatcher's
//     per-workgroup scratch buffer, distinct from the SSBO
//     descriptor binding)
//   * OpControlBarrier with Workgroup execution + AcquireRelease
//     memory semantics (the GroupMemoryBarrierWithGroupSync
//     lowering)
//   * Cross-invocation observable side effects -- the test only
//     passes if invocation N can SEE invocation (N+1)'s write
//     after the barrier

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

groupshared uint cache[8];

[shader("compute")]
[numthreads(8, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    cache[tid.x] = tid.x;
    GroupMemoryBarrierWithGroupSync();
    data[tid.x] = cache[tid.x] ^ cache[(tid.x + 1u) % 8u];
}
