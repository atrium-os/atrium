// op_path.slang — atrium-core's `path` op compute kernel.
//
// Same shape as op_rectangle.slang: per scene node, write one
// InstanceRecord into the instance buffer and bump the atomic counter.
// "path" name is forward-looking; this op's first form is a rotated
// rect (cx, cy, length, width, angle, color), enough for clock tick
// marks, edit/term/find caret cursors. Generic-Bezier graduates to
// op_glyph (0x1003) when atrium-text bundle lands.
//
// Scene buffer layout:
//   bytes  0..15   header   = uint node_count + 12 bytes pad
//   bytes 16..63   nodes[0] = float4 model + float4 extra + float4 color  (48 B)
//   bytes 16+48*N  nodes[N]

struct InstanceRecord {
    float4 model;
    float4 extra;
    float4 color;
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

    uint base    = 16u + 48u * id;
    float4 model = asfloat(scene.Load4(base       ));
    float4 extra = asfloat(scene.Load4(base + 16u));
    float4 color = asfloat(scene.Load4(base + 32u));

    uint slot;
    InterlockedAdd(counter[0], 1u, slot);

    InstanceRecord rec;
    rec.model = model;
    rec.extra = extra;
    rec.color = color;
    instances[slot] = rec;
}
