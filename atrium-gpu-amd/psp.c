/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * psp.c — stage 3 of the silicon bring-up (docs/spec/r9700-bare-metal.md
 * §4.1): hand the PSP bootloader its firmware pieces so the secure OS (sOS)
 * starts. Opt-in and test-only for now: runs from the base attach on silicon
 * only when the kenv hw.atrium_gpu.psp_bootload=1 is set before kldload, and
 * the attach still refuses afterwards.
 *
 * Interface: the C2PMSG mailbox as Linux's MIT-licensed psp_v14_0.c drives
 * it (offsets from mp_14_0_2_offset.h). Selected by the PSP's discovered
 * version (14.x), never by device id. The firmware file is opaque; the Rust
 * planner (rs/psp_plan.rs via base/base_rs.rs) reads only its descriptor
 * table and returns which byte ranges to send with which command, in order.
 *
 * After the sOS is up (or if it already was): create the kernel-mode (GPCOM)
 * ring and send one command, LOAD_TOC, whose response carries the TMR size
 * the PSP wants: a harmless check that the ring protocol works end to end.
 *
 * Second opt-in, hw.atrium_gpu.psp_ipfw=1: after LOAD_TOC, query the PSP's
 * firmware reservation, then hand it every GC/SDMA/SMU microcode image with
 * LOAD_IP_FW (order and types from the Rust planner, rs/ip_fw.rs), send
 * AUTOLOAD_RLC and wait for the RLC to report bootload complete. Once per
 * cold boot: if the RLC already reports it, nothing is sent.
 *
 * The staging buffer lives in VRAM (amdgpu's debug_use_vram_fw_buf path),
 * written through BAR0; its GPU address is MMHUB's FB_LOCATION_BASE << 24
 * plus the offset, as the VBIOS programmed it. No bus mastering is needed.
 */
#include "atrium_gpu_amd.h"

#include <sys/endian.h>
#include <sys/firmware.h>

#include <dev/pci/pcireg.h>
#include <dev/pci/pcivar.h>

/* MP0 14.x mailbox, dword offsets in MP0 segment 0. */
#define PSP14_C2PMSG_35		0x63	/* bootloader command / ready (bit 31) */
#define PSP14_C2PMSG_36		0x64	/* bootloader buffer address >> 20 */
#define PSP14_C2PMSG_64		0x80	/* ring control: command / response */
#define PSP14_C2PMSG_67		0x83	/* KM ring write pointer (dwords) */
#define PSP14_C2PMSG_69		0x85	/* ring address lo */
#define PSP14_C2PMSG_70		0x86	/* ring address hi */
#define PSP14_C2PMSG_71		0x87	/* ring size */
#define PSP14_C2PMSG_81		0x91	/* sOS sign of life */
#define PSP_TOS_RESP		0x80000000u
#define PSP_TOS_MASK		0x8000ffffu	/* response flag + status */
#define PSP_RING_TYPE_KM	2
#define PSP_CTRL_DESTROY_RINGS	0x30000u
/* NBIF 6.x segment 2 (nbif_6_3_1_offset.h): HDP write flush, then a read. */
#define NBIF6_HDP_MEM_COHERENCY_FLUSH_CNTL 0xf7
#define NBIF6_RCC_CONFIG_MEMSIZE	0xc3
#define PSP_BL_READY		0x80000000u
/* MMHUB 4.x, dword offset in segment 0. */
#define MMHUB4_FB_LOCATION_BASE	0x554
#define MMHUB4_FB_BASE_MASK	0x00ffffffu

#define PSP_BUF_SIZE		(1u << 20)
#define PSP_BUF_VRAM_OFF	(64u << 20)	/* 1 MiB aligned, inside BAR0 */
#define PSP_RING_VRAM_OFF	(PSP_BUF_VRAM_OFF + PSP_BUF_SIZE)
#define PSP_RING_SIZE		0x1000u
#define PSP_CMD_VRAM_OFF	(PSP_RING_VRAM_OFF + PSP_RING_SIZE)
#define PSP_CMD_SIZE		0x1000u
#define PSP_FENCE_VRAM_OFF	(PSP_CMD_VRAM_OFF + PSP_CMD_SIZE)
#define PSP_VRAM_END		(PSP_FENCE_VRAM_OFF + 0x1000u)
/* IP microcode images, each 4 KiB aligned (LOAD_IP_FW wants that). */
#define PSP_FW_VRAM_OFF		(68u << 20)
#define PSP_FW_AREA		(16u << 20)
#define PSP_FW_VRAM_END		(PSP_FW_VRAM_OFF + PSP_FW_AREA)
#define PSP_CMD_WAIT_US		5000000

/* GFX command buffer (psp_gfx_if.h, 1024 bytes) and ring frame (64). */
#define GFX_CMD_BUF_VERSION	1
#define GFX_CMD_ID_LOAD_IP_FW	0x06
#define GFX_CMD_ID_LOAD_TOC	0x20
#define GFX_CMD_ID_AUTOLOAD_RLC	0x21
#define GFX_CMD_ID_FB_FW_RESERV_ADDR	0x50
#define GFX_CMD_ID_FB_FW_RESERV_EXT_ADDR 0x51
#define PSP_ERR_UNKNOWN_COMMAND	0x100
#define GFX_CMD_BUF_LEN		1024
#define GFX_CMD_OFF_SIZE	0
#define GFX_CMD_OFF_VERSION	4
#define GFX_CMD_OFF_ID		8
#define GFX_CMD_OFF_CMD		28	/* union psp_gfx_commands */
#define GFX_CMD_OFF_RESP	864	/* struct psp_gfx_resp */
#define GFX_RESP_OFF_STATUS	0
#define GFX_RESP_OFF_FW_ADDR_LO	8
#define GFX_RESP_OFF_FW_ADDR_HI	12
#define GFX_RESP_OFF_TMR_SIZE	16
#define GFX_RESP_OFF_URESP	64	/* union psp_gfx_uresp */
#define RB_FRAME_LEN		64
#define RB_OFF_CMD_LO		0
#define RB_OFF_CMD_HI		4
#define RB_OFF_CMD_SIZE		8
#define RB_OFF_FENCE_LO		12
#define RB_OFF_FENCE_HI		16
#define RB_OFF_FENCE_VAL	20
#define PSP_WAIT_US		2000000		/* per step */

#define PSP_SOS_CMD		0x20000u
#define PSP_TYPE_TOC		4

struct amd_psp_step {
	uint32_t cmd, fw_type, fw_version, offset, size;
};
int	atrium_amd_psp_plan(const uint8_t *bin, size_t len,
	    struct amd_psp_step *out, size_t max);	/* Rust */
int	atrium_amd_psp_find(const uint8_t *bin, size_t len, uint32_t type,
	    uint32_t *off, uint32_t *size);		/* Rust */
#define PSP_MAX_STEPS		10

/* GC 12.x (gc_12_0_0_offset.h): dword offsets in GC segments 0 and 1. */
#define GC12_CP_STAT			0x0f40	/* seg 0 */
#define GC12_RLC_RLCS_BOOTLOAD_STATUS	0x4e7c	/* seg 1 */
#define GC12_BOOTLOAD_COMPLETE		0x80000000u
#define PSP_AUTOLOAD_WAIT_US		2000000

struct amd_ip_fw_item {
	uint32_t file, fw_type, offset, size;
};
int	atrium_amd_ip_fw_plan(const uint8_t *const *bins, const size_t *lens,
	    struct amd_ip_fw_item *out, size_t max);	/* Rust, rs/ip_fw.rs */
#define IP_FW_FILES		8
#define IP_FW_MAX_ITEMS		32

static bus_size_t
psp_reg(struct atrium_amd_softc *sc, uint32_t dw)
{
	return (((bus_size_t)sc->ip.mp0.base[0] + dw) * 4);
}

static int
psp_wait(struct atrium_amd_softc *sc, uint32_t dw, uint32_t mask,
    uint32_t want, int changed_from, uint32_t old)
{
	uint32_t v = 0;
	int us;

	for (us = 0; us < PSP_WAIT_US; us += 100) {
		v = amd_mmio_read32(sc, psp_reg(sc, dw));
		if (changed_from ? v != old : (v & mask) == want)
			return (0);
		DELAY(100);
	}
	device_printf(sc->dev, "psp: timeout on C2PMSG reg %#x (last %#x)\n",
	    dw, v);
	return (ETIMEDOUT);
}

/*
 * After an ASIC reset: wait for the PSP bootloader to be ready again and
 * report whether a sOS is running (it should not be). 0 or ETIMEDOUT.
 */
int
amd_psp_wait_bootloader(struct atrium_amd_softc *sc, uint32_t *sos)
{
	int err;

	if (!sc->ip.mp0.present || sc->ip.mp0.major != 14)
		return (ENOTSUP);
	err = psp_wait(sc, PSP14_C2PMSG_35, PSP_BL_READY, PSP_BL_READY, 0, 0);
	if (sos != NULL)
		*sos = amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_81));
	return (err);
}

