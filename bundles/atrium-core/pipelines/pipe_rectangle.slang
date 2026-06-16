// pipe_rectangle.slang — atrium-core's `rect` op render pipeline.
//
// Slang single-file equivalent of the previous pipe_rectangle.{vert,frag}
// pair. Both vertex and fragment entry points live in this module;
// slangc emits two SPV files, one per [shader(...)] attribute.
//
// Consumed by fresco-vulkan's indirect-instanced draw: one instance
// per rect, four vertices per instance forming a quad via
// TRIANGLE_STRIP. No vertex buffer bound — vertices are computed from
// SV_VertexID (0..3 → corners of the unit square).
//
// Bindings (Vulkan set=0):
//   binding 0  StructuredBuffer<InstanceRecord>  — written by op_rectangle.comp
//   binding 1  cbuffer Screen                    — per-frame viewport size

struct InstanceRecord {
    float4 model;   // x, y, w, h in screen pixels
    float4 color;
    float4 params;  // .x = corner radius (px)
};

[[vk::binding(0, 0)]]
StructuredBuffer<InstanceRecord> instances;

[[vk::binding(1, 0)]]
cbuffer Screen {
    float2 size;    // viewport dimensions in pixels
};

struct VSOutput {
    float4 position : SV_Position;
    float4 color    : COLOR0;
    float2 local    : TEXCOORD0;  // pixel offset within the rect (0..size)
    float3 shape    : TEXCOORD1;  // (w, h, radius) in pixels
};

/* Signed distance to a rounded box centred at the origin, half-extents `b`,
 * corner radius `r`. Negative inside, positive outside. */
float sd_round_box(float2 p, float2 b, float r)
{
    float2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, float2(0.0, 0.0))) - r;
}

[shader("vertex")]
VSOutput vert_main(uint vertex_index   : SV_VertexID,
                   uint instance_index : SV_InstanceID)
{
    InstanceRecord inst = instances[instance_index];

    /* TRIANGLE_STRIP corner ordering (CCW front face):
     *   vert 0 → (0,0)  top-left
     *   vert 1 → (1,0)  top-right
     *   vert 2 → (0,1)  bottom-left
     *   vert 3 → (1,1)  bottom-right
     */
    float2 corner = float2(
        float((vertex_index & 1) != 0),
        float((vertex_index & 2) != 0)
    );

    float2 pos = inst.model.xy + corner * inst.model.zw;

    /* Screen-pixel space → Vulkan clip space. Vulkan's clip space is
     * Y-down (clip.y = -1 is top, +1 is bottom), matching framebuffer
     * memory layout. Our wire convention is top-left origin in pixels,
     * so the mapping is the identity — no flip.
     */
    float2 clip = (pos / size) * 2.0 - 1.0;

    VSOutput o;
    o.position = float4(clip, 0.0, 1.0);
    o.color    = inst.color;
    o.local    = corner * inst.model.zw;                 // 0..(w,h)
    o.shape    = float3(inst.model.zw, inst.params.x);   // w, h, radius
    return o;
}

[shader("fragment")]
float4 frag_main(VSOutput i) : SV_Target
{
    float radius = i.shape.z;
    if (radius <= 0.0) {
        return i.color;   // square — no SDF, no AA cost
    }

    float2 half_ext = i.shape.xy * 0.5;
    float  r        = min(radius, min(half_ext.x, half_ext.y));
    float2 p        = i.local - half_ext;                 // centred pixel coord
    float  d        = sd_round_box(p, half_ext, r);

    /* 1px-ish anti-aliased coverage from the signed distance (pixels). */
    float aa    = max(fwidth(d), 1e-5);
    float alpha = saturate(0.5 - d / aa);

    return float4(i.color.rgb, i.color.a * alpha);
}
