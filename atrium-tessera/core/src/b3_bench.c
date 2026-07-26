/* #89 step 1: userspace BLAKE3 portable-vs-NEON bench. No kernel risk. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include "b3_blake3.h"

size_t blake3_simd_degree(void);

static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}

int main(int argc, char **argv) {
    size_t sz = (argc > 1 ? (size_t)atoi(argv[1]) : 128) * 1024;
    int iters = (argc > 2 ? atoi(argv[2]) : 200);
    uint8_t *buf = malloc(sz);
    for (size_t i = 0; i < sz; i++) buf[i] = (uint8_t)(i * 31 + 7);
    uint8_t out[32];
    /* warm */
    for (int i = 0; i < 8; i++) {
        blake3_hasher h; blake3_hasher_init(&h);
        blake3_hasher_update(&h, buf, sz); blake3_hasher_finalize(&h, out, 32);
    }
    double t0 = now();
    for (int i = 0; i < iters; i++) {
        blake3_hasher h; blake3_hasher_init(&h);
        blake3_hasher_update(&h, buf, sz); blake3_hasher_finalize(&h, out, 32);
    }
    double el = now() - t0;
    double mibs = (double)sz * iters / (1024.0*1024.0) / el;
    printf("simd_degree=%zu  size=%zuKiB  iters=%d  %.1f MiB/s  digest=",
           blake3_simd_degree(), sz/1024, iters, mibs);
    for (int i = 0; i < 8; i++) printf("%02x", out[i]);
    printf("\n");
    free(buf);
    return 0;
}
