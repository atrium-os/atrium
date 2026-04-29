/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * atrium-bootfb — userspace-mappable cdev for the EFI GOP framebuffer.
 *
 * vt(4)/efifb already maps and writes to this same memory; we coexist
 * by mapping the same physical region into userspace via this cdev's
 * d_mmap callback. The splash binary's writes immediately overwrite
 * vt's text output — that's the desired behavior: once we paint, the
 * console text disappears under the splash artwork.
 *
 * No exclusive ownership of the device is taken. When the native GPU
 * driver (e.g. atrium-virtio-gpu) does its own SET_SCANOUT, the EFI
 * GOP framebuffer is no longer the active scanout source — writes to
 * this cdev silently no-op (visible memory still updates, just not
 * presented). The splash binary handles that transition by polling for
 * `/dev/atrium-display0` to appear and exiting cleanly.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/conf.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/module.h>
#include <sys/mutex.h>
#include <sys/linker.h>
#include <sys/proc.h>
#include <sys/sx.h>
#include <sys/uio.h>

#include <vm/vm.h>
#include <vm/pmap.h>

#include <machine/metadata.h>

#include "atrium_bootfb.h"

#define ATRIUM_BOOTFB_NAME	"atrium-bootfb0"

struct atrium_bootfb_softc {
	struct cdev		*cdev;
	struct atrium_bootfb_info info;
	uint64_t		paddr;	/* fb physical base */
	int			attached;
};

static struct atrium_bootfb_softc atrium_bootfb_sc;

static d_open_t  atrium_bootfb_open;
static d_close_t atrium_bootfb_close;
static d_ioctl_t atrium_bootfb_ioctl;
static d_mmap_t  atrium_bootfb_mmap;

static struct cdevsw atrium_bootfb_cdevsw = {
	.d_version = D_VERSION,
	.d_open    = atrium_bootfb_open,
	.d_close   = atrium_bootfb_close,
	.d_ioctl   = atrium_bootfb_ioctl,
	.d_mmap    = atrium_bootfb_mmap,
	.d_name    = "atrium-bootfb",
};

static int
atrium_bootfb_open(struct cdev *dev, int oflags, int devtype, struct thread *td)
{
	(void)dev; (void)oflags; (void)devtype; (void)td;
	return (0);
}

static int
atrium_bootfb_close(struct cdev *dev, int fflag, int devtype, struct thread *td)
{
	(void)dev; (void)fflag; (void)devtype; (void)td;
	return (0);
}

static int
atrium_bootfb_ioctl(struct cdev *dev, u_long cmd, caddr_t data, int fflag,
                    struct thread *td)
{
	struct atrium_bootfb_softc *sc = &atrium_bootfb_sc;
	(void)dev; (void)fflag; (void)td;

	switch (cmd) {
	case ATRIUM_BOOTFB_IOC_GET_INFO:
		memcpy(data, &sc->info, sizeof(sc->info));
		return (0);
	default:
		return (ENOTTY);
	}
}

/*
 * Map a page of the framebuffer to userspace. The cdev mmap path
 * calls us once per page; we just translate offset → physical address
 * within the framebuffer's contiguous range. Memattr WC gives writes
 * a fast path on most architectures (sequential writes get combined
 * before hitting the GOP).
 */
static int
atrium_bootfb_mmap(struct cdev *dev, vm_ooffset_t offset, vm_paddr_t *paddr,
                   int nprot, vm_memattr_t *memattr)
{
	struct atrium_bootfb_softc *sc = &atrium_bootfb_sc;
	(void)dev; (void)nprot;

	if (!sc->attached)
		return (ENXIO);
	if (offset >= sc->info.size)
		return (EINVAL);

	*paddr = (vm_paddr_t)(sc->paddr + offset);
	*memattr = VM_MEMATTR_WRITE_COMBINING;
	return (0);
}

/*
 * Decode `efi_fb`'s mask fields into a friendlier format code. EFI
 * GOP gives us per-channel byte masks; we recognize the two common
 * 32-bit-pixel layouts and report ATRIUM_BOOTFB_FORMAT_UNKNOWN
 * otherwise (the splash app falls back to memcpy + per-pixel mask).
 */
