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

/* Set a queue's GPUVM context: its submissions translate under `vmid`. */
static void
amd_queue_set_vmid(struct atrium_amd_softc *sc, uint32_t qid, uint16_t vmid)
{
	amd_mmio_write32(sc, ATRIUM_AMD_Q_BASE + qid * ATRIUM_AMD_Q_STRIDE +
	    ATRIUM_AMD_QF_VMID, vmid);
}

/*
 * Program a queue onto a ring under a VMID, without ringing the doorbell. This
 * is the privileged setup the kernel always owns; the doorbell that follows can
 * be rung by the kernel (amd_submit) or, for a user-mode queue, by userspace
 * after mmap'ing the doorbell page. Returns the queue's doorbell byte offset.
 */
int
amd_queue_program(struct atrium_amd_softc *sc, uint64_t ring_va,
    uint32_t engine, uint16_t vmid, uint32_t *doorbell_off)
{
	switch (engine) {
	case ATRIUM_GPU_ENGINE_GFX:
		/* Graphics ring = queue 0, direct CP_RB0_* MMIO (base holds VA>>8). */
		amd_queue_set_vmid(sc, ATRIUM_AMD_GFX_QID, vmid);
		amd_mmio_write32(sc, regCP_RB0_BASE, (uint32_t)(ring_va >> 8));
		amd_mmio_write32(sc, regCP_RB0_CNTL, ATRIUM_AMD_RING_BYTES);
		amd_mmio_write32(sc, regCP_RB_DOORBELL_CONTROL,
		    ATRIUM_AMD_GFX_DOORBELL);
		amd_mmio_write32(sc, regCP_ME_CNTL, 0);	/* clear halt -> run */
		*doorbell_off = ATRIUM_AMD_GFX_DOORBELL;
		return (0);

	case ATRIUM_GPU_ENGINE_COMPUTE:
		/* Compute queue 1 via the MEC HQD path. */
		amd_mmio_write32(sc, regHQD_SELECT, ATRIUM_AMD_COMPUTE_QID);
		amd_queue_set_vmid(sc, ATRIUM_AMD_COMPUTE_QID, vmid);
		amd_mmio_write32(sc, regCP_HQD_PQ_BASE, (uint32_t)(ring_va >> 8));
		amd_mmio_write32(sc, regCP_HQD_PQ_CONTROL, ATRIUM_AMD_RING_BYTES);
		amd_mmio_write32(sc, regCP_HQD_PQ_DOORBELL_CONTROL,
		    ATRIUM_AMD_COMPUTE_DOORBELL);
		amd_mmio_write32(sc, regCP_HQD_ACTIVE, 1);
		*doorbell_off = ATRIUM_AMD_COMPUTE_DOORBELL;
		return (0);

	default:
		return (EINVAL);
	}
}

int
amd_submit(struct atrium_amd_softc *sc, struct atrium_amd_bo *ring,
    uint32_t n_dwords, uint32_t engine, uint16_t vmid)
{
	uint32_t doorbell_off;
	int err;

	err = amd_queue_program(sc, ring->gpu_va, engine, vmid, &doorbell_off);
	if (err != 0)
		return (err);
	bus_write_4(sc->doorbell, doorbell_off, n_dwords);
	return (0);
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

/*
 * Program the graphics DRAW state the rasterizer reads when it executes a
 * DRAW_INDEX_AUTO packet: the vertex buffer + render target GPU-VAs and the RT
 * dimensions. Depth/texture/blend are disabled here (this is the solid-color
 * path — the pixel is the interpolated vertex color); they become ioctl
 * parameters when textured/blended draws land.
 */
void
amd_set_draw(struct atrium_amd_softc *sc, uint64_t vtx_va, uint64_t rt_va,
    uint32_t width, uint32_t height, uint64_t tex_va, uint32_t tex_w,
    uint32_t tex_h, uint32_t tex_filter, uint32_t blend, uint64_t depth_va)
{
	amd_mmio_write32(sc, regDRAW_VTX_LO, (uint32_t)(vtx_va & 0xffffffff));
	amd_mmio_write32(sc, regDRAW_VTX_HI, (uint32_t)(vtx_va >> 32));
	amd_mmio_write32(sc, regDRAW_RT_LO, (uint32_t)(rt_va & 0xffffffff));
	amd_mmio_write32(sc, regDRAW_RT_HI, (uint32_t)(rt_va >> 32));
	amd_mmio_write32(sc, regDRAW_RT_DIM, (width << 16) | height);
	/* depth buffer (0 = no depth test) */
	amd_mmio_write32(sc, regDEPTH_LO, (uint32_t)(depth_va & 0xffffffff));
	amd_mmio_write32(sc, regDEPTH_HI, (uint32_t)(depth_va >> 32));
	/* texture (0 = interpolate vertex color) + dims + filter */
	amd_mmio_write32(sc, regTEX_LO, (uint32_t)(tex_va & 0xffffffff));
	amd_mmio_write32(sc, regTEX_HI, (uint32_t)(tex_va >> 32));
	amd_mmio_write32(sc, regTEX_DIM, (tex_w << 16) | tex_h);
	amd_mmio_write32(sc, regTEX_FILTER, tex_filter);
	amd_mmio_write32(sc, regBLEND_ENABLE, blend);
}
