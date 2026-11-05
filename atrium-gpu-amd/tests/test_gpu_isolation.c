/*
 * test_gpu_isolation — the gpu-isolation.md regression for atrium-gpu-amd.
 *
 * Part A is the spec's seven-step smoke test, adapted to this driver's ABI
 * (handles are fds, so "another context's handle" is an fd that is not a BO of
 * this device). Part B reproduces each of the five isolation breaks from
 * docs/reviews/2026-11-03-gpu-kernel-and-tier2-review.md §A3 and checks it is
 * closed. Breaks 1 and 2 need work that stays in flight, which gpusim gives us
 * with a ring parked on an unsatisfied WAIT_REG_MEM; the driver sees it through
 * the queue read pointer (gpusim 1ea93e5 or later).
 *
 * Run as root in the guest with the three modules loaded:
 *   cc -Wall -o /tmp/test_gpu_isolation tests/test_gpu_isolation.c
 *   /tmp/test_gpu_isolation [--attest]
 * --attest prints the jaild.policy.toml stanza for this build on success.
 */
#include "atrium_gpu_amd_abi.h"
#include "atrium_display_abi.h"
#include "../../include/atrium/display_abi_v2.h"

#include <sys/types.h>
#include <sys/event.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define PM4_TYPE3		3u
#define IT_NOP			0x10u
#define IT_WRITE_DATA		0x37u
#define IT_WAIT_REG_MEM		0x3cu
#define IT_DMA_DATA		0x50u
#define IT_RELEASE_MEM		0x49u
#define WAIT_FN_GE		5u

#define GPU		"/dev/atrium-gpu0"
#define GPUCTL		"/dev/atrium-gpu0ctl"
#define DISPLAY		"/dev/atrium-display0"
#define ISO		"dev.atrium_gpu_amd.0.isolation."
#define SENTINEL	0x5e57a11eu
#define RESULT		0x000a11ceu

static int failures;

#define CHECK(cond, ...) do {						\
	if (cond) {							\
		printf("  ok   ");					\
	} else {							\
		printf("  FAIL ");					\
		failures++;						\
	}								\
	printf(__VA_ARGS__);						\
	printf("\n");							\
} while (0)

static uint32_t
type3(uint32_t opcode, uint32_t body_dwords)
{
	return ((PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) |
	    (opcode << 8));
}

static int
xopen(const char *path)
{
	int fd = open(path, O_RDWR);

	if (fd < 0) {
		fprintf(stderr, "open %s: %s\n", path, strerror(errno));
		exit(2);
	}
	return (fd);
}

static int
vm_create(int fd)
{
	struct atrium_gpu_vm_create v;

	memset(&v, 0, sizeof(v));
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_CREATE, &v) != 0) {
		perror("VM_CREATE");
		exit(2);
	}
	return ((int)v.out_fd);
}

static int
bo_new(int fd, int vm, uint64_t size, uint32_t flags, uint64_t *va)
{
	struct atrium_gpu_bo_alloc a;
	struct atrium_gpu_vm_bind b;

	memset(&a, 0, sizeof(a));
	a.size = size;
	a.flags = flags;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0) {
		perror("BO_ALLOC");
		exit(2);
	}
	if (vm >= 0) {
		memset(&b, 0, sizeof(b));
		b.vm_fd = vm;
		b.bo_fd = a.bo_fd;
		if (ioctl(fd, ATRIUM_GPU_IOC_VM_BIND, &b) != 0) {
			perror("VM_BIND");
			exit(2);
		}
		*va = b.va;
	}
	return ((int)a.bo_fd);
}

static int
bo_write(int fd, int bo, const void *src, uint64_t len)
{
	struct atrium_gpu_bo_xfer x;

	memset(&x, 0, sizeof(x));
	x.bo_fd = bo;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)src;
	return (ioctl(fd, ATRIUM_GPU_IOC_BO_WRITE, &x) == 0 ? 0 : errno);
}

static uint32_t
bo_read32(int fd, int bo)
{
	struct atrium_gpu_bo_xfer x;
	uint32_t v = 0xffffffffu;

	memset(&x, 0, sizeof(x));
	x.bo_fd = bo;
	x.len = sizeof(v);
	x.user_ptr = (uint64_t)(uintptr_t)&v;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_READ, &x) != 0)
		return (0xdeaddeadu);
	return (v);
}

/* Submit; returns 0 or the errno. */
static int
submit(int fd, int vm, int ring, uint32_t n, uint32_t engine)
{
	struct atrium_gpu_submit s;

	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ring;
	s.n_dwords = n;
	s.engine = engine;
	s.signal_syncobj_fd = -1;
	return (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) == 0 ? 0 : errno);
}

static int
queue_map(int fd, int vm, int ring, uint32_t engine,
    struct atrium_gpu_queue_map *m)
{
	memset(m, 0, sizeof(*m));
	m->vm_fd = vm;
	m->ring_fd = ring;
	m->engine = engine;
	return (ioctl(fd, ATRIUM_GPU_IOC_QUEUE_MAP, m) == 0 ? 0 : errno);
}

