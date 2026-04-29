/*
 * protocol.h — private mirror of the Fresco wire protocol.
 *
 * Mirrors fresco-server/src/command/protocol.rs (Command/Completion
 * record layout) and fresco-server/src/platform/ivshmem.rs (memory
 * layout, ring sizes, control register offsets).
 *
 * Single source of truth: the server's Rust types. This file must
 * stay byte-for-byte synchronized.
 */

#ifndef _FRESCO_PROTOCOL_H_
#define _FRESCO_PROTOCOL_H_

#include <stdint.h>

/* ---------------------------------------------------------------- */
/* Memory layout — must match ivshmem.rs                              */
/* ---------------------------------------------------------------- */

#define FRESCO_SHMEM_SIZE          (32u * 1024 * 1024)

/* Multi-client layout: per-open clients each get an 80 KiB slot
 * (cmd ring + comp ring + input ring) at SLOTS_BASE + slot *
 * SLOT_STRIDE. CAS staging is shared. The server routes each
 * input event to the owning client's slot — no broadcast ring. */
#define FRESCO_NUM_CLIENT_SLOTS    4u
#define FRESCO_SLOTS_BASE          0x10000u
#define FRESCO_SLOT_STRIDE         0x14000u
#define FRESCO_SLOT_CMD_OFFSET     0x00000u
#define FRESCO_SLOT_COMP_OFFSET    0x08000u
#define FRESCO_SLOT_INPUT_OFFSET   0x10000u

#define FRESCO_CTRL_OFFSET         0x0000u

/* Per-slot staging region for CMD_UPLOAD_DMA. 7 MiB per slot — fits
 * a 4 MiB atlas with room to spare. Used for big-blob uploads; the
 * client memcpy's the blob, then sends a single CMD_UPLOAD_DMA
 * referencing length, and the server reads directly from staging. */
#define FRESCO_STAGING_BASE        \
    (FRESCO_SLOTS_BASE + FRESCO_NUM_CLIENT_SLOTS * FRESCO_SLOT_STRIDE)
#define FRESCO_SLOT_STAGING_SIZE   0x700000u
#define FRESCO_STAGING_OFFSET      FRESCO_STAGING_BASE  /* legacy alias */

#define FRESCO_CMD_RING_ENTRIES    256u
#define FRESCO_CMD_ENTRY_SIZE      128u
#define FRESCO_INPUT_RING_ENTRIES  256u
#define FRESCO_INPUT_ENTRY_SIZE    64u

/* Control register offsets (within FRESCO_CTRL_OFFSET).
 *
 * Globals — server-only metadata:
 *   0x18  status
 *   0x1c  display_width
 *   0x20  display_height
 *   0x24  refresh_hz
 *   0x28  system_font_hash (32 bytes)
 *   0x40  slots_alive_mask  (kmod toggles bits)
 *
 * Per-client (24 bytes per slot, starting at 0x100):
 *   0x100 + 24i + 0   cmd_write    (guest writes, server reads)
 *   0x100 + 24i + 4   cmd_read     (server writes)
 *   0x100 + 24i + 8   comp_write   (server writes)
 *   0x100 + 24i + 12  comp_read    (guest writes)
 *   0x100 + 24i + 16  input_write  (server writes)
 *   0x100 + 24i + 20  input_read   (guest writes)
 */
#define FRESCO_CTRL_STATUS         0x18u
#define FRESCO_CTRL_DISPLAY_W      0x1cu
#define FRESCO_CTRL_DISPLAY_H      0x20u
#define FRESCO_CTRL_REFRESH_HZ     0x24u
#define FRESCO_CTRL_SYSTEM_FONT    0x28u  /* 32 bytes */
#define FRESCO_CTRL_SLOTS_ALIVE_MASK  0x40u

#define FRESCO_CTRL_PER_SLOT_BASE   0x100u
/* 32-byte stride: 24 bytes used (cmd/comp/input R/W pairs), 8 bytes
 * reserved for future per-slot state. Must match
 * fresco-server/src/platform/ivshmem.rs and fresco-kmod/fresco.c. */
