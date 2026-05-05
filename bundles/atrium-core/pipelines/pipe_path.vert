// Vertex shader for atrium-core's `path` op render pipeline.
//
// One instance per oriented rect, 4 vertices per instance via
// TRIANGLE_STRIP. Each instance carries (cx, cy, length, width,
// angle); the vertex shader builds a centered local quad of size
// (length, width), rotates it by `angle` (radians, CCW around +Z),
// translates by (cx, cy), then projects to clip space.
//
// `length` is the dimension along the rotation axis; `width` is
// perpendicular. With angle=0 the quad spans
// [(cx - L/2, cy - W/2), (cx + L/2, cy + W/2)].

#version 460

struct InstanceRecord {
    vec4 model;     /* cx, cy, length, width */
    vec4 extra;     /* angle, _, _, _ */
    vec4 color;
};

layout(set = 0, binding = 0) readonly buffer InstanceBuf {
    InstanceRecord instances[];
} instance_buf;

layout(set = 0, binding = 1) uniform Screen {
    vec2 size;
} screen;

layout(location = 0) out vec4 v_color;

void main() {
    InstanceRecord inst = instance_buf.instances[gl_InstanceIndex];

    /* TRIANGLE_STRIP corner ordering, in [0,1]^2 then mapped to
     * [-0.5,+0.5]^2 so the rect is centered on origin before rotation. */
    vec2 unit = vec2(
        float((gl_VertexIndex & 1) != 0),
        float((gl_VertexIndex & 2) != 0)
    );
    vec2 local = (unit - 0.5) * vec2(inst.model.z, inst.model.w);

    float a = inst.extra.x;
    float c = cos(a);
    float s = sin(a);
    vec2 rotated = vec2(
        c * local.x - s * local.y,
        s * local.x + c * local.y
    );

    vec2 pos = inst.model.xy + rotated;
    vec2 clip = (pos / screen.size) * 2.0 - 1.0;
    clip.y = -clip.y;

    gl_Position = vec4(clip, 0.0, 1.0);
    v_color = inst.color;
}
