// pipe_texture.slang — atrium-core's `texture` op render pipeline.
//
// Same TRIANGLE_STRIP corner trick as pipe_rectangle; emits a UV
// in [0, 1]^2 per vertex so the fragment shader samples the bound
// texture at full extent. No per-instance UV transform yet —
// sub-rect atlas sampling lands when atrium-core grows glyph +
// nine-slice ops.

struct InstanceRecord {
    float4 model;        // x, y, w, h in screen pixels
};

[[vk::binding(0, 0)]]
StructuredBuffer<InstanceRecord> instances;

[[vk::binding(1, 0)]]
cbuffer Screen {
    float2 size;         // viewport dimensions in pixels
};

[[vk::binding(2, 0)]]
Sampler2D u_tex;         // combined image+sampler (set 0 / binding 2)

struct VSOutput {
    float4 position : SV_Position;
    float2 uv       : TEXCOORD0;
};

[shader("vertex")]
VSOutput vert_main(uint vertex_index   : SV_VertexID,
                   uint instance_index : SV_InstanceID)
{
    InstanceRecord inst = instances[instance_index];

    float2 corner = float2(
        float((vertex_index & 1) != 0),
        float((vertex_index & 2) != 0)
    );

    /* Vulkan default clip space is Y-down; identity mapping from
     * our top-left-pixel convention. */
    float2 pos  = inst.model.xy + corner * inst.model.zw;
    float2 clip = (pos / size) * 2.0 - 1.0;

    VSOutput o;
    o.position = float4(clip, 0.0, 1.0);
    o.uv       = corner;
    return o;
}

[shader("fragment")]
float4 frag_main(float2 uv : TEXCOORD0) : SV_Target
{
    return u_tex.Sample(uv);
}
