// Vertex shader for atrium-core's `texture` op.
//
// Same TRIANGLE_STRIP corner trick as pipe_rectangle.vert; emits a UV
// in [0, 1]² per vertex so the fragment shader can sample the bound
// texture at full extent. No per-instance UV transform yet — sub-rect
// atlas sampling lands when atrium-core adds glyph + nine-slice ops.

#version 460

struct InstanceRecord {
    vec4 model;       /* x, y, w, h in screen pixels */
};

layout(set = 0, binding = 0) readonly buffer InstanceBuf {
    InstanceRecord instances[];
} instance_buf;

layout(set = 0, binding = 1) uniform Screen {
    vec2 size;        /* viewport dimensions in pixels */
} screen;

layout(location = 0) out vec2 v_uv;

void main() {
    InstanceRecord inst = instance_buf.instances[gl_InstanceIndex];

    vec2 corner = vec2(
        float((gl_VertexIndex & 1) != 0),
        float((gl_VertexIndex & 2) != 0)
    );

    /* Vulkan default clip space is Y-down (clip.y=-1 top, +1 bottom);
     * our wire convention is top-left pixels (pos.y=0 top). Identity
     * mapping — no flip. */
    vec2 pos  = inst.model.xy + corner * inst.model.zw;
    vec2 clip = (pos / screen.size) * 2.0 - 1.0;

    gl_Position = vec4(clip, 0.0, 1.0);
    v_uv = corner;
}
