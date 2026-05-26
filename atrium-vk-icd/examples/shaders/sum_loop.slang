// sum_loop.slang -- accumulator loop: sum 1..=data[0].
// Distinct from loop_mul (which multiplies a single carried
// value) in that the loop carries TWO function-local
// variables (the counter `i` and the accumulator `s`) that
// both need promotion to OpPhi by spirv-opt --ssa-rewrite.
//
//   uint s = 0;
//   for (uint i = 1; i <= data[0]; i = i + 1) s = s + i;
//   data[0] = s;
//
//   seed   = 5  ->  s = 1+2+3+4+5 = 15
//   expect = 15

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint n = data[0];
    uint s = 0u;
    [unroll(0)]
    for (uint i = 1u; i <= n; i = i + 1u) {
        s = s + i;
    }
    data[0] = s;
}
