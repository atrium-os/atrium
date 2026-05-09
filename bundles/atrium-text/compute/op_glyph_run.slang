// op_glyph_run.slang — atrium-text's `glyph_run` op compute kernel.
//
// Each scene node represents one shaped text run: an origin in window
// coordinates, an atlas slot reference, and N glyphs at relative
// (dx, dy) offsets from the origin. The kernel expands one node into
// N InstanceRecords for the render pipeline to draw.
//
// Scene buffer layout — 64 B per node (4× vec4):
//   bytes  0..15           header     = uint node_count + 12 B pad
//   bytes 16..31  + 64*N   nodes[N].origin     (float4)
//   bytes 32..47  + 64*N   nodes[N].atlas_dim  (float4)
//   bytes 48..63  + 64*N   nodes[N].color      (float4)
//   bytes 64..79  + 64*N   nodes[N].meta       (int4: glyph_count, glyph_offset, _, _)
// Stride 64 matches Rust's `GlyphRunNode` (4× [f32; 4]). Earlier
// kernels used stride 96 mistakenly, which corrupted reads of any
// node beyond the first within a batch — visible as "only one text
// run per atlas renders".
//
// GlyphInstance layout (32 B per glyph, packed std430-style):
//   d_offset.xy           (float2)        bytes 0..7
//   atlas_uv.xyzw         (float4)        bytes 8..23   (atlas px coords: u, v, w, h)
//                                                       NOTE: GLSL std430 vec4 must
//                                                       start on a 16-byte boundary;
//                                                       we use float4 below at offset
//                                                       16, with d_offset padded out.
//   bearing.xy            (float2)        bytes 24..31
// In std430: vec2 d_offset (8B) + 8B pad → vec4 atlas_uv (16B) → vec2 bearing (8B) +
// 8B trailing pad to stride to 32B.

struct GlyphInstance {
    float2 d_offset;       // dx, dy from run origin (in pixels)
    float2 _pad0;          // std430 padding to align next float4
    float4 atlas_uv;       // u, v, w, h in atlas pixel coords
    float2 bearing;        // bearing_x, bearing_y for baseline placement
    float2 _pad1;          // std430 trailing pad to 48B... actually 40B + pad → 48B
                           // In Slang+SPIR-V we need to match what fresco-text writes.
                           // Existing GLSL struct is exactly 48 bytes. Keep that.
};

struct InstanceRecord {
    float4 dst_rect;       // x, y, w, h in window pixels
    float4 src_rect;       // u0, v0, u1, v1 in atlas UV [0, 1]
    float4 color;
};

[[vk::binding(0, 0)]]
ByteAddressBuffer scene;

[[vk::binding(1, 0)]]
StructuredBuffer<GlyphInstance> glyphs;

[[vk::binding(2, 0)]]
RWStructuredBuffer<InstanceRecord> instances;

[[vk::binding(3, 0)]]
RWStructuredBuffer<uint> counter;

[shader("compute")]
[numthreads(64, 1, 1)]
void comp_main(uint3 tid : SV_DispatchThreadID)
{
    uint id         = tid.x;
    uint node_count = scene.Load(0);
    if (id >= node_count) {
        return;
    }

    /* Read this run's SceneNode (64 B at offset 16 + 64*id). */
    uint base       = 16u + 64u * id;
    float4 origin     = asfloat(scene.Load4(base       ));
    float4 atlas_dim  = asfloat(scene.Load4(base + 16u));
    float4 color      = asfloat(scene.Load4(base + 32u));
    int4   meta       = asint  (scene.Load4(base + 48u));

    uint glyph_count  = uint(meta.x);
    uint glyph_offset = uint(meta.y);

    /* Reserve glyph_count consecutive instance-buffer slots so this
     * run's glyphs land contiguously and can be drawn as one indirect
     * draw range. */
    uint slot_base;
    InterlockedAdd(counter[0], glyph_count, slot_base);

    for (uint i = 0u; i < glyph_count; i++) {
        GlyphInstance g = glyphs[glyph_offset + i];

        /* Destination rect in window coords. The glyph's quad starts
         * at (origin + d_offset + bearing_x,
         *     origin + d_offset - bearing_y)
         * with the FreeType convention that bearing_y is the
         * baseline-to-top offset (positive = drawn above baseline;
         * we subtract because pixel y grows downward). */
        float4 dst = float4(
            origin.x + g.d_offset.x + g.bearing.x,
            origin.y + g.d_offset.y - g.bearing.y,
            g.atlas_uv.z,
            g.atlas_uv.w
        );

        /* Atlas UVs normalised to [0, 1] from atlas pixel coords. */
        float4 src = float4(
            g.atlas_uv.x                       / atlas_dim.x,
            g.atlas_uv.y                       / atlas_dim.y,
            (g.atlas_uv.x + g.atlas_uv.z)      / atlas_dim.x,
            (g.atlas_uv.y + g.atlas_uv.w)      / atlas_dim.y
        );

        InstanceRecord rec;
        rec.dst_rect = dst;
        rec.src_rect = src;
        rec.color    = color;
        instances[slot_base + i] = rec;
    }
}
