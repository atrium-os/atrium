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
#include <sys/energy_budget.h>

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
		amd_dma_page_free(&sc->dma[i]);
	sc->n_dma = 0;
	/*
	 * Reset the rest of the gpu module's footprint in the SHARED softc back to
	 * the pristine state the base left it in, so a re-attach (driver hot-swap,
	 * docs/spec/gpu-driver-hotswap.md Protocol A) starts clean: the IH ring is
	 * freed above, so drop its pointer + read cursor, and forget any
	 * completions the ISR still owed (their syncobjs are gone with the fds).
	 */
	sc->ih_kva = NULL;
	sc->ih_rptr = 0;
	sc->n_pending = 0;
	/*
	 * The BAR resources and the softc mutex belong to the base (pci) module
	 * (it mapped them into the shared softc); it releases them on its own
	 * detach. This GPU module only tears down what it set up.
	 */
}

static int
atrium_amd_probe(device_t dev)
{
	const char *name = device_get_name(dev);

	/* The base module names our child "atrium_gpu_amd" (our driver name), so
	 * only we probe it; confirm + claim it. */
	if (name != NULL && strcmp(name, "atrium_gpu_amd") == 0) {
		device_set_desc(dev, "Atrium AMD RDNA4 GPU (gpusim)");
		return (BUS_PROBE_DEFAULT);
	}
	return (ENXIO);
}

static uint64_t
amd_energy_demand_mw(void *arg)
{
	struct atrium_amd_softc *sc = arg;

	return (amd_mmio_read32(sc, regSCHED_POWER_DEMAND_MW));
}

static void
amd_energy_budget_mw(void *arg, uint64_t mw)
{
	struct atrium_amd_softc *sc = arg;

	amd_mmio_write32(sc, regSCHED_POWER_BUDGET_MW,
	    mw > UINT32_MAX ? UINT32_MAX : (uint32_t)mw);
}

static int
atrium_amd_attach(device_t dev)
{
	/*
	 * The base (pci) module owns the device: it mapped the BARs into the
	 * SHARED softc and set sc->dev to the PCI device. We attach to its
	 * "atrium_gpu" child and do the GPU compute/render bring-up against that
	 * shared softc (every helper takes `sc` and uses sc->dev, so they work
	 * unchanged). BAR mapping + the softc mutex are the base module's.
	 */
	struct atrium_amd_softc *sc = device_get_softc(device_get_parent(dev));
	uint32_t id, grbm;
	int err;

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
	/*
	 * MAKEDEV_CHECKNAME so a name collision returns EEXIST instead of
	 * panicking: a clean detach destroys the cdev, but on the hot-swap path we
	 * never want a stale node to take down the kernel — fail the attach loudly
	 * instead.
	 */
	err = make_dev_p(MAKEDEV_CHECKNAME, &sc->cdev, &atrium_amd_cdevsw, NULL,
	    UID_ROOT, GID_WHEEL, 0600, "atrium-gpu%d", device_get_unit(dev));
	if (err != 0) {
		device_printf(dev, "failed to create /dev/atrium-gpu%d: %d\n",
		    device_get_unit(dev), err);
		amd_teardown(sc);
		return (err);
	}
	sc->cdev->si_drv1 = sc;

	/*
	 * Energy-budget federation member (P6): the kernel water_fills
	 * the shared power cap; this device OBEYS its budget (the model
	 * throttles execution to it) and exposes demand telemetry.
	 */
	sc->energy_member = energy_member_register("gpu0",
	    amd_energy_demand_mw, amd_energy_budget_mw, sc, 1);

	device_printf(dev, "ready: /dev/atrium-gpu%d\n", device_get_unit(dev));
	return (0);
}

static int
atrium_amd_detach(device_t dev)
{
	/*
	 * Operate on the SHARED softc owned by the base (pci) module — the same
	 * one attach used (device_get_softc(parent)). This child's own softc is
	 * unused (the driver_t declares zero softc); reading it here instead would
	 * tear down a zeroed struct (leaking the real cdev/IH ring and wrongly
	 * unregistering energy slot 0). detach mirrors attach exactly.
	 */
	struct atrium_amd_softc *sc = device_get_softc(device_get_parent(dev));

	/*
	 * BOs are fd-backed objects that outlive any single ioctl and hold a
	 * back-reference to this softc; if we freed the device while a bo_fd
	 * were still open, its eventual fo_close would touch freed memory.
	 * Refuse to detach until userspace has closed them. (A later milestone
	 * can replace this with a device refcount the BOs hold.)
	 */
	if (sc->bo_count > 0 || sc->vm_count > 0)
		return (EBUSY);

	if (sc->energy_member >= 0) {
		energy_member_unregister(sc->energy_member);
		sc->energy_member = -1;
	}
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
	0,	/* no per-child softc: all state lives in the base's SHARED softc,
		 * reached via device_get_softc(device_get_parent(dev)). Declaring a
		 * child softc here is the trap that made detach use the wrong struct. */
};

/* Attach to the "atrium_gpu" child of the base (pci) module, not `pci`. */
DRIVER_MODULE(atrium_gpu_amd, atrium_gpu_amd_pci, atrium_amd_driver, NULL, NULL);
MODULE_DEPEND(atrium_gpu_amd, atrium_gpu_amd_pci, 1, 1, 1);
MODULE_VERSION(atrium_gpu_amd, 1);
