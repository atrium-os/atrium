/*
 * firstlight.c — first light on /dev/atrium-display0, the model-correct way.
 *
 * The native display path: the GPU *renders* the frame (it is the endpoint —
 * there is no host compositor on this path), the result lands in a VRAM scanout
 * buffer, and the display engine scans it out. This demo drives that end to end
 * with only primitives the in-VM test already proves:
 *
 *   1. GPU draw a recognizable frame (a 2x2 R/G/B/W texture sampled across a
 *      full-screen quad -> four colored quadrants) into a System render target.
 *   2. GPU DMA-copy that frame into a VRAM scanout BO (the display reads VRAM).
 *   3. BO_EXPORT_SCANOUT -> {vram_offset, size} (the dma-buf-style handle).
 *   4. /dev/atrium-display0: ENUM -> SET_MODE -> vsync PAGE_FLIP.
 *   5. Hold it on screen (re-flip on a cadence; keeps the BO resident) so the
 *      gpusim QEMU window shows the frame until killed.
 *
 * Build + run in the guest (needs run-vm.sh --gpusim --display):
 *   cc -I. tests/firstlight.c -o /tmp/firstlight && /tmp/firstlight
 */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "atrium_gpu_amd_abi.h"
#include "atrium_display_abi.h"

#define PM4_TYPE3		3u
#define IT_SET_SH_REG		0x76u
#define IT_DRAW_INDEX_AUTO	0x2du
#define IT_DMA_DATA		0x50u
#define SIM_DRAW_VTX_LO		0x214u

#define W	640u
#define H	480u

struct vtx { float x, y, z, u, v; uint32_t color; };

static uint32_t
type3(uint32_t opcode, uint32_t body_dwords)
{
	return ((PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) |
	    (opcode << 8));
}

static int
vm_create(int fd)
{
	struct atrium_gpu_vm_create v;

	memset(&v, 0, sizeof(v));
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_CREATE, &v) != 0)
		return (-1);
	return ((int)v.out_fd);
}

/* Allocate a BO (flags=0 System, ATRIUM_GPU_BO_VRAM for scanout) + bind it. */
static int
bo_alloc(int fd, int vm, uint64_t size, uint32_t flags, uint32_t *handle,
    uint64_t *gpu_va)
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
	*handle = a.bo_fd;
	*gpu_va = b.va;
	return (0);
}

static int
bo_write(int fd, uint32_t handle, const void *src, uint64_t len)
{
	struct atrium_gpu_bo_xfer x;

	memset(&x, 0, sizeof(x));
	x.bo_fd = handle;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)src;
	return (ioctl(fd, ATRIUM_GPU_IOC_BO_WRITE, &x));
}

static int
bo_read(int fd, uint32_t handle, void *dst, uint64_t len)
{
	struct atrium_gpu_bo_xfer x;

	memset(&x, 0, sizeof(x));
	x.bo_fd = handle;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)dst;
	return (ioctl(fd, ATRIUM_GPU_IOC_BO_READ, &x));
}

static int
submit(int fd, int vm, uint32_t ring_handle, uint32_t n_dwords)
{
	struct atrium_gpu_submit s;

	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ring_handle;
	s.n_dwords = n_dwords;
	s.engine = ATRIUM_GPU_ENGINE_GFX;
	s.signal_syncobj_fd = -1;
	return (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s));
}

/* GPU DMA-copy src_va -> dst_va (mem->mem), `bytes` long. */
static int
dma_copy(int fd, int vm, uint32_t ring_h, uint64_t src_va, uint64_t dst_va,
    uint32_t bytes)
{
	uint32_t ring[7];

	ring[0] = type3(IT_DMA_DATA, 6);
	ring[1] = 0;
	ring[2] = (uint32_t)(src_va & 0xffffffff);
	ring[3] = (uint32_t)(src_va >> 32);
	ring[4] = (uint32_t)(dst_va & 0xffffffff);
	ring[5] = (uint32_t)(dst_va >> 32);
	ring[6] = bytes;
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0)
		return (-1);
	return (submit(fd, vm, ring_h, 7));
}

