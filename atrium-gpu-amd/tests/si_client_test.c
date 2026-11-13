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
 *   6 refusals   the retired ring SUBMIT ('A',4), user-mode queues,
 *                other families and priorities, in-dependencies, bad blobs,
 *                the ioctls on the wrong fd.
 *   7 fault      (step 2) a queue whose job writes to a VA its VM does not
 *                map: its completion fails with EIO, the queue refuses more
 *                work, another VM's queue runs on, and the faulting VM runs
 *                clean on a new queue.
 *   8 hang       (step 2) a queue whose job waits forever (WAIT_REG_MEM on a
 *                value never written): after hang_ms its completion fails
 *                with EIO, the queue refuses more work, others run on, and
 *                the VM runs clean on a new queue.
 *   9 user mode  (step 3) a USER_MODE queue: QUEUE_MAP2's layout, mmap of
 *                ring, writeback page and doorbell; write_42 rung from user
 *                space twice (fence at the kernel's fence VA); refusals
 *                (QUEUE_SUBMIT on it, QUEUE_MAP2/mmap of a kernel-mode
 *                queue, private/exec/oversized mappings); its close takes
 *                ring and doorbell back (SIGBUS in a child); a stuck user
 *                ring (WAIT_REG_MEM in an IB) is lost after hang_ms
 *                (QUEUE_MAP2: EIO) while another VM runs on; the VM then
 *                runs clean on a kernel-mode queue.
 *  10 free rule  (step 4) a BO bound in a VM with live queues is freed at its
 *                close (one MES preemption of the VM's queues, not deferred
 *                until they close), then the queue runs on; the same while
 *                one of the VM's queues is stuck: freed at once or held, and
 *                nothing deferred is left once that queue is lost.
 *  11 VRAM       (5d) the initial scrub finishes; a VRAM BO bound in a VM
 *                reads zero (CP DMA copy into a System BO), takes a fill and
 *                reads it back, refuses CPU reads; freed, it is scrubbed
 *                (vram sysctls), and a new VRAM BO reads zero again.
 *  12 real ops   (5e) gpusim's SIM_COMPUTE_KERNEL ops COPY, INC and FILL as
 *                real shaders (si_ops.h, oracle-built): each on its own
 *                code page, 64 elements, from a System and a VRAM source.
 *  13 W^X        with isolation.gpu_wx = 1 (client BOs): BIND_EXEC without
 *                READONLY is refused; code and IB bound EXEC run with data
 *                pages without X; the shader copied into a read-write data
 *                BO faults (EIO) and the VM runs on.
 *  14 hostile    (silicon only; security model §10 item 1) client packets
 *     PM4        aimed at registers only the kernel may touch, from the
 *                kernel's own list (sysctl si_protected_regs, per-IP
 *                tables): VM context 0 on both hubs, the IH ring base, MES's
 *                pipe control (WRITE_DATA and COPY_DATA writes of the value
 *                already there, so a write that lands changes nothing) and
 *                the SMU argument mailbox (read only). Each runs on a fresh
 *                VM and must be refused (its work fails, the marker after it
 *                never lands, the register is unchanged); a read must not
 *                return the real value; a bystander VM runs clean after
 *                each. Each hostile job runs in a child under its own uid:
 *                if its queue forced a whole-GPU reset, that uid's next
 *                QUEUE_CREATE is refused (EPERM, isolation.reset_lockout_s)
 *                while root's still works. Stops after 2 whole-GPU
 *                re-inits (cap 3 per 10 min).
 *
 * Prints one PASS/FAIL line per check and a summary; exit 0 only if all
 * passed.
 */
#include "../atrium_gpu_amd_abi.h"
#include "si_w42.h"
#include "si_ops.h"

#include <sys/types.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/sysctl.h>
#include <sys/uio.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <unistd.h>

/* The retired v1 ring SUBMIT ('A',4), kept here only to check it is gone. */
struct si_old_submit {
	uint64_t	signal_value;
	uint32_t	vm_fd, ring_fd, n_dwords, engine;
	int32_t		signal_syncobj_fd;
	uint32_t	deadline_ns;
};
#define SI_OLD_IOC_SUBMIT	_IOW('A', 4, struct si_old_submit)

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

/*
 * SI_SKIPHANG=1 (wedge bisection, run 6eed21f1): leaves out the three
 * hung-queue tests (8, 9's stuck user ring, 10's stuck queue). Test 7's
 * store fault comes before them and recovers; test 15's comes after them
 * and wedges the GFXHUB, with the same VMID, slot and fault status.
 */
