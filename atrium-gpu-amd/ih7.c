/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * ih7.c — stage 4b of the silicon bring-up (docs/spec/r9700-bare-metal.md
 * §4.4): the interrupt handler (IH, in OSSSYS) ring 0 on real silicon, its
 * MSI-X or MSI vector, and a self-test interrupt. ih.c is gpusim's model of the
 * same thing and stays as it is until the silicon path replaces it.
 *
 * Interface: Linux's MIT-licensed ih_v7_0.c, amdgpu_ih.c and nbif_v6_3_1.c
 * (offsets osssys_7_0_0_offset.h, nbif_6_3_1_offset.h; fields from the
 * matching *_sh_mask.h). With firmware loaded by the PSP the IH reaches its
 * ring through GPU virtual memory (MC_SPACE 4, VMID 0), so the ring and the
 * write-pointer writeback page are system pages mapped into the GART that
 * stage 4a set up. Read pointer by register (no doorbell); RPTR_REARM so
 * one message is sent per read-pointer write.
 *
 * Each IV is 8 dwords (amdgpu_ih_decode_iv_helper): client 7:0, source
 * 15:8, ring 23:16, vmid 27:24 in dword 0; timestamp; pasid; four data.
 *
 * Self-test: the host raises the MP1 software interrupt (MP1_SMN_IH_SW_INT,
 * the generator the PMFW uses for its SMU-to-host interrupt, ID 0xFE) and
 * expects one IV from client MP1, source 0xFE, then acks it. Whether the
 * host may raise it is not documented; no cookie means "inconclusive", not
 * a failure, and the real proof becomes the first EOP in 4e.
 * Box run 2026-11-12 19:11: ring programmed, IH_STATUS idle throughout,
 * no IV: the IH input never saw the trigger, so the GART path is untested.
 *
 * Everything is per version: ih_gens[] by OSSSYS version, swint_gens[] by
 * MP1 version. An unknown version is not touched (power spec A3.10).
 */
#include "atrium_gpu_amd.h"

#include <sys/bus.h>
#include <sys/rman.h>
#include <machine/atomic.h>
#include <dev/pci/pcireg.h>
#include <dev/pci/pcivar.h>
#include <vm/vm.h>
#include <vm/pmap.h>

#define IH_RING_SIZE		(64u << 10)	/* 2048 IVs */
#define IH_IV_SIZE		32u
/*
 * GART addresses we use. Page 0 stays unmapped (reads go to the dummy page),
 * so firmware that dereferences address 0 never sees live IVs (the MES
 * MSCRATCH lead, 2026-11-12).
 */
#define IH_GART_RING		0x00001000ull
#define IH_GART_WB		(IH_GART_RING + IH_RING_SIZE)

/* IH_RB_CNTL fields (osssys_7_0_0_sh_mask.h). */
#define RB_ENABLE		(1u << 0)
#define RB_SIZE_SHIFT		1
#define RB_SIZE_MASK		(0x3fu << 1)
#define WPTR_WRITEBACK_ENABLE	(1u << 8)
#define WPTR_OVERFLOW_ENABLE	(1u << 16)
#define ENABLE_INTR		(1u << 17)
#define MC_SNOOP		(1u << 20)
#define RPTR_REARM		(1u << 21)
#define MC_RO			(1u << 22)
#define MC_VMID_MASK		(0xfu << 24)
#define MC_SPACE_SHIFT		28
#define MC_SPACE_MASK		(0x7u << 28)
#define MC_SPACE_GPUVA		4u
#define WPTR_OVERFLOW_CLEAR	(1u << 31)
#define WPTR_RB_OVERFLOW	(1u << 0)
#define FLOOD_CNTL_ENABLE	(1u << 3)
#define MSI_STORM_DELAY_MASK	0x7u
#define STORM_CLIENT18		(1u << 18)

/* BIF_BX0_INTERRUPT_CNTL (nbif_6_3_1_sh_mask.h). */
#define IH_DUMMY_RD_OVERRIDE	(1u << 0)
#define IH_REQ_NONSNOOP_EN	(1u << 3)

