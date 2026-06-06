/*
 * cp.c — Command Processor: queue map + doorbell submit, compute state.
 *
 * Takes a PM4 ring already laid into a BO (built by userspace) and runs it on
 * an engine: program the queue onto the ring, then ring its doorbell. The gfx
 * ring (queue 0) goes through the direct CP_RB0_* registers; a compute queue
 * (queue 1) goes through the MEC HQD path (CP_HQD_*), whose activation is the
 * MQD->HQD map the MES performs. The model drains the ring synchronously
 * within the doorbell write.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

int
amd_submit(struct atrium_amd_softc *sc, struct atrium_amd_bo *ring,
    uint32_t n_dwords, uint32_t engine)
{
	uint64_t ring_va = ring->gpu_va;

	switch (engine) {
	case ATRIUM_GPU_ENGINE_GFX:
		/*
		 * Graphics ring = queue 0, direct CP_RB0_* MMIO (ring base
		 * holds VA>>8). Clear the ME halt so the CP runs the ring.
		 */
		amd_mmio_write32(sc, regCP_RB0_BASE, (uint32_t)(ring_va >> 8));
		amd_mmio_write32(sc, regCP_RB0_CNTL, ATRIUM_AMD_RING_BYTES);
		amd_mmio_write32(sc, regCP_RB_DOORBELL_CONTROL,
		    ATRIUM_AMD_GFX_DOORBELL);
		amd_mmio_write32(sc, regCP_ME_CNTL, 0);
		bus_write_4(sc->doorbell, ATRIUM_AMD_GFX_DOORBELL, n_dwords);
		return (0);

	case ATRIUM_GPU_ENGINE_COMPUTE:
		/*
		 * Compute queue 1 via the MEC HQD path: select the HQD, program
		 * its ring/size/doorbell, activate (gated on MES init), ring it.
		 */
		amd_mmio_write32(sc, regHQD_SELECT, ATRIUM_AMD_COMPUTE_QID);
		amd_mmio_write32(sc, regCP_HQD_PQ_BASE,
		    (uint32_t)(ring_va >> 8));
		amd_mmio_write32(sc, regCP_HQD_PQ_CONTROL, ATRIUM_AMD_RING_BYTES);
		amd_mmio_write32(sc, regCP_HQD_PQ_DOORBELL_CONTROL,
		    ATRIUM_AMD_COMPUTE_DOORBELL);
		amd_mmio_write32(sc, regCP_HQD_ACTIVE, 1);
		bus_write_4(sc->doorbell, ATRIUM_AMD_COMPUTE_DOORBELL, n_dwords);
		return (0);

	default:
		return (EINVAL);
	}
}

/*
 * Program the compute state the SoftwareBackend reads at DISPATCH time: the
 * built-in kernel selector and the source/dest GPU-VAs (which it walks through
 * GPUVM). These are device-private registers, so only the kernel writes them —
 * userspace names the buffers by GPU-VA via the ioctl.
 */
void
amd_set_compute(struct atrium_amd_softc *sc, uint32_t kernel, uint64_t src_va,
    uint64_t dst_va)
{
	amd_mmio_write32(sc, regCOMPUTE_KERNEL, kernel);
	amd_mmio_write32(sc, regCOMPUTE_SRC_LO, (uint32_t)(src_va & 0xffffffff));
	amd_mmio_write32(sc, regCOMPUTE_SRC_HI, (uint32_t)(src_va >> 32));
	amd_mmio_write32(sc, regCOMPUTE_DST_LO, (uint32_t)(dst_va & 0xffffffff));
	amd_mmio_write32(sc, regCOMPUTE_DST_HI, (uint32_t)(dst_va >> 32));
}
