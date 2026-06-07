/*
 * atrium_gpu_test — userspace exercise of the /dev/atrium-gpu0 ABI.
 *
 * Reproduces the M3 (gfx fence) and M4 (compute) proofs entirely from
 * userspace, over the ioctl interface: allocate BOs, build PM4 rings, submit,
 * read results back. This is what a real user-mode driver does — and it
 * replaces the attach self-tests the kernel used to run.
 *
 * Build + run in the guest:
 *   cc -Wall -o /tmp/atrium_gpu_test tests/atrium_gpu_test.c
 *   /tmp/atrium_gpu_test
 */
#include "atrium_gpu_amd_abi.h"

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

/* PM4 type-3 header + the opcodes/fields this test emits (engine/src/pm4.rs). */
#define PM4_TYPE3		3u
#define IT_NOP			0x10u
#define IT_RELEASE_MEM		0x49u
#define IT_DISPATCH_DIRECT	0x15u
#define IT_DRAW_INDEX_AUTO	0x2du
#define KERNEL_INC		2u
#define FENCE_MAGIC		0xcafef00ddeadbeefULL

static uint32_t
type3(uint32_t opcode, uint32_t body_dwords)
{
	return ((PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) |
	    (opcode << 8));
}

static int
bo_alloc(int fd, uint64_t size, uint32_t *handle, uint64_t *gpu_va)
{
	struct atrium_gpu_bo_alloc a;

	memset(&a, 0, sizeof(a));
	a.size = size;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0)
		return (-1);
	*handle = a.handle;
	*gpu_va = a.gpu_va;
	return (0);
}

static int
bo_write(int fd, uint32_t handle, const void *src, uint64_t len)
{
	struct atrium_gpu_bo_xfer x;

	memset(&x, 0, sizeof(x));
	x.handle = handle;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)src;
	return (ioctl(fd, ATRIUM_GPU_IOC_BO_WRITE, &x));
}

static int
bo_read(int fd, uint32_t handle, void *dst, uint64_t len)
{
	struct atrium_gpu_bo_xfer x;

	memset(&x, 0, sizeof(x));
	x.handle = handle;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)dst;
	return (ioctl(fd, ATRIUM_GPU_IOC_BO_READ, &x));
}

static int
submit(int fd, uint32_t ring_handle, uint32_t n_dwords, uint32_t engine)
{
	struct atrium_gpu_submit s;

	memset(&s, 0, sizeof(s));
	s.ring_handle = ring_handle;
	s.n_dwords = n_dwords;
	s.engine = engine;
	return (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s));
}

/* M3: lay [NOP, RELEASE_MEM(fence, magic)], submit on gfx, read the fence. */
static int
test_gfx_fence(int fd)
{
	uint32_t ring_h, fence_h, ring[9];
	uint64_t ring_va, fence_va, fence = 0;

	if (bo_alloc(fd, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, 4096, &fence_h, &fence_va) != 0) {
		printf("gfx: BO alloc failed\n");
		return (1);
	}
	ring[0] = type3(IT_NOP, 1);
	ring[1] = 0;
	ring[2] = type3(IT_RELEASE_MEM, 6);
	ring[3] = (5u << 8);			/* EVENT_INDEX = end-of-pipe */
	ring[4] = (2u << 29) | (2u << 24);	/* DATA_SEL_64BIT | INT_SEL_CONFIRM */
	ring[5] = (uint32_t)(fence_va & 0xffffffff);
	ring[6] = (uint32_t)(fence_va >> 32);
	ring[7] = (uint32_t)(FENCE_MAGIC & 0xffffffff);
	ring[8] = (uint32_t)(FENCE_MAGIC >> 32);

	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    submit(fd, ring_h, 9, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, fence_h, &fence, sizeof(fence)) != 0) {
		printf("gfx: ioctl failed\n");
		return (1);
	}
	if (fence != FENCE_MAGIC) {
		printf("gfx FAILED: fence = 0x%016llx (expected 0x%016llx)\n",
		    (unsigned long long)fence, (unsigned long long)FENCE_MAGIC);
		return (1);
	}
	printf("gfx OK: fence = 0x%016llx\n", (unsigned long long)fence);
	return (0);
}

