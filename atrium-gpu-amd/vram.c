/*
 * vram.c — the device VRAM allocator, owned by the BASE (pci) module.
 *
 * Device-local VRAM (BAR0) is a DEVICE resource, not a GPU-block one: the gpu's
 * VRAM buffer objects carve from it today, and a display cursor/overlay plane
 * would carve from the same pool tomorrow. So the allocation cursor lives in the
 * shared softc and the carve happens here, in the device owner — an IP module
 * just calls amd_vram_alloc rather than reaching into sc->vram_next itself.
 *
 * It is a bump allocator (no per-BO reclaim): VRAM offsets are handed out
 * monotonically and reset wholesale by a device FLR (which wipes VRAM). That
 * matches the model and is enough for the current workloads; a real free-list is
 * a later refinement that, being here, only this file would change.
 */
#include "atrium_gpu_amd.h"

int
amd_vram_alloc(struct atrium_amd_softc *sc, uint64_t size, uint64_t *out_off)
{
	mtx_lock(&sc->lock);
	if (sc->vram_next + size > ATRIUM_AMD_VRAM_BYTES) {
		mtx_unlock(&sc->lock);
		return (ENOMEM);
	}
	*out_off = sc->vram_next;
	sc->vram_next += size;
	mtx_unlock(&sc->lock);
	return (0);
}
