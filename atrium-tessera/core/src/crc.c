/*
 * tessera-core: CRC-32 (IEEE 802.3 polynomial 0xedb88320, reflected).
 *
 * Slice-by-8 implementation: ~1+ GB/s on a modern core, no SIMD or
 * carryless-multiply required. Hardware-instruction dispatch (ARMv8
 * `crc32x`, x86 PCLMULQDQ) is reserved for v2 — slice-by-8 saturates
 * realistic disk bandwidth for v1.
 *
 * Used for: superblock CRC, journal header / record CRCs, pack header
 * / footer / pack-data CRCs, B+tree node CRCs, manifest header CRC.
 */

#include "tessera/crc.h"
#include "tessera_compat.h"

/* Compile-time endian gate: v1 targets little-endian only (aarch64 +
 * x86_64). Adding a big-endian path is a v2 portability item. */
#if defined(__BYTE_ORDER__) && __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#  error "tessera-core requires a little-endian host (v1 limitation)"
#endif

static uint32_t crc_table[8][256];
static int      crc_table_initialised = 0;

static void
crc_table_init(void)
{
	for (uint32_t i = 0; i < 256; i++) {
		uint32_t c = i;
		for (int j = 0; j < 8; j++)
			c = (c >> 1) ^ ((c & 1u) ? 0xedb88320u : 0u);
		crc_table[0][i] = c;
	}
	for (uint32_t i = 0; i < 256; i++) {
		uint32_t c = crc_table[0][i];
		for (int k = 1; k < 8; k++) {
			c = crc_table[0][c & 0xffu] ^ (c >> 8);
			crc_table[k][i] = c;
		}
	}
	crc_table_initialised = 1;
}

static inline uint32_t
le32_load(const uint8_t *p)
{
	uint32_t v;
	memcpy(&v, p, 4);
	return v;
}

uint32_t
tessera_crc32_init(void)
{
	if (!crc_table_initialised) crc_table_init();
	return 0xffffffffu;
}

uint32_t
tessera_crc32_update(uint32_t s, const uint8_t *data, size_t len)
{
	if (data == NULL || len == 0) return s;
	if (!crc_table_initialised) crc_table_init();

	const uint8_t *p = data;
	while (len >= 8) {
		uint32_t a = s ^ le32_load(p);
		uint32_t b =     le32_load(p + 4);
		s = crc_table[7][ a        & 0xffu] ^
		    crc_table[6][(a >>  8) & 0xffu] ^
		    crc_table[5][(a >> 16) & 0xffu] ^
		    crc_table[4][(a >> 24)        ] ^
		    crc_table[3][ b        & 0xffu] ^
		    crc_table[2][(b >>  8) & 0xffu] ^
		    crc_table[1][(b >> 16) & 0xffu] ^
		    crc_table[0][(b >> 24)        ];
		p   += 8;
		len -= 8;
	}
	while (len > 0) {
		s = (s >> 8) ^ crc_table[0][(s ^ *p++) & 0xffu];
		len--;
	}
	return s;
}

uint32_t
tessera_crc32_final(uint32_t s)
{
	return ~s;
}

uint32_t
tessera_crc32(const uint8_t *data, size_t len)
{
	uint32_t s = tessera_crc32_init();
	s = tessera_crc32_update(s, data, len);
	return tessera_crc32_final(s);
}
