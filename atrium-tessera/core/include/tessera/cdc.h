/*
 * tessera/cdc.h — content-defined chunking (FastCDC).
 *
 * Splits a stream of bytes into chunks at content-driven boundaries
 * via a rolling gear hash. Per tessera-fs §6.5: 64 KiB average,
 * 16 KiB min, 256 KiB max by default (tunable per-call).
 */

#ifndef TESSERA_CDC_H_
#define TESSERA_CDC_H_

#ifdef _KERNEL
#  include <sys/types.h>
#else
#  include <stdint.h>
#  include <stddef.h>
#endif

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
	uint32_t  avg_chunk;     /* target average chunk size (bytes) */
	uint32_t  min_chunk;     /* minimum chunk size; chunks below this are merged */
	uint32_t  max_chunk;     /* maximum chunk size; force a boundary at this */
} tessera_cdc_params_t;

/* Default parameters (64 KiB / 16 KiB / 256 KiB). */
extern const tessera_cdc_params_t tessera_cdc_default_params;

/*
 * Walk `data` of length `len`. For each detected boundary, append
 * the byte offset to `out_boundaries[*n_out]` and increment *n_out.
 * Boundaries are inclusive end-offsets; the last boundary always
 * equals `len`.
 *
 * Returns 0 on success, TESSERA_EINVAL if buffer too small.
 *
 * The caller owns `out_boundaries`; pre-allocate `len/min_chunk + 2`
 * entries to be safe.
 */
int tessera_cdc_split(const uint8_t *data, size_t len,
                      const tessera_cdc_params_t *params,
                      size_t *out_boundaries, size_t cap, size_t *n_out);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_CDC_H_ */
