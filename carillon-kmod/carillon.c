/*
 * carillon.c — FreeBSD guest endpoint for the Carillon GPU VM transport.
 *
 * A newbus PCI driver for QEMU's ivshmem-doorbell device (vendor 0x1af4,
 * device 0x1110). It maps BAR0 (device registers) + BAR2 (the shared
 * region), allocates an MSI-X vector for the host->guest doorbell, and
 * exposes /dev/carillon0:
 *
 *   - mmap(2)  maps BAR2 so userspace drives the rings + frame arena
 *              directly (the GuestRing protocol in src/carillon.rs).
 *   - ioctl CARILLON_RING  rings the host doorbell (BAR0 doorbell write).
 *   - ioctl CARILLON_WAIT  parks the caller on the completion waitqueue,
 *              woken by the MSI-X ISR — a true sleep, the no-spin
 *              invariant (no ring-spin, no vsync timer).
 *
 * Native FreeBSD primitives only (newbus, MSI-X, cdev, msleep/wakeup) —
 * no linuxkpi, no drm-kmod. See docs/spec/carillon.md §9.
 *
 * Idioms mirror atrium-kmod/atrium_virtio_gpu.c (proven on this platform);
 * device register + doorbell semantics mirror the verified karythra-os
 * ivshmem_gpu.rs; the ring protocol mirrors the verified
 * aqueduct-gpu-host src/carillon.rs::GuestRing.
 *
 * STATUS: first compiled + booted in the `run-vm.sh --carillon` VM
 * session (T1); not host-buildable (FreeBSD kernel module).
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/module.h>
#include <sys/bus.h>
#include <sys/conf.h>
#include <sys/malloc.h>
#include <sys/lock.h>
#include <sys/mutex.h>
#include <sys/rman.h>
#include <sys/sysctl.h>

#include <machine/bus.h>
#include <machine/resource.h>

#include <vm/vm.h>
#include <vm/pmap.h>

#include <dev/pci/pcireg.h>
#include <dev/pci/pcivar.h>

#include "carillon_abi.h"

MALLOC_DEFINE(M_CARILLON, "carillon", "Carillon GPU transport");

struct carillon_softc {
	device_t	dev;

	/* BAR0: device registers (doorbell / interrupt). */
	int		reg_rid;
	struct resource *reg_res;

	/* BAR2: shared-memory region (mapped to userspace via mmap). */
	int		shm_rid;
	struct resource *shm_res;
	vm_paddr_t	shm_paddr;
	bus_size_t	shm_size;

	/* BAR1: MSI-X table + PBA. pci_alloc_msix requires the table BAR to
	 * be allocated + RF_ACTIVE in our resource list before it will hand
	 * out vectors, so the driver must map it (the bus does not). */
	int		msix_bar_rid;
	struct resource *msix_bar_res;

	/* MSI-X doorbell interrupt. */
	int		irq_rid;
	struct resource *irq_res;
	void		*intr_cookie;
	int		msix_alloced;

	/* Completion wakeup: ISR bumps `db_seq` + wakes waiters. */
	struct mtx	lock;
	uint32_t	db_seq;	   /* doorbell arrivals (monotonic) */
	uint64_t	irq_count; /* diagnostics */

	struct cdev	*cdev;
};

/* ----------------------------------------------------------------------- */
/* Register helpers                                                         */
/* ----------------------------------------------------------------------- */

static inline uint32_t
carillon_reg_read(struct carillon_softc *sc, bus_size_t off)
{
	return (bus_read_4(sc->reg_res, off));
}

static inline void
carillon_reg_write(struct carillon_softc *sc, bus_size_t off, uint32_t val)
{
	bus_write_4(sc->reg_res, off, val);
}

/* ----------------------------------------------------------------------- */
/* Interrupt handler (host -> guest doorbell)                               */
/* ----------------------------------------------------------------------- */

static void
carillon_intr(void *arg)
{
	struct carillon_softc *sc = arg;

	/* Reading IntStatus clears the interrupt. */
	(void)carillon_reg_read(sc, CARILLON_REG_INTSTATUS);

	mtx_lock(&sc->lock);
	sc->db_seq++;
	sc->irq_count++;
	wakeup(&sc->db_seq);
	mtx_unlock(&sc->lock);
}

/* ----------------------------------------------------------------------- */
/* cdev: /dev/carillon0                                                     */
/* ----------------------------------------------------------------------- */

static d_open_t  carillon_open;
static d_ioctl_t carillon_ioctl;
static d_mmap_t  carillon_mmap;

static struct cdevsw carillon_cdevsw = {
	.d_version = D_VERSION,
	.d_open    = carillon_open,
	.d_ioctl   = carillon_ioctl,
	.d_mmap    = carillon_mmap,
	.d_name    = "carillon",
};

static int
carillon_open(struct cdev *cdev, int oflags __unused, int devtype __unused,
    struct thread *td __unused)
{
	return (0);
}