/*
 * Make CPU writes through the BAR visible to the GPU's own agents (the PSP
 * reads VRAM directly, behind HDP): HDP_MEM_COHERENCY_FLUSH_CNTL, then a read
 * as amdgpu does. NBIF segment 2 from discovery.
 */
static void psp_hdp_flush(struct atrium_amd_softc *sc);

void
amd_hdp_flush(struct atrium_amd_softc *sc)
{
	psp_hdp_flush(sc);
}

static void
psp_hdp_flush(struct atrium_amd_softc *sc)
{
	bus_size_t b = (bus_size_t)sc->ip.nbif.base[2];

	amd_mmio_write32(sc, (b + NBIF6_HDP_MEM_COHERENCY_FLUSH_CNTL) * 4, 0);
	(void)amd_mmio_read32(sc, (b + NBIF6_RCC_CONFIG_MEMSIZE) * 4);
}

/* GFX_CTRL_CMD_ID_DESTROY_RINGS: the sOS forgets any ring (ours or stale). */
static int
psp_ring_destroy(struct atrium_amd_softc *sc)
{
	int err;

	amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_64), PSP_CTRL_DESTROY_RINGS);
	DELAY(20000);
	err = psp_wait(sc, PSP14_C2PMSG_64, PSP_TOS_MASK, PSP_TOS_RESP, 0, 0);
	device_printf(sc->dev, "psp: rings destroyed%s (C2PMSG_64 %#x)\n",
	    err == 0 ? "" : " (no response)",
	    amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_64)));
	return (err);
}

