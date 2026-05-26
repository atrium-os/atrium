// bit_count.slang -- OpBitCount on a u32 (core SPIR-V opcode,
// distinct from the GLSL.std.450 ext-inst family that
// max_const + float_sqrt covered).
//
//   data[0] = countbits(data[0])
//
//   seed   = 0x12345678
//          -> 0001 0010 0011 0100 0101 0110 0111 1000
//          -> set bits: 1+2+2+1+2+3+3+1 = 15  (counting 1s per nibble)
//   expect = 13   (actual popcount of 0x12345678)
//
// Recount: 0x1=0001 (1), 0x2=0010 (1), 0x3=0011 (2),
//          0x4=0100 (1), 0x5=0101 (2), 0x6=0110 (2),
//          0x7=0111 (3), 0x8=1000 (1).  Total = 1+1+2+1+2+2+3+1 = 13.

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = countbits(data[0]);
}
