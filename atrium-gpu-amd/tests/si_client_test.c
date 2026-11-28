/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * si_client_test.c — stage 5c-2b step 1 on the R9700
 * (docs/spec/r9700-bare-metal.md §4.5.1): client work on queue fds, each a
 * MES user queue of its own, driven from userspace through
 * atrium_gpu_amd_abi.h (QUEUE_CREATE on a VM fd, QUEUE_SUBMIT on the queue
 * fd; v2 layouts). Cross-built static for FreeBSD amd64
 * (scripts/r9700/build-si-client-test.sh); run as root with the kmods
 * attached (kenv hw.atrium_gpu.client=1). Pass --render-only to run
 * the solid and interpolated GLSL triangles (22 and 23).
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
 *  17 dummy page one VM's shader stores a pattern to an unmapped VA; a
 *                fresh VM's shader then loads from an unmapped VA. Faulting
 *                writes go to the hub's one fault default page; the reader
 *                must not see the writer's pattern. With zero_va (default)
 *                empty VA reads as zeros without a fault.
 *  18 scale      (stage 6b) SCALE_VMS VMs at once, each with a queue, and
 *                SCALE_EXTRA more queues in the first VMs: far past the 7
 *                VMs and 8 queues silicon used to have, and past the 8
 *                VMIDs MES has, so MES time-shares them. Every queue runs
 *                write_42 onto its own page, all submitted before any is
 *                waited for, three rounds; every page must hold its own
 *                64 x 42 and nothing else's. Raises isolation.queue_limit
 *                for the run (root's queues count too) and puts it back.
 *  20 gfx queue  (stage 7a) with isolation.gfx_queues = 1 (refused while
 *                0): a GFX-family queue fd on an MES gfx user queue runs
 *                WRITE_DATA jobs with kqueue completion, many in a row,
 *                beside the VM's compute queue; write_42's dispatch on it
 *                is reported (INFO); a store to an unmapped VA loses only
 *                that queue (EIO) while another VM runs on; a new gfx
 *                queue in the same VM runs after.
 *  19 unbind     VM_UNBIND: flags and a BO not bound there are refused; a
 *                BO shared into four VMs and unbound from each no longer
 *                keeps them alive once their fds close (isolation.vms),
 *                while a binding left in place does until the BO closes;
 *                a BO unbound while work under its VM may use it, then
 *                closed, leaves nothing deferred once that work is done;
 *                an unbound page faults (EIO), the VM runs on a new
 *                queue and the range can be bound again at the same VA.
 *  21 triangle   (stage 7b) one draw on a gfx queue: an NGG vertex shader
 *                and a constant-colour pixel shader into a linear 64 x 64
 *                RGBA8 target, compared with a CPU rasterisation (pixels
 *                within half a pixel of an edge are not judged); the VS's
 *                system SGPRs/VGPRs are printed per lane. Runs after 20.
 *  22 GLSL triangle (stage 7c) the same draw with both shaders compiled
 *                from GLSL by our shader compiler (si_tri_glsl.h, from
 *                atrium-rdna4-compiler/examples/si_tri_glsl.rs). Runs
 *                after 21.
 *  23 colour triangle (stage 7c-2) varyings: the GLSL VS writes a colour
 *                per vertex and a flat int through the GFX12 attribute
 *                ring, the PS interpolates them; every inside pixel is
 *                checked against CPU barycentrics (within 3/255).
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

/* Code and IBs: read-only and executable (GPU W^X, on by default). */
#define BIND_CODE	(ATRIUM_GPU_BIND_READONLY | ATRIUM_GPU_BIND_EXEC)

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

/*
 * SI_SKIP=<n>[,<n>...] (wedge bisection, run 0f855443: test 15's store
 * fault wedged as the first fault of the load, while test 7's identical
 * fault recovers): leaves out whole tests 9-12, which run between them.
 */
static int
skipped(int n, const char *what)
{
	const char *e = getenv("SI_SKIP"), *p;
	char *end;

	for (p = e; p != NULL && *p != '\0'; p = *end == ',' ? end + 1 : end) {
		if (strtol(p, &end, 10) == n) {
			printf("SKIP: %d %s (SI_SKIP)\n", n, what);
			return (1);
		}
		if (end == p)
			break;
	}
	return (0);
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
	if (vm_bind(j->vm, j->shader, BIND_CODE, &j->shader_va) ||
	    vm_bind(j->vm, j->ib, BIND_CODE, &j->ib_va) ||
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
	    vm_bind(a->vm, code, BIND_CODE, &code_va) != 0 ||
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
 * 13: GPU W^X (ABI 2.3 §5.4) with isolation.gpu_wx = 1 (client BOs only;
 * the driver default since 6c), restored after: only pages bound EXEC (which needs READONLY) are
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
	int r, n, ms, ri0 = -1, ri = -1, bylate, noby;
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
	/*
	 * SI15_NOBY=1 (wedge bisection, run be7f9fa2: a BO free before the
	 * store did not help): no bystander VM at all. Test 7's other VM
	 * had long been running; test 15's bystander is a fresh VM and
	 * queue added just before the fault.
	 */
	noby = (env = getenv("SI15_NOBY")) != NULL && *env == '1';
	if (noby) {
		bylate = 1;
		printf("15: no bystander VM (SI15_NOBY)\n");
	}
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
		if (bylate && !noby && i == 0) {
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
		/*
		 * SI15_CLOSEOUT=1 (wedge bisection, run d8436096: no
		 * bystander, still wedged): exactly test 7's fault. Close
		 * the job's own out BO, then store to where it was, so the
		 * faulting page is one that was mapped and is now unmapped.
		 * Every wedging store so far hit a page never mapped.
		 */
		if ((env = getenv("SI15_CLOSEOUT")) != NULL && *env == '1' &&
		    !c[i].spin) {
			close(h.out);
			h.out = -1;
			va = h.out_va;
			printf("15: out BO closed, store at its old VA %#jx "
			    "(SI15_CLOSEOUT)\n", (uintmax_t)va);
		}
		if (!noby)
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
		if (!noby) {
			n = count(by.out, POISON_B, &first);
			snprintf(what, sizeof(what), "15 hostile ISA: %s: the "
			    "bystander's memory is untouched", c[i].name);
			check(n == SIW42_OUT_DW, what, "%d/64 words intact, "
			    "out[0] %#x", n, first);
		}
		r = job_submit(&h, h.out_va) == 0 ? 0 : errno;
		snprintf(what, sizeof(what), "15 hostile ISA: %s: the lost "
		    "queue refuses new work", c[i].name);
		check(r == EIO, what, "%s", r == 0 ? "accepted" : strerror(r));
		snprintf(what, sizeof(what), "15 hostile ISA: after %s, "
		    "another VM runs clean", c[i].name);
		if (!noby)
			run_clean(&by, what);
		/*
		 * SI15_QFIRST=1 (wedge bisection, run 6849e284): the first
		 * flush to time out is the one for freeing test 15's out BO
		 * (job_close frees the BOs before it closes the queue), on a
		 * VM whose queue is lost but still open. Test 7, which
		 * recovers, closes its lost queue first. Do that here.
		 */
		if ((env = getenv("SI15_QFIRST")) != NULL && *env == '1') {
			printf("15: lost queue closed before the BOs "
			    "(SI15_QFIRST)\n");
			close(h.q);
			h.q = -1;
			sleep(1);
		}
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

/*
 * 16: a fault storm (24 h run 2963f5db: a fuzzed ACQUIRE_MEM with a GL2
 * range op over the whole VA space made one queue raise ~8 M fault IVs
 * and overflow the IH ring 12125 times). One such packet: the queue is
 * lost, the IH ring does not overflow, and a bystander runs clean. Since
 * zero_va (caede735) empty VA no longer faults: the queue hangs and the
 * hang path resets the GPU, with no fault IVs at all.
 */
#define GCR_GL2_RANGE_RANGE	(2u << 11)
#define GCR_GL2_INV		(1u << 14)
#define GCR_GL2_WB		(1u << 15)

static void
fault_storm(void)
{
	struct job h, by;
	uint32_t ib[16];
	int64_t f0, f1, o0, o1;
	int r, ms, ri0 = -1, ri = -1, n = 0, x;
	char what[160];

	job_init(&h);
	job_init(&by);
	if (job_open(&by, "16 bystander") != 0 || job_open(&h, "16 storm")
	    != 0) {
		check(0, "16 fault storm", "setup: %s", strerror(errno));
		goto out;
	}
	(void)preg_read(NULL, &ri0);
	ib[n++] = PKT3(0x58, 6);		/* ACQUIRE_MEM */
	ib[n++] = 0;				/* CP_COHER_CNTL */
	ib[n++] = 0xffffffffu;			/* CP_COHER_SIZE */
	ib[n++] = 0x00ffffffu;			/* CP_COHER_SIZE_HI */
	ib[n++] = 0;				/* CP_COHER_BASE */
	ib[n++] = 0;				/* CP_COHER_BASE_HI */
	ib[n++] = 0x0000000a;			/* POLL_INTERVAL */
	ib[n++] = GCR_GL2_RANGE_RANGE | GCR_GL2_INV | GCR_GL2_WB;
	if (bo_xfer(h.ib, ib, (size_t)n * 4, 1) != 0) {
		check(0, "16 fault storm", "IB: %s", strerror(errno));
		goto out;
	}
	f0 = iso("vm_faults");
	o0 = iso("ih_overflows");
	r = queue_submit(h.q, h.ib_va, (uint32_t)n, h.so, ++h.value, NULL);
	if (r != 0) {
		check(0, "16 fault storm", "submit: %s", strerror(r));
		goto out;
	}
	r = job_wait(&h, 15000, &ms);
	sleep(2);
	/*
	 * With zero_va (caede735) the packet raises no fault: the queue
	 * hangs, and the hang path (REMOVE refused, MES RESET refused) resets
	 * the GPU some 4-7 s after the hang is seen. Wait up to 20 s for that
	 * re-init to start, then for it to finish (a re-init in progress
	 * reads as lost, -1).
	 */
	if (r != 0)
		for (x = 0; x < 100 && preg_read(NULL, &ri) >= 0 && ri == ri0;
		    x++)
			usleep(200000);
	for (x = 0; x < 150 && (preg_read(NULL, &ri) < 0 || ri < 0); x++)
		usleep(200000);
	f1 = iso("vm_faults");
	o1 = iso("ih_overflows");
	printf("16: range ACQUIRE_MEM: %s after %d ms; fault IVs +%jd, IH "
	    "overflows +%jd\n", r == 0 ? "completed" : strerror(r), ms,
	    (intmax_t)(f1 - f0), (intmax_t)(o1 - o0));
	if (r == 0 && f1 == f0)
		printf("KNOWN: 16 fault storm: the packet raised no fault on "
		    "this GPU; nothing to mute\n");
	else {
		/* Run 5eff3373: +3.8 M IVs, +1867 overflows over 4 s before
		 * the driver reset the GPU. Bounded now: a storm that outlives
		 * the mute goes straight to the reset. */
		check(f1 - f0 < 100000, "16 fault storm: the storm is cut "
		    "short", "+%jd fault IVs, re-inits %d -> %d",
		    (intmax_t)(f1 - f0), ri0, ri);
		check(o1 - o0 < 50, "16 fault storm: the IH ring barely "
		    "overflows", "+%jd overflows", (intmax_t)(o1 - o0));
	}
	job_close(&h);
	if (ri != ri0) {
		/* A GPU reset loses every VM: the bystander too. */
		job_close(&by);
		if (job_open(&by, "16 bystander after the reset") != 0) {
			check(0, "16 fault storm: afterwards another VM runs "
			    "clean", "setup: %s", strerror(errno));
			goto out;
		}
	}
	snprintf(what, sizeof(what), "16 fault storm: afterwards another VM "
	    "runs clean");
	run_clean(&by, what);
	printf("16: re-inits %d -> %d\n", ri0, ri);
out:
	job_close(&h);
	job_close(&by);
}

/*
 * 17: the fault default page. A faulting access is sent to one default
 * page (gmc12: the dummy page, GCVM_L2_PROTECTION_FAULT_DEFAULT_ADDR,
 * shared by every VM). Writer VM W's FILL shader stores a pattern to an
 * unmapped VA; reader VM R's COPY shader then loads from an unmapped VA
 * into its own out page. Any pattern word in R's out page is data that
 * crossed from one VM to another. Both queues are lost to their faults
 * (shader faults, which recover without a GPU reset).
 */
#define DUMMY_VA	0xf00000000ull		/* unmapped in every VM */
#define DUMMY_PAT	0x5ec2e700u
static int dummy_zeros;			/* of the last dummy_read */

static int
dummy_job(struct job *j, const char *name, const uint32_t *code,
    size_t bytes, uint64_t src, uint64_t dst, uint32_t value, int *cbo)
{
	uint64_t code_va;

	if (job_open(j, name) != 0)
		return (errno);
	*cbo = bo_alloc(4096, 0);
	if (*cbo < 0 || vm_bind(j->vm, *cbo, BIND_CODE, &code_va) != 0)
		return (errno);
	return (ops_run(j, *cbo, code_va, code, bytes, src, dst, value));
}

/* Copy the unmapped page into a fresh VM's out page: how many words carry
 * the pattern (-1 if the reader could not be set up or read back), and
 * the first word. */
static int
dummy_read(const char *name, uint32_t *first, int *rr)
{
	struct job r;
	uint32_t out[SIOPS_N];
	uint64_t code_va;
	int cbo = -1, n = -1;
	u_int i;

	job_init(&r);
	*rr = 0;
	if (job_open(&r, name) != 0 || poison(r.out, POISON_A) != 0 ||
	    (cbo = bo_alloc(4096, 0)) < 0 ||
	    vm_bind(r.vm, cbo, BIND_CODE, &code_va) != 0) {
		*rr = errno;
		goto out;
	}
	*rr = ops_run(&r, cbo, code_va, siops_copy, sizeof(siops_copy),
	    DUMMY_VA, r.out_va, 0);
	if (bo_xfer(r.out, out, sizeof(out), 0) != 0)
		goto out;
	*first = out[0];
	for (i = 0, n = 0, dummy_zeros = 0; i < SIOPS_N; i++) {
		n += (out[i] & 0xffffff00u) == DUMMY_PAT;
		dummy_zeros += out[i] == 0;
	}
out:
	if (cbo >= 0)
		close(cbo);
	job_close(&r);
	return (n);
}

static void
dummy_page(void)
{
	struct job w, by;
	uint32_t first = 0;
	int cbo = -1, rw, rr, n0, n1, ri0 = -1, ri = -1;
	int64_t d0, d1;

	job_init(&w);
	job_init(&by);
	(void)preg_read(NULL, &ri0);
	n0 = dummy_read("17 reader before", &first, &rr);
	printf("17: before any write: %d/64 pattern words, out[0] %#x (%s)\n",
	    n0, first, rr == 0 ? "completed" : strerror(rr));
	d0 = iso("dummy_dirty");
	rw = dummy_job(&w, "17 writer", siops_fill, sizeof(siops_fill), 0,
	    DUMMY_VA, DUMMY_PAT | 0x42, &cbo);
	printf("17: writer's store to the unmapped VA: %s\n", rw == 0 ?
	    "completed" : strerror(rw));
	if (cbo >= 0)
		close(cbo);
	job_close(&w);
	usleep(200000);		/* the evict task scrubs after the kill */
	d1 = iso("dummy_dirty");
	/* The positive control: the writer's faulting stores reached the
	 * shared fault page (the kernel found it written and zeroed it). */
	printf("17: fault page scrubs that found it written: +%jd\n",
	    (intmax_t)(d1 - d0));
	n1 = dummy_read("17 reader after", &first, &rr);
	/*
	 * zero_va on (the reader's load completes): empty VA is a valid
	 * read-only page, so the store is a write-permission fault that never
	 * reaches the fault page (caede735 on silicon: status 0xc41040, 0
	 * dirty scrubs). zero_va off: the store does reach it, and the kernel
	 * must find it written and zero it.
	 */
	if (d1 >= 0 && d0 >= 0 && rr == 0)
		check(d1 == d0, "17 dummy page: a store to unmapped VA does "
		    "not reach the fault page", "+%jd dirty scrubs",
		    (intmax_t)(d1 - d0));
	else if (d1 >= 0 && d0 >= 0)
		check(d1 > d0, "17 dummy page: the writer's faulting stores "
		    "reach the fault page, and the kernel zeroes it",
		    "+%jd dirty scrubs", (intmax_t)(d1 - d0));
	(void)preg_read(NULL, &ri);
	printf("17: reader's load from the unmapped VA: %s; %d/64 pattern "
	    "words, out[0] %#x; re-inits %d -> %d\n", rr == 0 ? "completed" :
	    strerror(rr), n1, first, ri0, ri);
	if (n1 < 0)
		check(0, "17 dummy page", "reader setup or readback: %s",
		    strerror(errno));
	else
		check(n1 == 0, "17 dummy page: no VM reads another VM's "
		    "faulting writes", "%d/64 words carried the writer's "
		    "pattern (out[0] %#x)", n1, first);
	/* hw.atrium_gpu.zero_va (default on): empty client VA is a read-only
	 * zero page, so the load completes with zeros and never touches the
	 * fault default page. Off: the load faults (5e11d762: the wave never
	 * got past it, so the out page kept its poison). */
	if (rr == 0)
		check(dummy_zeros == SIOPS_N, "17 dummy page: unmapped VA "
		    "reads as zeros", "%d/64 words 0", dummy_zeros);
	else
		printf("KNOWN: 17 dummy page: the load from unmapped VA "
		    "faulted (%s): zero_va off?\n", strerror(rr));
	if (job_open(&by, "17 bystander") == 0)
		run_clean(&by, "17 dummy page: afterwards another VM runs "
		    "clean");
	job_close(&by);
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

/* 18: many VMs and queues at once (stage 6b). */
#define SCALE_VMS	40
#define SCALE_EXTRA	12
#define QLIMIT_SYSCTL	"dev.atrium_gpu_amd.0.isolation.queue_limit"

static void
scale(void)
{
	static struct job j[SCALE_VMS];
	int xq[SCALE_EXTRA], xso[SCALE_EXTRA], xout[SCALE_EXTRA];
	uint64_t xva[SCALE_EXTRA], xval[SCALE_EXTRA];
	int nv = 0, nx = 0, i, r, n, ms, round, bad, old, lim = 256;
	size_t len = sizeof(old);
	uint32_t first;
	char name[16];

	if (sysctlbyname(QLIMIT_SYSCTL, &old, &len, &lim, sizeof(lim)) != 0) {
		check(0, "18 scale", "%s: %s", QLIMIT_SYSCTL, strerror(errno));
		return;
	}
	for (i = 0; i < SCALE_VMS; i++) {
		snprintf(name, sizeof(name), "18-%d", i);
		if (job_open(&j[i], name) != 0) {
			job_close(&j[i]);
			break;
		}
		nv++;
	}
	check(nv == SCALE_VMS, "18 scale: VMs with a queue each open at once",
	    "%d of %d (the 5c limit was 7 VMs, 8 queues)", nv, SCALE_VMS);
	for (i = 0; i < SCALE_EXTRA && nv > 0; i++) {
		struct job *h = &j[i % nv];

		xq[i] = queue_create(h->vm);
		xso[i] = syncobj();
		xout[i] = bo_alloc(4096, 0);
		xval[i] = 0;
		if (xq[i] < 0 || xso[i] < 0 || xout[i] < 0 ||
		    vm_bind(h->vm, xout[i], 0, &xva[i]) != 0) {
			printf("18: extra queue %d: %s\n", i, strerror(errno));
			if (xq[i] >= 0)
				close(xq[i]);
			if (xso[i] >= 0)
				close(xso[i]);
			if (xout[i] >= 0)
				close(xout[i]);
			break;
		}
		nx++;
	}
	check(nx == SCALE_EXTRA, "18 scale: more queues in the same VMs",
	    "%d of %d (%d queues in all)", nx, SCALE_EXTRA, nv + nx);
	for (round = 0; round < 3 && nv > 0; round++) {
		bad = 0;
		/* Everything goes before anything is waited for. Each job's
		 * IB BO is its own, so a VM with an extra queue runs the IB
		 * for the extra page from a second IB slot: the shared IB is
		 * rewritten only once its first submit is done. */
		for (i = 0; i < nv; i++)
			if (poison(j[i].out, POISON_A + round + i) != 0 ||
			    job_submit(&j[i], j[i].out_va) != 0) {
				printf("18: round %d: submit %d: %s\n", round, i,
				    strerror(errno));
				bad++;
			}
		for (i = 0; i < nv; i++) {
			r = job_wait(&j[i], 10000, &ms);
			n = count(j[i].out, SIW42_OUT_VALUE, &first);
			if (r != 0 || n != SIW42_OUT_DW) {
				printf("18: round %d: VM %d: %s after %d ms, "
				    "%d/64, out[0] %#x\n", round, i, r == 0 ?
				    "ok" : strerror(r), ms, n, first);
				bad++;
			}
		}
		/* The extra queues: the same shader writing their own pages
		 * (each VM's IB now free again), all at once, then checked. */
		for (i = 0; i < nx; i++) {
			struct job *h = &j[i % nv];

			if (poison(xout[i], POISON_B + round + i) != 0 ||
			    job_ib(h, xva[i]) != 0 ||
			    queue_submit(xq[i], h->ib_va, SIW42_IB_DW, xso[i],
			    ++xval[i], NULL) != 0) {
				printf("18: round %d: extra %d: submit failed\n",
				    round, i);
				bad++;
				continue;
			}
			/* One VM's IB holds one job's VAs: wait before the
			 * next extra queue of the same VM rewrites it. */
			{
				struct job w = *h;

				w.so = xso[i];
				w.value = xval[i];
				r = job_wait(&w, 10000, &ms);
			}
			n = count(xout[i], SIW42_OUT_VALUE, &first);
			if (r != 0 || n != SIW42_OUT_DW) {
				printf("18: round %d: extra queue %d: %s, %d/64, "
				    "out[0] %#x\n", round, i, r == 0 ? "ok" :
				    strerror(r), n, first);
				bad++;
			}
		}
		check(bad == 0, "18 scale: every queue writes 64 x 42 on its "
		    "own page", "round %d: %d VMs, %d extra queues, %d bad",
		    round, nv, nx, bad);
	}
	for (i = 0; i < nx; i++) {
		close(xq[i]);
		close(xso[i]);
		close(xout[i]);
	}
	for (i = 0; i < nv; i++)
		job_close(&j[i]);
	(void)sysctlbyname(QLIMIT_SYSCTL, NULL, NULL, &old, sizeof(old));
	/* Everything closed: a fresh VM and queue still work. */
	{
		struct job k;

		if (job_open(&k, "18-after") == 0)
			run_clean(&k, "18 scale: a fresh VM runs after they all "
			    "closed");
		else
			check(0, "18 scale: a fresh VM after", "%s",
			    strerror(errno));
		job_close(&k);
	}
}

/* ---- 20: gfx queues (stage 7a) ---- */

#define GFXQ_SYSCTL	"dev.atrium_gpu_amd.0.isolation.gfx_queues"
#define GFXQ_MARK	0x7a000000u

static int
gfxq_set(int v, int *old)
{
	size_t len = sizeof(*old);

	return (sysctlbyname(GFXQ_SYSCTL, old, &len, &v, sizeof(v)));
}

/* WRITE_DATA of n words v, v+1, ... at va; returns dwords. */
static u_int
gfxq_write(uint32_t *w, uint64_t va, uint32_t v, u_int n)
{
	u_int i;

	w[0] = PKT3(0x37u, 3 + n - 1);
	w[1] = 5u << 8 | 1u << 20;	/* DST_SEL memory, WR_CONFIRM, ME */
	w[2] = (uint32_t)va;
	w[3] = (uint32_t)(va >> 32);
	for (i = 0; i < n; i++)
		w[4 + i] = v + i;
	return (4 + n);
}

/* Run `dw` dwords of j's IB on queue q; 0 or the completion error. */
static int
gfxq_run(struct job *j, int q, const uint32_t *w, u_int dw, int *ms)
{
	int r;

	if (bo_xfer(j->ib, __DECONST(void *, w), dw * 4, 1) != 0)
		return (errno);
	r = queue_submit(q, j->ib_va, dw, j->so, ++j->value, NULL);
	if (r != 0)
		return (r);
	return (job_wait(j, 4000, ms));
}

static void
gfx_queue(struct job *other)
{
	struct job j;
	uint32_t w[64], out[SIW42_OUT_DW], first;
	int old = 0, tmp, q = -1, r, ms, n, i;

	job_init(&j);
	if (gfxq_set(0, &old) != 0) {
		check(0, "20 gfx queue", "%s: %s", GFXQ_SYSCTL, strerror(errno));
		return;
	}
	if (job_open(&j, "20") != 0) {
		check(0, "20 gfx queue", "setup failed");
		goto out;
	}
	r = queue_create2(j.vm, ATRIUM_GPU_QUEUE_FAMILY_GFX, 0, 0) >= 0 ? 0 :
	    errno;
	check(r == EINVAL, "20 gfx queue: refused while gfx_queues is 0",
	    "%s", r == 0 ? "accepted" : strerror(r));
	if (gfxq_set(1, &tmp) != 0) {
		check(0, "20 gfx queue", "set: %s", strerror(errno));
		goto out;
	}
	q = queue_create2(j.vm, ATRIUM_GPU_QUEUE_FAMILY_GFX, 0, 0);
	check(q >= 0, "20 gfx queue: QUEUE_CREATE of family GFX", "%s",
	    q >= 0 ? "ok" : strerror(errno));
	if (q < 0)
		goto out;

	/* a: one WRITE_DATA job. */
	(void)poison(j.out, POISON_A);
	r = gfxq_run(&j, q, w, gfxq_write(w, j.out_va, GFXQ_MARK, 8), &ms);
	n = bo_xfer(j.out, out, sizeof(out), 0) == 0 ? 0 : -1;
	for (i = 0; n >= 0 && i < 8; i++)
		n += out[i] == GFXQ_MARK + (uint32_t)i;
	check(r == 0 && n == 8 && out[8] == POISON_A, "20 gfx queue: a "
	    "WRITE_DATA job runs and completes", "completion %s after %d ms, "
	    "%d/8 words, out[0] %#x, out[8] %#x", r == 0 ? "ok" : strerror(r),
	    ms, n, out[0], out[8]);

	/* b: 32 jobs in a row, each a word of its own. */
	(void)poison(j.out, POISON_A);
	for (i = 0, r = 0; i < 32 && r == 0; i++) {
		u_int dw = gfxq_write(w, j.out_va + 4 * (uint64_t)i,
		    GFXQ_MARK + 0x100 + (uint32_t)i, 1);

		if (bo_xfer(j.ib, w, dw * 4, 1) != 0)
			r = errno;
		else
			r = queue_submit(q, j.ib_va, dw, j.so, ++j.value,
			    NULL);
		if (r == 0)
			r = job_wait(&j, 4000, &ms);
	}
	n = bo_xfer(j.out, out, sizeof(out), 0) == 0 ? 0 : -1;
	for (i = 0; n >= 0 && i < 32; i++)
		n += out[i] == GFXQ_MARK + 0x100 + (uint32_t)i;
	check(r == 0 && n == 32, "20 gfx queue: 32 jobs in a row", "%s, "
	    "%d/32 words", r == 0 ? "ok" : strerror(r), n);

	/* c: the VM's compute queue runs beside it. */
	run_clean(&j, "20 gfx queue: the VM's compute queue runs beside it");

	/* d: write_42 (a compute dispatch) on the gfx queue: INFO only. */
	if (poison(j.out, POISON_A) == 0 && job_submit_on(&j, q, j.out_va,
	    NULL) == 0) {
		r = job_wait(&j, 4000, &ms);
		n = count(j.out, SIW42_OUT_VALUE, &first);
		printf("INFO: 20 gfx queue: write_42's dispatch on the gfx "
		    "queue: completion %s after %d ms, %d/64 words 42\n",
		    r == 0 ? "ok" : strerror(r), ms, n);
		if (r != 0) {
			close(q);
			close(j.so);
			j.so = syncobj();
			j.value = 0;
			q = queue_create2(j.vm, ATRIUM_GPU_QUEUE_FAMILY_GFX, 0,
			    0);
		}
	}

	/* e: a store to an unmapped VA loses only this queue. */
	if (q >= 0) {
		r = gfxq_run(&j, q, w, gfxq_write(w, 0x7ff000000000ull,
		    GFXQ_MARK, 1), &ms);
		/*
		 * A CP write (WRITE_DATA) that faults goes to the hub's
		 * default page and the CP runs on: the job may fence before
		 * the fault IV is handled (run 72876a35: fence 35 of 35),
		 * so it can report success. Either way the queue is lost,
		 * which the next check holds it to.
		 */
		check(r == EIO || r == 0, "20 gfx queue: a store to an "
		    "unmapped VA by the CP finishes or fails (EIO)", "got %s "
		    "after %d ms", r == 0 ? "success" : strerror(r), ms);
		usleep(100000);		/* the fault IV */
		r = queue_submit(q, j.ib_va, 5, -1, 0, NULL);
		check(r == EIO, "20 gfx queue: the lost queue refuses new work",
		    "%s", r == 0 ? "accepted" : strerror(r));
		/* On a new queue: test 16's GPU reset may have lost the
		 * one `other` had (runs 048af0f9..72876a35: EIO). */
		if (other != NULL && other->vm >= 0 && job_renew(other) == 0)
			run_clean(other, "20 gfx queue: another VM's queue "
			    "runs on");
		close(q);
		close(j.so);
		j.so = syncobj();
		j.value = 0;
	}

	/* f: a new gfx queue in the same VM. */
	q = queue_create2(j.vm, ATRIUM_GPU_QUEUE_FAMILY_GFX, 0, 0);
	if (q < 0) {
		check(0, "20 gfx queue: a new gfx queue after", "%s",
		    strerror(errno));
		goto out;
	}
	(void)poison(j.out, POISON_A);
	r = gfxq_run(&j, q, w, gfxq_write(w, j.out_va, GFXQ_MARK + 0x200, 4),
	    &ms);
	n = bo_xfer(j.out, out, sizeof(out), 0) == 0 ? 0 : -1;
	for (i = 0; n >= 0 && i < 4; i++)
		n += out[i] == GFXQ_MARK + 0x200 + (uint32_t)i;
	check(r == 0 && n == 4, "20 gfx queue: a new gfx queue in the same VM "
	    "runs after", "completion %s, %d/4 words", r == 0 ? "ok" :
	    strerror(r), n);
out:
	if (q >= 0)
		close(q);
	job_close(&j);
	if (gfxq_set(old, &tmp) != 0)
		check(0, "20 gfx queue", "restore: %s", strerror(errno));
}

/* ---- 19: VM_UNBIND ---- */

static int
vm_unbind(int vm, int bo, uint32_t flags)
{
	struct atrium_gpu_vm_unbind u;

	memset(&u, 0, sizeof(u));
	u.vm_fd = (uint32_t)vm;
	u.bo_fd = (uint32_t)bo;
	u.flags = flags;
	return (ioctl(fd, ATRIUM_GPU_IOC_VM_UNBIND, &u) == 0 ? 0 : errno);
}

/* VM_BIND2 at a chosen VA. */
static int
vm_bind_at(int vm, int bo, uint64_t va)
{
	struct atrium_gpu_vm_bind2 b;

	memset(&b, 0, sizeof(b));
	b.vm_fd = (uint32_t)vm;
	b.bo_fd = (uint32_t)bo;
	b.va = va;
	return (ioctl(fd, ATRIUM_GPU_IOC_VM_BIND2, &b) == 0 && b.va == va ?
	    0 : errno != 0 ? errno : EINVAL);
}

#define UNBIND_VMS	4

static void
unbind(void)
{
	struct job j;
	int vm[UNBIND_VMS], shared, other, i, r, r2, ok, ms, n;
	int64_t v0, v1, v2, now;
	uint64_t va, out_va;
	uint32_t first;

	job_init(&j);
	v0 = iso("vms");
	if (v0 < 0) {
		check(0, "19 unbind", "isolation.vms unreadable: %s",
		    strerror(errno));
		return;
	}

	/* a: refusals. */
	shared = bo_alloc(4096, 0);
	other = bo_alloc(4096, 0);
	vm[0] = vm_create();
	if (shared < 0 || other < 0 || vm[0] < 0 ||
	    vm_bind(vm[0], shared, 0, &va) != 0) {
		check(0, "19 unbind", "setup: %s", strerror(errno));
		goto out0;
	}
	r = vm_unbind(vm[0], shared, 1);
	check(r == EINVAL, "19 unbind: unknown flags refused", "%s",
	    r == 0 ? "accepted" : strerror(r));
	r = vm_unbind(vm[0], other, 0);
	check(r == ENOENT, "19 unbind: a BO not bound in the VM refused",
	    "%s", r == 0 ? "accepted" : strerror(r));
	r = vm_unbind(vm[0], vm[0], 0);
	check(r != 0, "19 unbind: a VM fd as the BO refused", "%s",
	    r == 0 ? "accepted" : strerror(r));

	/* b: one BO in four VMs, unbound from each: the VMs go with their
	 * fds. */
	for (i = 1; i < UNBIND_VMS; i++) {
		vm[i] = vm_create();
		if (vm[i] < 0 || vm_bind(vm[i], shared, 0, &va) != 0) {
			check(0, "19 unbind", "VM %d: %s", i, strerror(errno));
			while (i-- > 1)
				close(vm[i]);
			goto out0;
		}
	}
	for (i = 0, ok = 0; i < UNBIND_VMS; i++)
		ok += vm_unbind(vm[i], shared, 0) == 0;
	r = vm_unbind(vm[0], shared, 0);
	check(ok == UNBIND_VMS && r == ENOENT, "19 unbind: a shared BO "
	    "unbinds from each of its VMs, once", "%d/%d unbound, again: %s",
	    ok, UNBIND_VMS, r == 0 ? "accepted" : strerror(r));
	for (i = 0; i < UNBIND_VMS; i++)
		close(vm[i]);
	v1 = iso("vms");
	check(v1 == v0, "19 unbind: unbound, the VMs go when their fds "
	    "close", "vms %jd -> %jd", (intmax_t)v0, (intmax_t)v1);

	/* c: the control: a binding left in place keeps its VM alive. */
	vm[0] = vm_create();
	r = vm[0] >= 0 ? (vm_bind(vm[0], shared, 0, &va) == 0 ? 0 : errno) :
	    errno;
	if (vm[0] >= 0)
		close(vm[0]);
	v1 = iso("vms");
	close(shared);
	shared = -1;
	v2 = iso("vms");
	check(r == 0 && v1 == v0 + 1 && v2 == v0, "19 unbind: a binding "
	    "left in place holds its VM until the BO closes", "bind %s, vms "
	    "%jd -> %jd -> %jd", r == 0 ? "ok" : strerror(r), (intmax_t)v0,
	    (intmax_t)v1, (intmax_t)v2);

	/* d: unbind under work that may still use the page, then close. */
	if (job_open(&j, "19") != 0) {
		check(0, "19 unbind", "job setup failed");
		goto out0;
	}
	if (poison(j.out, POISON_A) != 0 || job_submit(&j, j.out_va) != 0) {
		check(0, "19 unbind: in flight", "submit: %s", strerror(errno));
		goto out;
	}
	out_va = j.out_va;
	r2 = vm_unbind(j.vm, j.out, 0);
	r = job_wait(&j, 8000, &ms);
	close(j.out);
	j.out = -1;
	/*
	 * The work may store to the page after its PTEs went (box run 6e):
	 * then it faults (EIO), its queue is lost, and a BO the driver could
	 * not prove idle waits until that VM has no user queue (defer.rs:
	 * BusyAt.uq), so a new queue made in the VM first would hold it
	 * (run 72876a35). Close the lost queue, check, then make the new one.
	 */
	if (r != 0) {
		close(j.q);
		j.q = -1;
	}
	for (i = 0; i < 100 && (now = iso("deferred_now")) != 0; i++)
		usleep(20000);
	check(r2 == 0 && (r == 0 || r == EIO) && now == 0, "19 unbind: a BO "
	    "unbound under work that may use it, then closed, leaves nothing "
	    "deferred once that work's queue is done", "unbind %s, the work "
	    "%s after %d ms, deferred_now %jd", r2 == 0 ? "ok" : strerror(r2),
	    r == 0 ? "landed" : strerror(r), ms, (intmax_t)now);
	if (r != 0 && job_renew(&j) != 0) {
		check(0, "19 unbind: renew", "%s", strerror(errno));
		goto out;
	}
	if (j.out < 0 && ((j.out = bo_alloc(4096, 0)) < 0 ||
	    vm_bind_at(j.vm, j.out, out_va) != 0)) {
		check(0, "19 unbind: rebind", "%s", strerror(errno));
		goto out;
	}
	run_clean(&j, "19 unbind: the VM runs clean after");
	out_va = j.out_va;

	/* e: an unbound page faults; the range binds again at its VA. */
	if ((r = vm_unbind(j.vm, j.out, 0)) != 0 ||
	    poison(j.out, POISON_A) != 0 || job_submit(&j, out_va) != 0) {
		check(0, "19 unbind: fault", "%s", strerror(r != 0 ? r : errno));
		goto out;
	}
	r = job_wait(&j, 8000, &ms);
	n = count(j.out, SIW42_OUT_VALUE, &first);
	check(r == EIO && n == 0, "19 unbind: a store to the unbound page "
	    "faults and fails the work, the BO untouched", "completion %s "
	    "after %d ms, %d/64 words 42", r == 0 ? "ok" : strerror(r), ms,
	    n);
	if (job_renew(&j) != 0) {
		check(0, "19 unbind: renew", "%s", strerror(errno));
		goto out;
	}
	r = vm_bind_at(j.vm, j.out, out_va);
	check(r == 0, "19 unbind: the BO binds again at the same VA", "%s",
	    r == 0 ? "ok" : strerror(r));
	if (r == 0)
		run_clean(&j, "19 unbind: and the VM runs clean on it");
out:
	job_close(&j);
out0:
	if (shared >= 0)
		close(shared);
	if (other >= 0)
		close(other);
}

/* ---- 21: the first triangle, by hand (stage 7b) ---- */

/*
 * One draw on a gfx queue: an NGG vertex shader (wave32, VS as the merged
 * ES/GS stage GFX12 always uses) places three vertices by their vertex id,
 * a pixel shader exports a constant colour, the colour target is a linear
 * 64 x 64 RGBA8 surface in system memory, read back and compared with a CPU
 * rasterisation. Register state follows Mesa 25.2 for GFX12 (MIT): the
 * graphics preamble (ac_cmdbuf.c gfx12_init_graphics_preamble_state, radv
 * radv_emit_graphics, radeonsi gfx12_init_gfx_preamble_state's user-queue
 * CONTEXT_CONTROL), the attribute/position/primitive rings (radv_queue.c),
 * the NGG and PS registers (radv_shader.c, radv_cmd_buffer.c) and the
 * colour surface (ac_descriptors.c ac_init_gfx12_cb_surface). Offsets come
 * from gfx12_regs.h, generated from Linux's GFX12 header. Shader dwords:
 * llvm-mc -mcpu=gfx1201 from si_tri_vs.s / si_tri_ps.s.
 *
 * The VS also stores, per lane, its SGPRs s0..s7 and VGPRs v0..v4 to a
 * debug page (user SGPRs s[8:9]); they are printed whatever the outcome,
 * so a first run on silicon shows the merged-shader ABI it really got.
 */
#include "gfx12_regs.h"
#include "si_tri_glsl.h"

#define TRI_W		64
#define TRI_H		64
#define TRI_PITCH	(TRI_W * 4)
#define TRI_BG		0x5a5a5a5au
#define TRI_FG		0xff00ffffu	/* R=G=0xff, B=0, A=0xff (bytes R,G,B,A) */
/* 4 shader engines on the R9700 (Navi 48): ring sizes as radv's for GFX12
 * (ac_gpu_info.c): 1400 KiB attribute ring per SE, 16384 position and
 * 16368 primitive exports per SE, both rings sized for max_se^2. */
#define TRI_SE		4u
#define TRI_ATTR_SE	0x160000u		/* 1400 KiB, 64 KiB aligned */
#define TRI_POS_SE	(16384u * 16)
#define TRI_PRIM_SE	((16368u * 4 + 31) & ~31u)
#define TRI_POS_OFF	(TRI_ATTR_SE * TRI_SE)
#define TRI_POS_SZ	((TRI_POS_SE * TRI_SE * TRI_SE + 0xffffu) & ~0xffffu)
#define TRI_PRIM_OFF	(TRI_POS_OFF + TRI_POS_SZ)
#define TRI_PRIM_SZ	((TRI_PRIM_SE * TRI_SE * TRI_SE + 0xffffu) & ~0xffffu)
#define TRI_RINGS	(TRI_PRIM_OFF + TRI_PRIM_SZ)
#define TRI_RINGS_VA	0x18000000ull		/* 64 KiB aligned (base >> 16) */

/* si_tri_vs.s: NGG VS. s2 gs_tg_info, s3 merged_wave_info (Mesa radv:
 * merged shaders have 8 system SGPRs, user SGPRs from s8); v0 the packed
 * primitive, v3 the vertex id. */
static const uint32_t tri_vs[] = {
	0xbefe00c1u, 0xd71f0005u, 0x000100c1u, 0x30180a86u,
	0x7e1a0200u, 0x7e1c0201u, 0x7e1e0202u, 0x7e200203u,
	0xee068008u, 0x06800000u, 0x0000000cu, 0xee068008u,
	0x07000000u, 0x0000040cu, 0xee068008u, 0x07800000u,
	0x0000080cu, 0xee068008u, 0x08000000u, 0x00000c0cu,
	0xee068008u, 0x00000000u, 0x0000100cu, 0xee068008u,
	0x00800000u, 0x0000140cu, 0xee068008u, 0x01000000u,
	0x0000180cu, 0xee068008u, 0x01800000u, 0x00001c0cu,
	0xee068008u, 0x02000000u, 0x0000200cu, 0x7e1a0204u,
	0x7e1c0205u, 0x7e1e0206u, 0x7e200207u, 0xee068008u,
	0x06800000u, 0x0000240cu, 0xee068008u, 0x07000000u,
	0x0000280cu, 0xee068008u, 0x07800000u, 0x00002c0cu,
	0xee068008u, 0x08000000u, 0x0000300cu, 0x7e1a02ffu,
	0x7b7b0000u, 0x381a0b0du, 0xee068008u, 0x06800000u,
	0x00003c0cu, 0x930aff03u, 0x00040018u, 0xbf07800au,
	0xbfa20007u, 0x930bff02u, 0x0009000cu, 0x930cff02u,
	0x00090016u, 0x840c8c0cu, 0x8c7d0c0bu, 0xbfb60009u,
	0xbe8d007eu, 0x930eff03u, 0x00080008u, 0x7c980a0eu,
	0x8b7e6a0du, 0xbfa50002u, 0xf8000941u, 0x00000000u,
	0xbefe000du, 0x8b0fff03u, 0x000000ffu, 0x7c980a0fu,
	0x8b7e6a0du, 0xbfa50016u, 0x7e0c02f1u, 0x7e0e02f1u,
	0x7e1002ffu, 0x3e800000u, 0x7e1202ffu, 0x3f400000u,
	0x7c940681u, 0xd5010006u, 0x01a9e106u, 0x7c940682u,
	0x020c1106u, 0x020e1307u, 0x7e140280u, 0x7e1602f2u,
	0xee068008u, 0x03000000u, 0x0000340cu, 0xee068008u,
	0x03800000u, 0x0000380cu, 0xf80008cfu, 0x0b0a0706u,
	0xbfb00000u,
};
/* si_tri_ps.s: count invocations at user SGPRs s[0:1] (the debug page +
 * TRI_PS_COUNT), export (1, 1, 0, 1) to MRT0. */
static const uint32_t tri_ps[] = {
	0x7e080281u, 0x7e0a0280u, 0xee0d4000u, 0x02000000u,
	0x00000005u, 0x7e0002f2u, 0x7e0202f2u, 0x7e040280u,
	0x7e0602f2u, 0xf800080fu, 0x03020100u, 0xbfb00000u,
};
#define TRI_PS_OFF	0x400u		/* 256-byte aligned (PGM_LO is >> 8) */
#define TRI_PS_COUNT	0x800u		/* PS invocation counter, debug page */
#define TRI_STATS0	0x900u		/* 14 pipeline statistics, before */
#define TRI_STATS1	0xa00u		/* and after the draw */
#define TRI_STATS2	0xb00u		/* after the second, unclipped draw */

/* Triangle in pixels (viewport 64 x 64, y down): the VS's NDC
 * (-0.5,-0.5), (0.5,-0.5), (0.25,0.75). */
static const float tri_px[3][2] = { { 16, 16 }, { 48, 16 }, { 40, 56 } };

struct tri_pm4 {
	uint32_t *w;
	u_int n, max;
};

static void
tri_put(struct tri_pm4 *p, uint32_t v)
{
	if (p->n < p->max)
		p->w[p->n] = v;
	p->n++;
}

#define PKT3_GFX(op, body)	(3u << 30 | (((body) - 1) & 0x3fffu) << 16 | \
				(op) << 8)
#define TRI_CTX_BASE	0x28000u
#define TRI_SH_BASE	0xb000u
#define TRI_UC_BASE	0x30000u

/* `n` consecutive registers from byte address `r`. */
static void
tri_regs(struct tri_pm4 *p, uint32_t r, const uint32_t *v, u_int n)
{
	uint32_t op, base, idx = 0;
	u_int i;

	if (r >= TRI_UC_BASE) {
		op = 0x79;			/* SET_UCONFIG_REG */
		base = TRI_UC_BASE;
	} else if (r >= TRI_CTX_BASE) {
		op = 0x69;			/* SET_CONTEXT_REG */
		base = TRI_CTX_BASE;
	} else {
		op = 0x76;			/* SET_SH_REG */
		base = TRI_SH_BASE;
	}
	tri_put(p, PKT3_GFX(op, n + 1));
	tri_put(p, ((r - base) >> 2) | idx);
	for (i = 0; i < n; i++)
		tri_put(p, v[i]);
}

static void
tri_reg(struct tri_pm4 *p, uint32_t r, uint32_t v)
{
	tri_regs(p, r, &v, 1);
}

/* SET_SH_REG_INDEX with index 3 (Mesa ac_pm4_set_reg_idx3: CU masks). */
static void
tri_sh_idx3(struct tri_pm4 *p, uint32_t r, uint32_t v)
{
	tri_put(p, PKT3_GFX(0x9bu, 2));
	tri_put(p, ((r - TRI_SH_BASE) >> 2) | 3u << 28);
	tri_put(p, v);
}

static uint32_t
tri_f(float f)
{
	uint32_t u;

	memcpy(&u, &f, 4);
	return (u);
}

/* The whole command stream: preamble, rings, target, state, one draw. */
static u_int
tri_build(uint32_t *w, u_int max, uint64_t vs_va, uint64_t ps_va,
    uint64_t cb_va, uint64_t dbg_va, uint64_t rings_va, int glsl)
{
	struct tri_pm4 p = { w, 0, max };
	uint32_t v[16];
	u_int i;
	/* glsl: 0 hand-written (21), 1 GLSL (22), 2 GLSL with varyings (23). */
	const u_int vs_vgprs = glsl == 2 ? COLOR_VS_VGPRS : glsl ?
	    GLSL_VS_VGPRS : 24;
	const u_int vs_user = glsl == 2 ? COLOR_VS_USER_SGPRS : glsl ?
	    GLSL_VS_USER_SGPRS : 2;
	const u_int ps_vgprs = glsl == 2 ? COLOR_PS_VGPRS : glsl ?
	    GLSL_PS_VGPRS : 8;
	const u_int ps_user = glsl == 2 ? COLOR_PS_USER_SGPRS : glsl ?
	    GLSL_PS_USER_SGPRS : 2;
	const uint32_t ps_ena = glsl == 2 ? COLOR_PS_INPUT_ENA : glsl ?
	    GLSL_PS_INPUT_ENA : 1u << 1;	/* PERSP_CENTER */
	const u_int params = glsl == 2 ? COLOR_VS_PARAMS : 0;
	const u_int ps_inputs = glsl == 2 ? COLOR_PS_INPUTS : 0;

	/* CONTEXT_CONTROL as radeonsi's user-queue register shadowing. */
	tri_put(&p, PKT3_GFX(0x28u, 2));
	tri_put(&p, 1u << 31 | 1u << 24 | 1u << 16 | 1u << 15 | 1u << 1);
	tri_put(&p, 1u << 31 | 1u << 24 | 1u << 16 | 1u << 15 | 1u << 1 |
	    1u << 0);

	/* Graphics preamble (gfx12_init_graphics_preamble_state; temporal
	 * hints left regular). */
	tri_sh_idx3(&p, R_SPI_SHADER_PGM_RSRC3_PS, 0xffffu);
	tri_reg(&p, R_SPI_SHADER_REQ_CTRL_PS, 1u | 3u << 1);
	for (i = 0; i < 4; i++)
		tri_reg(&p, R_SPI_SHADER_USER_ACCUM_PS_0 + 4 * i, 0);
	tri_reg(&p, R_SPI_SHADER_PGM_HI_ES, 0);
	tri_sh_idx3(&p, R_SPI_SHADER_PGM_RSRC3_GS, 0xfffffdfdu);
	for (i = 0; i < 4; i++)
		tri_reg(&p, R_SPI_SHADER_USER_ACCUM_ESGS_0 + 4 * i, 0);
	tri_reg(&p, R_SPI_SHADER_PGM_HI_LS, 0);
	tri_sh_idx3(&p, R_SPI_SHADER_PGM_RSRC3_HS, 0xffffffffu);
	for (i = 0; i < 4; i++)
		tri_reg(&p, R_SPI_SHADER_USER_ACCUM_LSHS_0 + 4 * i, 0);
	tri_reg(&p, R_SPI_SHADER_PGM_RSRC4_HS, 0x3ffu | 1u << 11);
	tri_reg(&p, R_SPI_SHADER_PGM_RSRC4_PS, 0x3ffu | 1u << 10);

	tri_reg(&p, R_DB_GL1_INTERFACE_CONTROL, 0);
	tri_reg(&p, R_DB_MEM_TEMPORAL, 0);
	tri_reg(&p, R_DB_VIEWPORT_CONTROL, 0);
	tri_reg(&p, R_DB_SPI_VRS_CENTER_LOCATION, 0);
	tri_reg(&p, R_TA_BC_BASE_ADDR, 0);
	tri_reg(&p, R_TA_BC_BASE_ADDR + 4, 0);
	tri_reg(&p, R_DB_STENCIL_OPVAL, 1u | 1u << 8);
	tri_reg(&p, R_SC_MEM_TEMPORAL, 0);
	tri_reg(&p, R_SC_MEM_SPEC_READ, 1u | 1u << 2 | 1u << 4);
	tri_reg(&p, R_PA_SC_SCREEN_SCISSOR_TL, 0);
	tri_reg(&p, R_PA_SC_SCREEN_SCISSOR_TL + 4, 65535u | 65535u << 16);
	tri_reg(&p, R_PA_SC_WINDOW_SCISSOR_TL, 0);
	tri_reg(&p, R_PA_SC_GENERIC_SCISSOR_TL, 0);
	tri_reg(&p, R_PA_SC_GENERIC_SCISSOR_TL + 4, 65535u | 65535u << 16);
	tri_reg(&p, R_PA_SC_SCREEN_EXTENT_CONTROL, 0);
	tri_reg(&p, R_PA_SC_TILE_STEERING_OVERRIDE, 0);
	tri_reg(&p, R_PA_SC_VRS_INFO, 0);
	tri_reg(&p, R_CB_RMI_GL2_CACHE_CONTROL, 0);
	tri_reg(&p, R_SPI_BARYC_SSAA_CNTL, 1u << 2);
	tri_reg(&p, R_SX_PS_DOWNCONVERT_CONTROL, 0xff);
	tri_reg(&p, R_PA_CL_POINT_X_RAD, 0);
	tri_reg(&p, R_PA_CL_POINT_Y_RAD, 0);
	tri_reg(&p, R_PA_CL_POINT_SIZE, 0);
	tri_reg(&p, R_PA_CL_POINT_CULL_RAD, 0);
	tri_reg(&p, R_PA_CL_NANINF_CNTL, 0);
	tri_reg(&p, R_PA_SU_LINE_STIPPLE_CNTL, 0);
	tri_reg(&p, R_PA_SU_LINE_STIPPLE_SCALE, tri_f(1.0f));
	tri_reg(&p, R_PA_SU_SMALL_PRIM_FILTER_CNTL, 1u | 1u << 6);
	tri_reg(&p, R_PA_SU_OVER_RASTERIZATION_CNTL, 0);
	tri_reg(&p, R_PA_STEREO_CNTL, 1u << 1);
	tri_reg(&p, R_VGT_HOS_MAX_TESS_LEVEL, tri_f(64.0f));
	tri_reg(&p, R_VGT_HOS_MIN_TESS_LEVEL, 0);
	tri_reg(&p, R_GE_SE_ENHANCE, 0);
	tri_reg(&p, R_GE_IA_ENHANCE, 0);
	tri_reg(&p, R_GE_WD_ENHANCE, 0);
	tri_reg(&p, R_VGT_REUSE_OFF, 0);
	tri_reg(&p, R_VGT_DRAW_PAYLOAD_CNTL, 0);
	tri_reg(&p, R_DB_HTILE_SURFACE, 0);
	tri_reg(&p, R_VGT_TESS_DISTRIBUTION, 128u | 128u << 8 | 128u << 16 |
	    24u << 24 | 6u << 29);
	tri_reg(&p, R_PA_SC_HISZ_RENDER_OVERRIDE, 0);
	tri_reg(&p, R_PA_SC_BINNER_OUTPUT_TIMEOUT_COUNTER, 0x800);
	tri_reg(&p, R_PA_SC_BINNER_CNTL_1, 254u | 511u << 16);
	tri_reg(&p, R_PA_SC_BINNER_CNTL_2, 1u << 23);
	tri_reg(&p, R_PA_SC_NGG_MODE_CNTL, 64);
	tri_reg(&p, R_PA_SC_SHADER_CONTROL, 1);
	for (i = 0; i < 8; i++)
		tri_reg(&p, R_CB_MEM0_INFO + 4 * i, 0);
	tri_reg(&p, R_GE_MIN_VTX_INDX, 0);
	tri_reg(&p, R_GE_INDX_OFFSET, 0);
	tri_reg(&p, R_GE_MULTI_PRIM_IB_RESET_EN, 1u << 2);
	tri_reg(&p, R_GE_GS_THROTTLE, 1u | 4u << 3 | 3u << 6 | 0x40u << 9 |
	    2u << 16 | 3u << 19 | 0xffu << 23);
	tri_reg(&p, R_GE_MAX_VTX_INDX, 0xffffffffu);
	tri_reg(&p, R_VGT_INSTANCE_BASE_ID, 0);
	tri_reg(&p, R_GE_STEREO_CNTL, 0);
	tri_reg(&p, R_GE_USER_VGPR_EN, 0);
	tri_reg(&p, R_VGT_PRIMITIVEID_RESET, 0);
	tri_reg(&p, R_GE_VRS_RATE, 0);
	tri_reg(&p, R_PA_SU_LINE_STIPPLE_VALUE, 0);
	tri_reg(&p, R_PA_SC_LINE_STIPPLE_STATE, 0);
	tri_reg(&p, R_PA_SU_PRIM_FILTER_CNTL, 0);
	tri_reg(&p, R_SPI_GRP_LAUNCH_GUARANTEE_ENABLE, 1u | 1u << 2 |
	    1u << 3 | 2u << 5 | 2u << 8 | 1u << 11 | 1u << 15);
	tri_reg(&p, R_SPI_GRP_LAUNCH_GUARANTEE_CTRL, 3u | 4u << 3 | 4u << 6 |
	    4u << 10 | 8u << 19);

	/* radv_emit_graphics (no CLEAR_STATE on GFX11+) and radeonsi's
	 * GFX12 additions. */
	for (i = 0; i < 16; i++) {
		tri_reg(&p, R_PA_SC_VPORT_ZMIN_0 + 8 * i, 0);
		tri_reg(&p, R_PA_SC_VPORT_ZMIN_0 + 8 * i + 4, tri_f(1.0f));
	}
	tri_reg(&p, R_PA_SC_EDGERULE, 0xaaaaaaaau);
	tri_reg(&p, R_PA_SU_HARDWARE_SCREEN_OFFSET, 0);
	tri_reg(&p, R_PA_CL_NGG_CNTL, 30u << 2);
	tri_reg(&p, R_PA_CL_VRS_CNTL, 1u | 1u << 9);
	tri_reg(&p, R_VGT_MULTI_PRIM_IB_RESET_INDX, 0xffffffffu);
	tri_reg(&p, R_PA_SU_POINT_SIZE, 8u | 8u << 16);
	tri_reg(&p, R_PA_SU_POINT_MINMAX, 0xffffu << 16);
	tri_reg(&p, R_SPI_INTERP_CONTROL_0, 1u | 1u << 1 | 2u << 2 | 3u << 5 |
	    0u << 8 | 1u << 11);
	tri_reg(&p, R_PA_SU_VTX_CNTL, 1u | 2u << 1 | 5u << 3);
	tri_reg(&p, R_PA_CL_VTE_CNTL, 0x3fu | 1u << 10);
	tri_reg(&p, R_DB_RENDER_CONTROL, 0);
	tri_reg(&p, R_SPI_BARYC_CNTL, 0);
	tri_reg(&p, R_PA_SC_CONSERVATIVE_RASTERIZATION_CNTL, 1u << 20);
	tri_reg(&p, R_SPI_SHADER_GS_MESHLET_CTRL, 0);

	/* Rings (radv_queue.c): throttles, attribute ring, then the four
	 * position/primitive ring registers together. */
	v[0] = 0x12355123u;
	v[1] = 0x1544du;
	v[2] = (uint32_t)(rings_va >> 16);
	v[3] = ((TRI_ATTR_SE >> 16) - 1) | 1u << 17;
	tri_regs(&p, R_SPI_GS_THROTTLE_CNTL1, v, 4);
	v[0] = (uint32_t)((rings_va + TRI_POS_OFF) >> 16);
	v[1] = TRI_POS_SE >> 5;
	v[2] = (uint32_t)((rings_va + TRI_PRIM_OFF) >> 16);
	v[3] = TRI_PRIM_SE >> 5 | 2u << 16 | 3u << 18 | 3u << 21 | 1u << 26 |
	    1u << 27;
	tri_regs(&p, R_GE_POS_RING_BASE, v, 4);

	/* Colour target 0: linear 64 x 64 COLOR_8_8_8_8 UNORM, SWAP_STD. */
	tri_reg(&p, R_CB_COLOR0_BASE, (uint32_t)(cb_va >> 8));
	tri_reg(&p, R_CB_COLOR0_VIEW, 0);
	tri_reg(&p, R_CB_COLOR0_VIEW2, 0);
	tri_reg(&p, R_CB_COLOR0_ATTRIB, 0);
	tri_reg(&p, R_CB_COLOR0_FDCC_CONTROL, 1u << 2 | 1u << 28);
	tri_reg(&p, R_CB_COLOR0_ATTRIB2, (TRI_H - 1) | (TRI_W - 1) << 16);
	tri_reg(&p, R_CB_COLOR0_ATTRIB3, 0u | 0u << 15 | 0u << 19 | 1u << 24);
	tri_reg(&p, R_CB_COLOR0_BASE_EXT, (uint32_t)(cb_va >> 40));
	tri_reg(&p, R_CB_COLOR0_INFO, 10u | 0u << 8 | 0u << 11 | 1u << 15 |
	    1u << 17);
	for (i = 1; i < 8; i++)
		tri_reg(&p, R_CB_COLOR0_INFO + 4 * i, 0);	/* INVALID */
	/* No depth/stencil (radv_gfx12_emit_null_ds_state). */
	tri_reg(&p, R_DB_Z_INFO, 0u | 3u << 2);
	tri_reg(&p, R_DB_STENCIL_INFO, 0u | 1u << 29);
	tri_reg(&p, R_PA_SC_HIZ_INFO, 0);
	tri_reg(&p, R_PA_SC_HIS_INFO, 0);
	tri_reg(&p, R_DB_RENDER_OVERRIDE2, 1u << 27);
	tri_reg(&p, R_DB_RENDER_OVERRIDE, 1u << 12);
	tri_reg(&p, R_DB_DEPTH_CONTROL, 0);
	tri_reg(&p, R_DB_STENCIL_CONTROL, 0);
	tri_reg(&p, R_DB_COUNT_CONTROL, 0);
	tri_reg(&p, R_DB_ALPHA_TO_MASK, 0);
	tri_reg(&p, R_DB_SHADER_CONTROL, 1u << 4);	/* EARLY_Z_THEN_LATE_Z */

	/* Viewport 0 over the target (y down), scissor, guard band. */
	v[0] = tri_f(TRI_W / 2.0f);
	v[1] = tri_f(TRI_W / 2.0f);
	v[2] = tri_f(TRI_H / 2.0f);
	v[3] = tri_f(TRI_H / 2.0f);
	v[4] = tri_f(1.0f);
	v[5] = 0;
	tri_regs(&p, R_PA_CL_VPORT_XSCALE, v, 6);
	v[0] = v[1] = v[2] = v[3] = tri_f(1.0f);
	tri_regs(&p, R_PA_CL_GB_VERT_CLIP_ADJ, v, 4);
	v[0] = 0;
	v[1] = TRI_W | TRI_H << 16;
	tri_regs(&p, R_PA_SC_VPORT_SCISSOR_0_TL, v, 2);
	tri_reg(&p, R_PA_SC_WINDOW_OFFSET, 0);
	/* GFX12 window scissor BR is inclusive (radv_cmd_buffer.c). */
	tri_reg(&p, R_PA_SC_WINDOW_SCISSOR_TL + 4, (TRI_W - 1) | (TRI_H - 1) << 16);
	tri_reg(&p, R_PA_SC_CLIPRECT_RULE, 0xffff);

	/* Rasteriser: one sample, no culling, binning off. */
	tri_reg(&p, R_PA_SC_AA_CONFIG, 0);
	v[0] = v[1] = 0xffffffffu;
	tri_regs(&p, R_PA_SC_AA_MASK_X0Y0_X1Y0, v, 2);
	v[0] = v[1] = 0;
	tri_regs(&p, R_PA_SC_CENTROID_PRIORITY_0, v, 2);
	tri_reg(&p, R_PA_SC_SAMPLE_PROPERTIES, 0);
	tri_reg(&p, R_PA_SC_MODE_CNTL_0, 1u << 1 | 1u << 5);
	tri_reg(&p, R_PA_SC_MODE_CNTL_1, 0);
	tri_reg(&p, R_PA_SC_LINE_CNTL, 0);
	tri_reg(&p, R_PA_SU_LINE_CNTL, 8);
	tri_reg(&p, R_PA_SC_BINNER_CNTL_0, 3u | 2u << 4 | 2u << 7 |
	    1u << 18 | 63u << 19 | 1u << 27 | 1u << 28);
	tri_reg(&p, R_PA_SU_SC_MODE_CNTL, 2u << 5 | 2u << 8);
	tri_reg(&p, R_PA_CL_CLIP_CNTL, 1u << 19 | 1u << 24);

	/* Colour output: write RGBA, no blend, ROP3 copy. */
	tri_reg(&p, R_CB_TARGET_MASK, 0xf);
	tri_reg(&p, R_CB_SHADER_MASK, 0xf);
	tri_reg(&p, R_CB_COLOR_CONTROL, 1u << 4 | 0xccu << 16);
	tri_reg(&p, R_CB_BLEND0_CONTROL, 0);
	tri_reg(&p, R_SX_PS_DOWNCONVERT, 0);
	tri_reg(&p, R_SX_BLEND_OPT_EPSILON, 0);
	tri_reg(&p, R_SX_BLEND_OPT_CONTROL, 0);

	/* VS on the NGG stage: wave32, 24 VGPRs (v0..v16 used), 2 user
	 * SGPRs (the debug VA, in s[8:9]). */
	tri_reg(&p, R_SPI_SHADER_PGM_LO_ES, (uint32_t)(vs_va >> 8));
	tri_reg(&p, R_SPI_SHADER_PGM_RSRC1_GS, (vs_vgprs - 1) / 8 |
	    0xc0u << 12);
	tri_reg(&p, R_SPI_SHADER_PGM_RSRC2_GS, vs_user << 1);
	tri_reg(&p, R_SPI_SHADER_PGM_RSRC4_GS, 0x3ffu | 1u << 11 |
	    127u << 16);
	/* 21: the debug VA; 22 (our compiler's ABI): first vertex, first
	 * instance. */
	if (params != 0) {
		/* 23: the attribute ring descriptor first (Mesa
		 * ac_build_attr_ring_descriptor: R32G32B32A32_FLOAT, xyzw,
		 * stride 16 x params, 16-byte swizzle, index stride 32,
		 * structured-with-offset bounds), then first vertex and
		 * first instance. */
		v[0] = (uint32_t)rings_va;
		v[1] = (uint32_t)(rings_va >> 32 & 0xffffu) |
		    (16u * params) << 16 | 3u << 30;
		v[2] = 0xffffffffu;
		v[3] = 4u | 5u << 3 | 6u << 6 | 7u << 9 | 63u << 12 |
		    2u << 21 | 3u << 28;
		v[4] = v[5] = 0;
	} else {
		v[0] = glsl ? 0 : (uint32_t)dbg_va;
		v[1] = glsl ? 0 : (uint32_t)(dbg_va >> 32);
	}
	tri_regs(&p, R_SPI_SHADER_USER_DATA_GS_0, v, vs_user);
	tri_reg(&p, R_PA_CL_VS_OUT_CNTL, 0);
	tri_reg(&p, R_VGT_GS_INSTANCE_CNT, 1u << 2);
	v[0] = 1;			/* IDX0: SPI_SHADER_1COMP */
	v[1] = 4;			/* POS0: SPI_SHADER_4COMP */
	tri_regs(&p, R_SPI_SHADER_IDX_FORMAT, v, 2);
	tri_reg(&p, R_GE_MAX_OUTPUT_PER_SUBGROUP, 128);
	tri_reg(&p, R_GE_NGG_SUBGRP_CNTL, 1);
	tri_reg(&p, R_GE_CNTL, 128u | 128u << 9 | 256u << 21 | 1u << 31);
	tri_reg(&p, R_VGT_PRIMITIVEID_EN, 0);
	tri_reg(&p, R_VGT_SHADER_STAGES_EN, 1u << 22);	/* GS_W32_EN */

	/* PS: wave32, 8 VGPRs; one interpolation mode on (the hardware wants
	 * one), no inputs, no parameters from the VS. */
	tri_reg(&p, R_SPI_SHADER_PGM_LO_PS, (uint32_t)(ps_va >> 8));
	tri_reg(&p, R_SPI_SHADER_PGM_LO_PS + 4, (uint32_t)(ps_va >> 40));
	tri_reg(&p, R_SPI_SHADER_PGM_RSRC1_PS, (ps_vgprs - 1) / 8 |
	    0xc0u << 12);
	tri_reg(&p, R_SPI_SHADER_PGM_RSRC2_PS, ps_user << 1);
	v[0] = (uint32_t)(dbg_va + TRI_PS_COUNT);
	v[1] = (uint32_t)((dbg_va + TRI_PS_COUNT) >> 32);
	tri_regs(&p, R_SPI_SHADER_USER_DATA_PS_0, v, 2);
	/* VS_EXPORT_COUNT (params - 1) or NO_PC_EXPORT; NUM_INTERP. */
	tri_reg(&p, R_SPI_SHADER_GS_OUT_CONFIG_PS, (params != 0 ? params - 1 :
	    1u << 10) | ps_inputs << 11);
	v[0] = v[1] = ps_ena;
	tri_regs(&p, R_SPI_PS_INPUT_ENA, v, 2);
	if (ps_inputs != 0)
		tri_regs(&p, R_SPI_PS_INPUT_CNTL_0, color_ps_input_cntl,
		    ps_inputs);
	tri_reg(&p, R_SPI_PS_IN_CONTROL, 1u << 15);	/* PS_W32_EN */
	tri_reg(&p, R_PA_SC_HISZ_CONTROL, 2);
	tri_reg(&p, R_SPI_SHADER_Z_FORMAT, 0);
	tri_reg(&p, R_SPI_SHADER_COL_FORMAT, 9);	/* SPI_SHADER_32_ABGR */

	/* Pipeline statistics around the draw (radv_query.c): where the
	 * triangle stops, if it does. */
	tri_put(&p, PKT3_GFX(0x46u, 1));		/* EVENT_WRITE */
	tri_put(&p, 25);				/* PIPELINESTAT_START */
	tri_put(&p, PKT3_GFX(0x46u, 3));
	tri_put(&p, 30 | 2u << 8);			/* SAMPLE_PIPELINESTAT */
	tri_put(&p, (uint32_t)(dbg_va + TRI_STATS0));
	tri_put(&p, (uint32_t)((dbg_va + TRI_STATS0) >> 32));

	/* The draw: a triangle list, one instance, three auto-index vertices. */
	tri_put(&p, PKT3_GFX(0x7au, 2));		/* SET_UCONFIG_REG_INDEX */
	tri_put(&p, ((R_VGT_PRIMITIVE_TYPE - TRI_UC_BASE) >> 2) | 1u << 28);
	tri_put(&p, 4);					/* DI_PT_TRILIST */
	tri_put(&p, PKT3_GFX(0x2fu, 1));		/* NUM_INSTANCES */
	tri_put(&p, 1);
	tri_put(&p, PKT3_GFX(0x2du, 2));		/* DRAW_INDEX_AUTO */
	tri_put(&p, 3);
	tri_put(&p, 2);					/* DI_SRC_SEL_AUTO_INDEX */
	tri_put(&p, PKT3_GFX(0x46u, 3));
	tri_put(&p, 30 | 2u << 8);			/* SAMPLE_PIPELINESTAT */
	tri_put(&p, (uint32_t)(dbg_va + TRI_STATS1));
	tri_put(&p, (uint32_t)((dbg_va + TRI_STATS1) >> 32));

	if (glsl) {
		tri_put(&p, PKT3_GFX(0x46u, 1));
		tri_put(&p, 26);			/* PIPELINESTAT_STOP */
		return (p.n);
	}

	/*
	 * Diagnosis (run cd1aa333: clip inv 1, clip prims 0, PS never ran):
	 * the same draw again with the clipper off (CLIP_DISABLE), its PS
	 * counting into the next word. If this one lights pixels, what the
	 * clipper saw (positions, W, clip setup) is what is wrong.
	 */
	tri_reg(&p, R_PA_CL_CLIP_CNTL, 1u << 16 | 1u << 19 | 1u << 24);
	v[0] = (uint32_t)(dbg_va + TRI_PS_COUNT + 4);
	v[1] = (uint32_t)((dbg_va + TRI_PS_COUNT + 4) >> 32);
	tri_regs(&p, R_SPI_SHADER_USER_DATA_PS_0, v, 2);
	tri_put(&p, PKT3_GFX(0x2du, 2));		/* DRAW_INDEX_AUTO */
	tri_put(&p, 3);
	tri_put(&p, 2);
	tri_put(&p, PKT3_GFX(0x46u, 3));
	tri_put(&p, 30 | 2u << 8);			/* SAMPLE_PIPELINESTAT */
	tri_put(&p, (uint32_t)(dbg_va + TRI_STATS2));
	tri_put(&p, (uint32_t)((dbg_va + TRI_STATS2) >> 32));
	tri_put(&p, PKT3_GFX(0x46u, 1));
	tri_put(&p, 26);				/* PIPELINESTAT_STOP */
	return (p.n);
}

/* Edge function of (a, b) at (x, y), > 0 inside for this winding. */
static float
tri_edge(const float *a, const float *b, float x, float y)
{
	return ((b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0]));
}

/* 1: inside, 0: outside, -1: within half a pixel of an edge (either). */
static int
tri_cover(int px, int py)
{
	float x = px + 0.5f, y = py + 0.5f, e, len2;
	int k, in = 1, near = 0;

	for (k = 0; k < 3; k++) {
		const float *a = tri_px[k], *b = tri_px[(k + 1) % 3];

		e = tri_edge(a, b, x, y);
		len2 = (b[0] - a[0]) * (b[0] - a[0]) +
		    (b[1] - a[1]) * (b[1] - a[1]);
		if (e < 0)
			in = 0;
		if (e * e < 0.25f * len2)	/* distance < 0.5 px */
			near = 1;
	}
	return (near ? -1 : in);
}

/* 23: the colour the CPU expects at pixel (px, py): the barycentric
 * weights of vertices 0, 1, 2 as red, green, blue (W is 1 everywhere, so
 * perspective and linear agree), alpha 1 (the flat int arrived). */
static uint32_t
tri_colour(int px, int py)
{
	float x = px + 0.5f, y = py + 0.5f, l[3];
	uint32_t c = 0xffu << 24;
	int k;

	for (k = 0; k < 3; k++) {
		const float *a = tri_px[(k + 1) % 3], *b = tri_px[(k + 2) % 3];

		l[k] = tri_edge(a, b, x, y) / tri_edge(a, b, tri_px[k][0],
		    tri_px[k][1]);
		if (l[k] < 0)
			l[k] = 0;
		if (l[k] > 1)
			l[k] = 1;
		c |= (uint32_t)(l[k] * 255.0f + 0.5f) << (8 * k);
	}
	return (c);
}

/* Pixel p matches want within tol per channel. */
static int
tri_near(uint32_t p, uint32_t want, int tol)
{
	int k, d;

	for (k = 0; k < 4; k++) {
		d = (int)(p >> (8 * k) & 0xff) - (int)(want >> (8 * k) & 0xff);
		if (d > tol || d < -tol)
			return (0);
	}
	return (1);
}

static void
triangle(int glsl)
{
	const char *t = glsl == 2 ? "23 colour triangle" : glsl ?
	    "22 GLSL triangle" : "21 triangle";
	const char *tn = glsl == 2 ? "23" : glsl ? "22" : "21";
	char what[160];
	uint32_t want, worst = 0, worst_want = 0;
	int wx = -1, wy = -1;
	struct job j;
	uint32_t *ib = NULL, *pix = NULL, dbg[1024];
	uint64_t cb_va, dbg_va;
	int old = 0, tmp, q = -1, code = -1, cb = -1, dbgbo = -1, rings = -1;
	int r, ms, x, y, c, in = 0, out = 0, edge = 0, bad_in = 0, bad_out = 0;
	u_int dw, i;

	job_init(&j);
	if (gfxq_set(1, &old) != 0) {
		check(0, t, "%s: %s", GFXQ_SYSCTL, strerror(errno));
		return;
	}
	ib = calloc(4096, sizeof(*ib));
	pix = malloc(TRI_PITCH * TRI_H);
	if (ib == NULL || pix == NULL || job_open(&j, tn) != 0) {
		check(0, t, "setup failed");
		goto out;
	}
	q = queue_create2(j.vm, ATRIUM_GPU_QUEUE_FAMILY_GFX, 0, 0);
	code = bo_alloc(4096, 0);
	cb = bo_alloc(TRI_PITCH * TRI_H, 0);
	dbgbo = bo_alloc(4096, 0);
	rings = bo_alloc(TRI_RINGS, ATRIUM_GPU_BO_VRAM);
	if (q < 0 || code < 0 || cb < 0 || dbgbo < 0 || rings < 0) {
		check(0, t, "queue/BOs: %s", strerror(errno));
		goto out;
	}
	{
		uint8_t img[4096];

		memset(img, 0, sizeof(img));
		if (glsl == 2) {
			memcpy(img, color_vs, sizeof(color_vs));
			memcpy(img + TRI_PS_OFF, color_ps, sizeof(color_ps));
		} else if (glsl) {
			memcpy(img, glsl_vs, sizeof(glsl_vs));
			memcpy(img + TRI_PS_OFF, glsl_ps, sizeof(glsl_ps));
		} else {
			memcpy(img, tri_vs, sizeof(tri_vs));
			memcpy(img + TRI_PS_OFF, tri_ps, sizeof(tri_ps));
		}
		for (i = 0; i < TRI_W * TRI_H; i++)
			pix[i] = TRI_BG;
		for (i = 0; i < 1024; i++)
			dbg[i] = POISON_A;
		if (bo_xfer(code, img, sizeof(img), 1) != 0 ||
		    bo_xfer(cb, pix, TRI_PITCH * TRI_H, 1) != 0 ||
		    bo_xfer(dbgbo, dbg, sizeof(dbg), 1) != 0) {
			check(0, t, "fill: %s", strerror(errno));
			goto out;
		}
	}
	{
		uint64_t code_va;

		if (vm_bind(j.vm, code, BIND_CODE, &code_va) != 0 ||
		    vm_bind(j.vm, cb, 0, &cb_va) != 0 ||
		    vm_bind(j.vm, dbgbo, 0, &dbg_va) != 0 ||
		    (r = vm_bind_at(j.vm, rings, TRI_RINGS_VA)) != 0) {
			check(0, t, "bind: %s", strerror(errno));
			goto out;
		}
		dw = tri_build(ib, 4096, code_va, code_va + TRI_PS_OFF, cb_va,
		    dbg_va, TRI_RINGS_VA, glsl);
		printf("%s: VS %#jx, PS %#jx, target %#jx, debug %#jx, rings "
		    "%#jx (%u KiB); %u dwords of PM4\n", tn, (uintmax_t)code_va,
		    (uintmax_t)(code_va + TRI_PS_OFF), (uintmax_t)cb_va,
		    (uintmax_t)dbg_va, (uintmax_t)TRI_RINGS_VA,
		    (u_int)(TRI_RINGS >> 10), dw);
	}
	if (dw > 1024) {
		check(0, t, "%u dwords: more than the IB page", dw);
		goto out;
	}
	r = gfxq_run(&j, q, ib, dw, &ms);
	snprintf(what, sizeof(what), "%s: the draw completes on the gfx queue",
	    t);
	check(r == 0, what, "completion %s after %d ms", r == 0 ? "ok" :
	    strerror(r), ms);

	/* What the VS saw, per lane, whatever happened. */
	if (glsl && bo_xfer(dbgbo, dbg, sizeof(dbg), 0) == 0) {
		static const char *const nm[14] = { "PS inv", "clip prims",
		    "clip inv", "VS inv", "GS inv", "GS prims", "IA prims",
		    "IA verts", "HS inv", "DS inv", "CS inv", "task", "mesh",
		    "mesh prims" };
		const uint32_t *a = dbg + TRI_STATS0 / 4, *b = dbg +
		    TRI_STATS1 / 4;

		printf("%s: pipeline statistics:", tn);
		for (i = 0; i < 14; i++)
			printf(" %s %jd", nm[i], (intmax_t)(((uint64_t)b[2 * i] |
			    (uint64_t)b[2 * i + 1] << 32) - ((uint64_t)a[2 * i] |
			    (uint64_t)a[2 * i + 1] << 32)));
		printf("\n");
	} else if (bo_xfer(dbgbo, dbg, sizeof(dbg), 0) == 0) {
		for (i = 0; i < 8; i++) {
			const uint32_t *l = dbg + 16 * i;

			if (l[15] == POISON_A)
				continue;
			printf("21: VS lane %u (tag %#x): s0..s7 %#x %#x %#x %#x "
			    "%#x %#x %#x %#x; v0..v4 %#x %#x %#x %#x %#x; "
			    "pos x %#x y %#x\n", i,
			    l[15], l[0], l[1], l[2], l[3], l[9], l[10], l[11],
			    l[12], l[4], l[5], l[6], l[7], l[8], l[13], l[14]);
		}
		check(dbg[15] != POISON_A, "21 triangle: the vertex shader ran",
		    "lane 0 %s", dbg[15] != POISON_A ? "wrote its record" :
		    "never ran");
		check(dbg[31] != POISON_A && dbg[47] != POISON_A, "21 triangle: "
		    "the vertex shader ran for all three vertices", "lanes 1, 2 "
		    "%s, %s", dbg[31] != POISON_A ? "ran" : "never ran",
		    dbg[47] != POISON_A ? "ran" : "never ran");
		{
			/* Hardware order (radv_query.c's index table). */
			static const char *const nm[14] = { "PS inv",
			    "clip prims", "clip inv", "VS inv", "GS inv",
			    "GS prims", "IA prims", "IA verts", "HS inv",
			    "DS inv", "CS inv", "task", "mesh", "mesh prims" };
			for (u_int d = 0; d < 2; d++) {
				const uint32_t *a = dbg + (d == 0 ? TRI_STATS0 :
				    TRI_STATS1) / 4, *b = dbg + (d == 0 ?
				    TRI_STATS1 : TRI_STATS2) / 4;

				printf("21: pipeline statistics, draw %u (%s):", d,
				    d == 0 ? "clipped" : "CLIP_DISABLE");
				for (i = 0; i < 14; i++) {
					uint64_t x = a[2 * i] |
					    (uint64_t)a[2 * i + 1] << 32,
					    y = b[2 * i] |
					    (uint64_t)b[2 * i + 1] << 32;

					printf(" %s %jd%s", nm[i],
					    (intmax_t)(y - x), b[2 * i] ==
					    POISON_A ? " (unwritten)" : "");
				}
				printf("\n");
			}
		}
		printf("21: PS invocations counted: draw 0 %u, draw 1 "
		    "(CLIP_DISABLE) %u (0: none ran; about 600 each expected)\n",
		    dbg[TRI_PS_COUNT / 4] - POISON_A,
		    dbg[TRI_PS_COUNT / 4 + 1] - POISON_A);
	}
	if (bo_xfer(cb, pix, TRI_PITCH * TRI_H, 0) != 0) {
		check(0, t, "readback: %s", strerror(errno));
		goto out;
	}
	for (y = 0; y < TRI_H; y++) {
		for (x = 0; x < TRI_W; x++) {
			uint32_t p = pix[y * TRI_W + x];

			c = tri_cover(x, y);
			want = glsl == 2 ? tri_colour(x, y) : TRI_FG;
			if (c < 0) {
				edge += glsl == 2 ? p != TRI_BG : p == TRI_FG;
				continue;
			}
			if (c) {
				in++;
				if (!tri_near(p, want, glsl == 2 ? 3 : 0)) {
					if (bad_in++ == 0) {
						worst = p;
						worst_want = want;
						wx = x;
						wy = y;
					}
				}
			} else {
				out++;
				bad_out += p != TRI_BG;
			}
		}
	}
	for (y = 0; y < TRI_H; y += 4) {
		char row[TRI_W / 2 + 1];

		for (x = 0; x < TRI_W; x += 2) {
			uint32_t p = pix[y * TRI_W + x];

			row[x / 2] = tri_near(p, glsl == 2 ? tri_colour(x, y) :
			    TRI_FG, glsl == 2 ? 3 : 0) ? '#' : p == TRI_BG ?
			    '.' : '?';
		}
		row[TRI_W / 2] = '\0';
		printf("%s: |%s|\n", tn, row);
	}
	snprintf(what, sizeof(what), "%s: every pixel inside is the %s, every "
	    "pixel outside untouched", t, glsl == 2 ? "interpolated colour "
	    "(within 3/255)" : "colour");
	check(bad_in == 0 && bad_out == 0 && in > 0, what,
	    "inside %d/%d wrong, outside %d/%d wrong, edge pixels lit %d, "
	    "pixel (32,24) %#x want %#x; first wrong (%d,%d) %#x want %#x",
	    bad_in, in, bad_out, out, edge, pix[24 * TRI_W + 32],
	    glsl == 2 ? tri_colour(32, 24) : TRI_FG, wx, wy, worst,
	    worst_want);
out:
	if (q >= 0)
		close(q);
	if (code >= 0)
		close(code);
	if (cb >= 0)
		close(cb);
	if (dbgbo >= 0)
		close(dbgbo);
	if (rings >= 0)
		close(rings);
	job_close(&j);
	free(ib);
	free(pix);
	if (gfxq_set(old, &tmp) != 0)
		check(0, t, "restore: %s", strerror(errno));
}

int
main(int argc, char **argv)
{
	struct job a, b, c, e, f;
	struct si_old_submit s;
	struct atrium_gpu_queue_submit qs;
	struct atrium_gpu_queue_create qc;
	uint64_t seq1 = 0, seq2 = 0;
	uint32_t first;
	int r, n, ms, q2, sv[2], st;
	pid_t pid;

	if (argc > 2 || (argc == 2 && strcmp(argv[1], "--render-only") != 0)) {
		fprintf(stderr, "usage: %s [--render-only]\n", argv[0]);
		return (2);
	}
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
	if (argc == 2) {
		triangle(1);
		triangle(2);
		goto out;
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
	/*
	 * SI_SKIP7=1 (wedge bisection, run 772891ad: test 15 doing test 7's
	 * exact fault still wedged, one second after test 7 recovered): so
	 * test 15's fault is the run's first VM fault.
	 */
	if (getenv("SI_SKIP7") != NULL && *getenv("SI_SKIP7") == '1') {
		printf("SKIP: 7 fault (SI_SKIP7)\n");
	} else if (job_open(&f, "7") == 0) {
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
	if (!skipped(9, "user mode"))
		user_mode(&a);

	/* 10: the free rule. */
	if (!skipped(10, "free rule"))
		free_rule(&a);

	/* 11: VRAM BOs (5d). */
	if (!skipped(11, "VRAM"))
		vram(&a);

	/* 12: real-shader variants of gpusim's compute ops (5e). */
	if (!skipped(12, "real ops"))
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

	/* 16: a fault storm, muted (before 14 too). */
	fault_storm();

	/* 17: the shared fault default page (before 14 too). */
	if (!skipped(17, "dummy page"))
		dummy_page();

	/* 18: many VMs and queues (6b; before 14's re-inits). */
	if (!skipped(18, "scale"))
		scale();

	/* 20: gfx queues (stage 7a; before 14's re-inits). */
	if (!skipped(20, "gfx queue"))
		gfx_queue(&a);

	/* 21: the first triangle (stage 7b), right after the gfx queue and
	 * before 19 and 14, which may use up the re-inits (run 048af0f9:
	 * three were gone before it got to run). */
	if (!skipped(21, "triangle"))
		triangle(0);

	/* 22: the same triangle from GLSL, through our compiler (7c). */
	if (!skipped(22, "GLSL triangle"))
		triangle(1);

	/* 23: varyings through the attribute ring (7c-2). */
	if (!skipped(23, "colour triangle"))
		triangle(2);

	/* 19: VM_UNBIND (before 14's re-inits). */
	if (!skipped(19, "unbind"))
		unbind();

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