/*
 * mmap the BAR2 shared region. `offset` is a byte offset within BAR2;
 * return its physical address so the userspace mapping aliases the same
 * pages QEMU exposed (and the host mmap'd on the other side).
 */
static int
carillon_mmap(struct cdev *cdev, vm_ooffset_t offset, vm_paddr_t *paddr,
    int nprot __unused, vm_memattr_t *memattr)
{
	struct carillon_softc *sc = cdev->si_drv1;

	if (sc->shm_res == NULL)
		return (ENXIO);
	if (offset < 0 || (bus_size_t)offset >= sc->shm_size)
		return (EINVAL);

	*paddr = sc->shm_paddr + offset;
	/*
	 * Map the shared region WRITE_BACK (cacheable). Leaving memattr
	 * untouched makes ARM64 default to DEVICE (uncacheable), which is
	 * wrong for the SPSC rings + frame arena. Matches the proven
	 * atrium_virtio_gpu.c BAR mapping; the guest issues explicit cache
	 * maintenance where cross-HVF coherency needs it.
	 */
	if (memattr != NULL)
		*memattr = VM_MEMATTR_WRITE_BACK;
	return (0);
}

static int
carillon_ioctl(struct cdev *cdev, u_long cmd, caddr_t data,
    int fflag __unused, struct thread *td __unused)
{
	struct carillon_softc *sc = cdev->si_drv1;

	switch (cmd) {
	case CARILLON_RING:
		/*
		 * Ring the host: write (peer_id<<16)|vector to the doorbell
		 * register. QEMU turns this into the host peer's eventfd
		 * becoming readable.
		 */
		carillon_reg_write(sc, CARILLON_REG_DOORBELL,
		    (CARILLON_HOST_PEER_ID << 16) | CARILLON_DOORBELL_VECTOR);
		return (0);

	case CARILLON_WAIT: {
		struct carillon_wait *w = (struct carillon_wait *)data;
		int timo, err;

		timo = (w->timeout_ms == 0) ? 0 :
		    (int)((uint64_t)w->timeout_ms * hz / 1000);
		if (w->timeout_ms != 0 && timo == 0)
			timo = 1;

		mtx_lock(&sc->lock);
		err = 0;
		/*
		 * Edge-safe: sleep only while the doorbell counter has not
		 * advanced past the caller's last-seen value. A doorbell that
		 * fired between the caller's ring and this wait already bumped
		 * db_seq, so we return immediately without losing it.
		 */
		if (sc->db_seq == w->seq)
			err = msleep(&sc->db_seq, &sc->lock, PCATCH,
			    "carwait", timo);
		w->seq = sc->db_seq;
		mtx_unlock(&sc->lock);

		/* EWOULDBLOCK (timeout) is not an error to the caller. */
		if (err == EWOULDBLOCK)
			err = 0;
		return (err);
	}

	default:
		return (ENOTTY);
	}
}

/* ----------------------------------------------------------------------- */
/* newbus probe / attach / detach                                           */
/* ----------------------------------------------------------------------- */

static int
carillon_probe(device_t dev)
{
	if (pci_get_vendor(dev) == CARILLON_PCI_VENDOR &&
	    pci_get_device(dev) == CARILLON_PCI_DEVICE) {
		device_set_desc(dev, "Carillon GPU transport (ivshmem-doorbell)");
		return (BUS_PROBE_DEFAULT);
	}
	return (ENXIO);
}

