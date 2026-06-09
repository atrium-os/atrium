/*
 * atrium_gpu_test — userspace exercise of the /dev/atrium-gpu0 ABI.
 *
 * Drives the ioctl interface the way a user-mode driver would: create an
 * address space (VM), allocate BOs in it, build PM4 rings, submit under the
 * VM, wait via a kqueue-able syncobj, read results back. The final test proves
 * per-VM isolation: the same GPU-VA in two VMs resolves to different memory.
 *
 * Build + run in the guest:
 *   cc -Wall -o /tmp/atrium_gpu_test tests/atrium_gpu_test.c
 *   /tmp/atrium_gpu_test
 */
#include "atrium_gpu_amd_abi.h"

#include <errno.h>
#include <fcntl.h>
#include <sys/event.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define PM4_TYPE3		3u
#define IT_NOP			0x10u
#define IT_RELEASE_MEM		0x49u
#define IT_DISPATCH_DIRECT	0x15u
#define IT_DRAW_INDEX_AUTO	0x2du
#define IT_WRITE_DATA		0x37u
#define IT_WAIT_REG_MEM		0x3cu
#define IT_DMA_DATA		0x50u	/* CP DMA copy (memory <-> memory) */
#define IT_SET_SH_REG		0x76u	/* write a run of state regs from the ring */
#define WAIT_FN_GE		5u	/* WAIT_REG_MEM FUNCTION: value >= reference */
#define SIM_COMPUTE_KERNEL	0x200u	/* SIM-aperture offset of COMPUTE_KERNEL */
#define SIM_DRAW_VTX_LO		0x214u	/* SIM-aperture offset of the 12-reg draw block */

static uint32_t type3(uint32_t opcode, uint32_t body_dwords);	/* defined below */

/* Vertex the rasterizer consumes: NDC position + texcoord + RGBA color = 24B. */
struct vtx {
	float		x, y, z, u, v;
	uint32_t	color;
};

/* Mirror render.rs blend_over: src-over, out.rgb = src.rgb*sa + dst.rgb*(1-sa). */
static uint32_t
blend_over(uint32_t src, uint32_t dst)
{
	float sa = ((src >> 24) & 0xff) / 255.0f;
	float da = ((dst >> 24) & 0xff) / 255.0f;
	uint32_t r, g, b, a;
#define CH(s) ((uint32_t)(((src >> (s)) & 0xff) * sa + \
	    ((dst >> (s)) & 0xff) * (1.0f - sa) + 0.5f))
	r = CH(0); g = CH(8); b = CH(16);
#undef CH
	a = (uint32_t)((sa + da * (1.0f - sa)) * 255.0f + 0.5f);
	return r | (g << 8) | (b << 16) | (a << 24);
}

/*
 * Opaque-blob submit (ABI-v2): the compute/draw state travels in the ring as a
 * SET_SH_REG packet the CP applies — the kernel never touches the compute/draw
 * state registers. These emit such a packet into `r` and return its dword count.
 * The register blocks are consecutive in the SIM aperture, so one packet each.
 */
static int
emit_compute_sh(uint32_t *r, uint32_t kernel, uint64_t src, uint64_t dst)
{
	r[0] = type3(IT_SET_SH_REG, 6);		/* 1 offset + 5 values */
	r[1] = SIM_COMPUTE_KERNEL;
	r[2] = kernel;
	r[3] = (uint32_t)(src & 0xffffffff);
	r[4] = (uint32_t)(src >> 32);
	r[5] = (uint32_t)(dst & 0xffffffff);
	r[6] = (uint32_t)(dst >> 32);
	return (7);
}

static int
emit_draw_sh(uint32_t *r, uint64_t vtx, uint64_t rt, uint32_t w, uint32_t h,
    uint64_t tex, uint32_t tw, uint32_t th, uint32_t filter, uint32_t blend,
    uint64_t depth)
{
	r[0] = type3(IT_SET_SH_REG, 13);	/* 1 offset + 12 values */
	r[1] = SIM_DRAW_VTX_LO;
	r[2] = (uint32_t)(vtx & 0xffffffff);
	r[3] = (uint32_t)(vtx >> 32);
	r[4] = (uint32_t)(rt & 0xffffffff);
	r[5] = (uint32_t)(rt >> 32);
	r[6] = (w << 16) | h;			/* RT_DIM */
	r[7] = (uint32_t)(depth & 0xffffffff);
	r[8] = (uint32_t)(depth >> 32);
	r[9] = (uint32_t)(tex & 0xffffffff);
	r[10] = (uint32_t)(tex >> 32);
	r[11] = (tw << 16) | th;		/* TEX_DIM */
	r[12] = blend;
	r[13] = filter;
	return (14);
}

/* A full-screen quad (2 tris) at NDC depth z, solid color, UV spanning [0,1]. */
static void
fill_quad(struct vtx *q, float z, uint32_t color)
{
	static const float pos[6][4] = {	/* x, y, u, v */
		{ -1, -1, 0, 1 }, { 1, -1, 1, 1 }, { 1, 1, 1, 0 },
		{ -1, -1, 0, 1 }, { 1, 1, 1, 0 }, { -1, 1, 0, 0 },
	};
	int i;

	for (i = 0; i < 6; i++) {
		q[i].x = pos[i][0];
		q[i].y = pos[i][1];
		q[i].z = z;
		q[i].u = pos[i][2];
		q[i].v = pos[i][3];
		q[i].color = color;
	}
}
#define KERNEL_INC		2u
#define FENCE_MAGIC		0xcafef00ddeadbeefULL

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

/* Allocate a BO and bind it into `vm` at an auto VA — the common path. */
static int
bo_alloc(int fd, int vm, uint64_t size, uint32_t *handle, uint64_t *gpu_va)
{
	struct atrium_gpu_bo_alloc a;
	struct atrium_gpu_vm_bind b;

	memset(&a, 0, sizeof(a));
	a.size = size;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0)
		return (-1);
	memset(&b, 0, sizeof(b));
	b.vm_fd = vm;
	b.bo_fd = a.bo_fd;
	b.va = 0;	/* let the kernel pick */
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
submit(int fd, int vm, uint32_t ring_handle, uint32_t n_dwords, uint32_t engine)
{
	struct atrium_gpu_submit s;

	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ring_handle;
	s.n_dwords = n_dwords;
	s.engine = engine;
	s.signal_syncobj_fd = -1;	/* no completion syncobj */
	return (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s));
}

/* M9a: a BO is a file descriptor — write/read, then close reclaims it. */
static int
test_bo_fd(int fd, int vm)
{
	uint32_t h;
	uint64_t va;
	uint32_t data = 0xa5a5a5a5, back = 0;

	if (bo_alloc(fd, vm, 4096, &h, &va) != 0) {
		printf("bo_fd: alloc failed\n");
		return (1);
	}
	if (bo_write(fd, h, &data, sizeof(data)) != 0 ||
	    bo_read(fd, h, &back, sizeof(back)) != 0 || back != data) {
		printf("bo_fd FAILED: write/read round-trip (got 0x%08x)\n", back);
		return (1);
	}
	if (close((int)h) != 0) {
		printf("bo_fd FAILED: close() errored\n");
		return (1);
	}
	if (bo_write(fd, h, &data, sizeof(data)) == 0) {
		printf("bo_fd FAILED: closed fd still usable\n");
		return (1);
	}
	printf("bo_fd OK: BO is an fd (rw at va 0x%llx, close reclaims, EBADF "
	    "after)\n", (unsigned long long)va);
	return (0);
}

