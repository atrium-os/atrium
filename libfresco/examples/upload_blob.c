/*
 * upload_blob.c — phase-2 test: upload a material_solid blob via the
 * CAS API, verify dedup, and confirm the host CAS reports STATUS_EXISTS
 * on a follow-up query.
 */

#include <stdio.h>
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

static void
print_hash(const fresco_hash_t h)
{
        for (int i = 0; i < 8; i++) printf("%02x", h[i]);
        printf("..");
}

int
main(void)
{
        fresco_t *f = fresco_open(NULL);
        if (f == NULL) { perror("fresco_open"); return 1; }

        /* Build a 12-byte solid-color material blob */
        uint8_t blob[12];
        size_t blob_len = fresco_blob_material_solid(blob, 1.0f, 0.5f, 0.0f, 1.0f);
        printf("built material_solid (orange): %zu bytes\n", blob_len);

        /* First upload — full BEGIN/FINISH round-trip. */
        fresco_hash_t h1;
        double t0 = now_ms();
        if (fresco_cas_put(f, blob, blob_len, h1) != 0) {
                perror("fresco_cas_put #1");
                fresco_close(f);
                return 2;
        }
        printf("upload #1: hash="); print_hash(h1);
        printf(" took %.2f ms\n", now_ms() - t0);

        /* Second upload of identical bytes — must hit local cache,
         * no commands sent, near-zero latency. */
        fresco_hash_t h2;
        t0 = now_ms();
        if (fresco_cas_put(f, blob, blob_len, h2) != 0) {
                perror("fresco_cas_put #2");
                fresco_close(f);
                return 3;
        }
        double dedup_ms = now_ms() - t0;
        printf("upload #2: hash="); print_hash(h2);
        printf(" took %.4f ms (deduped)\n", dedup_ms);

        if (memcmp(h1, h2, 32) != 0) {
                printf("FAIL: hashes differ across identical uploads\n");
                fresco_close(f);
                return 4;
        }

        /* Server-side query — must report EXISTS. */
        int q = fresco_cas_query(f, h1);
        printf("server query: %s\n", q == 1 ? "EXISTS" : q == 0 ? "NOT_FOUND" : "ERROR");

        /* Bigger blob (forces multi-chunk DATA path) */
        uint8_t big[1024];
        for (size_t i = 0; i < sizeof(big); i++) big[i] = (uint8_t)(i ^ 0x5a);
        fresco_hash_t hbig;
        t0 = now_ms();
        if (fresco_cas_put(f, big, sizeof(big), hbig) != 0) {
                perror("fresco_cas_put big");
                fresco_close(f);
                return 5;
        }
        printf("upload big (%zuB): hash=", sizeof(big));
        print_hash(hbig);
        printf(" took %.2f ms\n", now_ms() - t0);

        fresco_close(f);

        if (q == 1 && dedup_ms < 1.0) {
                printf("PASS: CAS upload + dedup + query all working\n");
                return 0;
        }
        printf("FAIL: dedup or query check failed\n");
        return 6;
}
