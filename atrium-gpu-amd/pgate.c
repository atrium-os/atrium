/*
 * pgate.c — power-gate idle IP blocks (the driver-controlled lever).
 *
 * The power state firmware owns is the DVFS *curve*; the driver owns the gating
 * *policy*: which blocks to power-gate when idle. This handler reads which IP
 * blocks the current workload uses (the engine reports it in BLOCK_BUSY), gates
 * every other block, and reads back the draw before/after over the power-gating
 * register aperture (APER_PGATE; model: engine/src/pgate_regs.rs).
 *
 * Foreknowledge (Fresco knows the frame, Lyra the buffer, the GPU scheduler the
 * dispatch): when the caller names the blocks the NEXT workload needs, the driver
 * pre-wakes exactly those — keeping them powered so the upcoming work pays no wake
 * latency — while still gating everything genuinely idle. Clock gating is not
 * here: it is hardware-automatic, sub-cycle, not software-programmed.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

int
amd_powergate(struct atrium_amd_softc *sc, struct atrium_gpu_powergate *p)
{
	uint32_t nblocks, allmask, idle;

	nblocks = amd_mmio_read32(sc, regPGATE_NUM_BLOCKS);
	allmask = (nblocks >= 32) ? 0xffffffffu : ((1u << nblocks) - 1u);

	/* Tell the device which blocks the current workload is using. */
	amd_mmio_write32(sc, regPGATE_SET_BUSY, p->busy_mask & allmask);
	p->power_before_mw = amd_mmio_read32(sc, regPGATE_POWER_MW);

	/*
	 * Policy: gate every idle block. With foreknowledge of the next workload,
	 * keep the blocks it will need powered (pre-wake) so it pays no wake stall.
	 */
	idle = ~p->busy_mask & allmask;
	idle &= ~p->next_busy;
	amd_mmio_write32(sc, regPGATE_BLOCK_GATE, idle);

	p->gate_mask = amd_mmio_read32(sc, regPGATE_BLOCK_GATE);
	p->power_after_mw = amd_mmio_read32(sc, regPGATE_POWER_MW);

	/* What the next workload would stall on — 0 once its blocks are pre-woken. */
	amd_mmio_write32(sc, regPGATE_NEXT_BUSY, p->next_busy & allmask);
	p->wake_stall_us = amd_mmio_read32(sc, regPGATE_WAKE_STALL_US);

	return (0);
}