#define SWINT_ID_MASK		0xffu
#define SWINT_VALID		(1u << 8)
#define SWINT_CTRL_MASK		(1u << 0)
#define SWINT_CTRL_ACK		(1u << 8)
#define SWINT_ID		0xfeu
#define IH_CLIENT_MP1		0x1fu
#define IH_CLIENT_GRBM_CP	0x14u
#define IH_SRC_CP_EOP		0xb5u	/* 181: EOP, incl. MES API fences */

struct ih_gen {
	uint8_t major, minor;
	const char *name;
	/* OSSSYS segment 0 dword offsets. */
	uint32_t rb_cntl, rb_rptr, rb_wptr, rb_base, rb_base_hi;
	uint32_t wptr_addr_hi, wptr_addr_lo, doorbell_rptr;
	uint32_t storm_list, flood_cntl, msi_storm, status, cntl2;
	/* NBIF segment 2. */
	uint32_t bif_int_cntl, bif_int_cntl2;
};

static const struct ih_gen ih_gens[] = {
	{ 7, 0, "OSSSYS 7.0",
	  0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
	  0xaa, 0xd5, 0xf1, 0xc2, 0xc1,
	  0xf1, 0xf2 },
};

struct swint_gen {
	uint8_t major, minor;
	uint32_t sw_int, sw_int_ctrl;	/* MP1 segment 1 */
};

static const struct swint_gen swint_gens[] = {
	{ 14, 0, 0x141, 0x142 },	/* mp_14_0_2_offset.h */
};

struct amd_ih7 {
	struct atrium_amd_softc *sc;
	const struct ih_gen *g;
	struct amd_gart *gt;
	volatile uint32_t *ring;
	volatile uint32_t *wb;
	vm_paddr_t ring_pa, wb_pa;
	uint32_t rptr;			/* bytes */
	struct resource *irq, *msix_table;
	int irq_rid, msix_table_rid;
	void *intr_cookie;
	bool msi;
	const char *kind;		/* MSI-X, MSI or polling */
	volatile u_int nintr;
	uint16_t pci_cmd;
	/*
	 * Stage 5c-2: a consumer (the client node, si_client.c) takes every
	 * IV from the interrupt thread, or from a 10 ms poll without a
	 * vector. `mtx` serialises draining the ring.
	 */
	struct mtx mtx;
	amd_ih7_consumer_fn *fn;
	void *fn_arg;
	struct callout poll;
};

static bus_size_t
oss(struct amd_ih7 *ih, uint32_t dw)
{
	return (((bus_size_t)ih->sc->ip.osssys.base[0] + dw) * 4);
}

static uint32_t
ihr(struct amd_ih7 *ih, uint32_t dw)
{
	return (amd_mmio_read32(ih->sc, oss(ih, dw)));
}

static void
ihw(struct amd_ih7 *ih, uint32_t dw, uint32_t v)
{
	amd_mmio_write32(ih->sc, oss(ih, dw), v);
}

static bus_size_t
nbif(struct amd_ih7 *ih, uint32_t dw)
{
	return (((bus_size_t)ih->sc->ip.nbif.base[2] + dw) * 4);
}

static bus_size_t
mp1(struct atrium_amd_softc *sc, uint32_t dw)
{
	return (((bus_size_t)sc->ip.mp1.base[1] + dw) * 4);
}

static void ih7_consume(struct amd_ih7 *ih);

static int
ih7_filter(void *arg)
{
	struct amd_ih7 *ih = arg;

	atomic_add_int(&ih->nintr, 1);
	/* With a consumer the ring is drained in the interrupt thread. */
	if (atomic_load_ptr(&ih->fn) != NULL)
		return (FILTER_SCHEDULE_THREAD);
	return (FILTER_HANDLED);
}

static void
ih7_ithread(void *arg)
{
	ih7_consume(arg);
}

/*
 * One message-signalled vector for ring 0: MSI-X if the function has it,
 * else plain MSI (the R9700 exposes only MSI, 1 message, 2026-11-12), else
 * none and the ring is polled. Every IH client shares the one vector.
 */
