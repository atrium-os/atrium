// pipe_path.slang — atrium-core's `path` op render pipeline.
//
// One instance per oriented rect, 4 vertices per instance via
// TRIANGLE_STRIP. Each instance carries (cx, cy, length, width,
// angle); the vertex shader builds a centered local quad of size
// (length, width), rotates by `angle` (radians, CCW around +Z),
// translates by (cx, cy), then projects to clip space.

struct InstanceRecord {
    float4 model;     // cx, cy, length, width
    float4 extra;     // angle, _, _, _
    float4 color;
};

[[vk::binding(0, 0)]]
StructuredBuffer<InstanceRecord> instances;

[[vk::binding(1, 0)]]
cbuffer Screen {
    float2 size;
};

struct VSOutput {
    float4 position : SV_Position;
    float4 color    : COLOR0;
};

[shader("vertex")]
VSOutput vert_main(uint vertex_index   : SV_VertexID,
                   uint instance_index : SV_InstanceID)
{
    InstanceRecord inst = instances[instance_index];

    /* TRIANGLE_STRIP corner ordering, in [0,1]^2 then mapped to
     * [-0.5,+0.5]^2 so the rect is centered on origin before
     * rotation. */
    float2 unit = float2(
        float((vertex_index & 1) != 0),
        float((vertex_index & 2) != 0)
    );
    float2 local = (unit - 0.5) * float2(inst.model.z, inst.model.w);

    float a = inst.extra.x;
    float c = cos(a);
    float s = sin(a);
    float2 rotated = float2(
        c * local.x - s * local.y,
        s * local.x + c * local.y
    );

    /* Vulkan default clip space is Y-down (clip.y=-1 top, +1 bottom);
     * our wire convention is top-left pixels (pos.y=0 top). Identity
     * mapping — no flip. */
    float2 pos  = inst.model.xy + rotated;
    float2 clip = (pos / size) * 2.0 - 1.0;

    VSOutput o;
    o.position = float4(clip, 0.0, 1.0);
    o.color    = inst.color;
    return o;
}

[shader("fragment")]
float4 frag_main(float4 color : COLOR0) : SV_Target
{
    return color;
}
