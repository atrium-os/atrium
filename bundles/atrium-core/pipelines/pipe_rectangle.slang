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
};

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
    return o;
}

[shader("fragment")]
float4 frag_main(float4 color : COLOR0) : SV_Target
{
    return color;
}
