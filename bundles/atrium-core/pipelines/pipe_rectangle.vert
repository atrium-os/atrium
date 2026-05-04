// Vertex shader for atrium-core's `rect` op render pipeline.
//
// Consumed by fresco-vulkan's indirect-instanced draw (step 8): one
// instance per rect, one of 4 vertices per instance forming a quad
// via TRIANGLE_STRIP. We don't bind a vertex buffer — vertices are
// computed from gl_VertexIndex (0..3 → corners of the unit square).
//
// `Instances` is the same buffer the compute kernel
// (op_rectangle.comp) wrote into; `Screen` is per-frame uniforms.

#version 460

struct InstanceRecord {
    vec4 model;     /* x, y, w, h in screen pixels */
    vec4 color;
};

layout(set = 0, binding = 0) readonly buffer InstanceBuf {
    InstanceRecord instances[];
} instance_buf;

layout(set = 0, binding = 1) uniform Screen {
    vec2 size;      /* viewport dimensions in pixels */
} screen;

layout(location = 0) out vec4 v_color;

void main() {
    InstanceRecord inst = instance_buf.instances[gl_InstanceIndex];

    /* TRIANGLE_STRIP corner ordering (CCW front face):
     *   vert 0 → (0,0)  top-left
     *   vert 1 → (1,0)  top-right
     *   vert 2 → (0,1)  bottom-left
     *   vert 3 → (1,1)  bottom-right
     */
    vec2 corner = vec2(
        float((gl_VertexIndex & 1) != 0),
        float((gl_VertexIndex & 2) != 0)
    );

    vec2 pos = inst.model.xy + corner * inst.model.zw;

    /* Screen-pixel space → Vulkan clip space:
     *   x: [0, w] → [-1, 1]
     *   y: [0, h] → [+1, -1]   (Vulkan Y-down on framebuffer; flip
     *                            here so callers think top-left origin)
     */
    vec2 clip = (pos / screen.size) * 2.0 - 1.0;
    clip.y = -clip.y;

    gl_Position = vec4(clip, 0.0, 1.0);
    v_color = inst.color;
}
