/*
 * Tests for tessera_cdc_split (FastCDC).
 *
 * Properties exercised:
 *   1. Determinism            — same input ⇒ same boundaries.
 *   2. Min/max enforcement    — every chunk >= min_chunk (except the
 *                                last) and <= max_chunk.
 *   3. Coverage               — last boundary == len; every byte is
 *                                covered exactly once.
 *   4. Locality / shift property
 *                              — inserting bytes at the start re-aligns
 *                                most boundaries downstream (the
 *                                rolling-hash whole point — defeats a
 *                                fixed-size chunker).
 *   5. EINVAL on bad params    — min > avg, avg > max, etc.
 *   6. Average chunk size      — across a 4 MiB random buffer with
 *                                64 KiB target, mean must land in
 *                                [40 KiB, 100 KiB] (loose bounds).
 *
 * No SHA-256 dependency; runs on macOS host.
 */

#include "tessera/cdc.h"
#include "tessera/error.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;

#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

static void
fill_xorshift(uint8_t *buf, size_t n, uint64_t seed)
{
	uint64_t s = seed ? seed : 1;
	for (size_t i = 0; i < n; i++) {
		s ^= s << 13;
		s ^= s >> 7;
		s ^= s << 17;
		buf[i] = (uint8_t)(s & 0xff);
	}
}

static void
test_determinism(void)
{
	const size_t N = 1u << 20; /* 1 MiB */
	uint8_t *buf = malloc(N);
	fill_xorshift(buf, N, 0x123456789abcdefULL);

	size_t cap = N / tessera_cdc_default_params.min_chunk + 4;
	size_t *b1 = malloc(cap * sizeof *b1);
	size_t *b2 = malloc(cap * sizeof *b2);
	size_t n1 = 0, n2 = 0;

	CHECK(tessera_cdc_split(buf, N, &tessera_cdc_default_params,
	    b1, cap, &n1) == TESSERA_OK);
	CHECK(tessera_cdc_split(buf, N, &tessera_cdc_default_params,
	    b2, cap, &n2) == TESSERA_OK);

	CHECK(n1 == n2);
	if (n1 == n2)
		CHECK(memcmp(b1, b2, n1 * sizeof *b1) == 0);

	free(b1); free(b2); free(buf);
}

static void
test_size_bounds_and_coverage(void)
{
	const tessera_cdc_params_t *P = &tessera_cdc_default_params;
	const size_t N = 4u * 1024u * 1024u; /* 4 MiB */
	uint8_t *buf = malloc(N);
	fill_xorshift(buf, N, 0xfeedfacecafebabeULL);

	size_t cap = N / P->min_chunk + 4;
	size_t *bounds = malloc(cap * sizeof *bounds);
	size_t n = 0;
	CHECK(tessera_cdc_split(buf, N, P, bounds, cap, &n) == TESSERA_OK);

	CHECK(n > 0);
	CHECK(bounds[n - 1] == N);

	size_t prev = 0;
	uint64_t total = 0;
	for (size_t i = 0; i < n; i++) {
		size_t sz = bounds[i] - prev;
		CHECK(bounds[i] > prev);            /* strictly monotonic */
		if (i + 1 < n) {                    /* not the last chunk */
			CHECK(sz >= P->min_chunk);
		}
		CHECK(sz <= P->max_chunk);
		total += sz;
		prev = bounds[i];
	}
	CHECK(total == N);

	/* Mean chunk size should land near avg_chunk (64 KiB). Loose
	 * bounds because the input is small (only ~64 chunks). */
	double mean = (double)N / (double)n;
	CHECK(mean >= 40.0 * 1024.0 && mean <= 100.0 * 1024.0);
	printf("  4 MiB / %zu chunks → mean %.0f bytes (target 65536)\n",
	    n, mean);

	free(bounds); free(buf);
}

