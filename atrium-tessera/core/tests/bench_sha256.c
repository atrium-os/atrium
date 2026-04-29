/*
 * SHA-256 microbenchmark — checks whether libmd's runtime-dispatched
 * SHA-256 lands on the ARMv8 SHA-2 hardware path (or x86 SHA-NI).
 *
 * Heuristic: ~1+ GB/s on a single core means hardware extensions are
 * in use; ~200–400 MB/s indicates the portable C fallback. The exact
 * threshold is environment-dependent (CPU clock, cache pressure), but
 * the order-of-magnitude separation between HW and SW is reliable.
 */

#include <sys/time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/tessera.h"

#define BUF_BYTES   (4u * 1024u * 1024u)   /* 4 MiB */
#define ITERATIONS  256u                    /* total: 1 GiB hashed */

static double
now_seconds(void)
{
	struct timeval tv;
	gettimeofday(&tv, NULL);
	return (double)tv.tv_sec + (double)tv.tv_usec / 1e6;
}

int
main(void)
{
	uint8_t *buf = malloc(BUF_BYTES);
	if (buf == NULL) {
		fprintf(stderr, "malloc failed\n");
		return 1;
	}
	/* Pseudo-random fill — defeats any zero-page optimisation. */
	for (size_t i = 0; i < BUF_BYTES; i++)
		buf[i] = (uint8_t)(i * 1103515245u + 12345u);

	tessera_hash_t h;
	/* Warm-up. */
	tessera_sha256(buf, BUF_BYTES, h);

	double t0 = now_seconds();
	for (unsigned i = 0; i < ITERATIONS; i++) {
		buf[0] = (uint8_t)i;   /* prevent CSE-style cheating */
		tessera_sha256(buf, BUF_BYTES, h);
	}
	double t1 = now_seconds();

	double total_bytes = (double)BUF_BYTES * (double)ITERATIONS;
	double dt = t1 - t0;
	double mibps = (total_bytes / (1024.0 * 1024.0)) / dt;

	printf("SHA-256: %.0f MiB hashed in %.3f s  =  %.1f MiB/s\n",
	    total_bytes / (1024.0 * 1024.0), dt, mibps);
	printf("final digest byte: %02x (sink to defeat DCE)\n", h[31]);

	if (mibps >= 800.0) {
		printf("=> HARDWARE path likely active (>= 800 MiB/s)\n");
		free(buf);
		return 0;
	} else {
		printf("=> SOFTWARE path likely active (< 800 MiB/s) — investigate libmd dispatch\n");
		free(buf);
		return 2;
	}
}
