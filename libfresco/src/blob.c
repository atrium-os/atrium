/*
 * blob.c — typed blob builders.
 *
 * Each builder writes a complete CAS-uploadable blob (8-byte header
 * + payload) into the caller's buffer and returns total length.
 *
 * Blob header layout (mirrors fresco-server/src/scene/nodes.rs):
 *   bytes 0..2  type_id  (u16, LE)
 *   bytes 2..4  version  (u16, LE)
 *   bytes 4..8  flags    (u32, LE)
 */

#include "fresco.h"

#include <string.h>

static void
write_u16le(uint8_t *p, uint16_t v)
{
        p[0] = (uint8_t)(v & 0xff);
        p[1] = (uint8_t)(v >> 8);
}

static void
write_u32le(uint8_t *p, uint32_t v)
{
        p[0] = (uint8_t)(v & 0xff);
        p[1] = (uint8_t)((v >> 8) & 0xff);
        p[2] = (uint8_t)((v >> 16) & 0xff);
        p[3] = (uint8_t)((v >> 24) & 0xff);
}

static void
write_header(uint8_t *out, uint16_t type_id, uint16_t version, uint32_t flags)
{
        write_u16le(out + 0, type_id);
        write_u16le(out + 2, version);
        write_u32le(out + 4, flags);
}

static uint32_t
pack_rgba(float r, float g, float b, float a)
{
        /* Server's rgba8_to_float() reads R from low byte. */
        uint32_t ri = (uint32_t)(r * 255.0f + 0.5f);
        uint32_t gi = (uint32_t)(g * 255.0f + 0.5f);
        uint32_t bi = (uint32_t)(b * 255.0f + 0.5f);
        uint32_t ai = (uint32_t)(a * 255.0f + 0.5f);
        if (ri > 255) ri = 255;
        if (gi > 255) gi = 255;
        if (bi > 255) bi = 255;
        if (ai > 255) ai = 255;
        return ri | (gi << 8) | (bi << 16) | (ai << 24);
}

size_t
fresco_blob_material_solid(uint8_t *out, float r, float g, float b, float a)
{
        /* Server check: NODE_MATERIAL_SOLID parser requires payload ≥ 8 B,
         * but only reads the first 4 (base_color). Pad to 8. */
        write_header(out, FRESCO_NODE_MATERIAL_SOLID, 1, 0);
        write_u32le(out + 8, pack_rgba(r, g, b, a));
        write_u32le(out + 12, 0);  /* reserved */
        return 16;
}

size_t
fresco_blob_vertex_data(uint8_t *out, const float *verts, size_t nf)
{
        write_header(out, FRESCO_NODE_VERTEX_DATA, 1, 0);
        memcpy(out + 8, verts, nf * sizeof(float));
        return 8 + nf * sizeof(float);
}

size_t
fresco_blob_index_data(uint8_t *out, const uint16_t *idx, size_t n)
{
        write_header(out, FRESCO_NODE_INDEX_DATA, 1, 0);
        memcpy(out + 8, idx, n * sizeof(uint16_t));
        return 8 + n * sizeof(uint16_t);
}

size_t
fresco_blob_mesh(uint8_t *out,
                 uint32_t vertex_count, uint32_t index_count,
                 uint32_t vertex_format_flags,
                 const fresco_hash_t vertex_hash,
                 const fresco_hash_t index_hash)
{
        write_header(out, FRESCO_NODE_MESH, 1, vertex_format_flags);
        write_u32le(out + 8,  vertex_count);
        write_u32le(out + 12, index_count);
        memcpy(out + 16, vertex_hash, 32);
        memcpy(out + 48, index_hash,  32);
        return 80;
}

size_t
fresco_blob_renderable(uint8_t *out,
                       const fresco_hash_t mesh_hash,
                       const fresco_hash_t material_hash)
{
        write_header(out, FRESCO_NODE_RENDERABLE, 1, 0);
        memcpy(out + 8,  mesh_hash,     32);
        memcpy(out + 40, material_hash, 32);
        return 72;
}

size_t
fresco_blob_transform(uint8_t *out, const float matrix[16])
{
        write_header(out, FRESCO_NODE_TRANSFORM, 1, 0);
        memcpy(out + 8, matrix, 16 * sizeof(float));
        return 72;
}

size_t
fresco_blob_camera(uint8_t *out,
                   float fov_y, float aspect,
                   float near_plane, float far_plane,
                   const fresco_hash_t view_xform)
{
        write_header(out, FRESCO_NODE_CAMERA, 1, 0);
        memcpy(out + 8,  &fov_y,      4);
        memcpy(out + 12, &aspect,     4);
        memcpy(out + 16, &near_plane, 4);
        memcpy(out + 20, &far_plane,  4);
        memcpy(out + 24, view_xform, 32);
        return 56;
}

size_t
fresco_blob_pixel_data(uint8_t *out, const void *rgba8, size_t len)
{
        write_header(out, FRESCO_NODE_PIXEL_DATA, 1, 0);
        memcpy(out + 8, rgba8, len);
        return 8 + len;
}

size_t
fresco_blob_texture(uint8_t *out,
                    uint32_t width, uint32_t height,
                    uint8_t format, uint8_t filter, uint8_t wrap,
                    const fresco_hash_t pixel_data_hash)
{
        write_header(out, FRESCO_NODE_TEXTURE, 1, 0);
        write_u32le(out + 8,  format);                          /* format       */
        write_u32le(out + 12, width);                           /* width        */
        write_u32le(out + 16, height);                          /* height       */
        write_u32le(out + 20, ((uint32_t)wrap << 8) | filter);  /* filter+wrap  */
        memcpy(out + 24, pixel_data_hash, 32);                  /* pixel ref    */
        return 56;
}

size_t
fresco_blob_material_textured(uint8_t *out,
                              const fresco_hash_t texture_hash,
                              uint32_t tint_rgba)
{
        write_header(out, FRESCO_NODE_MATERIAL_TEXTURED, 1, 0);
        memcpy(out + 8, texture_hash, 32);                      /* albedo_tex   */
        write_u32le(out + 40, tint_rgba);                       /* tint         */
        return 44;
}