static uint32_t
classify_format(const struct efi_fb *fb)
{
	/* BGRA8888: B in low byte, A in high byte. */
	if (fb->fb_mask_blue  == 0x000000FFu &&
	    fb->fb_mask_green == 0x0000FF00u &&
	    fb->fb_mask_red   == 0x00FF0000u)
		return (ATRIUM_BOOTFB_FORMAT_BGRA8);
	/* RGBA8888: R in low byte. */
	if (fb->fb_mask_red   == 0x000000FFu &&
	    fb->fb_mask_green == 0x0000FF00u &&
	    fb->fb_mask_blue  == 0x00FF0000u)
		return (ATRIUM_BOOTFB_FORMAT_RGBA8);
	return (ATRIUM_BOOTFB_FORMAT_UNKNOWN);
}

static int
atrium_bootfb_attach(void)
{
	struct atrium_bootfb_softc *sc = &atrium_bootfb_sc;
	struct efi_fb *efb;
	struct make_dev_args args;
	int err;

	bzero(sc, sizeof(*sc));

	/*
	 * Pull the EFI framebuffer descriptor out of the kernel's
	 * preloaded boot metadata. `preload_kmdp` is a kernel-side
	 * cached pointer; `MODINFO_METADATA | MODINFOMD_EFI_FB` is the
	 * key the bootloader uses (matches `vt_efifb`'s lookup).
	 */
	if (preload_kmdp == NULL) {
		printf("atrium-bootfb: preload_kmdp is NULL\n");
		return (ENXIO);
	}
	efb = (struct efi_fb *)preload_search_info(preload_kmdp,
	    MODINFO_METADATA | MODINFOMD_EFI_FB);
	if (efb == NULL) {
		printf("atrium-bootfb: no EFI framebuffer metadata "
		       "(non-EFI boot, or firmware didn't expose GOP)\n");
		return (ENXIO);
	}

	sc->paddr        = efb->fb_addr;
	sc->info.size    = efb->fb_size;
	sc->info.width   = efb->fb_width;
	sc->info.height  = efb->fb_height;
	sc->info.stride  = efb->fb_stride * 4;	/* efi_fb stride is in pixels */
	sc->info.bpp     = 32;
	sc->info.mask_red      = efb->fb_mask_red;
	sc->info.mask_green    = efb->fb_mask_green;
	sc->info.mask_blue     = efb->fb_mask_blue;
	sc->info.mask_reserved = efb->fb_mask_reserved;
	sc->info.format  = classify_format(efb);

	if (sc->paddr == 0 || sc->info.width == 0 || sc->info.height == 0) {
		printf("atrium-bootfb: efi_fb has zeroed fields, refusing\n");
		return (ENXIO);
	}

	make_dev_args_init(&args);
	args.mda_devsw = &atrium_bootfb_cdevsw;
	args.mda_uid   = UID_ROOT;
	args.mda_gid   = GID_OPERATOR;
	args.mda_mode  = 0660;
	err = make_dev_s(&args, &sc->cdev, ATRIUM_BOOTFB_NAME);
	if (err != 0) {
		printf("atrium-bootfb: make_dev_s failed: %d\n", err);
		return (err);
	}

	sc->attached = 1;
	printf("atrium-bootfb: %ux%u stride=%u format=%u "
	       "(masks R=%08x G=%08x B=%08x A=%08x) at phys 0x%016jx, %ju bytes\n",
	    sc->info.width, sc->info.height, sc->info.stride, sc->info.format,
	    sc->info.mask_red, sc->info.mask_green, sc->info.mask_blue,
	    sc->info.mask_reserved,
	    (uintmax_t)sc->paddr, (uintmax_t)sc->info.size);
	return (0);
}

static int
atrium_bootfb_detach(void)
{
	struct atrium_bootfb_softc *sc = &atrium_bootfb_sc;

	if (sc->cdev != NULL) {
		destroy_dev(sc->cdev);
		sc->cdev = NULL;
	}
	sc->attached = 0;
	return (0);
}

static int
atrium_bootfb_modevent(module_t mod, int what, void *arg)
{
	(void)mod; (void)arg;
	switch (what) {
	case MOD_LOAD:   return (atrium_bootfb_attach());
	case MOD_UNLOAD: return (atrium_bootfb_detach());
	case MOD_QUIESCE: return (0);
	default:         return (EOPNOTSUPP);
	}
}

static moduledata_t atrium_bootfb_mod = {
	"atrium_bootfb",
	atrium_bootfb_modevent,
	NULL
};

DECLARE_MODULE(atrium_bootfb, atrium_bootfb_mod, SI_SUB_DRIVERS, SI_ORDER_ANY);
MODULE_VERSION(atrium_bootfb, 1);
