// Fragment shader for atrium-core's `path` op render pipeline.
//
// Trivial pass-through, same as pipe_rectangle.frag. The interesting
// work happens in the vertex shader (rotation). Future path variants
// (full bezier strokes, anti-aliased outlines) ship their own frag.

#version 460

layout(location = 0) in vec4 v_color;
layout(location = 0) out vec4 frag_color;

void main() {
    frag_color = v_color;
}
