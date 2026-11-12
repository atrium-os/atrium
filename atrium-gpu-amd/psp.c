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
 * The staging buffer lives in VRAM (amdgpu's debug_use_vram_fw_buf path),
 * written through BAR0; its GPU address is MMHUB's FB_LOCATION_BASE << 24
 * plus the offset, as the VBIOS programmed it. No bus mastering is needed.
 */
#include "atrium_gpu_amd.h"

#include <sys/firmware.h>

#include <dev/pci/pcireg.h>
#include <dev/pci/pcivar.h>

/* MP0 14.x mailbox, dword offsets in MP0 segment 0. */
#define PSP14_C2PMSG_35		0x63	/* bootloader command / ready (bit 31) */
#define PSP14_C2PMSG_36		0x64	/* bootloader buffer address >> 20 */
#define PSP14_C2PMSG_81		0x91	/* sOS sign of life */
#define PSP_BL_READY		0x80000000u
/* MMHUB 4.x, dword offset in segment 0. */
#define MMHUB4_FB_LOCATION_BASE	0x554
#define MMHUB4_FB_BASE_MASK	0x00ffffffu

#define PSP_BUF_SIZE		(1u << 20)
#define PSP_BUF_VRAM_OFF	(64u << 20)	/* 1 MiB aligned, inside BAR0 */
#define PSP_WAIT_US		2000000		/* per step */

#define PSP_SOS_CMD		0x20000u

struct amd_psp_step {
	uint32_t cmd, fw_type, fw_version, offset, size;
};
int	atrium_amd_psp_plan(const uint8_t *bin, size_t len,
	    struct amd_psp_step *out, size_t max);	/* Rust */
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

int
amd_psp_bootload(struct atrium_amd_softc *sc)
{
	const struct firmware *fw = NULL;
	struct amd_psp_step st[PSP_MAX_STEPS];
	struct resource *vram = NULL;
	char name[64];
	uint64_t fb_base, mc;
	bus_size_t lim;
	int rid, n, i, err = 0;

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

	if (amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_81)) != 0) {
		device_printf(sc->dev, "psp: sOS already running (C2PMSG_81 %#x)\n",
		    amd_mmio_read32(sc, psp_reg(sc, PSP14_C2PMSG_81)));
		return (0);
	}

	snprintf(name, sizeof(name), "amdgpu/psp_%u_%u_%u_sos.bin",
	    sc->ip.mp0.major, sc->ip.mp0.minor, sc->ip.mp0.rev);
	fw = firmware_get(name);
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
	    rman_get_size(vram) < PSP_BUF_VRAM_OFF + PSP_BUF_SIZE) {
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
		/* Read back through the same window: the writes have landed. */
		(void)bus_read_4(vram, PSP_BUF_VRAM_OFF + ((st[i].size - 1) & ~3u));
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
out:
	if (vram != NULL)
		bus_release_resource(sc->dev, SYS_RES_MEMORY, rid, vram);
	firmware_put(fw, FIRMWARE_UNLOAD);
	return (err);
}
