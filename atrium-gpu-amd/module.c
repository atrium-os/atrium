/*
 * atrium-gpu-amd — from-scratch FreeBSD kernel driver for AMD RDNA4 GPUs.
 *
 * KERNEL side of the Atrium GPU split (kernel = C, userspace = Rust),
 * exercised against the gpusim functional model (vendor 0x1002 / device
 * 0x7550) before real silicon. Milestones (design §8):
 *
 *   M1  newbus PCI bring-up + BAR mapping + first register read (GRBM_STATUS
 *       over PCI), gated behind COMMAND.MSE/BME.
 *   M2  bring the GPU "alive": reset to a known state, load CP microcode
 *       (the PSP firmware load), and initialize the MES scheduler — the
 *       handshake the model's referee requires before any queue/doorbell
 *       can be honored (device-reference §4 steps 2 + 5).
 *
 * WHY one combined newbus driver here, not the §4.1 pci/gpu/display split:
 * through M2 the driver only proves "match the device, satisfy the bring-up
 * gates, and walk the model from cold → CP/MES up." The three-module split
 * earns its keep once GPUVM + submit add real surface area; introducing it
 * now would be structure without a reason for it (the very thing §2
 * rejects). Split when a block needs it.
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

/*
 * SIM-aperture bring-up registers (device-reference §4; model
 * engine/src/device.rs `regs`). These have no single real-HW analog — they
 * model the PSP/MES/reset handshake a real driver drives through a dozen
 * scattered GC/SMU registers. The model places them at APER_SIM (BAR5
 * 0x2_0000) + the documented offset; absolute offsets are spelled out so a
 * reader can match them to the model without arithmetic.
 *
 * Reset handshake (mode-1/FLR class): write REQ=1, poll STATUS until it
 * reads 1 (reset latched, awaiting ack), write ACK=1 (STATUS clears). A full
 * reset wipes engine state, so CP firmware must be (re)loaded afterwards.
 *
 * Firmware: stage FW_CP_VERSION (the ucode the PSP would load), then write
 * FW_CP_LOAD=1 — the model activates the CP only if the staged version is at
 * least CP_FW_MIN_VERSION (an older ucode is refused, mirroring a real
 * firmware-too-old failure). MES_INIT=1 then brings the scheduler up, and is
 * gated on the CP firmware being loaded.
 */
#define ATRIUM_AMD_APER_SIM	0x20000		/* BAR5 SIM-aperture base */
#define regFW_CP_VERSION	(ATRIUM_AMD_APER_SIM + 0x50)	/* staged CP ucode version (w) */
#define regFW_CP_LOAD		(ATRIUM_AMD_APER_SIM + 0x54)	/* w 1: validate + activate CP */
#define regRESET_REQ		(ATRIUM_AMD_APER_SIM + 0x58)	/* w 1: begin full GPU reset */
#define regRESET_STATUS		(ATRIUM_AMD_APER_SIM + 0x5c)	/* r 1: reset latched, awaiting ack */
#define regRESET_ACK		(ATRIUM_AMD_APER_SIM + 0x60)	/* w 1: ack / close reset window */
#define regMES_INIT		(ATRIUM_AMD_APER_SIM + 0x68)	/* w 1: init MES (needs CP fw) */

/*
 * Minimum CP microcode version the model accepts (engine/src/device.rs
 * CP_FW_MIN_VERSION). A real driver derives this from the firmware blob it
 * loads from disk; the model has no blob, so we stage exactly the minimum.
 */
#define ATRIUM_AMD_CP_FW_VERSION	0x40

/*
 * Bounded poll for the reset to latch. The model completes the reset
 * synchronously (STATUS reads 1 on the first poll), but real silicon takes
 * microseconds — so we poll with a timeout rather than assume instant, which
 * is the shape the driver must keep for hardware.
 */
