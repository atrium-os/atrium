/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * si_client_test.c — stage 5c-2b step 1 on the R9700
 * (docs/spec/r9700-bare-metal.md §4.5.1): client work on queue fds, each a
 * MES user queue of its own, driven from userspace through
 * atrium_gpu_amd_abi.h (QUEUE_CREATE on a VM fd, QUEUE_SUBMIT on the queue
 * fd; v2 layouts). Cross-built static for FreeBSD amd64
 * (scripts/r9700/build-si-client-test.sh); run as root with the kmods
 * attached (kenv hw.atrium_gpu.client=1).
 *
 *   1 queue      one VM, one queue fd, write_42 (oracle-built, IB patched
 *                with the VAs the kernel chose), completion through kqueue
 *                on the submit's out syncobj; out_seqno counts from 1.
 *   2 two VMs    two fresh VMs, a queue each, bound at the SAME VAs onto their
 *                own pages; both run at once, each sees only its own 64 x 42.
 *   3 two queues one VM with a second queue: both run, in turn.
 *   4 passing    a queue fd and a syncobj passed over SCM_RIGHTS to a child
 *                (forked before the queue existed) that submits on it.
 *   5 close      a queue closed right after a submit: the completion either
 *                lands or is cancelled (ECANCELED), never hangs; the VM then
 *                runs clean on a new queue.
 *   6 refusals   the 5c-2a SUBMIT on the client node, user-mode queues,
 *                other families and priorities, in-dependencies, bad blobs,
 *                the ioctls on the wrong fd, VRAM BOs.
 *
 * Prints one PASS/FAIL line per check and a summary; exit 0 only if all
 * passed.
 */
#include "../atrium_gpu_amd_abi.h"
#include "si_w42.h"

#include <sys/types.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>

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
vm_bind(int vm, int bo, uint32_t flags, uint64_t *va)
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

/* QUEUE_CREATE on a VM fd. -1 with errno on failure. */
static int
queue_create2(int vm, uint32_t family, uint32_t prio, uint32_t flags)
{
	struct atrium_gpu_queue_create c;

	memset(&c, 0, sizeof(c));
	c.struct_size = sizeof(c);
	c.family_id = family;
	c.priority = prio;
	c.flags = flags;
	if (ioctl(vm, ATRIUM_GPU_IOC_QUEUE_CREATE, &c) != 0)
		return (-1);
	return (c.out_queue_fd);
}

static int
queue_create(int vm)
{
	return (queue_create2(vm, ATRIUM_GPU_QUEUE_FAMILY_COMPUTE, 0, 0));
}

/* QUEUE_SUBMIT: `dw` dwords at `va`, signalling `so` to `value` (so < 0:
 * nothing). 0 or the errno. */
static int
queue_submit(int q, uint64_t va, uint32_t dw, int so, uint64_t value,
    uint64_t *seqno)
{
	struct atrium_gpu_queue_submit s;
	struct atrium_gpu_syncobj_point pt;

	memset(&s, 0, sizeof(s));
	memset(&pt, 0, sizeof(pt));
	s.struct_size = sizeof(s);
	s.blob_va = va;
	s.blob_size = (uint64_t)dw * 4;
	if (so >= 0) {
		pt.syncobj_fd = so;
		pt.value = value;
		s.out_dep_ptr = (uint64_t)(uintptr_t)&pt;
		s.out_dep_count = 1;
	}
	if (ioctl(q, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &s) != 0)
		return (errno);
	if (seqno != NULL)
		*seqno = s.out_seqno;
	return (0);
}

/* One client "process": a VM with the shader, an IB and an output page. */
struct job {
	int vm, q, shader, ib, out, so;
	uint64_t shader_va, ib_va, out_va;
	uint64_t value;
};

static void
job_init(struct job *j)
{
	memset(j, 0, sizeof(*j));
	j->vm = j->q = j->shader = j->ib = j->out = j->so = -1;
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
	if (vm_bind(j->vm, j->shader, ATRIUM_GPU_BIND_READONLY, &j->shader_va) ||
	    vm_bind(j->vm, j->ib, ATRIUM_GPU_BIND_READONLY, &j->ib_va) ||
	    vm_bind(j->vm, j->out, 0, &j->out_va)) {
		printf("%s: bind failed: %s\n", name, strerror(errno));
		return (-1);
	}
	if (bo_xfer(j->shader, __DECONST(void *, siw42_shader),
	    sizeof(siw42_shader), 1) != 0) {
		printf("%s: shader write failed: %s\n", name, strerror(errno));
		return (-1);
	}
	j->q = queue_create(j->vm);
	if (j->q < 0) {
		printf("%s: QUEUE_CREATE failed: %s\n", name, strerror(errno));
		return (-1);
	}
	printf("%s: VM fd %d, queue fd %d: shader %#jx, IB %#jx, out %#jx\n",
	    name, j->vm, j->q, (uintmax_t)j->shader_va, (uintmax_t)j->ib_va,
	    (uintmax_t)j->out_va);
	return (0);
}

