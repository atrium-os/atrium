// bit_chain.slang -- chains shift + xor + or in one shader so a
// single rung exercises three distinct IR opcodes:
//     OpShiftLeftLogical, OpBitwiseXor, OpBitwiseOr.
//
// Slang lowers the C-style expression directly to those SPIR-V
// instructions.  The chain forces the bespoke backend's
// regalloc to plumb intermediate values through registers
// rather than fusing into a single instruction.
//
// data[0] = ((data[0] << 4) ^ 0xFF) | 0x100
//
//   seed   = 1
//          -> (1 << 4)      = 0x10
//          -> (0x10 ^ 0xFF) = 0xEF
//          -> (0xEF | 0x100) = 0x1EF = 495
//   expect = 495
//
// Compile:
//   external/slang-bin/bin/slangc -target spirv -entry main \
//     -stage compute -o bit_chain.comp.spv bit_chain.slang

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = ((data[0] << 4u) ^ 0xFFu) | 0x100u;
}
