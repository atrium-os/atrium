/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * si_fuzz.c — the GPU ABI v2 §10.5 fuzz gate (gpu-isolation.md: "24 h with
 * zero new crashes at the attested commit"), against the kernel driver
 * itself, not the app channel (atrium-test-client's gpu_fuzz.rs fuzzes
 * frescod). Cross-built static for FreeBSD amd64
 * (scripts/r9700/build-si-fuzz.sh); run as root with the kmods attached.
 *
 *   si-fuzz <ioctl|blob|all> <seconds> [seed] [ioctl-threads]
 *
 * Three processes under a root supervisor:
 *
 *   ioctl  (target 1) worker threads under a throwaway uid issue the 'A'
 *          ioctls on random fds of every kind (the device node, VM, BO,
 *          queue and syncobj fds, kqueues, pipes, sockets, a file, closed
 *          numbers) with mutated structs: sizes and struct_size off by a
 *          word or absurd, counts that overflow, pointers to nothing, to
 *          kernel space, to a read-only page, across a guard page, and into
 *          a region another thread keeps unmapping, re-mapping and
 *          re-protecting. Other threads close and dup2 over the fds while
 *          calls on them are in flight, mmap random fds, wait on syncobjs
 *          through kqueue, and fork children that call and exit. Calls only
 *          the kernel's control node may make (GPU_RESET, SCHED, POWERGATE,
 *          the retired ring SUBMIT) and user-mode queues must be refused.
 *   blob   (target 2) one process under another throwaway uid: its own VM
 *          with the write_42 shader and IB, submitting mutated PM4 (the
 *          IB with words flipped or replaced, random packet streams, splices,
 *          blob VAs and sizes off the IB). Each job must complete or fail
 *          (EIO/ECANCELED) within the hang watchdog; after a failure a new
 *          queue in the same VM must run write_42 clean.
 *   canary (the supervisor, as root) a bystander VM with a sealed
 *          read-only page runs write_42 every few seconds: it must stay
 *          correct and its sealed page unchanged, unless a whole-GPU
 *          re-init happened meanwhile (then it starts over on a new VM).
 *
 * Workers restart every generation with a new uid, so the reset-blame
 * lockout of one does not stop the next. After a re-init (silicon:
 * si_protected_regs' "reinits" line) every worker is stopped for
 * SI_FUZZ_PACE seconds (default 610, the driver's whole window), so its cap (3 per 10 min)
 * is never reached by the fuzzer; "lost" ends the run.
 *
 * Findings: a canary that fails twice with no re-init, a job whose
 * completion never comes, a VM that does not run clean after a contained
 * fault, a privileged call that succeeds, a worker killed by a signal it
 * was not sent, a thread stuck in the kernel, objects left behind at the
 * end (deferred frees, doorbell grants, VRAM). Kernel panics and KASAN
 * reports show up on the console, not here; the last op of every thread
 * is printed with each finding.
 *
 * The end: a health check (a fresh VM runs write_42), the errno table per
 * ioctl, and "RESULT: PASS" (exit 0) only with no findings.
 */
#define main si_client_test_main
#include "si_client_test.c"
#undef main

#include <sys/param.h>
#include <pthread.h>
#include <setjmp.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <sys/stat.h>
#include <grp.h>

#define MASTER_FD	250		/* the device fd, in every process */
#define POOL_LO		3
#define POOL_HI		160		/* fds the fuzz picks from: [0, POOL_HI) */
#define FD_SOFTMAX	120
#define MAXT		16
#define NFIND		64
#define CTL_DEV		"/dev/atrium-gpu0ctl"

/* ---- shared state (MAP_SHARED, set up before the forks) ---- */

enum { E_OK, E_INVAL, E_BADF, E_FAULT, E_NOTTY, E_PERM, E_NOMEM, E_NOSPC,
	E_IO, E_BUSY, E_NOTSUP, E_OTHER, E_N };
static const char *const ename[E_N] = { "ok", "EINVAL", "EBADF", "EFAULT",
	"ENOTTY", "EPERM", "ENOMEM", "ENOSPC", "EIO", "EBUSY", "EOPNOTSUPP",
	"other" };

struct slot {
	_Atomic uint64_t ops;
	_Atomic int64_t	 since_ms;	/* in a call since; 0 = not in one */
	char		 last[120];
	int		 stuck_told;
};

#define NCMD 20
struct shm {
	struct slot	 t[MAXT + 1];	/* MAXT: the blob worker */
	_Atomic uint64_t hist[NCMD][E_N];
	_Atomic uint64_t blob_jobs, blob_ok, blob_failed, blob_clean,
			 blob_renew, blob_reopen, mmaps_ok, forks;
	_Atomic int	 nfind;
	char		 find[NFIND][200];
	_Atomic int	 gen_reinit;	/* a worker saw "reinits" move */
	_Atomic int	 setup_fails;	/* blob job opens timed out in a row */
};
static struct shm *S;

static int64_t
now_ms(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ((int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

static void finding(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

static void
finding(const char *fmt, ...)
{
	char b[200];
	va_list ap;
	int i;

	va_start(ap, fmt);
	vsnprintf(b, sizeof(b), fmt, ap);
	va_end(ap);
	printf("FINDING: %s\n", b);
	i = atomic_fetch_add(&S->nfind, 1);
	if (i < NFIND)
		strlcpy(S->find[i], b, sizeof(S->find[i]));
}

static void
dump_last(void)
{
	int i;

	for (i = 0; i <= MAXT; i++)
		if (S->t[i].last[0] != '\0')
			printf("  last op, slot %d (%ju ops): %s%s\n", i,
			    (uintmax_t)S->t[i].ops, S->t[i].last,
			    S->t[i].since_ms ? " [in the kernel]" : "");
}

static int
ebucket(int e)
{
	switch (e) {
	case 0: return (E_OK);
	case EINVAL: return (E_INVAL);
	case EBADF: return (E_BADF);
	case EFAULT: return (E_FAULT);
	case ENOTTY: return (E_NOTTY);
	case EPERM: case EACCES: return (E_PERM);
	case ENOMEM: return (E_NOMEM);
	case ENOSPC: return (E_NOSPC);
	case EIO: return (E_IO);
	case EBUSY: return (E_BUSY);
	case EOPNOTSUPP: return (E_NOTSUP);
	default: return (E_OTHER);
	}
}

static inline uint64_t
rnd(uint64_t *s)
{
	uint64_t x = *s;

	x ^= x >> 12;
	x ^= x << 25;
	x ^= x >> 27;
	*s = x;
	return (x * 0x2545F4914F6CDD1DULL);
}

static inline uint32_t
rn(uint64_t *s, uint32_t n)
{
	return (n == 0 ? 0 : (uint32_t)(rnd(s) % n));
}

/* The re-init count on silicon; -1 = no such sysctl (gpusim), -2 = lost. */
static int
reinits(void)
{
	int ri, n;

	n = preg_read(NULL, &ri);
	if (n < 0)
		return (-1);
	return (ri < 0 ? -2 : ri);
}

/*
 * A queue's work fails with EIO a moment before the driver shows the card
 * lost (si_recover fails every queue's work, then resets): wait up to 20 s
 * for the count to move before blaming the failure on the queue alone.
 * True if no re-init came.
 */
static bool
no_reinit_since(int ri0)
{
	int i;

	for (i = 0; i < 100; i++) {
		if (reinits() != ri0)
			return (false);
		usleep(200000);
	}
	return (true);
}

/* ---- target 1: ioctl structs ---- */

enum fk { F_FD, F_VMFD, F_BOFD, F_SOFD, F_BLOBSZ, F_PTR, F_DEPPTR, F_SIZE, F_BOSIZE, F_FLAGS, F_VA, F_OUT,
	F_PAD, F_SSIZE, F_VAL, F_TIMEOUT, F_COUNT, F_END };

struct field { uint8_t off, size, kind; };

struct cdesc {
	const char	*name;
	unsigned long	 cmd;
	uint32_t	 ssize;
	int		 priv;		/* must never succeed here */
	struct field	 f[12];
};

#define FLD(t, m, k) { offsetof(t, m), sizeof(((t *)0)->m), k }
#define FEND { 0, 0, F_END }

static const struct cdesc cmds[NCMD] = {
	{ "BO_ALLOC", ATRIUM_GPU_IOC_BO_ALLOC, sizeof(struct atrium_gpu_bo_alloc), 0, {
	    FLD(struct atrium_gpu_bo_alloc, size, F_BOSIZE),
	    FLD(struct atrium_gpu_bo_alloc, bo_fd, F_OUT),
	    FLD(struct atrium_gpu_bo_alloc, flags, F_FLAGS), FEND } },
	{ "BO_WRITE", ATRIUM_GPU_IOC_BO_WRITE, sizeof(struct atrium_gpu_bo_xfer), 0, {
	    FLD(struct atrium_gpu_bo_xfer, offset, F_SIZE),
	    FLD(struct atrium_gpu_bo_xfer, len, F_SIZE),
	    FLD(struct atrium_gpu_bo_xfer, user_ptr, F_PTR),
	    FLD(struct atrium_gpu_bo_xfer, bo_fd, F_BOFD),
	    FLD(struct atrium_gpu_bo_xfer, pad, F_PAD), FEND } },
	{ "BO_READ", ATRIUM_GPU_IOC_BO_READ, sizeof(struct atrium_gpu_bo_xfer), 0, {
	    FLD(struct atrium_gpu_bo_xfer, offset, F_SIZE),
	    FLD(struct atrium_gpu_bo_xfer, len, F_SIZE),
	    FLD(struct atrium_gpu_bo_xfer, user_ptr, F_PTR),
	    FLD(struct atrium_gpu_bo_xfer, bo_fd, F_BOFD),
	    FLD(struct atrium_gpu_bo_xfer, pad, F_PAD), FEND } },
	{ "GET_IRQS", ATRIUM_GPU_IOC_GET_IRQS, sizeof(struct atrium_gpu_irqs), 0, {
	    FLD(struct atrium_gpu_irqs, count, F_OUT), FEND } },
	{ "SYNCOBJ_CREATE", ATRIUM_GPU_IOC_SYNCOBJ_CREATE, sizeof(struct atrium_gpu_syncobj_create), 0, {
	    FLD(struct atrium_gpu_syncobj_create, out_fd, F_OUT),
	    FLD(struct atrium_gpu_syncobj_create, pad, F_PAD), FEND } },
	{ "SYNCOBJ_SIGNAL", ATRIUM_GPU_IOC_SYNCOBJ_SIGNAL, sizeof(struct atrium_gpu_syncobj_op), 0, {
	    FLD(struct atrium_gpu_syncobj_op, value, F_VAL),
	    FLD(struct atrium_gpu_syncobj_op, syncobj_fd, F_SOFD),
	    FLD(struct atrium_gpu_syncobj_op, pad, F_PAD), FEND } },
	{ "SYNCOBJ_QUERY", ATRIUM_GPU_IOC_SYNCOBJ_QUERY, sizeof(struct atrium_gpu_syncobj_op), 0, {
	    FLD(struct atrium_gpu_syncobj_op, value, F_VAL),
	    FLD(struct atrium_gpu_syncobj_op, syncobj_fd, F_SOFD),
	    FLD(struct atrium_gpu_syncobj_op, pad, F_PAD), FEND } },
	{ "SYNCOBJ_WAIT", ATRIUM_GPU_IOC_SYNCOBJ_WAIT, sizeof(struct atrium_gpu_syncobj_wait), 0, {
	    FLD(struct atrium_gpu_syncobj_wait, value, F_VAL),
	    FLD(struct atrium_gpu_syncobj_wait, syncobj_fd, F_SOFD),
	    FLD(struct atrium_gpu_syncobj_wait, timeout_ms, F_TIMEOUT), FEND } },
	{ "VM_CREATE", ATRIUM_GPU_IOC_VM_CREATE, sizeof(struct atrium_gpu_vm_create), 0, {
	    FLD(struct atrium_gpu_vm_create, out_fd, F_OUT),
	    FLD(struct atrium_gpu_vm_create, pad, F_PAD), FEND } },
	{ "VM_BIND", ATRIUM_GPU_IOC_VM_BIND, sizeof(struct atrium_gpu_vm_bind), 0, {
	    FLD(struct atrium_gpu_vm_bind, va, F_VA),
	    FLD(struct atrium_gpu_vm_bind, vm_fd, F_VMFD),
	    FLD(struct atrium_gpu_vm_bind, bo_fd, F_BOFD), FEND } },
	{ "QUEUE_MAP", ATRIUM_GPU_IOC_QUEUE_MAP, sizeof(struct atrium_gpu_queue_map), 0, {
	    FLD(struct atrium_gpu_queue_map, doorbell_mmap_offset, F_OUT),
	    FLD(struct atrium_gpu_queue_map, vm_fd, F_VMFD),
	    FLD(struct atrium_gpu_queue_map, ring_fd, F_BOFD),
	    FLD(struct atrium_gpu_queue_map, engine, F_FLAGS), FEND } },
	{ "QUERY_CAPS", ATRIUM_GPU_IOC_QUERY_CAPS, sizeof(struct atrium_gpu_caps_query), 0, {
	    FLD(struct atrium_gpu_caps_query, caps_ptr, F_PTR),
	    FLD(struct atrium_gpu_caps_query, caps_size, F_SIZE), FEND } },
	{ "VM_BIND2", ATRIUM_GPU_IOC_VM_BIND2, sizeof(struct atrium_gpu_vm_bind2), 0, {
	    FLD(struct atrium_gpu_vm_bind2, va, F_VA),
	    FLD(struct atrium_gpu_vm_bind2, vm_fd, F_VMFD),
	    FLD(struct atrium_gpu_vm_bind2, bo_fd, F_BOFD),
	    FLD(struct atrium_gpu_vm_bind2, flags, F_FLAGS),
	    FLD(struct atrium_gpu_vm_bind2, pad, F_PAD), FEND } },
	{ "QUEUE_CREATE", ATRIUM_GPU_IOC_QUEUE_CREATE, sizeof(struct atrium_gpu_queue_create), 0, {
	    FLD(struct atrium_gpu_queue_create, struct_size, F_SSIZE),
	    FLD(struct atrium_gpu_queue_create, family_id, F_FLAGS),
	    FLD(struct atrium_gpu_queue_create, priority, F_FLAGS),
	    FLD(struct atrium_gpu_queue_create, flags, F_FLAGS),
	    FLD(struct atrium_gpu_queue_create, out_queue_fd, F_OUT),
	    FLD(struct atrium_gpu_queue_create, _pad, F_PAD), FEND } },
	{ "QUEUE_MAP2", ATRIUM_GPU_IOC_QUEUE_MAP2, sizeof(struct atrium_gpu_queue_map2), 0, {
	    FLD(struct atrium_gpu_queue_map2, struct_size, F_SSIZE),
	    FLD(struct atrium_gpu_queue_map2, _reserved, F_PAD),
	    FLD(struct atrium_gpu_queue_map2, out_ring_offset, F_OUT), FEND } },
	{ "QUEUE_SUBMIT", ATRIUM_GPU_IOC_QUEUE_SUBMIT, sizeof(struct atrium_gpu_queue_submit), 0, {
	    FLD(struct atrium_gpu_queue_submit, struct_size, F_SSIZE),
	    FLD(struct atrium_gpu_queue_submit, flags, F_FLAGS),
	    FLD(struct atrium_gpu_queue_submit, blob_va, F_VA),
	    FLD(struct atrium_gpu_queue_submit, blob_size, F_BLOBSZ),
	    FLD(struct atrium_gpu_queue_submit, in_dep_ptr, F_DEPPTR),
	    FLD(struct atrium_gpu_queue_submit, in_dep_count, F_COUNT),
	    FLD(struct atrium_gpu_queue_submit, deadline_ns, F_FLAGS),
	    FLD(struct atrium_gpu_queue_submit, out_dep_ptr, F_DEPPTR),
	    FLD(struct atrium_gpu_queue_submit, out_dep_count, F_COUNT),
	    FLD(struct atrium_gpu_queue_submit, _pad2, F_PAD),
	    FLD(struct atrium_gpu_queue_submit, out_seqno, F_OUT), FEND } },
	{ "GPU_RESET", ATRIUM_GPU_IOC_GPU_RESET, 0, 1, { FEND } },
	{ "SCHED", ATRIUM_GPU_IOC_SCHED, sizeof(struct atrium_gpu_sched), 1, {
	    FLD(struct atrium_gpu_sched, op, F_FLAGS),
	    FLD(struct atrium_gpu_sched, arg, F_FLAGS), FEND } },
	{ "POWERGATE", ATRIUM_GPU_IOC_POWERGATE, sizeof(struct atrium_gpu_powergate), 1, {
	    FLD(struct atrium_gpu_powergate, busy_mask, F_FLAGS),
	    FLD(struct atrium_gpu_powergate, next_busy, F_FLAGS), FEND } },
	{ "old SUBMIT", SI_OLD_IOC_SUBMIT, sizeof(struct si_old_submit), 1, {
	    FLD(struct si_old_submit, vm_fd, F_FD),
	    FLD(struct si_old_submit, ring_fd, F_FD),
	    FLD(struct si_old_submit, n_dwords, F_SIZE), FEND } },
};
#define C_QUEUE_CREATE	13

/*
 * Memory the pointer fields aim at. `buf` and `ro` are only read and
 * written by the kernel and by the thread that owns them; `flap` is
 * unmapped, re-mapped and re-protected by the flapper thread, so no thread
 * but the flapper ever touches it.
 */
#define BUF_SZ		(64 * 1024)
#define FLAP_PAGES	4
static char *flap;
static char *ro;
static size_t pgsz;
static int fuzz_uid_root;		/* running as root: user mode allowed */
/* SI_FUZZ_NOCHECK=1: no canary and no clean-run checks, for a model whose
 * queue-fd completions do not arrive (gpusim b035966: the IB's work lands,
 * the fence never does); the kernel paths are still fuzzed. */
static int nocheck;

struct tctx {
	int		 id;
	uint64_t	 rs;
	char		*buf;		/* BUF_SZ, a guard page after it */
	struct atrium_gpu_syncobj_point *deps;
	uint64_t	 vas[16];
	int		 nva;
	int		 typed[4][8];	/* recent VM, BO, syncobj, queue fds */
	int		 ntyped[4];
};
enum { TY_VM, TY_BO, TY_SO, TY_Q };

static void
remember(struct tctx *t, int ty, int f)
{
	if (f < 0)
		return;
	if (t->ntyped[ty] < 8)
		t->typed[ty][t->ntyped[ty]++] = f;
	else
		t->typed[ty][rnd(&t->rs) % 8] = f;
}

static uint64_t
pick_fd(struct tctx *t)
{
	static const int64_t odd[] = { -1, MASTER_FD, 0x7fffffff, 1024, -2,
	    0x80000000LL };
	uint32_t r = rn(&t->rs, 100);

	if (r < 85)
		return (rn(&t->rs, 64));
	if (r < 93)
		return (rn(&t->rs, 256));
	if (r < 97)
		return ((uint32_t)odd[rn(&t->rs, nitems(odd))]);
	return ((uint32_t)rnd(&t->rs));
}

/* Mostly a recent fd of the type the field wants (it may have been closed
 * or replaced since: that is the point); else any fd. */
static uint64_t
pick_typed(struct tctx *t, int ty)
{
	if (t->ntyped[ty] > 0 && rn(&t->rs, 10) < 7)
		return ((uint32_t)t->typed[ty][rn(&t->rs, t->ntyped[ty])]);
	return (pick_fd(t));
}

static uint64_t
pick_ptr(struct tctx *t, int dep)
{
	switch (rn(&t->rs, 12)) {
	case 0: return (0);
	case 1: return ((uint64_t)(uintptr_t)(t->buf + BUF_SZ - 8));
	case 2: return ((uint64_t)(uintptr_t)(flap + rn(&t->rs, FLAP_PAGES * pgsz)));
	case 3: return (0xfffff80000000000ULL + (rnd(&t->rs) & 0xffff000));
	case 4: return (~0ULL - rn(&t->rs, 64));
	case 5: return ((uint64_t)(uintptr_t)(t->buf + 1 + rn(&t->rs, 7)));
	case 6: return ((uint64_t)(uintptr_t)ro);
	case 7: return (rnd(&t->rs));
	case 8: return (0x1000 + rn(&t->rs, 0x10000));
	default:
		return ((uint64_t)(uintptr_t)(dep ? (char *)t->deps :
		    t->buf + (rn(&t->rs, 4) == 0 ? rn(&t->rs, BUF_SZ) : 0)));
	}
}

static uint64_t
pick_size(struct tctx *t, int bo)
{
	static const uint64_t odd[] = { 0, 1, 3, 4, 8, 4095, 4096, 4097, 65536,
	    1ULL << 31, 1ULL << 32, 1ULL << 48, 1ULL << 63, ~0ULL, ~0ULL - 4095,
	    ~0ULL - 3, 0x7fffffffffffffffULL };
	uint32_t r = rn(&t->rs, 100);

	if (bo) {
		/* Real allocations stay small; the absurd ones must fail. */
		if (r < 70)
			return (4096ULL * (1 + rn(&t->rs, 16)));
		if (r < 75)
			return (1 + rn(&t->rs, 8192));
		if (r < 77)
			return (1ULL << (20 + rn(&t->rs, 5)));	/* 1-16 MiB */
		if (r < 78)
			return (1ULL << 32);
		r = 90;
	}
	if (r < 40)
		return (rn(&t->rs, 8192));
	if (r < 80)
		return (odd[rn(&t->rs, nitems(odd))]);
	return (rnd(&t->rs));
}

static uint64_t
pick_flags(struct tctx *t)
{
	uint32_t r = rn(&t->rs, 10);

	if (r < 4)
		return (0);
	if (r < 7)
		return (1u << rn(&t->rs, 32));
	if (r < 8)
		return (rn(&t->rs, 8));
	return ((uint32_t)rnd(&t->rs));
}

static uint64_t
pick_va(struct tctx *t)
{
	static const uint64_t odd[] = { 0, 1, 0x1000, 1ULL << 47, (1ULL << 47) - 4096,
	    (1ULL << 48) - 4096, 0xffff800000000000ULL, ~0ULL, ~0ULL - 4095,
	    0x800000000000ULL - 1 };
	uint32_t r = rn(&t->rs, 100);

	if (r < 30 && t->nva > 0)
		return (t->vas[rn(&t->rs, t->nva)] + (rn(&t->rs, 4) == 0 ?
		    (rnd(&t->rs) & 0xfffc) : 0));
	if (r < 60)
		return (0);
	if (r < 80)
		return (odd[rn(&t->rs, nitems(odd))]);
	if (r < 90)
		return (rnd(&t->rs) & 0x0000fffffffff000ULL);
	return (rnd(&t->rs));
}

static void
fill_field(struct tctx *t, char *s, const struct field *f, uint32_t ssize)
{
	uint64_t v = 0;

	switch (f->kind) {
	case F_FD: v = pick_fd(t); break;
	case F_VMFD: v = pick_typed(t, TY_VM); break;
	case F_BOFD: v = pick_typed(t, TY_BO); break;
	case F_SOFD: v = pick_typed(t, TY_SO); break;
	case F_BLOBSZ:
		v = rn(&t->rs, 4) ? 4 * (uint64_t)(1 + rn(&t->rs, 64)) :
		    pick_size(t, 0);
		break;
	case F_PTR: v = pick_ptr(t, 0); break;
	case F_DEPPTR: v = rn(&t->rs, 3) == 0 ? 0 : pick_ptr(t, rn(&t->rs, 3) != 0); break;
	case F_SIZE: v = pick_size(t, 0); break;
	case F_BOSIZE: v = pick_size(t, 1); break;
	case F_FLAGS: v = pick_flags(t); break;
	case F_VA: v = pick_va(t); break;
	case F_OUT: v = rn(&t->rs, 4) == 0 ? rnd(&t->rs) : 0; break;
	case F_PAD: v = rn(&t->rs, 8) == 0 ? rnd(&t->rs) : 0; break;
	case F_SSIZE:
		switch (rn(&t->rs, 10)) {
		case 0: v = 0; break;
		case 1: v = ssize - 4; break;
		case 2: v = ssize + 4; break;
		case 3: v = 4096; break;
		case 4: v = ~0u; break;
		case 5: v = rnd(&t->rs); break;
		default: v = ssize; break;
		}
		break;
	case F_VAL: v = rn(&t->rs, 3) == 0 ? rnd(&t->rs) : rn(&t->rs, 64); break;
	case F_TIMEOUT: v = rn(&t->rs, 4) == 0 ? rn(&t->rs, 100) : 0; break;
	case F_COUNT:
		switch (rn(&t->rs, 8)) {
		case 0: v = 0x7fffffff; break;
		case 1: v = ~0u; break;
		case 2: v = 1u << (16 + rn(&t->rs, 12)); break;
		case 3: case 4: v = 0; break;
		default: v = 1 + rn(&t->rs, 4); break;
		}
		break;
	}
	memcpy(s + f->off, &v, f->size);	/* little-endian: the low bytes */
}

static void
note(struct tctx *t, const char *fmt, ...) __attribute__((format(printf, 2, 3)));

static void
note(struct tctx *t, const char *fmt, ...)
{
	va_list ap;

	va_start(ap, fmt);
	vsnprintf(S->t[t->id].last, sizeof(S->t[t->id].last), fmt, ap);
	va_end(ap);
}

static void
enter(struct tctx *t)
{
	atomic_store(&S->t[t->id].since_ms, now_ms());
}

static void
leave(struct tctx *t)
{
	atomic_store(&S->t[t->id].since_ms, 0);
	atomic_fetch_add(&S->t[t->id].ops, 1);
}

/* One mutated call of a known ioctl on a random fd. */
static void
op_ioctl(struct tctx *t)
{
	const struct cdesc *c;
	char s[256] __aligned(8);
	uint32_t sent_flags = 0;
	uint64_t f;
	int ci, i, r, e, target;

	ci = rn(&t->rs, NCMD);
	c = &cmds[ci];
	memset(s, 0, sizeof(s));
	if (rn(&t->rs, 16) == 0)
		for (i = 0; i < (int)c->ssize; i++)
			s[i] = (char)rnd(&t->rs);
	for (i = 0; c->f[i].kind != F_END; i++)
		fill_field(t, s, &c->f[i], c->ssize);
	/* The deps the pointers may aim at: random fds and values. */
	for (i = 0; i < 4; i++) {
		t->deps[i].syncobj_fd = (int32_t)pick_typed(t, TY_SO);
		t->deps[i]._pad = rn(&t->rs, 8) == 0 ? (uint32_t)rnd(&t->rs) : 0;
		t->deps[i].value = rn(&t->rs, 2) ? rn(&t->rs, 64) : rnd(&t->rs);
	}
	if (ci == C_QUEUE_CREATE)
		memcpy(&sent_flags, s + offsetof(struct atrium_gpu_queue_create,
		    flags), 4);
	/* Mostly the fd the call belongs on; sometimes any fd at all. */
	if (rn(&t->rs, 10) < 7) {
		if (c->cmd == ATRIUM_GPU_IOC_QUEUE_CREATE)
			f = pick_typed(t, TY_VM);
		else if (c->cmd == ATRIUM_GPU_IOC_QUEUE_SUBMIT ||
		    c->cmd == ATRIUM_GPU_IOC_QUEUE_MAP2)
			f = pick_typed(t, TY_Q);
		else
			f = MASTER_FD;
	} else
		f = pick_fd(t);
	target = (int)(uint32_t)f;
	note(t, "ioctl %s on fd %d [%02x %02x %02x %02x %02x %02x %02x %02x "
	    "%02x %02x %02x %02x %02x %02x %02x %02x ...]", c->name, target,
	    (uint8_t)s[0], (uint8_t)s[1], (uint8_t)s[2], (uint8_t)s[3],
	    (uint8_t)s[4], (uint8_t)s[5], (uint8_t)s[6], (uint8_t)s[7],
	    (uint8_t)s[8], (uint8_t)s[9], (uint8_t)s[10], (uint8_t)s[11],
	    (uint8_t)s[12], (uint8_t)s[13], (uint8_t)s[14], (uint8_t)s[15]);
	enter(t);
	r = ioctl(target, c->cmd, s);
	e = r == 0 ? 0 : errno;
	leave(t);
	atomic_fetch_add(&S->hist[ci][ebucket(e)], 1);
	if (r != 0)
		return;
	if (c->priv && target == MASTER_FD)
		finding("%s succeeded on the client node (uid %d)", c->name,
		    (int)getuid());
	if (ci == C_QUEUE_CREATE &&
	    (sent_flags & ATRIUM_GPU_QUEUE_FLAG_USER_MODE) && !fuzz_uid_root)
		finding("QUEUE_CREATE USER_MODE succeeded for uid %d",
		    (int)getuid());
	if (c->cmd == ATRIUM_GPU_IOC_VM_CREATE)
		remember(t, TY_VM, (int)((struct atrium_gpu_vm_create *)(void *)s)->out_fd);
	else if (c->cmd == ATRIUM_GPU_IOC_BO_ALLOC)
		remember(t, TY_BO, (int)((struct atrium_gpu_bo_alloc *)(void *)s)->bo_fd);
	else if (c->cmd == ATRIUM_GPU_IOC_SYNCOBJ_CREATE)
		remember(t, TY_SO, (int)((struct atrium_gpu_syncobj_create *)(void *)s)->out_fd);
	else if (c->cmd == ATRIUM_GPU_IOC_QUEUE_CREATE)
		remember(t, TY_Q, ((struct atrium_gpu_queue_create *)(void *)s)->out_queue_fd);
	if (c->cmd == ATRIUM_GPU_IOC_VM_BIND2 || c->cmd == ATRIUM_GPU_IOC_VM_BIND) {
		uint64_t va;

		memcpy(&va, s, 8);
		t->vas[t->nva < 16 ? t->nva++ : rn(&t->rs, 16)] = va;
	}
}

/* A random 'A' ioctl number: any direction, number and size. */
static void
op_rawioctl(struct tctx *t)
{
	static const unsigned long dirs[] = { IOC_VOID, IOC_OUT, IOC_IN,
	    IOC_INOUT };
	unsigned long cmd;
	char s[256] __aligned(8);
	int fdn, i, r, e;

	for (i = 0; i < (int)sizeof(s); i++)
		s[i] = (char)rnd(&t->rs);
	cmd = _IOC(dirs[rn(&t->rs, 4)], 'A', rn(&t->rs, 128),
	    rn(&t->rs, 4) == 0 ? rn(&t->rs, 256) : 8 * rn(&t->rs, 12));
	if (rn(&t->rs, 4) == 0)
		cmd = ((uint32_t)rnd(&t->rs) & ~(unsigned long)0x1fff0000) |
		    ((unsigned long)rn(&t->rs, 256) << 16);
	fdn = rn(&t->rs, 2) ? MASTER_FD : (int)pick_fd(t);
	note(t, "raw ioctl %#lx on fd %d", cmd, fdn);
	enter(t);
	r = ioctl(fdn, cmd, s);
	e = r == 0 ? 0 : errno;
	leave(t);
	(void)e;
}

/* Objects made the honest way, so the fd pool has live ones of each kind. */
static void
op_create(struct tctx *t)
{
	uint64_t va;
	int a, b;

	switch (rn(&t->rs, 8)) {
	case 0:
		note(t, "create VM");
		enter(t); remember(t, TY_VM, vm_create()); leave(t);
		break;
	case 1: case 2:
		note(t, "create BO");
		enter(t);
		remember(t, TY_BO, bo_alloc(4096ULL * (1 + rn(&t->rs, 4)),
		    rn(&t->rs, 4) == 0 ? ATRIUM_GPU_BO_VRAM : 0));
		leave(t);
		break;
	case 3:
		note(t, "create syncobj");
		enter(t); remember(t, TY_SO, syncobj()); leave(t);
		break;
	case 4:
		a = (int)pick_typed(t, TY_VM);
		note(t, "create queue on fd %d", a);
		enter(t); remember(t, TY_Q, queue_create(a)); leave(t);
		break;
	case 5:
		a = (int)pick_typed(t, TY_VM);
		b = (int)pick_typed(t, TY_BO);
		note(t, "bind fd %d into fd %d", b, a);
		enter(t);
		if (vm_bind(a, b, rn(&t->rs, 2) ? ATRIUM_GPU_BIND_READONLY : 0,
		    &va) == 0)
			t->vas[t->nva < 16 ? t->nva++ : rn(&t->rs, 16)] = va;
		leave(t);
		break;
	case 6:
		note(t, "kqueue");
		(void)kqueue();
		break;
	default: {
		int p[2];

		note(t, "pipe/socketpair/file");
		switch (rn(&t->rs, 3)) {
		case 0: (void)pipe(p); break;
		case 1: (void)socketpair(AF_UNIX, SOCK_STREAM, 0, p); break;
		default: (void)open("/dev/null", O_RDWR); break;
		}
	}
	}
}

/* Closes and dup2s over fds that other threads may be calling on. */
static void
op_close(struct tctx *t)
{
	int a, b;

	a = POOL_LO + (int)rn(&t->rs, rn(&t->rs, 2) ? POOL_HI - POOL_LO :
	    FD_SOFTMAX + 20 - POOL_LO);
	/* Often a recent VM, BO, syncobj or queue: VMs are a scarce
	 * resource (ENOSPC), and closing a live object is the torn case. */
	if (rn(&t->rs, 5) < 2) {
		int ty = (int)rn(&t->rs, 4);

		if (t->ntyped[ty] > 0)
			a = t->typed[ty][rn(&t->rs, t->ntyped[ty])];
	}
	if (a == MASTER_FD || a < POOL_LO)
		return;
	if (rn(&t->rs, 3) == 0) {
		b = POOL_LO + (int)rn(&t->rs, POOL_HI - POOL_LO);
		note(t, "dup2 %d over %d", a, b);
		(void)dup2(a, b);
		return;
	}
	note(t, "close %d", a);
	enter(t);
	(void)close(a);
	leave(t);
}

static __thread sigjmp_buf *fault_jb;

static void
on_fault(int sig)
{
	if (fault_jb != NULL)
		siglongjmp(*fault_jb, 1);
	signal(sig, SIG_DFL);
	raise(sig);
}

/* mmap a random fd at a random offset; touch it under a fault guard. */
static void
op_mmap(struct tctx *t)
{
	static const int prots[] = { PROT_READ, PROT_READ | PROT_WRITE,
	    PROT_READ | PROT_EXEC, PROT_READ | PROT_WRITE | PROT_EXEC };
	sigjmp_buf jb;
	volatile char sink;
	size_t len;
	off_t off;
	void *p;
	int fdn;

	fdn = rn(&t->rs, 2) ? MASTER_FD : (int)pick_fd(t);
	len = pgsz * (1 + rn(&t->rs, 4));
	off = rn(&t->rs, 2) ? (off_t)(pgsz * rn(&t->rs, 64)) :
	    (off_t)(rnd(&t->rs) & ~(uint64_t)(pgsz - 1) & 0x7fffffffffffULL);
	note(t, "mmap fd %d off %#jx len %zu", fdn, (uintmax_t)off, len);
	enter(t);
	p = mmap(NULL, len, prots[rn(&t->rs, 4)], rn(&t->rs, 4) ?
	    MAP_SHARED : MAP_PRIVATE, fdn, off);
	leave(t);
	if (p == MAP_FAILED)
		return;
	atomic_fetch_add(&S->mmaps_ok, 1);
	fault_jb = &jb;
	if (sigsetjmp(jb, 1) == 0)
		sink = *(volatile char *)p;
	fault_jb = NULL;
	(void)sink;
	munmap(p, len);
}

/* Wait on a random fd as a syncobj, through kqueue; never blocks. */
static void
op_kq(struct tctx *t)
{
	struct timespec ts = { 0, 1000000L * rn(&t->rs, 3) };
	struct kevent kev;
	int kq, fdn;

	fdn = (int)pick_fd(t);
	kq = kqueue();
	if (kq < 0)
		return;
	EV_SET(&kev, fdn, EVFILT_READ, EV_ADD | (rn(&t->rs, 2) ? EV_ONESHOT : 0),
	    0, (intptr_t)rnd(&t->rs) & (rn(&t->rs, 2) ? 0x3f : -1), NULL);
	note(t, "kevent on fd %d", fdn);
	enter(t);
	(void)kevent(kq, &kev, 1, &kev, 1, &ts);
	leave(t);
	if (rn(&t->rs, 4) == 0)
		return;		/* left open: a kqueue holding a knote */
	close(kq);
}

/* A child that inherits everything, calls a few times and exits. */
static void
op_fork(struct tctx *t)
{
	struct tctx c;
	pid_t pid;
	int i, st;

	note(t, "fork");
	atomic_fetch_add(&S->forks, 1);
	pid = fork();
	if (pid == 0) {
		c = *t;
		c.rs ^= 0x9e3779b97f4a7c15ULL;
		for (i = 0; i < 64; i++) {
			if (rn(&c.rs, 3) == 0)
				op_close(&c);
			else
				op_ioctl(&c);
		}
		_exit(0);
	}
	if (pid > 0)
		(void)waitpid(pid, &st, 0);
}

/* The lowest free fd number: fds are handed out lowest first, so past
 * FD_SOFTMAX the table below is full and closes come first. */
static int
lowest_free(void)
{
	int f = dup(0);

	if (f >= 0)
		close(f);
	return (f < 0 ? FD_SOFTMAX + 1 : f);
}

static void *
ioctl_thread(void *arg)
{
	struct tctx *t = arg;
	uint32_t r;

	for (;;) {
		if ((r = (uint32_t)lowest_free()) > FD_SOFTMAX) {
			op_close(t);
			continue;
		}
		r = rn(&t->rs, 1000);
		if (r < 560)
			op_ioctl(t);
		else if (r < 620)
			op_rawioctl(t);
		else if (r < 760)
			op_create(t);
		else if (r < 890)
			op_close(t);
		else if (r < 930)
			op_kq(t);
		else if (r < 960)
			op_mmap(t);
		else if (r < 961)
			op_fork(t);
		else if (r < 962 && rn(&t->rs, 8) == 0) {
			/* A purge: every fd but the device's, while the
			 * other threads keep calling on them. */
			int i;

			note(t, "purge");
			for (i = POOL_LO; i < FD_SOFTMAX + 20; i++)
				if (i != MASTER_FD)
					(void)close(i);
			memset(t->ntyped, 0, sizeof(t->ntyped));
		}
		else
			op_ioctl(t);
	}
	return (NULL);
}

/* Keeps unmapping, re-mapping and re-protecting the flap pages. */
static void *
flap_thread(void *arg)
{
	static const int prots[] = { PROT_NONE, PROT_READ,
	    PROT_READ | PROT_WRITE };
	struct tctx *t = arg;
	size_t i, off;
	char *p;

	for (;;) {
		off = pgsz * rn(&t->rs, FLAP_PAGES);
		switch (rn(&t->rs, 4)) {
		case 0:	/* "unmapped", but reserved: nothing else lands there */
			(void)mmap(flap + off, pgsz, PROT_NONE,
			    MAP_GUARD | MAP_FIXED, -1, 0);
			break;
		case 1:
			p = mmap(flap + off, pgsz, PROT_READ | PROT_WRITE,
			    MAP_ANON | MAP_PRIVATE | MAP_FIXED, -1, 0);
			if (p != MAP_FAILED)
				for (i = 0; i < pgsz; i += 8)
					*(uint64_t *)(p + i) = rn(&t->rs, 4) ?
					    rn(&t->rs, 64) : rnd(&t->rs);
			break;
		default:
			(void)mprotect(flap + off, pgsz,
			    prots[rn(&t->rs, nitems(prots))]);
			break;
		}
		usleep(rn(&t->rs, 200));
	}
	return (NULL);
}

static void
drop_to(uid_t uid)
{
	gid_t g = (gid_t)uid;

	if (setgroups(1, &g) != 0 || setgid(g) != 0 || setuid(uid) != 0) {
		printf("si-fuzz: cannot become uid %u: %s\n", (u_int)uid,
		    strerror(errno));
		_exit(2);
	}
}

/* Close everything but stdio and the device fd (the canary's fds stay out). */
static void
close_inherited(void)
{
	int i, n = getdtablesize();

	for (i = 3; i < n && i < 4096; i++)
		if (i != MASTER_FD)
			(void)close(i);
}

static void
ioctl_worker(uint64_t seed, int nthreads, uid_t uid)
{
	static struct tctx tc[MAXT];
	pthread_t th;
	struct tctx ft;
	int i, c;

	close_inherited();
	drop_to(uid);
	fuzz_uid_root = 0;
	c = open(CTL_DEV, O_RDWR);
	if (c >= 0) {
		finding("uid %u opened %s", (u_int)uid, CTL_DEV);
		close(c);
	}
	/* The pool starts with a few honest objects of each kind. */
	for (i = 0; i < 6; i++) {
		int vm = vm_create(), bo = bo_alloc(4096, 0);
		uint64_t va;

		(void)syncobj();
		if (vm >= 0) {
			if (bo >= 0)
				(void)vm_bind(vm, bo, 0, &va);
			(void)queue_create(vm);
		}
	}
	memset(&ft, 0, sizeof(ft));
	ft.id = MAXT - 1;
	ft.rs = seed ^ 0xf1a9f1a9ULL;
	for (i = 0; i < nthreads && i < MAXT - 1; i++) {
		tc[i].id = i;
		tc[i].rs = seed + 0x1000193ULL * (uint64_t)(i + 1);
		tc[i].buf = mmap(NULL, BUF_SZ + pgsz, PROT_READ | PROT_WRITE,
		    MAP_ANON | MAP_PRIVATE, -1, 0);
		if (tc[i].buf == MAP_FAILED)
			_exit(2);
		(void)mprotect(tc[i].buf + BUF_SZ, pgsz, PROT_NONE);
		tc[i].deps = (void *)(tc[i].buf + BUF_SZ - 4 * pgsz);
		if (pthread_create(&th, NULL, ioctl_thread, &tc[i]) != 0)
			_exit(2);
	}
	if (pthread_create(&th, NULL, flap_thread, &ft) != 0)
		_exit(2);
	for (;;)
		pause();
}

/* ---- target 2: submit blobs ---- */

#define BLOB_MAX	1024		/* dwords: the 4 KiB IB page */

static const uint8_t pm4_ops[] = {
	0x10, 0x11, 0x12, 0x15, 0x16, 0x1e, 0x22, 0x23, 0x28, 0x33, 0x37,
	0x39, 0x3c, 0x3f, 0x40, 0x42, 0x43, 0x45, 0x46, 0x49, 0x50, 0x58,
	0x59, 0x5d, 0x5f, 0x68, 0x69, 0x76, 0x79, 0x9b,
};

struct bctx {
	uint64_t	 rs;
	struct job	 j;
	int		 hang_wait;
};

static uint32_t
blob_word(struct bctx *b)
{
	struct job *j = &b->j;
	uint64_t va;

	switch (rn(&b->rs, 12)) {
	case 0: case 1: return ((uint32_t)rnd(&b->rs));
	case 2: return (rn(&b->rs, 16));
	case 3: return (0xffffffffu);
	case 4:	/* a VA of our own, low or high half */
	case 5:
		va = (rn(&b->rs, 3) == 0 ? j->shader_va : rn(&b->rs, 2) ?
		    j->out_va : j->ib_va) + (rnd(&b->rs) & 0xffc);
		return (rn(&b->rs, 2) ? (uint32_t)va : (uint32_t)(va >> 32));
	case 6: return (0x2c00 + rn(&b->rs, 0x400));	/* SH regs */
	case 7: return (rn(&b->rs, 0x400));		/* SH/ctx offsets */
	case 8: return (rn(&b->rs, 0x10000));		/* any register */
	case 9: return (1u << rn(&b->rs, 32));
	default: return (rn(&b->rs, 0x100));
	}
}

/* A random packet at w; its length in dwords (<= room). */
static u_int
blob_packet(struct bctx *b, uint32_t *w, u_int room)
{
	u_int n, i, op;

	if (room < 2)
		return (0);
	n = rn(&b->rs, 4) == 0 ? rn(&b->rs, MIN(room - 1, 64u)) :
	    rn(&b->rs, MIN(room - 1, 8u));
	switch (rn(&b->rs, 16)) {
	case 0:		/* type 0: register writes */
		w[0] = 0u << 30 | (n & 0x3fff) << 16 | rn(&b->rs, 0x10000);
		break;
	case 1:		/* type 2 filler */
		w[0] = 2u << 30;
		return (1);
	case 2:		/* anything */
		w[0] = (uint32_t)rnd(&b->rs);
		n = MIN((w[0] >> 16) & 0x3fff, room - 1);
		break;
	default:
		op = rn(&b->rs, 8) == 0 ? rn(&b->rs, 0x100) :
		    pm4_ops[rn(&b->rs, nitems(pm4_ops))];
		/* the count field is body dwords - 1 */
		w[0] = PKT3(op, n == 0 ? 0 : n - 1) |
		    (rn(&b->rs, 8) == 0 ? rn(&b->rs, 4) : 0);
		if (n == 0)
			n = 1;
		if (n > room - 1)
			n = room - 1;
		break;
	}
	for (i = 1; i <= n; i++)
		w[i] = blob_word(b);
	return (1 + n);
}

/* A mutated blob in w; its size in dwords. Sets *va and *bytes. */
static u_int
blob_make(struct bctx *b, uint32_t *w, uint64_t *va, uint64_t *bytes,
    const char **kind)
{
	struct job *j = &b->j;
	u_int n = 0, i, k, cut;

	*va = j->ib_va;
	memcpy(w, siw42_ib, sizeof(siw42_ib));
	for (i = 0; i < SIW42_NPATCH; i++)
		w[siw42_patch[i].dw] = (uint32_t)((siw42_patch[i].which ?
		    j->out_va : j->shader_va) >> siw42_patch[i].shift);
	switch (rn(&b->rs, 10)) {
	case 0: case 1: case 2:
		*kind = "w42 mutated";
		n = SIW42_IB_DW;
		for (k = 1 + rn(&b->rs, 8); k > 0; k--) {
			i = rn(&b->rs, n);
			switch (rn(&b->rs, 3)) {
			case 0: w[i] ^= 1u << rn(&b->rs, 32); break;
			case 1: w[i] = blob_word(b); break;
			default:
				if (n < BLOB_MAX) {	/* insert */
					memmove(w + i + 1, w + i, (n - i) * 4);
					w[i] = blob_word(b);
					n++;
				}
				break;
			}
		}
		break;
	case 3: case 4: case 5: case 6:
		*kind = "packets";
		for (k = 1 + rn(&b->rs, 30); k > 0 && n < BLOB_MAX - 8; k--)
			n += blob_packet(b, w + n, BLOB_MAX - 8 - n);
		if (rn(&b->rs, 2))
			n += pr_mark(w + n, j->out_va, PR_MARK);
		break;
	case 7: case 8:
		*kind = "splice";
		cut = rn(&b->rs, SIW42_IB_DW);
		n = cut;
		for (k = 1 + rn(&b->rs, 10); k > 0 && n < BLOB_MAX - 8; k--)
			n += blob_packet(b, w + n, BLOB_MAX - 8 - n);
		if (rn(&b->rs, 2) && n + SIW42_IB_DW - cut <= BLOB_MAX) {
			memcpy(w + n, siw42_ib + cut, (SIW42_IB_DW - cut) * 4);
			n += SIW42_IB_DW - cut;
		}
		break;
	default:
		*kind = "blob va/size";
		n = SIW42_IB_DW;
		switch (rn(&b->rs, 5)) {
		case 0: *va = j->ib_va + 4 * rn(&b->rs, SIW42_IB_DW); break;
		case 1: *va = j->out_va; break;
		case 2: *va = j->shader_va; break;
		case 3: *va = j->ib_va + 4096 - 4 * rn(&b->rs, 8); break;
		default: *va = j->ib_va + 1 + rn(&b->rs, 3); break;
		}
		*bytes = rn(&b->rs, 2) ? 4 * (1 + (uint64_t)rn(&b->rs, 2048)) :
		    (uint64_t)n * 4;
		return (n);
	}
	*bytes = (uint64_t)n * 4;
	if (rn(&b->rs, 16) == 0)
		*bytes += rn(&b->rs, 2) ? 4 * rn(&b->rs, 64) : 1 + rn(&b->rs, 3);
	return (n);
}

/* QUEUE_SUBMIT of `bytes` (any size) at `va`. 0 or the errno. */
static int
blob_submit(int q, uint64_t va, uint64_t bytes, int so, uint64_t value)
{
	struct atrium_gpu_queue_submit s;
	struct atrium_gpu_syncobj_point pt;

	memset(&s, 0, sizeof(s));
	memset(&pt, 0, sizeof(pt));
	s.struct_size = sizeof(s);
	s.blob_va = va;
	s.blob_size = bytes;
	pt.syncobj_fd = so;
	pt.value = value;
	s.out_dep_ptr = (uint64_t)(uintptr_t)&pt;
	s.out_dep_count = 1;
	return (ioctl(q, ATRIUM_GPU_IOC_QUEUE_SUBMIT, &s) == 0 ? 0 : errno);
}

/* write_42 on the job's queue: 1 if 64 x 42 landed. */
static int
blob_clean(struct bctx *b, int *err)
{
	uint32_t first;
	int ms;

	*err = 0;
	if (poison(b->j.out, POISON_A) != 0 ||
	    job_submit_on(&b->j, b->j.q, b->j.out_va, NULL) != 0) {
		*err = errno;
		return (0);
	}
	*err = job_wait(&b->j, b->hang_wait, &ms);
	return (*err == 0 && count(b->j.out, SIW42_OUT_VALUE, &first) ==
	    SIW42_OUT_DW);
}

#define BLOB_LOCKED	3	/* the worker's uid is locked out */

static void
blob_worker(uint64_t seed, uid_t uid, int hang_wait)
{
	static uint32_t w[BLOB_MAX];
	struct slot *sl = &S->t[MAXT];
	struct bctx b;
	struct tctx nt;
	uint64_t va, bytes;
	const char *kind;
	u_int n;
	int r, ms, err, ri0, refused;

	close_inherited();
	drop_to(uid);
	memset(&b, 0, sizeof(b));
	memset(&nt, 0, sizeof(nt));
	nt.id = MAXT;
	b.rs = seed | 1;
	b.hang_wait = hang_wait;
	job_init(&b.j);
	for (;;) {
		if (b.j.vm < 0) {
			atomic_fetch_add(&S->blob_reopen, 1);
			if (job_open(&b.j, "blob") != 0) {
				if (errno == EPERM)
					_exit(BLOB_LOCKED);
				/* ENOSPC is the ioctl workers holding every
				 * VM; ETIMEDOUT is the GPU not answering. */
				if (errno == ETIMEDOUT)
					atomic_fetch_add(&S->setup_fails, 1);
				job_close(&b.j);
				job_init(&b.j);
				sleep(1);
				continue;
			}
		}
		atomic_store(&S->setup_fails, 0);
		ri0 = reinits();
		bytes = 0;
		n = blob_make(&b, w, &va, &bytes, &kind);
		snprintf(sl->last, sizeof(sl->last), "blob %s: %u dw, va %#jx "
		    "(IB %#jx) %ju bytes, w0 %08x", kind, n, (uintmax_t)va,
		    (uintmax_t)b.j.ib_va, (uintmax_t)bytes, w[0]);
		if (bo_xfer(b.j.ib, w, MIN(n, BLOB_MAX) * 4, 1) != 0) {
			job_close(&b.j);
			job_init(&b.j);
			continue;
		}
		if (poison(b.j.out, POISON_B) != 0) {
			job_close(&b.j);
			job_init(&b.j);
			continue;
		}
		atomic_fetch_add(&S->blob_jobs, 1);
		enter(&nt);
		r = blob_submit(b.j.q, va, bytes, b.j.so, ++b.j.value);
		refused = r != 0;
		if (r == 0)
			r = job_wait(&b.j, b.hang_wait, &ms);
		else
			b.j.value--;	/* refused: nothing owed */
		leave(&nt);
		if (r == ETIMEDOUT) {
			if (reinits() == ri0 && !nocheck)
				finding("blob job: no completion within %d ms "
				    "(%s)", b.hang_wait, sl->last);
			job_close(&b.j);
			job_init(&b.j);
			continue;
		}
		if (r == 0) {
			atomic_fetch_add(&S->blob_ok, 1);
			continue;
		}
		atomic_fetch_add(&S->blob_failed, 1);
		if (refused && r != EIO)
			continue;	/* refused at submit: the queue is fine */
		/* The queue is lost: a new one in the same VM runs clean. */
		atomic_fetch_add(&S->blob_renew, 1);
		if (job_renew(&b.j) != 0) {
			if (errno == EPERM)
				_exit(BLOB_LOCKED);
			job_close(&b.j);
			job_init(&b.j);
			continue;
		}
		enter(&nt);
		r = blob_clean(&b, &err);
		leave(&nt);
		if (r) {
			atomic_fetch_add(&S->blob_clean, 1);
			continue;
		}
		if (!nocheck && ri0 != -2 && no_reinit_since(ri0))
			finding("blob: the VM did not run clean on a new queue "
			    "after a contained failure (%s; after: %s)",
			    strerror(err), sl->last);
		job_close(&b.j);
		job_init(&b.j);
	}
}

/* ---- the canary and the supervisor ---- */

#define SEAL_PAT	0x5ea1ed00u

struct canary {
	struct job	j;
	int		seal;
	uint64_t	seal_va;
	int		bad;		/* consecutive failures */
};

static void
canary_close(struct canary *c)
{
	if (c->seal >= 0)
		close(c->seal);
	c->seal = -1;
	job_close(&c->j);
	job_init(&c->j);
}

static int
canary_open(struct canary *c)
{
	uint32_t w[1024];
	u_int i;

	c->seal = -1;
	if (job_open(&c->j, "canary") != 0)
		return (-1);
	for (i = 0; i < nitems(w); i++)
		w[i] = SEAL_PAT | i;
	if ((c->seal = bo_alloc(4096, 0)) < 0 ||
	    bo_xfer(c->seal, w, sizeof(w), 1) != 0 ||
	    vm_bind(c->j.vm, c->seal, ATRIUM_GPU_BIND_READONLY,
	    &c->seal_va) != 0)
		return (-1);
	return (0);
}

/* 0 = clean; else what went wrong in `why`. */
static int
canary_run(struct canary *c, int wait_ms, char *why, size_t wl)
{
	uint32_t w[1024], first;
	u_int i;
	int r, n, ms;

	if (poison(c->j.out, POISON_A) != 0 ||
	    job_submit_on(&c->j, c->j.q, c->j.out_va, NULL) != 0) {
		snprintf(why, wl, "submit: %s", strerror(errno));
		return (-1);
	}
	r = job_wait(&c->j, wait_ms, &ms);
	n = count(c->j.out, SIW42_OUT_VALUE, &first);
	if (r != 0 || n != SIW42_OUT_DW) {
		snprintf(why, wl, "completion %s after %d ms, %d/64 words 42",
		    r == 0 ? "ok" : strerror(r), ms, n);
		return (-1);
	}
	if (bo_xfer(c->seal, w, sizeof(w), 0) != 0) {
		snprintf(why, wl, "sealed page unreadable: %s",
		    strerror(errno));
		return (-1);
	}
	for (i = 0; i < nitems(w); i++)
		if (w[i] != (SEAL_PAT | i)) {
			snprintf(why, wl, "sealed page word %u is %#x", i,
			    w[i]);
			return (-1);
		}
	return (0);
}

struct worker {
	pid_t		pid;
	int64_t		started, until;
	uid_t		uid;
	const char	*name;
};

static void
worker_stop(struct worker *w)
{
	int64_t t0;
	int st;

	if (w->pid <= 0)
		return;
	kill(w->pid, SIGKILL);
	t0 = now_ms();
	while (waitpid(w->pid, &st, WNOHANG) == 0) {
		if (now_ms() - t0 > 30000) {
			finding("%s worker (uid %u) did not die 30 s after "
			    "SIGKILL: a thread stuck in the kernel", w->name,
			    (u_int)w->uid);
			dump_last();
			break;
		}
		usleep(100000);
	}
	w->pid = 0;
}

/* A worker that exited on its own: how. */
static void
worker_reaped(struct worker *w, int st)
{
	if (WIFSIGNALED(st) && WTERMSIG(st) != SIGKILL) {
		finding("%s worker (uid %u) died on signal %d", w->name,
		    (u_int)w->uid, WTERMSIG(st));
		dump_last();
	} else if (WIFEXITED(st) && WEXITSTATUS(st) != 0 &&
	    WEXITSTATUS(st) != BLOB_LOCKED)
		printf("si-fuzz: %s worker exited %d\n", w->name,
		    WEXITSTATUS(st));
	else if (WIFEXITED(st) && WEXITSTATUS(st) == BLOB_LOCKED)
		printf("si-fuzz: %s worker's uid %u locked out (reset blame)\n",
		    w->name, (u_int)w->uid);
	w->pid = 0;
}

static void
status(int64_t t0, int ri, uint64_t canary_runs)
{
	uint64_t ops = 0;
	int i;

	for (i = 0; i <= MAXT; i++)
		ops += S->t[i].ops;
	printf("si-fuzz: %4jd s: %ju calls, blob %ju jobs (%ju ok, %ju failed, "
	    "%ju clean after), canary %ju, re-inits %d, cp_errors %jd, "
	    "vm_faults %jd, hangs %jd, findings %d\n",
	    (intmax_t)((now_ms() - t0) / 1000), (uintmax_t)ops,
	    (uintmax_t)S->blob_jobs, (uintmax_t)S->blob_ok,
	    (uintmax_t)S->blob_failed, (uintmax_t)S->blob_clean,
	    (uintmax_t)canary_runs, ri, (intmax_t)iso("cp_errors"),
	    (intmax_t)iso("vm_faults"), (intmax_t)iso("hangs"),
	    (int)S->nfind);
}

static volatile sig_atomic_t stop_now;

static void
on_term(int sig)
{
	(void)sig;
	stop_now = 1;
}

static void
usage(void)
{
	printf("usage: si-fuzz <ioctl|blob|all> <seconds> [seed] [threads]\n");
	exit(2);
}

int
main(int argc, char **argv)
{
	struct worker wi = { 0, 0, 0, 0, "ioctl" }, wb = { 0, 0, 0, 0, "blob" };
	struct canary can;
	char why[160];
	const char *e;
	uint64_t seed, gen = 0, canary_runs = 0;
	int64_t t0, end, pause_until = 0, next_canary = 0, next_status = 0;
	int want_ioctl, want_blob, nthreads = 6, ri, ri_last, hang_ms,
	    hang_wait, pace = 610, gen_s = 60, d, st, i, j;
	int64_t lost_since = 0;
	int64_t base_deferred, base_grants, base_vram;
	pid_t pid;

	setvbuf(stdout, NULL, _IOLBF, 0);
	if (argc < 3)
		usage();
	want_ioctl = strcmp(argv[1], "ioctl") == 0 || strcmp(argv[1], "all") == 0;
	want_blob = strcmp(argv[1], "blob") == 0 || strcmp(argv[1], "all") == 0;
	if (!want_ioctl && !want_blob)
		usage();
	seed = argc > 3 ? strtoull(argv[3], NULL, 0) : (uint64_t)time(NULL);
	if (seed == 0)
		seed = 1;
	if (argc > 4)
		nthreads = MAX(1, MIN(atoi(argv[4]), MAXT - 1));
	if ((e = getenv("SI_FUZZ_PACE")) != NULL)
		pace = atoi(e);
	if ((e = getenv("SI_FUZZ_NOCHECK")) != NULL && *e == '1')
		nocheck = 1;
	if ((e = getenv("SI_FUZZ_GEN")) != NULL)
		gen_s = MAX(5, atoi(e));
	pgsz = (size_t)getpagesize();
	signal(SIGPIPE, SIG_IGN);
	signal(SIGTERM, on_term);	/* end early, with the end block */
	signal(SIGINT, on_term);
	signal(SIGSEGV, on_fault);
	signal(SIGBUS, on_fault);

	S = mmap(NULL, sizeof(*S), PROT_READ | PROT_WRITE,
	    MAP_ANON | MAP_SHARED, -1, 0);
	/* Guard pages on both sides: a copyout runs into one, not into
	 * whatever is mapped next. */
	flap = mmap(NULL, (FLAP_PAGES + 2) * pgsz, PROT_READ | PROT_WRITE,
	    MAP_ANON | MAP_PRIVATE, -1, 0);
	if (flap != MAP_FAILED) {
		mprotect(flap, pgsz, PROT_NONE);
		mprotect(flap + (FLAP_PAGES + 1) * pgsz, pgsz, PROT_NONE);
		flap += pgsz;
	}
	ro = mmap(NULL, pgsz, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE,
	    -1, 0);
	if (S == MAP_FAILED || flap == MAP_FAILED || ro == MAP_FAILED)
		return (2);
	memset(ro, 0x5a, pgsz);
	mprotect(ro, pgsz, PROT_READ);

	fd = open(DEV, O_RDWR);
	if (fd < 0) {
		printf("FAIL: open %s: %s\n", DEV, strerror(errno));
		return (1);
	}
	if (dup2(fd, MASTER_FD) < 0)
		return (2);
	close(fd);
	fd = MASTER_FD;
	fuzz_uid_root = getuid() == 0;
	if (!fuzz_uid_root) {
		printf("si-fuzz: run as root (workers drop to throwaway uids)\n");
		return (2);
	}

	hang_ms = (int)iso("hang_ms");
	hang_wait = (hang_ms > 0 ? hang_ms : 5000) * 3 + 5000;
	ri_last = reinits();
	base_deferred = iso("deferred_now");
	base_grants = iso("grants_live");
	base_vram = vram_stat("used");
	printf("si-fuzz: %s for %s s, seed %#jx, %d ioctl threads, hang_ms %d "
	    "(job wait %d ms), re-inits %d, pace %d s, generations %d s\n",
	    argv[1], argv[2], (uintmax_t)seed, nthreads, hang_ms, hang_wait,
	    ri_last, pace, gen_s);

	job_init(&can.j);
	can.seal = -1;
	can.bad = 0;
	if (nocheck)
		printf("si-fuzz: SI_FUZZ_NOCHECK: no canary, no clean-run "
		    "checks\n");
	else if (canary_open(&can) != 0 || canary_run(&can, hang_wait, why,
	    sizeof(why)) != 0) {
		printf("FAIL: the canary does not run before the fuzz: %s\n",
		    why);
		return (1);
	}

	/* The driver allows 3 re-inits per 600 s from the first: one left
	 * over from before (si-client-test 14 spends 2) would leave the fuzz
	 * a single one (run 7a1464ba: the cap after 16 s). Start once the
	 * window is over. */
	if (ri_last > 0 && pace > 0) {
		printf("si-fuzz: %d re-init(s) before the start: waiting %d s "
		    "for the driver's window to close\n", ri_last, pace);
		for (i = 0; i < pace && !stop_now; i++)
			sleep(1);
	}

	t0 = now_ms();
	end = t0 + 1000LL * strtoll(argv[2], NULL, 10);
	while (now_ms() < end && !stop_now) {
		int64_t now = now_ms();

		/* A GPU on which no new job can be set up any more (run
		 * 78169ef0: the GFXHUB stopped acking flushes; the canary's
		 * old VM kept running while every new one failed). */
		if (S->setup_fails >= 10) {
			finding("10 blob job setups timed out with no success "
			    "between: the GPU takes no new work");
			dump_last();
			break;
		}

		/* Reap workers that ended on their own. */
		while ((pid = waitpid(-1, &st, WNOHANG)) > 0) {
			if (pid == wi.pid)
				worker_reaped(&wi, st);
			else if (pid == wb.pid)
				worker_reaped(&wb, st);
		}

		/* A whole-GPU re-init: stop everything, pace, start over. */
		/* "lost" is also the state during every re-init (run
		 * 7a1464ba misread it as the cap): only lost for 90 s is. */
		ri = reinits();
		if (ri == -2) {
			/* Stop the workers as soon as the card is lost, not
			 * when the re-init is done: run 64f44616's blob worker
			 * put its next hostile job on the fresh card before the
			 * supervisor saw the count move, so every re-init came
			 * with a second one ~17 s later. */
			if (lost_since == 0) {
				lost_since = now;
				worker_stop(&wi);
				worker_stop(&wb);
				canary_close(&can);
				can.bad = 0;
			}
			if (now - lost_since > 90000) {
				finding("the driver reports the GPU lost for "
				    "90 s (re-init cap or a failed re-init)");
				dump_last();
				break;
			}
			usleep(200000);
			continue;
		}
		lost_since = 0;
		if (ri > ri_last && ri_last >= 0) {
			printf("si-fuzz: re-init %d -> %d; workers stopped for "
			    "%d s\n", ri_last, ri, pace);
			dump_last();
			worker_stop(&wi);
			worker_stop(&wb);
			pause_until = now + 1000LL * pace;
			S->setup_fails = 0;
			canary_close(&can);
			can.bad = 0;
		}
		ri_last = ri;

		/* Generations: fresh uids, fresh seeds. */
		if (wi.pid > 0 && now >= wi.until)
			worker_stop(&wi);
		if (wb.pid > 0 && now >= wb.until)
			worker_stop(&wb);
		if (now >= pause_until) {
			if (want_ioctl && wi.pid == 0) {
				gen++;
				for (i = 0; i <= MAXT; i++) {
					S->t[i].stuck_told = 0;
					S->t[i].since_ms = 0;
				}
				wi.uid = 61000 + (uid_t)(gen % 1000);
				printf("si-fuzz: ioctl generation %ju: uid %u, "
				    "seed %#jx\n", (uintmax_t)gen,
				    (u_int)wi.uid, (uintmax_t)(seed + gen));
				wi.started = now;
				wi.until = now + 1000LL * gen_s;
				if ((wi.pid = fork()) == 0) {
					ioctl_worker(seed + gen, nthreads,
					    wi.uid);
					_exit(0);
				}
				if (wi.pid < 0)
					wi.pid = 0;
			}
			if (want_blob && wb.pid == 0) {
				gen++;
				wb.uid = 62000 + (uid_t)(gen % 1000);
				printf("si-fuzz: blob generation %ju: uid %u, "
				    "seed %#jx\n", (uintmax_t)gen,
				    (u_int)wb.uid, (uintmax_t)(seed + gen));
				wb.started = now;
				wb.until = now + 2000LL * gen_s;
				if ((wb.pid = fork()) == 0) {
					blob_worker(seed + gen, wb.uid,
					    hang_wait);
					_exit(0);
				}
				if (wb.pid < 0)
					wb.pid = 0;
			}
		}

		/* A thread that has been in the kernel too long. */
		for (i = 0; i <= MAXT; i++) {
			int64_t s = S->t[i].since_ms;

			d = i == MAXT ? hang_wait * 2 + 30000 : 60000;
			if (s != 0 && now - s > d && !S->t[i].stuck_told &&
			    (i == MAXT ? wb.pid : wi.pid) > 0) {
				S->t[i].stuck_told = 1;
				finding("slot %d in one call for %jd s: %s", i,
				    (intmax_t)((now - s) / 1000), S->t[i].last);
			}
		}

		/* The canary. */
		if (!nocheck && now >= next_canary) {
			next_canary = now + 5000;
			if (can.j.vm < 0 && canary_open(&can) != 0) {
				canary_close(&can);
				if (reinits() == ri_last && ++can.bad >= 3)
					finding("the canary VM cannot be "
					    "re-created: %s", strerror(errno));
			} else if (can.j.vm >= 0) {
				int r0 = reinits();

				canary_runs++;
				if (canary_run(&can, hang_wait, why,
				    sizeof(why)) == 0)
					can.bad = 0;
				else if (!no_reinit_since(r0)) {
					canary_close(&can);	/* re-init */
				} else if (++can.bad >= 2) {
					finding("canary: %s", why);
					dump_last();
					canary_close(&can);
					can.bad = 0;
				} else {
					/* Its queue may be lost now (the hang
					 * check): the next run is on a new
					 * one, so only two in a row count. */
					printf("si-fuzz: canary slow once: "
					    "%s\n", why);
					canary_close(&can);
				}
			}
		}

		if (now >= next_status) {
			next_status = now + 60000;
			status(t0, ri_last, canary_runs);
		}
		usleep(200000);
	}

	if (stop_now)
		printf("si-fuzz: stopped by a signal at %jd s\n",
		    (intmax_t)((now_ms() - t0) / 1000));
	/* The end: stop, let the driver settle, then the health check. */
	worker_stop(&wi);
	worker_stop(&wb);
	status(t0, ri_last, canary_runs);
	canary_close(&can);
	sleep(3);
	for (i = 0; i < 100 && iso("deferred_now") > base_deferred; i++)
		usleep(200000);
	for (i = 0; i < 450 && (ri = reinits()) == -2; i++)
		usleep(200000);	/* a re-init in progress: up to 90 s */
	printf("si-fuzz: re-inits at the end %d\n", ri);
	if (ri == -2)
		finding("the GPU is lost at the end");
	if (iso("deferred_now") > base_deferred)
		finding("deferred BO frees left: %jd (start %jd)",
		    (intmax_t)iso("deferred_now"), (intmax_t)base_deferred);
	if (iso("grants_live") > base_grants)
		finding("doorbell grants left: %jd (start %jd)",
		    (intmax_t)iso("grants_live"), (intmax_t)base_grants);
	for (i = 0; i < 50 && vram_stat("used") > base_vram; i++)
		usleep(200000);
	if (vram_stat("used") > base_vram)
		finding("VRAM still allocated: %jd bytes (start %jd)",
		    (intmax_t)vram_stat("used"), (intmax_t)base_vram);
	{
		struct canary h;

		job_init(&h.j);
		h.seal = -1;
		h.bad = 0;
		if (canary_open(&h) != 0)
			finding("health: no new VM after the fuzz: %s",
			    strerror(errno));
		else if (nocheck)
			printf("si-fuzz: health: a new VM and queue open\n");
		else if (canary_run(&h, hang_wait, why, sizeof(why)) != 0)
			finding("health: a new VM does not run clean: %s", why);
		else
			printf("si-fuzz: health: a new VM runs write_42 clean\n");
		canary_close(&h);
	}

	printf("si-fuzz: errno per ioctl (calls by the ioctl workers)\n");
	printf("  %-15s", "");
	for (j = 0; j < E_N; j++)
		printf(" %10s", ename[j]);
	printf("\n");
	for (i = 0; i < NCMD; i++) {
		printf("  %-15s", cmds[i].name);
		for (j = 0; j < E_N; j++)
			printf(" %10ju", (uintmax_t)S->hist[i][j]);
		printf("\n");
	}
	printf("si-fuzz: mmaps that succeeded %ju, forks %ju, blob VM reopens "
	    "%ju, queue renews %ju\n", (uintmax_t)S->mmaps_ok,
	    (uintmax_t)S->forks, (uintmax_t)S->blob_reopen,
	    (uintmax_t)S->blob_renew);
	d = MIN((int)S->nfind, NFIND);
	for (i = 0; i < d; i++)
		printf("FINDING %d: %s\n", i + 1, S->find[i]);
	printf("RESULT: %s (%d findings, seed %#jx)\n", S->nfind == 0 ?
	    "PASS" : "FAIL", (int)S->nfind, (uintmax_t)seed);
	return (S->nfind == 0 ? 0 : 1);
}