static uint64_t
iso(const char *name)
{
	char oid[128];
	uint64_t v64 = 0;
	int v32;
	size_t len = sizeof(v64);

	snprintf(oid, sizeof(oid), ISO "%s", name);
	if (sysctlbyname(oid, &v64, &len, NULL, 0) != 0) {
		fprintf(stderr, "sysctl %s: %s\n", oid, strerror(errno));
		exit(2);
	}
	if (len == sizeof(v32)) {
		memcpy(&v32, &v64, sizeof(v32));
		return ((uint64_t)(int64_t)v32);
	}
	return (v64);
}

static double
now_s(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (ts.tv_sec + ts.tv_nsec / 1e9);
}

/* WRITE_DATA(va) = value; returns dwords. */
static int
emit_write(uint32_t *r, uint64_t va, uint32_t value)
{
	r[0] = type3(IT_WRITE_DATA, 5);
	r[1] = (5u << 8) | (1u << 20);
	r[2] = (uint32_t)(va & 0xffffffff);
	r[3] = (uint32_t)(va >> 32);
	r[4] = value;
	r[5] = 0;
	return (6);
}

/* WAIT_REG_MEM(word_va >= 1); returns dwords. */
static int
emit_wait(uint32_t *r, uint64_t word_va)
{
	r[0] = type3(IT_WAIT_REG_MEM, 6);
	r[1] = (WAIT_FN_GE << 0) | (1u << 4);
	r[2] = (uint32_t)(word_va & 0xffffffff);
	r[3] = (uint32_t)(word_va >> 32);
	r[4] = 1;
	r[5] = 0xffffffff;
	r[6] = 0x10;
	return (7);
}

/* A two-dword NOP ring, rung on compute to make gpusim sweep every queue. */
static int
kick(int fd, int vm)
{
	uint32_t r[2] = { type3(IT_NOP, 1), 0 };
	uint64_t va;
	int ring = bo_new(fd, vm, 4096, 0, &va), err;

	bo_write(fd, ring, r, sizeof(r));
	err = submit(fd, vm, ring, 2, ATRIUM_GPU_ENGINE_COMPUTE);
	close(ring);
	return (err);
}

/*
 * A parked job in its own VM: a gfx ring that waits on a word nobody has
 * written, then writes RESULT into `res` (and, if target_va != 0, SENTINEL to
 * target_va). Returns the ring's VM; word/res BO fds come back by pointer.
 */
struct parked {
	int fd, vm, ring, word, res;
	uint64_t word_va, res_va;
};

static int
park(struct parked *p, int fd, uint64_t extra_va, int extra_in_vm)
{
	uint32_t r[32], zero = 0;
	uint64_t ring_va;
	int n = 0;

	p->fd = fd;
	if (!extra_in_vm)
		p->vm = vm_create(fd);
	p->word = bo_new(fd, p->vm, 4096, 0, &p->word_va);
	p->res = bo_new(fd, p->vm, 4096, 0, &p->res_va);
	p->ring = bo_new(fd, p->vm, 4096, 0, &ring_va);
	bo_write(fd, p->word, &zero, sizeof(zero));
	bo_write(fd, p->res, &zero, sizeof(zero));
	n += emit_wait(r + n, p->word_va);
	n += emit_write(r + n, p->res_va, RESULT);
	if (extra_va != 0)
		n += emit_write(r + n, extra_va, SENTINEL);
	bo_write(fd, p->ring, r, n * 4);
	return (submit(fd, p->vm, p->ring, n, ATRIUM_GPU_ENGINE_GFX));
}

static void
unpark(struct parked *p)
{
	uint32_t one = 1;

	bo_write(p->fd, p->word, &one, sizeof(one));
	kick(p->fd, p->vm);
}

/* ---------------------------------------------------------------------------
 * Part A: gpu-isolation.md §Validation steps 1-7.
 */
