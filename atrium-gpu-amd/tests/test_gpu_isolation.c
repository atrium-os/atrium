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
 * All work goes through queue fds (QUEUE_CREATE on a VM, QUEUE_SUBMIT of a blob
 * VA); the old ring SUBMIT is gone. On gpusim a queue fd runs on the shared
 * engine queues, so breaks 1-3 still test their sharing rules there. On
 * silicon (no GRAPHICS feature) every queue fd is its own MES user queue in its
 * own VMID, gfx work runs on compute, and the parts that test the shared-queue
 * model, QUEUE_MAP, the display or the gpusim watchdog print SKIP; their
 * silicon counterparts are in si_client_test.c (tests 7-10).
 *
 * Run as root in the guest with the three modules loaded:
 *   cc -Wall -o /tmp/test_gpu_isolation tests/test_gpu_isolation.c
 *   /tmp/test_gpu_isolation [--attest]
 * --attest prints the jaild.policy.toml stanza for this build on success.
 * Part F (VRAM scrubbing) needs gpusim with DMA_DATA fill (see gpusim pm4.rs).
 * Part G (per-uid BO caps) changes and restores the isolation.*_limit_pct
 * sysctls.
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
/* DMA_DATA control word for a memory-to-memory copy (SRC/DST_SEL L2): what
 * silicon needs; gpusim treats any non-DATA source as a copy. */
#define DMA_CTL_COPY	0xe0300000u

static int failures;
static int g_si;	/* silicon: no GRAPHICS feature (detect_si) */

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

/* A queue fd on `vm` for `engine`: gfx is gpusim only, so on silicon gfx work
 * runs on a compute queue. -1 with errno on failure. */
static int
queue_new(int vm, uint32_t engine)
{
	struct atrium_gpu_queue_create c;

	memset(&c, 0, sizeof(c));
	c.struct_size = sizeof(c);
	c.family_id = engine == ATRIUM_GPU_ENGINE_GFX && !g_si ?
	    ATRIUM_GPU_QUEUE_FAMILY_GFX : ATRIUM_GPU_QUEUE_FAMILY_COMPUTE;
	if (ioctl(vm, ATRIUM_GPU_IOC_QUEUE_CREATE, &c) != 0)
		return (-1);
	return ((int)c.out_queue_fd);
}

/* QUEUE_SUBMIT `n` dwords at `va`, signalling `so` (if >= 0) to `value`.
 * 0 or the errno. */
static int
queue_submit(int q, uint64_t va, uint32_t n, int so, uint64_t value)
{
	struct atrium_gpu_queue_submit s;
	struct atrium_gpu_syncobj_point pt;

	memset(&s, 0, sizeof(s));
	memset(&pt, 0, sizeof(pt));
	s.struct_size = sizeof(s);
	s.blob_va = va;
	s.blob_size = (uint64_t)n * 4;
	if (so >= 0) {
		pt.syncobj_fd = so;
		pt.value = value;
		s.out_dep_ptr = (uint64_t)(uintptr_t)&pt;
		s.out_dep_count = 1;
	}
	return (ioctl(q, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &s) == 0 ? 0 : errno);
}

/*
 * Queues of async submits (submit_so, park), closed at the end of each part
 * (qreap) or before a test closes the VM they run on: a queue fd holds its VM.
 * On silicon each is a MES user queue, and there are only a few.
 */
static int g_q[16];
static int g_nq;

static void
qreap(void)
{
	while (g_nq > 0)
		close(g_q[--g_nq]);
}

/* Submit on a queue of its own and return at once (the work may park),
 * signalling `so` (if >= 0) to `value`. 0 or the errno. */
static int
submit_so(int fd __unused, int vm, uint64_t va, uint32_t n, uint32_t engine,
    int so, uint64_t value)
{
	int q, err;

	if (g_nq == (int)(sizeof(g_q) / sizeof(g_q[0])))
		qreap();
	if ((q = queue_new(vm, engine)) < 0)
		return (errno);
	if ((err = queue_submit(q, va, n, so, value)) != 0) {
		close(q);
		return (err);
	}
	g_q[g_nq++] = q;
	return (0);
}

