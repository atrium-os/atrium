/*
 * test_b3_simd — every compiled-in BLAKE3 backend gives the same digest.
 *
 * Content addresses are BLAKE3 digests, so a SIMD backend that differs from
 * portable in even one case would silently re-address the store. On amd64
 * this runs portable, SSE4.1 and AVX2 (as far as the CPU allows) over sizes
 * that hit every chunk/block/SIMD-degree boundary and compares them. On
 * other arches it checks the known empty-input vector and passes.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/hash.h"

int
main(void)
{
	static const size_t sizes[] = { 0, 1, 63, 64, 65, 1023, 1024, 1025,
	    2048, 3072, 4095, 4096, 4097, 8192, 15360, 16384, 31744, 65536,
	    65537, 100000, 1u << 20, (1u << 20) + 7, 4u << 20 };
	static const uint8_t empty[4] = { 0xaf, 0x13, 0x49, 0xb9 };
	size_t n = (4u << 20) + 8;
	uint8_t *d = malloc(n);
	tessera_hash_t ref, got;
	int top, bad = 0;

	if (d == NULL)
		return (2);
	for (size_t i = 0; i < n; i++)
		d[i] = (uint8_t)(i * 31 + 7);
	top = tessera_b3_simd_limit(-1);
	printf("BLAKE3 SIMD level available: %d\n", top);

	tessera_blake3_256(d, 0, got);
	if (memcmp(got, empty, sizeof(empty)) != 0) {
		printf("FAIL: BLAKE3(\"\") != af1349b9...\n");
		bad = 1;
	}
	for (size_t k = 0; k < sizeof(sizes) / sizeof(sizes[0]); k++) {
		tessera_b3_simd_limit(0);
		tessera_blake3_256(d, sizes[k], ref);
		for (int l = 1; l <= top; l++) {
			tessera_b3_simd_limit(l);
			tessera_blake3_256(d, sizes[k], got);
			if (memcmp(got, ref, sizeof(ref)) != 0) {
				printf("FAIL: size %zu level %d differs from "
				    "portable\n", sizes[k], l);
				bad = 1;
			}
		}
	}
	tessera_b3_simd_limit(2);
	free(d);
	printf(bad ? "FAIL\n" : "ok: %d level(s) byte-identical over %zu sizes\n",
	    top + 1, sizeof(sizes) / sizeof(sizes[0]));
	return (bad);
}