/* M3: lay [NOP, RELEASE_MEM(fence, magic)], submit on gfx, read the fence. */
static int
test_gfx_fence(int fd, int vm)
{
	uint32_t ring_h, fence_h, ring[9];
	uint64_t ring_va, fence_va, fence = 0;

	if (bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &fence_h, &fence_va) != 0) {
		printf("gfx: BO alloc failed\n");
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
	    submit(fd, vm, ring_h, 9, ATRIUM_GPU_ENGINE_GFX) != 0 ||
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
test_compute(int fd, int vm)
{
	uint32_t src_h, dst_h, ring_h, ring[11];
	uint32_t in[4] = { 10, 20, 30, 40 }, out[4] = { 0 };
	uint64_t src_va, dst_va, ring_va;
	int i, n;

	if (bo_alloc(fd, vm, 4096, &src_h, &src_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &dst_h, &dst_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("compute: BO alloc failed\n");
		return (1);
	}
	if (bo_write(fd, src_h, in, sizeof(in)) != 0) {
		printf("compute: write src failed\n");
		return (1);
	}
	/* Compute state rides in the ring (SET_SH_REG), then DISPATCH reads it. */
	n = emit_compute_sh(ring, KERNEL_INC, src_va, dst_va);
	ring[n++] = type3(IT_DISPATCH_DIRECT, 3);
	ring[n++] = 4;
	ring[n++] = 1;
	ring[n++] = 1;
	if (bo_write(fd, ring_h, ring, n * 4) != 0) {
		printf("compute: write ring failed\n");
		return (1);
	}
	if (submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_COMPUTE) != 0 ||
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

static void
put_vert(uint8_t *v, int i, float x, float y, float z, uint32_t color)
{
	float zero = 0.0f;

	memcpy(v + i * 24 + 0, &x, 4);
	memcpy(v + i * 24 + 4, &y, 4);
	memcpy(v + i * 24 + 8, &z, 4);
	memcpy(v + i * 24 + 12, &zero, 4);
	memcpy(v + i * 24 + 16, &zero, 4);
	memcpy(v + i * 24 + 20, &color, 4);
}

/* M6: render a full-screen quad (2 tris) of solid color, read back the RT. */
static int
test_draw(int fd, int vm)
{
	const uint32_t W = 16, H = 16, C = 0xff3366cc;
	uint32_t vtx_h, rt_h, ring_h, ring[16], rt[16 * 16];
	uint64_t vtx_va, rt_va, ring_va;
	uint8_t v[6 * 24];
	unsigned i;
	int n;

	if (bo_alloc(fd, vm, sizeof(v), &vtx_h, &vtx_va) != 0 ||
	    bo_alloc(fd, vm, sizeof(rt), &rt_h, &rt_va) != 0 ||
	    bo_alloc(fd, vm, sizeof(ring), &ring_h, &ring_va) != 0) {
		printf("draw: BO alloc failed\n");
		return (1);
	}
	put_vert(v, 0, -1, -1, 0, C);
	put_vert(v, 1,  1, -1, 0, C);
	put_vert(v, 2,  1,  1, 0, C);
	put_vert(v, 3, -1, -1, 0, C);
	put_vert(v, 4,  1,  1, 0, C);
	put_vert(v, 5, -1,  1, 0, C);
	/* Draw state rides in the ring (SET_SH_REG); no SET_DRAW ioctl. */
	n = emit_draw_sh(ring, vtx_va, rt_va, W, H, 0, 0, 0, 0, 0, 0);
	ring[n++] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[n++] = 6;
	if (bo_write(fd, vtx_h, v, sizeof(v)) != 0 ||
	    bo_write(fd, ring_h, ring, n * 4) != 0 ||
	    submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, rt_h, rt, sizeof(rt)) != 0) {
		printf("draw: submit failed\n");
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
test_irq(int fd, int vm)
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
		return (0);
	}
	if (bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &fence_h, &fence_va) != 0) {
		printf("irq: BO alloc failed\n");
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
	    submit(fd, vm, ring_h, 9, ATRIUM_GPU_ENGINE_GFX) != 0) {
		printf("irq: submit failed\n");
		return (1);
	}
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

/* M9b: a submission signals a timeline syncobj; wait via kqueue + blocking. */
static int
test_syncobj(int fd, int vm)
{
	struct atrium_gpu_syncobj_create cr;
	struct atrium_gpu_syncobj_op q;
	struct atrium_gpu_syncobj_wait wt;
	struct atrium_gpu_submit s;
	struct timespec ts = { 2, 0 };
	struct kevent kev;
	uint32_t ring_h, fence_h, ring[9];
	uint64_t ring_va, fence_va;
	int so_fd, kq, n;

	memset(&cr, 0, sizeof(cr));
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_CREATE, &cr) != 0) {
		printf("syncobj: create failed\n");
		return (1);
	}
	so_fd = (int)cr.out_fd;

	kq = kqueue();
	EV_SET(&kev, so_fd, EVFILT_READ, EV_ADD, 0, 1 /* threshold */, NULL);
	if (kq < 0 || kevent(kq, &kev, 1, NULL, 0, NULL) != 0) {
		printf("syncobj: kqueue register failed\n");
		return (1);
	}

	if (bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &fence_h, &fence_va) != 0) {
		printf("syncobj: BO alloc failed\n");
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
	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ring_h;
	s.n_dwords = 9;
	s.engine = ATRIUM_GPU_ENGINE_GFX;
	s.signal_syncobj_fd = so_fd;
	s.signal_value = 1;
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) != 0) {
		printf("syncobj: submit failed\n");
		return (1);
	}

	n = kevent(kq, NULL, 0, &kev, 1, &ts);
	if (n != 1 || (int)kev.ident != so_fd) {
		printf("syncobj FAILED: kqueue did not fire (n=%d)\n", n);
		return (1);
	}

	memset(&wt, 0, sizeof(wt));
	wt.syncobj_fd = so_fd;
	wt.value = 1;
	wt.timeout_ms = 1000;
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_WAIT, &wt) != 0) {
		printf("syncobj FAILED: SYNCOBJ_WAIT did not complete\n");
		return (1);
	}
	memset(&q, 0, sizeof(q));
	q.syncobj_fd = so_fd;
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_QUERY, &q) != 0 || q.value < 1) {
		printf("syncobj FAILED: query value %llu\n",
		    (unsigned long long)q.value);
		return (1);
	}

	close(kq);
	close(so_fd);
	printf("syncobj OK: submit signalled, kqueue EVFILT_READ fired, "
	    "value=%llu\n", (unsigned long long)q.value);
	return (0);
}