static int
psp_ring_create(struct atrium_amd_softc *sc, uint64_t ring_mc)
{
	int err;

	err = psp_wait(sc, PSP14_C2PMSG_64, PSP_TOS_MASK, PSP_TOS_RESP, 0, 0);
	if (err != 0) {
		device_printf(sc->dev, "psp: sOS not ready for a ring\n");
		return (err);
	}
	amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_69), (uint32_t)ring_mc);
	amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_70),
	    (uint32_t)(ring_mc >> 32));
	amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_71), PSP_RING_SIZE);
	amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_64),
	    PSP_RING_TYPE_KM << 16);
	DELAY(20000);
	err = psp_wait(sc, PSP14_C2PMSG_64, PSP_TOS_MASK, PSP_TOS_RESP, 0, 0);
	device_printf(sc->dev, "psp: KM ring %s (C2PMSG_64 %#x)\n",
	    err == 0 ? "created" : "NOT created",
	    amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_64)));
	return (err);
}

/*
 * Submit the 1 KiB command already written at PSP_CMD_VRAM_OFF; wait for the
 * fence; return the response status.
 */
static int
psp_cmd_submit(struct atrium_amd_softc *sc, struct resource *vram,
    uint64_t fb_base, uint32_t fence, uint32_t *status)
{
	uint8_t frame[RB_FRAME_LEN];
	uint64_t cmd_mc = fb_base + PSP_CMD_VRAM_OFF;
	uint64_t fence_mc = fb_base + PSP_FENCE_VRAM_OFF;
	uint32_t wptr, slot;
	int us;

	memset(frame, 0, sizeof(frame));
	le32enc(frame + RB_OFF_CMD_LO, (uint32_t)cmd_mc);
	le32enc(frame + RB_OFF_CMD_HI, (uint32_t)(cmd_mc >> 32));
	le32enc(frame + RB_OFF_CMD_SIZE, GFX_CMD_BUF_LEN);
	le32enc(frame + RB_OFF_FENCE_LO, (uint32_t)fence_mc);
	le32enc(frame + RB_OFF_FENCE_HI, (uint32_t)(fence_mc >> 32));
	le32enc(frame + RB_OFF_FENCE_VAL, fence);

	wptr = amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_67));
	slot = (wptr % (PSP_RING_SIZE / 4)) / (RB_FRAME_LEN / 4);
	bus_write_region_1(vram, PSP_RING_VRAM_OFF + slot * RB_FRAME_LEN,
	    frame, sizeof(frame));
	psp_hdp_flush(sc);
	amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_67),
	    (wptr + RB_FRAME_LEN / 4) % (PSP_RING_SIZE / 4));

	for (us = 0; us < PSP_CMD_WAIT_US; us += 200) {
		if (bus_read_4(vram, PSP_FENCE_VRAM_OFF) == fence)
			break;
		DELAY(200);
	}
	*status = bus_read_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_RESP +
	    GFX_RESP_OFF_STATUS);
	if (us >= PSP_CMD_WAIT_US) {
		device_printf(sc->dev, "psp: command timed out (wptr %#x, "
		    "fence %#x)\n", wptr, bus_read_4(vram, PSP_FENCE_VRAM_OFF));
		return (ETIMEDOUT);
	}
	return (0);
}

