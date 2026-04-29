/* tessera-core: FastCDC chunking. Phase 1 implements; phase 0 stub. */

#include "tessera/cdc.h"
#include "tessera/error.h"

const tessera_cdc_params_t tessera_cdc_default_params = {
	.avg_chunk = 64u * 1024u,
	.min_chunk = 16u * 1024u,
	.max_chunk = 256u * 1024u,
};

int
tessera_cdc_split(const uint8_t *data, size_t len,
                  const tessera_cdc_params_t *params,
                  size_t *out_boundaries, size_t cap, size_t *n_out)
{
	(void)data; (void)len; (void)params;
	(void)out_boundaries; (void)cap; (void)n_out;
	return TESSERA_ENOTIMPL;
}