/* Run an INC compute over a one-element src in `vm`; returns dst[0] (or -1). */
static long
inc_one(int fd, int vm, uint32_t input)
{
	uint32_t src_h, dst_h, ring_h, ring[11], out = 0;
	uint64_t src_va, dst_va, ring_va;
	int n;

	if (bo_alloc(fd, vm, 4096, &src_h, &src_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &dst_h, &dst_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0)
		return (-1);
	if (bo_write(fd, src_h, &input, sizeof(input)) != 0)
		return (-1);
	/* Same src VA for both VMs — that's the point; state rides in the ring. */
	n = emit_compute_sh(ring, KERNEL_INC, src_va, dst_va);
	ring[n++] = type3(IT_DISPATCH_DIRECT, 3);
	ring[n++] = 1;
	ring[n++] = 1;
	ring[n++] = 1;
	if (bo_write(fd, ring_h, ring, n * 4) != 0)
		return (-1);
	if (submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_COMPUTE) != 0)
		return (-1);
	if (bo_read(fd, dst_h, &out, sizeof(out)) != 0)
		return (-1);
	return ((long)out);
}

/*
 * M9c: per-VM isolation. Two VMs each place their first BO at the same GPU-VA
 * (BO_VA_BASE); a compute under each reads that VA and must see only its own
 * VM's data — proving the VA resolves through different page tables per VMID.
 */
static int
test_isolation(int fd)
{
	long out_a, out_b;
	int vma, vmb;

	vma = vm_create(fd);
	vmb = vm_create(fd);
	if (vma < 0 || vmb < 0) {
		printf("isolation: VM_CREATE failed\n");
		return (1);
	}
	/* Each VM's first BO (the src) lands at the same VA, BO_VA_BASE. */
	out_a = inc_one(fd, vma, 100);
	out_b = inc_one(fd, vmb, 200);
	if (out_a != 101 || out_b != 201) {
		printf("isolation FAILED: vmA=%ld (want 101), vmB=%ld (want 201)\n",
		    out_a, out_b);
		return (1);
	}
	close(vma);
	close(vmb);
	printf("isolation OK: same VA -> 100->101 in vmA, 200->201 in vmB "
	    "(per-VMID page tables)\n");
	return (0);
}

/*
 * M9d: BO_CREATE allocates memory not in any address space; VM_BIND maps it
 * into a VM at a chosen VA. Proves the two are separate (v2 principle 4) and
 * that a BO can only be bound once.
 */
static int
test_vm_bind(int fd)
{
	struct atrium_gpu_bo_alloc a;
	struct atrium_gpu_vm_bind b;
	uint32_t data = 0x5a5a5a5a, back = 0;
	int vm;

	vm = vm_create(fd);
	if (vm < 0) {
		printf("vm_bind: VM_CREATE failed\n");
		return (1);
	}
	memset(&a, 0, sizeof(a));
	a.size = 4096;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0) {
		printf("vm_bind: BO_CREATE failed\n");
		return (1);
	}
	/* Bind it at an explicit VA (fresh VM, so 0x10000000 is free). */
	memset(&b, 0, sizeof(b));
	b.vm_fd = vm;
	b.bo_fd = a.bo_fd;
	b.va = 0x10000000;
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_BIND, &b) != 0 || b.va != 0x10000000) {
		printf("vm_bind FAILED: bind returned va 0x%llx\n",
		    (unsigned long long)b.va);
		return (1);
	}
	if (bo_write(fd, a.bo_fd, &data, sizeof(data)) != 0 ||
	    bo_read(fd, a.bo_fd, &back, sizeof(back)) != 0 || back != data) {
		printf("vm_bind FAILED: read-back after bind\n");
		return (1);
	}
	/* A second bind of the same BO must be rejected (EBUSY). */
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_BIND, &b) == 0) {
		printf("vm_bind FAILED: double-bind allowed\n");
		return (1);
	}
	close((int)a.bo_fd);
	close(vm);
	printf("vm_bind OK: create + bind are separate (va 0x10000000), "
	    "double-bind rejected\n");
	return (0);
}

/*
 * M9e: user-mode queue. The kernel programs the queue (QUEUE_MAP) and hands
 * back the doorbell; userspace mmap()s it and rings it DIRECTLY — no submit
 * ioctl on the hot path. The doorbell page is the (capability-scoped) MMIO the
 * jail holds.
 */
static int
test_umq(int fd, int vm)
{
	struct atrium_gpu_queue_map m;
	uint32_t ring_h, fence_h, ring[9];
	uint64_t ring_va, fence_va, fence = 0;
	volatile uint32_t *doorbell;
	void *map;

	if (bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &fence_h, &fence_va) != 0) {
		printf("umq: BO alloc failed\n");
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
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0) {
		printf("umq: write ring failed\n");
		return (1);
	}

	memset(&m, 0, sizeof(m));
	m.vm_fd = vm;
	m.ring_fd = ring_h;
	m.engine = ATRIUM_GPU_ENGINE_GFX;
	if (ioctl(fd, ATRIUM_GPU_IOC_QUEUE_MAP, &m) != 0) {
		printf("umq: QUEUE_MAP failed\n");
		return (1);
	}
	map = mmap(NULL, m.doorbell_size, PROT_READ | PROT_WRITE, MAP_SHARED,
	    fd, m.doorbell_mmap_offset);
	if (map == MAP_FAILED) {
		perror("umq: mmap doorbell");
		return (1);
	}

	/* Ring the doorbell directly from userspace — this is the UMQ hot path. */
	doorbell = (volatile uint32_t *)((char *)map + m.doorbell_word_offset);
	*doorbell = 9;

	if (bo_read(fd, fence_h, &fence, sizeof(fence)) != 0) {
		printf("umq: read fence failed\n");
		return (1);
	}
	munmap(map, m.doorbell_size);
	if (fence != FENCE_MAGIC) {
		printf("umq FAILED: fence = 0x%016llx\n",
		    (unsigned long long)fence);
		return (1);
	}
	printf("umq OK: userspace rang the doorbell directly, fence = 0x%016llx\n",
	    (unsigned long long)fence);
	return (0);
}

