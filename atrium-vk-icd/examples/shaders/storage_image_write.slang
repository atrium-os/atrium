// storage_image_write.slang -- compute shader that binds a
// storage image (RWTexture2D<uint>) + an SSBO and writes
// to both.  The SSBO sentinel acts as the readback proof
// that dispatch ran; the image write exercises the storage-
// image binding wire path through the Khronos loader.
//
// Two complementary writes:
//
//   img[uint2(0,0)] = 99   -- writes the storage image at
//                             (0,0).  Tests Tier2Backend's
//                             compute_image_by_binding map
//                             + Arc 150's
//                             IMG_TABLE_DESC_BASE=72 shift.
//   data[0] = 42           -- sentinel write to the SSBO.
//                             Readback verifies this.
//
//   ATRIUM_VK_SMOKE_SEED    = anything (overwritten)
//   ATRIUM_VK_SMOKE_EXPECT  = 42
//   plus the new ATRIUM_VK_SMOKE_USE_IMAGE=1 flag in the
//   example that wires a R32_UINT VkImage at binding 1.

[[vk::binding(0, 0)]] RWStructuredBuffer<uint> data;
[[vk::binding(1, 0)]] RWTexture2D<uint> img;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    img[uint2(0u, 0u)] = 99u;
    data[0] = 42u;
}