/* Draw state (vtx/rt/dim/tex/filter) carried in the ring as a SET_SH_REG. */
static int
emit_draw_sh(uint32_t *r, uint64_t vtx, uint64_t rt, uint32_t w, uint32_t h,
    uint64_t tex, uint32_t tw, uint32_t th, uint32_t filter, uint32_t blend,
    uint64_t depth)
{
	r[0] = type3(IT_SET_SH_REG, 13);
	r[1] = SIM_DRAW_VTX_LO;
	r[2] = (uint32_t)(vtx & 0xffffffff);
	r[3] = (uint32_t)(vtx >> 32);
	r[4] = (uint32_t)(rt & 0xffffffff);
	r[5] = (uint32_t)(rt >> 32);
	r[6] = (w << 16) | h;
	r[7] = (uint32_t)(depth & 0xffffffff);
	r[8] = (uint32_t)(depth >> 32);
	r[9] = (uint32_t)(tex & 0xffffffff);
	r[10] = (uint32_t)(tex >> 32);
	r[11] = (tw << 16) | th;
	r[12] = blend;
	r[13] = filter;
	return (14);
}

int
main(void)
{
	const uint32_t fb_bytes = W * H * 4u;
	/* 2x2 texture: red, green / blue, white. Distinct quadrants = unmistakable
	 * first light (exact channel order is irrelevant — four regions is proof). */
	uint32_t tex[4] = { 0xffff0000u, 0xff00ff00u, 0xff0000ffu, 0xffffffffu };
	/* full-screen quad; u=(x+1)/2, v=(1-y)/2 maps screen -> texture. */
	struct vtx verts[6] = {
		{ -1, -1, 0, 0, 1, 0 }, { 1, -1, 0, 1, 1, 0 }, { 1, 1, 0, 1, 0, 0 },
		{ -1, -1, 0, 0, 1, 0 }, { 1, 1, 0, 1, 0, 0 }, { -1, 1, 0, 0, 0, 0 },
	};
	uint32_t tex_h, vtx_h, rb_h, ring_h, scan_h;
	uint64_t tex_va, vtx_va, rb_va, ring_va, scan_va;
	uint32_t ring[16];
	struct atrium_gpu_bo_export_scanout exp;
	struct atrium_display_connector conn;
	struct atrium_display_setmode sm;
	struct atrium_display_flip fl;
	struct atrium_display_status st0, st1;
	int fd, dfd, vm, n;

	setbuf(stdout, NULL);	/* unbuffered: we print then loop forever holding the frame */
	fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0) { perror("open /dev/atrium-gpu0"); return (1); }
	vm = vm_create(fd);
	if (vm < 0) { fprintf(stderr, "VM_CREATE failed\n"); return (1); }

	/* Small System inputs + readback, plus the VRAM scanout we render straight
	 * into (the GPU is the endpoint — no host compositor, no staging copy). The
	 * VM's GPU-VA window is 512 pages, so we bind only the 300-page scanout +
	 * three 1-page BOs, not a second full-screen System RT. */
	if (bo_alloc(fd, vm, 4096, 0, &tex_h, &tex_va) != 0) {
		perror("alloc tex"); return (1); }
	if (bo_alloc(fd, vm, 4096, 0, &vtx_h, &vtx_va) != 0) {
		perror("alloc vtx"); return (1); }
	if (bo_alloc(fd, vm, 4096, 0, &ring_h, &ring_va) != 0) {
		perror("alloc ring"); return (1); }
	if (bo_alloc(fd, vm, 4096, 0, &rb_h, &rb_va) != 0) {
		perror("alloc readback"); return (1); }
	if (bo_alloc(fd, vm, fb_bytes, ATRIUM_GPU_BO_VRAM, &scan_h, &scan_va) != 0) {
		perror("alloc scanout (VRAM FB)"); return (1); }
	if (bo_write(fd, tex_h, tex, sizeof(tex)) != 0 ||
	    bo_write(fd, vtx_h, verts, sizeof(verts)) != 0) {
		fprintf(stderr, "BO write failed\n");
		return (1);
	}

	/* 1. GPU render: textured (2x2, nearest) full-screen quad straight into the
	 * VRAM scanout BO. */
	n = emit_draw_sh(ring, vtx_va, scan_va, W, H, tex_va, 2, 2, 0, 0, 0);
	ring[n++] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[n++] = 6;
	if (bo_write(fd, ring_h, ring, n * 4) != 0 || submit(fd, vm, ring_h, n) != 0) {
		fprintf(stderr, "draw submit failed\n");
		return (1);
	}

	/* Verify the render reached VRAM: DMA-copy the first page out and check two
	 * top-row pixels (left = red texel, right = green texel). */
	if (dma_copy(fd, vm, ring_h, scan_va, rb_va, 4096) != 0) {
		fprintf(stderr, "scanout readback (dma_copy) failed\n");
		return (1);
	}
	{
		uint32_t row0[1024];
		uint32_t left, right;
		if (bo_read(fd, rb_h, row0, sizeof(row0)) != 0) {
			fprintf(stderr, "readback BO_READ failed\n");
			return (1);
		}
		left = row0[W / 4];		/* top-left quadrant */
		right = row0[3 * W / 4];	/* top-right quadrant */
		if (left != tex[0] || right != tex[1]) {
			fprintf(stderr, "render FAILED: top row left=%08x right=%08x "
			    "(want %08x %08x)\n", left, right, tex[0], tex[1]);
			return (1);
		}
		printf("render OK: %ux%u into VRAM, top row left=%08x(R) right=%08x(G)\n",
		    W, H, left, right);
	}

	/* 2. Export the VRAM BO as a dma-buf-style scanout handle. */
	memset(&exp, 0, sizeof(exp));
	exp.bo_fd = scan_h;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_EXPORT_SCANOUT, &exp) != 0) {
		perror("BO_EXPORT_SCANOUT");
		return (1);
	}
	printf("export OK: vram_offset=0x%llx size=%llu\n",
	    (unsigned long long)exp.vram_offset, (unsigned long long)exp.size);

	/* 4. Display: connector present, set the mode, vsync flip. */
	dfd = open("/dev/atrium-display0", O_RDWR);
	if (dfd < 0) { perror("open /dev/atrium-display0"); return (1); }
	memset(&conn, 0, sizeof(conn));
	if (ioctl(dfd, ATRIUM_DISPLAY_IOC_ENUM, &conn) != 0 || !conn.connected) {
		fprintf(stderr, "no connector (connected=%u)\n", conn.connected);
		return (1);
	}
	memset(&sm, 0, sizeof(sm));
	sm.vram_offset = exp.vram_offset;
	sm.size = exp.size;
	if (ioctl(dfd, ATRIUM_DISPLAY_IOC_SET_MODE, &sm) != 0 || sm.fault != 0) {
		fprintf(stderr, "SET_MODE fault=%u\n", sm.fault);
		return (1);
	}
	memset(&fl, 0, sizeof(fl));
	fl.vram_offset = exp.vram_offset;
	fl.size = exp.size;
	fl.vsync = 1;
	if (ioctl(dfd, ATRIUM_DISPLAY_IOC_PAGE_FLIP, &fl) != 0 || fl.fault != 0) {
		fprintf(stderr, "PAGE_FLIP fault=%u\n", fl.fault);
		return (1);
	}

	/* Confirm the display is live: vblank advances under the host timer. */
	memset(&st0, 0, sizeof(st0));
	ioctl(dfd, ATRIUM_DISPLAY_IOC_STATUS, &st0);
	usleep(120 * 1000);
	memset(&st1, 0, sizeof(st1));
	ioctl(dfd, ATRIUM_DISPLAY_IOC_STATUS, &st1);
	printf("FIRST LIGHT: 4-quadrant frame scanned out on /dev/atrium-display0 "
	    "(vblank %llu -> %llu) — look at the QEMU window\n",
	    (unsigned long long)st0.vblank_count, (unsigned long long)st1.vblank_count);

	/* Hold it: re-flip on a cadence so the frame stays latched and the scanout
	 * BO stays resident (closing fds would free the VRAM). Ctrl-C to stop. */
	for (;;) {
		ioctl(dfd, ATRIUM_DISPLAY_IOC_PAGE_FLIP, &fl);
		usleep(100 * 1000);
	}
	return (0);
}
