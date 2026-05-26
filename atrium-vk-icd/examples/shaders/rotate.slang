// rotate.slang -- rotate-right-by-4 on a u32 via shift + or.
// Exercises OpShiftRightLogical (the only major shift our
// existing slang rungs hadn't reached -- bit_chain uses
// ShiftLeftLogical only).
//
// data[0] = (data[0] >> 4) | (data[0] << 28)
//
//   seed   = 0x12345678
//          -> (0x12345678 >> 4) = 0x01234567
//          -> (0x12345678 << 28) = 0x80000000  (bottom nibble 8)
//          -> 0x01234567 | 0x80000000 = 0x81234567
//   expect = 0x81234567
//
// Compile:
//   external/slang-bin/bin/slangc -target spirv -entry main \
//     -stage compute -o rotate.comp.spv rotate.slang

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = (data[0] >> 4u) | (data[0] << 28u);
}
