// Vertex shader for atrium-text's `glyph_run` op.
//
// One instance per glyph (the compute kernel expanded each scene node
// into N InstanceRecords). 4 vertices per instance form a quad via
// TRIANGLE_STRIP. UVs are interpolated from the InstanceRecord's
// src_rect; the fragment shader samples the atlas with them.
//
// Vulkan default clip space is Y-down (clip.y=-1 top, +1 bottom);
// our wire convention is top-left pixels. Identity mapping — no flip.

#version 460

struct InstanceRecord {
    vec4 dst_rect;       /* x, y, w, h in window pixels */
    vec4 src_rect;       /* u0, v0, u1, v1 in atlas UV [0, 1] */
    vec4 color;
};

layout(set = 0, binding = 0) readonly buffer InstanceBuf {
    InstanceRecord instances[];
} instance_buf;

layout(set = 0, binding = 1) uniform Screen {
    vec2 size;
} screen;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_color;

void main() {
    InstanceRecord inst = instance_buf.instances[gl_InstanceIndex];

    /* TRIANGLE_STRIP corner ordering, [0,1]^2 unit square. */
    vec2 unit = vec2(
        float((gl_VertexIndex & 1) != 0),
        float((gl_VertexIndex & 2) != 0)
    );

    vec2 pos  = inst.dst_rect.xy + unit * inst.dst_rect.zw;
    vec2 clip = (pos / screen.size) * 2.0 - 1.0;

    gl_Position = vec4(clip, 0.0, 1.0);
    v_uv        = mix(inst.src_rect.xy, inst.src_rect.zw, unit);
    v_color     = inst.color;
}