static int
skiphang(const char *what)
{
	const char *e = getenv("SI_SKIPHANG");

	if (e == NULL || *e != '1')
		return (0);
	printf("SKIP: %s (SI_SKIPHANG)\n", what);
	return (1);
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

/* After its queue was lost: a new queue, a fresh syncobj, an out page. */
static int
job_renew(struct job *j)
{
	close(j->q);
	close(j->so);
	j->so = syncobj();
	j->value = 0;
	if (j->so < 0 || (j->q = queue_create(j->vm)) < 0)
		return (-1);
	if (j->out < 0 && ((j->out = bo_alloc(4096, 0)) < 0 ||
	    vm_bind(j->vm, j->out, 0, &j->out_va) != 0))
		return (-1);
	return (0);
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
/* ---- 9: user-mode queues (step 3) ---- */

static int ioerr(int f, unsigned long cmd, void *arg);

#define PKT3(op, n)	(3u << 30 | ((n) & 0x3fffu) << 16 | (op) << 8)

struct uqm {
	int q;
	uint8_t *m;
	size_t len;
	struct atrium_gpu_queue_map2 mi;
	uint64_t wptr;
	uint32_t emitted;
};

static int
uq_open(int vm, struct uqm *u)
{
	memset(u, 0, sizeof(*u));
	u->m = NULL;
	u->q = queue_create2(vm, ATRIUM_GPU_QUEUE_FAMILY_COMPUTE, 0,
	    ATRIUM_GPU_QUEUE_FLAG_USER_MODE);
	if (u->q < 0)
		return (-1);
	u->mi.struct_size = sizeof(u->mi);
	if (ioctl(u->q, ATRIUM_GPU_IOC_QUEUE_MAP2, &u->mi) != 0)
		return (-1);
	u->len = u->mi.out_doorbell_offset + u->mi.out_doorbell_size;
	u->m = mmap(NULL, u->len, PROT_READ | PROT_WRITE, MAP_SHARED, u->q, 0);
	if (u->m == MAP_FAILED) {
		u->m = NULL;
		return (-1);
	}
	return (0);
}

static void
uq_close(struct uqm *u)
{
	if (u->m != NULL)
		munmap(u->m, u->len);
	if (u->q >= 0)
		close(u->q);
	u->m = NULL;
	u->q = -1;
}

static volatile uint8_t *
uq_wb(struct uqm *u)
{
	return (u->m + u->mi.out_wptr_offset - 8);
}

/* Packets onto the ring, then emitted, wptr and the doorbell. */
static void
uq_ring(struct uqm *u, const uint32_t *pkt, u_int n)
{
	volatile uint32_t *ring = (volatile uint32_t *)(u->m +
	    u->mi.out_ring_offset);
	u_int i, rdw = (u_int)(u->mi.out_ring_size / 4);

	for (i = 0; i < n; i++)
		ring[(u->wptr + i) % rdw] = pkt[i];
	u->wptr += n;
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	*(volatile uint32_t *)(uq_wb(u) + 0x18) = u->emitted;
	*(volatile uint64_t *)(uq_wb(u) + 0x08) = u->wptr;
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	*(volatile uint64_t *)(u->m + u->mi.out_doorbell_offset) = u->wptr;
}

/* An IB, then a RELEASE_MEM of the next fence value (no interrupt). */
static uint32_t
uq_job(struct uqm *u, uint64_t ib, u_int ib_dw)
{
	uint64_t f = u->mi.out_fence_gpu_va;
	uint32_t p[12];
	u_int n = 0;

	u->emitted++;
	p[n++] = PKT3(0x3fu, 2);			/* INDIRECT_BUFFER */
	p[n++] = (uint32_t)ib;
	p[n++] = (uint32_t)(ib >> 32);
	p[n++] = 1u << 23 | ib_dw;			/* valid */
	p[n++] = PKT3(0x49u, 6);			/* RELEASE_MEM */
	p[n++] = 1u << 22 | 1u << 21 | 3u << 25 | 5u << 8 | 0x14u;
	p[n++] = 1u << 29;				/* 32 bits, no int */
	p[n++] = (uint32_t)f;
	p[n++] = (uint32_t)(f >> 32);
	p[n++] = u->emitted;
	p[n++] = 0;
	p[n++] = 0;
	uq_ring(u, p, n);
	return (u->emitted);
}

static uint32_t
uq_fence(struct uqm *u)
{
	return (*(volatile uint32_t *)(uq_wb(u) + 0x10));
}

/* 0 when the fence reaches `v` within `ms`, else ETIMEDOUT. */
static int
uq_wait(struct uqm *u, uint32_t v, int ms, int *took)
{
	struct timeval t0, t1;

	gettimeofday(&t0, NULL);
	for (;;) {
		gettimeofday(&t1, NULL);
		*took = (int)((t1.tv_sec - t0.tv_sec) * 1000 +
		    (t1.tv_usec - t0.tv_usec) / 1000);
		if ((int32_t)(uq_fence(u) - v) >= 0)
			return (0);
		if (*took > ms)
			return (ETIMEDOUT);
		usleep(1000);
	}
}

/* A child touches `p` (write or read): 1 if it died of SIGBUS. */
static int
touch_sigbus(volatile uint8_t *p, int write, int *how)
{
	pid_t pid;
	int st;

	pid = fork();
	if (pid < 0)
		return (0);
	if (pid == 0) {
		struct rlimit nocore = { 0, 0 };

		(void)setrlimit(RLIMIT_CORE, &nocore);	/* the SIGBUS is wanted */
		if (write)
			*(volatile uint64_t *)p = 1;
		else
			(void)*(volatile uint32_t *)p;
		_exit(0);
	}
	if (waitpid(pid, &st, 0) != pid)
		return (0);
	*how = st;
	return (WIFSIGNALED(st) && WTERMSIG(st) == SIGBUS);
}

static void
user_mode(struct job *other)
{
	struct job g;
	struct uqm u, h;
	struct atrium_gpu_queue_map2 mi;
	struct atrium_gpu_queue_submit qs;
	uint32_t first, v, w[7];
	void *p;
	int r, n, ms, st = 0;

	if (job_open(&g, "9") != 0) {
		check(0, "9 user mode", "setup failed");
		job_close(&g);
		return;
	}
	close(g.q);
	g.q = -1;
	if (uq_open(g.vm, &u) != 0) {
		check(0, "9 user mode: create, QUEUE_MAP2, mmap", "%s",
		    strerror(errno));
		uq_close(&u);
		job_close(&g);
		return;
	}
	check(u.mi.out_ring_offset == 0 && u.mi.out_ring_size == 4096 &&
	    u.mi.out_wptr_offset == 0x1008 && u.mi.out_doorbell_offset ==
	    0x2000 && u.mi.out_doorbell_size == 4096 &&
	    u.mi.out_fence_gpu_va != 0, "9 user mode: QUEUE_MAP2 layout",
	    "ring %#jx+%#jx, wptr %#jx, doorbell %#jx+%#jx, fence VA %#jx",
	    (uintmax_t)u.mi.out_ring_offset, (uintmax_t)u.mi.out_ring_size,
	    (uintmax_t)u.mi.out_wptr_offset,
	    (uintmax_t)u.mi.out_doorbell_offset,
	    (uintmax_t)u.mi.out_doorbell_size,
	    (uintmax_t)u.mi.out_fence_gpu_va);

	/* write_42, rung from user space, twice on the same ring. */
	for (n = 1; n <= 2; n++) {
		if (poison(g.out, n == 1 ? POISON_A : POISON_B) != 0 ||
		    job_ib(&g, g.out_va) != 0) {
			check(0, "9 user mode: job", "setup: %s",
			    strerror(errno));
			break;
		}
		v = uq_job(&u, g.ib_va, SIW42_IB_DW);
		r = uq_wait(&u, v, 2000, &ms);
		st = count(g.out, SIW42_OUT_VALUE, &first);
		check(r == 0 && st == SIW42_OUT_DW, n == 1 ?
		    "9 user mode: write_42 rung from user space" :
		    "9 user mode: a second job on the same ring",
		    "fence %u (want %u) after %d ms, %d/64 words 42, out[0] "
		    "%#x, rptr report %u, wptr %ju", uq_fence(&u), v, ms, st,
		    first, *(volatile uint32_t *)uq_wb(&u),
		    (uintmax_t)u.wptr);
	}

	/* Refusals. */
	memset(&qs, 0, sizeof(qs));
	qs.struct_size = sizeof(qs);
	qs.blob_va = g.ib_va;
	qs.blob_size = SIW42_IB_DW * 4;
	r = ioerr(u.q, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &qs);
	check(r == EOPNOTSUPP, "9 user mode: QUEUE_SUBMIT on it refused",
	    "%s", r == 0 ? "accepted" : strerror(r));
	memset(&mi, 0, sizeof(mi));
	mi.struct_size = sizeof(mi);
	r = other->q >= 0 ? ioerr(other->q, ATRIUM_GPU_IOC_QUEUE_MAP2, &mi) :
	    -1;
	check(r == EINVAL, "9 user mode: QUEUE_MAP2 of a kernel-mode queue "
	    "refused", "%s", r == 0 ? "accepted" : strerror(r));
	p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, other->q, 0);
	r = p == MAP_FAILED ? errno : 0;
	if (p != MAP_FAILED)
		munmap(p, 4096);
	check(r != 0, "9 user mode: mmap of a kernel-mode queue refused",
	    "%s", r == 0 ? "mapped" : strerror(r));
	p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, u.q, 0);
	r = p == MAP_FAILED ? errno : 0;
	if (p != MAP_FAILED)
		munmap(p, 4096);
	check(r != 0, "9 user mode: a private mapping refused", "%s",
	    r == 0 ? "mapped" : strerror(r));
	p = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_SHARED, u.q, 0);
	r = p == MAP_FAILED ? errno : 0;
	if (p != MAP_FAILED)
		munmap(p, 4096);
	check(r != 0, "9 user mode: an executable mapping refused", "%s",
	    r == 0 ? "mapped" : strerror(r));
	p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, u.q,
	    (off_t)u.len);
	r = p == MAP_FAILED ? errno : 0;
	if (p != MAP_FAILED)
		munmap(p, 4096);
	check(r != 0, "9 user mode: a mapping past the queue's pages "
	    "refused", "%s", r == 0 ? "mapped" : strerror(r));

	/* The close takes every mapping back. */
	close(u.q);
	u.q = -1;
	r = touch_sigbus(u.m + u.mi.out_ring_offset, 0, &st);
	check(r, "9 user mode: after close, the ring is gone (SIGBUS)",
	    "child status %#x", st);
	r = touch_sigbus(u.m + u.mi.out_doorbell_offset, 1, &st);
	check(r, "9 user mode: after close, the doorbell is gone (SIGBUS)",
	    "child status %#x", st);
	uq_close(&u);

	/* A stuck user ring is lost after hang_ms; others run on. */
	if (skiphang("9 hang")) {
		;
	} else if (uq_open(g.vm, &h) != 0) {
		check(0, "9 hang: user queue", "%s", strerror(errno));
	} else {
		(void)poison(g.out, POISON_A);
		w[0] = 3u << 30 | 5u << 16 | 0x3cu << 8;  /* WAIT_REG_MEM */
		w[1] = 3u | 1u << 4;		/* equal, memory */
		w[2] = (uint32_t)g.out_va;
		w[3] = (uint32_t)(g.out_va >> 32);
		w[4] = 0x12345678u;		/* never written */
		w[5] = 0xffffffffu;
		w[6] = 0x20u;
		if (bo_xfer(g.ib, w, sizeof(w), 1) != 0) {
			check(0, "9 hang", "IB: %s", strerror(errno));
		} else {
			struct timeval t0, t1;

			(void)uq_job(&h, g.ib_va, 7);
			gettimeofday(&t0, NULL);
			do {
				usleep(250000);
				memset(&mi, 0, sizeof(mi));
				mi.struct_size = sizeof(mi);
				r = ioerr(h.q, ATRIUM_GPU_IOC_QUEUE_MAP2, &mi);
				gettimeofday(&t1, NULL);
				ms = (int)((t1.tv_sec - t0.tv_sec) * 1000 +
				    (t1.tv_usec - t0.tv_usec) / 1000);
			} while (r == 0 && ms < 15000);
			check(r == EIO && ms >= 3000, "9 hang: a stuck user "
			    "ring is lost after hang_ms (QUEUE_MAP2: EIO)",
			    "%s after %d ms, fence %u", r == 0 ? "still alive" :
			    strerror(r), ms, uq_fence(&h));
			run_clean(other, "9 hang: another VM's queue runs on");
		}
		uq_close(&h);
	}

	/* The VM runs on, on a kernel-mode queue. */
	g.q = queue_create(g.vm);
	if (g.q < 0)
		check(0, "9 user mode: kernel queue after", "%s",
		    strerror(errno));
	else
		run_clean(&g, "9 user mode: the VM runs clean on a kernel-mode "
		    "queue after");
	job_close(&g);
}