/*
 * Build a GFX command (`cmd_id`, `nargs` dwords of its union) at
 * PSP_CMD_VRAM_OFF, submit it with fence value `fence`, and return the
 * response's status in *status. The response is pre-filled with a pattern so
 * fields the PSP leaves alone stand out.
 */
static int
psp_cmd(struct atrium_amd_softc *sc, struct resource *vram, uint64_t fb_base,
    uint32_t cmd_id, const uint32_t *args, int nargs, uint32_t fence,
    uint32_t *status)
{
	int k;

	bus_set_region_4(vram, PSP_CMD_VRAM_OFF, 0, PSP_CMD_SIZE / 4);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_SIZE, GFX_CMD_BUF_LEN);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_VERSION,
	    GFX_CMD_BUF_VERSION);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_ID, cmd_id);
	for (k = 0; k < nargs; k++)
		bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_CMD + 4 * k,
		    args[k]);
	bus_set_region_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_RESP, 0xdeadbeef,
	    (GFX_CMD_BUF_LEN - GFX_CMD_OFF_RESP) / 4);
	bus_write_4(vram, PSP_FENCE_VRAM_OFF, 0);
	psp_hdp_flush(sc);
	return (psp_cmd_submit(sc, vram, fb_base, fence, status));
}

static uint32_t
psp_resp(struct resource *vram, uint32_t off)
{
	return (bus_read_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_RESP + off));
}

/* LOAD_TOC: the PSP reads the table of contents and answers the TMR size. */
static int
psp_load_toc(struct atrium_amd_softc *sc, struct resource *vram,
    uint64_t fb_base, const struct firmware *fw, uint32_t fence)
{
	uint32_t off, size, status, tmr, args[3];
	uint64_t mc = fb_base + PSP_BUF_VRAM_OFF;
	int r, err;

	r = atrium_amd_psp_find(fw->data, fw->datasize, PSP_TYPE_TOC, &off,
	    &size);
	if (r <= 0) {
		device_printf(sc->dev, "psp: no TOC in the firmware file (%d)\n",
		    r);
		return (ENOENT);
	}
	bus_set_region_4(vram, PSP_BUF_VRAM_OFF, 0, PSP_BUF_SIZE / 4);
	bus_write_region_1(vram, PSP_BUF_VRAM_OFF,
	    (const uint8_t *)fw->data + off, size);

	psp_hdp_flush(sc);
	args[0] = (uint32_t)mc;
	args[1] = (uint32_t)(mc >> 32);
	args[2] = size;
	err = psp_cmd(sc, vram, fb_base, GFX_CMD_ID_LOAD_TOC, args, 3, fence,
	    &status);
	tmr = bus_read_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_RESP +
	    GFX_RESP_OFF_TMR_SIZE);
	{
		uint32_t w[8];
		int k;

		for (k = 0; k < 8; k++)
			w[k] = bus_read_4(vram, PSP_CMD_VRAM_OFF +
			    GFX_CMD_OFF_RESP + 4 * k);
		device_printf(sc->dev, "psp: fence %#x; resp %08x %08x %08x "
		    "%08x %08x %08x %08x %08x\n",
		    bus_read_4(vram, PSP_FENCE_VRAM_OFF), w[0], w[1], w[2],
		    w[3], w[4], w[5], w[6], w[7]);
	}
	device_printf(sc->dev, "psp: LOAD_TOC (%u bytes): %s, status %#x, "
	    "TMR size %#x (%u MiB)\n", size, err == 0 ? "done" : "failed",
	    status, tmr, tmr >> 20);
	return (err != 0 ? err : (status != 0 ? EIO : 0));
}