/* M9f: query device capabilities and walk the TLV (skipping unknown records). */
static int
test_caps(int fd)
{
	struct atrium_gpu_caps_query q;
	uint8_t buf[256];
	const char *vendor = NULL;
	uint32_t feat = 0;
	size_t off = 0;
	int saw_ver = 0;

	memset(&q, 0, sizeof(q));
	q.caps_ptr = (uint64_t)(uintptr_t)buf;
	q.caps_size = sizeof(buf);
	if (ioctl(fd, ATRIUM_GPU_IOC_QUERY_CAPS, &q) != 0) {
		printf("caps: QUERY_CAPS failed\n");
		return (1);
	}
	while (off + sizeof(struct atrium_gpu_cap_record) <= q.caps_size) {
		struct atrium_gpu_cap_record *r =
		    (struct atrium_gpu_cap_record *)(buf + off);
		uint8_t *data = buf + off + sizeof(*r);

		switch (r->cap_id) {
		case ATRIUM_GPU_CAP_ABI_VERSION:
			saw_ver = 1;
			break;
		case ATRIUM_GPU_CAP_VENDOR:
			vendor = (const char *)data;
			break;
		case ATRIUM_GPU_CAP_FEATURES:
			memcpy(&feat, data, sizeof(feat));
			break;
		default:
			break;	/* unknown cap — skip it (forward compat) */
		}
		off += sizeof(*r) + r->cap_size;
		off = (off + 3u) & ~(size_t)3u;
	}
	if (!saw_ver || vendor == NULL ||
	    !(feat & ATRIUM_GPU_FEAT_USER_QUEUES) ||
	    !(feat & ATRIUM_GPU_FEAT_SYNCOBJ)) {
		printf("caps FAILED: vendor=%s feat=0x%x\n",
		    vendor ? vendor : "(none)", feat);
		return (1);
	}
	printf("caps OK: \"%s\", features 0x%x (graphics+compute+UMQ+syncobj+"
	    "vm_bind)\n", vendor, feat);
	return (0);
}

/*
 * M10: asynchronous cross-queue completion. Queue A (compute) waits on a fence
 * word and then signals its syncobj; queue B (gfx) writes that word. A is
 * submitted FIRST and its ring parks on the WAIT — so its syncobj must still
 * read 0 (completion is deferred, NOT signalled inline at submit). Only when
 * B's later doorbell writes the word does the model resume A, raise the
 * end-of-pipe IRQ, and the ISR signal A's syncobj. This exercises the
 * persistent cross-doorbell parking (gpusim) + the ISR-driven signal path
 * (kmod) — the real-hardware async-vs-sync distinction the sync drain hid.
 */
static int
test_cross_queue(int fd, int vm)
{
	struct atrium_gpu_syncobj_create cr;
	struct atrium_gpu_syncobj_op q;
	struct atrium_gpu_syncobj_wait wt;
	struct atrium_gpu_submit s;
	uint32_t addr_h, fence_h, ringa_h, ringb_h;
	uint32_t ringa[14], ringb[6], zero = 0;
	uint64_t addr_va, fence_va, ringa_va, ringb_va, fb = 0;
	int so_fd;

	memset(&cr, 0, sizeof(cr));
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_CREATE, &cr) != 0) {
		printf("cross_queue: syncobj create failed\n");
		return (1);
	}
	so_fd = (int)cr.out_fd;

	if (bo_alloc(fd, vm, 4096, &addr_h, &addr_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &fence_h, &fence_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ringa_h, &ringa_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ringb_h, &ringb_va) != 0) {
		printf("cross_queue: BO alloc failed\n");
		return (1);
	}
	if (bo_write(fd, addr_h, &zero, sizeof(zero)) != 0) {
		printf("cross_queue: zero-init failed\n");
		return (1);
	}

	/* Ring A (compute): WAIT_REG_MEM(addr >= 1), then RELEASE_MEM + IRQ. */
	ringa[0] = type3(IT_WAIT_REG_MEM, 6);
	ringa[1] = (WAIT_FN_GE << 0) | (1u << 4);	/* function GE | mem-space */
	ringa[2] = (uint32_t)(addr_va & 0xffffffff);
	ringa[3] = (uint32_t)(addr_va >> 32);
	ringa[4] = 1;					/* reference */
	ringa[5] = 0xffffffff;				/* mask */
	ringa[6] = 0x10;
	ringa[7] = type3(IT_RELEASE_MEM, 6);
	ringa[8] = (5u << 8);
	ringa[9] = (2u << 29) | (2u << 24);
	ringa[10] = (uint32_t)(fence_va & 0xffffffff);
	ringa[11] = (uint32_t)(fence_va >> 32);
	ringa[12] = (uint32_t)(FENCE_MAGIC & 0xffffffff);
	ringa[13] = (uint32_t)(FENCE_MAGIC >> 32);

	/* Ring B (gfx): WRITE_DATA(addr) = 1 — unblocks A. */
	ringb[0] = type3(IT_WRITE_DATA, 5);
	ringb[1] = (5u << 8) | (1u << 20);		/* DST_SEL memory | WR_CONFIRM */
	ringb[2] = (uint32_t)(addr_va & 0xffffffff);
	ringb[3] = (uint32_t)(addr_va >> 32);
	ringb[4] = 1;
	ringb[5] = 0;

	if (bo_write(fd, ringa_h, ringa, sizeof(ringa)) != 0 ||
	    bo_write(fd, ringb_h, ringb, sizeof(ringb)) != 0) {
		printf("cross_queue: ring write failed\n");
		return (1);
	}

	/* Submit A first; its ring parks on the WAIT (addr is still 0). */
	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ringa_h;
	s.n_dwords = 14;
	s.engine = ATRIUM_GPU_ENGINE_COMPUTE;
	s.signal_syncobj_fd = so_fd;
	s.signal_value = 1;
	if (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) != 0) {
		printf("cross_queue: submit A failed\n");
		return (1);
	}

	/* KEY: completion is deferred — the syncobj must still read 0 here. */
	memset(&q, 0, sizeof(q));
	q.syncobj_fd = so_fd;
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_QUERY, &q) != 0) {
		printf("cross_queue: query failed\n");
		return (1);
	}
	if (q.value != 0) {
		printf("cross_queue FAILED: syncobj signalled at submit "
		    "(value=%llu) — completion was not deferred\n",
		    (unsigned long long)q.value);
		return (1);
	}

	/* Submit B; writing the word unblocks A, which completes + raises IRQ. */
	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ringb_h;
	s.n_dwords = 6;
	s.engine = ATRIUM_GPU_ENGINE_GFX;
	s.signal_syncobj_fd = -1;
	if (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) != 0) {
		printf("cross_queue: submit B failed\n");
		return (1);
	}

	/* A's syncobj is now signalled — by the ISR, on B's doorbell. */
	memset(&wt, 0, sizeof(wt));
	wt.syncobj_fd = so_fd;
	wt.value = 1;
	wt.timeout_ms = 1000;
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_WAIT, &wt) != 0) {
		printf("cross_queue FAILED: A never completed after B's submit\n");
		return (1);
	}
	if (bo_read(fd, fence_h, &fb, sizeof(fb)) != 0 || fb != FENCE_MAGIC) {
		printf("cross_queue FAILED: A's fence = 0x%llx (expected magic)\n",
		    (unsigned long long)fb);
		return (1);
	}

	close(so_fd);
	printf("cross_queue OK: A parked (syncobj=0 at submit), B's write "
	    "unblocked it, ISR signalled the deferred completion\n");
	return (0);
}

/*
 * M11: fault/hang -> reset -> recovery. A compute ring waits on a word nobody
 * will ever write, so it parks forever (persistent parking, M10) — the
 * submission is lost. The fence-wait must TIME OUT; the driver then issues a
 * full GPU reset, which tears down the rings and reloads firmware/MES, and a
 * normal submit succeeds on the clean engine. This is the timeout -> reset ->
 * resubmit recovery a real driver runs on a device-lost, made testable by the
 * persistent-parking model.
 */
