/* Fuzz tessera_pack_open() and the reader accessors.
 *
 * WHY THIS TARGET. A pack is the on-disk container for blob bytes. Its 4096-B
 * header carries index_blocks, bloom_bytes, data_offset and data_length —
 * every one of which becomes pointer arithmetic inside tessera_pack_open():
 *
 *     r->bloom_base = r->index_base + index_blocks * TESSERA_SECTOR_SIZE;
 *     r->data_base  = data + r->header.data_offset;
 *     ... tessera_crc32(r->data_base, r->header.data_length)
 *
 * These are read off disk. A corrupt or hostile pack controls them directly.
 *
 * TWO GATES ARE FORCED so the fuzzer reaches that arithmetic at all:
 *   - the 8-byte magic, and
 *   - crc32_header, recomputed over the mutated header.
 * A blind mutator cannot produce a correct CRC32 over 88 bytes it is also
 * mutating; without this the target would explore nothing but the reject path.
 * Both gates are pure integrity checks already covered by test_codec — what is
 * NOT covered, and what this target exists to reach, is everything the reader
 * does once it believes the header.
 *
 * total_pack_bytes is NOT forced. tessera_pack_open() checks it against len,
 * and whether that check is sufficient to bound data_offset/data_length is
 * precisely the question, so the fuzzer must be free to set it.
 *
 * Exact-size heap buffer, for the reason given in fuzz_manifest.c.
 */
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/pack.h"
#include "tessera/crc.h"

#define CAP 256u

int
LLVMFuzzerTestOneInput(const uint8_t *data, size_t len)
{
	/* pack_open's own minimum: a header sector plus a footer sector. */
	if (len < TESSERA_SECTOR_SIZE * 2) return 0;
	if (len > (1u << 20)) return 0;          /* keep inputs cheap to CRC */

	uint8_t *buf = malloc(len);
	if (buf == NULL) return 0;
	memcpy(buf, data, len);

	memcpy(buf, TESSERA_MAGIC_PACK, 8);
	/* total_pack_bytes must equal len or pack_open rejects immediately. A
	 * mutator cannot guess an 8-byte value that tracks the input size it is
	 * also changing — left free, the fuzzer spent 27.8 MILLION executions at
	 * cov:2, i.e. it never once got inside. Setting it is what makes every
	 * OTHER header field reachable, and those are the fields that become
	 * pointer arithmetic. The check itself is one comparison, verifiable by
	 * reading; what it does or does not bound is the actual question. */
	uint64_t tpb = (uint64_t)len;
	memcpy(buf + offsetof(tessera_pack_header_t, total_pack_bytes), &tpb, 8);

	const size_t crc_off = offsetof(tessera_pack_header_t, crc32_header);
	uint32_t hcrc = tessera_crc32(buf, crc_off);
	memcpy(buf + crc_off, &hcrc, 4);

	tessera_pack_reader_t *r = tessera_pack_open(buf, len);
	if (r != NULL) {
		uint32_t count = tessera_pack_blob_count(r);
		uint32_t n = count < CAP ? count : CAP;
		for (uint32_t i = 0; i < n; i++) {
			tessera_hash_t h;
			if (tessera_pack_blob_hash_at(r, i, h) != TESSERA_OK)
				continue;
			/* Feed a hash the pack itself returned back into lookup
			 * and the bloom filter: the interesting path is the one
			 * where the entry EXISTS and the reader then indexes
			 * into the data region with the offset it stored. */
			const uint8_t *bytes = NULL;
			uint32_t sz = 0;
			if (tessera_pack_lookup(r, h, &bytes, &sz) == TESSERA_OK
			    && bytes != NULL) {
				/* TOUCH the blob. lookup hands back a pointer
				 * INTO the pack plus a length, both derived
				 * from the untrusted index; an out-of-range
				 * pair is only a bug once someone reads it,
				 * which every real caller does. */
				volatile uint8_t sink = 0;
				for (uint32_t k = 0; k < sz; k++) sink ^= bytes[k];
				(void)sink;
			}
			(void)tessera_pack_bloom_might_contain(r, h);
		}
		/* Past the end, and the 32-bit-overflow candidates. */
		uint32_t edge[] = { count, count + 1, 0x7fffffffu,
		                    0x80000000u, 0xffffffffu };
		for (unsigned e = 0; e < sizeof edge / sizeof edge[0]; e++) {
			tessera_hash_t h;
			(void)tessera_pack_blob_hash_at(r, edge[e], h);
		}
		tessera_pack_close(r);
	}
	free(buf);
	return 0;
}
