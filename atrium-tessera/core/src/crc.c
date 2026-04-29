/*
 * tessera-core: CRC32 (IEEE 802.3 polynomial 0xedb88320, reflected).
 * Phase 1 will switch to hardware-accelerated dispatch.
 */

#include "tessera/crc.h"

uint32_t
tessera_crc32(const uint8_t *data, size_t len)
{
	(void)data; (void)len;
	return 0;  /* TODO: phase 1 — table-based + HW dispatch */
}

uint32_t
tessera_crc32_init(void)        { return 0xffffffffu; }

uint32_t
tessera_crc32_update(uint32_t s, const uint8_t *data, size_t len)
{
	(void)data; (void)len;
	return s;  /* TODO: phase 1 */
}

uint32_t
tessera_crc32_final(uint32_t s) { return ~s; }
