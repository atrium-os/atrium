// max_const.slang -- exercises GLSL.std.450 UMax via Slang's
// `max(x, y)` builtin on uints.  First rung that hits the
// frontend's GLSL.std.450 extended-instruction dispatcher
// through the loader-mediated path; previously only covered
// by the bespoke backend's differential_compute tests at
// the IR level.
//
//   data[0] = max(data[0], 100u);
//
//   seed   = 50   ->  expect = 100
//   (seed = 200 would give expect = 200; we pick the side
//    that exercises the clamping behaviour)

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    data[0] = max(data[0], 100u);
}