static void
ih7_msi_setup(struct amd_ih7 *ih)
{
	device_t dev = ih->sc->dev;
	int count = 1;

	if (pci_msix_count(dev) > 0) {
		ih->msix_table_rid = pci_msix_table_bar(dev);
		ih->msix_table = bus_alloc_resource_any(dev, SYS_RES_MEMORY,
		    &ih->msix_table_rid, RF_ACTIVE);
		if (ih->msix_table != NULL &&
		    pci_alloc_msix(dev, &count) == 0 && count >= 1)
			ih->kind = "MSI-X";
		else if (ih->msix_table != NULL) {
			bus_release_resource(dev, SYS_RES_MEMORY,
			    ih->msix_table_rid, ih->msix_table);
			ih->msix_table = NULL;
		}
	}
	count = 1;
	if (ih->kind == NULL && pci_msi_count(dev) > 0 &&
	    pci_alloc_msi(dev, &count) == 0 && count >= 1)
		ih->kind = "MSI";
	if (ih->kind == NULL)
		goto fail;
	ih->irq_rid = 1;
	ih->irq = bus_alloc_resource_any(dev, SYS_RES_IRQ, &ih->irq_rid,
	    RF_ACTIVE);
	if (ih->irq == NULL)
		goto fail_msi;
	if (bus_setup_intr(dev, ih->irq, INTR_TYPE_MISC | INTR_MPSAFE,
	    ih7_filter, ih7_ithread, ih, &ih->intr_cookie) != 0)
		goto fail_irq;
	ih->msi = true;
	device_printf(dev, "ih: %s, 1 vector, for every IH client\n",
	    ih->kind);
	return;
fail_irq:
	bus_release_resource(dev, SYS_RES_IRQ, ih->irq_rid, ih->irq);
	ih->irq = NULL;
fail_msi:
	pci_release_msi(dev);
	if (ih->msix_table != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, ih->msix_table_rid,
		    ih->msix_table);
	ih->msix_table = NULL;
fail:
	device_printf(dev, "ih: WARNING: no MSI-X or MSI vector (MSI-X %d, "
	    "MSI %d); polling the ring every 10 ms: completions are late. A "
	    "device that exposes a vector it will not give is one an earlier "
	    "detach leaked it from (reboot to recover)\n", pci_msix_count(dev),
	    pci_msi_count(dev));
	ih->kind = "polling";
}

static void
ih7_msi_teardown(struct amd_ih7 *ih)
{
	device_t dev = ih->sc->dev;

	if (ih->intr_cookie != NULL)
		bus_teardown_intr(dev, ih->irq, ih->intr_cookie);
	ih->intr_cookie = NULL;
	if (ih->irq != NULL) {
		bus_release_resource(dev, SYS_RES_IRQ, ih->irq_rid, ih->irq);
		pci_release_msi(dev);
	}
	ih->irq = NULL;
	if (ih->msix_table != NULL)
		bus_release_resource(dev, SYS_RES_MEMORY, ih->msix_table_rid,
		    ih->msix_table);
	ih->msix_table = NULL;
	ih->msi = false;
}

static void
ih7_report(struct amd_ih7 *ih, const char *when)
{
	device_printf(ih->sc->dev, "ih: %s: RB_CNTL %#x, RPTR %#x, WPTR %#x "
	    "(writeback %#x), STATUS %#x, %s interrupts %u\n", when,
	    ihr(ih, ih->g->rb_cntl), ihr(ih, ih->g->rb_rptr),
	    ihr(ih, ih->g->rb_wptr), ih->wb != NULL ? ih->wb[0] : 0,
	    ihr(ih, ih->g->status), ih->kind != NULL ? ih->kind : "no",
	    ih->nintr);
}

/* Ring off, pointers to 0 (ih_v7_0_toggle_ring_interrupts(false)). */
static void
ih7_ring_off(struct amd_ih7 *ih)
{
	uint32_t c;

	c = ihr(ih, ih->g->rb_cntl);
	ihw(ih, ih->g->rb_cntl, c & ~(RB_ENABLE | ENABLE_INTR));
	ihw(ih, ih->g->rb_rptr, 0);
	ihw(ih, ih->g->rb_wptr, 0);
	ih->rptr = 0;
}

