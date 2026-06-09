/*
 * atrium-gpu-amd — from-scratch FreeBSD kernel driver for AMD RDNA4 GPUs.
 *
 * KERNEL side of the Atrium GPU split (kernel = C, userspace = Rust),
 * exercised against the gpusim functional model (vendor 0x1002 / device
 * 0x7550) before real silicon. This file owns newbus probe/attach/detach, the
 * /dev/atrium-gpu0 cdev, and resource teardown; the per-block work lives in
 * firmware.c / gmc.c / bo.c / cp.c / ioctl.c (design §4.2). Milestones (§8):
 *
 *   M1  PCI bring-up + BAR map + first register read (GRBM_STATUS), gated
 *       behind COMMAND.MSE/BME.
 *   M2  cold -> alive: reset, CP firmware load, MES init (firmware.c).
 *   M3  GPUVM + PM4 submit -> fence write-back (gmc.c + cp.c).
 *   M4  compute DISPATCH -> results that depend on input (cp.c, SoftwareBackend).
 *   M5  the userspace ABI: a BO + submit interface on /dev/atrium-gpu0
 *       (ioctl.c + abi header). The submit/compute proofs that M3/M4 ran as
 *       attach self-tests now run from userspace (tests/atrium_gpu_test.c) —
 *       attach just brings the device up and publishes the cdev.
 *   M6  graphics: a DRAW_INDEX_AUTO over the gfx ring rasterizes a triangle
 *       list into a render target (cp.c amd_set_draw + SET_DRAW ioctl); the
 *       test renders a solid quad and reads the pixels back.
 *   M7  interrupt-driven completion: MSI-X + an ISR draining the IH ring
 *       (irq.c), so a RELEASE_MEM end-of-pipe IRQ reaches the guest rather
 *       than relying on the model's synchronous drain. GET_IRQS exposes the
 *       serviced count; non-fatal fallback to poll mode if MSI-X is absent.
 *   M8  blocking fence-wait: IOC_WAIT_FENCE sleeps (msleep) until a fence
 *       word in a BO reaches a value — woken by the ISR — or times out, the
 *       GPU-sync primitive a real client uses instead of busy-polling.
 *   M9a fd-as-handle: a buffer object is a struct file (bo.c), not an integer
 *       in a table — lifetime is the fd refcount, it is SCM_RIGHTS-passable,
 *       and BO_CREATE returns an fd that write/read/submit/wait resolve via
 *       fget. The first step of converging the bring-up ABI toward v2.
 *   M9b kqueue-native sync: WAIT_FENCE is replaced by a timeline syncobj fd
 *       (sync.c) a submission signals on completion; userspace waits blocking
 *       (SYNCOBJ_WAIT) or via kqueue (the fd is EVFILT_READ-able). The BSD-
 *       native completion path a Fresco compositor folds into one kevent().
 *   M9c per-process address spaces: VM_CREATE makes a vm_fd with its own VMID
 *       and page tables (vm.c); BOs are created + mapped in a VM, submits run
 *       under it. The same GPU-VA in two VMs resolves to different memory —
 *       per-context isolation, the foundation for Portcullis-jailed clients.
 *   M9d bind apart from submit (v2 principle 4): BO_CREATE allocates memory
 *       only; VM_BIND maps a BO into a VM at a VA. A BO is unbound until then
 *       and can be bound once. Completes the v2 memory model's shape.
 *   M9e user-mode queues (ABI-v2 §5.9): QUEUE_MAP programs a queue (the
 *       privileged part); userspace mmap()s the BAR2 doorbell (d_mmap) and
 *       rings it directly, off the syscall path. The doorbell page is the
 *       capability — the current AMD/MES direction, expressed BSD-natively.
 *   M9f device discovery (ABI-v2 §5.1): QUERY_CAPS returns a TLV of caps
 *       (ABI version, vendor, feature bits) userspace walks, skipping any it
 *       does not recognize — the forward-compatible probe Mesa needs.
 *
 * WHY one kmod (not the §4.1 three-kmod pci/gpu/display split): there is no
 * display engine yet, so a separate PCI module would buy nothing. WHY the
 * §4.2 per-file split now: M5 adds a real userspace ABI on top of distinct
 * memory/command/bring-up blocks — each is now a coherent unit worth its own
 * file. (The split earned its keep; through M4 one file read better.)
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

#include <sys/module.h>
#include <sys/kernel.h>

/*
 * Release everything attach acquired: the cdev, the DMA pages (page tables,
 * IH, BOs), and the two BAR resources. Safe to call partway through a failed
 * attach — every field is NULL/zero until its step runs (sc->dev is set
 * first). Used by both the attach error paths and detach.
 */
static void
amd_teardown(struct atrium_amd_softc *sc)
{
	int i;

	amd_irq_teardown(sc);
	if (sc->cdev != NULL) {
		destroy_dev(sc->cdev);
		sc->cdev = NULL;
	}
	for (i = 0; i < sc->n_dma; i++)
		free(sc->dma[i].kva, M_DEVBUF);
	sc->n_dma = 0;
	if (sc->doorbell != NULL) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY,
		    sc->doorbell_rid, sc->doorbell);
		sc->doorbell = NULL;
	}
	if (sc->regs != NULL) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY, sc->regs_rid,
		    sc->regs);
		sc->regs = NULL;
	}
	if (sc->lock_inited) {
		mtx_destroy(&sc->lock);
		sc->lock_inited = 0;
	}
}

static int
atrium_amd_probe(device_t dev)
{
	if (pci_get_vendor(dev) == ATRIUM_AMD_VENDOR &&
	    pci_get_device(dev) == ATRIUM_AMD_DEVICE) {
		device_set_desc(dev, "Atrium AMD RDNA4 GPU (gpusim)");
		return (BUS_PROBE_DEFAULT);
	}
	return (ENXIO);
}