/*
 * FB_FW_RESERV_ADDR / _EXT_ADDR: where the PSP keeps its own firmware in
 * VRAM (amdgpu's psp_update_fw_reservation, PSP 14). Read only: logged so the
 * VRAM allocator can keep out of it; an sOS that does not know the command
 * answers PSP_ERR_UNKNOWN_COMMAND.
 */
static void
psp_fw_reservation(struct atrium_amd_softc *sc, struct resource *vram,
    uint64_t fb_base, uint32_t *fence)
{
	static const struct { uint32_t id; const char *name; } q[] = {
		{ GFX_CMD_ID_FB_FW_RESERV_ADDR, "fw reservation" },
		{ GFX_CMD_ID_FB_FW_RESERV_EXT_ADDR, "fw reservation (ext)" },
	};
	uint64_t memsize, addr;
	uint32_t status, size;
	u_int i;

	memsize = (uint64_t)amd_mmio_read32(sc,
	    ((bus_size_t)sc->ip.nbif.base[2] + NBIF6_RCC_CONFIG_MEMSIZE) * 4) << 20;
	CTASSERT(nitems(q) <= nitems(sc->psp_resv_off));
	for (i = 0; i < nitems(q); i++) {
		sc->psp_resv_off[i] = 0;
		sc->psp_resv_size[i] = 0;
	}
	for (i = 0; i < nitems(q); i++) {
		if (psp_cmd(sc, vram, fb_base, q[i].id, NULL, 0, ++*fence,
		    &status) != 0)
			return;
		if (status == PSP_ERR_UNKNOWN_COMMAND) {
			device_printf(sc->dev, "psp: %s: not supported\n",
			    q[i].name);
			continue;
		}
		addr = (uint64_t)psp_resp(vram, GFX_RESP_OFF_URESP) << 32 |
		    psp_resp(vram, GFX_RESP_OFF_URESP + 4);
		size = psp_resp(vram, GFX_RESP_OFF_URESP + 8);
		if (status == 0 && size != 0) {
			sc->psp_resv_off[i] = addr;
			sc->psp_resv_size[i] = size;
		}
		device_printf(sc->dev, "psp: %s: status %#x, VRAM %#jx + %#x "
		    "(%u MiB)%s\n", q[i].name, status, (uintmax_t)addr, size,
		    size >> 20, size != 0 && addr + size != memsize ?
		    " NOT at the top of VRAM" : "");
	}
}

static bool
gc_reg_ok(struct atrium_amd_softc *sc, u_int seg, uint32_t dw)
{
	return (sc->ip.gc.nbase > seg &&
	    ((bus_size_t)sc->ip.gc.base[seg] + dw) * 4 + 4 <=
	    rman_get_size(sc->regs));
}

static uint32_t
gc_read(struct atrium_amd_softc *sc, u_int seg, uint32_t dw)
{
	return (amd_mmio_read32(sc, ((bus_size_t)sc->ip.gc.base[seg] + dw) * 4));
}

/*
 * LOAD_IP_FW for every GC/SDMA/SMU image, then AUTOLOAD_RLC. Files are
 * picked by discovered version; the planner says what goes where.
 */
