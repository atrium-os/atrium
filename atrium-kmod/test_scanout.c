/*
 * test_scanout.c — D0 step 3 smoke: enumerate connectors, set a mode,
 * fill a scanout BO with a test pattern, page-flip. Verifies the full
 * Atrium display path against virtio-gpu end-to-end.
 *
 * To actually SEE the result, boot QEMU with a display front-end (the
 * default `-nographic` swallows scanout output). Add e.g.
 *   -display cocoa
 * to scripts/run-vm.sh's qemu invocation. The kernel's IOC chain works
 * regardless; the test reports success based on ioctl return codes.
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/endian.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

#include "atrium_gpu.h"

int
main(void)
{
	int gpu = open("/dev/atrium-gpu0",     O_RDWR);
	int dpy = open("/dev/atrium-display0", O_RDWR);
	if (gpu < 0 || dpy < 0) {
		fprintf(stderr, "open: %s\n", strerror(errno));
		return (1);
	}

	struct atrium_display_bind_gpu bind = { .gpu_fd = gpu };
	if (ioctl(dpy, ATRIUM_DISPLAY_IOC_BIND_GPU, &bind) != 0) {
		fprintf(stderr, "BIND_GPU: %s\n", strerror(errno));
		return (1);
	}
	printf("BIND_GPU ok\n");

	/* Two-call pattern: count, then list. */
	struct atrium_display_enum en = { .count_in = 0 };
	if (ioctl(dpy, ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS, &en) != 0) {
		fprintf(stderr, "ENUM_CONNECTORS: %s\n", strerror(errno));
		return (1);
	}
	printf("ENUM_CONNECTORS: %u connectors\n", en.count_out);
	if (en.count_out == 0) return (1);

	struct atrium_display_connector *cs =
	    calloc(en.count_out, sizeof(*cs));
	en.count_in = en.count_out;
	en.connectors_ptr = (uint64_t)(uintptr_t)cs;
	if (ioctl(dpy, ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS, &en) != 0) {
		fprintf(stderr, "ENUM (2): %s\n", strerror(errno));
		return (1);
	}
	printf("  connector 0: id=%u type=%u flags=0x%x\n",
	    cs[0].id, cs[0].type, cs[0].flags);

	struct atrium_display_mode mode;
	struct atrium_display_modes_query mq = {
		.connector_id = cs[0].id, .count_in = 1,
		.modes_ptr = (uint64_t)(uintptr_t)&mode,
	};
	if (ioctl(dpy, ATRIUM_DISPLAY_IOC_MODES, &mq) != 0) {
		fprintf(stderr, "MODES: %s\n", strerror(errno));
		return (1);
	}
	printf("  mode: %ux%u @ %u mHz\n", mode.width, mode.height,
	    mode.refresh_mhz);

	uint32_t W = mode.width, H = mode.height;
	uint64_t bytes = (uint64_t)W * H * 4;

	/* Allocate scanout BO. */
	struct atrium_gpu_alloc al = {
		.size = bytes,
		.flags = ATRIUM_GPU_BO_GPU_VISIBLE | ATRIUM_GPU_BO_CPU_VISIBLE |
		         ATRIUM_GPU_BO_COHERENT    | ATRIUM_GPU_BO_SCANOUT,
	};
	if (ioctl(gpu, ATRIUM_GPU_IOC_ALLOC, &al) != 0) {
		fprintf(stderr, "ALLOC scanout: %s\n", strerror(errno));
		return (1);
	}
	printf("ALLOC scanout: handle=%u size=%lu\n", al.handle,
	    (unsigned long)al.size);

	uint32_t *fb = mmap(NULL, al.size, PROT_READ | PROT_WRITE,
	    MAP_SHARED, gpu, al.mmap_offset);
	if (fb == MAP_FAILED) {
		fprintf(stderr, "mmap: %s\n", strerror(errno));
		return (1);
	}

	/* Solid color #2266aa (BGRA = 0xaa 0x66 0x22 0xff). */
	uint32_t color = 0xff2266aa;
	for (uint32_t i = 0; i < W * H; i++) fb[i] = color;

	struct atrium_display_set_mode sm = {
		.connector_id = cs[0].id,
		.scanout_handle = al.handle,
		.mode = mode,
	};
	if (ioctl(dpy, ATRIUM_DISPLAY_IOC_SET_MODE, &sm) != 0) {
		fprintf(stderr, "SET_MODE: %s\n", strerror(errno));
		return (1);
	}
	printf("SET_MODE ok\n");

	struct atrium_display_page_flip pf = {
		.connector_id = cs[0].id,
		.scanout_handle = al.handle,
	};
	if (ioctl(dpy, ATRIUM_DISPLAY_IOC_PAGE_FLIP, &pf) != 0) {
		fprintf(stderr, "PAGE_FLIP: %s\n", strerror(errno));
		return (1);
	}
	printf("PAGE_FLIP ok\n");

	/* Animate: gradient swap, flip again. */
	for (uint32_t y = 0; y < H; y++)
		for (uint32_t x = 0; x < W; x++)
			fb[y * W + x] = 0xff000000 |
			    (((x * 255) / W) << 16) |
			    (((y * 255) / H) << 8);

	if (ioctl(dpy, ATRIUM_DISPLAY_IOC_PAGE_FLIP, &pf) != 0) {
		fprintf(stderr, "PAGE_FLIP 2: %s\n", strerror(errno));
		return (1);
	}
	printf("PAGE_FLIP (gradient) ok\n");

	return (0);
}
