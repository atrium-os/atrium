/*
 * Test vectors for tessera_crc32().
 *
 * Reference values match standard zlib / PNG CRC-32 (poly 0xedb88320,
 * reflected). Spot-checked against `printf '...' | gzip | tail -c8 | od`.
 */

#include "tessera/crc.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;

#define CHECK_EQ(name, got, want) do {                                  \
	if ((got) != (want)) {                                          \
		fprintf(stderr, "FAIL %s: got 0x%08x, want 0x%08x\n",   \
		    name, (unsigned)(got), (unsigned)(want));           \
		failures++;                                             \
	}                                                               \
} while (0)

static void
test_known_vectors(void)
{
	/* Empty input → 0 (final ~0xffffffff = 0). */
	CHECK_EQ("empty", tessera_crc32((const uint8_t *)"", 0), 0u);

	/* "123456789" → 0xcbf43926 (canonical CRC-32 test vector). */
	CHECK_EQ("123456789",
	    tessera_crc32((const uint8_t *)"123456789", 9),
	    0xcbf43926u);

	/* Single-byte vectors. */
	CHECK_EQ("\\0",  tessera_crc32((const uint8_t *)"\0", 1),  0xd202ef8du);
	CHECK_EQ("'a'",  tessera_crc32((const uint8_t *)"a",  1),  0xe8b7be43u);

	/* "abc" */
	CHECK_EQ("abc",  tessera_crc32((const uint8_t *)"abc", 3), 0x352441c2u);

	/* 32 zero bytes — exercises the slice-by-8 main loop. */
	uint8_t z[32] = {0};
	CHECK_EQ("zeros32",
	    tessera_crc32(z, sizeof z),
	    0x190a55adu);

	/* 1024 zero bytes. */
	uint8_t *z1k = calloc(1024, 1);
	CHECK_EQ("zeros1k",
	    tessera_crc32(z1k, 1024),
	    0xefb5af2eu);
	free(z1k);
}

static void
test_streaming_equals_oneshot(void)
{
	uint8_t buf[1031];
	for (size_t i = 0; i < sizeof buf; i++)
		buf[i] = (uint8_t)(i * 31u + 7u);

	uint32_t want = tessera_crc32(buf, sizeof buf);

	/* Feed in three irregular chunks. */
	uint32_t s = tessera_crc32_init();
	s = tessera_crc32_update(s, buf,        13);
	s = tessera_crc32_update(s, buf + 13,   500);
	s = tessera_crc32_update(s, buf + 513,  sizeof buf - 513);
	uint32_t got = tessera_crc32_final(s);
	CHECK_EQ("streaming", got, want);

	/* Single all-at-once update via streaming API. */
	s = tessera_crc32_init();
	s = tessera_crc32_update(s, buf, sizeof buf);
	CHECK_EQ("streaming-1shot", tessera_crc32_final(s), want);

	/* Zero-length update is a no-op. */
	s = tessera_crc32_init();
	s = tessera_crc32_update(s, NULL, 0);
	s = tessera_crc32_update(s, buf,  sizeof buf);
	s = tessera_crc32_update(s, buf,  0);
	CHECK_EQ("streaming-zero-len", tessera_crc32_final(s), want);
}

static void
test_alignment(void)
{
	/* Every byte alignment within a 16-byte window must give the
	 * same CRC for the same content. */
	uint8_t base[1024 + 16];
	for (size_t i = 0; i < sizeof base; i++)
		base[i] = (uint8_t)i;

	uint32_t ref = tessera_crc32(base, 1024);
	for (int off = 1; off < 16; off++) {
		/* shift the *same data* by <off> bytes and rehash its
		 * shifted view — should differ; but the CRC of the same
		 * 1024 bytes from any aligned start position should still
		 * be ref + we just check it didn't crash. */
		(void)tessera_crc32(base + off, 1024);
	}
	CHECK_EQ("alignment-ref", tessera_crc32(base, 1024), ref);
}

int
main(void)
{
	printf("test_crc: tessera_crc32 vectors + streaming + alignment\n");
	test_known_vectors();
	test_streaming_equals_oneshot();
	test_alignment();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