static void
part_a(void)
{
	uint64_t va_a[3], va_b[2], va;
	uint32_t r[8], v;
	int fda, fdb, fdc, vma, vmb, vmc, a[3], b[2], c, pfd[2];

	printf("Part A: gpu-isolation.md smoke steps\n");
	/* 1. two clients, a context (VM) each. */
	fda = xopen(GPU);
	fdb = xopen(GPU);
	vma = vm_create(fda);
	vmb = vm_create(fdb);
	CHECK(vma >= 0 && vmb >= 0, "1. two clients, one VM each");

	/* 2. A: three BOs, the third filled with the sentinel. */
	a[0] = bo_new(fda, vma, 4096, 0, &va_a[0]);
	a[1] = bo_new(fda, vma, 4096, 0, &va_a[1]);
	a[2] = bo_new(fda, vma, 4096, 0, &va_a[2]);
	v = SENTINEL;
	CHECK(bo_write(fda, a[2], &v, 4) == 0 && bo_read32(fda, a[2]) == SENTINEL,
	    "2. A's BO holds the sentinel at VA 0x%llx",
	    (unsigned long long)va_a[2]);

	/* 3. B names something that is not one of this device's BOs. */
	pipe(pfd);
	CHECK(bo_write(fdb, pfd[0], &v, 4) == EINVAL,
	    "3. a non-BO fd as a BO handle is refused (EINVAL)");
	CHECK(bo_write(fdb, 999, &v, 4) == EBADF,
	    "3. an fd number B does not hold is refused (EBADF)");
	close(pfd[0]);
	close(pfd[1]);

	/* 4/5. B copies from A's VA in its own VM, where nothing is mapped. */
	b[0] = bo_new(fdb, vmb, 4096, 0, &va_b[0]);	/* ring */
	b[1] = bo_new(fdb, vmb, 4096, 0, &va_b[1]);	/* destination */
	r[0] = type3(IT_DMA_DATA, 6);
	r[1] = 0;
	r[2] = (uint32_t)(va_a[2] & 0xffffffff);
	r[3] = (uint32_t)(va_a[2] >> 32);
	r[4] = (uint32_t)(va_b[1] & 0xffffffff);
	r[5] = (uint32_t)(va_b[1] >> 32);
	r[6] = 4;
	bo_write(fdb, b[0], r, 7 * 4);
	v = 0;
	bo_write(fdb, b[1], &v, 4);
	submit(fdb, vmb, b[0], 7, ATRIUM_GPU_ENGINE_GFX);
	v = bo_read32(fdb, b[1]);
	CHECK(v != SENTINEL,
	    "4/5. B's copy from A's VA did not return A's data (got 0x%08x)", v);
	/* Re-arm gfx for later steps: B's faulted copy stopped the queue under
	 * B's VMID; closing B's VM below dequeues it. */

	/* 6. B's same-VA trick: its own BO at the VA A used holds B's data. */
	v = bo_read32(fdb, b[0]);
	CHECK(va_b[0] == va_a[0] && v != SENTINEL,
	    "6. the same VA in B's VM resolves to B's own pages");

	/* 7. A goes away; a fresh client's new BO holds no trace of A. */
	close(a[0]);
	close(a[1]);
	close(a[2]);
	close(vma);
	close(fda);
	close(b[0]);
	close(b[1]);
	close(vmb);
	close(fdb);
	fdc = xopen(GPU);
	vmc = vm_create(fdc);
	c = bo_new(fdc, vmc, 4096, 0, &va);
	v = bo_read32(fdc, c);
	CHECK(v == 0, "7. a fresh BO after A closed reads zero (0x%08x)", v);
	close(c);
	close(vmc);
	close(fdc);
}

/* ---------------------------------------------------------------------------
 * Part B: the five breaks.
 */

/* Break 1: another VM cannot take over a queue that has work in flight. */
static void
break1(void)
{
	struct parked p;
	uint64_t refused0, waits0, va;
	uint32_t r[8];
	double t0, dt;
	int fda, fdb, vmb, ringb, resb, err, n;

	printf("Break 1: shared-queue reprogramming under another VMID\n");
	fda = xopen(GPU);
	fdb = xopen(GPU);
	CHECK(park(&p, fda, 0, 0) == 0, "A's gfx ring submitted and parked");

	vmb = vm_create(fdb);
	ringb = bo_new(fdb, vmb, 4096, 0, &va);
	resb = bo_new(fdb, vmb, 4096, 0, &va);
	n = emit_write(r, va, 0xb0b);
	bo_write(fdb, ringb, r, n * 4);
	refused0 = iso("queue_refused");
	waits0 = iso("queue_waits");
	t0 = now_s();
	err = submit(fdb, vmb, ringb, n, ATRIUM_GPU_ENGINE_GFX);
	dt = now_s() - t0;
	CHECK(err == EBUSY, "B's gfx submit while A's work is queued: %s "
	    "after %.1f s (want EBUSY)", strerror(err), dt);
	CHECK(iso("queue_refused") == refused0 + 1 &&
	    iso("queue_waits") == waits0 + 1,
	    "counted as one wait and one refusal");
	CHECK(bo_read32(fda, p.res) == 0, "A's queued work has not run yet");

	unpark(&p);
	CHECK(bo_read32(fda, p.res) == RESULT,
	    "A's work ran to completion in A's VM once unblocked");
	err = submit(fdb, vmb, ringb, n, ATRIUM_GPU_ENGINE_GFX);
	CHECK(err == 0 && bo_read32(fdb, resb) == 0xb0b,
	    "B's submit runs once A's work has drained");
	close(ringb);
	close(resb);
	close(vmb);
	close(fdb);
	close(p.word);
	close(p.res);
	close(p.ring);
	close(p.vm);
	close(fda);
}