/*
 * Drain: log every IV between our read pointer and the writeback wptr,
 * return how many came from (client, source), then write the read pointer
 * (which re-arms the MSI). Overflow: restart past the oldest (wptr + 32).
 */
static int
ih7_drain(struct amd_ih7 *ih, uint32_t client, uint32_t src, int max_log)
{
	uint32_t wptr, c;
	volatile uint32_t *iv;
	int n = 0, seen = 0;

	mtx_lock(&ih->mtx);
	wptr = ih->wb[0];
	if (wptr & WPTR_RB_OVERFLOW) {
		device_printf(ih->sc->dev, "ih: ring overflow (wptr %#x)\n",
		    wptr);
		wptr &= ~WPTR_RB_OVERFLOW;
		ih->rptr = (wptr + IH_IV_SIZE) & (IH_RING_SIZE - 1);
		c = ihr(ih, ih->g->rb_cntl);
		ihw(ih, ih->g->rb_cntl, c | WPTR_OVERFLOW_CLEAR);
		ihw(ih, ih->g->rb_cntl, c & ~WPTR_OVERFLOW_CLEAR);
	}
	wptr &= IH_RING_SIZE - 1;
	while (ih->rptr != wptr) {
		iv = ih->ring + ih->rptr / 4;
		if ((iv[0] & 0xff) == client && ((iv[0] >> 8) & 0xff) == src)
			seen++;
		if (n++ < max_log)
			device_printf(ih->sc->dev, "ih: IV client %#x src %#x "
			    "ring %u vmid %u pasid %u data %#x %#x %#x %#x\n",
			    iv[0] & 0xff, (iv[0] >> 8) & 0xff,
			    (iv[0] >> 16) & 0xff, (iv[0] >> 24) & 0xf,
			    iv[3] & 0xffff, iv[4], iv[5], iv[6], iv[7]);
		ih->rptr = (ih->rptr + IH_IV_SIZE) & (IH_RING_SIZE - 1);
	}
	if (n > max_log)
		device_printf(ih->sc->dev, "ih: ... %d more IVs\n",
		    n - max_log);
	ihw(ih, ih->g->rb_rptr, ih->rptr);
	mtx_unlock(&ih->mtx);
	return (seen);
}

/*
 * The consumer's drain: each IV between our read pointer and the
 * writeback wptr goes to the consumer, then a NULL marks the end of the
 * batch, then the read pointer is written (re-arming the MSI).
 */
static void
ih7_consume(struct amd_ih7 *ih)
{
	uint32_t wptr, c, iv[IH_IV_SIZE / 4];
	u_int i;

	mtx_lock(&ih->mtx);
	if (ih->fn == NULL) {
		mtx_unlock(&ih->mtx);
		return;
	}
	wptr = ih->wb[0];
	if (wptr & WPTR_RB_OVERFLOW) {
		ih->sc->ih_overflows++;
		wptr &= ~WPTR_RB_OVERFLOW;
		ih->rptr = (wptr + IH_IV_SIZE) & (IH_RING_SIZE - 1);
		c = ihr(ih, ih->g->rb_cntl);
		ihw(ih, ih->g->rb_cntl, c | WPTR_OVERFLOW_CLEAR);
		ihw(ih, ih->g->rb_cntl, c & ~WPTR_OVERFLOW_CLEAR);
	}
	wptr &= IH_RING_SIZE - 1;
	while (ih->rptr != wptr) {
		for (i = 0; i < nitems(iv); i++)
			iv[i] = ih->ring[ih->rptr / 4 + i];
		ih->fn(ih->fn_arg, iv);
		ih->rptr = (ih->rptr + IH_IV_SIZE) & (IH_RING_SIZE - 1);
	}
	ih->fn(ih->fn_arg, NULL);
	ihw(ih, ih->g->rb_rptr, ih->rptr);
	mtx_unlock(&ih->mtx);
}

