/*
 * cas.c — CAS upload protocol + local dedup cache.
 *
 * Implements fresco_cas_put / fresco_cas_query.
 *
 * Upload protocol (mirrors fresco-server/src/command/frontend.rs):
 *   CMD_UPLOAD_BEGIN  payload: u32 total_size, [pad u32], up to 112 B data
 *   CMD_UPLOAD_DATA   payload: u32 offset (ignored, sequential),
 *                              up to 116 B data
 *   CMD_UPLOAD_FINISH payload: 32 B reserved, u32 upload_id
 *                              (echoed in completion id)
 *   sequence_id is the same across the BEGIN/DATA/FINISH chain and
 *   correlates the in-flight upload server-side.
 *
 * Local dedup: we hash with libmd, check the cache, and skip the
 * upload entirely on hit. Saves bytes on every reused blob across
 * a session.
 */

#include "fresco.h"
#include "protocol.h"
#include "transport.h"
#include "cas.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sha256.h>     /* libmd */

#include "../../fresco-kmod/fresco_ioctl.h"

/* ---------------------------------------------------------------- */
/* Cache                                                              */
/* ---------------------------------------------------------------- */

void
fresco_cas_cache_init(struct fresco_cas_cache *c)
{
        memset(c, 0, sizeof(*c));
}

static size_t
slot_for(const uint8_t hash[32])
{
        /* First 8 bytes of SHA-256 are uniformly distributed; just
         * take them as a u64 and modulo. */
        uint64_t k =
            (uint64_t)hash[0] | ((uint64_t)hash[1] << 8) |
            ((uint64_t)hash[2] << 16) | ((uint64_t)hash[3] << 24) |
            ((uint64_t)hash[4] << 32) | ((uint64_t)hash[5] << 40) |
            ((uint64_t)hash[6] << 48) | ((uint64_t)hash[7] << 56);
        return (size_t)(k % FRESCO_CAS_CACHE_SLOTS);
}

bool
fresco_cas_cache_has(struct fresco_cas_cache *c, const uint8_t hash[32])
{
        struct fresco_cas_entry *e = &c->slots[slot_for(hash)];
        return e->present && memcmp(e->hash, hash, 32) == 0;
}

void
fresco_cas_cache_insert(struct fresco_cas_cache *c, const uint8_t hash[32])
{
        struct fresco_cas_entry *e = &c->slots[slot_for(hash)];
        memcpy(e->hash, hash, 32);
        e->present = true;
}

/* ---------------------------------------------------------------- */
/* Helpers — direct ring writes, like fresco_raw_submit but reusing  */
/* a single sequence_id across the BEGIN/DATA/FINISH chain.          */
/* ---------------------------------------------------------------- */

static void ring_doorbell(fresco_t *f);

static int
submit_with_seq(fresco_t *f, uint32_t seq, uint16_t opcode,
                const void *payload, size_t payload_len)
{
        /* Wait for ring space. Big uploads (textures) easily generate
         * thousands of UPLOAD_DATA commands; if we send them all before
         * the host drains, we overflow the 256-slot ring. Ring the
         * doorbell to wake the host, then briefly sleep. ~1 ms of
         * polling per overflow is fine — the host drains a full ring
         * (256 entries) in a single tick.
         *
         * 2-second deadline catches the pathological case where the
         * host has died and the ring will never drain. */
        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (;;) {
                uint32_t w = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_cmd_write(f->slot));
                uint32_t r = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_cmd_read(f->slot));
                if ((uint32_t)(w - r) < FRESCO_CMD_RING_ENTRIES)
                        break;
                ring_doorbell(f);
                struct timespec now;
                clock_gettime(CLOCK_MONOTONIC, &now);
                long ms = (now.tv_sec - t0.tv_sec) * 1000 +
                          (now.tv_nsec - t0.tv_nsec) / 1000000;
                if (ms >= 2000) { errno = ETIMEDOUT; return -1; }
                struct timespec sl = { 0, 1000000 };  /* 1 ms */
                nanosleep(&sl, NULL);
        }

        uint32_t w = fresco_ctrl_read_u32(f->shmem, fresco_ctrl_cmd_write(f->slot));
        struct fresco_command cmd;
        memset(&cmd, 0, sizeof(cmd));
        cmd.opcode      = opcode;
        cmd.flags       = fresco_opcode_is_routable(opcode) ? f->default_window : 0;
        cmd.sequence_id = seq;
        if (payload != NULL && payload_len > 0)
                memcpy(cmd.payload, payload, payload_len);

        size_t ring_slot = w % FRESCO_CMD_RING_ENTRIES;
        volatile char *dst = (volatile char *)f->shmem
            + fresco_slot_cmd_ring_offset(f->slot)
            + ring_slot * FRESCO_CMD_ENTRY_SIZE;
        memcpy((void *)dst, &cmd, sizeof(cmd));
        fresco_ctrl_write_u32(f->shmem, fresco_ctrl_cmd_write(f->slot), w + 1);
        return 0;
}