/* Break 2: BO and VM teardown while the GPU may still use them. */
static void
break2(void)
{
	struct parked p;
	uint64_t deferred0, reaped0, dequeues0, waits0, tva, va;
	uint32_t r[8];
	int now0, fda, target, fdd, vmd, ringd, resd, n;

	printf("Break 2: teardown while work is in flight\n");
	fda = xopen(GPU);
	now0 = (int)iso("deferred_now");
	deferred0 = iso("deferred");
	reaped0 = iso("reaped");
	dequeues0 = iso("dequeues");

	/* A parked ring that will write into `target` when it resumes. */
	p.vm = vm_create(fda);
	target = bo_new(fda, p.vm, 4096, 0, &tva);
	CHECK(park(&p, fda, tva, 1) == 0, "A parked a ring that writes into "
	    "BO T at 0x%llx", (unsigned long long)tva);

	close(target);
	CHECK(iso("deferred") == deferred0 + 1 &&
	    (int)iso("deferred_now") == now0 + 1,
	    "closing T while A's ring is queued defers its pages");

	unpark(&p);	/* resumes: the write to T's VA now faults in A's VM */
	CHECK(bo_read32(fda, p.res) == RESULT,
	    "A's work resumed in A's VM (its write to T's old VA has no PTE)");
	usleep(100 * 1000);	/* the reaper polls every ~10 ms */

	/* Close the rest of A: the VM teardown stops any queue still under its
	 * VMID before freeing page tables, which retires every deferral. */
	close(p.word);
	close(p.res);
	close(p.ring);
	close(p.vm);
	usleep(100 * 1000);
	CHECK((int)iso("deferred_now") == now0 && iso("reaped") >= reaped0 + 1,
	    "T's pages were freed only after the queue moved on");
	printf("       (dequeues with work dropped: %llu)\n",
	    (unsigned long long)(iso("dequeues") - dequeues0));

	/* A VM whose ring is parked forever, closed outright. */
	CHECK(park(&p, fda, 0, 0) == 0, "A parked a ring that never resumes");
	dequeues0 = iso("dequeues");
	close(p.word);
	close(p.res);
	close(p.ring);
	close(p.vm);
	usleep(100 * 1000);
	CHECK(iso("dequeues") == dequeues0 + 1 &&
	    (int)iso("deferred_now") == now0,
	    "closing the VM stopped its queue and freed its deferred BOs");

	/* The VMID is free and the queue idle: a new VM runs at once. */
	fdd = xopen(GPU);
	vmd = vm_create(fdd);
	ringd = bo_new(fdd, vmd, 4096, 0, &va);
	resd = bo_new(fdd, vmd, 4096, 0, &va);
	n = emit_write(r, va, 0xd00d);
	bo_write(fdd, ringd, r, n * 4);
	waits0 = iso("queue_waits");
	CHECK(submit(fdd, vmd, ringd, n, ATRIUM_GPU_ENGINE_GFX) == 0 &&
	    bo_read32(fdd, resd) == 0xd00d && iso("queue_waits") == waits0,
	    "a new VM (VMID reused) submits on gfx without waiting");
	close(ringd);
	close(resd);
	close(vmd);
	close(fdd);
	close(fda);
}

/* Break 3: doorbells are owner-only, revocable capabilities. */
static void
break3(void)
{
	struct atrium_gpu_queue_map m1, m2, mg;
	uint64_t va, revokes0;
	uint32_t r[8];
	volatile uint32_t *db;
	void *map, *bad;
	int fd1, fd2, vm1, vm2, ring1, ring2, res1, n, status;
	pid_t pid;

	printf("Break 3: doorbell mmap and QUEUE_MAP hijack\n");
	fd1 = xopen(GPU);
	fd2 = xopen(GPU);
	vm1 = vm_create(fd1);
	vm2 = vm_create(fd2);
	ring1 = bo_new(fd1, vm1, 4096, 0, &va);
	res1 = bo_new(fd1, vm1, 4096, 0, &va);
	n = emit_write(r, va, 0x1111);
	bo_write(fd1, ring1, r, n * 4);
	ring2 = bo_new(fd2, vm2, 4096, 0, &va);
	bo_write(fd2, ring2, r, n * 4);

	/* Nobody owns compute yet: no one can map its doorbell. */
	bad = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd2,
	    0x1000);	/* the compute doorbell page */
	CHECK(bad == MAP_FAILED, "mmap of an unowned doorbell is refused (%s)",
	    bad == MAP_FAILED ? strerror(errno) : "mapped!");

	CHECK(queue_map(fd1, vm1, ring1, ATRIUM_GPU_ENGINE_COMPUTE, &m1) == 0,
	    "client 1 maps the compute queue (doorbell 0x%llx)",
	    (unsigned long long)m1.doorbell_mmap_offset);
	CHECK(queue_map(fd2, vm2, ring2, ATRIUM_GPU_ENGINE_COMPUTE, &m2) == EBUSY,
	    "client 2 cannot take the compute queue (EBUSY)");
	CHECK(submit(fd2, vm2, ring2, n, ATRIUM_GPU_ENGINE_COMPUTE) == EBUSY,
	    "client 2 cannot submit on client 1's queue (EBUSY)");
	bad = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd2,
	    m1.doorbell_mmap_offset);
	CHECK(bad == MAP_FAILED && errno == EACCES,
	    "client 2 cannot mmap client 1's doorbell (EACCES)");
	bad = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd1, 0x0);
	CHECK(bad == MAP_FAILED,
	    "client 1 cannot mmap the gfx doorbell it does not own");

	map = mmap(NULL, m1.doorbell_size, PROT_READ | PROT_WRITE, MAP_SHARED,
	    fd1, m1.doorbell_mmap_offset);
	CHECK(map != MAP_FAILED, "client 1 maps its own doorbell");
	if (map != MAP_FAILED) {
		db = (volatile uint32_t *)((char *)map + m1.doorbell_word_offset);
		*db = n;
		CHECK(bo_read32(fd1, res1) == 0x1111,
		    "client 1 rang its queue directly from userspace");
	}

	/* Release on close: client 1's mapping goes dead, client 2 may map. */
	revokes0 = iso("revokes");
	close(fd1);
	CHECK(iso("revokes") == revokes0 + 1, "closing client 1 revoked its grant");
	pid = fork();
	if (pid == 0) {
		*db = n;	/* must fault: the page was taken back */
		_exit(0);
	}
	waitpid(pid, &status, 0);
	CHECK(WIFSIGNALED(status), "a store to the revoked doorbell faults "
	    "(%s)", WIFSIGNALED(status) ? strsignal(WTERMSIG(status)) :
	    "it did not");
	if (map != MAP_FAILED)
		munmap(map, m1.doorbell_size);
	CHECK(queue_map(fd2, vm2, ring2, ATRIUM_GPU_ENGINE_COMPUTE, &m2) == 0,
	    "client 2 maps the compute queue once client 1 is gone");
	CHECK(queue_map(fd2, vm2, ring2, ATRIUM_GPU_ENGINE_GFX, &mg) == 0 &&
	    mg.doorbell_mmap_offset != m2.doorbell_mmap_offset,
	    "gfx and compute doorbells stay on separate pages");
	close(ring1);
	close(res1);
	close(vm1);
	close(ring2);
	close(vm2);
	close(fd2);	/* gives both queues back */
}