static void
ih7_poll(void *arg)
{
	struct amd_ih7 *ih = arg;

	ih7_consume(ih);
	if (atomic_load_ptr(&ih->fn) != NULL)
		callout_reset(&ih->poll, MAX(1, hz / 100), ih7_poll, ih);
}

/*
 * Install (fn != NULL) or remove the consumer. Once removal returns, fn
 * is not running and will not be called again.
 */
void
amd_ih7_set_consumer(struct amd_ih7 *ih, amd_ih7_consumer_fn *fn, void *arg)
{
	mtx_lock(&ih->mtx);
	ih->fn_arg = arg;
	atomic_store_ptr(&ih->fn, fn);
	mtx_unlock(&ih->mtx);
	if (fn != NULL) {
		if (!ih->msi)
			callout_reset(&ih->poll, MAX(1, hz / 100), ih7_poll,
			    ih);
		/* Whatever arrived before: take it now. */
		ih7_consume(ih);
	} else
		callout_drain(&ih->poll);
}

static int
ih7_wait(struct amd_ih7 *ih, uint32_t old_wptr, int ms)
{
	int i;

	for (i = 0; i < ms * 10; i++) {
		if (ih->wb[0] != old_wptr)
			return (0);
		DELAY(100);
	}
	return (ETIMEDOUT);
}

static const struct ih_gen *
ih_gen_find(struct atrium_amd_softc *sc)
{
	size_t i;

	for (i = 0; i < nitems(ih_gens); i++)
		if (ih_gens[i].major == sc->ip.osssys.major &&
		    ih_gens[i].minor == sc->ip.osssys.minor)
			return (&ih_gens[i]);
	return (NULL);
}

static const struct swint_gen *
swint_gen_find(struct atrium_amd_softc *sc)
{
	size_t i;

	for (i = 0; i < nitems(swint_gens); i++)
		if (swint_gens[i].major == sc->ip.mp1.major &&
		    swint_gens[i].minor == sc->ip.mp1.minor)
			return (&swint_gens[i]);
	return (NULL);
}

/* Self-test: raise the MP1 software interrupt and look for its IV. */
static void
ih7_selftest(struct amd_ih7 *ih)
{
	struct atrium_amd_softc *sc = ih->sc;
	const struct swint_gen *s = swint_gen_find(sc);
	uint32_t ctrl, old, before;
	int seen, err;

	if (s == NULL) {
		device_printf(sc->dev, "ih: no software interrupt for MP1 "
		    "%u.%u; self-test skipped\n", sc->ip.mp1.major,
		    sc->ip.mp1.minor);
		return;
	}
	ctrl = amd_mmio_read32(sc, mp1(sc, s->sw_int_ctrl));
	old = amd_mmio_read32(sc, mp1(sc, s->sw_int));
	device_printf(sc->dev, "ih: MP1 SW_INT %#x, SW_INT_CTRL %#x before\n",
	    old, ctrl);
	before = ih->nintr;
	/* Unmask, as smu_v14_0_set_irq_state does for the PMFW's own use. */
	amd_mmio_write32(sc, mp1(sc, s->sw_int_ctrl), ctrl & ~SWINT_CTRL_MASK);
	amd_mmio_write32(sc, mp1(sc, s->sw_int),
	    (old & ~SWINT_ID_MASK) | SWINT_ID | SWINT_VALID);
	device_printf(sc->dev, "ih: MP1 SW_INT %#x, SW_INT_CTRL %#x after "
	    "the trigger\n", amd_mmio_read32(sc, mp1(sc, s->sw_int)),
	    amd_mmio_read32(sc, mp1(sc, s->sw_int_ctrl)));
	err = ih7_wait(ih, ih->wb[0], 100);
	DELAY(1000);
	seen = ih7_drain(ih, IH_CLIENT_MP1, SWINT_ID, 16);
	/* Ack (smu_v14_0_irq_process), clear VALID, restore the mask. */
	amd_mmio_write32(sc, mp1(sc, s->sw_int_ctrl),
	    (ctrl & ~SWINT_CTRL_MASK) | SWINT_CTRL_ACK);
	amd_mmio_write32(sc, mp1(sc, s->sw_int),
	    (old & ~(SWINT_ID_MASK | SWINT_VALID)) | SWINT_ID);
	amd_mmio_write32(sc, mp1(sc, s->sw_int_ctrl), ctrl);
	DELAY(1000);
	ih7_report(ih, "after self-test");
	if (seen > 0)
		device_printf(sc->dev, "ih: self-test: %d IV(s) from MP1 "
		    "source %#x, %u %s interrupt(s): PASS\n", seen,
		    SWINT_ID, ih->nintr - before, ih->kind);
	else
		device_printf(sc->dev, "ih: self-test: no IV from MP1 (%s), "
		    "%u %s interrupt(s): inconclusive\n",
		    err == 0 ? "other IVs only" : "wptr never moved",
		    ih->nintr - before, ih->kind);
}

