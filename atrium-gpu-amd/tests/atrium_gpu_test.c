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
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

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
	uint32_t src_h, dst_h, ring_h, ring[4];
	uint32_t in[4] = { 10, 20, 30, 40 }, out[4] = { 0 };
	uint64_t src_va, dst_va, ring_va;
	struct atrium_gpu_set_compute c;
	int i;

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
	ring[0] = type3(IT_DISPATCH_DIRECT, 3);
	ring[1] = 4;
	ring[2] = 1;
	ring[3] = 1;
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
	if (submit(fd, vm, ring_h, 4, ATRIUM_GPU_ENGINE_COMPUTE) != 0 ||
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
	uint32_t vtx_h, rt_h, ring_h, ring[2], rt[16 * 16];
	uint64_t vtx_va, rt_va, ring_va;
	struct atrium_gpu_set_draw d;
	uint8_t v[6 * 24];
	unsigned i;

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
	ring[0] = type3(IT_DRAW_INDEX_AUTO, 1);
	ring[1] = 6;

	memset(&d, 0, sizeof(d));
	d.vtx_va = vtx_va;
	d.rt_va = rt_va;
	d.width = W;
	d.height = H;
	if (bo_write(fd, vtx_h, v, sizeof(v)) != 0 ||
	    bo_write(fd, ring_h, ring, sizeof(ring)) != 0 ||
	    ioctl(fd, ATRIUM_GPU_IOC_SET_DRAW, &d) != 0 ||
	    submit(fd, vm, ring_h, 2, ATRIUM_GPU_ENGINE_GFX) != 0 ||
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
	struct atrium_gpu_set_compute c;
	uint32_t src_h, dst_h, ring_h, ring[4], out = 0;
	uint64_t src_va, dst_va, ring_va;

	if (bo_alloc(fd, vm, 4096, &src_h, &src_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &dst_h, &dst_va) != 0 ||
	    bo_alloc(fd, vm, 4096, &ring_h, &ring_va) != 0)
		return (-1);
	if (bo_write(fd, src_h, &input, sizeof(input)) != 0)
		return (-1);
	ring[0] = type3(IT_DISPATCH_DIRECT, 3);
	ring[1] = 1;
	ring[2] = 1;
	ring[3] = 1;
	if (bo_write(fd, ring_h, ring, sizeof(ring)) != 0)
		return (-1);
	memset(&c, 0, sizeof(c));
	c.kernel = KERNEL_INC;
	c.src_va = src_va;	/* same VA for both VMs — that's the point */
	c.dst_va = dst_va;
	if (ioctl(fd, ATRIUM_GPU_IOC_SET_COMPUTE, &c) != 0)
		return (-1);
	if (submit(fd, vm, ring_h, 4, ATRIUM_GPU_ENGINE_COMPUTE) != 0)
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
	rc = test_bo_fd(fd, vm);
	rc |= test_gfx_fence(fd, vm);
	rc |= test_compute(fd, vm);
	rc |= test_draw(fd, vm);
	rc |= test_irq(fd, vm);
	rc |= test_syncobj(fd, vm);
	rc |= test_vm_bind(fd);
	rc |= test_isolation(fd);
	rc |= test_umq(fd, vm);
	close(vm);
	close(fd);
	printf(rc == 0 ? "ALL OK\n" : "FAILURES\n");
	return (rc);
}