/* Break 4: the kernel bounds a scanout by the BO it was given. */
static void
break4(void)
{
	struct atrium_display_config cfg;
	struct atrium_display_commit c;
	int fd, dfd, small, err;

	printf("Break 4: scanout past the end of a BO\n");
	fd = xopen(GPU);
	dfd = open(DISPLAY, O_RDWR);
	if (dfd < 0) {
		printf("  skip (no %s: %s)\n", DISPLAY, strerror(errno));
		close(fd);
		return;
	}
	memset(&cfg, 0, sizeof(cfg));
	cfg.connector_type = 2;	/* HDMI 2.1 */
	cfg.plug_mode = 0;	/* VGA 640x480 */
	ioctl(dfd, ATRIUM_DISPLAY_IOC_CONFIG, &cfg);
	small = bo_new(fd, -1, 4096, ATRIUM_GPU_BO_VRAM, NULL);

	memset(&c, 0, sizeof(c));
	c.struct_size = sizeof(c);
	c.flags = ATRIUM_DISPLAY_COMMIT_MODESET;
	c.format = ATRIUM_DISPLAY_FORMAT_XRGB8888;
	c.bo_fd = small;
	c.in_fence_fd = -1;
	c.out_fence_fd = -1;
	err = ioctl(dfd, ATRIUM_DISPLAY_IOC_COMMIT, &c) == 0 ? 0 : errno;
	CHECK(err == EINVAL, "MODESET of a 4 KiB BO as a 640x480 scanout: %s "
	    "(want EINVAL from the kernel, not a referee fault)",
	    err ? strerror(err) : "accepted");
	c.pitch = 256;	/* a row shorter than 640 pixels */
	err = ioctl(dfd, ATRIUM_DISPLAY_IOC_COMMIT, &c) == 0 ? 0 : errno;
	CHECK(err == EINVAL, "a pitch shorter than the mode's row is refused");
	cfg.plug_mode = 3;	/* back to the power-on desktop monitor */
	ioctl(dfd, ATRIUM_DISPLAY_IOC_CONFIG, &cfg);
	close(small);
	close(dfd);
	close(fd);
}

/* Break 5: device-wide control is not on the client node. */
static void
break5(void)
{
	struct atrium_gpu_sched s;
	struct atrium_gpu_powergate pg;
	struct stat st;
	int fd, status;
	pid_t pid;

	printf("Break 5: unprivileged reset, scheduler and power control\n");
	fd = xopen(GPU);
	memset(&s, 0, sizeof(s));
	memset(&pg, 0, sizeof(pg));
	s.op = 2;
	CHECK(ioctl(fd, ATRIUM_GPU_IOC_GPU_RESET, NULL) != 0 && errno == EPERM,
	    "GPU_RESET on the client node: EPERM");
	CHECK(ioctl(fd, ATRIUM_GPU_IOC_SCHED, &s) != 0 && errno == EPERM,
	    "SCHED on the client node: EPERM");
	CHECK(ioctl(fd, ATRIUM_GPU_IOC_POWERGATE, &pg) != 0 && errno == EPERM,
	    "POWERGATE on the client node: EPERM");
	close(fd);
	CHECK(stat(GPUCTL, &st) == 0 && st.st_uid == 0 &&
	    (st.st_mode & 0777) == 0600, "%s exists, root-only 0600", GPUCTL);
	pid = fork();
	if (pid == 0) {
		if (setgid(65534) != 0 || setuid(65534) != 0)
			_exit(3);
		_exit(open(GPUCTL, O_RDWR) < 0 && errno == EACCES ? 0 : 1);
	}
	waitpid(pid, &status, 0);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
	    "a non-root process cannot open the control node");
	fd = xopen(GPUCTL);
	CHECK(ioctl(fd, ATRIUM_GPU_IOC_SCHED, &s) == 0,
	    "SCHED works on the control node");
	close(fd);
}