static void ih7_unmap_free(struct amd_ih7 *ih);

/*
 * Ring 0 through the GART and MSI-X, enabled (ih_v7_0_irq_init). The GART
 * (4a) must be up. On success *out holds the live ring; amd_ih7_down undoes
 * it. Used by the stage 4b test and by the attached driver (stage 5a).
 */
int
amd_ih7_up(struct atrium_amd_softc *sc, struct amd_gart *gt,
    struct amd_ih7 **out)
{
	struct amd_ih7 *ihp;
	const struct ih_gen *g;
	device_t dev = sc->dev;
	uint64_t ring_va, wb_va;
	uint32_t c, i;
	int err;

	*out = NULL;
	g = ih_gen_find(sc);
	if (g == NULL) {
		device_printf(dev, "ih: no table for OSSSYS %u.%u; not "
		    "touching it\n", sc->ip.osssys.major, sc->ip.osssys.minor);
		return (ENOTSUP);
	}
	if (!gt->enabled)
		return (ENXIO);
	ihp = malloc(sizeof(*ihp), M_DEVBUF, M_WAITOK | M_ZERO);
#define ih (*ihp)
	mtx_init(&ih.mtx, "atrium-gpu-ih", NULL, MTX_DEF);
	callout_init(&ih.poll, 1);
	ih.sc = sc;
	ih.gt = gt;
	ih.g = g;

