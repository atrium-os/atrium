/*
 * slot.c — slot-graph command wrappers (opcodes 0x0110-0x0117).
 *
 * Payload layouts mirror fresco-server/src/command/frontend.rs.
 * Slot IDs are guest-allocated (the host's slot_table is keyed on the
 * u16 slot_id we send in CMD_SLOT_ALLOC).
 */

#include "fresco.h"
#include "protocol.h"

#include <errno.h>
#include <string.h>

static void
put_u16(uint8_t *p, uint16_t v)
{
        p[0] = (uint8_t)(v & 0xff);
        p[1] = (uint8_t)(v >> 8);
}

static void
put_u32(uint8_t *p, uint32_t v)
{
        p[0] = (uint8_t)(v & 0xff);
        p[1] = (uint8_t)((v >> 8) & 0xff);
        p[2] = (uint8_t)((v >> 16) & 0xff);
        p[3] = (uint8_t)((v >> 24) & 0xff);
}

int
fresco_slot_alloc(fresco_t *f, fresco_slot_t slot_id,
                  uint16_t node_type, uint32_t flags)
{
        /* Payload (matches handle_slot_alloc):
         *   [0..2]   slot_id
         *   [2..4]   node_type
         *   [4..8]   flags
         *   [8..40]  xform_hash    (NULL_HASH — set later)
         *   [40..72] rend_hash     (NULL_HASH — set later via SET_CONTENT)
         *   [72..74] child_count   (0)
         */
        uint8_t pld[120];
        memset(pld, 0, sizeof(pld));
        put_u16(pld + 0, slot_id);
        put_u16(pld + 2, node_type);
        put_u32(pld + 4, flags);
        return fresco_raw_submit(f, FRESCO_CMD_SLOT_ALLOC, 0, 0,
            pld, 76);
}

int
fresco_slot_free(fresco_t *f, fresco_slot_t slot_id)
{
        uint8_t pld[2];
        put_u16(pld, slot_id);
        return fresco_raw_submit(f, FRESCO_CMD_SLOT_FREE, 0, 0, pld, 2);
}

int
fresco_slot_set_xform_inline(fresco_t *f, fresco_slot_t slot_id,
                             const float xform[16])
{
        /* Payload (handle_slot_set_xform mode=1):
         *   [0..2]   slot_id
         *   [2..4]   mode = 1
         *   [4..68]  16 floats */
        uint8_t pld[68];
        memset(pld, 0, sizeof(pld));
        put_u16(pld + 0, slot_id);
        put_u16(pld + 2, 1);    /* mode 1 = inline 4x4 */
        memcpy(pld + 4, xform, 16 * sizeof(float));
        return fresco_raw_submit(f, FRESCO_CMD_SLOT_SET_XFORM, 0, 0,
            pld, sizeof(pld));
}

int
fresco_slot_set_content(fresco_t *f, fresco_slot_t slot_id,
                        const fresco_hash_t content)
{
        /* Payload (handle_slot_set_content):
         *   [0..2]    slot_id
         *   [2..34]   content hash */
        uint8_t pld[34];
        put_u16(pld + 0, slot_id);
        memcpy(pld + 2, content, 32);
        return fresco_raw_submit(f, FRESCO_CMD_SLOT_SET_CONTENT, 0, 0,
            pld, sizeof(pld));
}

int
fresco_slot_set_root(fresco_t *f, fresco_slot_t slot_id)
{
        uint8_t pld[2];
        put_u16(pld, slot_id);
        return fresco_raw_submit(f, FRESCO_CMD_SLOT_SET_ROOT, 0, 0, pld, 2);
}

int
fresco_slot_set_children(fresco_t *f, fresco_slot_t slot_id,
                         const fresco_slot_t *children, size_t n)
{
        /* Server caps at 56 children per command (handle_slot_set_children).
         * Layout (matches frontend.rs):
         *   [0..2]   slot_id
         *   [2..4]   count
         *   [4..6]   flags (unused)
         *   [6..]    u16 child slot IDs
         */
        if (n > 56) { errno = EINVAL; return -1; }
        uint8_t pld[120];
        memset(pld, 0, sizeof(pld));
        put_u16(pld + 0, slot_id);
        put_u16(pld + 2, (uint16_t)n);
        for (size_t i = 0; i < n; i++)
                put_u16(pld + 6 + i * 2, children[i]);
        return fresco_raw_submit(f, FRESCO_CMD_SLOT_SET_CHILDREN, 0, 0,
                                 pld, 6 + n * 2);
}

int
fresco_frame_begin(fresco_t *f, uint32_t frame_number)
{
        uint8_t pld[4];
        put_u32(pld, frame_number);
        return fresco_raw_submit(f, FRESCO_CMD_FRAME_BEGIN, 0, 0, pld, 4);
}

int
fresco_frame_end(fresco_t *f)
{
        return fresco_raw_submit(f, FRESCO_CMD_FRAME_END, 0, 0, NULL, 0);
}

int
fresco_set_camera(fresco_t *f, const fresco_hash_t camera_hash)
{
        /* CMD_SET_CAMERA payload: hash at offset 0 (= cmd offset 8). */
        return fresco_raw_submit(f, FRESCO_CMD_SET_CAMERA, 0, 0,
            camera_hash, 32);
}

void
fresco_matrix_identity(float out[16])
{
        memset(out, 0, 16 * sizeof(float));
        out[0] = 1.0f;
        out[5] = 1.0f;
        out[10] = 1.0f;
        out[15] = 1.0f;
}
