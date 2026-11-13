/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * si_client_test.c — stage 5c-2a on the R9700 (docs/spec/r9700-bare-metal.md
 * §4.5): the client node on silicon, driven from userspace through the same
 * ABI gpusim's tests use (atrium_gpu_amd_abi.h). Cross-built static for
 * FreeBSD amd64 (scripts/r9700/build-si-client-test.sh); run as root with
 * the kmods attached (kenv hw.atrium_gpu.client=1).
 *
 *   1 dispatch   one VM, write_42 (oracle-built, IB patched with the VAs the
 *                kernel chose), completion through kqueue on a syncobj.
 *   2 two VMs    two fresh VMs bind at the SAME VAs onto their own pages; both
 *                run, each sees only its own 64 x 42.
 *   3 fault      a VM whose IB writes to a VA it does not map, where another
 *                VM's output lives: its syncobj fails with EIO (kqueue EV_EOF),
 *                and the other VM's page keeps its poison.
 *   4 recovery   the faulting VM, then the others, run clean jobs afterwards.
 *   5 refusals   graphics engine, VRAM BOs and user-mode queues are not there
 *                yet on silicon: EOPNOTSUPP.
 *
 * Prints one PASS/FAIL line per check and a summary; exit 0 only if all
 * passed.
 */
#include "../atrium_gpu_amd_abi.h"
#include "si_w42.h"

#include <sys/types.h>

#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <unistd.h>

#define DEV		"/dev/atrium-gpu0"
#define POISON_A	0xa5a5a5a5u
#define POISON_B	0x5b5b5b5bu

static int fd;
static int npass, nfail;

static void
check(int ok, const char *what, const char *fmt, ...)
	__attribute__((format(printf, 3, 4)));

static void
check(int ok, const char *what, const char *fmt, ...)
{
	va_list ap;

	printf("%s: %s", ok ? "PASS" : "FAIL", what);
	if (fmt != NULL) {
		printf(" (");
		va_start(ap, fmt);
		vprintf(fmt, ap);
		va_end(ap);
		printf(")");
	}
	printf("\n");
	if (ok)
		npass++;
	else
		nfail++;
}

static int
vm_create(void)
{
	struct atrium_gpu_vm_create v;

	memset(&v, 0, sizeof(v));
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_CREATE, &v) != 0)
		return (-1);
	return ((int)v.out_fd);
}

static int
bo_alloc(uint64_t size, uint32_t flags)
{
	struct atrium_gpu_bo_alloc a;

	memset(&a, 0, sizeof(a));
	a.size = size;
	a.flags = flags;
	if (ioctl(fd, ATRIUM_GPU_IOC_BO_ALLOC, &a) != 0)
		return (-1);
	return ((int)a.bo_fd);
}

static int
bind(int vm, int bo, uint32_t flags, uint64_t *va)
{
	struct atrium_gpu_vm_bind2 b;

	memset(&b, 0, sizeof(b));
	b.vm_fd = (uint32_t)vm;
	b.bo_fd = (uint32_t)bo;
	b.flags = flags;
	if (ioctl(fd, ATRIUM_GPU_IOC_VM_BIND2, &b) != 0)
		return (-1);
	*va = b.va;
	return (0);
}

static int
bo_xfer(int bo, void *p, size_t len, int write)
{
	struct atrium_gpu_bo_xfer x;

	memset(&x, 0, sizeof(x));
	x.bo_fd = (uint32_t)bo;
	x.len = len;
	x.user_ptr = (uint64_t)(uintptr_t)p;
	return (ioctl(fd, write ? ATRIUM_GPU_IOC_BO_WRITE :
	    ATRIUM_GPU_IOC_BO_READ, &x));
}

static int
syncobj(void)
{
	struct atrium_gpu_syncobj_create c;

	memset(&c, 0, sizeof(c));
	if (ioctl(fd, ATRIUM_GPU_IOC_SYNCOBJ_CREATE, &c) != 0)
		return (-1);
	return ((int)c.out_fd);
}

/* One client "process": a VM with the shader, an IB and an output page. */
struct job {
	int vm, shader, ib, out, so;
	uint64_t shader_va, ib_va, out_va;
	uint64_t value;
};

static void
job_init(struct job *j)
{
	memset(j, 0, sizeof(*j));
	j->vm = j->shader = j->ib = j->out = j->so = -1;
}