/*
 * Submit on a queue of its own and let the work finish: the old SUBMIT's
 * synchronous use. 0 or the errno. On silicon the kernel ends every blob
 * with its own end-of-pipe, so we wait (up to 5 s) on a syncobj. On gpusim
 * the blob is the ring and a syncobj signals only from an interrupting EOP
 * the ring itself carries, so, as with the old SUBMIT, we submit without
 * one: the engine queue runs it before the next submit or BO access.
 */
static int
submit(int fd, int vm, uint64_t va, uint32_t n, uint32_t engine)
{
	struct atrium_gpu_syncobj_wait w;
	int q, so, err;

	if (!g_si) {
		if ((q = queue_new(vm, engine)) < 0)
			return (errno);
		err = queue_submit(q, va, n, -1, 0);
		close(q);
		return (err);
	}
	so = syncobj_new(fd);
	if ((q = queue_new(vm, engine)) < 0)
		err = errno;
	else
		err = queue_submit(q, va, n, so, 1);
	if (err == 0) {
		memset(&w, 0, sizeof(w));
		w.syncobj_fd = so;
		w.value = 1;
		w.timeout_ms = 5000;
		if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_WAIT, &w) != 0)
			err = errno;
	}
	if (q >= 0)
		close(q);
	close(so);
	return (err);
}

