#version 450
layout(location = 0) in vec3 color;
layout(location = 1) flat in int tag;
layout(location = 0) out vec4 o;
void main() { o = vec4(color, tag == 7 ? 1.0 : 0.5); }