static void
job_close(struct job *j)
{
	int *fds[] = { &j->so, &j->out, &j->ib, &j->shader, &j->q, &j->vm };
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

/* Patch the IB for shader and data VAs and write it. */
static int
job_ib(struct job *j, uint64_t data_va)
{
	uint32_t ib[SIW42_IB_DW];
	u_int i;

	memcpy(ib, siw42_ib, sizeof(ib));
	for (i = 0; i < SIW42_NPATCH; i++)
		ib[siw42_patch[i].dw] = (uint32_t)((siw42_patch[i].which ?
		    data_va : j->shader_va) >> siw42_patch[i].shift);
	return (bo_xfer(j->ib, ib, sizeof(ib), 1));
}

/* job_ib, then QUEUE_SUBMIT on queue `q`. 0 or -1 with errno. */
static int
job_submit_on(struct job *j, int q, uint64_t data_va, uint64_t *seqno)
{
	int r;

	if (job_ib(j, data_va) != 0)
		return (-1);
	r = queue_submit(q, j->ib_va, SIW42_IB_DW, j->so, ++j->value, seqno);
	if (r != 0) {
		errno = r;
		return (-1);
	}
	return (0);
}

static int
job_submit(struct job *j, uint64_t data_va)
{
	return (job_submit_on(j, j->q, data_va, NULL));
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
run_clean_on(struct job *j, int q, const char *what)
{
	uint32_t first;
	int r, n, ms;

	if (poison(j->out, POISON_A) != 0 ||
	    job_submit_on(j, q, j->out_va, NULL) != 0) {
		check(0, what, "submit: %s", strerror(errno));
		return;
	}
	r = job_wait(j, 2000, &ms);
	n = count(j->out, SIW42_OUT_VALUE, &first);
	check(r == 0 && n == SIW42_OUT_DW, what, "completion %s after %d ms, "
	    "%d/64 words 42, out[0] %#x", r == 0 ? "ok" : strerror(r), ms, n,
	    first);
}

static void
run_clean(struct job *j, const char *what)
{
	run_clean_on(j, j->q, what);
}

/* SCM_RIGHTS: send two fds / receive them. */
static int
send_fds(int sock, int a, int b)
{
	struct msghdr m;
	struct iovec iov;
	char byte = 'q';
	union {
		struct cmsghdr h;
		char buf[CMSG_SPACE(2 * sizeof(int))];
	} cm;
	struct cmsghdr *h;

	memset(&m, 0, sizeof(m));
	memset(&cm, 0, sizeof(cm));
	iov.iov_base = &byte;
	iov.iov_len = 1;
	m.msg_iov = &iov;
	m.msg_iovlen = 1;
	m.msg_control = cm.buf;
	m.msg_controllen = sizeof(cm.buf);
	h = CMSG_FIRSTHDR(&m);
	h->cmsg_level = SOL_SOCKET;
	h->cmsg_type = SCM_RIGHTS;
	h->cmsg_len = CMSG_LEN(2 * sizeof(int));
	memcpy(CMSG_DATA(h), (int[2]){ a, b }, 2 * sizeof(int));
	return (sendmsg(sock, &m, 0) == 1 ? 0 : -1);
}

static int
recv_fds(int sock, int *a, int *b)
{
	struct msghdr m;
	struct iovec iov;
	char byte;
	union {
		struct cmsghdr h;
		char buf[CMSG_SPACE(2 * sizeof(int))];
	} cm;
	struct cmsghdr *h;
	int fds[2];

	memset(&m, 0, sizeof(m));
	iov.iov_base = &byte;
	iov.iov_len = 1;
	m.msg_iov = &iov;
	m.msg_iovlen = 1;
	m.msg_control = cm.buf;
	m.msg_controllen = sizeof(cm.buf);
	if (recvmsg(sock, &m, 0) != 1)
		return (-1);
	h = CMSG_FIRSTHDR(&m);
	if (h == NULL || h->cmsg_type != SCM_RIGHTS ||
	    h->cmsg_len != CMSG_LEN(2 * sizeof(int)))
		return (-1);
	memcpy(fds, CMSG_DATA(h), sizeof(fds));
	*a = fds[0];
	*b = fds[1];
	return (0);
}

/* errno of an ioctl, 0 if it succeeded. */
static int
ioerr(int f, unsigned long cmd, void *arg)
{
	return (ioctl(f, cmd, arg) == 0 ? 0 : errno);
}

int
main(void)
{
	struct job a, b, c, e;
	struct atrium_gpu_submit s;
	struct atrium_gpu_queue_submit qs;
	struct atrium_gpu_queue_create qc;
	uint64_t seq1 = 0, seq2 = 0;
	uint32_t first;
	int r, n, ms, bo, q2, sv[2], st;
	pid_t pid;

	setvbuf(stdout, NULL, _IOLBF, 0);
	job_init(&a);
	job_init(&b);
	job_init(&c);
	job_init(&e);
	fd = open(DEV, O_RDWR);
	if (fd < 0) {
		printf("FAIL: open %s: %s\n", DEV, strerror(errno));
		return (1);
	}

	/* 1: one VM, one queue, one dispatch, kqueue completion. */
	if (job_open(&a, "1") == 0) {
		if (poison(a.out, POISON_A) != 0 ||
		    job_submit_on(&a, a.q, a.out_va, &seq1) != 0)
			check(0, "1 queue", "submit: %s", strerror(errno));
		else {
			r = job_wait(&a, 2000, &ms);
			n = count(a.out, SIW42_OUT_VALUE, &first);
			check(r == 0 && n == SIW42_OUT_DW, "1 queue: write_42 on "
			    "its own MES user queue", "completion %s after %d "
			    "ms, %d/64 words 42, out[0] %#x", r == 0 ? "ok" :
			    strerror(r), ms, n, first);
			check(seq1 == 1, "1 queue: out_seqno counts from 1",
			    "%ju", (uintmax_t)seq1);
		}
		run_clean(&a, "1 queue: a second job on the same queue");
	}

	/* 2: two fresh VMs, a queue each, at the same VAs, both at once. */
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
			    "2 two VMs: each queue writes 64 x 42 on its own "
			    "page", "B %s %d/64, C %s %d/64", strerror(rb), nb,
			    strerror(rc), nc);
		}
	}

	/* 3: a second queue in VM A; both queues run, in turn. */
	if (a.vm >= 0) {
		q2 = queue_create(a.vm);
		if (q2 < 0)
			check(0, "3 two queues in one VM", "QUEUE_CREATE: %s",
			    strerror(errno));
		else {
			run_clean_on(&a, q2, "3 two queues in one VM: the "
			    "second runs");
			run_clean_on(&a, a.q, "3 two queues in one VM: the "
			    "first still runs");
			close(q2);
		}
	}

	/*
	 * 4: a child that did not inherit the queue (forked before it
	 * existed) gets it and the syncobj over SCM_RIGHTS and submits; the
	 * parent waits for the completion and checks the page.
	 */
	if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
		check(0, "4 queue fd over SCM_RIGHTS", "socketpair: %s",
		    strerror(errno));
	} else if ((pid = fork()) == 0) {
		int cq = -1, cso = -1;
		uint64_t va, val;

		close(sv[0]);
		if (recv_fds(sv[1], &cq, &cso) != 0 ||
		    read(sv[1], &va, sizeof(va)) != sizeof(va) ||
		    read(sv[1], &val, sizeof(val)) != sizeof(val))
			_exit(2);
		r = queue_submit(cq, va, SIW42_IB_DW, cso, val, NULL);
		close(cq);
		close(cso);
		_exit(r == 0 ? 0 : 3);
	} else {
		close(sv[1]);
		if (job_open(&e, "4") != 0 || poison(e.out, POISON_A) != 0 ||
		    job_ib(&e, e.out_va) != 0) {
			check(0, "4 queue fd over SCM_RIGHTS", "setup: %s",
			    strerror(errno));
			close(sv[0]);
			(void)waitpid(pid, &st, 0);
		} else {
			uint64_t val = ++e.value;

			if (send_fds(sv[0], e.q, e.so) != 0 ||
			    write(sv[0], &e.ib_va, sizeof(e.ib_va)) !=
			    sizeof(e.ib_va) ||
			    write(sv[0], &val, sizeof(val)) != sizeof(val))
				printf("4: send failed: %s\n", strerror(errno));
			close(sv[0]);
			(void)waitpid(pid, &st, 0);
			r = job_wait(&e, 2000, &ms);
			n = count(e.out, SIW42_OUT_VALUE, &first);
			check(WIFEXITED(st) && WEXITSTATUS(st) == 0 && r == 0 &&
			    n == 64, "4 queue fd over SCM_RIGHTS: the child's "
			    "submit runs", "child exit %d, completion %s, "
			    "%d/64 words 42", WIFEXITED(st) ?
			    WEXITSTATUS(st) : -1, r == 0 ? "ok" : strerror(r),
			    n);
		}
	}

	/* 5: close a queue right after a submit; the VM runs on after. */
	if (b.vm >= 0) {
		if (poison(b.out, POISON_A) != 0 ||
		    job_submit(&b, b.out_va) != 0) {
			check(0, "5 close in flight", "submit: %s",
			    strerror(errno));
		} else {
			close(b.q);
			b.q = -1;
			r = job_wait(&b, 2000, &ms);
			check(r == 0 || r == ECANCELED, "5 close in flight: "
			    "the completion lands or is cancelled", "%s after "
			    "%d ms", r == 0 ? "landed" : strerror(r), ms);
			close(b.so);
			b.so = syncobj();
			b.value = 0;
			b.q = queue_create(b.vm);
			if (b.q < 0)
				check(0, "5 close in flight: new queue",
				    "QUEUE_CREATE: %s", strerror(errno));
			else
				run_clean(&b, "5 close in flight: the VM runs "
				    "clean on a new queue");
		}
	}

	/* 6: refusals (need test 1's VM, queue and IB). */
	if (a.vm < 0 || a.q < 0 || a.ib < 0) {
		check(0, "6 refusals", "no VM and queue from test 1");
		goto out;
	}
	memset(&s, 0, sizeof(s));
	s.vm_fd = (uint32_t)a.vm;
	s.ring_fd = (uint32_t)a.ib;
	s.n_dwords = SIW42_IB_DW;
	s.engine = ATRIUM_GPU_ENGINE_COMPUTE;
	s.signal_syncobj_fd = -1;
	r = ioerr(fd, ATRIUM_GPU_IOC_SUBMIT, &s);
	check(r == EOPNOTSUPP, "6 the 5c-2a SUBMIT on the client node is "
	    "gone", "%s", r == 0 ? "accepted" : strerror(r));
	r = queue_create2(a.vm, ATRIUM_GPU_QUEUE_FAMILY_COMPUTE, 0,
	    ATRIUM_GPU_QUEUE_FLAG_USER_MODE) >= 0 ? 0 : errno;
	check(r == EOPNOTSUPP, "6 user-mode queue refused (step 3)", "%s",
	    r == 0 ? "accepted" : strerror(r));
	r = queue_create2(a.vm, 1, 0, 0) >= 0 ? 0 : errno;
	check(r == EINVAL, "6 unknown queue family refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	r = queue_create2(a.vm, ATRIUM_GPU_QUEUE_FAMILY_COMPUTE, 1, 0) >= 0 ?
	    0 : errno;
	check(r == EINVAL, "6 priority other than 0 refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	memset(&qc, 0, sizeof(qc));
	qc.struct_size = sizeof(qc);
	r = ioerr(fd, ATRIUM_GPU_IOC_QUEUE_CREATE, &qc);
	check(r == ENOTTY, "6 QUEUE_CREATE on the client node refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	memset(&qs, 0, sizeof(qs));
	qs.struct_size = sizeof(qs);
	qs.blob_va = a.ib_va;
	qs.blob_size = SIW42_IB_DW * 4;
	r = ioerr(a.vm, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &qs);
	check(r == ENOTTY, "6 QUEUE_SUBMIT on a VM fd refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	qs.in_dep_count = 1;
	r = ioerr(a.q, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &qs);
	check(r == EOPNOTSUPP, "6 in-dependencies refused (not yet)", "%s",
	    r == 0 ? "accepted" : strerror(r));
	qs.in_dep_count = 0;
	qs.blob_size = 6;
	r = ioerr(a.q, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &qs);
	check(r == EINVAL, "6 a blob that is not whole dwords refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	qs.blob_size = 0;
	r = ioerr(a.q, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &qs);
	check(r == EINVAL, "6 an empty blob refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	bo = bo_alloc(4096, ATRIUM_GPU_BO_VRAM);
	r = bo >= 0 ? 0 : errno;
	if (bo >= 0)
		close(bo);
	check(r == EOPNOTSUPP, "6 VRAM BO refused (5d)", "%s",
	    r == 0 ? "accepted" : strerror(r));
	run_clean(&a, "6 the first queue still runs after the refusals");
	(void)seq2;

out:
	job_close(&a);
	job_close(&b);
	job_close(&c);
	job_close(&e);
	close(fd);
	printf("si_client_test: %d passed, %d failed: %s\n", npass, nfail,
	    nfail == 0 && npass > 0 ? "PASS" : "FAIL");
	return (nfail == 0 && npass > 0 ? 0 : 1);
}