static int
job_open(struct job *j, const char *name)
{
	job_init(j);
	j->vm = vm_create();
	j->shader = bo_alloc(4096, 0);
	j->ib = bo_alloc(4096, 0);
	j->out = bo_alloc(4096, 0);
	j->so = syncobj();
	if (j->vm < 0 || j->shader < 0 || j->ib < 0 || j->out < 0 ||
	    j->so < 0) {
		printf("%s: setup failed: %s\n", name, strerror(errno));
		return (-1);
	}
	if (bind(j->vm, j->shader, ATRIUM_GPU_BIND_READONLY, &j->shader_va) ||
	    bind(j->vm, j->ib, ATRIUM_GPU_BIND_READONLY, &j->ib_va) ||
	    bind(j->vm, j->out, 0, &j->out_va)) {
		printf("%s: bind failed: %s\n", name, strerror(errno));
		return (-1);
	}
	if (bo_xfer(j->shader, __DECONST(void *, siw42_shader),
	    sizeof(siw42_shader), 1) != 0) {
		printf("%s: shader write failed: %s\n", name, strerror(errno));
		return (-1);
	}
	printf("%s: VM fd %d: shader %#jx, IB %#jx, out %#jx\n", name, j->vm,
	    (uintmax_t)j->shader_va, (uintmax_t)j->ib_va,
	    (uintmax_t)j->out_va);
	return (0);
}

static void
job_close(struct job *j)
{
	int *fds[] = { &j->so, &j->out, &j->ib, &j->shader, &j->vm };
	size_t i;

	for (i = 0; i < sizeof(fds) / sizeof(fds[0]); i++)
		if (*fds[i] >= 0) {
			close(*fds[i]);
			*fds[i] = -1;
		}
}

static int
poison(int bo, uint32_t v)
{
	uint32_t w[SIW42_OUT_DW];
	u_int i;

	for (i = 0; i < SIW42_OUT_DW; i++)
		w[i] = v;
	return (bo_xfer(bo, w, sizeof(w), 1));
}

/* How many of the 64 output words are `want`; -1 if unreadable. */
static int
count(int bo, uint32_t want, uint32_t *first)
{
	uint32_t w[SIW42_OUT_DW];
	u_int i;
	int n = 0;

	if (bo_xfer(bo, w, sizeof(w), 0) != 0)
		return (-1);
	*first = w[0];
	for (i = 0; i < SIW42_OUT_DW; i++)
		n += w[i] == want;
	return (n);
}

/* Patch the IB for shader and data VAs, write it, submit it. */
static int
job_submit(struct job *j, uint64_t data_va)
{
	struct atrium_gpu_submit s;
	uint32_t ib[SIW42_IB_DW];
	u_int i;

	memcpy(ib, siw42_ib, sizeof(ib));
	for (i = 0; i < SIW42_NPATCH; i++)
		ib[siw42_patch[i].dw] = (uint32_t)((siw42_patch[i].which ?
		    data_va : j->shader_va) >> siw42_patch[i].shift);
	if (bo_xfer(j->ib, ib, sizeof(ib), 1) != 0)
		return (-1);
	memset(&s, 0, sizeof(s));
	s.vm_fd = (uint32_t)j->vm;
	s.ring_fd = (uint32_t)j->ib;
	s.n_dwords = SIW42_IB_DW;
	s.engine = ATRIUM_GPU_ENGINE_COMPUTE;
	s.signal_syncobj_fd = j->so;
	s.signal_value = ++j->value;
	return (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s));
}

/*
 * Wait on the job's syncobj through kqueue. 0 = reached its value; else
 * the error the kernel reported (EV_EOF fflags), or ETIMEDOUT.
 */
static int
job_wait(struct job *j, int ms, int *took_ms)
{
	struct kevent kev;
	struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
	struct timeval t0, t1;
	int kq, n, r;

	kq = kqueue();
	if (kq < 0)
		return (errno);
	EV_SET(&kev, j->so, EVFILT_READ, EV_ADD, 0, (intptr_t)j->value, NULL);
	gettimeofday(&t0, NULL);
	n = kevent(kq, &kev, 1, &kev, 1, &ts);
	gettimeofday(&t1, NULL);
	*took_ms = (int)((t1.tv_sec - t0.tv_sec) * 1000 +
	    (t1.tv_usec - t0.tv_usec) / 1000);
	if (n < 0)
		r = errno;
	else if (n == 0)
		r = ETIMEDOUT;
	else if (kev.flags & EV_ERROR)
		r = (int)kev.data;
	else if (kev.flags & EV_EOF)
		r = kev.fflags != 0 ? (int)kev.fflags : EIO;
	else
		r = 0;
	close(kq);
	return (r);
}

/* Run the job writing its own out page; check 64 x 42. */
static void
run_clean(struct job *j, const char *what)
{
	uint32_t first;
	int r, n, ms;

	if (poison(j->out, POISON_A) != 0 || job_submit(j, j->out_va) != 0) {
		check(0, what, "submit: %s", strerror(errno));
		return;
	}
	r = job_wait(j, 2000, &ms);
	n = count(j->out, SIW42_OUT_VALUE, &first);
	check(r == 0 && n == SIW42_OUT_DW, what, "completion %s after %d ms, "
	    "%d/64 words 42, out[0] %#x", r == 0 ? "ok" : strerror(r), ms, n,
	    first);
}

