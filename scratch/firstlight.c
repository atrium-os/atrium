/*
 * firstlight — prove the gpusim display presentation end to end.
 *
 * VRAM is GPU-only (no CPU mapping), so: fill a SYSTEM staging BO with color
 * bars (BO_WRITE), GPU-DMA-copy it into a VRAM scanout BO (IT_DMA_DATA), then
 * DISPLAY_SET_MODE that VRAM BO. The QEMU gpusim window (graphic console +
 * gpusim_gfx_update) then presents it. XRGB8888 LE == the x8r8g8b8 surface
 * byte order, so the straight-memcpy blit shows true colors.
 *
 *   cc -I/mnt/host/atrium-gpu-amd /mnt/host/scratch/firstlight.c -o /tmp/fl
 *   /tmp/fl &        # holds ~30s; screendump the gpusim window
 */
#include "atrium_gpu_amd_abi.h"

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define PM4_TYPE3 3u
#define IT_DMA_DATA 0x50u

static uint32_t type3(uint32_t opcode, uint32_t body_dwords)
{
	return (PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) | (opcode << 8);
}

static int vm_create(int fd)
{
	struct atrium_gpu_vm_create v;
	memset(&v, 0, sizeof(v));
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_CREATE, &v) != 0)
		return (-1);
	return ((int)v.out_fd);
}

/* Alloc a BO (VRAM if `vram`), bind it into `vm` at an auto VA. */
static int bo(int fd, int vm, uint64_t size, uint32_t flags, uint32_t *h, uint64_t *va)
{
	struct atrium_gpu_bo_alloc a;
	struct atrium_gpu_vm_bind b;
	memset(&a, 0, sizeof(a));
	a.size = size;
	a.flags = flags;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0)
		return (-1);
	memset(&b, 0, sizeof(b));
	b.vm_fd = vm;
	b.bo_fd = a.bo_fd;
	b.va = 0;
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_BIND, &b) != 0)
		return (-1);
	*h = a.bo_fd;
	*va = b.va;
	return (0);
}

static int bo_write(int fd, uint32_t h, const void *src, uint64_t len)
{
	struct atrium_gpu_bo_xfer x;
	memset(&x, 0, sizeof(x));
	x.bo_fd = h;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)src;
	return (ioctl(fd, ATRIUM_GPU_IOC_BO_WRITE, &x));
}

int main(void)
{
	int fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0) {
		perror("open /dev/atrium-gpu0");
		return (1);
	}
	int vm = vm_create(fd);
	if (vm < 0) {
		perror("VM_CREATE");
		return (1);
	}

	const int W = 640, H = 480;
	const uint64_t sz = (uint64_t)W * H * 4;

	/* color bars in a host buffer */
	static const uint32_t BARS[8] = {
		0x00ff0000, 0x0000ff00, 0x000000ff, 0x00ffff00,
		0x0000ffff, 0x00ff00ff, 0x00ffffff, 0x00808080,
	};
	uint32_t *px = malloc(sz);
	for (int y = 0; y < H; y++)
		for (int x = 0; x < W; x++)
			px[y * W + x] = BARS[(x * 8) / W];

	uint32_t stage_h, fb_h, ring_h;
	uint64_t stage_va, fb_va, ring_va;
	if (bo(fd, vm, sz, 0, &stage_h, &stage_va) != 0 ||              /* system staging */
	    bo(fd, vm, sz, ATRIUM_GPU_BO_VRAM, &fb_h, &fb_va) != 0 ||   /* VRAM scanout  */
	    bo(fd, vm, 4096, 0, &ring_h, &ring_va) != 0) {
		perror("bo alloc/bind");
		return (1);
	}
	if (bo_write(fd, stage_h, px, sz) != 0) {
		perror("BO_WRITE staging");
		return (1);
	}

	/* GPU DMA copy: staging -> VRAM FB. */
	uint32_t ring[7];
	ring[0] = type3(IT_DMA_DATA, 6);
	ring[1] = 0; /* mem -> mem */
	ring[2] = (uint32_t)(stage_va & 0xffffffff);
	ring[3] = (uint32_t)(stage_va >> 32);
	ring[4] = (uint32_t)(fb_va & 0xffffffff);
	ring[5] = (uint32_t)(fb_va >> 32);
	ring[6] = (uint32_t)sz;
	struct atrium_gpu_submit s;
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0) {
		perror("BO_WRITE ring");
		return (1);
	}
	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ring_h;
	s.n_dwords = 7;
	s.engine = ATRIUM_GPU_ENGINE_GFX;
	s.signal_syncobj_fd = -1;
	if (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) != 0) {
		perror("SUBMIT dma");
		return (1);
	}

	/* Scan it out. */
	struct atrium_gpu_display_setmode sm;
	memset(&sm, 0, sizeof(sm));
	sm.fb_fd = fb_h;
	if (ioctl(fd, ATRIUM_GPU_IOC_DISPLAY_SET_MODE, &sm) != 0 || sm.fault != 0) {
		printf("DISPLAY_SET_MODE fault=%u\n", sm.fault);
		return (1);
	}

	printf("first light: 640x480 color bars are the scanout; holding 30s\n");
	fflush(stdout);
	sleep(30);
	return (0);
}