static int
atrium_amd_attach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(dev);
	uint32_t id, grbm;
	int err;

	sc->dev = dev;
	mtx_init(&sc->lock, "atrium-gpu", NULL, MTX_DEF);
	sc->lock_inited = 1;

	/*
	 * PCI bring-up gate (device-reference §2, §4; referee INV-PCI-0001/
	 * 0002): the device faults DMA before Bus-Master-Enable and BAR access
	 * before Memory-Space-Enable. pci_enable_busmaster() sets BME; allocating
	 * a BAR with RF_ACTIVE enables memory-space decoding (MSE). This ordering
	 * is load-bearing — the model unlocks the BARs only once COMMAND lands.
	 */
	pci_enable_busmaster(dev);

	sc->regs_rid = ATRIUM_AMD_REGS_BAR;
	sc->regs = bus_alloc_resource_any(dev, SYS_RES_MEMORY, &sc->regs_rid,
	    RF_ACTIVE);
	if (sc->regs == NULL) {
		device_printf(dev, "failed to map BAR5 (MMIO register file)\n");
		return (ENXIO);
	}
	sc->doorbell_rid = ATRIUM_AMD_DOORBELL_BAR;
	sc->doorbell = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
	    &sc->doorbell_rid, RF_ACTIVE);
	if (sc->doorbell == NULL) {
		device_printf(dev, "failed to map BAR2 (doorbell)\n");
		amd_teardown(sc);
		return (ENXIO);
	}

	/*
	 * Identity probe: the SIM-aperture ID reads 'GPUS'. Before MSE took
	 * effect this would read 0xffffffff (the referee leaving the access
	 * unclaimed, INV-PCI-0002), so it doubles as a bring-up check. GRBM_STATUS
	 * reads 0 — the model is functional, not register-complete.
	 */
	id = amd_mmio_read32(sc, regSIM_ID);
	device_printf(dev, "identity probe = 0x%08x ('%c%c%c%c')%s\n", id,
	    (int)((id >> 24) & 0xff), (int)((id >> 16) & 0xff),
	    (int)((id >> 8) & 0xff), (int)(id & 0xff),
	    id == SIM_ID_MAGIC ? " OK" : " [UNEXPECTED — not 'GPUS']");
	grbm = amd_mmio_read32(sc, regGRBM_STATUS);
	device_printf(dev, "GRBM_STATUS = 0x%08x (gpusim models this as 0)\n",
	    grbm);

	/*
	 * Cold -> alive bring-up (device-reference §4): reset to a known state,
	 * stand up GMC/IH so the CP has page tables + an interrupt ring, then
	 * load CP firmware and init the MES. Order is load-bearing; the model's
	 * referee faults a doorbell before firmware or a queue map before MES.
	 */
	if ((err = amd_reset(sc)) != 0) {
		amd_teardown(sc);
		return (err);
	}
	if ((err = amd_gmc_init(sc)) != 0) {
		amd_teardown(sc);
		return (err);
	}
	amd_firmware_load(sc);
	amd_mes_init(sc);

	/*
	 * Hook MSI-X for interrupt-driven completion (the device raises an
	 * end-of-pipe IRQ on a RELEASE_MEM fence). Non-fatal: if MSI-X can't be
	 * set up, the device still works via the synchronous-drain path, so we
	 * log and run in poll mode rather than failing attach.
	 */
	if (amd_irq_setup(sc) == 0)
		device_printf(dev, "MSI-X enabled (interrupt-driven completion)\n");
	else
		device_printf(dev, "MSI-X unavailable — poll mode\n");

	/*
	 * Publish the userspace interface. From here a user-mode driver (or the
	 * in-tree test) allocates BOs, lays PM4 rings, and submits — the M3/M4
	 * proofs now run from userspace rather than as attach self-tests.
	 */
	sc->cdev = make_dev(&atrium_amd_cdevsw, device_get_unit(dev), UID_ROOT,
	    GID_WHEEL, 0600, "atrium-gpu%d", device_get_unit(dev));
	if (sc->cdev == NULL) {
		device_printf(dev, "failed to create /dev/atrium-gpu%d\n",
		    device_get_unit(dev));
		amd_teardown(sc);
		return (ENXIO);
	}
	sc->cdev->si_drv1 = sc;
	device_printf(dev, "ready: /dev/atrium-gpu%d\n", device_get_unit(dev));
	return (0);
}

static int
atrium_amd_detach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(dev);

	/*
	 * BOs are fd-backed objects that outlive any single ioctl and hold a
	 * back-reference to this softc; if we freed the device while a bo_fd
	 * were still open, its eventual fo_close would touch freed memory.
	 * Refuse to detach until userspace has closed them. (A later milestone
	 * can replace this with a device refcount the BOs hold.)
	 */
	if (sc->bo_count > 0 || sc->vm_count > 0)
		return (EBUSY);

	amd_teardown(sc);
	return (0);
}

static device_method_t atrium_amd_methods[] = {
	DEVMETHOD(device_probe,		atrium_amd_probe),
	DEVMETHOD(device_attach,	atrium_amd_attach),
	DEVMETHOD(device_detach,	atrium_amd_detach),
	DEVMETHOD_END
};

static driver_t atrium_amd_driver = {
	"atrium_gpu_amd",
	atrium_amd_methods,
	sizeof(struct atrium_amd_softc),
};

DRIVER_MODULE(atrium_gpu_amd, pci, atrium_amd_driver, NULL, NULL);
MODULE_DEPEND(atrium_gpu_amd, pci, 1, 1, 1);
MODULE_VERSION(atrium_gpu_amd, 17);