#define FRESCO_CTRL_PER_SLOT_STRIDE 32u

static inline uint32_t fresco_slot_cmd_ring_offset(uint32_t slot)   { return FRESCO_SLOTS_BASE + slot * FRESCO_SLOT_STRIDE + FRESCO_SLOT_CMD_OFFSET; }
static inline uint32_t fresco_slot_comp_ring_offset(uint32_t slot)  { return FRESCO_SLOTS_BASE + slot * FRESCO_SLOT_STRIDE + FRESCO_SLOT_COMP_OFFSET; }
static inline uint32_t fresco_slot_input_ring_offset(uint32_t slot) { return FRESCO_SLOTS_BASE + slot * FRESCO_SLOT_STRIDE + FRESCO_SLOT_INPUT_OFFSET; }
static inline uint32_t fresco_ctrl_cmd_write(uint32_t slot)   { return FRESCO_CTRL_PER_SLOT_BASE + slot * FRESCO_CTRL_PER_SLOT_STRIDE + 0; }
static inline uint32_t fresco_ctrl_cmd_read(uint32_t slot)    { return FRESCO_CTRL_PER_SLOT_BASE + slot * FRESCO_CTRL_PER_SLOT_STRIDE + 4; }
static inline uint32_t fresco_ctrl_comp_write(uint32_t slot)  { return FRESCO_CTRL_PER_SLOT_BASE + slot * FRESCO_CTRL_PER_SLOT_STRIDE + 8; }
static inline uint32_t fresco_ctrl_comp_read(uint32_t slot)   { return FRESCO_CTRL_PER_SLOT_BASE + slot * FRESCO_CTRL_PER_SLOT_STRIDE + 12; }
static inline uint32_t fresco_ctrl_input_write(uint32_t slot) { return FRESCO_CTRL_PER_SLOT_BASE + slot * FRESCO_CTRL_PER_SLOT_STRIDE + 16; }
static inline uint32_t fresco_ctrl_input_read(uint32_t slot)  { return FRESCO_CTRL_PER_SLOT_BASE + slot * FRESCO_CTRL_PER_SLOT_STRIDE + 20; }
static inline uint32_t fresco_slot_staging_offset(uint32_t slot) { return FRESCO_STAGING_BASE + slot * FRESCO_SLOT_STAGING_SIZE; }

/* ---------------------------------------------------------------- */
/* On-wire records                                                    */
/* ---------------------------------------------------------------- */

/* 128-byte command record (guest → server). */
struct fresco_command {
    uint16_t opcode;
    uint16_t flags;
    uint32_t sequence_id;
    uint32_t payload[30];   /* 120 bytes */
} __attribute__((packed));

_Static_assert(sizeof(struct fresco_command) == 128,
               "fresco_command must be 128 bytes");

/* 128-byte completion record (server → guest).
 * Layout matches the Completion struct in protocol.rs. */
struct fresco_completion_wire {
    uint16_t comp_type;
    uint16_t status;
    uint32_t id;
    uint8_t  result_hash[32];
    uint32_t _pad[22];
} __attribute__((packed));

_Static_assert(sizeof(struct fresco_completion_wire) == 128,
               "fresco_completion_wire must be 128 bytes");

/* 64-byte input event record (server → guest). */
#define FRESCO_INPUT_KEY            1
#define FRESCO_INPUT_MOUSE_MOVE     2
#define FRESCO_INPUT_MOUSE_BUTTON   3
#define FRESCO_INPUT_SCROLL         4
#define FRESCO_INPUT_RESIZE         5

struct fresco_input_event {
    uint16_t event_type;
    uint16_t code;
    int32_t  value_a;
    int32_t  value_b;
    uint32_t target_window;     /* server-tagged target window id */
    uint32_t _pad[12];
} __attribute__((packed));

_Static_assert(sizeof(struct fresco_input_event) == 64,
               "fresco_input_event must be 64 bytes");

#endif /* _FRESCO_PROTOCOL_H_ */
