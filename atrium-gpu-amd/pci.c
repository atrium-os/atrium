/*
 * pci.c — atrium_gpu_amd_pci.ko, the §4.1 base module.
 *
 * Owns the newbus PCI attach to the real `pci` bus (vendor 0x1002 / device
 * 0x7550 = the gpusim functional model): enables bus-mastering, maps the
 * register (BAR5) and doorbell (BAR2) BARs into the SHARED softc, then creates
 * two child devices — "atrium_gpu_amd" and "atrium_gpu_amd_display" — that the
 * GPU and display modules attach to. Both children reach the shared softc (and thus the
 * mapped BARs + inline MMIO accessors) via device_get_softc(device_get_parent()).
 *
 * This is the §4.1 three-module split: the display module loads independently of
 * the GPU module, both depend on this base. The shared softc is THIS module's
 * softc (sizeof(struct atrium_amd_softc)); sc->dev is this PCI device, so every
 * existing helper that takes `sc` and uses sc->dev (reset, gmc, firmware, MSI-X,
 * display registers) works unchanged from either child.
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

#include <sys/module.h>
#include <sys/kernel.h>

static int
atrium_amd_pci_probe(device_t dev)
{
	if (pci_get_vendor(dev) == ATRIUM_AMD_VENDOR &&
	    pci_get_device(dev) == ATRIUM_AMD_DEVICE) {
		device_set_desc(dev, "Atrium AMD RDNA4 GPU (gpusim) — PCI base");
		return (BUS_PROBE_DEFAULT);
	}
	return (ENXIO);
}

static void
atrium_amd_pci_teardown(struct atrium_amd_softc *sc)
{
	/* Unhook the ISR + free the IH ring before the MSI-X vectors it used. */
	amd_irq_teardown(sc);
	amd_ih_fini(sc);
	if (sc->msix_table != NULL) {
		pci_release_msi(sc->dev);
		bus_release_resource(sc->dev, SYS_RES_MEMORY, sc->msix_table_rid,
		    sc->msix_table);
		sc->msix_table = NULL;
	}
	if (sc->doorbell != NULL) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY, sc->doorbell_rid,
		    sc->doorbell);
		sc->doorbell = NULL;
	}
	if (sc->regs != NULL) {
		bus_release_resource(sc->dev, SYS_RES_MEMORY, sc->regs_rid,
		    sc->regs);
		sc->regs = NULL;
	}
	if (sc->lock_inited) {
		/* Detach any lingering vblank knotes, then free the knlist (both tied
		 * to lock_inited), before destroying the lock they share. */
		seldrain(&sc->display_sel);
		knlist_destroy(&sc->display_sel.si_note);
		mtx_destroy(&sc->lock);
		sc->lock_inited = 0;
	}
}

static int
atrium_amd_pci_attach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(dev);
	device_t child;

	sc->dev = dev;
	sc->energy_member = -1;
	sc->display_energy_member = -1;
	mtx_init(&sc->lock, "atrium-gpu", NULL, MTX_DEF);
	sc->lock_inited = 1;
	/*
	 * The vblank knote list shares sc->lock and lives as long as the lock does
	 * (tied to lock_inited). The GPU module's IH ISR fires it from inside the
	 * sc->lock-held retire path; the display cdev registers EVFILT_READ knotes
	 * on it. Init here so the ISR's KNOTE_LOCKED is always valid.
	 */
	knlist_init_mtx(&sc->display_sel.si_note, &sc->lock);

	/*
	 * PCI bring-up gate (referee INV-PCI-0001/0002): the device faults DMA
	 * before Bus-Master-Enable and BAR access before Memory-Space-Enable.
	 * pci_enable_busmaster sets BME; RF_ACTIVE BAR alloc enables MSE.
	 */
	pci_enable_busmaster(dev);

	sc->regs_rid = ATRIUM_AMD_REGS_BAR;
	sc->regs = bus_alloc_resource_any(dev, SYS_RES_MEMORY, &sc->regs_rid,
	    RF_ACTIVE);
	if (sc->regs == NULL) {
		device_printf(dev, "failed to map BAR5 (MMIO register file)\n");
		atrium_amd_pci_teardown(sc);
		return (ENXIO);
	}
	sc->doorbell_rid = ATRIUM_AMD_DOORBELL_BAR;
	sc->doorbell = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
	    &sc->doorbell_rid, RF_ACTIVE);
	if (sc->doorbell == NULL) {
		device_printf(dev, "failed to map BAR2 (doorbell)\n");
		atrium_amd_pci_teardown(sc);
		return (ENXIO);
	}

	/*
	 * MSI-X allocation is the device OWNER's job (§4.1: "MSI-X allocation,
	 * IRQ vector routing" lives in the base). pci(9) needs the MSI-X table
	 * BAR resident before pci_alloc_msix(); we enable the vectors here, and a
	 * child (gpu) bus_alloc's vector 0 + hooks its own ISR. Doing this from a
	 * non-owning child is what silently broke interrupt delivery. Non-fatal:
	 * children fall back to the synchronous-drain (poll) path.
	 */
	sc->msix_table_rid = pci_msix_table_bar(dev);
	sc->msix_table = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
	    &sc->msix_table_rid, RF_ACTIVE);
	if (sc->msix_table != NULL) {
		int count = 1;	/* the model signals vector 0 */
		if (pci_alloc_msix(dev, &count) != 0 || count < 1) {
			bus_release_resource(dev, SYS_RES_MEMORY,
			    sc->msix_table_rid, sc->msix_table);
			sc->msix_table = NULL;
			device_printf(dev, "MSI-X unavailable — children poll\n");
		}
	}

	/*
	 * Cold device reset: a full FLR is device-wide (it resets GFX *and* DCN),
	 * so the device owner does it ONCE here, before any child attaches — the
	 * children then bring up their own block onto a clean device. (This is why
	 * the gpu module no longer FLRs; it would reset the display behind its back.)
	 * Non-fatal: if the reset doesn't latch we log and press on.
	 */
	if (amd_flr(sc) != 0)
		device_printf(dev, "cold FLR failed — continuing\n");

	/*
	 * Stand up the device-global IH ring + hook the single ISR HERE, in the
	 * device owner — before any child attaches. The IH ring is a device
	 * resource (GFX end-of-pipe AND DCN vblank ride it to one ISR), so it
	 * belongs to the base, not the gpu module; each IP child later registers a
	 * handler for its own cause (amd_ih_set_handler). Both non-fatal: without
	 * the ring/ISR the device still works via the synchronous-drain poll path.
	 */
	if (amd_ih_init(sc) != 0)
		device_printf(dev, "IH ring unavailable — children poll\n");
	else if (amd_irq_setup(sc) == 0)
		device_printf(dev, "IH ring + MSI-X up (interrupt-driven)\n");
	else
		device_printf(dev, "MSI-X unavailable — children poll\n");

	/*
	 * The GPU (compute/render) and display engine are distinct IP blocks on
	 * the one device (GFX + DCN). Expose each as a child so its driver is a
	 * separate, independently-loadable kmod (§4.1). bus_generic_attach probes
	 * them against whichever of atrium_gpu_amd.ko / atrium_gpu_amd_display.ko
	 * are loaded.
	 */
	/*
	 * Name each child after the driver that claims it: device_add_child sets
	 * the child's devclass to this name, so only the driver of the same name
	 * attaches (atrium_gpu_amd.ko → the gpu child, atrium_gpu_amd_display.ko →
	 * the display child). A mismatched name = the child never attaches.
	 */
	child = device_add_child(dev, "atrium_gpu_amd", DEVICE_UNIT_ANY);
	if (child == NULL)
		device_printf(dev, "could not add atrium_gpu_amd child\n");
	child = device_add_child(dev, "atrium_gpu_amd_display", DEVICE_UNIT_ANY);
	if (child == NULL)
		device_printf(dev, "could not add atrium_gpu_amd_display child\n");

	bus_attach_children(dev);

	device_printf(dev, "PCI base ready (BARs mapped; gpu/display children added)\n");
	return (0);
}