static void
ring_doorbell(fresco_t *f)
{
        uint16_t vec = 0;
        (void)ioctl(f->fd, FRESCO_IOC_DOORBELL, &vec);
}

static void
sha256(const void *data, size_t len, uint8_t out[32])
{
        SHA256_CTX ctx;
        SHA256_Init(&ctx);
        SHA256_Update(&ctx, data, len);
        SHA256_Final(out, &ctx);
}

/* Drain the completion ring until we see one matching `want_id`,
 * with a 2 s deadline. Returns 1 on found, 0 on timeout, -1 on err.
 * We must not assume each wake corresponds to our completion — input
 * events also wake the kqueue. */
static int
await_completion(fresco_t *f, uint32_t want_id, fresco_completion_t *out)
{
        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (;;) {
                while (fresco_raw_completion_poll(f, out) == 1) {
                        if (out->id == want_id)
                                return 1;
                        /* Async window event arrived while we were
                         * waiting — queue it for later poll. Other
                         * completions (older uploads/queries) get
                         * dropped. */
                        fresco_winevt_try_enqueue(f, out);
                }
                /* Compute remaining ms */
                struct timespec now;
                clock_gettime(CLOCK_MONOTONIC, &now);
                long elapsed_ms =
                    (now.tv_sec - t0.tv_sec) * 1000 +
                    (now.tv_nsec - t0.tv_nsec) / 1000000;
                if (elapsed_ms >= 2000) return 0;
                int rc = fresco_wait(f, (int)(2000 - elapsed_ms));
                if (rc < 0) return -1;
        }
}

/* ---------------------------------------------------------------- */
/* Public API                                                         */
/* ---------------------------------------------------------------- */

