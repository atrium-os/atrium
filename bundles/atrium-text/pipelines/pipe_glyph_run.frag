// Fragment shader for atrium-text's `glyph_run` op.
//
// Samples a single-channel R8 atlas to get glyph coverage, multiplies
// by the per-instance color, emits premultiplied output. The render
// pipeline's blend state is ONE * src + (ONE_MINUS_SRC_ALPHA) * dst,
// so this composites correctly over arbitrary backgrounds without a
// separate alpha-channel test.

#version 460

layout(set = 0, binding = 2) uniform sampler2D atlas;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 0) out vec4 frag_color;

void main() {
    float coverage = texture(atlas, v_uv).r;
    frag_color = vec4(v_color.rgb * coverage, v_color.a * coverage);
}
