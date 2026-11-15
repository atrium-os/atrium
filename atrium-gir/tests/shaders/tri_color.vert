#version 450
// Stage 7c-2: the 7b triangle with a colour per vertex (red, green, blue)
// and a flat integer, through the attribute ring.
layout(location = 0) out vec3 color;
layout(location = 1) flat out int tag;
void main() {
    int i = gl_VertexIndex;
    float x = i == 1 ? 0.5 : (i == 2 ? 0.25 : -0.5);
    float y = i == 2 ? 0.75 : -0.5;
    gl_Position = vec4(x, y, 0.0, 1.0);
    color = vec3(i == 0 ? 1.0 : 0.0, i == 1 ? 1.0 : 0.0, i == 2 ? 1.0 : 0.0);
    tag = 7;
}
