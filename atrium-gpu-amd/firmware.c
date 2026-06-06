/*
 * firmware.c — PSP/MES bring-up: reset, CP microcode load, MES init.
 *
 * The handshake that turns a cold device "alive" (device-reference §4). Order
 * is load-bearing and the model's referee enforces it: a full reset tears down
 * the CP, so firmware reloads after it; the MES rides on the CP microcode, so
 * it comes up only once firmware is loaded. The caller (attach) sequences
 * reset -> GMC/IH -> firmware -> MES.
 */
#include "atrium_gpu_amd.h"

/*
 * Reset to a known state. A from-scratch driver inherits the device in
 * whatever state firmware/a prior driver left it; reset to a defined baseline
 * before programming. RESET_STATUS is the one bring-up status the model
 * exposes readably, so it doubles as our verification that SIM-aperture writes
 * land and read back. The model latches synchronously (STATUS reads 1 on the
 * first poll); the poll-with-timeout keeps the shape real silicon needs.
 */
int
amd_reset(struct atrium_amd_softc *sc)
{
	int i;

	amd_mmio_write32(sc, regRESET_REQ, 1);
	for (i = 0; i < ATRIUM_AMD_RESET_POLLS; i++) {
		if (amd_mmio_read32(sc, regRESET_STATUS) != 0)
			break;
		DELAY(ATRIUM_AMD_RESET_DELAY);
	}
	if (i == ATRIUM_AMD_RESET_POLLS) {
		device_printf(sc->dev, "reset did not latch (RESET_STATUS "
		    "stuck at 0)\n");
		return (ENXIO);
	}
	amd_mmio_write32(sc, regRESET_ACK, 1);
	if (amd_mmio_read32(sc, regRESET_STATUS) != 0) {
		device_printf(sc->dev, "reset window did not close after ACK "
		    "(RESET_STATUS still 1)\n");
		return (ENXIO);
	}
	device_printf(sc->dev, "GPU reset complete (REQ -> STATUS=1 -> ACK -> "
	    "STATUS=0)\n");
	return (0);
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
