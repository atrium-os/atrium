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
#define PSP_CMD_WAIT_US		5000000

/* GFX command buffer (psp_gfx_if.h, 1024 bytes) and ring frame (64). */
#define GFX_CMD_BUF_VERSION	1
#define GFX_CMD_ID_LOAD_TOC	0x20
#define GFX_CMD_BUF_LEN		1024
#define GFX_CMD_OFF_SIZE	0
#define GFX_CMD_OFF_VERSION	4
#define GFX_CMD_OFF_ID		8
#define GFX_CMD_OFF_CMD		28	/* union psp_gfx_commands */
#define GFX_CMD_OFF_RESP	864	/* struct psp_gfx_resp */
#define GFX_RESP_OFF_STATUS	0
#define GFX_RESP_OFF_TMR_SIZE	16
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
 * Make CPU writes through the BAR visible to the GPU's own agents (the PSP
 * reads VRAM directly, behind HDP): HDP_MEM_COHERENCY_FLUSH_CNTL, then a read
 * as amdgpu does. NBIF segment 2 from discovery.
 */
static void
psp_hdp_flush(struct atrium_amd_softc *sc)
{
	bus_size_t b = (bus_size_t)sc->ip.nbif.base[2];

	amd_mmio_write32(sc, (b + NBIF6_HDP_MEM_COHERENCY_FLUSH_CNTL) * 4, 0);
	(void)amd_mmio_read32(sc, (b + NBIF6_RCC_CONFIG_MEMSIZE) * 4);
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

/* LOAD_TOC: the PSP reads the table of contents and answers the TMR size. */
static int
psp_load_toc(struct atrium_amd_softc *sc, struct resource *vram,
    uint64_t fb_base, const struct firmware *fw)
{
	uint32_t off, size, status, tmr;
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

	bus_set_region_4(vram, PSP_CMD_VRAM_OFF, 0, PSP_CMD_SIZE / 4);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_SIZE, GFX_CMD_BUF_LEN);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_VERSION,
	    GFX_CMD_BUF_VERSION);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_ID, GFX_CMD_ID_LOAD_TOC);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_CMD + 0, (uint32_t)mc);
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_CMD + 4,
	    (uint32_t)(mc >> 32));
	bus_write_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_CMD + 8, size);
	bus_write_4(vram, PSP_FENCE_VRAM_OFF, 0);
	psp_hdp_flush(sc);

	err = psp_cmd_submit(sc, vram, fb_base, 1, &status);
	tmr = bus_read_4(vram, PSP_CMD_VRAM_OFF + GFX_CMD_OFF_RESP +
	    GFX_RESP_OFF_TMR_SIZE);
	device_printf(sc->dev, "psp: LOAD_TOC (%u bytes): %s, status %#x, "
	    "TMR size %#x (%u MiB)\n", size, err == 0 ? "done" : "failed",
	    status, tmr, tmr >> 20);
	return (err != 0 ? err : (status != 0 ? EIO : 0));
}

int
amd_psp_bootload(struct atrium_amd_softc *sc)
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
	vram = bus_alloc_resource_any(sc->dev, SYS_RES_MEMORY, &rid, RF_ACTIVE);
	if (vram == NULL ||
	    rman_get_size(vram) < PSP_VRAM_END) {
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
		bus_set_region_4(vram, PSP_RING_VRAM_OFF, 0, PSP_RING_SIZE / 4);
		psp_hdp_flush(sc);
		err = psp_ring_create(sc, fb_base + PSP_RING_VRAM_OFF);
	}
	if (err == 0)
		err = psp_load_toc(sc, vram, fb_base, fw);
out:
	if (vram != NULL)
		bus_release_resource(sc->dev, SYS_RES_MEMORY, rid, vram);
	firmware_put(fw, FIRMWARE_UNLOAD);
	return (err);
}
