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

/*
 * Stop a queue: halt the gfx ME, or deactivate the compute HQD, and clear its
 * VMID. Work still queued on it is dropped. The caller holds the queue's lock
 * (queue.c). On gpusim the stop takes effect at the register write; real
 * silicon needs the HQD dequeue request and a wait for CP_HQD_ACTIVE to clear
 * (MES REMOVE_QUEUE once queues move to the MES, review §A2.3).
 */
void
amd_queue_stop(struct atrium_amd_softc *sc, uint32_t qid)
{
	if (qid == ATRIUM_AMD_GFX_QID) {
		amd_mmio_write32(sc, regCP_ME_CNTL, 1);	/* halt */
	} else {
		amd_mmio_write32(sc, regHQD_SELECT, qid);
		amd_mmio_write32(sc, regCP_HQD_ACTIVE, 0);
	}
	amd_queue_set_vmid(sc, qid, 0);
}

/*
 * NB: there is no amd_set_compute/amd_set_draw. Compute and draw state is no
 * longer poked into the COMPUTE/DRAW state registers by the kernel — it travels
 * in the submitted ring as a SET_SH_REG packet the CP applies (the opaque-blob
 * submit of ABI-v2). The kernel is register-agnostic for the command stream;
 * userspace<->firmware owns the register layout.
 */

/*
 * Firmware energy-fair scheduler: the kernel sets per-queue weights + kernels;
 * the device (firmware) runs the energy-fair selection and exposes the per-queue
 * Joule counters. The kernel does NOT run the scheduling loop — it programs
 * weights and reads counters (gpu-scheduler §9: kernel sets weights, firmware
 * enforces). A register driver over the model's SCHED block.
 */
void
amd_sched(struct atrium_amd_softc *sc, struct atrium_gpu_sched *s)
{
	switch (s->op) {
	case 0:	/* add a queue with the staged weight + kernel */
		amd_mmio_write32(sc, regSCHED_WEIGHT, s->arg);
		amd_mmio_write32(sc, regSCHED_KERNEL_OPS, s->ops);
		amd_mmio_write32(sc, regSCHED_KERNEL_BYTES, s->bytes);
		amd_mmio_write32(sc, regSCHED_KERNEL_LEVEL, s->level);
		amd_mmio_write32(sc, regSCHED_ADD_QUEUE, 1);
		break;
	case 1:	/* run `arg` progress-fair scheduling rounds */
		amd_mmio_write32(sc, regSCHED_RUN_ROUNDS, s->arg);
		break;
	case 2:	/* query queue `arg`: engine time (fairness) + Joules (telemetry) */
		amd_mmio_write32(sc, regSCHED_SELECT, s->arg);
		s->busy_us = amd_mmio_read32(sc, regSCHED_BUSY_US);
		s->energy_uj = amd_mmio_read32(sc, regSCHED_ENERGY_UJ);
		s->runs = amd_mmio_read32(sc, regSCHED_RUNS);
		break;
	case 3:	/* set the deadline window (ns); 0 = deadline-blind/fair rounds */
		amd_mmio_write32(sc, regSCHED_DEADLINE_WINDOW, s->arg);
		break;
	case 4:	/* stamp queue `arg` with a deadline `deadline_ns` from now (0=clear) */
		amd_mmio_write32(sc, regSCHED_SELECT, s->arg);
		amd_mmio_write32(sc, regSCHED_DEADLINE, s->deadline_ns);
		break;
	}
	s->count = amd_mmio_read32(sc, regSCHED_QUEUE_COUNT);
}
