/*
 * smoke.c — phase-1 libfresco smoke test.
 *
 * Open /dev/fresco0 via libfresco, read display info, do a single
 * CMD_QUERY_HASH round-trip with a known-absent (all-zero) hash,
 * verify the host replies COMP_QUERY_RESULT/STATUS_NOT_FOUND.
 *
 * Replaces fresco-kmod/test_roundtrip.c at lib level. If this passes,
 * libfresco's transport layer is sound; we can move on to phase 2
 * (CAS upload + dedup).
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "fresco.h"

static double
now_ms(void)
{
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        return ts.tv_sec * 1000.0 + ts.tv_nsec / 1.0e6;
}

int
main(int argc, char **argv)
{
        const char *dev = (argc > 1) ? argv[1] : NULL;

        fresco_t *f = fresco_open(dev);
        if (f == NULL) {
                perror("fresco_open");
                return 1;
        }

        fresco_display_t disp;
        fresco_get_display(f, &disp);
        printf("display: %ux%u @ %u Hz\n", disp.width, disp.height, disp.refresh_hz);

        fresco_hash_t font;
        if (fresco_get_system_font(f, font)) {
                printf("system font hash: %02x%02x%02x%02x..\n",
                    font[0], font[1], font[2], font[3]);
        } else {
                printf("system font hash: <none>\n");
        }

        /* Issue CMD_QUERY_HASH for an all-zero hash payload. The server
         * looks at the first 32 bytes of payload as the hash to query. */
        uint8_t hash[32] = {0};
        double t0 = now_ms();
        if (fresco_raw_submit(f, FRESCO_CMD_QUERY_HASH, 0, 0xC0FFEE,
                              hash, sizeof(hash)) != 0) {
                perror("fresco_raw_submit");
                fresco_close(f);
                return 2;
        }

        /* Wait via kqueue + drain. The kernel also wakes on input-ring
         * advances (mouse moves over the host window count), so loop
         * until a completion shows up or the deadline passes. */
        fresco_completion_t comp;
        double deadline = t0 + 2000.0;
        for (;;) {
                if (fresco_raw_completion_poll(f, &comp) == 1)
                        break;
                int remaining = (int)(deadline - now_ms());
                if (remaining <= 0) {
                        fprintf(stderr, "timeout waiting for completion\n");
                        fresco_close(f);
                        return 4;
                }
                if (fresco_wait(f, remaining) < 0) {
                        perror("fresco_wait");
                        fresco_close(f);
                        return 3;
                }
        }
        double rtt_ms = now_ms() - t0;

        printf("completion: type=0x%02x status=0x%02x id=0x%08x\n",
            comp.comp_type, comp.status, comp.id);
        printf("round-trip latency: %.2f ms\n", rtt_ms);

        fresco_close(f);

        if (comp.comp_type == FRESCO_COMP_QUERY_RESULT &&
            comp.status    == FRESCO_STATUS_NOT_FOUND) {
                printf("PASS: libfresco transport is sound\n");
                return 0;
        }
        printf("FAIL: unexpected completion shape\n");
        return 5;
}