static int
carillon_attach(device_t dev)
{
	struct carillon_softc *sc = device_get_softc(dev);
	int msix_count, err;

	sc->dev = dev;
	mtx_init(&sc->lock, "carillon", NULL, MTX_DEF);
	pci_enable_busmaster(dev);

	/* BAR0: device registers. */
	sc->reg_rid = PCIR_BAR(0);
	sc->reg_res = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
	    &sc->reg_rid, RF_ACTIVE);
	if (sc->reg_res == NULL) {
		device_printf(dev, "cannot map BAR0 (registers)\n");
		err = ENXIO;
		goto fail;
	}

	/* BAR2: shared-memory region. */
	sc->shm_rid = PCIR_BAR(2);
	sc->shm_res = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
	    &sc->shm_rid, RF_ACTIVE);
	if (sc->shm_res == NULL) {
		device_printf(dev, "cannot map BAR2 (shmem)\n");
		err = ENXIO;
		goto fail;
	}
	sc->shm_paddr = rman_get_start(sc->shm_res);
	sc->shm_size = rman_get_size(sc->shm_res);

	/*
	 * Map BAR1 (MSI-X table + PBA) so pci_alloc_msix accepts it. Both
	 * the table and PBA live in BAR1 here (same BAR), so this one
	 * allocation covers both. Non-fatal: if it fails we just take INTx.
	 */
	sc->msix_bar_rid = PCIR_BAR(1);
	sc->msix_bar_res = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
	    &sc->msix_bar_rid, RF_ACTIVE);
	if (sc->msix_bar_res == NULL)
		device_printf(dev, "warning: cannot map BAR1 (MSI-X table); "
		    "MSI-X will be unavailable\n");

	/*
	 * Doorbell interrupt. Prefer MSI-X (one vector); fall back to legacy
	 * INTx when MSI-X is unavailable. Under macOS HVF, guest MSI
	 * allocation fails platform-wide (every PCI device falls back to
	 * INTx), and the Atrium QEMU ivshmem patch raises INTx via its
	 * poll-timer when MSI-X is off — so INTx is the working doorbell on
	 * the bring-up host. On real HW / working-MSI platforms this still
	 * takes the MSI-X path. The table BAR is mapped by pci_alloc_msix
	 * internally — do NOT pre-allocate it.
	 */
	msix_count = 1;
	if (pci_alloc_msix(dev, &msix_count) == 0 && msix_count >= 1) {
		sc->msix_alloced = 1;
		sc->irq_rid = 1; /* MSI-X vectors start at rid 1 */
		device_printf(dev, "doorbell: MSI-X (1 vector)\n");
	} else {
		sc->irq_rid = 0; /* legacy INTx */
		device_printf(dev, "doorbell: MSI-X unavailable, using legacy INTx\n");
	}

	sc->irq_res = bus_alloc_resource_any(dev, SYS_RES_IRQ, &sc->irq_rid,
	    RF_ACTIVE | RF_SHAREABLE);
	if (sc->irq_res == NULL) {
		device_printf(dev, "cannot allocate IRQ resource\n");
		err = ENXIO;
		goto fail;
	}
	err = bus_setup_intr(dev, sc->irq_res, INTR_TYPE_MISC | INTR_MPSAFE,
	    NULL, carillon_intr, sc, &sc->intr_cookie);
	if (err != 0) {
		device_printf(dev, "bus_setup_intr failed: %d\n", err);
		goto fail;
	}

	/* Unmask the interrupt (BAR0 IntMask = all ones). */
	carillon_reg_write(sc, CARILLON_REG_INTMASK, 0xffffffffu);

	/* cdev. */
	{
		struct make_dev_args args;
		make_dev_args_init(&args);
		args.mda_devsw = &carillon_cdevsw;
		args.mda_uid = UID_ROOT;
		args.mda_gid = GID_WHEEL;
		args.mda_mode = 0600;
		err = make_dev_s(&args, &sc->cdev, "carillon0");
		if (err != 0) {
			device_printf(dev, "make_dev_s failed: %d\n", err);
			goto fail;
		}
		sc->cdev->si_drv1 = sc;
	}

	device_printf(dev,
	    "attached: BAR2 shmem %ju bytes, doorbell ready, /dev/carillon0\n",
	    (uintmax_t)sc->shm_size);
	return (0);

fail:
	if (sc->intr_cookie != NULL)
		bus_teardown_intr(dev, sc->irq_res, sc->intr_cookie);
	if (sc->irq_res != NULL)
		bus_release_resource(dev, SYS_RES_IRQ, sc->irq_rid, sc->irq_res);
	if (sc->msix_alloced)
		pci_release_msi(dev);
	if (sc->msix_bar_res != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, sc->msix_bar_rid,
		    sc->msix_bar_res);
	if (sc->shm_res != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, sc->shm_rid, sc->shm_res);
	if (sc->reg_res != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, sc->reg_rid, sc->reg_res);
	mtx_destroy(&sc->lock);
	return (err);
}

static int
carillon_detach(device_t dev)
{
	struct carillon_softc *sc = device_get_softc(dev);

	if (sc->cdev != NULL)
		destroy_dev(sc->cdev);
	if (sc->intr_cookie != NULL)
		bus_teardown_intr(dev, sc->irq_res, sc->intr_cookie);
	if (sc->irq_res != NULL)
		bus_release_resource(dev, SYS_RES_IRQ, sc->irq_rid, sc->irq_res);
	if (sc->msix_alloced)
		pci_release_msi(dev);
	if (sc->msix_bar_res != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, sc->msix_bar_rid,
		    sc->msix_bar_res);
	if (sc->shm_res != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, sc->shm_rid, sc->shm_res);
	if (sc->reg_res != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, sc->reg_rid, sc->reg_res);
	mtx_destroy(&sc->lock);
	return (0);
}

/* ----------------------------------------------------------------------- */
/* Driver glue                                                              */
/* ----------------------------------------------------------------------- */

static device_method_t carillon_methods[] = {
	DEVMETHOD(device_probe,  carillon_probe),
	DEVMETHOD(device_attach, carillon_attach),
	DEVMETHOD(device_detach, carillon_detach),
	DEVMETHOD_END
};

static driver_t carillon_driver = {
	"carillon",
	carillon_methods,
	sizeof(struct carillon_softc),
};

DRIVER_MODULE(carillon, pci, carillon_driver, NULL, NULL);
MODULE_VERSION(carillon, 1);
MODULE_DEPEND(carillon, pci, 1, 1, 1);