int
fresco_cas_put(fresco_t *f, const void *data, size_t len, fresco_hash_t out)
{
        if (data == NULL && len > 0) { errno = EINVAL; return -1; }

        sha256(data, len, out);

        if (fresco_cas_cache_has(&f->cas_cache, out))
                return 0;

        uint32_t seq = f->next_seq++;

        /* DMA fast path for blobs over a few KiB: memcpy into the
         * client's slot's staging window, send one CMD_UPLOAD_DMA
         * referencing the length, server reads from there. Avoids
         * fragmenting the blob into thousands of 116-byte chunks
         * that flood the cmd ring (a 4 MiB texture is ~36000 cmds
         * inline; one cmd via DMA). */
        if (len > 4096 && len <= FRESCO_SLOT_STAGING_SIZE) {
                volatile char *stg = (volatile char *)f->shmem
                    + fresco_slot_staging_offset(f->slot);
                memcpy((void *)stg, data, len);

                uint8_t pld[8];
                uint32_t len_le = (uint32_t)len;
                memcpy(pld, &len_le, 4);
                memset(pld + 4, 0, 4);
                if (submit_with_seq(f, seq, FRESCO_CMD_UPLOAD_DMA,
                                    pld, 8) != 0)
                        return -1;
                ring_doorbell(f);

                fresco_completion_t comp;
                int r = await_completion(f, seq, &comp);
                if (r < 0) return -1;
                if (r == 0) { errno = ETIMEDOUT; return -1; }
                if (comp.comp_type != FRESCO_COMP_UPLOAD_COMPLETE ||
                    comp.status    != FRESCO_STATUS_OK) {
                        errno = EIO;
                        return -1;
                }
                if (memcmp(comp.result_hash, out, 32) != 0) {
                        memcpy(out, comp.result_hash, 32);
                        errno = EPROTO;
                        return -1;
                }
                fresco_cas_cache_insert(&f->cas_cache, out);
                return 0;
        }

        const uint8_t *p = data;
        size_t left = len;

        /* CMD_UPLOAD_BEGIN payload layout:
         *   payload[0..4] = total_size (u32, LE)
         *   payload[4..8] = reserved
         *   payload[8..(8+min(112,total_size))] = first chunk
         *
         * We fill payload[0..4] with total_size, then up to 112 bytes
         * starting at payload[8]. */
        uint8_t pld[120];
        memset(pld, 0, sizeof(pld));
        uint32_t total_le = (uint32_t)len;
        memcpy(pld, &total_le, 4);
        size_t n = left < 112 ? left : 112;
        memcpy(pld + 8, p, n);
        if (submit_with_seq(f, seq, FRESCO_CMD_UPLOAD_BEGIN, pld, 8 + n) != 0)
                return -1;
        p += n;
        left -= n;

        /* CMD_UPLOAD_DATA chunks: payload[0..4] = offset (server ignores
         * but echoes), payload[4..120] = up to 116 bytes data. */
        uint32_t off = (uint32_t)n;
        while (left > 0) {
                memset(pld, 0, sizeof(pld));
                memcpy(pld, &off, 4);
                size_t m = left < 116 ? left : 116;
                memcpy(pld + 4, p, m);
                if (submit_with_seq(f, seq, FRESCO_CMD_UPLOAD_DATA,
                                    pld, 4 + m) != 0)
                        return -1;
                p   += m;
                off += (uint32_t)m;
                left -= m;
        }

        /* CMD_UPLOAD_FINISH: payload[32..36] = upload_id (returned in
         * completion.id). Reuse seq as the upload_id. */
        memset(pld, 0, sizeof(pld));
        memcpy(pld + 32, &seq, 4);
        if (submit_with_seq(f, seq, FRESCO_CMD_UPLOAD_FINISH, pld, 36) != 0)
                return -1;

        ring_doorbell(f);

        fresco_completion_t comp;
        int r = await_completion(f, seq, &comp);
        if (r < 0) return -1;
        if (r == 0) { errno = ETIMEDOUT; return -1; }
        if (comp.comp_type != FRESCO_COMP_UPLOAD_COMPLETE ||
            comp.status    != FRESCO_STATUS_OK) {
                errno = EIO;
                return -1;
        }
        if (memcmp(comp.result_hash, out, 32) != 0) {
                /* Server's hash disagrees with ours — protocol breach.
                 * Trust the server: copy theirs to `out` and don't cache
                 * (we'd cache the wrong thing). */
                memcpy(out, comp.result_hash, 32);
                errno = EPROTO;
                return -1;
        }

        fresco_cas_cache_insert(&f->cas_cache, out);
        return 0;
}

int
fresco_cas_put_texture(fresco_t *f,
                       uint32_t width, uint32_t height,
                       const void *rgba8, size_t bytes,
                       fresco_hash_t out)
{
        if (rgba8 == NULL || bytes == 0 ||
            bytes != (size_t)width * height * 4) {
                errno = EINVAL;
                return -1;
        }

        /* Pixel data first — separate blob, dedup'd independently. */
        uint8_t *pixel_blob = malloc(8 + bytes);
        if (pixel_blob == NULL) { errno = ENOMEM; return -1; }
        size_t pn = fresco_blob_pixel_data(pixel_blob, rgba8, bytes);
        fresco_hash_t pix_h;
        if (fresco_cas_put(f, pixel_blob, pn, pix_h) != 0) {
                int e = errno;
                free(pixel_blob);
                errno = e;
                return -1;
        }
        free(pixel_blob);

        /* Texture header references the pixel hash. */
        uint8_t hdr[56];
        size_t hn = fresco_blob_texture(hdr, width, height, 0, 0, 0, pix_h);
        return fresco_cas_put(f, hdr, hn, out);
}

/* ───────────────────────────────────────────────────────────────── */
/* Multi-window lifecycle — phase B1                                  */
/* ───────────────────────────────────────────────────────────────── */