int
main(void)
{
	struct job a, b, c, d;
	struct atrium_gpu_submit s;
	struct atrium_gpu_queue_map qm;
	uint32_t first;
	int r, n, ms, bo;

	setvbuf(stdout, NULL, _IOLBF, 0);
	job_init(&a);
	job_init(&b);
	job_init(&c);
	job_init(&d);
	fd = open(DEV, O_RDWR);
	if (fd < 0) {
		printf("FAIL: open %s: %s\n", DEV, strerror(errno));
		return (1);
	}

	/* 1: one VM, one dispatch, kqueue completion. */
	if (job_open(&a, "1") == 0)
		run_clean(&a, "1 dispatch: write_42 in its own VM");

	/* 2: two fresh VMs at the same VAs. */
	if (job_open(&b, "2b") == 0 && job_open(&c, "2c") == 0) {
		check(b.out_va == c.out_va && b.shader_va == c.shader_va,
		    "2 same VAs in both VMs", "out %#jx / %#jx",
		    (uintmax_t)b.out_va, (uintmax_t)c.out_va);
		if (poison(b.out, POISON_A) || poison(c.out, POISON_B) ||
		    job_submit(&b, b.out_va) || job_submit(&c, c.out_va)) {
			check(0, "2 two VMs", "submit: %s", strerror(errno));
		} else {
			int rb = job_wait(&b, 2000, &ms);
			int rc = job_wait(&c, 2000, &ms);
			int nb = count(b.out, SIW42_OUT_VALUE, &first);
			int nc = count(c.out, SIW42_OUT_VALUE, &first);

			check(rb == 0 && rc == 0 && nb == 64 && nc == 64,
			    "2 two VMs: each sees 64 x 42 on its own page",
			    "B %s %d/64, C %s %d/64", strerror(rb), nb,
			    strerror(rc), nc);
		}
	}

	/*
	 * 3: VM D's IB writes at C's out VA, which D does not map. C's page
	 * is re-poisoned first and must keep its poison.
	 */
	if (job_open(&d, "3d") == 0 && c.out >= 0) {
		(void)poison(c.out, POISON_B);
		/* D's own out is bound at the same VA as C's: unbind it by
		 * closing, so the VA is unmapped in D. */
		uint64_t target = d.out_va;

		close(d.out);
		d.out = -1;
		if (job_submit(&d, target) != 0) {
			check(0, "3 fault", "submit: %s", strerror(errno));
		} else {
			r = job_wait(&d, 8000, &ms);
			check(r == EIO, "3 fault: the faulting VM's syncobj "
			    "fails with EIO", "got %s after %d ms",
			    r == 0 ? "success" : strerror(r), ms);
			n = count(c.out, POISON_B, &first);
			check(n == 64, "3 fault: the other VM's page at that VA "
			    "is untouched", "%d/64 poison, out[0] %#x", n,
			    first);
		}
		/*
		 * 4: D recovers with a page of its own again. Its syncobj
		 * keeps the EIO (the ABI's sticky error state), so the next
		 * job signals a fresh one.
		 */
		close(d.so);
		d.so = syncobj();
		d.value = 0;
		d.out = bo_alloc(4096, 0);
		if (d.out < 0 || bind(d.vm, d.out, 0, &d.out_va) != 0)
			check(0, "4 recovery", "rebind: %s", strerror(errno));
		else
			run_clean(&d, "4 recovery: the faulting VM runs clean");
		run_clean(&b, "4 recovery: another VM runs clean");
		run_clean(&a, "4 recovery: the first VM runs clean");
	}

	/* 5: refusals (need test 1's VM and IB). */
	if (a.vm < 0 || a.ib < 0) {
		check(0, "5 refusals", "no VM from test 1");
		goto out;
	}
	memset(&s, 0, sizeof(s));
	s.vm_fd = (uint32_t)a.vm;
	s.ring_fd = (uint32_t)a.ib;
	s.n_dwords = SIW42_IB_DW;
	s.engine = ATRIUM_GPU_ENGINE_GFX;
	s.signal_syncobj_fd = -1;
	r = ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &s) == 0 ? 0 : errno;
	check(r == EOPNOTSUPP, "5 graphics submit refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	bo = bo_alloc(4096, ATRIUM_GPU_BO_VRAM);
	r = bo >= 0 ? 0 : errno;
	if (bo >= 0)
		close(bo);
	check(r == EOPNOTSUPP, "5 VRAM BO refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	memset(&qm, 0, sizeof(qm));
	qm.vm_fd = (uint32_t)a.vm;
	qm.ring_fd = (uint32_t)a.ib;
	qm.engine = ATRIUM_GPU_ENGINE_COMPUTE;
	r = ioctl(fd, ATRIUM_GPU_IOC_QUEUE_MAP, &qm) == 0 ? 0 : errno;
	check(r == EOPNOTSUPP, "5 user-mode queue refused", "%s",
	    r == 0 ? "accepted" : strerror(r));

out:
	job_close(&a);
	job_close(&b);
	job_close(&c);
	job_close(&d);
	close(fd);
	printf("si_client_test: %d passed, %d failed: %s\n", npass, nfail,
	    nfail == 0 && npass > 0 ? "PASS" : "FAIL");
	return (nfail == 0 && npass > 0 ? 0 : 1);
}
