/*
 * reset.c — the device-wide FLR (mode-1 reset) + its coordinator, owned by the
 * BASE (pci) module.
 *
 * A full reset is a DEVICE event: on real silicon it resets every IP block, GFX
 * and DCN (display) alike, and wipes device-local VRAM. So it cannot be a thing
 * the gpu module does behind the display's back (the bug this fixes: the gpu
 * module FLR'd the whole device at every attach, so a gpu hot-swap reset the
 * display engine the display module was driving). The FLR lives here, in the
 * device owner.
 *
 * Two entry points:
 *   - amd_flr: the raw REQ -> poll STATUS -> ACK handshake. The base calls it
 *     once at device attach for the cold reset (no children exist yet, so no
 *     coordination is needed — the children attach onto a clean device).
 *   - amd_device_reset: the recovery path (a userspace GPU reset, device-lost).
 *     Here the children DO exist and may be mid-operation, so it brackets the
 *     FLR with every registered IP's prepare/restore hook: quiesce all blocks,
 *     FLR, re-init all blocks. The gpu reloads its firmware/MES; the display
 *     re-arms its scanout + vblank. This is amdgpu's pre_reset/post_reset shape.
 */
#include "atrium_gpu_amd.h"

/*
 * Drive the device FLR handshake. RESET_STATUS is the one bring-up status the
 * model exposes readably, so it doubles as verification that SIM-aperture writes
 * land. The model latches synchronously (STATUS reads 1 on the first poll); the
 * poll-with-timeout keeps the shape real silicon needs.
 */
int
amd_flr(struct atrium_amd_softc *sc)
{
	int i;

	amd_mmio_write32(sc, regRESET_REQ, 1);
	for (i = 0; i < ATRIUM_AMD_RESET_POLLS; i++) {
		if (amd_mmio_read32(sc, regRESET_STATUS) != 0)
			break;
		DELAY(ATRIUM_AMD_RESET_DELAY);
	}
	if (i == ATRIUM_AMD_RESET_POLLS) {
		device_printf(sc->dev, "FLR did not latch (RESET_STATUS stuck at 0)\n");
		return (ENXIO);
	}
	amd_mmio_write32(sc, regRESET_ACK, 1);
	if (amd_mmio_read32(sc, regRESET_STATUS) != 0) {
		device_printf(sc->dev, "FLR window did not close after ACK "
		    "(RESET_STATUS still 1)\n");
		return (ENXIO);
	}
	device_printf(sc->dev, "device FLR complete (REQ -> STATUS=1 -> ACK -> "
	    "STATUS=0)\n");
	return (0);
}

/*
 * Coordinated device reset (recovery). Snapshot the per-IP hooks under sc->lock
 * (so a detaching module that clears its hooks can't race us), then run them
 * UNLOCKED — prepare may quiesce hardware and restore reloads firmware, neither
 * of which can hold the spinlock. Order: quiesce every block, FLR once, re-init
 * every block.
 */
int
amd_device_reset(struct atrium_amd_softc *sc)
{
	struct atrium_amd_reset_hooks hooks[ATRIUM_AMD_IP_COUNT];
	int i, err;

	mtx_lock(&sc->lock);
	for (i = 0; i < ATRIUM_AMD_IP_COUNT; i++)
		hooks[i] = sc->reset_hooks[i];
	mtx_unlock(&sc->lock);

	for (i = 0; i < ATRIUM_AMD_IP_COUNT; i++)
		if (hooks[i].prepare != NULL)
			hooks[i].prepare(sc);

	err = amd_flr(sc);

	for (i = 0; i < ATRIUM_AMD_IP_COUNT; i++)
		if (hooks[i].restore != NULL)
			hooks[i].restore(sc);

	return (err);
}
