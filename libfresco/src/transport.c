/*
 * transport.c — open/mmap/ring/kqueue glue for /dev/fresco0.
 *
 * Phase 1: connection lifecycle + raw command/completion + kqueue
 * wait. Higher-level slot/CAS APIs build on top in later phases.
 */

#include "fresco.h"
#include "protocol.h"
#include "transport.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/event.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/types.h>

#include "../../fresco-kmod/fresco_ioctl.h"

/* ---------------------------------------------------------------- */
/* Lifecycle                                                          */
/* ---------------------------------------------------------------- */

fresco_t *
fresco_open(const char *dev_path)
{
        if (dev_path == NULL)
                dev_path = "/dev/fresco0";

        int fd = open(dev_path, O_RDWR);
        if (fd < 0)
                return NULL;

        void *shmem = mmap(NULL, FRESCO_SHMEM_SIZE, PROT_READ | PROT_WRITE,
            MAP_SHARED, fd, 0);
        if (shmem == MAP_FAILED) {
                int e = errno;
                close(fd);
                errno = e;
                return NULL;
        }

        fresco_t *f = calloc(1, sizeof(*f));
        if (f == NULL) {
                munmap(shmem, FRESCO_SHMEM_SIZE);
                close(fd);
                errno = ENOMEM;
                return NULL;
        }
        f->fd = fd;
        f->shmem = shmem;
        f->next_seq = 1;
        fresco_cas_cache_init(&f->cas_cache);

        /* Ask the kmod which client slot we got. The slot index
         * locates this connection's private cmd/comp rings within
         * shmem. Older kmods without this ioctl fall back to slot 0. */
        uint32_t slot = 0;
        if (ioctl(fd, FRESCO_IOC_CLIENT_ID, &slot) != 0)
                slot = 0;
        f->slot = slot;
        return f;
}

void
fresco_close(fresco_t *f)
{
        if (f == NULL)
                return;
        if (f->shmem != NULL)
                munmap((void *)f->shmem, FRESCO_SHMEM_SIZE);
        if (f->fd >= 0)
                close(f->fd);
        free(f);
}

int
fresco_fd(const fresco_t *f)
{
        return f->fd;
}

/* ---------------------------------------------------------------- */
/* Display + system info                                              */
/* ---------------------------------------------------------------- */

int
fresco_get_display(fresco_t *f, fresco_display_t *out)
{
        out->width      = fresco_ctrl_read_u32(f->shmem, FRESCO_CTRL_DISPLAY_W);
        out->height     = fresco_ctrl_read_u32(f->shmem, FRESCO_CTRL_DISPLAY_H);
        out->refresh_hz = fresco_ctrl_read_u32(f->shmem, FRESCO_CTRL_REFRESH_HZ);
        return 0;
}

int
fresco_get_system_font(fresco_t *f, fresco_hash_t out)
{
        const volatile uint8_t *src =
            (const volatile uint8_t *)f->shmem + FRESCO_CTRL_SYSTEM_FONT;
        int nonzero = 0;
        for (size_t i = 0; i < 32; i++) {
                out[i] = src[i];
                if (out[i] != 0)
                        nonzero = 1;
        }
        return nonzero;
}

int
fresco_wait(fresco_t *f, int ms)
{
        int kq = kqueue();
        if (kq < 0)
                return -1;

        struct kevent ev;
        EV_SET(&ev, f->fd, EVFILT_READ, EV_ADD | EV_ONESHOT, 0, 0, NULL);
        if (kevent(kq, &ev, 1, NULL, 0, NULL) < 0) {
                int e = errno;
                close(kq);
                errno = e;
                return -1;
        }

        struct timespec ts;
        struct timespec *tsp = NULL;
        if (ms >= 0) {
                ts.tv_sec  = ms / 1000;
                ts.tv_nsec = (long)(ms % 1000) * 1000000L;
                tsp = &ts;
        }

        int n = kevent(kq, NULL, 0, &ev, 1, tsp);
        int e = errno;
        close(kq);
        errno = e;
        if (n < 0) return -1;
        return n > 0 ? 1 : 0;
}

/* ---------------------------------------------------------------- */
/* Raw transport                                                      */
/* ---------------------------------------------------------------- */

int
fresco_raw_submit(fresco_t *f,
                  uint16_t opcode, uint16_t flags, uint32_t sequence_id,
                  const void *payload, size_t payload_len)
{
        if (payload_len > sizeof(((struct fresco_command *)0)->payload)) {
                errno = EINVAL;
                return -1;
        }

        uint32_t w = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_cmd_write(f->slot));
        uint32_t r = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_cmd_read(f->slot));
        if ((uint32_t)(w - r) >= FRESCO_CMD_RING_ENTRIES) {
                errno = EAGAIN;
                return -1;
        }

        struct fresco_command cmd;
        memset(&cmd, 0, sizeof(cmd));
        cmd.opcode      = opcode;
        /* If caller passed flags=0 for a routable op, fill in the
         * connection's default window. Explicit non-zero flags
         * override (e.g. fresco_raw_submit caller targeting another
         * window directly). */
        cmd.flags       = (flags == 0 && fresco_opcode_is_routable(opcode))
                              ? f->default_window : flags;
        cmd.sequence_id = sequence_id;
        if (payload != NULL && payload_len > 0)
                memcpy(cmd.payload, payload, payload_len);

        size_t ring_slot = w % FRESCO_CMD_RING_ENTRIES;
        volatile char *dst = (volatile char *)f->shmem
            + fresco_slot_cmd_ring_offset(f->slot)
            + ring_slot * FRESCO_CMD_ENTRY_SIZE;
        /* memcpy to a volatile region — cast away volatility, the ring
         * is single-producer (us) so there's no concurrent writer. */
        memcpy((void *)dst, &cmd, sizeof(cmd));

        fresco_ctrl_write_u32(f->shmem, fresco_ctrl_cmd_write(f->slot), w + 1);

        /* Ring the doorbell. The server polls the cmd ring at frame
         * cadence anyway, but the doorbell shortens latency. */
        uint16_t vec = 0;
        (void)ioctl(f->fd, FRESCO_IOC_DOORBELL, &vec);

        return 0;
}

uint32_t
fresco_client_slot(const fresco_t *f)
{
        return f->slot;
}

int
fresco_raw_completion_poll(fresco_t *f, fresco_completion_t *out)
{
        uint32_t w = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_comp_write(f->slot));
        uint32_t r = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_comp_read(f->slot));
        if (w == r)
                return 0;

        size_t ring_slot = r % FRESCO_CMD_RING_ENTRIES;
        const volatile char *src = (const volatile char *)f->shmem
            + fresco_slot_comp_ring_offset(f->slot)
            + ring_slot * FRESCO_CMD_ENTRY_SIZE;

        memcpy(out, (const void *)src, sizeof(*out));
        fresco_ctrl_write_u32(f->shmem, fresco_ctrl_comp_read(f->slot), r + 1);
        return 1;
}