int
fresco_create_window(fresco_t *f,
                     uint32_t width, uint32_t height,
                     uint32_t flags, const char *title,
                     fresco_window_t *out)
{
        if (out == NULL) { errno = EINVAL; return -1; }

        uint32_t seq = f->next_seq++;
        uint8_t pld[120];
        memset(pld, 0, sizeof(pld));
        memcpy(pld + 0,  &width,  4);
        memcpy(pld + 4,  &height, 4);
        memcpy(pld + 8,  &flags,  4);
        if (title != NULL) {
                size_t n = strnlen(title, 15);
                memcpy(pld + 12, title, n);
        }
        if (submit_with_seq(f, seq, FRESCO_CMD_CREATE_WINDOW, pld, 28) != 0)
                return -1;
        ring_doorbell(f);

        /* Server echoes our sequence_id in result_hash[0..4] so we
         * match this completion against our request even if multiple
         * CREATE_WINDOWs are in flight. */
        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (;;) {
                fresco_completion_t comp;
                while (fresco_raw_completion_poll(f, &comp) == 1) {
                        if (comp.comp_type != FRESCO_COMP_WINDOW_CREATED) {
                                fresco_winevt_try_enqueue(f, &comp);
                                continue;
                        }
                        uint32_t echoed = (uint32_t)comp.result_hash[0]
                                        | ((uint32_t)comp.result_hash[1] << 8)
                                        | ((uint32_t)comp.result_hash[2] << 16)
                                        | ((uint32_t)comp.result_hash[3] << 24);
                        if (echoed != seq) continue;
                        if (comp.status != FRESCO_STATUS_OK) {
                                errno = EIO;
                                return -1;
                        }
                        *out = (fresco_window_t)comp.id;
                        return 0;
                }
                struct timespec now;
                clock_gettime(CLOCK_MONOTONIC, &now);
                long ms = (now.tv_sec - t0.tv_sec) * 1000 +
                          (now.tv_nsec - t0.tv_nsec) / 1000000;
                if (ms >= 2000) { errno = ETIMEDOUT; return -1; }
                int rc = fresco_wait(f, (int)(2000 - ms));
                if (rc < 0) return -1;
        }
}

void
fresco_set_default_window(fresco_t *f, fresco_window_t window_id)
{
        f->default_window = window_id;
}

int
fresco_destroy_window(fresco_t *f, fresco_window_t window_id)
{
        uint8_t pld[4];
        uint32_t id32 = window_id;
        memcpy(pld, &id32, 4);
        return fresco_raw_submit(f, FRESCO_CMD_DESTROY_WINDOW, 0, 0, pld, 4);
}

int
fresco_window_set_title(fresco_t *f, fresco_window_t window_id, const char *title)
{
        if (title == NULL) { errno = EINVAL; return -1; }
        uint8_t pld[120];
        memset(pld, 0, sizeof(pld));
        uint32_t id32 = window_id;
        memcpy(pld, &id32, 4);
        size_t n = strnlen(title, 116);
        memcpy(pld + 4, title, n);
        return fresco_raw_submit(f, FRESCO_CMD_WINDOW_SET_TITLE, 0, 0,
                                 pld, 4 + n);
}

int
fresco_window_set_pos(fresco_t *f, fresco_window_t window_id, float x, float y)
{
        uint8_t pld[12];
        uint32_t id32 = window_id;
        memcpy(pld + 0, &id32, 4);
        memcpy(pld + 4, &x, 4);
        memcpy(pld + 8, &y, 4);
        return fresco_raw_submit(f, FRESCO_CMD_WINDOW_SET_POS, 0, 0, pld, 12);
}

int
fresco_window_set_size(fresco_t *f, fresco_window_t window_id,
                       uint32_t width, uint32_t height)
{
        uint8_t pld[12];
        uint32_t id32 = window_id;
        memcpy(pld + 0, &id32,   4);
        memcpy(pld + 4, &width,  4);
        memcpy(pld + 8, &height, 4);
        return fresco_raw_submit(f, FRESCO_CMD_WINDOW_SET_SIZE, 0, 0, pld, 12);
}

int
fresco_cas_query(fresco_t *f, const fresco_hash_t hash)
{
        if (fresco_cas_cache_has(&f->cas_cache, hash))
                return 1;

        uint32_t seq = f->next_seq++;
        uint8_t pld[120];
        memset(pld, 0, sizeof(pld));
        memcpy(pld, hash, 32);          /* payload[0..32] = hash to query */
        memcpy(pld + 32, &seq, 4);      /* payload[32..36] = query_id */
        if (submit_with_seq(f, seq, FRESCO_CMD_QUERY_HASH, pld, 36) != 0)
                return -1;
        ring_doorbell(f);

        fresco_completion_t comp;
        int r = await_completion(f, seq, &comp);
        if (r < 0) return -1;
        if (r == 0) { errno = ETIMEDOUT; return -1; }
        if (comp.comp_type != FRESCO_COMP_QUERY_RESULT) {
                errno = EIO;
                return -1;
        }
        if (comp.status == FRESCO_STATUS_EXISTS) {
                fresco_cas_cache_insert(&f->cas_cache, hash);
                return 1;
        }
        return 0;
}
