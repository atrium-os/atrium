/*
 * tessera/crc.h — CRC32 dispatch.
 *
 * Used for journal record CRCs, pack header / footer CRCs, superblock
 * CRCs, B+tree node CRCs. Hardware-accelerated via SSE 4.2 CRC32 on
 * x86 and ARMv8 base-ISA CRC32 on aarch64.
 */

#ifndef TESSERA_CRC_H_
#define TESSERA_CRC_H_

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* CRC-32 (IEEE-802.3 polynomial 0xedb88320, reflected). One-shot. */
uint32_t tessera_crc32(const uint8_t *data, size_t len);

/* Streaming variant for incremental CRC over discontiguous buffers. */
uint32_t tessera_crc32_init(void);
uint32_t tessera_crc32_update(uint32_t state, const uint8_t *data, size_t len);
uint32_t tessera_crc32_final(uint32_t state);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_CRC_H_ */