static int
test_fault_reset(int fd, int vm)
{
	struct atrium_gpu_syncobj_create cr;
	struct atrium_gpu_syncobj_wait wt;
	struct atrium_gpu_submit s;
	uint32_t stuck_h, ring_h, fence_h, ring[7], gring[9], zero = 0;
	uint64_t stuck_va, ring_va, fence_va, fb = 0;
	int so_fd;

	memset(&cr, 0, sizeof(cr));
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_CREATE, &cr) != 0) {
		printf("fault_reset: syncobj create failed\n");
		return (1);
	}
	so_fd = (int)cr.out_fd;

	if (bo_alloc(fd, vm, 4096, &stuck_h, &stuck_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &fence_h, &fence_va) != 0) {
		printf("fault_reset: BO alloc failed\n");
		return (1);
	}
	if (bo_write(fd, stuck_h, &zero, sizeof(zero)) != 0) {
		printf("fault_reset: zero-init failed\n");
		return (1);
	}

	/* A compute ring that waits on a word that never becomes 1. */
	ring[0] = type3(IT_WAIT_REG_MEM, 6);
	ring[1] = (WAIT_FN_GE << 0) | (1u << 4);
	ring[2] = (uint32_t)(stuck_va & 0xffffffff);
	ring[3] = (uint32_t)(stuck_va >> 32);
	ring[4] = 1;
	ring[5] = 0xffffffff;
	ring[6] = 0x10;
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0) {
		printf("fault_reset: ring write failed\n");
		return (1);
	}
	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ring_h;
	s.n_dwords = 7;
	s.engine = ATRIUM_GPU_ENGINE_COMPUTE;
	s.signal_syncobj_fd = so_fd;
	s.signal_value = 1;
	if (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) != 0) {
		printf("fault_reset: submit failed\n");
		return (1);
	}

	/* The submission is lost — the fence-wait must time out, not complete. */
	memset(&wt, 0, sizeof(wt));
	wt.syncobj_fd = so_fd;
	wt.value = 1;
	wt.timeout_ms = 200;
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_WAIT, &wt) == 0) {
		printf("fault_reset FAILED: a lost submit's wait completed\n");
		return (1);
	}

	/* Recover the engine. */
	if (ioctl(fd, ATRIUM_GPU_IOC_GPU_RESET, NULL) != 0) {
		printf("fault_reset FAILED: GPU_RESET ioctl\n");
		return (1);
	}

	/* A normal submit now runs on the clean engine. */
	gring[0] = type3(IT_NOP, 1);
	gring[1] = 0;
	gring[2] = type3(IT_RELEASE_MEM, 6);
	gring[3] = (5u << 8);
	gring[4] = (2u << 29) | (2u << 24);
	gring[5] = (uint32_t)(fence_va & 0xffffffff);
	gring[6] = (uint32_t)(fence_va >> 32);
	gring[7] = (uint32_t)(FENCE_MAGIC & 0xffffffff);
	gring[8] = (uint32_t)(FENCE_MAGIC >> 32);
	if (bo_write(fd, ring_h, gring, sizeof(gring)) != 0 ||
	    submit(fd, vm, ring_h, 9, ATRIUM_GPU_ENGINE_GFX) != 0) {
		printf("fault_reset FAILED: post-reset submit\n");
		return (1);
	}
	if (bo_read(fd, fence_h, &fb, sizeof(fb)) != 0 || fb != FENCE_MAGIC) {
		printf("fault_reset FAILED: post-reset fence = 0x%llx\n",
		    (unsigned long long)fb);
		return (1);
	}

	close(so_fd);
	printf("fault_reset OK: lost submit timed out, GPU reset recovered the "
	    "engine (post-reset fence = 0x%llx)\n", (unsigned long long)fb);
	return (0);
}

/*
 * M12a: textured draw. A 2x2 RGBA texture sampled (nearest) across a
 * full-screen quad whose UVs span [0,1] — the 16x16 RT splits into four 8x8
 * quadrants, each showing one texel. Proves the rasterizer fetches from a bound
 * texture (vs M6's interpolated vertex color).
 */
static int
test_textured_draw(int fd, int vm)
{
	uint32_t tex_h, vtx_h, rt_h, ring_h, ring[16], rt[256];
	uint64_t tex_va, vtx_va, rt_va, ring_va;
	int n;
	/* texel index = ty*2 + tx: [0]=(0,0) [1]=(1,0) [2]=(0,1) [3]=(1,1) */
	uint32_t tex[4] = { 0xff0000aa, 0x00ff00bb, 0x0000ffcc, 0xffffffdd };
	/* full-screen quad (2 tris); u=(x+1)/2, v=(1-y)/2 maps screen->texture */
	struct vtx verts[6] = {
		{ -1, -1, 0, 0, 1, 0 }, { 1, -1, 0, 1, 1, 0 }, { 1, 1, 0, 1, 0, 0 },
		{ -1, -1, 0, 0, 1, 0 }, { 1, 1, 0, 1, 0, 0 }, { -1, 1, 0, 0, 0, 0 },
	};

	if (bo_alloc(fd, vm, 4096, &tex_h, &tex_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &vtx_h, &vtx_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &rt_h, &rt_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("textured: BO alloc failed\n");
		return (1);
	}
	if (bo_write(fd, tex_h, tex, sizeof(tex)) != 0 ||
	    bo_write(fd, vtx_h, verts, sizeof(verts)) != 0) {
		printf("textured: BO write failed\n");
		return (1);
	}
	/* textured (2x2), nearest, no blend/depth — all carried in the ring. */
	n = emit_draw_sh(ring, vtx_va, rt_va, 16, 16, tex_va, 2, 2, 0, 0, 0);
	ring[n++] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[n++] = 6;
	if (bo_write(fd, ring_h, ring, n * 4) != 0 ||
	    submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, rt_h, rt, sizeof(rt)) != 0) {
		printf("textured: draw/readback failed\n");
		return (1);
	}
	/* rt[py*16 + px]; sample one pixel in each 8x8 quadrant. */
	if (rt[4 * 16 + 4] != tex[0] || rt[4 * 16 + 12] != tex[1] ||
	    rt[12 * 16 + 4] != tex[2] || rt[12 * 16 + 12] != tex[3]) {
		printf("textured FAILED: quadrants %08x %08x %08x %08x\n",
		    rt[4 * 16 + 4], rt[4 * 16 + 12], rt[12 * 16 + 4],
		    rt[12 * 16 + 12]);
		return (1);
	}
	printf("textured OK: 2x2 texture sampled into 4 RT quadrants\n");
	return (0);
}

/*
 * M12b: alpha blend. Fill the RT with an opaque color, then draw a semi-
 * transparent solid quad over it with BLEND_ENABLE — the result is the
 * src-over composite, not the source. Mirrors the model's blend_over.
 */
