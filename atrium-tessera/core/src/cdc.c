/*
 * tessera-core: FastCDC content-defined chunking.
 *
 * Algorithm: Xia et al., "FastCDC: a Fast and Efficient Content-Defined
 * Chunking Approach for Data Deduplication" (USENIX ATC '16).
 *
 * One 64-bit gear-hash rolling window with normalised chunking. Two
 * masks are used so the cut-point distribution clusters near the
 * target average size (avg_chunk):
 *
 *   - byte position < min_chunk          : skip (no boundary possible)
 *   - byte position in [min_chunk, avg)  : match against MASK_S (strict
 *                                          — half the natural hit rate
 *                                          of avg_log2)
 *   - byte position in [avg, max_chunk]  : match against MASK_L (loose
 *                                          — twice the natural rate)
 *   - byte position == max_chunk         : forced cut
 *
 * The 256-entry gear table is generated at first call from a fixed-
 * seed xorshift64 PRNG. This keeps the table:
 *   1) self-contained (no external file or RNG dependency),
 *   2) deterministic across hosts and builds (boundaries are part of
 *      the content-addressed identity, so any drift breaks dedup),
 *   3) well-distributed (xorshift64 with a non-zero seed has full 2^64-1
 *      period and good byte-level randomness — enough for gear hashing).
 */

#include "tessera/cdc.h"
#include "tessera/error.h"

#include <string.h>

const tessera_cdc_params_t tessera_cdc_default_params = {
	.avg_chunk =  64u * 1024u,
	.min_chunk =  16u * 1024u,
	.max_chunk = 256u * 1024u,
};

/* ── deterministic 256-entry gear table ──────────────────────────── */

static uint64_t gear_table[256];
static int      gear_initialised = 0;

static void
gear_init(void)
{
	/* Seed: golden-ratio constant. xorshift64 needs a non-zero state. */
	uint64_t s = 0x9e3779b97f4a7c15ULL;
	for (int i = 0; i < 256; i++) {
		s ^= s << 13;
		s ^= s >> 7;
		s ^= s << 17;
		gear_table[i] = s;
	}
	gear_initialised = 1;
}

/* ── helpers ─────────────────────────────────────────────────────── */

static unsigned
log2_floor(uint32_t x)
{
	unsigned r = 0;
	while (x > 1) { x >>= 1; r++; }
	return r;
}

/* ── chunker ─────────────────────────────────────────────────────── */

int
tessera_cdc_split(const uint8_t *data, size_t len,
                  const tessera_cdc_params_t *params,
                  size_t *out_boundaries, size_t cap, size_t *n_out)
{
	if (data == NULL || params == NULL || out_boundaries == NULL ||
	    n_out == NULL)
		return TESSERA_EINVAL;
	if (params->min_chunk == 0 ||
	    params->avg_chunk < params->min_chunk ||
	    params->max_chunk < params->avg_chunk)
		return TESSERA_EINVAL;

	if (!gear_initialised) gear_init();
	*n_out = 0;
	if (len == 0) return TESSERA_OK;

	const size_t min_c = params->min_chunk;
	const size_t avg_c = params->avg_chunk;
	const size_t max_c = params->max_chunk;

	const unsigned avg_log2 = log2_floor((uint32_t)avg_c);
	const uint64_t mask_s = (avg_log2 + 1 < 64)
	    ? ((((uint64_t)1 << (avg_log2 + 1)) - 1) << 32)
	    : ~(uint64_t)0;
	const uint64_t mask_l = (avg_log2 >= 1 && avg_log2 - 1 < 64)
	    ? ((((uint64_t)1 << (avg_log2 - 1)) - 1) << 32)
	    : ~(uint64_t)0;

	size_t pos = 0;
	while (pos < len) {
		const size_t remain = len - pos;

		/* Tail shorter than min_chunk: emit a single tail boundary
		 * and stop. tessera-fs §6.5 allows the final chunk to be
		 * below min_chunk. */
		if (remain <= min_c) {
			if (*n_out >= cap) return TESSERA_EINVAL;
			out_boundaries[(*n_out)++] = len;
			break;
		}

		const size_t window_end =
		    pos + (remain < max_c ? remain : max_c);
		const size_t stage1_end =
		    pos + (avg_c < remain ? avg_c : remain);

		size_t i = pos + min_c;
		uint64_t fp = 0;

		/* Stage 1: strict mask (rare cuts) before avg_chunk. */
		while (i < stage1_end) {
			fp = (fp << 1) + gear_table[data[i]];
			if ((fp & mask_s) == 0) break;
			i++;
		}
		/* Stage 2: loose mask between avg_chunk and max_chunk. */
		if (i == stage1_end) {
			while (i < window_end) {
				fp = (fp << 1) + gear_table[data[i]];
				if ((fp & mask_l) == 0) break;
				i++;
			}
		}
		/* If stage 2 exhausts without a match, i == window_end →
		 * forced cut. Otherwise i is the matched position. */

		if (*n_out >= cap) return TESSERA_EINVAL;
		out_boundaries[(*n_out)++] = i;
		pos = i;
	}
	return TESSERA_OK;
}
