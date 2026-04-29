/*
 * transport.h — internal handle and ring helpers.
 */

#ifndef _FRESCO_TRANSPORT_H_
#define _FRESCO_TRANSPORT_H_

#include <stdint.h>

#include "cas.h"
#include "fresco.h"

/* Opcodes that operate on per-window scene/slot state. */
static inline int
fresco_opcode_is_routable(uint16_t opcode)
{
    switch (opcode) {
    case FRESCO_CMD_SET_ROOT:
    case FRESCO_CMD_SET_CAMERA:
    case FRESCO_CMD_SLOT_ALLOC:
    case FRESCO_CMD_SLOT_FREE:
    case FRESCO_CMD_SLOT_SET_XFORM:
    case FRESCO_CMD_SLOT_SET_CONTENT:
    case FRESCO_CMD_SLOT_SET_CHILDREN:
    case FRESCO_CMD_SLOT_SET_FLAGS:
    case FRESCO_CMD_SLOT_SET_ROOT:
    case FRESCO_CMD_SLOT_SET_TEXT:
    case FRESCO_CMD_SLOT_SET_CAS_CHILDREN:
    case FRESCO_CMD_RENDER:
    case FRESCO_CMD_FRAME_BEGIN:
    case FRESCO_CMD_FRAME_END:
        return 1;
    default:
        return 0;
    }
}

/* Async window-lifecycle events stashed by the completion-drain
 * helpers when they encounter CLOSE_REQUESTED / RESIZED / FOCUS
 * while waiting for an unrelated completion (upload, create_window,
 * query). Apps drain via fresco_window_event_poll / _wait. */
#define FRESCO_WINEVT_QUEUE_CAP 32
struct fresco_winevt_queue {
    fresco_window_event_t   buf[FRESCO_WINEVT_QUEUE_CAP];
    uint32_t                head;       /* next write */
    uint32_t                tail;       /* next read */
};

struct fresco {
    int                          fd;
    volatile void               *shmem;     /* mmap of /dev/fresco0 */
    struct fresco_cas_cache      cas_cache;
    uint32_t                     next_seq;  /* sequence_id allocator */
    uint32_t                     slot;      /* per-open client slot index */
    uint16_t                     default_window;  /* target for routable ops */
    struct fresco_winevt_queue   winevt;
};

/* Convert a wire completion into a window event and enqueue it iff
 * comp_type is one of the async lifecycle types. Returns 1 if
 * enqueued (caller should continue draining), 0 if not a window
 * event (caller decides what to do with `comp`). Drops oldest if the
 * queue is full — POC-grade backpressure. */
int fresco_winevt_try_enqueue(struct fresco *f, const fresco_completion_t *comp);

/* Volatile helpers for control-register reads/writes. The shmem
 * region is host-RAM-backed and shared with the server, so volatile
 * access is needed to defeat compiler reordering — but no fence
 * instructions are required (x86/aarch64 host coherency suffices for
 * a single-producer, single-consumer ring with monotonic indices). */

static inline uint32_t
fresco_ctrl_read_u32(volatile void *shmem, uint32_t off)
{
    return *(volatile uint32_t *)((volatile char *)shmem + off);
}

static inline void
fresco_ctrl_write_u32(volatile void *shmem, uint32_t off, uint32_t val)
{
    *(volatile uint32_t *)((volatile char *)shmem + off) = val;
}

#endif /* _FRESCO_TRANSPORT_H_ */
