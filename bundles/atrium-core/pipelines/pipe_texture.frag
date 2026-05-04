// Fragment shader for atrium-core's `texture` op.
//
// Samples the bound texture at the UV interpolated from the vertex
// shader's per-corner [0, 1] coords. The sampler binding lives at
// set 0, binding 2 — fresco-server allocates a per-slot descriptor
// set so swapping the bound vkImageView is just a per-batch
// vkCmdBindDescriptorSets, not a write-descriptor every frame.

#version 460

layout(set = 0, binding = 2) uniform sampler2D u_tex;

layout(location = 0) in  vec2 v_uv;
layout(location = 0) out vec4 frag;

void main() {
    frag = texture(u_tex, v_uv);
}