/* Silicon or gpusim: silicon has no GRAPHICS feature. */
static void
detect_si(void)
{
	struct atrium_gpu_caps_query q;
	uint8_t buf[256];
	uint32_t feat = 0;
	size_t off = 0;
	int fd = xopen(GPU);

	memset(&q, 0, sizeof(q));
	q.caps_ptr = (uint64_t)(uintptr_t)buf;
	q.caps_size = sizeof(buf);
	if (ioctl(fd, ATRIUM_GPU_IOC_QUERY_CAPS, &q) != 0) {
		perror("QUERY_CAPS");
		exit(2);
	}
	while (off + sizeof(struct atrium_gpu_cap_record) <= q.caps_size) {
		struct atrium_gpu_cap_record *r =
		    (struct atrium_gpu_cap_record *)(buf + off);

		if (r->cap_id == ATRIUM_GPU_CAP_FEATURES_V1)
			memcpy(&feat, buf + off + sizeof(*r), sizeof(feat));
		off += sizeof(*r) + r->cap_size;
		off = (off + 3u) & ~(size_t)3u;
	}
	close(fd);
	g_si = !(feat & ATRIUM_GPU_FEAT_GRAPHICS);
	printf("%s (features 0x%x)\n", g_si ? "silicon: MES user queues, "
	    "gfx work on compute" : "gpusim: shared engine queues", feat);
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
	err = submit(fd, vm, va, 2, ATRIUM_GPU_ENGINE_COMPUTE);
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
	return (submit_so(fd, p->vm, ring_va, n, ATRIUM_GPU_ENGINE_GFX, -1, 0));
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
	r[1] = DMA_CTL_COPY;
	r[2] = (uint32_t)(va_a[2] & 0xffffffff);
	r[3] = (uint32_t)(va_a[2] >> 32);
	r[4] = (uint32_t)(va_b[1] & 0xffffffff);
	r[5] = (uint32_t)(va_b[1] >> 32);
	r[6] = 4;
	bo_write(fdb, b[0], r, 7 * 4);
	v = 0;
	bo_write(fdb, b[1], &v, 4);
	submit(fdb, vmb, va_b[0], 7, ATRIUM_GPU_ENGINE_GFX);
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
	uint64_t refused0, waits0, va, rvb;
	uint32_t r[8];
	double t0, dt;
	int fda, fdb, vmb, ringb, resb, err, n, i;

	printf("Break 1: shared-queue reprogramming under another VMID\n");
	fda = xopen(GPU);
	fdb = xopen(GPU);
	CHECK(park(&p, fda, 0, 0) == 0, "A's gfx ring submitted and parked");

	vmb = vm_create(fdb);
	ringb = bo_new(fdb, vmb, 4096, 0, &rvb);
	resb = bo_new(fdb, vmb, 4096, 0, &va);
	n = emit_write(r, va, 0xb0b);
	bo_write(fdb, ringb, r, n * 4);
	if (g_si) {
		/* Per-VM MES queues: nothing to share, so nothing to wait for. */
		t0 = now_s();
		err = submit(fdb, vmb, rvb, n, ATRIUM_GPU_ENGINE_GFX);
		dt = now_s() - t0;
		CHECK(err == 0 && bo_read32(fdb, resb) == 0xb0b && dt < 1.0,
		    "B's work runs at once on its own queue while A's is parked "
		    "(%s, %.3f s)", strerror(err), dt);
		CHECK(bo_read32(fda, p.res) == 0, "A's parked work has not run");
		unpark(&p);
		for (i = 0; i < 200 && bo_read32(fda, p.res) != RESULT; i++)
			usleep(10 * 1000);
		CHECK(bo_read32(fda, p.res) == RESULT,
		    "A's work ran to completion in A's VM once unblocked");
		goto out;
	}
	refused0 = iso("queue_refused");
	waits0 = iso("queue_waits");
	t0 = now_s();
	err = submit(fdb, vmb, rvb, n, ATRIUM_GPU_ENGINE_GFX);
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
	err = submit(fdb, vmb, rvb, n, ATRIUM_GPU_ENGINE_GFX);
	CHECK(err == 0 && bo_read32(fdb, resb) == 0xb0b,
	    "B's submit runs once A's work has drained");
out:
	qreap();
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
	uint64_t deferred0, reaped0, dequeues0, waits0, tva, va, rvd;
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
	qreap();	/* a queue fd holds its VM */
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
	qreap();
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
	ringd = bo_new(fdd, vmd, 4096, 0, &rvd);
	resd = bo_new(fdd, vmd, 4096, 0, &va);
	n = emit_write(r, va, 0xd00d);
	bo_write(fdd, ringd, r, n * 4);
	waits0 = iso("queue_waits");
	CHECK(submit(fdd, vmd, rvd, n, ATRIUM_GPU_ENGINE_GFX) == 0 &&
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
	uint64_t va, rv2, revokes0;
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
	ring2 = bo_new(fd2, vm2, 4096, 0, &rv2);
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
	CHECK(submit(fd2, vm2, rv2, n, ATRIUM_GPU_ENGINE_COMPUTE) == EBUSY,
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
	if (g_si)	/* silicon has no scheduler op yet (si_client.c) */
		CHECK(ioctl(fd, ATRIUM_GPU_IOC_SCHED, &s) != 0 &&
		    errno == EOPNOTSUPP, "SCHED on the control node reaches "
		    "the driver (EOPNOTSUPP on silicon, not EPERM)");
	else
		CHECK(ioctl(fd, ATRIUM_GPU_IOC_SCHED, &s) == 0,
		    "SCHED works on the control node");
	close(fd);
}

/* ---------------------------------------------------------------------------
 * Part C: completion attribution and the hang watchdog (review §A2.2, §A2.6).
 */
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
	struct kevent kev;
	struct timespec ts = { 1, 0 };
	uint64_t hangs0, failed0, word_va, ring_va, fence_va, va, rvb;
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
	CHECK(submit_so(fda, vma, ring_va, n, ATRIUM_GPU_ENGINE_COMPUTE, so,
	    1) == 0,
	    "A's compute ring parked, owing its syncobj value 1");

	/* B: an end-of-pipe on gfx must not retire A's compute completion. */
	fdb = xopen(GPU);
	vmb = vm_create(fdb);
	ringb = bo_new(fdb, vmb, 4096, 0, &rvb);
	fenceb = bo_new(fdb, vmb, 4096, 0, &fence_va);
	n = emit_eop(r, fence_va, 0xb);
	bo_write(fdb, ringb, r, n * 4);
	err = submit(fdb, vmb, rvb, n, ATRIUM_GPU_ENGINE_GFX);
	CHECK(err == 0 && bo_read32(fdb, fenceb) == 0xb,
	    "B's gfx ring raised its end-of-pipe");
	CHECK(syncobj_value(fda, so) == 0,
	    "A2.2: B's gfx EOP did not signal A's compute syncobj");

	/* B wants compute: the watchdog stops A's hung ring, B gets in. */
	resb = bo_new(fdb, vmb, 4096, 0, &va);
	n = emit_write(r, va, 0xb0b);
	bo_write(fdb, ringb, r, n * 4);
	t0 = now_s();
	err = submit(fdb, vmb, rvb, n, ATRIUM_GPU_ENGINE_COMPUTE);
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
		err = submit_so(fda, vma, ring_va, n,
		    ATRIUM_GPU_ENGINE_COMPUTE, so, 7);
		CHECK(err == 0 && syncobj_wait(fda, so, 7, 1000) == 0 &&
		    bo_read32(fda, f2) == 0xc,
		    "A's next compute submit signals its new syncobj");
		close(f2);
	}

	qreap();
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
static void
part_d(void)
{
	struct atrium_gpu_vm_bind2 b2;
	uint64_t va, rva, rvb, fva, faults0, ro_va;
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
	err = submit_so(fda, vma, rva, n, ATRIUM_GPU_ENGINE_COMPUTE, so, 1);
	CHECK(err == 0 && syncobj_wait(fda, so, 1, 3000) == EIO,
	    "A's faulting work completes with EIO, not success");
	CHECK(iso("vm_faults") > faults0, "the fault was reported (%llu)",
	    (unsigned long long)(iso("vm_faults") - faults0));

	fdb = xopen(GPU);
	vmb = vm_create(fdb);
	ringb = bo_new(fdb, vmb, 4096, 0, &rvb);
	resb = bo_new(fdb, vmb, 4096, 0, &va);
	n = emit_write(r, va, 0xd0d0);
	bo_write(fdb, ringb, r, n * 4);
	err = submit(fdb, vmb, rvb, n, ATRIUM_GPU_ENGINE_GFX);
	CHECK(err == 0 && bo_read32(fdb, resb) == 0xd0d0,
	    "B's gfx work runs after A faulted on compute (%s, 0x%x)",
	    strerror(err), bo_read32(fdb, resb));
	n = emit_write(r, va, 0xc0c0);
	bo_write(fdb, ringb, r, n * 4);
	err = submit(fdb, vmb, rvb, n, ATRIUM_GPU_ENGINE_COMPUTE);
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
	err = submit_so(fda, vma, rva, n, ATRIUM_GPU_ENGINE_COMPUTE, so, 1);
	CHECK(err == 0 && syncobj_wait(fda, so, 1, 3000) == EIO &&
	    bo_read32(fda, ro) == SENTINEL && iso("vm_faults") > faults0,
	    "a GPU store to it faults, leaves it intact, and fails the work");

	qreap();
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

/* ---------------------------------------------------------------------------
 * Part F: freed VRAM is scrubbed before anyone else gets it (vram.c, clear.c).
 */
#define VRAM	"dev.atrium_gpu_amd_pci.0.vram."

static uint64_t
vram(const char *name)
{
	char oid[128];
	uint64_t v = 0;
	size_t len = sizeof(v);

	snprintf(oid, sizeof(oid), VRAM "%s", name);
	if (sysctlbyname(oid, &v, &len, NULL, 0) != 0) {
		fprintf(stderr, "sysctl %s: %s\n", oid, strerror(errno));
		exit(2);
	}
	return (v);
}

static int
bo_read(int fd, int bo, void *dst, uint64_t len)
{
	struct atrium_gpu_bo_xfer x;

	memset(&x, 0, sizeof(x));
	x.bo_fd = bo;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)dst;
	return (ioctl(fd, ATRIUM_GPU_IOC_BO_READ, &x) == 0 ? 0 : errno);
}

/* DMA_DATA copy src_va -> dst_va (bytes) on gfx (compute on silicon);
 * returns the submit errno. */
static int
gpu_copy(int fd, int vm, uint64_t src_va, uint64_t dst_va, uint32_t bytes)
{
	uint32_t r[7];
	uint64_t ring_va;
	int ring = bo_new(fd, vm, 4096, 0, &ring_va), err;

	r[0] = type3(IT_DMA_DATA, 6);
	r[1] = DMA_CTL_COPY;
	r[2] = (uint32_t)(src_va & 0xffffffff);
	r[3] = (uint32_t)(src_va >> 32);
	r[4] = (uint32_t)(dst_va & 0xffffffff);
	r[5] = (uint32_t)(dst_va >> 32);
	r[6] = bytes;
	bo_write(fd, ring, r, sizeof(r));
	err = submit(fd, vm, ring_va, 7, ATRIUM_GPU_ENGINE_GFX);
	close(ring);
	return (err);
}

/* Wait (up to `ms`) for every freed VRAM range to be scrubbed. */
static int
vram_settle(int ms)
{
	double end = now_s() + ms / 1000.0;

	while (vram("dirty") != 0) {
		if (now_s() > end)
			return (0);
		usleep(10 * 1000);
	}
	return (1);
}

#define F_BYTES	(64 * 1024)

static void
part_f(void)
{
	static uint32_t buf[F_BYTES / 4];
	uint64_t used0, scrubbed0, enomem0, total, sva, vva, i, cycles;
	int fda, fdb, vma, vmb, stage, v, w, ok, n, err;
	double t0;

	printf("Part F: freed VRAM is scrubbed before reuse\n");
	CHECK(vram("clear_engine") == 1, "a VRAM clear engine is registered");
	total = vram("total");
	printf("       (VRAM %llu MiB, %llu MiB CPU-visible)\n",
	    (unsigned long long)(total >> 20),
	    (unsigned long long)(vram("visible") >> 20));
	vram_settle(5000);
	used0 = vram("used");
	scrubbed0 = vram("scrubbed");
	enomem0 = vram("enomem");

	/* A fills a VRAM BO with the sentinel (GPU copy from System). */
	fda = xopen(GPU);
	vma = vm_create(fda);
	stage = bo_new(fda, vma, F_BYTES, 0, &sva);
	v = bo_new(fda, vma, F_BYTES, ATRIUM_GPU_BO_VRAM, &vva);
	for (i = 0; i < F_BYTES / 4; i++)
		buf[i] = SENTINEL;
	bo_write(fda, stage, buf, F_BYTES);
	err = gpu_copy(fda, vma, sva, vva, F_BYTES);
	memset(buf, 0, sizeof(buf));
	bo_write(fda, stage, buf, F_BYTES);
	err = err != 0 ? err : gpu_copy(fda, vma, vva, sva, F_BYTES);
	bo_read(fda, stage, buf, F_BYTES);
	CHECK(err == 0 && buf[0] == SENTINEL && buf[F_BYTES / 4 - 1] == SENTINEL,
	    "A's VRAM BO holds the sentinel (round trip through the GPU)");
	/* On silicon a VM's page tables come from VRAM too (gmc12.c
	 * AMD_VM12_4L_HEAP), so A's bindings add a few table pages. */
	CHECK(vram("used") >= used0 + F_BYTES &&
	    vram("used") <= used0 + F_BYTES + (1ULL << 20),
	    "VRAM in use grew by A's BO (and at most 1 MiB of its page tables: "
	    "+%llu KiB)", (unsigned long long)((vram("used") - used0) >> 10));

	/* A frees it: it is dirty, then scrubbed by the GPU. */
	close(v);
	close(stage);
	close(vma);
	close(fda);
	ok = vram_settle(5000);
	CHECK(ok && vram("used") == used0 &&
	    vram("scrubbed") >= scrubbed0 + F_BYTES,
	    "A's freed VRAM was scrubbed (%llu KiB) and is free again",
	    (unsigned long long)((vram("scrubbed") - scrubbed0) >> 10));

	/* B takes VRAM of the same size: it must read all zero. */
	fdb = xopen(GPU);
	vmb = vm_create(fdb);
	stage = bo_new(fdb, vmb, F_BYTES, 0, &sva);
	w = bo_new(fdb, vmb, F_BYTES, ATRIUM_GPU_BO_VRAM, &vva);
	for (i = 0; i < F_BYTES / 4; i++)
		buf[i] = 0xffffffffu;
	bo_write(fdb, stage, buf, F_BYTES);
	err = gpu_copy(fdb, vmb, vva, sva, F_BYTES);
	bo_read(fdb, stage, buf, F_BYTES);
	for (i = 0, n = 0; i < F_BYTES / 4; i++)
		if (buf[i] != 0)
			n++;
	CHECK(err == 0 && n == 0, "B's new VRAM BO reads zero (%d non-zero "
	    "dwords, none may be A's sentinel)", n);
	close(w);
	close(stage);

	/* Churn: allocate and free more VRAM than the device has, 64 MiB at a
	 * time. A bump allocator runs dry; a reusing one never does. */
	t0 = now_s();
	cycles = total / (64ULL << 20) + 16;
	for (i = 0, n = 0; i < cycles; i++) {
		struct atrium_gpu_bo_alloc a;

		memset(&a, 0, sizeof(a));
		a.size = 64ULL << 20;
		a.flags = ATRIUM_GPU_BO_VRAM;
		if (ioctl(fdb, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0) {
			n++;
			break;
		}
		close((int)a.bo_fd);
	}
	ok = vram_settle(30000);	/* the last scrub can lag under TCG */
	CHECK(n == 0 && ok, "allocated and freed %llu MiB of VRAM (%.1fx the "
	    "device) in %.1f s with no ENOMEM",
	    (unsigned long long)(i * 64), (double)(i * (64ULL << 20)) / total,
	    now_s() - t0);
	/* B's VM goes too (its page tables are VRAM on silicon). */
	close(vmb);
	close(fdb);
	ok = vram_settle(5000);
	CHECK(ok && vram("used") == used0 && vram("enomem") == enomem0 &&
	    vram("scrub_failed") == 0,
	    "VRAM in use is back where it started; no scrub failed");
}

/* ---------------------------------------------------------------------------
 * Part G: per-uid BO caps (kmod audit 2026-11-07, G2). One uid's BOs may hold
 * at most vram_limit_pct of VRAM and sys_limit_pct of RAM; past that BO_ALLOC
 * fails with EDQUOT, while another uid still allocates, and freeing gives the
 * charge back.
 */
#define G_BO	(1ULL << 20)

static void
iso_set_int(const char *name, int v)
{
	char oid[128];

	snprintf(oid, sizeof(oid), ISO "%s", name);
	if (sysctlbyname(oid, NULL, NULL, &v, sizeof(v)) != 0) {
		fprintf(stderr, "sysctl %s=%d: %s\n", oid, v, strerror(errno));
		exit(2);
	}
}

static int
bo_try(int fd, uint32_t flags, int *bo)
{
	struct atrium_gpu_bo_alloc a;

	memset(&a, 0, sizeof(a));
	a.size = G_BO;
	a.flags = flags;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0)
		return (errno);
	*bo = (int)a.bo_fd;
	return (0);
}

static void
part_g_one(const char *what, const char *knob, uint32_t flags, uint64_t total,
    uint64_t held)
{
	static int bos[4096];
	uint64_t refused0, limit;
	int old, pct, fd, n, err, i, bo, status;
	pid_t pid;

	old = (int)iso(knob);
	refused0 = iso("quota_refused");
	/* A cap of a few dozen MiB at most, so the test stays quick. */
	pct = (int)((64ULL << 20) * 100 / total);
	pct = pct < 1 ? 1 : pct > 50 ? 50 : pct;
	limit = total / 100 * (uint64_t)pct;
	iso_set_int(knob, pct);
	CHECK(iso(knob) == (uint64_t)pct, "%s set to %d%% (%llu MiB)", knob,
	    pct, (unsigned long long)(limit >> 20));

	fd = xopen(GPU);
	for (n = 0, err = 0; n < (int)(sizeof(bos) / sizeof(bos[0])); n++)
		if ((err = bo_try(fd, flags, &bos[n])) != 0)
			break;
	/* `held`: what root may hold already (the display keeps the last
	 * scanout BO it was given; it stays charged to its allocator). */
	CHECK(err == EDQUOT && (uint64_t)n * G_BO <= limit &&
	    (uint64_t)(n + 1) * G_BO + held > limit,
	    "%s: %d one-MiB BOs, then EDQUOT (%s)", what, n, strerror(err));
	CHECK(iso("quota_refused") == refused0 + 1, "counted as one refusal");

	/* Another uid has its own budget. The node is root-only, so the child
	 * opens it first and then becomes nobody: BOs are charged to the uid
	 * that allocates them, not to whoever opened the node. */
	pid = fork();
	if (pid == 0) {
		int cfd = open(GPU, O_RDWR);

		if (cfd < 0 || setgid(65534) != 0 || setuid(65534) != 0)
			_exit(3);
		_exit(bo_try(cfd, flags, &bo) == 0 ? 0 : 1);
	}
	waitpid(pid, &status, 0);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
	    "%s: another uid can still allocate", what);

	for (i = 0; i < n; i++)
		close(bos[i]);
	err = bo_try(fd, flags, &bo);
	CHECK(err == 0, "%s: freeing gives the charge back (%s)", what,
	    strerror(err));
	if (err == 0)
		close(bo);
	close(fd);
	iso_set_int(knob, old);
}

