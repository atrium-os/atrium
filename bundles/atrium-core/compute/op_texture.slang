// op_texture.slang — atrium-core's `texture` op compute kernel.
//
// Mirrors op_rectangle.slang's shape (one thread per scene node;
// atomic into instance counter; write InstanceRecord). The texture
// op only needs a per-instance model rect — the texture binding
// lives in the render pipeline's descriptor set, not here, so each
// compute dispatch is implicitly scoped to one slot.
//
// Scene buffer layout:
//   bytes  0..15   header   = uint node_count + 12 bytes pad
//   bytes 16..31   nodes[0] = float4 model    (16 B)
//   bytes 16+16*N  nodes[N]

struct InstanceRecord {
    float4 model;        // same shape; render shader generates UVs
                         // from vertex index.
};

[[vk::binding(0, 0)]]
ByteAddressBuffer scene;

[[vk::binding(1, 0)]]
RWStructuredBuffer<InstanceRecord> instances;

[[vk::binding(2, 0)]]
RWStructuredBuffer<uint> counter;

[shader("compute")]
[numthreads(64, 1, 1)]
void comp_main(uint3 tid : SV_DispatchThreadID)
{
    uint id         = tid.x;
    uint node_count = scene.Load(0);
    if (id >= node_count) {
        return;
    }

    uint base    = 16u + 16u * id;
    float4 model = asfloat(scene.Load4(base));

    uint slot;
    InterlockedAdd(counter[0], 1u, slot);

    InstanceRecord rec;
    rec.model = model;
    instances[slot] = rec;
}
