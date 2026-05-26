// groupshared_xor.slang -- real workgroup-shared memory +
// barrier test.  Arc 150 made this pass end-to-end through
// the Khronos Vulkan loader.
//
// [numthreads(8,1,1)] makes Slang emit the cache as a genuine
// `OpVariable Workgroup` (the [numthreads(1,1,1)] case in
// groupshared_var was folded away by Slang's optimizer).
//
// Each lane writes its index into cache, syncs the workgroup,
// then reads its neighbour's value and XORs the two:
//
//   cache[tid.x] = tid.x;
//   GroupMemoryBarrierWithGroupSync();
//   data[tid.x] = cache[tid.x] ^ cache[(tid.x + 1) % 8];
//
// Expected output (per "real barrier" semantics):
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
// Before Arc 150 this was parked: invocations within a
// workgroup ran serially on a single worker thread,
// OpControlBarrier was a no-op, cross-lane reads of
// cache[(i+1) % 8] saw zero because the later lane hadn't
// run yet -- so data[i] = i ^ 0 = i, sum 28.  The "barrier
// surfaces a real architectural constraint" finding from
// Arc 149.
//
// Arc 150 made this pass by switching the dispatcher to a
// parallel-lane mode when lx*ly*lz > 1, spawning one OS
// thread per invocation sharing a std::sync::Barrier(N),
// and wiring atrium_barrier into the image-table at offset
// IMG_TABLE_BARRIER_OFFSET=64 so the compiled cs_main can
// call it via `ldr x9, [X19, #64]; blr x9` (bespoke) or
// load+call_indirect (cranelift).
//
// Run with BUFFER_U32S=8, DISPATCH_X=1, SEED=0, EXPECT=24.
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