static void
part_g(void)
{
	uint64_t ram = 0;
	size_t len = sizeof(ram);

	printf("Part G: per-uid BO caps\n");
	CHECK(iso("vram_limit_pct") == 75 && iso("sys_limit_pct") == 50,
	    "defaults: 75%% of VRAM, 50%% of RAM per uid");
	vram_settle(5000);
	part_g_one("VRAM", "vram_limit_pct", ATRIUM_GPU_BO_VRAM, vram("total"),
	    vram("used"));
	sysctlbyname("hw.physmem", &ram, &len, NULL, 0);
	part_g_one("System", "sys_limit_pct", 0, ram, 0);
	vram_settle(5000);
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
	    "notes = \"test_gpu_isolation on %s\"\n",
	    failures == 0 && strstr(build, "dirty") == NULL ? "true" : "false",
	    date, build, rev, g_si ? "silicon; per-VM MES user queues" :
	    "gpusim; shared engine queues");
}

/* The card was reset and every client lost: a fresh VM_CREATE fails ENXIO. */
static int
dev_lost(void)
{
	struct atrium_gpu_vm_create v;
	int fd, lost;

	fd = open(GPU, O_RDWR);
	if (fd < 0)
		return (errno == ENXIO);
	memset(&v, 0, sizeof(v));
	lost = ioctl(fd, ATRIUM_GPU_IOC_VM_CREATE, &v) != 0 && errno == ENXIO;
	if (!lost)
		close((int)v.out_fd);
	close(fd);
	return (lost);
}