/* ---------------------------------------------------------------------------
 * Part C: completion attribution and the hang watchdog (review §A2.2, §A2.6).
 */
static int
syncobj_new(int fd)
{
	struct atrium_gpu_syncobj_create c;

	memset(&c, 0, sizeof(c));
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_CREATE, &c) != 0) {
		perror("SYNCOBJ_CREATE");
		exit(2);
	}
	return ((int)c.out_fd);
}

static uint64_t
syncobj_value(int fd, int so)
{
	struct atrium_gpu_syncobj_op o;

	memset(&o, 0, sizeof(o));
	o.syncobj_fd = so;
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_QUERY, &o) != 0)
		return (~0ull);
	return (o.value);
}

static int
syncobj_wait(int fd, int so, uint64_t value, uint32_t ms)
{
	struct atrium_gpu_syncobj_wait w;

	memset(&w, 0, sizeof(w));
	w.syncobj_fd = so;
	w.value = value;
	w.timeout_ms = ms;
	return (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_WAIT, &w) == 0 ? 0 : errno);
}

/* RELEASE_MEM(fence_va) = value, with an end-of-pipe interrupt. */
static int
emit_eop(uint32_t *r, uint64_t fence_va, uint32_t value)
{
	r[0] = type3(IT_RELEASE_MEM, 6);
	r[1] = (5u << 8);
	r[2] = (2u << 29) | (2u << 24);
	r[3] = (uint32_t)(fence_va & 0xffffffff);
	r[4] = (uint32_t)(fence_va >> 32);
	r[5] = value;
	r[6] = 0;
	return (7);
}

static int
hang_ms(int v)
{
	int old = 0;
	size_t len = sizeof(old);

	if (sysctlbyname("dev.atrium_gpu_amd.0.isolation.hang_ms", &old, &len,
	    v > 0 ? &v : NULL, v > 0 ? sizeof(v) : 0) != 0) {
		perror("sysctl hang_ms");
		exit(2);
	}
	return (old);
}

static void
part_c(void)
{
	struct atrium_gpu_submit s;
	struct kevent kev;
	struct timespec ts = { 1, 0 };
	uint64_t hangs0, failed0, word_va, ring_va, fence_va, va;
	uint32_t r[32], zero = 0;
	int fda, fdb, vma, vmb, word, ring, fence, so, ringb, fenceb, resb;
	int kq, n, err, old;
	double t0, dt;

	printf("Part C: completion attribution and the hang watchdog\n");
	old = hang_ms(500);
	hangs0 = iso("hangs");
	failed0 = iso("fences_failed");

	/* A: a compute ring parked on a word nobody writes, owing syncobj 1. */
	fda = xopen(GPU);
	vma = vm_create(fda);
	so = syncobj_new(fda);
	word = bo_new(fda, vma, 4096, 0, &word_va);
	fence = bo_new(fda, vma, 4096, 0, &fence_va);
	ring = bo_new(fda, vma, 4096, 0, &ring_va);
	bo_write(fda, word, &zero, sizeof(zero));
	n = emit_wait(r, word_va);
	n += emit_eop(r + n, fence_va, 0xa);
	bo_write(fda, ring, r, n * 4);
	memset(&s, 0, sizeof(s));
	s.vm_fd = vma;
	s.ring_fd = ring;
	s.n_dwords = n;
	s.engine = ATRIUM_GPU_ENGINE_COMPUTE;
	s.signal_syncobj_fd = so;
	s.signal_value = 1;
	CHECK(ioctl(fda, ATRIUM_GPU_IOC_SUBMIT, &s) == 0,
	    "A's compute ring parked, owing its syncobj value 1");

	/* B: an end-of-pipe on gfx must not retire A's compute completion. */
	fdb = xopen(GPU);
	vmb = vm_create(fdb);
	ringb = bo_new(fdb, vmb, 4096, 0, &va);
	fenceb = bo_new(fdb, vmb, 4096, 0, &fence_va);
	n = emit_eop(r, fence_va, 0xb);
	bo_write(fdb, ringb, r, n * 4);
	err = submit(fdb, vmb, ringb, n, ATRIUM_GPU_ENGINE_GFX);
	CHECK(err == 0 && bo_read32(fdb, fenceb) == 0xb,
	    "B's gfx ring raised its end-of-pipe");
	CHECK(syncobj_value(fda, so) == 0,
	    "A2.2: B's gfx EOP did not signal A's compute syncobj");

	/* B wants compute: the watchdog stops A's hung ring, B gets in. */
	resb = bo_new(fdb, vmb, 4096, 0, &va);
	n = emit_write(r, va, 0xb0b);
	bo_write(fdb, ringb, r, n * 4);
	t0 = now_s();
	err = submit(fdb, vmb, ringb, n, ATRIUM_GPU_ENGINE_COMPUTE);
	dt = now_s() - t0;
	CHECK(err == 0 && bo_read32(fdb, resb) == 0xb0b,
	    "B's compute submit ran after %.2f s (hang_ms 500; %s)", dt,
	    strerror(err));
	CHECK(iso("hangs") == hangs0 + 1, "counted as one hang");

	/* A's waiter learns its work is gone instead of timing out. */
	t0 = now_s();
	err = syncobj_wait(fda, so, 1, 3000);
	dt = now_s() - t0;
	CHECK(err == EIO && dt < 0.5,
	    "A's SYNCOBJ_WAIT returns EIO at once (%s, %.2f s)",
	    strerror(err), dt);
	CHECK(iso("fences_failed") == failed0 + 1,
	    "counted as one failed completion");
	kq = kqueue();
	EV_SET(&kev, so, EVFILT_READ, EV_ADD, 0, 1, NULL);
	n = kevent(kq, &kev, 1, &kev, 1, &ts);
	CHECK(n == 1 && (kev.flags & EV_EOF) != 0 && kev.fflags == EIO,
	    "kqueue reports EV_EOF with EIO on A's syncobj");
	close(kq);
	CHECK(bo_read32(fda, fence) == 0, "A's stopped ring never ran its EOP");

	/* A's own next submit runs normally. */
	close(so);
	so = syncobj_new(fda);
	{
		uint64_t fva;
		int f2 = bo_new(fda, vma, 4096, 0, &fva);

		n = emit_eop(r, fva, 0xc);
		bo_write(fda, ring, r, n * 4);
		s.n_dwords = n;
		s.signal_syncobj_fd = so;
		s.signal_value = 7;
		err = ioctl(fda, ATRIUM_GPU_IOC_SUBMIT, &s) == 0 ? 0 : errno;
		CHECK(err == 0 && syncobj_wait(fda, so, 7, 1000) == 0 &&
		    bo_read32(fda, f2) == 0xc,
		    "A's next compute submit signals its new syncobj");
		close(f2);
	}

	close(so);
	close(word);
	close(fence);
	close(ring);
	close(vma);
	close(fda);
	close(ringb);
	close(fenceb);
	close(resb);
	close(vmb);
	close(fdb);
	hang_ms(old);
}

