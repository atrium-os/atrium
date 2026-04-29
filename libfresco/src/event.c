/*
 * event.c — input ring decoder + kqueue-driven wait helper.
 *
 * The host writes 64-byte input events into the input ring at
 * FRESCO_INPUT_RING_OFFSET; the kernel callout in fresco.ko spots
 * the input_write pointer advancing and KNOTEs the cdev's kqueue.
 * Userspace then drains via fresco_input_poll.
 */

#include "fresco.h"
#include "protocol.h"
#include "transport.h"

#include <errno.h>
#include <string.h>
#include <time.h>

/* ---------------------------------------------------------------- */
/* Async window-event queue                                          */
/* ---------------------------------------------------------------- */

int
fresco_winevt_try_enqueue(struct fresco *f, const fresco_completion_t *comp)
{
        fresco_window_event_t ev;
        ev.kind      = comp->comp_type;
        ev._pad0     = 0;
        ev.window_id = comp->id;
        ev.value_a   = 0;
        ev.value_b   = 0;

        switch (comp->comp_type) {
        case FRESCO_COMP_WINDOW_RESIZED:
                /* Server packs width/height into result_hash[0..8]. */
                memcpy(&ev.value_a, &comp->result_hash[0], 4);
                memcpy(&ev.value_b, &comp->result_hash[4], 4);
                break;
        case FRESCO_COMP_WINDOW_CLOSE_REQUESTED:
                break;
        case FRESCO_COMP_WINDOW_FOCUS:
                ev.value_a = (int32_t)comp->status;  /* 1 focused, 0 blurred */
                break;
        default:
                return 0;       /* not a window event */
        }

        uint32_t cap = FRESCO_WINEVT_QUEUE_CAP;
        uint32_t used = f->winevt.head - f->winevt.tail;
        if (used >= cap) {
                /* Full — drop oldest. POC-grade; in practice this only
                 * happens if the app stops draining for >32 events. */
                f->winevt.tail++;
        }
        f->winevt.buf[f->winevt.head % cap] = ev;
        f->winevt.head++;
        return 1;
}

int
fresco_window_event_poll(fresco_t *f, fresco_window_event_t *out)
{
        /* Drain the completion ring first so the queue reflects the
         * latest server state. Anything that's not a window event at
         * this point is unmatched (no synchronous waiter) and is
         * dropped — uploads and queries are await-completed inline. */
        for (;;) {
                fresco_completion_t comp;
                int r = fresco_raw_completion_poll(f, &comp);
                if (r <= 0) break;
                fresco_winevt_try_enqueue(f, &comp);
        }

        if (f->winevt.tail == f->winevt.head)
                return 0;
        *out = f->winevt.buf[f->winevt.tail % FRESCO_WINEVT_QUEUE_CAP];
        f->winevt.tail++;
        return 1;
}

int
fresco_event_wait(fresco_t *f,
                  fresco_input_t *in_out,
                  fresco_window_event_t *window_out,
                  int ms)
{
        /* Both rings checked first — already-queued events return
         * immediately. Window events take priority so close requests
         * aren't starved by streaming pointer moves. */
        if (fresco_window_event_poll(f, window_out) == 1)
                return 2;
        if (fresco_input_poll(f, in_out) == 1)
                return 1;
        if (ms == 0)
                return 0;

        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (;;) {
                int wait_ms = ms;
                if (ms > 0) {
                        struct timespec now;
                        clock_gettime(CLOCK_MONOTONIC, &now);
                        long elapsed_ms =
                            (now.tv_sec - t0.tv_sec) * 1000 +
                            (now.tv_nsec - t0.tv_nsec) / 1000000;
                        if (elapsed_ms >= ms) return 0;
                        wait_ms = (int)(ms - elapsed_ms);
                }
                int rc = fresco_wait(f, wait_ms);
                if (rc < 0) return -1;
                if (rc == 0) return 0;
                /* Wake fired — could be either ring. Drain both. */
                if (fresco_window_event_poll(f, window_out) == 1)
                        return 2;
                if (fresco_input_poll(f, in_out) == 1)
                        return 1;
                /* Spurious wake — keep waiting. */
        }
}

int
fresco_window_event_wait(fresco_t *f, fresco_window_event_t *out, int ms)
{
        if (fresco_window_event_poll(f, out) == 1)
                return 1;
        if (ms == 0)
                return 0;

        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (;;) {
                int wait_ms = ms;
                if (ms > 0) {
                        struct timespec now;
                        clock_gettime(CLOCK_MONOTONIC, &now);
                        long elapsed_ms =
                            (now.tv_sec - t0.tv_sec) * 1000 +
                            (now.tv_nsec - t0.tv_nsec) / 1000000;
                        if (elapsed_ms >= ms) return 0;
                        wait_ms = (int)(ms - elapsed_ms);
                }
                int rc = fresco_wait(f, wait_ms);
                if (rc < 0) return -1;
                if (rc == 0) return 0;
                if (fresco_window_event_poll(f, out) == 1)
                        return 1;
                /* Wake was for an input event — keep waiting. */
        }
}

int
fresco_input_poll(fresco_t *f, fresco_input_t *out)
{
        uint32_t w = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_input_write(f->slot));
        uint32_t r = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_input_read(f->slot));
        if (w == r)
                return 0;

        size_t ring_slot = r % FRESCO_INPUT_RING_ENTRIES;
        const volatile char *src = (const volatile char *)f->shmem
            + fresco_slot_input_ring_offset(f->slot)
            + ring_slot * FRESCO_INPUT_ENTRY_SIZE;

        struct fresco_input_event ev;
        memcpy(&ev, (const void *)src, sizeof(ev));

        out->event_type    = ev.event_type;
        out->code          = ev.code;
        out->value_a       = ev.value_a;
        out->value_b       = ev.value_b;
        out->target_window = ev.target_window;

        fresco_ctrl_write_u32(f->shmem, fresco_ctrl_input_read(f->slot), r + 1);
        return 1;
}

int
fresco_input_wait(fresco_t *f, fresco_input_t *out, int ms)
{
        /* Fast path — already something in the ring. */
        if (fresco_input_poll(f, out) == 1)
                return 1;
        if (ms == 0)
                return 0;

        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (;;) {
                int wait_ms = ms;
                if (ms > 0) {
                        struct timespec now;
                        clock_gettime(CLOCK_MONOTONIC, &now);
                        long elapsed_ms =
                            (now.tv_sec - t0.tv_sec) * 1000 +
                            (now.tv_nsec - t0.tv_nsec) / 1000000;
                        if (elapsed_ms >= ms) return 0;
                        wait_ms = (int)(ms - elapsed_ms);
                }
                int rc = fresco_wait(f, wait_ms);
                if (rc < 0) return -1;
                if (rc == 0) return 0;          /* timeout */
                /* Wake fired (could be input or completion). Try to drain. */
                if (fresco_input_poll(f, out) == 1)
                        return 1;
                /* Spurious wake (likely completion ring) — keep waiting. */
        }
}