/* ---- 10: the free rule (step 4) ---- */

static int64_t
iso(const char *name)
{
	char n[128];
	uint64_t v = 0;
	int iv = 0;
	size_t len = sizeof(v);

	snprintf(n, sizeof(n), "dev.atrium_gpu_amd.0.isolation.%s", name);
	if (sysctlbyname(n, &v, &len, NULL, 0) != 0)
		return (-1);
	if (len == sizeof(iv)) {
		memcpy(&iv, &v, sizeof(iv));
		return (iv);
	}
	return ((int64_t)v);
}

/* A scratch BO bound into `vm`, written, then closed. */
static int
bo_bind_close(int vm)
{
	uint32_t w[16];
	uint64_t va;
	int bo;

	bo = bo_alloc(4096, 0);
	if (bo < 0 || vm_bind(vm, bo, 0, &va) != 0) {
		if (bo >= 0)
			close(bo);
		return (-1);
	}
	memset(w, 0x3c, sizeof(w));
	(void)bo_xfer(bo, w, sizeof(w), 1);
	close(bo);
	return (0);
}

static void
free_rule(struct job *a)
{
	struct job h;
	int64_t d0, d1, now;
	uint32_t w[7];
	int r, ms, i;

	/* A VM whose queue is live and idle. */
	d0 = iso("deferred");
	if (d0 < 0) {
		check(0, "10 free rule", "isolation sysctls unreadable: %s",
		    strerror(errno));
		return;
	}
	if (bo_bind_close(a->vm) != 0) {
		check(0, "10 free rule", "BO: %s", strerror(errno));
		return;
	}
	d1 = iso("deferred");
	now = iso("deferred_now");
	check(d1 == d0 && now == 0, "10 free rule: a BO of a VM with a live "
	    "queue is freed at its close (preempted, not deferred)",
	    "deferred %jd -> %jd, now %jd", (intmax_t)d0, (intmax_t)d1,
	    (intmax_t)now);
	run_clean(a, "10 free rule: the queue runs on after the preemption");

	/* The same while one of the VM's queues is stuck. */
	if (skiphang("10 free rule (stuck)"))
		return;
	if (job_open(&h, "10") != 0) {
		check(0, "10 free rule (stuck)", "setup failed");
		job_close(&h);
		return;
	}
	(void)poison(h.out, POISON_A);
	w[0] = 3u << 30 | 5u << 16 | 0x3cu << 8;	/* WAIT_REG_MEM */
	w[1] = 3u | 1u << 4;
	w[2] = (uint32_t)h.out_va;
	w[3] = (uint32_t)(h.out_va >> 32);
	w[4] = 0x12345678u;
	w[5] = 0xffffffffu;
	w[6] = 0x20u;
	if (bo_xfer(h.ib, w, sizeof(w), 1) != 0 ||
	    (r = queue_submit(h.q, h.ib_va, 7, h.so, ++h.value, NULL)) != 0) {
		check(0, "10 free rule (stuck)", "submit: %s", strerror(errno));
		job_close(&h);
		return;
	}
	usleep(200000);
	d0 = iso("deferred");
	r = bo_bind_close(h.vm);
	d1 = iso("deferred");
	printf("10: BO closed under a stuck queue: %s (deferred %jd -> %jd)\n",
	    r != 0 ? strerror(errno) : d1 == d0 ? "freed at once" : "held",
	    (intmax_t)d0, (intmax_t)d1);
	r = job_wait(&h, 15000, &ms);
	check(r == EIO, "10 free rule (stuck): the stuck queue is still lost "
	    "after hang_ms", "got %s after %d ms", r == 0 ? "success" :
	    strerror(r), ms);
	close(h.q);
	h.q = -1;
	for (i = 0; i < 100 && (now = iso("deferred_now")) != 0; i++)
		usleep(20000);
	check(now == 0, "10 free rule (stuck): nothing deferred is left once "
	    "the queue is gone", "deferred_now %jd", (intmax_t)now);
	run_clean(a, "10 free rule (stuck): another VM's queue runs on");
	job_close(&h);
}

/* ---- 11: VRAM (5d) ---- */

#define VRAM_BO		(1u << 20)
#define VRAM_FILL	0xc0de1234u

static int64_t
vram_stat(const char *name)
{
	char n[128];
	uint64_t v = 0;
	size_t len = sizeof(v);

	snprintf(n, sizeof(n), "dev.atrium_gpu_amd_pci.0.vram.%s", name);
	if (sysctlbyname(n, &v, &len, NULL, 0) != 0)
		return (-1);
	return ((int64_t)v);
}

/* Wait (up to `ms`) for no dirty VRAM; the ms it took, or -1. */
static int
vram_clean_wait(int ms)
{
	int t;

	for (t = 0; t <= ms; t += 20) {
		if (vram_stat("dirty") == 0)
			return (t);
		usleep(20000);
	}
	return (-1);
}

/* One CP DMA_DATA packet (GFX9+ layout): a fill of `bytes` at `dst` with
 * `v` (fill) or a copy from `v` (an address). Through the L2 both ways. */
static u_int
dma_pkt(uint32_t *w, uint64_t dst, uint64_t v, uint32_t bytes, int fill)
{
	w[0] = 3u << 30 | 5u << 16 | 0x50u << 8;	/* DMA_DATA */
	w[1] = 1u << 31 | (fill ? 2u : 3u) << 29 | 3u << 20;
	w[2] = (uint32_t)v;
	w[3] = fill ? 0 : (uint32_t)(v >> 32);
	w[4] = (uint32_t)dst;
	w[5] = (uint32_t)(dst >> 32);
	w[6] = bytes;
	return (7);
}

/* Run `dw` dwords of packets as the job's IB and wait. 0 or an errno. */
static int
dma_run(struct job *j, const uint32_t *w, u_int dw)
{
	int r, ms;

	if (bo_xfer(j->ib, __DECONST(void *, w), dw * 4, 1) != 0)
		return (errno);
	if ((r = queue_submit(j->q, j->ib_va, dw, j->so, ++j->value,
	    NULL)) != 0)
		return (r);
	return (job_wait(j, 5000, &ms));
}