static int
test_blend(int fd, int vm)
{
	uint32_t vtx_h, rt_h, ring_h, ring[16], rt[256], got, exp;
	uint64_t vtx_va, rt_va, ring_va;
	uint32_t dcol = 0xff203040;	/* opaque dst */
	uint32_t scol = 0x80c0b0a0;	/* src, alpha 0x80 */
	struct vtx q[6] = {
		{ -1, -1, 0, 0, 0, 0 }, { 1, -1, 0, 0, 0, 0 }, { 1, 1, 0, 0, 0, 0 },
		{ -1, -1, 0, 0, 0, 0 }, { 1, 1, 0, 0, 0, 0 }, { -1, 1, 0, 0, 0, 0 },
	};
	int i, n;

	if (bo_alloc(fd, vm, 4096, &vtx_h, &vtx_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &rt_h, &rt_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("blend: BO alloc failed\n");
		return (1);
	}
	/* Pass 1: fill RT with the opaque dst color (blend=0), state in-ring. */
	for (i = 0; i < 6; i++)
		q[i].color = dcol;
	n = emit_draw_sh(ring, vtx_va, rt_va, 16, 16, 0, 0, 0, 0, 0, 0);
	ring[n++] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[n++] = 6;
	if (bo_write(fd, vtx_h, q, sizeof(q)) != 0 ||
	    bo_write(fd, ring_h, ring, n * 4) != 0 ||
	    submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX) != 0) {
		printf("blend: fill pass failed\n");
		return (1);
	}
	/* Pass 2: blend the semi-transparent src over it (blend=1). */
	for (i = 0; i < 6; i++)
		q[i].color = scol;
	n = emit_draw_sh(ring, vtx_va, rt_va, 16, 16, 0, 0, 0, 0, 1, 0);
	ring[n++] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[n++] = 6;
	if (bo_write(fd, vtx_h, q, sizeof(q)) != 0 ||
	    bo_write(fd, ring_h, ring, n * 4) != 0 ||
	    submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, rt_h, rt, sizeof(rt)) != 0) {
		printf("blend: blend pass failed\n");
		return (1);
	}
	got = rt[8 * 16 + 8];
	exp = blend_over(scol, dcol);
	/* Allow +/-1 per channel for float-rounding differences. */
	for (i = 0; i < 32; i += 8) {
		int g = (got >> i) & 0xff, e = (exp >> i) & 0xff;
		if (g - e > 1 || e - g > 1) {
			printf("blend FAILED: got %08x expected ~%08x\n", got, exp);
			return (1);
		}
	}
	if (got == scol) {
		printf("blend FAILED: result equals src (blend was a no-op)\n");
		return (1);
	}
	printf("blend OK: src-over %08x over %08x = %08x (~%08x)\n",
	    scol, dcol, got, exp);
	return (0);
}

/*
 * M12c: CP DMA copy. A DMA_DATA packet copies one BO to another entirely on
 * the GPU (memory->memory through GPUVM) — the SDMA/CP-DMA path a driver uses
 * for uploads and blits, with no CPU touch of the bytes between.
 */
static int
test_dma_copy(int fd, int vm)
{
	uint32_t src_h, dst_h, ring_h, ring[7], src[64], dst[64];
	uint64_t src_va, dst_va, ring_va;
	int i;

	if (bo_alloc(fd, vm, 4096, &src_h, &src_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &dst_h, &dst_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("dma_copy: BO alloc failed\n");
		return (1);
	}
	for (i = 0; i < 64; i++) {
		src[i] = 0xa5a50000u + (uint32_t)i;
		dst[i] = 0;
	}
	bo_write(fd, src_h, src, sizeof(src));
	bo_write(fd, dst_h, dst, sizeof(dst));

	ring[0] = type3(IT_DMA_DATA, 6);
	ring[1] = 0;				/* control (mem->mem) */
	ring[2] = (uint32_t)(src_va & 0xffffffff);
	ring[3] = (uint32_t)(src_va >> 32);
	ring[4] = (uint32_t)(dst_va & 0xffffffff);
	ring[5] = (uint32_t)(dst_va >> 32);
	ring[6] = sizeof(src);			/* byte count */
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    submit(fd, vm, ring_h, 7, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, dst_h, dst, sizeof(dst)) != 0) {
		printf("dma_copy: submit/readback failed\n");
		return (1);
	}
	if (memcmp(src, dst, sizeof(src)) != 0) {
		printf("dma_copy FAILED: dst[0]=%08x dst[63]=%08x\n", dst[0],
		    dst[63]);
		return (1);
	}
	printf("dma_copy OK: GPU copied %zu bytes BO->BO (dst[0]=%08x)\n",
	    sizeof(src), dst[0]);
	return (0);
}

/*
 * M13a: depth-tested draw. Init the depth buffer to far, then draw three
 * full-screen quads at different NDC depths. The z-test (smaller z = closer)
 * must occlude a farther quad drawn after a nearer one, and let a nearer quad
 * drawn last win — order-independent occlusion. depth_va was plumbed in M12;
 * this exercises it.
 */
static int
test_depth(int fd, int vm)
{
	uint32_t vtx_h, rt_h, dep_h, ring_h, ring[16], rt[256], depbuf[256];
	uint64_t vtx_va, rt_va, dep_va, ring_va;
	uint32_t cA = 0xffaa1111, cB = 0xff22bb22, cC = 0xff3333cc;
	struct vtx q[6];
	float far = 1.0f;
	uint32_t far_bits;
	int i, n;

	memcpy(&far_bits, &far, 4);
	if (bo_alloc(fd, vm, 4096, &vtx_h, &vtx_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &rt_h, &rt_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &dep_h, &dep_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("depth: BO alloc failed\n");
		return (1);
	}
	for (i = 0; i < 256; i++)
		depbuf[i] = far_bits;	/* depth cleared to far (1.0) */
	bo_write(fd, dep_h, depbuf, sizeof(depbuf));
	/* Draw state (incl. the depth buffer VA) rides in the ring; written once. */
	n = emit_draw_sh(ring, vtx_va, rt_va, 16, 16, 0, 0, 0, 0, 0, dep_va);
	ring[n++] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[n++] = 6;
	bo_write(fd, ring_h, ring, n * 4);

	/* near quad (z=0.5), then a farther quad (z=0.8) that must be occluded. */
	fill_quad(q, 0.5f, cA);
	bo_write(fd, vtx_h, q, sizeof(q));
	submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX);
	fill_quad(q, 0.8f, cB);
	bo_write(fd, vtx_h, q, sizeof(q));
	submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX);
	if (bo_read(fd, rt_h, rt, sizeof(rt)) != 0 || rt[8 * 16 + 8] != cA) {
		printf("depth FAILED: farther quad not occluded (%08x)\n",
		    rt[8 * 16 + 8]);
		return (1);
	}
	/* a nearer quad (z=0.2) drawn last must win. */
	fill_quad(q, 0.2f, cC);
	bo_write(fd, vtx_h, q, sizeof(q));
	submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX);
	if (bo_read(fd, rt_h, rt, sizeof(rt)) != 0 || rt[8 * 16 + 8] != cC) {
		printf("depth FAILED: nearer quad did not win (%08x)\n",
		    rt[8 * 16 + 8]);
		return (1);
	}
	printf("depth OK: farther draw occluded, nearer draw won (z-test)\n");
	return (0);
}

