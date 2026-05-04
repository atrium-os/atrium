// Fragment shader for atrium-core's `rect` op render pipeline.
//
// Trivial: pass the per-instance color through. Future ops with
// shading (paths, glyphs, textures) ship their own fragment shaders;
// this is just the rect case.

#version 460

layout(location = 0) in vec4 v_color;
layout(location = 0) out vec4 frag_color;

void main() {
    frag_color = v_color;
}