static int
psp_load_ip_fw(struct atrium_amd_softc *sc, struct resource *vram,
    uint64_t fb_base, uint32_t *fence)
{
	static const char *const suffix[IP_FW_FILES] = {
		NULL, NULL, "pfp", "me", "mec", "uni_mes", "imu", "rlc",
	};
	const struct firmware *fw[IP_FW_FILES] = { 0 };
	const uint8_t *bins[IP_FW_FILES];
	size_t lens[IP_FW_FILES];
	struct amd_ip_fw_item it[IP_FW_MAX_ITEMS];
	uint32_t at[IP_FW_MAX_ITEMS], args[4], status, cp, bl, end;
	const struct amd_ip_ver *gc = &sc->ip.gc, *sd = &sc->ip.sdma0,
	    *mp1 = &sc->ip.mp1;
	char name[64];
	int i, n, us, err = 0;

	if (!gc->present || gc->major != 12 || !sd->present || sd->major != 7 ||
	    !mp1->present || mp1->major != 14) {
		device_printf(sc->dev, "psp: GC %u.x / SDMA %u.x / SMU %u.x: no "
		    "IP firmware plan here\n", gc->major, sd->major, mp1->major);
		return (ENOTSUP);
	}
	if (!gc_reg_ok(sc, 0, GC12_CP_STAT) ||
	    !gc_reg_ok(sc, 1, GC12_RLC_RLCS_BOOTLOAD_STATUS))
		return (ENXIO);
	bl = gc_read(sc, 1, GC12_RLC_RLCS_BOOTLOAD_STATUS);
	cp = gc_read(sc, 0, GC12_CP_STAT);
	device_printf(sc->dev, "psp: before: RLC bootload status %#x, CP_STAT "
	    "%#x\n", bl, cp);
	if (bl & GC12_BOOTLOAD_COMPLETE) {
		device_printf(sc->dev, "psp: GC firmware already loaded this "
		    "boot; not loading again (needs a reset first)\n");
		return (0);
	}

	for (i = 0; i < IP_FW_FILES; i++) {
		if (i == 0)
			snprintf(name, sizeof(name), "amdgpu/smu_%u_%u_%u.bin",
			    mp1->major, mp1->minor, mp1->rev);
		else if (i == 1)
			snprintf(name, sizeof(name), "amdgpu/sdma_%u_%u_%u.bin",
			    sd->major, sd->minor, sd->rev);
		else
			snprintf(name, sizeof(name), "amdgpu/gc_%u_%u_%u_%s.bin",
			    gc->major, gc->minor, gc->rev, suffix[i]);
		fw[i] = firmware_get_flags(name, FIRMWARE_GET_NOWARN);
		if (fw[i] == NULL) {
			device_printf(sc->dev, "psp: no /boot/firmware/%s\n",
			    name);
			err = ENOENT;
			goto out;
		}
		bins[i] = fw[i]->data;
		lens[i] = fw[i]->datasize;
	}
	n = atrium_amd_ip_fw_plan(bins, lens, it, IP_FW_MAX_ITEMS);
	if (n <= 0) {
		device_printf(sc->dev, "psp: IP firmware refused by the planner "
		    "(%d)\n", n);
		err = EINVAL;
		goto out;
	}

	/* Stage every image, 4 KiB aligned, then flush once. */
	end = PSP_FW_VRAM_OFF;
	for (i = 0; i < n; i++) {
		at[i] = end;
		end = roundup2(end + it[i].size, 0x1000);
		if (end > PSP_FW_VRAM_END) {
			device_printf(sc->dev, "psp: IP firmware does not fit "
			    "%u MiB\n", PSP_FW_AREA >> 20);
			err = EFBIG;
			goto out;
		}
		if (it[i].file >= IP_FW_FILES) {
			err = EINVAL;
			goto out;
		}
		bus_write_region_1(vram, at[i],
		    bins[it[i].file] + it[i].offset, it[i].size);
	}
	psp_hdp_flush(sc);
	device_printf(sc->dev, "psp: %d IP firmware images, %u KiB staged at "
	    "VRAM+%#x\n", n, (end - PSP_FW_VRAM_OFF) >> 10, PSP_FW_VRAM_OFF);

	for (i = 0; i < n; i++) {
		uint64_t mc = fb_base + at[i];

		args[0] = (uint32_t)mc;
		args[1] = (uint32_t)(mc >> 32);
		args[2] = it[i].size;
		args[3] = it[i].fw_type;
		err = psp_cmd(sc, vram, fb_base, GFX_CMD_ID_LOAD_IP_FW, args, 4,
		    ++*fence, &status);
		device_printf(sc->dev, "psp: LOAD_IP_FW type %u, %u bytes: "
		    "status %#x%s\n", it[i].fw_type, it[i].size, status,
		    err != 0 ? " (timed out)" : "");
		if (err == 0 && status != 0)
			err = EIO;
		if (err != 0)
			goto out;
	}

	err = psp_cmd(sc, vram, fb_base, GFX_CMD_ID_AUTOLOAD_RLC, NULL, 0,
	    ++*fence, &status);
	device_printf(sc->dev, "psp: AUTOLOAD_RLC: status %#x%s\n", status,
	    err != 0 ? " (timed out)" : "");
	if (err == 0 && status != 0)
		err = EIO;
	if (err != 0)
		goto out;
	for (us = 0; us < PSP_AUTOLOAD_WAIT_US; us += 100) {
		bl = gc_read(sc, 1, GC12_RLC_RLCS_BOOTLOAD_STATUS);
		cp = gc_read(sc, 0, GC12_CP_STAT);
		if (cp == 0 && (bl & GC12_BOOTLOAD_COMPLETE))
			break;
		DELAY(100);
	}
	device_printf(sc->dev, "psp: RLC autoload %s after %d us: bootload "
	    "status %#x, CP_STAT %#x\n", us < PSP_AUTOLOAD_WAIT_US ?
	    "complete" : "NOT complete", us, bl, cp);
	if (us >= PSP_AUTOLOAD_WAIT_US)
		err = ETIMEDOUT;
out:
	for (i = 0; i < IP_FW_FILES; i++)
		if (fw[i] != NULL)
			firmware_put(fw[i], FIRMWARE_UNLOAD);
	return (err);
}