/* Copy 4 KiB at `src` into the job's out page; how many words are `want`
 * (-1 on failure, with *err). */
static int
vram_peek(struct job *j, uint64_t src, uint32_t want, int *err,
    uint32_t *first)
{
	uint32_t w[7];

	*first = 0;
	if (poison(j->out, POISON_B) != 0) {
		*err = errno;
		return (-1);
	}
	(void)dma_pkt(w, j->out_va, src, SIW42_OUT_DW * 4, 0);
	if ((*err = dma_run(j, w, 7)) != 0)
		return (-1);
	return (count(j->out, want, first));
}

static void
vram(struct job *a)
{
	uint32_t w[16], first, buf[16];
	uint64_t va, va2;
	int64_t total, s0, s1;
	int bo, bo2, n, r, t;

	total = vram_stat("total");
	t = vram_clean_wait(60000);
	check(total > 0 && t >= 0, "11 VRAM: the allocator is up and its "
	    "initial scrub finished", "total %jd MiB, base %#jx, clean after "
	    "%d ms, scrubbed %jd MiB", (intmax_t)(total >> 20),
	    (intmax_t)vram_stat("base"), t, (intmax_t)(vram_stat("scrubbed") >>
	    20));
	bo = bo_alloc(VRAM_BO, ATRIUM_GPU_BO_VRAM);
	if (bo < 0 || vm_bind(a->vm, bo, 0, &va) != 0) {
		check(0, "11 VRAM: a VRAM BO", "alloc/bind: %s",
		    strerror(errno));
		if (bo >= 0)
			close(bo);
		return;
	}
	printf("11: VRAM BO of %u KiB at VA %#jx\n", VRAM_BO >> 10,
	    (uintmax_t)va);
	r = bo_xfer(bo, buf, sizeof(buf), 0) == 0 ? 0 : errno;
	check(r == EINVAL, "11 VRAM: CPU read of a VRAM BO refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	n = vram_peek(a, va + VRAM_BO - 4096, 0, &r, &first);
	check(n == SIW42_OUT_DW, "11 VRAM: a new VRAM BO reads zero", "%d/64 "
	    "words 0 (%s), first %#x", n, r == 0 ? "ok" : strerror(r), first);
	(void)dma_pkt(w, va, VRAM_FILL, VRAM_BO, 1);
	r = dma_run(a, w, 7);
	n = r == 0 ? vram_peek(a, va + VRAM_BO / 2, VRAM_FILL, &r, &first) :
	    -1;
	check(n == SIW42_OUT_DW, "11 VRAM: a fill of the BO reads back", "%d/64 "
	    "words %#x (%s), first %#x", n, VRAM_FILL, r == 0 ? "ok" :
	    strerror(r), first);

	s0 = vram_stat("scrubbed");
	close(bo);
	t = vram_clean_wait(5000);
	s1 = vram_stat("scrubbed");
	check(t >= 0 && s1 - s0 >= VRAM_BO, "11 VRAM: the freed BO is scrubbed",
	    "clean after %d ms, scrubbed +%jd KiB", t, (intmax_t)((s1 - s0) >>
	    10));
	bo2 = bo_alloc(VRAM_BO, ATRIUM_GPU_BO_VRAM);
	if (bo2 < 0 || vm_bind(a->vm, bo2, 0, &va2) != 0) {
		check(0, "11 VRAM: a second VRAM BO", "alloc/bind: %s",
		    strerror(errno));
	} else {
		n = vram_peek(a, va2 + VRAM_BO / 2, 0, &r, &first);
		check(n == SIW42_OUT_DW, "11 VRAM: the next VRAM BO reads zero "
		    "(no data from the freed one)", "%d/64 words 0 (%s), first "
		    "%#x", n, r == 0 ? "ok" : strerror(r), first);
	}
	if (bo2 >= 0)
		close(bo2);
	printf("11: vram used %jd, dirty %jd, allocs %jd, frees %jd, scrub "
	    "jobs %jd (failed %jd)\n", (intmax_t)vram_stat("used"),
	    (intmax_t)vram_stat("dirty"), (intmax_t)vram_stat("allocs"),
	    (intmax_t)vram_stat("frees"), (intmax_t)vram_stat("scrub_jobs"),
	    (intmax_t)vram_stat("scrub_failed"));
	run_clean(a, "11 VRAM: the queue runs on after the VRAM work");
}

/* ---- 12: gpusim's compute ops as real shaders (5e) ---- */

/* Run `code` (one of si_ops.h's shaders) over src -> dst with `value`. */
static int
ops_run(struct job *j, int code_bo, uint64_t code_va, const uint32_t *code,
    size_t code_bytes, uint64_t src, uint64_t dst, uint32_t value)
{
	uint32_t ib[SIOPS_IB_DW];
	uint64_t prm[4] = { code_va, src, dst, value };
	u_int i;

	if (bo_xfer(code_bo, __DECONST(void *, code), code_bytes, 1) != 0)
		return (errno);
	memcpy(ib, siops_ib, sizeof(ib));
	for (i = 0; i < SIOPS_NPATCH; i++)
		ib[siops_patch[i].dw] = (uint32_t)(prm[siops_patch[i].which] >>
		    siops_patch[i].shift);
	return (dma_run(j, ib, SIOPS_IB_DW));
}

static void
real_ops(struct job *a)
{
	static const struct {
		const char *name;
		const uint32_t *code;
		size_t bytes;
		int op;			/* 0 copy, 1 inc, 2 fill */
	} ops[] = {
		{ "COPY", siops_copy, sizeof(siops_copy), 0 },
		{ "INC", siops_inc, sizeof(siops_inc), 1 },
		{ "FILL", siops_fill, sizeof(siops_fill), 2 },
	};
	uint32_t in[SIOPS_N], out[SIOPS_N], want, w[7];
	char name[64];
	uint64_t code_va, src_va, vsrc_va;
	int code, src, vsrc, r, bad;
	u_int i, k, v;

	memset(out, 0, sizeof(out));
	code = bo_alloc(4096, 0);
	src = bo_alloc(4096, 0);
	vsrc = bo_alloc(4096, ATRIUM_GPU_BO_VRAM);
	if (code < 0 || src < 0 || vsrc < 0 ||
	    vm_bind(a->vm, code, ATRIUM_GPU_BIND_READONLY, &code_va) != 0 ||
	    vm_bind(a->vm, src, ATRIUM_GPU_BIND_READONLY, &src_va) != 0 ||
	    vm_bind(a->vm, vsrc, 0, &vsrc_va) != 0) {
		check(0, "12 real ops", "setup: %s", strerror(errno));
		goto out;
	}
	for (i = 0; i < SIOPS_N; i++)
		in[i] = 7u + 3u * i;
	if (bo_xfer(src, in, sizeof(in), 1) != 0) {
		check(0, "12 real ops", "src: %s", strerror(errno));
		goto out;
	}
	/* The VRAM source: a CP DMA copy of the System one. */
	(void)dma_pkt(w, vsrc_va, src_va, sizeof(in), 0);
	if ((r = dma_run(a, w, 7)) != 0) {
		check(0, "12 real ops", "VRAM src copy: %s", strerror(r));
		goto out;
	}
	for (v = 0; v < 2; v++) {
		for (k = 0; k < sizeof(ops) / sizeof(ops[0]); k++) {
			if (poison(a->out, POISON_A) != 0) {
				check(0, "12 real ops", "poison: %s",
				    strerror(errno));
				goto out;
			}
			r = ops_run(a, code, code_va, ops[k].code, ops[k].bytes,
			    v ? vsrc_va : src_va, a->out_va, 0x600dbeefu);
			bad = -1;
			if (r == 0 && bo_xfer(a->out, out, sizeof(out), 0) == 0)
				for (i = 0, bad = 0; i < SIOPS_N; i++) {
					want = ops[k].op == 0 ? in[i] :
					    ops[k].op == 1 ? in[i] + 1 :
					    0x600dbeefu;
					bad += out[i] != want;
				}
			snprintf(name, sizeof(name), "12 real ops: %s, %s "
			    "source", ops[k].name, v ? "VRAM" : "System");
			check(r == 0 && bad == 0, name, "%s, %d/64 wrong; "
			    "out[0] %#x out[63] %#x", r == 0 ? "ok" :
			    strerror(r), bad, out[0], out[SIOPS_N - 1]);
		}
	}
out:
	if (code >= 0)
		close(code);
	if (src >= 0)
		close(src);
	if (vsrc >= 0)
		close(vsrc);
	run_clean(a, "12 real ops: write_42 runs on after");
}

/*
 * 13: GPU W^X (ABI 2.3 §5.4) with isolation.gpu_wx = 1 (client BOs only),
 * restored after: only pages bound EXEC (which needs READONLY) are
 * executable. The queue's writeback/EOP pages keep X (gpu_wx bit 2: box run
 * 05fdaebf lost a queue without it, and MES would not remove it). No probe
 * of an IB without X: a CP-side fault can wedge the queue the same way.
 */
#define WX_SYSCTL	"dev.atrium_gpu_amd.0.isolation.gpu_wx"

static int
wx_set(int v, int *old)
{
	size_t len = sizeof(*old);

	return (sysctlbyname(WX_SYSCTL, old, &len, &v, sizeof(v)));
}

static void
wx(void)
{
	struct job j;
	uint64_t va, data_va, code_va;
	uint32_t first;
	int old = 0, tmp, bo, data, r, n, ms;

	if (wx_set(1, &old) != 0) {
		check(0, "13 W^X", "set %s: %s", WX_SYSCTL, strerror(errno));
		return;
	}
	job_init(&j);
	data = -1;

	/* a: EXEC without READONLY is refused. */
	j.vm = vm_create();
	bo = bo_alloc(4096, 0);
	r = j.vm >= 0 && bo >= 0 &&
	    vm_bind(j.vm, bo, ATRIUM_GPU_BIND_EXEC, &va) != 0 ? errno : 0;
	check(r == EINVAL, "13 W^X: BIND_EXEC without READONLY is refused",
	    "%s", r == 0 ? "accepted" : strerror(r));
	if (bo >= 0)
		close(bo);

	/* b: shader and IB bound RO|EXEC, out RW (no X): write_42 runs. */
	j.shader = bo_alloc(4096, 0);
	j.ib = bo_alloc(4096, 0);
	j.out = bo_alloc(4096, 0);
	j.so = syncobj();
	if (j.vm < 0 || j.shader < 0 || j.ib < 0 || j.out < 0 || j.so < 0 ||
	    vm_bind(j.vm, j.shader, ATRIUM_GPU_BIND_READONLY |
	    ATRIUM_GPU_BIND_EXEC, &j.shader_va) != 0 ||
	    vm_bind(j.vm, j.ib, ATRIUM_GPU_BIND_READONLY |
	    ATRIUM_GPU_BIND_EXEC, &j.ib_va) != 0 ||
	    vm_bind(j.vm, j.out, 0, &j.out_va) != 0 ||
	    bo_xfer(j.shader, __DECONST(void *, siw42_shader),
	    sizeof(siw42_shader), 1) != 0 ||
	    (j.q = queue_create(j.vm)) < 0) {
		check(0, "13 W^X", "setup: %s", strerror(errno));
		goto out;
	}
	run_clean(&j, "13 W^X: code and IB bound EXEC run, data without X");

	/* c: the same shader in a read-write data BO (no X): the wave's
	 * instruction fetch faults and the work fails. */
	data = bo_alloc(4096, 0);
	if (data < 0 || vm_bind(j.vm, data, 0, &data_va) != 0 ||
	    bo_xfer(data, __DECONST(void *, siw42_shader),
	    sizeof(siw42_shader), 1) != 0) {
		check(0, "13 W^X", "data BO: %s", strerror(errno));
		goto out;
	}
	code_va = j.shader_va;
	j.shader_va = data_va;
	if (poison(j.out, POISON_A) != 0 || job_submit(&j, j.out_va) != 0) {
		check(0, "13 W^X: code in a data BO", "submit: %s",
		    strerror(errno));
		j.shader_va = code_va;
		goto out;
	}
	j.shader_va = code_va;
	r = job_wait(&j, 15000, &ms);
	n = count(j.out, SIW42_OUT_VALUE, &first);
	check(r == EIO && n == 0, "13 W^X: a jump into a read-write data BO "
	    "faults and fails the work", "completion %s after %d ms, %d/64 "
	    "words 42", r == 0 ? "ok" : strerror(r), ms, n);
	if (job_renew(&j) != 0) {
		check(0, "13 W^X: renew", "%s", strerror(errno));
		goto out;
	}
	run_clean(&j, "13 W^X: the VM runs clean on a new queue after");

out:
	if (data >= 0)
		close(data);
	job_close(&j);
	if (wx_set(old, &tmp) != 0)
		check(0, "13 W^X", "restore %s: %s", WX_SYSCTL,
		    strerror(errno));
}

static int
ioerr(int f, unsigned long cmd, void *arg)
{
	return (ioctl(f, cmd, arg) == 0 ? 0 : errno);
}

/* ---- 14: hostile PM4 (security model §10 item 1; silicon only) ---- */

#define PR_SYSCTL	"dev.atrium_gpu_amd_pci.0.si_protected_regs"
#define PR_MAX		8
#define PR_MARK		0x600d1400u	/* written after the hostile packet */
#define PR_REINIT_STOP	2		/* of SI_REINIT_MAX 3 per 10 min */

struct preg {
	char		name[40];
	uint32_t	dw;		/* dword register address */
	uint32_t	val;
	int		w;		/* the test may write it (same value) */
};

/* The kernel's target list; the count (-1: no sysctl, not silicon) and the
 * driver's re-init count (-1 if "lost"). */
static int
preg_read(struct preg *p, int *reinits)
{
	char buf[1024], *line, *s, mode[4];
	size_t len = sizeof(buf) - 1;
	unsigned long dw, v;
	u_int ri;
	int n = 0;

	if (sysctlbyname(PR_SYSCTL, buf, &len, NULL, 0) != 0)
		return (-1);
	buf[len] = '\0';
	*reinits = -1;
	for (s = buf; (line = strsep(&s, "\n")) != NULL;) {
		if (sscanf(line, "reinits %u", &ri) == 1) {
			*reinits = strstr(line, "lost") != NULL ? -1 : (int)ri;
			continue;
		}
		if (n < PR_MAX && p != NULL &&
		    sscanf(line, "%39s %lx %lx %3s", p[n].name, &dw, &v,
		    mode) == 4) {
			p[n].dw = (uint32_t)dw;
			p[n].val = (uint32_t)v;
			p[n].w = mode[0] == 'w';
			n++;
		}
	}
	return (n);
}

static uint32_t
preg_now(const char *name, int *ok)
{
	struct preg p[PR_MAX];
	int i, n, ri;

	*ok = 0;
	n = preg_read(p, &ri);
	for (i = 0; i < n; i++)
		if (strcmp(p[i].name, name) == 0) {
			*ok = 1;
			return (p[i].val);
		}
	return (0);
}

/* WRITE_DATA of `v` to register `dw` (dst_sel 0, write confirm). */
static u_int
pr_write_data(uint32_t *w, uint32_t dw, uint32_t v)
{
	w[0] = PKT3(0x37u, 3);
	w[1] = 1u << 20;
	w[2] = dw;
	w[3] = 0;
	w[4] = v;
	return (5);
}

/* COPY_DATA immediate `v` -> register `dw` (src_sel 5, dst_sel 0). */
static u_int
pr_copy_imm(uint32_t *w, uint32_t dw, uint32_t v)
{
	w[0] = PKT3(0x40u, 4);
	w[1] = 5u | 0u << 8 | 1u << 20;
	w[2] = v;
	w[3] = 0;
	w[4] = dw;
	w[5] = 0;
	return (6);
}

/* COPY_DATA register `dw` -> memory `va` (src_sel 0, dst_sel 5). */
static u_int
pr_copy_read(uint32_t *w, uint32_t dw, uint64_t va)
{
	w[0] = PKT3(0x40u, 4);
	w[1] = 0u | 5u << 8 | 1u << 20;
	w[2] = dw;
	w[3] = 0;
	w[4] = (uint32_t)va;
	w[5] = (uint32_t)(va >> 32);
	return (6);
}

/* WRITE_DATA of `v` to memory `va` (dst_sel 5). */
static u_int
pr_mark(uint32_t *w, uint64_t va, uint32_t v)
{
	w[0] = PKT3(0x37u, 3);
	w[1] = 5u << 8 | 1u << 20;
	w[2] = (uint32_t)va;
	w[3] = (uint32_t)(va >> 32);
	w[4] = v;
	return (5);
}

enum pr_kind { PR_WRITE_DATA, PR_COPY_IMM, PR_READ };
static const char *const pr_kname[] = { "WRITE_DATA", "COPY_DATA imm",
    "COPY_DATA read" };

/* What a hostile job's child reports. */
struct pr_res {
	int	err, ms, refused, ran, setup_err, requeue_err;
	uint32_t got;
};

/*
 * One hostile job on a fresh VM, in a child that runs as `uid` (the
 * device fd is inherited, so its calls count against that uid): the packet,
 * then a marker write to its own out page. Afterwards the child asks for a
 * new queue on a new VM: a uid whose queue forced a GPU reset is refused
 * (EPERM). *cpe: CP error IVs counted meanwhile. 0, or -1 if no child.
 */
static int
pr_job(enum pr_kind k, uint32_t dw, uint32_t v, uid_t uid, struct pr_res *res,
    int64_t *cpe)
{
	struct job j;
	struct pr_res r;
	uint32_t w[16], out[2];
	int64_t c0;
	u_int n = 0;
	int pfd[2], st, vm, q;
	pid_t pid;

	memset(res, 0, sizeof(*res));
	res->setup_err = EIO;
	if (pipe(pfd) != 0)
		return (-1);
	c0 = iso("cp_errors");
	if ((pid = fork()) < 0) {
		close(pfd[0]);
		close(pfd[1]);
		return (-1);
	}
	if (pid == 0) {
		close(pfd[0]);
		memset(&r, 0, sizeof(r));
		if (setuid(uid) != 0) {
			r.setup_err = errno;
			goto report;
		}
		if (job_open(&j, "14") != 0 || poison(j.out, POISON_B) != 0) {
			r.setup_err = errno != 0 ? errno : EIO;
			goto report;
		}
		if (k == PR_WRITE_DATA)
			n = pr_write_data(w, dw, v);
		else if (k == PR_COPY_IMM)
			n = pr_copy_imm(w, dw, v);
		else
			n = pr_copy_read(w, dw, j.out_va + 4);
		n += pr_mark(w + n, j.out_va, PR_MARK);
		if (bo_xfer(j.ib, w, n * 4, 1) != 0 ||
		    (r.err = queue_submit(j.q, j.ib_va, n, j.so, ++j.value,
		    NULL)) != 0) {
			r.setup_err = r.err != 0 ? r.err : errno;
			goto report;
		}
		r.err = job_wait(&j, 15000, &r.ms);
		if (bo_xfer(j.out, out, sizeof(out), 0) != 0)
			out[0] = out[1] = POISON_B;
		r.ran = r.err == 0 && out[0] == PR_MARK;
		r.refused = r.err != 0 && out[0] != PR_MARK;
		r.got = out[1];
		job_close(&j);
		/* A new queue for the same uid, after whatever happened. */
		usleep(500000);
		vm = vm_create();
		q = vm >= 0 ? queue_create(vm) : -1;
		r.requeue_err = q >= 0 ? 0 : errno;
		if (q >= 0)
			close(q);
		if (vm >= 0)
			close(vm);
report:
		(void)write(pfd[1], &r, sizeof(r));
		_exit(0);
	}
	close(pfd[1]);
	if (read(pfd[0], res, sizeof(*res)) != sizeof(*res))
		res->setup_err = EIO;
	close(pfd[0]);
	(void)waitpid(pid, &st, 0);
	*cpe = iso("cp_errors") - c0;
	return (0);
}

/* The order the targets run in: the ones a box run has not covered yet
 * first (box run 4efb65df stopped after the two VM context bases). */
static const char *const pr_order[] = { "ih_rb_base", "cp_mes_cntl",
    "gc_vm_ctx0_pt_base_lo", "mm_vm_ctx0_pt_base_lo", "smu_msg_arg" };

static int
pr_rank(const char *name)
{
	u_int i;

	for (i = 0; i < (sizeof(pr_order) / sizeof(pr_order[0])); i++)
		if (strcmp(pr_order[i], name) == 0)
			return ((int)i);
	return ((int)(sizeof(pr_order) / sizeof(pr_order[0])));
}

/*
 * 15: hostile ISA (F6 G5). The app path runs the app's own machine code, so
 * only the VMID confines it: a shader that never ends and shaders that store
 * where nothing of theirs is mapped must lose only their own queue, leave a
 * bystander VM's memory untouched, and let it run on. Measures the time to
 * fail on this GPU.
 */
#define HI_SPIN		0xbfa0ffffu	/* s_branch -1: branch to itself */

static void
hostile_isa(void)
{
	static const uint32_t spin[4] = { HI_SPIN, HI_SPIN, HI_SPIN, HI_SPIN };
	struct job h, by;
	struct {
		const char *name;
		int spin;
		uint64_t va;		/* 0: from the job (past its buffer) */
	} c[] = {
		/* Box run 1b71cafd: after the first two the GPU stayed busy
		 * and no new VM could be set up. One at a time, VA 0 last,
		 * each followed by a fresh VM: which one breaks it. */
		{ "stores at a high VA", 0, 0x7ffffff00000ull },
		{ "stores past its own buffer", 0, 0 },
		{ "a shader that never ends", 1, 0 },
		{ "stores at VA 0", 0, 1 },
	};
	struct job hc;
	uint32_t first;
	u_int i, nc = sizeof(c) / sizeof(c[0]);
	int r, n, ms, ri0 = -1, ri = -1, bylate;
	char what[160], vaname[64];
	const char *env;

	/*
	 * Box bisection (runs 6eca95c0, 40f28b9a): SI15_VA=<hex> runs just
	 * one store at that VA (1 = VA 0); SI15_SPIN=1 just the endless
	 * shader. The GPU wedge, once there, stays until a reset, so one
	 * probe per load.
	 */
	if ((env = getenv("SI15_VA")) != NULL) {
		c[0].va = strtoull(env, NULL, 16);
		c[0].spin = 0;
		snprintf(vaname, sizeof(vaname), "stores at VA %#jx",
		    (uintmax_t)(c[0].va == 1 ? 0 : c[0].va));
		c[0].name = vaname;
		nc = 1;
	} else if ((env = getenv("SI15_SPIN")) != NULL && *env == '1') {
		c[0] = c[2];
		nc = 1;
	}
	/*
	 * SI15_BYLATE=1 (wedge bisection, run 07740286: every wedging fault
	 * was on VMID 12 with the bystander on 11, every clean one on 11):
	 * the hostile queue opens first, so it gets the lower slot and
	 * VMID, and the bystander opens after it. Wedge still: two live
	 * VMs; clean: something about VMID 12.
	 */
	bylate = (env = getenv("SI15_BYLATE")) != NULL && *env == '1';
	job_init(&by);
	(void)preg_read(NULL, &ri0);
	if (!bylate && job_open(&by, "15 bystander") != 0) {
		check(0, "15 hostile ISA", "bystander: %s", strerror(errno));
		job_close(&by);
		return;
	}
	for (i = 0; i < nc; i++) {
		uint64_t va;

		if (job_open(&h, "15 hostile") != 0) {
			check(0, "15 hostile ISA", "%s: setup: %s", c[i].name,
			    strerror(errno));
			job_close(&h);
			continue;
		}
		if (bylate && i == 0) {
			printf("15: bystander opened after the hostile queue "
			    "(SI15_BYLATE)\n");
			if (job_open(&by, "15 bystander") != 0) {
				check(0, "15 hostile ISA", "bystander: %s",
				    strerror(errno));
				job_close(&by);
				job_close(&h);
				return;
			}
		}
		va = c[i].va == 1 ? 0 : c[i].va != 0 ? c[i].va :
		    h.out_va + 4096;
		if (c[i].spin && bo_xfer(h.shader, __DECONST(void *, spin),
		    sizeof(spin), 1) != 0) {
			check(0, "15 hostile ISA", "%s: shader: %s", c[i].name,
			    strerror(errno));
			job_close(&h);
			continue;
		}
		/*
		 * SI15_PREFREE=1 (wedge bisection, run 9267a45b: with the
		 * hang tests left out it still wedges): test 7, which
		 * recovers, frees a BO of its VM just before its store, so
		 * MES suspends and resumes its queue first. Do the same
		 * here: bind a page, free it, then submit.
		 */
		if ((env = getenv("SI15_PREFREE")) != NULL && *env == '1') {
			r = bo_bind_close(h.vm);
			printf("15: a BO of the hostile VM freed before the "
			    "submit (SI15_PREFREE): %s\n", r == 0 ? "ok" :
			    strerror(errno));
		}
		(void)poison(by.out, POISON_B);
		if (job_submit(&h, c[i].spin ? h.out_va : va) != 0) {
			check(0, "15 hostile ISA", "%s: submit: %s", c[i].name,
			    strerror(errno));
			job_close(&h);
			continue;
		}
		r = job_wait(&h, 15000, &ms);
		snprintf(what, sizeof(what), "15 hostile ISA: %s (VA %#jx) is "
		    "lost with EIO", c[i].name, (uintmax_t)(c[i].spin ?
		    h.out_va : va));
		check(r == EIO, what, "got %s after %d ms", r == 0 ?
		    "success" : strerror(r), ms);
		printf("15: %s: time to fail %d ms\n", c[i].name, ms);
		n = count(by.out, POISON_B, &first);
		snprintf(what, sizeof(what), "15 hostile ISA: %s: the "
		    "bystander's memory is untouched", c[i].name);
		check(n == SIW42_OUT_DW, what, "%d/64 words intact, out[0] %#x",
		    n, first);
		r = job_submit(&h, h.out_va) == 0 ? 0 : errno;
		snprintf(what, sizeof(what), "15 hostile ISA: %s: the lost "
		    "queue refuses new work", c[i].name);
		check(r == EIO, what, "%s", r == 0 ? "accepted" : strerror(r));
		snprintf(what, sizeof(what), "15 hostile ISA: after %s, "
		    "another VM runs clean", c[i].name);
		run_clean(&by, what);
		job_close(&h);
		/* A fresh VM: setting one up flushes its context. */
		sleep(2);
		r = job_open(&hc, "15 health") == 0 ? 0 : errno;
		snprintf(what, sizeof(what), "15 hostile ISA: after %s, a new "
		    "VM can be set up and runs clean", c[i].name);
		if (r != 0) {
			check(0, what, "setup: %s; the rest is not run",
			    strerror(r));
			job_close(&hc);
			break;
		}
		run_clean(&hc, what);
		job_close(&hc);
	}
	job_close(&by);
	if (preg_read(NULL, &ri) >= 0)
		check(ri == ri0, "15 hostile ISA: no GPU re-init was needed",
		    "re-inits %d -> %d", ri0, ri);
}

static void
hostile_pm4(void)
{
	struct preg p[PR_MAX], tmp;
	struct pr_res res;
	struct job by;
	enum pr_kind k;
	uint32_t got, now;
	uid_t uid = 60000 + (uid_t)(getpid() % 500) * 10;
	int64_t cpe;
	int n, i, x, ri0, ri, refused, ran, ok, stopped = 0, pass, resets;
	char what[160];

	n = preg_read(p, &ri0);
	if (n < 0) {
		printf("SKIP: 14 hostile PM4: no %s (not silicon)\n",
		    PR_SYSCTL);
		return;
	}
	check(n >= 5 && ri0 >= 0, "14 hostile PM4: the target list",
	    "%d registers, re-inits so far %d", n, ri0);
	for (i = 1; i < n; i++)		/* insertion sort by pr_rank */
		for (x = i; x > 0 && pr_rank(p[x].name) <
		    pr_rank(p[x - 1].name); x--) {
			tmp = p[x];
			p[x] = p[x - 1];
			p[x - 1] = tmp;
		}
	for (i = 0; i < n; i++)
		printf("14: target %s at dword %#x = %#x (%s)\n", p[i].name,
		    p[i].dw, p[i].val, p[i].w ? "write same value" :
		    "read only");
	if (job_open(&by, "14 bystander") != 0) {
		check(0, "14 hostile PM4", "bystander: %s", strerror(errno));
		job_close(&by);
		return;
	}

	/*
	 * Writes first (the ones that matter), then reads. Each job is a
	 * fresh VM; between jobs a bystander VM must run clean. A refusal
	 * that costs a whole-GPU re-init is counted; at PR_REINIT_STOP the
	 * rest are not run, so the box keeps one re-init in hand.
	 */
	for (k = PR_WRITE_DATA; k <= PR_READ && !stopped; k++) {
		for (i = 0; i < n && !stopped; i++) {
			if (k != PR_READ && !p[i].w)
				continue;
			uid++;
			(void)preg_read(NULL, &x);
			if (pr_job(k, p[i].dw, p[i].val, uid, &res, &cpe) != 0 ||
			    res.setup_err != 0) {
				snprintf(what, sizeof(what), "14 %s %s",
				    pr_kname[k], p[i].name);
				check(0, what, "setup as uid %u: %s", (u_int)uid,
				    strerror(res.setup_err != 0 ?
				    res.setup_err : errno));
				continue;
			}
			refused = res.refused;
			ran = res.ran;
			got = res.got;
			now = preg_now(p[i].name, &ok);
			(void)preg_read(NULL, &ri);
			resets = x >= 0 && ri >= 0 ? ri - x : -1;
			printf("14: %s %s as uid %u: %s after %d ms, marker %s, "
			    "CP errors +%jd, word %#x, register now %#x, "
			    "re-inits %d (+%d), the uid's next queue: %s\n",
			    pr_kname[k], p[i].name, (u_int)uid, res.err == 0 ?
			    "completed" : strerror(res.err), res.ms, ran ?
			    "written" : "not written", (intmax_t)cpe, got, now,
			    ri, resets, res.requeue_err == 0 ? "created" :
			    strerror(res.requeue_err));
			/* The reset rule: a uid whose queue forced a GPU
			 * reset gets no new queue; one that did not, does. */
			snprintf(what, sizeof(what), "14 after %s to %s: the "
			    "uid's next queue is %s", pr_kname[k], p[i].name,
			    resets > 0 ? "refused (it forced a GPU reset)" :
			    "created (no GPU reset)");
			check(resets > 0 ? res.requeue_err == EPERM :
			    res.requeue_err == 0, what, "%s", res.requeue_err ==
			    0 ? "created" : strerror(res.requeue_err));
			snprintf(what, sizeof(what), "14 %s to %s: refused",
			    pr_kname[k], p[i].name);
			if (k == PR_READ) {
				/* A leak: the real value read back. A value of
				 * 0 cannot tell a leak from a dropped read. */
				pass = refused || (ran && got != p[i].val);
				snprintf(what, sizeof(what), "14 COPY_DATA "
				    "read of %s: refused or not the real value",
				    p[i].name);
				if (!refused && ran && got == 0 &&
				    p[i].val == 0)
					printf("INFO: %s (inconclusive: the "
					    "register is 0, so a leak cannot be "
					    "told from a dropped read)\n", what);
				else
					check(pass, what, "%s, read %#x, real "
					    "%#x", refused ? "refused" : ran ?
					    "ran" : "neither", got, p[i].val);
			} else {
				/* Only a refusal proves the protection: a
				 * same-value write that ran may have landed. */
				pass = refused && ok && now == p[i].val;
				check(pass, what, "%s, CP errors +%jd, "
				    "register %#x -> %#x", refused ?
				    "refused" : ran ? "ran, not refused "
				    "(same value: no harm, but landing not "
				    "ruled out)" : "neither", (intmax_t)cpe,
				    p[i].val, now);
			}
			if (ri0 >= 0 && (ri < 0 || ri - ri0 >= PR_REINIT_STOP)) {
				printf("14: %d GPU re-init(s) since the start "
				    "of 14 (cap 3 per 10 min): the rest is "
				    "not run\n", ri < 0 ? -1 : ri - ri0);
				stopped = 1;
				break;
			}
			if (resets != 0) {
				/* The bystander's VM went with the reset. */
				job_close(&by);
				if (job_open(&by, "14 bystander") != 0)
					break;
			}
			snprintf(what, sizeof(what), "14 after %s to %s: "
			    "another VM runs clean", pr_kname[k], p[i].name);
			run_clean(&by, what);
		}
	}

	(void)preg_read(NULL, &ri);
	if (ri == ri0)
		check(1, "14 hostile PM4: a refused packet costs its queue, "
		    "not a whole-GPU re-init", "re-inits %d -> %d", ri0, ri);
	else
		printf("KNOWN: 14 hostile PM4: refused packets cost %d "
		    "whole-GPU re-init(s)%s; the uids that forced them were "
		    "locked out (per-queue recovery still owed)\n",
		    ri < 0 ? -1 : ri - ri0, stopped ? ", stopped early" : "");
	job_close(&by);
	if (job_open(&by, "14 after") == 0)
		run_clean(&by, "14 hostile PM4: a new VM runs clean after");
	job_close(&by);
}

int
main(void)
{
	struct job a, b, c, e, f;
	struct si_old_submit s;
	struct atrium_gpu_queue_submit qs;
	struct atrium_gpu_queue_create qc;
	uint64_t seq1 = 0, seq2 = 0;
	uint32_t first;
	int r, n, ms, q2, sv[2], st;
	pid_t pid;

	setvbuf(stdout, NULL, _IOLBF, 0);
	job_init(&a);
	job_init(&b);
	job_init(&c);
	job_init(&e);
	job_init(&f);
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
	r = ioerr(fd, SI_OLD_IOC_SUBMIT, &s);
	check(r == ENOTTY, "6 the retired ring SUBMIT ('A',4) is unknown",
	    "%s", r == 0 ? "accepted" : strerror(r));
	r = queue_create2(a.vm, 7, 0, 0) >= 0 ? 0 : errno;
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
	run_clean(&a, "6 the first queue still runs after the refusals");
	(void)seq2;

	/* 7: a fault loses only its own queue (needs a free VM: drop 4's). */
	job_close(&e);
	if (job_open(&f, "7") == 0) {
		uint64_t target = f.out_va;

		close(f.out);		/* unbound: the VA faults in F */
		f.out = -1;
		if (job_submit(&f, target) != 0) {
			check(0, "7 fault", "submit: %s", strerror(errno));
		} else {
			r = job_wait(&f, 8000, &ms);
			check(r == EIO, "7 fault: the faulting queue's work fails "
			    "with EIO", "got %s after %d ms", r == 0 ? "success" :
			    strerror(r), ms);
			r = job_submit(&f, target) == 0 ? 0 : errno;
			check(r == EIO, "7 fault: the lost queue refuses new "
			    "work", "%s", r == 0 ? "accepted" : strerror(r));
			run_clean(&a, "7 fault: another VM's queue runs on");
			if (job_renew(&f) != 0)
				check(0, "7 fault: renew", "%s",
				    strerror(errno));
			else
				run_clean(&f, "7 fault: the faulting VM runs "
				    "clean on a new queue");
		}
	}

	/* 8: a job that never ends loses its queue after hang_ms. */
	if (f.vm >= 0 && f.q >= 0 && f.out >= 0 && !skiphang("8 hang")) {
		uint32_t w[7];

		(void)poison(f.out, POISON_A);
		w[0] = 3u << 30 | 5u << 16 | 0x3cu << 8;  /* WAIT_REG_MEM */
		w[1] = 3u | 1u << 4;		/* equal, memory */
		w[2] = (uint32_t)f.out_va;
		w[3] = (uint32_t)(f.out_va >> 32);
		w[4] = 0x12345678u;		/* never written */
		w[5] = 0xffffffffu;
		w[6] = 0x20u;			/* poll interval */
		if (bo_xfer(f.ib, w, sizeof(w), 1) != 0 ||
		    (r = queue_submit(f.q, f.ib_va, 7, f.so, ++f.value,
		    NULL)) != 0) {
			check(0, "8 hang", "submit: %s", strerror(errno));
		} else {
			r = job_wait(&f, 15000, &ms);
			check(r == EIO && ms >= 3000, "8 hang: the hung queue's "
			    "work fails with EIO after hang_ms", "got %s after "
			    "%d ms", r == 0 ? "success" : strerror(r), ms);
			r = queue_submit(f.q, f.ib_va, 7, -1, 0, NULL);
			check(r == EIO, "8 hang: the lost queue refuses new work",
			    "%s", r == 0 ? "accepted" : strerror(r));
			run_clean(&a, "8 hang: another VM's queue runs on");
			if (job_renew(&f) != 0)
				check(0, "8 hang: renew", "%s",
				    strerror(errno));
			else
				run_clean(&f, "8 hang: the hung VM runs clean on "
				    "a new queue");
		}
	}

	/* 9: user-mode queues (needs a free VM: drop 8's). */
	job_close(&f);
	user_mode(&a);

	/* 10: the free rule. */
	free_rule(&a);

	/* 11: VRAM BOs (5d). */
	vram(&a);

	/* 12: real-shader variants of gpusim's compute ops (5e). */
	real_ops(&a);

	/* 13: GPU W^X. SI_SKIP13=1 leaves it out (box bisection: every run
	 * that wedged the GFXHUB at a 15 store fault had 13's SQC
	 * instruction fault just before it; test 7's store fault, before
	 * 13, never wedged). */
	if (getenv("SI_SKIP13") == NULL || *getenv("SI_SKIP13") != '1')
		wx();
	else
		printf("SKIP: 13 GPU W^X (SI_SKIP13)\n");

	/* 15: hostile ISA (F6 G5). Before 14, whose refusals use up re-inits. */
	hostile_isa();

	/* 14: hostile PM4 (silicon only). */
	hostile_pm4();

out:
	job_close(&a);
	job_close(&b);
	job_close(&c);
	job_close(&e);
	job_close(&f);
	close(fd);
	printf("si_client_test: %d passed, %d failed: %s\n", npass, nfail,
	    nfail == 0 && npass > 0 ? "PASS" : "FAIL");
	return (nfail == 0 && npass > 0 ? 0 : 1);
}
