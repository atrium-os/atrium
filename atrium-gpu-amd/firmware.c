/*
 * firmware.c — GFX-block bring-up: the gpu-scoped soft reset, CP microcode load,
 * MES init.
 *
 * The handshake that turns the GFX/compute block "alive" (device-reference §4).
 * Order is load-bearing and the model's referee enforces it: the MES rides on
 * the CP microcode, so it comes up only once firmware is loaded. The caller
 * (attach) sequences soft-reset -> GMC -> firmware -> MES.
 *
 * The device-wide FLR is NOT here — it is a device event the base owns (reset.c,
 * amd_flr). This module only ever resets its OWN block, via GRBM_SOFT_RESET.
 */
#include "atrium_gpu_amd.h"

/*
 * GPU-scoped soft reset: recover the GFX/compute engine (clears a hung pipe so
 * submissions are accepted again) WITHOUT touching device-local VRAM or the
 * display block — the gpu module has no business resetting those. Used at attach
 * (a cheap no-op on an already-clean device) and as the gpu's half of a
 * coordinated device reset. GRBM_SOFT_RESET is write-only; its effect shows when
 * a previously wedged engine honors the next submit.
 */
void
amd_grbm_soft_reset(struct atrium_amd_softc *sc)
{
	amd_mmio_write32(sc, regGPU_RESET, 1);
}

/*
 * Load CP firmware (models the PSP loading the CP microcode). Stage the ucode
 * version, then activate. The model refuses a version below CP_FW_MIN_VERSION;
 * we stage exactly the minimum since there is no real blob behind the model.
 * cp_fw_loaded is not read-back-able, so this is verified by construction
 * (correct version, post-reset order) — its effect shows up when a doorbell is
 * honored instead of faulting.
 */
void
amd_firmware_load(struct atrium_amd_softc *sc)
{
	amd_mmio_write32(sc, regFW_CP_VERSION, ATRIUM_AMD_CP_FW_VERSION);
	amd_mmio_write32(sc, regFW_CP_LOAD, 1);
	device_printf(sc->dev, "CP firmware loaded (version 0x%x)\n",
	    ATRIUM_AMD_CP_FW_VERSION);
}

/*
 * Initialize the MES scheduler. Gated on the CP firmware above; the MES is
 * what reads a queue descriptor and activates a queue, so it must be up before
 * any submit.
 */
void
amd_mes_init(struct atrium_amd_softc *sc)
{
	amd_mmio_write32(sc, regMES_INIT, 1);
	device_printf(sc->dev, "MES initialized — GPU alive\n");
}