static int
atrium_amd_pci_detach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(dev);
	int err;

	/* Children (gpu/display) detach first; they refuse if busy (open fds). */
	err = bus_detach_children(dev);
	if (err != 0)
		return (err);
	device_delete_children(dev);
	atrium_amd_pci_teardown(sc);
	return (0);
}

static device_method_t atrium_amd_pci_methods[] = {
	DEVMETHOD(device_probe,		atrium_amd_pci_probe),
	DEVMETHOD(device_attach,	atrium_amd_pci_attach),
	DEVMETHOD(device_detach,	atrium_amd_pci_detach),

	/* Bus passthrough so children can route resource/interrupt requests
	 * (e.g. the GPU child's MSI-X) up to the real PCI bus. */
	DEVMETHOD(bus_alloc_resource,	bus_generic_alloc_resource),
	DEVMETHOD(bus_release_resource,	bus_generic_release_resource),
	DEVMETHOD(bus_activate_resource, bus_generic_activate_resource),
	DEVMETHOD(bus_deactivate_resource, bus_generic_deactivate_resource),
	DEVMETHOD(bus_setup_intr,	bus_generic_setup_intr),
	DEVMETHOD(bus_teardown_intr,	bus_generic_teardown_intr),
	DEVMETHOD_END
};

static driver_t atrium_amd_pci_driver = {
	"atrium_gpu_amd_pci",
	atrium_amd_pci_methods,
	sizeof(struct atrium_amd_softc),	/* the SHARED softc */
};

DRIVER_MODULE(atrium_gpu_amd_pci, pci, atrium_amd_pci_driver, NULL, NULL);
MODULE_DEPEND(atrium_gpu_amd_pci, pci, 1, 1, 1);
MODULE_VERSION(atrium_gpu_amd_pci, 1);

/*
 * PnP table — lets devmatch(8)/devd identify and autoload this driver from the
 * device's PCI vendor:device. This is the discovery half of selective (non-
 * preload) loading: rather than carrying every vendor's base in loader.conf,
 * devmatch matches the GPU present against each base's MODULE_PNP_INFO and
 * loads only the one that fits. NB: a VGA-class GPU is still claimed by the
 * built-in vgapci at enumeration, so devmatch's load alone won't *bind* it —
 * the bind happens by preloading (win the probe) or a forced handoff at
 * display bring-up (devctl set driver -f). devmatch supplies the identity;
 * the bind policy is separate. The kld build's kldxref folds this into
 * /boot/.../linker.hints so devmatch can read it without loading the module.
 */
struct atrium_amd_pci_id { uint16_t vendor; uint16_t device; };
static const struct atrium_amd_pci_id atrium_amd_pci_ids[] = {
	{ ATRIUM_AMD_VENDOR, ATRIUM_AMD_DEVICE },
};
MODULE_PNP_INFO("U16:vendor;U16:device", pci, atrium_gpu_amd_pci,
    atrium_amd_pci_ids, nitems(atrium_amd_pci_ids));
