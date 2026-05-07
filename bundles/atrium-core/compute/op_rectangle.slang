// op_rectangle.slang — atrium-core's `rect` op compute kernel.
//
// Per scene node, write one InstanceRecord into the instance buffer
// and bump the atomic counter. fresco-server's per-frame compute
// pass dispatches this with one thread per scene node; the render
// pass (pipe_rectangle.slang) then reads `count` and `instances`
// via vkCmdDrawIndexedIndirectCount to issue one instanced draw
// covering every rect in one call.
//
// Scene buffer layout (matches the pre-Slang GLSL):
//   bytes  0..15   header   = uint node_count + 12 bytes pad
//   bytes 16..47   nodes[0] = float2 position + float2 size + float4 color
//   bytes 16+32*N  nodes[N]
//
// We read it through a ByteAddressBuffer — Slang has no equivalent
// of GLSL's "uint count; Node nodes[];" trailing-array struct, but
// the wire layout is preserved.

struct InstanceRecord {
    float4 model;      // x, y, w, h
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
    uint id          = tid.x;
    uint node_count  = scene.Load(0);
    if (id >= node_count) {
        return;
    }

    /* nodes[id] starts at byte 16 + 32*id. First 16 bytes =
     * position.xy + size.xy as a float4; next 16 bytes = color. */
    uint base   = 16u + 32u * id;
    float4 ps   = asfloat(scene.Load4(base));         // (px, py, sx, sy)
    float4 col  = asfloat(scene.Load4(base + 16u));

    uint slot;
    InterlockedAdd(counter[0], 1u, slot);

    InstanceRecord rec;
    rec.model = ps;
    rec.color = col;
    instances[slot] = rec;
}