/*
 * `vram_in`: BAR0 as the caller already holds it (a re-init after a mode 1
 * reset, smu.c amd_si_reinit), or NULL to take it for the load only.
 */
static int
psp_bootload(struct atrium_amd_softc *sc, struct resource *vram_in)
{
	const struct firmware *fw = NULL;
	struct amd_psp_step st[PSP_MAX_STEPS];
	struct resource *vram = NULL;
	char name[64];
	uint64_t fb_base, mc;
	bus_size_t lim;
	int rid, n, i, alive, err = 0;

	if (!sc->ip.mp0.present || sc->ip.mp0.major != 14 ||
	    sc->ip.mp0.nbase < 1) {
		device_printf(sc->dev, "psp: PSP %u.%u.%u has no bootloader "
		    "sequence here; not touching it\n", sc->ip.mp0.major,
		    sc->ip.mp0.minor, sc->ip.mp0.rev);
		return (ENOTSUP);
	}
	if (!sc->ip.mmhub.present || sc->ip.mmhub.major != 4) {
		device_printf(sc->dev, "psp: MMHUB %u.x: VRAM placement unknown\n",
		    sc->ip.mmhub.major);
		return (ENOTSUP);
	}
	lim = rman_get_size(sc->regs);
	if (psp_reg(sc, PSP14_C2PMSG_81) + 4 > lim ||
	    ((bus_size_t)sc->ip.mmhub.base[0] + MMHUB4_FB_LOCATION_BASE) * 4 + 4 > lim)
		return (ENXIO);

	if (!sc->ip.nbif.present || sc->ip.nbif.major != 6 ||
	    sc->ip.nbif.nbase < 3 ||
	    ((bus_size_t)sc->ip.nbif.base[2] + NBIF6_HDP_MEM_COHERENCY_FLUSH_CNTL) * 4 + 4 > lim) {
		device_printf(sc->dev, "psp: NBIF %u.x: no HDP flush known\n",
		    sc->ip.nbif.major);
		return (ENOTSUP);
	}
	alive = amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_81)) != 0;

	snprintf(name, sizeof(name), "amdgpu/psp_%u_%u_%u_sos.bin",
	    sc->ip.mp0.major, sc->ip.mp0.minor, sc->ip.mp0.rev);
	/* NOWARN: firmware(9) first tries a .ko by that name and logs the miss. */
	fw = firmware_get_flags(name, FIRMWARE_GET_NOWARN);
	if (fw == NULL) {
		device_printf(sc->dev, "psp: no /boot/firmware/%s\n", name);
		return (ENOENT);
	}
	n = atrium_amd_psp_plan(fw->data, fw->datasize, st, PSP_MAX_STEPS);
	if (n <= 0) {
		device_printf(sc->dev, "psp: %s rejected (%d)\n", name, n);
		err = EINVAL;
		goto out;
	}

	rid = PCIR_BAR(0);
	vram = vram_in != NULL ? vram_in : bus_alloc_resource_any(sc->dev,
	    SYS_RES_MEMORY, &rid, RF_ACTIVE);
	if (vram == NULL ||
	    rman_get_size(vram) < PSP_FW_VRAM_END) {
		device_printf(sc->dev, "psp: BAR0 (VRAM window) unusable\n");
		err = ENXIO;
		goto out;
	}
	fb_base = (uint64_t)(amd_mmio_read32(sc,
	    ((bus_size_t)sc->ip.mmhub.base[0] + MMHUB4_FB_LOCATION_BASE) * 4) &
	    MMHUB4_FB_BASE_MASK) << 24;
	mc = fb_base + PSP_BUF_VRAM_OFF;
	device_printf(sc->dev, "psp: %s, %d pieces; buffer VRAM+%#x = MC %#jx\n",
	    name, n, PSP_BUF_VRAM_OFF, (uintmax_t)mc);

	if (alive) {
		device_printf(sc->dev, "psp: sOS already running (C2PMSG_81 %#x); "
		    "no bootloader steps\n",
		    amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_81)));
		n = 0;
	}
	for (i = 0; i < n; i++) {
		device_printf(sc->dev, "psp: [%d] type %u version %#x, %u bytes, "
		    "cmd %#x\n", i, st[i].fw_type, st[i].fw_version, st[i].size,
		    st[i].cmd);
		err = psp_wait(sc, PSP14_C2PMSG_35, PSP_BL_READY, PSP_BL_READY, 0, 0);
		if (err != 0)
			break;
		bus_set_region_4(vram, PSP_BUF_VRAM_OFF, 0, PSP_BUF_SIZE / 4);
		bus_write_region_1(vram, PSP_BUF_VRAM_OFF,
		    (const uint8_t *)fw->data + st[i].offset, st[i].size);
		/* Flush HDP so the PSP sees the whole piece. */
		psp_hdp_flush(sc);
		amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_36),
		    (uint32_t)(mc >> 20));
		amd_mmio_write32(sc, psp_reg(sc, PSP14_C2PMSG_35), st[i].cmd);
		if (st[i].cmd == PSP_SOS_CMD) {
			DELAY(20000);	/* as amdgpu: handshake needs a moment */
			err = psp_wait(sc, PSP14_C2PMSG_81, 0, 0, 1, 0);
		} else {
			err = psp_wait(sc, PSP14_C2PMSG_35, PSP_BL_READY,
			    PSP_BL_READY, 0, 0);
		}
		if (err != 0)
			break;
	}
	device_printf(sc->dev, "psp: %s; C2PMSG_35 %#x, C2PMSG_81 %#x\n",
	    err == 0 ? "sOS up" : "stopped",
	    amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_35)),
	    amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_81)));
	if (err == 0) {
		/* A ring left by an earlier load points at memory we may not
		 * own any more; drop it first (amdgpu does on teardown). */
		if (alive)
			(void)psp_ring_destroy(sc);
		bus_set_region_4(vram, PSP_RING_VRAM_OFF, 0, PSP_RING_SIZE / 4);
		psp_hdp_flush(sc);
		err = psp_ring_create(sc, fb_base + PSP_RING_VRAM_OFF);
		if (err == 0) {
			uint32_t fence = 0;
			int ipfw = 0;

			psp_fw_reservation(sc, vram, fb_base, &fence);
			err = psp_load_toc(sc, vram, fb_base, fw, ++fence);
			/* Stage 3c test hook: kenv hw.atrium_gpu.psp_ipfw=1. */
			TUNABLE_INT_FETCH("hw.atrium_gpu.psp_ipfw", &ipfw);
			/*
			 * Loading the IP firmware boots the RLC with GC up,
			 * which the driver cannot idle yet (power P2): needs
			 * hw.atrium_gpu.gc_up_ok=1 too (smu.c).
			 */
			if (ipfw) {
				int gc_up_ok = 0;

				TUNABLE_INT_FETCH("hw.atrium_gpu.gc_up_ok",
				    &gc_up_ok);
				if (!gc_up_ok) {
					device_printf(sc->dev, "psp: IP firmware "
					    "load needs hw.atrium_gpu.gc_up_ok=1 "
					    "(GC cannot be idled yet); skipped\n");
					ipfw = 0;
				}
			}
			if (err == 0 && ipfw)
				err = psp_load_ip_fw(sc, vram, fb_base, &fence);
			/* We do not stay attached: leave no ring behind. */
			(void)psp_ring_destroy(sc);
		}
	}
out:
	if (vram != NULL && vram_in == NULL)
		bus_release_resource(sc->dev, SYS_RES_MEMORY, rid, vram);
	firmware_put(fw, FIRMWARE_UNLOAD);
	return (err);
}

int
amd_psp_bootload(struct atrium_amd_softc *sc)
{
	return (psp_bootload(sc, NULL));
}

int
amd_psp_bootload_on(struct atrium_amd_softc *sc, struct resource *vram)
{
	return (psp_bootload(sc, vram));
}
