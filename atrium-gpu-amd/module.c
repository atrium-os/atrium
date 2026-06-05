/*
 * atrium-gpu-amd — from-scratch FreeBSD kernel driver for AMD RDNA4 GPUs.
 *
 * KERNEL side of the Atrium GPU split (kernel = C, userspace = Rust). This
 * is milestone 1 (design §8): newbus PCI bring-up + BAR mapping + the first
 * register read — GRBM_STATUS over PCI — exercised against the gpusim
 * functional model (vendor 0x1002 / device 0x7550) before real silicon.
 *
 * WHY one combined newbus driver here, not the §4.1 pci/gpu/display split:
 * milestone 1 only proves "match the device, satisfy the PCI bring-up gate,
 * read a register." The three-module split earns its keep once firmware +
 * GPUVM + submit exist; introducing it now would be structure without a
 * reason for it (the very thing §2 rejects). Split when a block needs it.
 */

#include <sys/param.h>
#include <sys/module.h>
#include <sys/kernel.h>
#include <sys/systm.h>
#include <sys/bus.h>
#include <sys/rman.h>

#include <machine/bus.h>
#include <machine/resource.h>

#include <dev/pci/pcivar.h>
#include <dev/pci/pcireg.h>

#define ATRIUM_AMD_VENDOR	0x1002
#define ATRIUM_AMD_DEVICE	0x7550	/* RDNA4-class (gpusim target) */

/*
 * BAR5 = the MMIO register file, 256 KiB, 32-bit mem (gpusim device-
 * reference §2). PCIR_BAR(5) is its config-space BAR offset == the rid
 * bus_alloc_resource wants.
 */
#define ATRIUM_AMD_REGS_BAR	PCIR_BAR(5)

/*
 * Register offsets within BAR5. A real GFX12 register's absolute BAR5
 * offset = APER_<block> + reg_dword_offset*4 (device-reference §3).
 *
 * GRBM_STATUS: GC aperture base 0x0_0000 + dword 0x0da4 * 4 = 0x3690.
 * (gc_12_0_0 header; the value/field encodings are modeled functionally,
 *  the offset is bit-exact.)
 */
#define regGRBM_STATUS		0x3690

/*
 * gpusim identity probe register. Per the model (engine/src/device.rs
 * `regs::ID`) this lives at ABSOLUTE BAR5 offset 0x00 — note it is NOT in
 * the SIM aperture despite the device-reference prose; the const is a raw
 * 0x00, not sim(0x00). Reading it returns GPUSIM_MAGIC ('GPUS', stored
 * MSB-first as 0x47505553). Confirms (a) we're talking to the model and
 * (b) Memory-Space-Enable took effect — before MSE the referee leaves BAR
 * reads unclaimed (all-ones), INV-PCI-0002.
 */
#define regSIM_ID		0x00
#define SIM_ID_MAGIC		0x47505553u	/* 'G','P','U','S' (model GPUSIM_MAGIC) */

struct atrium_amd_softc {
	device_t	 dev;
	struct resource	*regs;		/* BAR5 MMIO register file */
	int		 regs_rid;
};

/*
 * Leaf MMIO mechanic (ring_helpers.c tier — §3.3 / §7.1). The ONLY place
 * raw bus_space register access lives; inline here while the driver is one
 * file, lifts to ring_helpers.c when there's a second user. Chip-agnostic:
 * takes a byte offset into the REGS BAR, touches no driver state, no control
 * flow. Callers read as `amd_mmio_read32(sc, regFOO)` and annotate the
 * register's meaning at the call site (§7.1).
 *
 * WHY a leaf helper, not bare bus_read_4 at each site: §7.1 — register
 * access is a leaf, so the BAR handle (and any future ordering/trace policy)
 * lives in one spot, not scattered across every reader. Only the read form
 * exists today; the write form lands with milestone 2 when something writes
 * a register (§7.5: no dead code until then).
 */
static inline uint32_t
amd_mmio_read32(struct atrium_amd_softc *sc, bus_size_t reg)
{
	return (bus_read_4(sc->regs, reg));
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

	sc->dev = dev;

	/*
	 * PCI bring-up gate (device-reference §2, §4; referee INV-PCI-0001/
	 * 0002): the device faults DMA before Bus-Master-Enable and BAR
	 * access before Memory-Space-Enable. pci_enable_busmaster() sets BME;
	 * allocating the BAR below with RF_ACTIVE enables memory-space
	 * decoding (MSE). COMMAND writes are mirrored to the model, which then
	 * unlocks BAR5 — so this ordering is load-bearing, not cosmetic.
	 */
	pci_enable_busmaster(dev);

	sc->regs_rid = ATRIUM_AMD_REGS_BAR;
	sc->regs = bus_alloc_resource_any(dev, SYS_RES_MEMORY, &sc->regs_rid,
	    RF_ACTIVE);
	if (sc->regs == NULL) {
		device_printf(dev, "failed to map BAR5 (MMIO register file)\n");
		return (ENXIO);
	}

	/*
	 * Sanity: the SIM-aperture ID register reads magic 'GPUS'. If MSE
	 * had not taken effect this would read 0xffffffff (the referee
	 * leaving the access unclaimed) — so this doubles as a bring-up check.
	 */
	id = amd_mmio_read32(sc, regSIM_ID);
	device_printf(dev, "identity probe = 0x%08x ('%c%c%c%c')%s\n", id,
	    (int)((id >> 24) & 0xff), (int)((id >> 16) & 0xff),
	    (int)((id >> 8) & 0xff), (int)(id & 0xff),
	    id == SIM_ID_MAGIC ? " OK" : " [UNEXPECTED — not 'GPUS']");

	/*
	 * Milestone 1 (design §8): read GRBM_STATUS over PCI and print it.
	 * Against gpusim this reads 0 — the model is FUNCTIONAL, not register-
	 * complete; reg_read only implements the registers the bring-up
	 * handshake consumes, returning 0 for the rest (GRBM_STATUS among
	 * them). On real Navi 48 silicon this is non-zero (idle/busy state).
	 */
	grbm = amd_mmio_read32(sc, regGRBM_STATUS);
	device_printf(dev, "GRBM_STATUS = 0x%08x (gpusim models this as 0)\n",
	    grbm);

	return (0);
}

static int
atrium_amd_detach(device_t dev)
{
	struct atrium_amd_softc *sc = device_get_softc(dev);

	if (sc->regs != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, sc->regs_rid,
		    sc->regs);
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
MODULE_VERSION(atrium_gpu_amd, 1);