/* M4: stage input, lay DISPATCH, set compute state, submit, read results. */
static int
test_compute(int fd)
{
	uint32_t src_h, dst_h, ring_h, ring[4];
	uint32_t in[4] = { 10, 20, 30, 40 }, out[4] = { 0 };
	uint64_t src_va, dst_va, ring_va;
	struct atrium_gpu_set_compute c;
	int i;

	if (bo_alloc(fd, 4096, &src_h, &src_va) != 0 ||
	    bo_alloc(fd, 4096, &dst_h, &dst_va) != 0 ||
	    bo_alloc(fd, 4096, &ring_h, &ring_va) != 0) {
		printf("compute: BO alloc failed\n");
		return (1);
	}
	if (bo_write(fd, src_h, in, sizeof(in)) != 0) {
		printf("compute: write src failed\n");
		return (1);
	}
	ring[0] = type3(IT_DISPATCH_DIRECT, 3);
	ring[1] = 4;	/* x = element count */
	ring[2] = 1;	/* y */
	ring[3] = 1;	/* z */
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0) {
		printf("compute: write ring failed\n");
		return (1);
	}

	memset(&c, 0, sizeof(c));
	c.kernel = KERNEL_INC;
	c.src_va = src_va;
	c.dst_va = dst_va;
	if (ioctl(fd, ATRIUM_GPU_IOC_SET_COMPUTE, &c) != 0) {
		printf("compute: set_compute failed\n");
		return (1);
	}
	if (submit(fd, ring_h, 4, ATRIUM_GPU_ENGINE_COMPUTE) != 0 ||
	    bo_read(fd, dst_h, out, sizeof(out)) != 0) {
		printf("compute: submit/read failed\n");
		return (1);
	}
	for (i = 0; i < 4; i++)
		if (out[i] != in[i] + 1) {
			printf("compute FAILED: got [%u %u %u %u]\n",
			    out[0], out[1], out[2], out[3]);
			return (1);
		}
	printf("compute OK: INC [%u %u %u %u] -> [%u %u %u %u]\n",
	    in[0], in[1], in[2], in[3], out[0], out[1], out[2], out[3]);
	return (0);
}

/* Write one 24-byte vertex (NDC x,y,z + texcoord u,v + RGBA color) at v[i]. */
static void
put_vert(uint8_t *v, int i, float x, float y, float z, uint32_t color)
{
	float zero = 0.0f;

	memcpy(v + i * 24 + 0, &x, 4);
	memcpy(v + i * 24 + 4, &y, 4);
	memcpy(v + i * 24 + 8, &z, 4);
	memcpy(v + i * 24 + 12, &zero, 4);	/* u */
	memcpy(v + i * 24 + 16, &zero, 4);	/* v */
	memcpy(v + i * 24 + 20, &color, 4);
}

/* M6: render a full-screen quad (2 tris) of solid color, read back the RT. */
static int
test_draw(int fd)
{
	const uint32_t W = 16, H = 16, C = 0xff3366cc;
	uint32_t vtx_h, rt_h, ring_h, ring[2], rt[16 * 16];
	uint64_t vtx_va, rt_va, ring_va;
	struct atrium_gpu_set_draw d;
	uint8_t v[6 * 24];
	unsigned i;

	if (bo_alloc(fd, sizeof(v), &vtx_h, &vtx_va) != 0 ||
	    bo_alloc(fd, sizeof(rt), &rt_h, &rt_va) != 0 ||
	    bo_alloc(fd, sizeof(ring), &ring_h, &ring_va) != 0) {
		printf("draw: BO alloc failed\n");
		return (1);
	}
	/* Two triangles tiling NDC [-1,1]^2 -> covers every RT pixel. */
	put_vert(v, 0, -1, -1, 0, C);
	put_vert(v, 1,  1, -1, 0, C);
	put_vert(v, 2,  1,  1, 0, C);
	put_vert(v, 3, -1, -1, 0, C);
	put_vert(v, 4,  1,  1, 0, C);
	put_vert(v, 5, -1,  1, 0, C);
	ring[0] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[1] = 6;	/* vertex count */

	memset(&d, 0, sizeof(d));
	d.vtx_va = vtx_va;
	d.rt_va = rt_va;
	d.width = W;
	d.height = H;
	if (bo_write(fd, vtx_h, v, sizeof(v)) != 0 ||
	    bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    ioctl(fd, ATRIUM_GPU_IOC_SET_DRAW, &d) != 0 ||
	    submit(fd, ring_h, 2, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, rt_h, rt, sizeof(rt)) != 0) {
		printf("draw: ioctl failed\n");
		return (1);
	}
	for (i = 0; i < W * H; i++)
		if (rt[i] != C) {
			printf("draw FAILED: pixel %u = 0x%08x (expected 0x%08x)\n",
			    i, rt[i], C);
			return (1);
		}
	printf("draw OK: %ux%u RT all = 0x%08x (full-screen quad)\n", W, H, C);
	return (0);
}