/*
 * Part D: one VM's page fault must not stop another VM's work (F6 G1). A
 * hostile app's shader or packet stream will touch unmapped VAs; if that
 * wedges the whole engine until a reset, any app can stall every other.
 */
static int
submit_so(int fd, int vm, int ring, uint32_t n, uint32_t engine, int so,
    uint64_t value)
{
	struct atrium_gpu_submit s;

	memset(&s, 0, sizeof(s));
	s.vm_fd = vm;
	s.ring_fd = ring;
	s.n_dwords = n;
	s.engine = engine;
	s.signal_syncobj_fd = so;
	s.signal_value = value;
	return (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) == 0 ? 0 : errno);
}

static void
part_d(void)
{
	struct atrium_gpu_vm_bind2 b2;
	uint64_t va, rva, fva, faults0, ro_va;
	uint32_t r[32], v;
	int fda, fdb, vma, vmb, ringa, ringb, resb, fence, ro, so, n, err;

	printf("Part D: faults cost only the VM that made them (F6 G1)\n");
	fda = xopen(GPU);
	vma = vm_create(fda);
	ringa = bo_new(fda, vma, 4096, 0, &rva);
	fence = bo_new(fda, vma, 4096, 0, &fva);
	so = syncobj_new(fda);
	faults0 = iso("vm_faults");
	/* A: a store far outside anything bound in A's VM, then an EOP. */
	n = emit_write(r, 0x7f0000000000ull, 0xbad);
	n += emit_eop(r + n, fva, 1);
	bo_write(fda, ringa, r, n * 4);
	err = submit_so(fda, vma, ringa, n, ATRIUM_GPU_ENGINE_COMPUTE, so, 1);
	CHECK(err == 0 && syncobj_wait(fda, so, 1, 3000) == EIO,
	    "A's faulting work completes with EIO, not success");
	CHECK(iso("vm_faults") > faults0, "the fault was reported (%llu)",
	    (unsigned long long)(iso("vm_faults") - faults0));

	fdb = xopen(GPU);
	vmb = vm_create(fdb);
	ringb = bo_new(fdb, vmb, 4096, 0, &rva);
	resb = bo_new(fdb, vmb, 4096, 0, &va);
	n = emit_write(r, va, 0xd0d0);
	bo_write(fdb, ringb, r, n * 4);
	err = submit(fdb, vmb, ringb, n, ATRIUM_GPU_ENGINE_GFX);
	CHECK(err == 0 && bo_read32(fdb, resb) == 0xd0d0,
	    "B's gfx work runs after A faulted on compute (%s, 0x%x)",
	    strerror(err), bo_read32(fdb, resb));
	n = emit_write(r, va, 0xc0c0);
	bo_write(fdb, ringb, r, n * 4);
	err = submit(fdb, vmb, ringb, n, ATRIUM_GPU_ENGINE_COMPUTE);
	CHECK(err == 0 && bo_read32(fdb, resb) == 0xc0c0,
	    "B's compute work runs on the queue A faulted on (%s, 0x%x)",
	    strerror(err), bo_read32(fdb, resb));

	/* A read-only binding (VM_BIND2): GPU stores to it fault. */
	ro = bo_new(fda, -1, 4096, 0, &ro_va);
	v = SENTINEL;
	bo_write(fda, ro, &v, 4);
	memset(&b2, 0, sizeof(b2));
	b2.vm_fd = vma;
	b2.bo_fd = ro;
	b2.flags = ATRIUM_GPU_BIND_READONLY;
	CHECK(ioctl(fda, ATRIUM_GPU_IOC_VM_BIND2, &b2) == 0,
	    "VM_BIND2 maps a BO read-only at 0x%llx", (unsigned long long)b2.va);
	b2.flags = 0x80;
	CHECK(ioctl(fda, ATRIUM_GPU_IOC_VM_BIND2, &b2) != 0 && errno == EINVAL,
	    "an unknown bind flag is refused");
	close(so);
	so = syncobj_new(fda);
	faults0 = iso("vm_faults");
	n = emit_write(r, b2.va, 0xbad);
	n += emit_eop(r + n, fva, 2);
	bo_write(fda, ringa, r, n * 4);
	err = submit_so(fda, vma, ringa, n, ATRIUM_GPU_ENGINE_COMPUTE, so, 1);
	CHECK(err == 0 && syncobj_wait(fda, so, 1, 3000) == EIO &&
	    bo_read32(fda, ro) == SENTINEL && iso("vm_faults") > faults0,
	    "a GPU store to it faults, leaves it intact, and fails the work");

	close(so);
	close(ro);
	close(fence);
	close(ringb);
	close(resb);
	close(vmb);
	close(fdb);
	close(ringa);
	close(vma);
	close(fda);
}