static void
test_shift_property(void)
{
	/* Insert 7 bytes at the front. With a content-defined chunker,
	 * the boundaries from the second chunk onward should re-align
	 * (i.e. boundary[i+1] - boundary[i] mostly matches the original
	 * spacing, just shifted by 7). A fixed-size chunker would re-cut
	 * every block. Practical assertion: at least 50% of the boundary
	 * deltas after the first one match between the two streams. */
	const size_t N = 1u << 20;
	uint8_t *a = malloc(N);
	uint8_t *b = malloc(N);
	fill_xorshift(a, N, 42);
	memcpy(b + 7, a, N - 7);
	memset(b, 0xab, 7); /* prepend 7 fresh bytes */

	const tessera_cdc_params_t *P = &tessera_cdc_default_params;
	size_t cap = N / P->min_chunk + 4;
	size_t *ba = malloc(cap * sizeof *ba);
	size_t *bb = malloc(cap * sizeof *bb);
	size_t na = 0, nb = 0;
	CHECK(tessera_cdc_split(a, N, P, ba, cap, &na) == TESSERA_OK);
	CHECK(tessera_cdc_split(b, N, P, bb, cap, &nb) == TESSERA_OK);

	/* Count deltas in `a` that match a delta in `b` (off-by-7 shift). */
	size_t matched = 0;
	for (size_t i = 1; i + 1 < na; i++) {
		size_t da = ba[i] - ba[i - 1];
		for (size_t j = 1; j + 1 < nb; j++) {
			if ((bb[j] - bb[j - 1]) == da &&
			    (bb[j] == ba[i] + 7)) { matched++; break; }
		}
	}
	double frac = na > 2 ? (double)matched / (double)(na - 2) : 0.0;
	printf("  shift-property match fraction: %.2f (na=%zu, nb=%zu)\n",
	    frac, na, nb);
	CHECK(frac >= 0.5);

	free(ba); free(bb); free(a); free(b);
}

static void
test_einval(void)
{
	uint8_t buf[4096];
	size_t bounds[8];
	size_t n;
	tessera_cdc_params_t bad;

	/* min > avg */
	bad = (tessera_cdc_params_t){ .avg_chunk = 1024, .min_chunk = 2048,
	                              .max_chunk = 4096 };
	CHECK(tessera_cdc_split(buf, sizeof buf, &bad, bounds, 8, &n)
	    == TESSERA_EINVAL);

	/* max < avg */
	bad = (tessera_cdc_params_t){ .avg_chunk = 4096, .min_chunk = 1024,
	                              .max_chunk = 2048 };
	CHECK(tessera_cdc_split(buf, sizeof buf, &bad, bounds, 8, &n)
	    == TESSERA_EINVAL);

	/* min == 0 */
	bad = (tessera_cdc_params_t){ .avg_chunk = 4096, .min_chunk = 0,
	                              .max_chunk = 8192 };
	CHECK(tessera_cdc_split(buf, sizeof buf, &bad, bounds, 8, &n)
	    == TESSERA_EINVAL);

	/* NULL inputs */
	CHECK(tessera_cdc_split(NULL, 0, &tessera_cdc_default_params,
	    bounds, 8, &n) == TESSERA_EINVAL);
	CHECK(tessera_cdc_split(buf, sizeof buf, NULL,
	    bounds, 8, &n) == TESSERA_EINVAL);
}

static void
test_short_input(void)
{
	/* Input shorter than min_chunk emits exactly one boundary at len. */
	uint8_t buf[1024];
	memset(buf, 0xa5, sizeof buf);
	size_t bounds[4];
	size_t n = 0;
	CHECK(tessera_cdc_split(buf, sizeof buf, &tessera_cdc_default_params,
	    bounds, 4, &n) == TESSERA_OK);
	CHECK(n == 1);
	CHECK(bounds[0] == sizeof buf);

	/* Empty input emits zero boundaries. */
	n = 0xdead;
	CHECK(tessera_cdc_split(buf, 0, &tessera_cdc_default_params,
	    bounds, 4, &n) == TESSERA_OK);
	CHECK(n == 0);
}

int
main(void)
{
	printf("test_cdc: FastCDC determinism / bounds / coverage / shift\n");
	test_determinism();
	test_size_bounds_and_coverage();
	test_shift_property();
	test_einval();
	test_short_input();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