int
main(int argc, char **argv)
{
	signal(SIGPIPE, SIG_IGN);
	detect_si();
#define RUN(f)		do {						\
	f();								\
	qreap();							\
	if (dev_lost()) {						\
		printf("device lost (ENXIO): stopping, the rest would "	\
		    "only cascade\n");					\
		printf("%d FAILURES\n", failures + 1);			\
		return (1);						\
	}								\
} while (0)
#define GPUSIM(f, why)	do {						\
	if (g_si)							\
		printf(#f " SKIP: gpusim only (%s)\n", why);		\
	else								\
		RUN(f);							\
} while (0)
	RUN(part_a);
	RUN(break5);
	GPUSIM(break3, "QUEUE_MAP; silicon doorbells: si_client_test 9");
	RUN(break1);
	GPUSIM(break2, "shared-queue deferral; silicon free rule: "
	    "si_client_test 10");
	GPUSIM(break4, "display");
	GPUSIM(part_c, "shared-queue watchdog; silicon hangs: si_client_test 8");
	GPUSIM(part_d, "shared-queue faults; silicon faults: si_client_test 7");
	GPUSIM(part_e, "display");
	RUN(part_f);
	RUN(part_g);
	if (argc > 1 && strcmp(argv[1], "--attest") == 0)
		attest();
	printf(failures == 0 ? "ALL OK\n" : "%d FAILURES\n", failures);
	return (failures == 0 ? 0 : 1);
}
