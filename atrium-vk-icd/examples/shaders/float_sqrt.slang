// float_sqrt.slang -- int -> float -> sqrt -> int round-trip.
// Exercises the bespoke backend's FP regalloc (V-regs, distinct
// from the integer W-regs the existing rungs use) and the
// GLSL.std.450 Sqrt ext-inst.  Also tests the int<->float cast
// path (Slang's `float(uint)` -> OpConvertUToF, `uint(float)`
// -> OpConvertFToU).
//
//   data[0] = uint(sqrt(float(data[0])));
//
//   seed   = 100  ->  sqrt(100.0) = 10.0 -> uint -> 10
//   expect = 10

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = uint(sqrt(float(data[0])));
}
