// Fragment shader for atrium-text's `glyph_run` op.
//
// Samples a single-channel R8 atlas for glyph coverage and emits a
// straight-alpha colour: full RGB, alpha modulated by coverage. The
// pipeline-wide blend state is SRC_ALPHA / ONE_MINUS_SRC_ALPHA
// (non-premul). Emitting premul here would double-multiply the
// coverage into the colour and produce visible darker outlines.

#version 460

layout(set = 0, binding = 2) uniform sampler2D atlas;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 0) out vec4 frag_color;

void main() {
    float coverage = texture(atlas, v_uv).r;
    frag_color = vec4(v_color.rgb, v_color.a * coverage);
}