/* M7: submit a fence-with-IRQ and confirm the ISR serviced a new interrupt. */
static int
test_irq(int fd)
{
	struct atrium_gpu_irqs q0, q1;
	uint32_t ring_h, fence_h, ring[9];
	uint64_t ring_va, fence_va;
	int i;

	if (ioctl(fd, ATRIUM_GPU_IOC_GET_IRQS, &q0) != 0) {
		printf("irq: GET_IRQS failed\n");
		return (1);
	}
	if (!q0.msix_enabled) {
		printf("irq: MSI-X unavailable — poll mode (skipped)\n");
		return (0);	/* not a failure: the device still works */
	}
	if (bo_alloc(fd, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, 4096, &fence_h, &fence_va) != 0) {
		printf("irq: BO alloc failed\n");
		return (1);
	}
	ring[0] = type3(IT_NOP, 1);
	ring[1] = 0;
	ring[2] = type3(IT_RELEASE_MEM, 6);
	ring[3] = (5u << 8);
	ring[4] = (2u << 29) | (2u << 24);	/* DATA_SEL_64BIT | INT_SEL_CONFIRM */
	ring[5] = (uint32_t)(fence_va & 0xffffffff);
	ring[6] = (uint32_t)(fence_va >> 32);
	ring[7] = (uint32_t)(FENCE_MAGIC & 0xffffffff);
	ring[8] = (uint32_t)(FENCE_MAGIC >> 32);
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    submit(fd, ring_h, 9, ATRIUM_GPU_ENGINE_GFX) != 0) {
		printf("irq: submit failed\n");
		return (1);
	}
	/* The ISR fires asynchronously after the doorbell; poll briefly. */
	for (i = 0; i < 100; i++) {
		if (ioctl(fd, ATRIUM_GPU_IOC_GET_IRQS, &q1) == 0 &&
		    q1.count > q0.count)
			break;
		usleep(1000);
	}
	if (q1.count <= q0.count) {
		printf("irq FAILED: MSI-X enabled but no delivery (count=%llu)\n",
		    (unsigned long long)q1.count);
		return (1);
	}
	printf("irq OK: MSI-X delivered (count %llu -> %llu)\n",
	    (unsigned long long)q0.count, (unsigned long long)q1.count);
	return (0);
}

/* M8: blocking fence-wait — a reached fence returns 0; an unreached one times out. */
static int
test_wait(int fd)
{
	struct atrium_gpu_wait_fence w;
	uint32_t ring_h, fence_h, ring[9];
	uint64_t ring_va, fence_va;

	if (bo_alloc(fd, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, 4096, &fence_h, &fence_va) != 0) {
		printf("wait: BO alloc failed\n");
		return (1);
	}
	ring[0] = type3(IT_NOP, 1);
	ring[1] = 0;
	ring[2] = type3(IT_RELEASE_MEM, 6);
	ring[3] = (5u << 8);
	ring[4] = (2u << 29) | (2u << 24);
	ring[5] = (uint32_t)(fence_va & 0xffffffff);
	ring[6] = (uint32_t)(fence_va >> 32);
	ring[7] = (uint32_t)(FENCE_MAGIC & 0xffffffff);
	ring[8] = (uint32_t)(FENCE_MAGIC >> 32);
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    submit(fd, ring_h, 9, ATRIUM_GPU_ENGINE_GFX) != 0) {
		printf("wait: submit failed\n");
		return (1);
	}

	/* The fence is written -> blocking wait should return success. */
	memset(&w, 0, sizeof(w));
	w.value = FENCE_MAGIC;
	w.handle = fence_h;
	w.offset = 0;
	w.timeout_ms = 1000;
	if (ioctl(fd, ATRIUM_GPU_IOC_WAIT_FENCE, &w) != 0) {
		printf("wait FAILED: reached fence did not return success\n");
		return (1);
	}

	/* A value never written -> the wait must sleep and time out. */
	w.value = 0xdeadbeef12345678ULL;
	w.timeout_ms = 50;
	if (ioctl(fd, ATRIUM_GPU_IOC_WAIT_FENCE, &w) == 0 || errno != EWOULDBLOCK) {
		printf("wait FAILED: unreached fence did not time out (errno=%d)\n",
		    errno);
		return (1);
	}
	printf("wait OK: reached fence returns, unreached fence times out\n");
	return (0);
}

int
main(void)
{
	int fd, rc;

	fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0) {
		perror("open /dev/atrium-gpu0");
		return (1);
	}
	rc = test_gfx_fence(fd);
	rc |= test_compute(fd);
	rc |= test_draw(fd);
	rc |= test_irq(fd);
	rc |= test_wait(fd);
	close(fd);
	printf(rc == 0 ? "ALL OK\n" : "FAILURES\n");
	return (rc);
}
