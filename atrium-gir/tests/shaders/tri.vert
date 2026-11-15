#version 450
// Stage 7c: the 7b triangle (si_tri_vs.s) from GLSL.
void main() {
    int i = gl_VertexIndex;
    float x = i == 1 ? 0.5 : (i == 2 ? 0.25 : -0.5);
    float y = i == 2 ? 0.75 : -0.5;
    gl_Position = vec4(x, y, 0.0, 1.0);
}
