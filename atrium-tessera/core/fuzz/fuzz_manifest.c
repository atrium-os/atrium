/* Fuzz tessera_manifest_parse() and every accessor that walks its body.
 *
 * WHY THIS TARGET. A manifest is bytes read off disk and then walked
 * entry-by-entry by the kmod on the read path. Its header carries an
 * entry_count and its variable-length kinds (DIRECTORY, XATTR_STORE,
 * DIRECTORY_BTREE) carry per-record lengths — all attacker- or
 * corruption-controlled. tessera_decode_manifest_header() validates a 4-byte
 * magic and NOTHING ELSE: no CRC, no length agreement. So every field below
 * the magic is untrusted input that reaches pointer arithmetic.
 *
 * THE MAGIC IS FORCED, deliberately. Left alone, a mutation-based fuzzer
 * spends its whole budget failing the 4-byte magic compare and never reaches
 * the walks, which is the code we actually want covered. Forcing it is the
 * standard "get past the gate" transform — the gate itself is trivial and
 * already covered by test_codec. Everything after byte 4 stays exactly as the
 * fuzzer produced it.
 *
 * The input is copied into a heap buffer of EXACTLY len bytes rather than
 * being passed in place: ASAN then puts redzones flush against both ends, so a
 * one-byte overread is a report instead of a silent read of adjacent stack.
 * Passing libFuzzer's own buffer would hide exactly the bugs we are hunting.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/manifest.h"

/* Walk far enough to exercise the loops without letting a 4-billion
 * entry_count turn one input into an eternity. The variable-length walks are
 * O(idx) each, so this is O(CAP^2) work in the worst case — 256 keeps a single
 * input under a millisecond while still driving every loop well past its
 * bounds check. */
#define CAP 256u

static void
walk(const tessera_manifest_parser_t *p)
{
	uint32_t count = tessera_manifest_parser_count(p);
	uint32_t n = count < CAP ? count : CAP;

	(void)tessera_manifest_parser_kind(p);
	(void)tessera_manifest_parser_size(p);

	/* Fixed-stride kinds. */
	for (uint32_t i = 0; i < n; i++) {
		tessera_chunk_record_t      c;
		tessera_tree_record_t       t;
		tessera_dir_bucket_record_t b;
		(void)tessera_manifest_chunk_at(p, i, &c);
		(void)tessera_manifest_tree_at(p, i, &t);
		(void)tessera_manifest_dir_bucket_at(p, i, &b);
	}

	/* Variable-length kinds — the ones that add a length read out of the
	 * body to a running offset. */
	for (uint32_t i = 0; i < n; i++) {
		uint64_t ino;
		const char *name;
		uint16_t nlen;
		(void)tessera_manifest_dirent_at(p, i, &ino, &name, &nlen);

		const char *xname; uint16_t xnl;
		const uint8_t *xval; uint16_t xvl;
		if (tessera_manifest_xattr_at(p, i, &xname, &xnl, &xval, &xvl)
		    == TESSERA_OK) {
			/* TOUCH the returned range. An accessor that hands back
			 * an out-of-bounds pointer+length is only a bug when
			 * someone dereferences it — which every real caller
			 * does. Reading it here is what makes ASAN fire. */
			volatile uint8_t sink = 0;
			for (uint16_t k = 0; k < xnl; k++) sink ^= (uint8_t)xname[k];
			for (uint16_t k = 0; k < xvl; k++) sink ^= xval[k];
			(void)sink;
		}

		tessera_hash_t child;
		(void)tessera_manifest_dir_btree_inner_at(p, i, child);

		uint64_t lino; const char *lname; uint16_t lnl;
		if (tessera_manifest_dir_btree_leaf_at(p, i, &lino, &lname, &lnl)
		    == TESSERA_OK) {
			volatile uint8_t sink = 0;
			for (uint16_t k = 0; k < lnl; k++) sink ^= (uint8_t)lname[k];
			(void)sink;
		}
	}

	/* Indices past the end, and the wrap-around candidates: an accessor
	 * that computes index * sizeof(record) in 32-bit arithmetic overflows
	 * here and lands back inside the body. */
	uint32_t edge[] = { count, count + 1, CAP, 0x7fffffffu, 0x80000000u,
	                    0xfffffffeu, 0xffffffffu };
	for (unsigned e = 0; e < sizeof edge / sizeof edge[0]; e++) {
		tessera_chunk_record_t c;
		tessera_tree_record_t  t;
		uint64_t ino; const char *nm; uint16_t nl;
		(void)tessera_manifest_chunk_at(p, edge[e], &c);
		(void)tessera_manifest_tree_at(p, edge[e], &t);
		(void)tessera_manifest_dirent_at(p, edge[e], &ino, &nm, &nl);
	}

	const uint8_t *idata; size_t ilen;
	if (tessera_manifest_inline_data(p, &idata, &ilen) == TESSERA_OK
	    && idata != NULL) {
		volatile uint8_t sink = 0;
		for (size_t k = 0; k < ilen; k++) sink ^= idata[k];
		(void)sink;
	}

	(void)tessera_manifest_dir_btree_is_leaf(p);
}

int
LLVMFuzzerTestOneInput(const uint8_t *data, size_t len)
{
	if (len < TESSERA_MANIFEST_HEADER_SIZE) return 0;

	uint8_t *buf = malloc(len);          /* exact size — see header comment */
	if (buf == NULL) return 0;
	memcpy(buf, data, len);
	memcpy(buf, TESSERA_MAGIC_MANIFEST, 4);   /* forced gate, see above */

	tessera_manifest_parser_t *p = tessera_manifest_parse(buf, len);
	if (p != NULL) {
		walk(p);
		tessera_manifest_parser_free(p);
	}
	free(buf);
	return 0;
}
