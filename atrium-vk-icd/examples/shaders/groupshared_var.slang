// groupshared_var.slang -- exercise SPIR-V's StorageClass
// Workgroup (HLSL's `groupshared`).  At [numthreads(1,1,1)]
// this is a single-invocation cache, but the dispatch path is
// distinct: the bespoke backend's dispatcher allocates a
// per-workgroup scratch buffer and passes its base pointer in
// the cs_main `wg` slot, separate from the SSBO descriptor
// path that every prior rung used.
//
//   groupshared uint cache;
//   void main() {
//       cache = data[0] * 2u;
//       data[0] = cache + 1u;
//   }
//
//   seed   = 5   ->  cache = 10  ->  data[0] = 11
//   expect = 11

[[vk::binding(0, 0)]]
RWStructuredBuffer<uint> data;

groupshared uint cache;

[shader("compute")]
[numthreads(1, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    cache = data[0] * 2u;
    data[0] = cache + 1u;
}