	ih.ring = (volatile uint32_t *)contigmalloc(IH_RING_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	ih.wb = (volatile uint32_t *)contigmalloc(PAGE_SIZE, M_DEVBUF, M_WAITOK | M_ZERO, 0,
	    ~(vm_paddr_t)0, PAGE_SIZE, 0);
	ih.ring_pa = vtophys(__DEVOLATILE(void *, ih.ring));
	ih.wb_pa = vtophys(__DEVOLATILE(void *, ih.wb));
	ring_va = gt->gart_start + IH_GART_RING;
	wb_va = gt->gart_start + IH_GART_WB;
	for (i = 0; i < IH_RING_SIZE / PAGE_SIZE; i++)
		(void)amd_gmc12_gart_map(gt, ring_va + i * PAGE_SIZE,
		    ih.ring_pa + i * PAGE_SIZE);
	(void)amd_gmc12_gart_map(gt, wb_va, ih.wb_pa);
	err = amd_gmc12_flush(gt);
	if (err != 0) {
		ih7_unmap_free(ihp);
		return (err);
	}

	/* The IH writes system memory: bus mastering on while it runs. */
	ih.pci_cmd = pci_read_config(dev, PCIR_COMMAND, 2);
	pci_enable_busmaster(dev);

	device_printf(dev, "ih: %s, ring %u KiB at GART %#jx (pa %#jx), "
	    "wptr writeback at GART %#jx\n", g->name, IH_RING_SIZE >> 10,
	    (uintmax_t)ring_va, (uintmax_t)ih.ring_pa, (uintmax_t)wb_va);
	ih.wb[0] = 0;
	ih7_report(&ih, "before");

	/* ih_v7_0_irq_init, ring 0 only, no doorbell. */
	ih7_ring_off(&ih);
	/* nbif_v6_3_1_ih_control: dummy page, dummy read off with MSI, snooped. */
	amd_mmio_write32(sc, nbif(&ih, g->bif_int_cntl2),
	    (uint32_t)(gt->dummy_phys >> 8));
	c = amd_mmio_read32(sc, nbif(&ih, g->bif_int_cntl));
	amd_mmio_write32(sc, nbif(&ih, g->bif_int_cntl),
	    c & ~(IH_DUMMY_RD_OVERRIDE | IH_REQ_NONSNOOP_EN));

	ih7_msi_setup(&ih);

	ihw(&ih, g->rb_base, (uint32_t)(ring_va >> 8));
	ihw(&ih, g->rb_base_hi, (uint32_t)(ring_va >> 40) & 0xff);
	c = ihr(&ih, g->rb_cntl);
	c &= ~(RB_SIZE_MASK | MC_SPACE_MASK | MC_VMID_MASK | MC_RO |
	    RPTR_REARM);
	c |= WPTR_OVERFLOW_CLEAR | WPTR_OVERFLOW_ENABLE |
	    ((uint32_t)(ffs(IH_RING_SIZE / 4) - 1) << RB_SIZE_SHIFT) |
	    WPTR_WRITEBACK_ENABLE | MC_SNOOP |
	    (MC_SPACE_GPUVA << MC_SPACE_SHIFT) | (ih.msi ? RPTR_REARM : 0);
	ihw(&ih, g->rb_cntl, c);
	ihw(&ih, g->wptr_addr_lo, (uint32_t)wb_va);
	ihw(&ih, g->wptr_addr_hi, (uint32_t)(wb_va >> 32) & 0xffff);
	ihw(&ih, g->rb_wptr, 0);
	ihw(&ih, g->rb_rptr, 0);
	ihw(&ih, g->doorbell_rptr, 0);	/* ENABLE = 0 */

	ihw(&ih, g->storm_list, ihr(&ih, g->storm_list) | STORM_CLIENT18);
	ihw(&ih, g->flood_cntl, ihr(&ih, g->flood_cntl) | FLOOD_CNTL_ENABLE);
	ihw(&ih, g->msi_storm,
	    (ihr(&ih, g->msi_storm) & ~MSI_STORM_DELAY_MASK) | 3);

	c = ihr(&ih, g->rb_cntl) & ~WPTR_OVERFLOW_CLEAR;
	ihw(&ih, g->rb_cntl, c | RB_ENABLE | ENABLE_INTR);
	ih7_report(&ih, "enabled");
#undef ih
	*out = ihp;
	return (0);
}

/* Unmap (zero PTEs fault to the dummy page) and free the ring pages. */
static void
ih7_unmap_free(struct amd_ih7 *ih)
{
	struct amd_gart *gt = ih->gt;
	uint64_t ring_va = gt->gart_start + IH_GART_RING;
	uint32_t i;

	for (i = 0; i <= IH_RING_SIZE / PAGE_SIZE; i++) {
		bus_write_4(gt->vram, gt->table_off +
		    ((ring_va - gt->gart_start) >> PAGE_SHIFT) * 8 + i * 8, 0);
		bus_write_4(gt->vram, gt->table_off +
		    ((ring_va - gt->gart_start) >> PAGE_SHIFT) * 8 + i * 8 + 4,
		    0);
	}
	(void)amd_gmc12_flush(gt);
	contigfree(__DEVOLATILE(void *, ih->ring), IH_RING_SIZE, M_DEVBUF);
	contigfree(__DEVOLATILE(void *, ih->wb), PAGE_SIZE, M_DEVBUF);
	mtx_destroy(&ih->mtx);
	free(ih, M_DEVBUF);
}

/*
 * After a card reset under the client node: the IH block's memory stays
 * (leaked; the client's consumer may still point at it), but the vector
 * goes back. Box runs 05:02-05:34: a detach that kept the vector left
 * "Device leaked 1 MSI vectors", and every later attach polled.
 */
void
amd_ih7_vector_off(struct amd_ih7 *ih)
{
	if (ih == NULL)
		return;
	ih7_msi_teardown(ih);
}

/*
 * The card was reset (mode 1) with bus mastering off: the vector back and
 * the memory freed, without the hardware (the GART table is rewritten at the
 * re-init). The consumer must already be off.
 */
void
amd_ih7_free_lost(struct amd_ih7 *ih)
{
	if (ih == NULL)
		return;
	if (ih->fn != NULL)
		amd_ih7_set_consumer(ih, NULL, NULL);
	ih7_msi_teardown(ih);
	callout_drain(&ih->poll);
	contigfree(__DEVOLATILE(void *, ih->ring), IH_RING_SIZE, M_DEVBUF);
	contigfree(__DEVOLATILE(void *, ih->wb), PAGE_SIZE, M_DEVBUF);
	mtx_destroy(&ih->mtx);
	free(ih, M_DEVBUF);
}

/* Ring off, wait, vector off (ih_v7_0_irq_disable), then unmap and free. */
void
amd_ih7_down(struct amd_ih7 *ih)
{
	device_t dev;

	if (ih == NULL)
		return;
	dev = ih->sc->dev;
	if (ih->fn != NULL)
		amd_ih7_set_consumer(ih, NULL, NULL);
	ih7_ring_off(ih);
	DELAY(1000);
	ih7_report(ih, "disabled");
	ih7_msi_teardown(ih);
	if ((ih->pci_cmd & PCIM_CMD_BUSMASTEREN) == 0)
		pci_disable_busmaster(dev);
	ih7_unmap_free(ih);
}

/* Drain the ring, counting CP end-of-pipe IVs (logs up to max_log). */
int
amd_ih7_drain_eop(struct amd_ih7 *ih, int max_log)
{
	return (ih7_drain(ih, IH_CLIENT_GRBM_CP, IH_SRC_CP_EOP, max_log));
}

u_int
amd_ih7_nintr(struct amd_ih7 *ih)
{
	return (ih->nintr);
}

/* True when the ring is served by a vector (MSI-X or MSI), not polled. */
bool
amd_ih7_has_vector(struct amd_ih7 *ih)
{
	return (ih != NULL && ih->msi);
}

/*
 * Stage 4b test: ring 0 through the GART, MSI-X, self-test, then all of it
 * undone. `during`, if not NULL, runs while the ring is live (stage 4d: MES,
 * whose API fences raise CP EOP interrupts, the IH's first real source). The
 * GART (4a) must be enabled; the caller tears the GART down after this
 * returns.
 */
int
amd_ih7_test(struct atrium_amd_softc *sc, struct amd_gart *gt,
    int (*during)(struct atrium_amd_softc *, struct amd_gart *))
{
	struct amd_ih7 *ih;
	device_t dev = sc->dev;
	int err;

	err = amd_ih7_up(sc, gt, &ih);
	if (err != 0) {
		device_printf(dev, "ih: stage 4b: %d\n", err);
		return (err);
	}
	/* Anything already queued by other clients: log it, then self-test. */
	DELAY(10000);
	(void)ih7_drain(ih, 0x100, 0x100, 16);
	ih7_selftest(ih);
	if (during != NULL) {
		uint32_t before = ih->nintr;
		int derr, seen;

		derr = during(sc, gt);
		DELAY(10000);
		seen = ih7_drain(ih, IH_CLIENT_GRBM_CP, IH_SRC_CP_EOP, 32);
		ih7_report(ih, "after stage 4d");
		device_printf(dev, "ih: stage 4d: %d CP EOP IV(s) (client %#x "
		    "src %u), %u %s interrupt(s): %s\n", seen,
		    IH_CLIENT_GRBM_CP, IH_SRC_CP_EOP, ih->nintr - before,
		    ih->kind, seen > 0 ? "IH write path PASS" : derr != 0 ?
		    "MES failed, no verdict" : "none (MES fence IRQs off?)");
	}
	amd_ih7_down(ih);
	device_printf(dev, "ih: stage 4b: 0\n");
	return (0);
}
