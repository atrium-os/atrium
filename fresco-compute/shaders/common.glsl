// fresco-compute shared declarations. Layout is normative: src/lib.rs packs
// these buffers byte for byte (std430, set 0).

// An op is five vec4s (80 bytes):
//   p0  RECT/TEXTURE/GLYPH: x, y, w, h.  PATH: cx, cy, length, width
//   p1  colour, linear RGB, straight alpha (RECT, PATH, GLYPH)
//   p2  RECT: radius.  PATH: cos, sin.  GLYPH: atlas u0, v0, u1, v1
//   p3  bounding box x0, y0, x1, y1 in whole pixels (x1/y1 exclusive)
//   p4  as uint bits: kind, texture index, 0, 0
// The buffers are flat arrays indexed by hand: the Tier-2 frontend takes a
// dynamic index only as the last step into a runtime array.
#define OP_VEC4S 5u

#define KIND_RECT    0u
#define KIND_PATH    1u
#define KIND_TEXTURE 2u
#define KIND_GLYPH   3u

#define TILE 16u

layout(std430, set = 0, binding = 0) readonly buffer FrameB {
    uint width;
    uint height;
    uint pitch;      // target pitch in pixels
    uint tiles_x;
    uint tiles_y;
    uint op_count;
    uint wpt;        // mask words per tile: ceil(op_count / 32)
    uint _pad;
    vec4 clear;      // linear RGB
} F;

layout(std430, set = 0, binding = 1) readonly buffer OpsB { vec4 opv[]; };