#define ATRIUM_AMD_RESET_POLLS	1000		/* max polls (×10us = 10ms budget) */
#define ATRIUM_AMD_RESET_DELAY	10		/* us between polls */

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
 * WHY a leaf helper, not bare bus_read_4/write_4 at each site: §7.1 —
 * register access is a leaf, so the BAR handle (and any future ordering/
 * trace policy) lives in one spot, not scattered across every caller. The
 * write form lands here at milestone 2, when the reset/firmware/MES
 * handshake first writes registers (§7.5: no dead code before then).
 */
static inline uint32_t
amd_mmio_read32(struct atrium_amd_softc *sc, bus_size_t reg)
{
	return (bus_read_4(sc->regs, reg));
}

static inline void
amd_mmio_write32(struct atrium_amd_softc *sc, bus_size_t reg, uint32_t val)
{
	bus_write_4(sc->regs, reg, val);
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
	int i;

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

	/*
	 * Milestone 2 (design §8): bring the GPU alive — reset, load CP
	 * firmware, init the MES. The order is load-bearing: a full reset
	 * tears down the CP, so firmware must be (re)loaded after it, and the
	 * MES rides on the CP microcode so it can only come up once firmware
	 * is loaded. The model's referee enforces exactly this ordering
	 * (a doorbell before firmware, or a queue map before the MES, faults).
	 *
	 * Step 1 — reset to a known state. A from-scratch driver inherits the
	 * device in whatever state firmware/a prior driver left it; reset to
	 * a defined baseline before programming. RESET_STATUS is the one
	 * bring-up status the model exposes readably, so it doubles as our
	 * verification that SIM-aperture writes land and read back.
	 */
	amd_mmio_write32(sc, regRESET_REQ, 1);
	for (i = 0; i < ATRIUM_AMD_RESET_POLLS; i++) {
		if (amd_mmio_read32(sc, regRESET_STATUS) != 0)
			break;
		DELAY(ATRIUM_AMD_RESET_DELAY);
	}
	if (i == ATRIUM_AMD_RESET_POLLS) {
		device_printf(dev, "GPU reset did not latch (RESET_STATUS "
		    "stuck at 0)\n");
		bus_release_resource(dev, SYS_RES_MEMORY, sc->regs_rid,
		    sc->regs);
		return (ENXIO);
	}
	amd_mmio_write32(sc, regRESET_ACK, 1);
	if (amd_mmio_read32(sc, regRESET_STATUS) != 0) {
		device_printf(dev, "GPU reset window did not close after ACK "
		    "(RESET_STATUS still 1)\n");
		bus_release_resource(dev, SYS_RES_MEMORY, sc->regs_rid,
		    sc->regs);
		return (ENXIO);
	}
	device_printf(dev, "GPU reset complete (REQ -> STATUS=1 -> ACK -> "
	    "STATUS=0)\n");

	/*
	 * Step 2 — load CP firmware (models the PSP loading the CP microcode).
	 * Stage the ucode version, then activate. The model refuses a version
	 * below CP_FW_MIN_VERSION; we stage exactly the minimum since there is
	 * no real blob behind the model. cp_fw_loaded is not read-back-able,
	 * so this is verified by construction (correct version, post-reset
	 * order) — its effect shows up later when a doorbell is honored
	 * instead of faulting.
	 */
	amd_mmio_write32(sc, regFW_CP_VERSION, ATRIUM_AMD_CP_FW_VERSION);
	amd_mmio_write32(sc, regFW_CP_LOAD, 1);
	device_printf(dev, "CP firmware loaded (version 0x%x)\n",
	    ATRIUM_AMD_CP_FW_VERSION);

	/*
	 * Step 3 — initialize the MES scheduler. Gated on the CP firmware
	 * above; the MES is what reads a queue descriptor and activates an
	 * HQD, so it must be up before any queue can be mapped (milestone 3).
	 */
	amd_mmio_write32(sc, regMES_INIT, 1);
	device_printf(dev, "MES initialized — GPU alive\n");

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
MODULE_VERSION(atrium_gpu_amd, 2);