/*
 * Display lease (ISOLATION.md "Display lease"): only the jail and uid named in
 * dev.atrium_gpu_amd_display.0.lease may open the display, root on the host
 * included; with no lease only host processes may. This runs on the host as
 * root, so it checks the host half and the sysctl; the jail half (another
 * jail refused, frescod's admitted) is run by hand in the VM.
 */
static void
part_e(void)
{
	const char *oid = "dev.atrium_gpu_amd_display.0.lease";
	char saved[300] = "", now[300];
	size_t len = sizeof(saved);
	int fd;

	printf("Part E: display lease\n");
	if (sysctlbyname(oid, saved, &len, NULL, 0) != 0) {
		CHECK(0, "%s readable (%s)", oid, strerror(errno));
		return;
	}
	if (saved[0] != '\0') {
		printf("  skip leased to %s (frescod running?)\n", saved);
		return;
	}
	CHECK(sysctlbyname(oid, NULL, NULL, "no-such-jail:65010",
	    sizeof("no-such-jail:65010")) == 0, "root can set a lease");
	fd = open(DISPLAY, O_RDWR);
	CHECK(fd < 0 && errno == EPERM,
	    "leased elsewhere: host root's open is refused (EPERM)");
	if (fd >= 0)
		close(fd);
	CHECK(sysctlbyname(oid, NULL, NULL, "nocolon", sizeof("nocolon")) != 0 &&
	    errno == EINVAL, "a malformed lease is refused (EINVAL)");
	CHECK(sysctlbyname(oid, NULL, NULL, "x:notanumber",
	    sizeof("x:notanumber")) != 0 && errno == EINVAL,
	    "a lease with a bad uid is refused (EINVAL)");
	len = sizeof(now);
	CHECK(sysctlbyname(oid, now, &len, NULL, 0) == 0 &&
	    strcmp(now, "no-such-jail:65010") == 0,
	    "a refused write leaves the lease as it was");
	CHECK(sysctlbyname(oid, NULL, NULL, "", 1) == 0, "root can clear it");
	fd = open(DISPLAY, O_RDWR);
	CHECK(fd >= 0, "no lease: host root opens the display again");
	if (fd >= 0)
		close(fd);
}

static void
attest(void)
{
	char build[128] = "unknown";
	size_t len = sizeof(build);
	int rev = 0;
	size_t rlen = sizeof(rev);
	time_t t = time(NULL);
	char date[16];

	sysctlbyname("dev.atrium_gpu_amd.0.build_id", build, &len, NULL, 0);
	sysctlbyname("dev.atrium_gpu_amd.0.abi_rev", &rev, &rlen, NULL, 0);
	strftime(date, sizeof(date), "%Y-%m-%d", gmtime(&t));
	printf("\n[gpu_drivers.attested.atrium-gpu-amd]\n"
	    "status                  = \"experimental\"\n"
	    "isolation_test_passed   = %s\n"
	    "isolation_test_date     = \"%s\"\n"
	    "isolation_test_commit   = \"%s\"\n"
	    "abi_rev                 = %d\n"
	    "notes = \"test_gpu_isolation on gpusim; shared queues until per-VM "
	    "MES queues\"\n",
	    failures == 0 && strstr(build, "dirty") == NULL ? "true" : "false",
	    date, build, rev);
}

int
main(int argc, char **argv)
{
	signal(SIGPIPE, SIG_IGN);
	part_a();
	break5();
	break3();
	break1();
	break2();
	break4();
	part_c();
	part_d();
	part_e();
	if (argc > 1 && strcmp(argv[1], "--attest") == 0)
		attest();
	printf(failures == 0 ? "ALL OK\n" : "%d FAILURES\n", failures);
	return (failures == 0 ? 0 : 1);
}