/*
 * M13b: bilinear texture filtering. The same 2x2 texture as the nearest test,
 * but tex_filter=1 — at the RT center the 4-tap sample blends adjacent texels,
 * so the value is intermediate (not a hard quadrant edge). tex_filter was
 * plumbed in M12; this exercises the bilinear path.
 */
static int
test_bilinear(int fd, int vm)
{
	uint32_t tex_h, vtx_h, rt_h, ring_h, ring[16], rt[256];
	uint64_t tex_va, vtx_va, rt_va, ring_va;
	/* left column byte0=0, right column byte0=200: a horizontal ramp. */
	uint32_t tex[4] = { 0x00000000, 0x000000c8, 0x00000000, 0x000000c8 };
	struct vtx q[6];
	int mid, n;

	if (bo_alloc(fd, vm, 4096, &tex_h, &tex_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &vtx_h, &vtx_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &rt_h, &rt_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("bilinear: BO alloc failed\n");
		return (1);
	}
	fill_quad(q, 0, 0);
	bo_write(fd, tex_h, tex, sizeof(tex));
	bo_write(fd, vtx_h, q, sizeof(q));
	/* textured (2x2), tex_filter=1 (bilinear) — state carried in the ring. */
	n = emit_draw_sh(ring, vtx_va, rt_va, 16, 16, tex_va, 2, 2, 1, 0, 0);
	ring[n++] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[n++] = 6;
	if (bo_write(fd, ring_h, ring, n * 4) != 0 ||
	    submit(fd, vm, ring_h, n, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, rt_h, rt, sizeof(rt)) != 0) {
		printf("bilinear: draw/readback failed\n");
		return (1);
	}
	mid = rt[8 * 16 + 8] & 0xff;	/* byte0 of the center pixel */
	if (mid <= 10 || mid >= 190) {
		printf("bilinear FAILED: center byte0 = %d (not interpolated)\n",
		    mid);
		return (1);
	}
	printf("bilinear OK: center texel interpolated to %d (between 0 and 200)\n",
	    mid);
	return (0);
}

/*
 * M16: multi-page BOs via bus_dma. Allocate BOs larger than one page (here 3
 * pages each), then have the GPU DMA-copy src->dst entirely on-device. The copy
 * crosses page boundaries, so it only succeeds if every page of both BOs is
 * mapped in the GPUVM — i.e. the per-page bus_dma scatter-gather + page-table
 * population works. Single-page BOs (size > PAGE_SIZE was EINVAL) couldn't do
 * this before.
 */
static int
test_multipage(int fd, int vm)
{
	const uint32_t BYTES = 3 * 4096;	/* 3-page BOs */
	const uint32_t N = BYTES / 4;
	uint32_t src_h, dst_h, ring_h, ring[7], *src, *dst, i;
	uint64_t src_va, dst_va, ring_va;
	int rc = 1;

	src = malloc(BYTES);
	dst = malloc(BYTES);
	if (src == NULL || dst == NULL) {
		printf("multipage: malloc failed\n");
		goto out;
	}
	for (i = 0; i < N; i++) {
		src[i] = 0x9a9a0000u + i;	/* position-dependent: a per-page */
		dst[i] = 0;			/* mapping bug shows as wrong words */
	}
	if (bo_alloc(fd, vm, BYTES, &src_h, &src_va) != 0 ||
	    bo_alloc(fd, vm, BYTES, &dst_h, &dst_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("multipage: multi-page BO alloc failed\n");
		goto out;
	}
	if (bo_write(fd, src_h, src, BYTES) != 0) {
		printf("multipage: write src failed\n");
		goto out;
	}
	/* DMA_DATA copy of the whole 3-page BO, src -> dst. */
	ring[0] = type3(IT_DMA_DATA, 6);
	ring[1] = 0;
	ring[2] = (uint32_t)(src_va & 0xffffffff);
	ring[3] = (uint32_t)(src_va >> 32);
	ring[4] = (uint32_t)(dst_va & 0xffffffff);
	ring[5] = (uint32_t)(dst_va >> 32);
	ring[6] = BYTES;
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    submit(fd, vm, ring_h, 7, ATRIUM_GPU_ENGINE_GFX) != 0 ||
	    bo_read(fd, dst_h, dst, BYTES) != 0) {
		printf("multipage: copy/readback failed\n");
		goto out;
	}
	for (i = 0; i < N; i++)
		if (dst[i] != src[i]) {
			printf("multipage FAILED: word %u (page %u) = %08x want %08x\n",
			    i, (i * 4) / 4096, dst[i], src[i]);
			goto out;
		}
	printf("multipage OK: %u-page BO copied across page boundaries "
	    "(bus_dma scatter-gather GPUVM)\n", BYTES / 4096);
	rc = 0;
out:
	free(src);
	free(dst);
	return (rc);
}

/*
 * M17: per-queue doorbell-page granularity. The doorbell BAR is divided into a
 * page per queue, so QUEUE_MAP hands gfx and compute *different* mmap offsets —
 * mapping one queue's doorbell page exposes only that queue (the page is the
 * capability, SCM_RIGHTS-grantable to a jailed client). Map only the compute
 * queue's page and ring it directly to prove it drives just that queue.
 */
static int
test_doorbell_pages(int fd, int vm)
{
	struct atrium_gpu_queue_map mg, mc;
	uint32_t src_h, dst_h, ring_h, ring[11];
	uint32_t in[4] = { 7, 8, 9, 10 }, out[4] = { 0 };
	uint64_t src_va, dst_va, ring_va;
	volatile uint32_t *doorbell;
	void *map;
	int n, i;

	if (bo_alloc(fd, vm, 4096, &src_h, &src_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &dst_h, &dst_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("doorbell_pages: BO alloc failed\n");
		return (1);
	}
	bo_write(fd, src_h, in, sizeof(in));
	n = emit_compute_sh(ring, KERNEL_INC, src_va, dst_va);
	ring[n++] = type3(IT_DISPATCH_DIRECT, 3);
	ring[n++] = 4;
	ring[n++] = 1;
	ring[n++] = 1;
	bo_write(fd, ring_h, ring, n * 4);

	/* Map a gfx and a compute queue; their doorbells must be on different
	 * pages (per-queue granularity). */
	memset(&mg, 0, sizeof(mg));
	mg.vm_fd = vm;
	mg.ring_fd = ring_h;
	mg.engine = ATRIUM_GPU_ENGINE_GFX;
	memset(&mc, 0, sizeof(mc));
	mc.vm_fd = vm;
	mc.ring_fd = ring_h;
	mc.engine = ATRIUM_GPU_ENGINE_COMPUTE;
	if (ioctl(fd, ATRIUM_GPU_IOC_QUEUE_MAP, &mg) != 0 ||
	    ioctl(fd, ATRIUM_GPU_IOC_QUEUE_MAP, &mc) != 0) {
		printf("doorbell_pages: QUEUE_MAP failed\n");
		return (1);
	}
	if (mg.doorbell_mmap_offset == mc.doorbell_mmap_offset) {
		printf("doorbell_pages FAILED: gfx and compute share a page (0x%llx)\n",
		    (unsigned long long)mg.doorbell_mmap_offset);
		return (1);
	}
	/* Map ONLY the compute queue's page and ring it directly. */
	map = mmap(NULL, mc.doorbell_size, PROT_READ | PROT_WRITE, MAP_SHARED,
	    fd, mc.doorbell_mmap_offset);
	if (map == MAP_FAILED) {
		perror("doorbell_pages: mmap");
		return (1);
	}
	doorbell = (volatile uint32_t *)((char *)map + mc.doorbell_word_offset);
	*doorbell = n;
	munmap(map, mc.doorbell_size);

	if (bo_read(fd, dst_h, out, sizeof(out)) != 0) {
		printf("doorbell_pages: read failed\n");
		return (1);
	}
	for (i = 0; i < 4; i++)
		if (out[i] != in[i] + 1) {
			printf("doorbell_pages FAILED: compute via own page gave "
			    "[%u %u %u %u]\n", out[0], out[1], out[2], out[3]);
			return (1);
		}
	printf("doorbell_pages OK: gfx page 0x%llx, compute page 0x%llx (separate); "
	    "compute rang via its own page\n",
	    (unsigned long long)mg.doorbell_mmap_offset,
	    (unsigned long long)mc.doorbell_mmap_offset);
	return (0);
}

/* Allocate a BO with placement flags (ATRIUM_GPU_BO_VRAM) and bind it. */
static int
bo_alloc_flags(int fd, int vm, uint64_t size, uint32_t flags, uint32_t *handle,
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

/* GPU-side DMA copy of `bytes` from src_va to dst_va via a DMA_DATA ring. */
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
	return (submit(fd, vm, ring_h, 7, ATRIUM_GPU_ENGINE_GFX));
}

/*
 * M18: VRAM-resident BOs + reset-loses-VRAM. A VRAM BO lives in device-local
 * memory (BAR0), not guest RAM — it's GPU-only (a direct CPU write is refused),
 * so it's populated by a GPU copy from a System staging BO. Data round-trips
 * through VRAM (staging -> VRAM -> result). A full GPU reset then clears VRAM
 * (System/GTT survives), so the BO reads back zero until the driver re-uploads
 * from its CPU shadow.
 */
static int
test_vram(int fd, int vm)
{
	uint32_t stg_h, vbo_h, res_h, ring_h;
	uint32_t pat[64], out[64];
	uint64_t stg_va, vbo_va, res_va, ring_va;
	int i;

	for (i = 0; i < 64; i++) {
		pat[i] = 0x77000000u + i;
		out[i] = 0;
	}
	if (bo_alloc(fd, vm, sizeof(pat), &stg_h, &stg_va) != 0 ||
	    bo_alloc_flags(fd, vm, sizeof(pat), ATRIUM_GPU_BO_VRAM, &vbo_h, &vbo_va) != 0 ||
	    bo_alloc(fd, vm, sizeof(pat), &res_h, &res_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0) {
		printf("vram: BO alloc failed\n");
		return (1);
	}
	if (bo_write(fd, stg_h, pat, sizeof(pat)) != 0) {
		printf("vram: staging write failed\n");
		return (1);
	}
	/* A VRAM BO is GPU-only: a direct CPU write must be refused. */
	if (bo_write(fd, vbo_h, pat, sizeof(pat)) == 0) {
		printf("vram FAILED: CPU wrote a VRAM BO directly\n");
		return (1);
	}
	/* Upload staging(System) -> VRAM, then read VRAM -> result(System). */
	if (dma_copy(fd, vm, ring_h, stg_va, vbo_va, sizeof(pat)) != 0 ||
	    dma_copy(fd, vm, ring_h, vbo_va, res_va, sizeof(pat)) != 0 ||
	    bo_read(fd, res_h, out, sizeof(out)) != 0) {
		printf("vram: round-trip copy failed\n");
		return (1);
	}
	if (memcmp(pat, out, sizeof(pat)) != 0) {
		printf("vram FAILED: round-trip through VRAM mismatch (out[0]=%08x)\n",
		    out[0]);
		return (1);
	}
	/* A full GPU reset loses VRAM — the BO now reads back zero. */
	if (ioctl(fd, ATRIUM_GPU_IOC_GPU_RESET, NULL) != 0) {
		printf("vram: GPU_RESET failed\n");
		return (1);
	}
	memset(out, 0xff, sizeof(out));
	if (dma_copy(fd, vm, ring_h, vbo_va, res_va, sizeof(pat)) != 0 ||
	    bo_read(fd, res_h, out, sizeof(out)) != 0) {
		printf("vram: post-reset read failed\n");
		return (1);
	}
	for (i = 0; i < 64; i++)
		if (out[i] != 0) {
			printf("vram FAILED: VRAM survived reset (out[%d]=%08x)\n",
			    i, out[i]);
			return (1);
		}
	/* Re-upload from the CPU shadow restores it. */
	if (dma_copy(fd, vm, ring_h, stg_va, vbo_va, sizeof(pat)) != 0 ||
	    dma_copy(fd, vm, ring_h, vbo_va, res_va, sizeof(pat)) != 0 ||
	    bo_read(fd, res_h, out, sizeof(out)) != 0 ||
	    memcmp(pat, out, sizeof(pat)) != 0) {
		printf("vram FAILED: re-upload after reset did not restore\n");
		return (1);
	}
	printf("vram OK: round-trips through VRAM; reset loses it; re-upload "
	    "restores\n");
	return (0);
}

int
main(void)
{
	int fd, vm, rc;

	fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0) {
		perror("open /dev/atrium-gpu0");
		return (1);
	}
	vm = vm_create(fd);
	if (vm < 0) {
		printf("VM_CREATE failed\n");
		close(fd);
		return (1);
	}
	rc = test_caps(fd);
	rc |= test_bo_fd(fd, vm);
	rc |= test_gfx_fence(fd, vm);
	rc |= test_compute(fd, vm);
	rc |= test_draw(fd, vm);
	rc |= test_irq(fd, vm);
	rc |= test_syncobj(fd, vm);
	rc |= test_vm_bind(fd);
	rc |= test_isolation(fd);
	rc |= test_umq(fd, vm);
	rc |= test_cross_queue(fd, vm);
	rc |= test_fault_reset(fd, vm);
	rc |= test_textured_draw(fd, vm);
	rc |= test_blend(fd, vm);
	rc |= test_dma_copy(fd, vm);
	rc |= test_depth(fd, vm);
	rc |= test_bilinear(fd, vm);
	rc |= test_multipage(fd, vm);
	rc |= test_doorbell_pages(fd, vm);
	rc |= test_vram(fd, vm);
	close(vm);
	close(fd);
	printf(rc == 0 ? "ALL OK\n" : "FAILURES\n");
	return (rc);
}
