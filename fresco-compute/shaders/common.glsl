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
//
// Bounds (threat model F2): the executor validates the header against the
// buffers before a dispatch (Frame::validate), so header-derived indices are
// in range. Indices taken from buffer CONTENT (texture index, texture
// offset and size) are checked in raster.comp against tex_count and texel_words; a
// bad one samples as zero.
//
// NaN (F2): the scene's floats are hostile, and GLSL's min/max/clamp and
// float-to-int conversion are undefined for NaN or out-of-range values, so
// executors could legally disagree. Every such use below goes through these
// helpers instead, which pick by an ordered compare (false for NaN) and
// convert only in-range values; src/reference.rs mirrors them.
#define OP_VEC4S 5u

float min_(float a, float b) { return a < b ? a : b; }   // NaN: b
float max_(float a, float b) { return a > b ? a : b; }   // NaN: b
float clamp_(float v, float lo, float hi) { return v >= lo ? (v <= hi ? v : hi) : lo; } // NaN: lo

// The largest texture edge the kernels sample; wider ones sample as zero.
#define MAX_TEX_DIM 65536u

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
    uint tex_count;  // texture-table entries that may be read
    uint texel_words; // words of texels[] that may be read
    uint _pad0;
    uint _pad1;
    uint _pad2;
    vec4 clear;      // linear RGB
} F;

layout(std430, set = 0, binding = 1) readonly buffer OpsB { vec4 opv[]; };
