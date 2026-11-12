/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * hospes_trap.c -- syscall traps for Hospes, the C half of hospes_trap.ko
 * (docs/spec/hospes.md §13 R1, step K1).
 *
 * A Hospes host process names the text of a foreign library it has mapped.
 * System calls made from that text are Linux calls (Widevine issues openat,
 * read, close and rt_sigaction directly), so they must not reach FreeBSD's
 * table: the kernel's hook (<sys/systrap.h>, one branch in syscallenter())
 * asks this module, and a call from inside a registered range runs nothing
 * and raises a synchronous SIGSYS. The host's handler emulates the call from
 * the saved registers and writes the result into them.
 *
 * Here: the dynamic system call that registers ranges, the hook, and the
 * table's lifetime (freed at exec and exit; a forked child starts without
 * one: fork clears p_systrap). The table and its
 * checks are Rust (hospes_trap.rs).
 *
 *	hospes_trap(HOSPES_TRAP_ADD, start, len)	add a range
 *	hospes_trap(HOSPES_TRAP_LOCK, 0, 0)		no more ranges
 *	hospes_trap(HOSPES_TRAP_COUNT, 0, 0)		how many (td_retval)
 *
 * Only for the calling process, and allowed in capability mode: the host
 * enters capability mode before it parses the library. The syscall number
 * is dynamic; user space finds it with modfind("sys/hospes_trap") and
 * modstat's data.intval.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/eventhandler.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/module.h>
#include <sys/mutex.h>
#include <sys/proc.h>
#include <sys/signalvar.h>
#include <sys/sysent.h>
#include <sys/sysproto.h>
#include <sys/systrap.h>

#include <vm/vm.h>
#include <vm/pmap.h>

#include <machine/atomic.h>
#include <machine/cpu.h>
#include <machine/frame.h>
#include <machine/vmparam.h>

#define	HOSPES_TRAP_ADD		1
#define	HOSPES_TRAP_LOCK	2
#define	HOSPES_TRAP_COUNT	3

/* hospes_trap.rs; struct hospes_trap_table is Rust's `Table`. */
struct hospes_trap_table;
size_t	hospes_trap_table_size(void);
bool	hospes_trap_hit(const struct hospes_trap_table *t, uintptr_t pc);
int	hospes_trap_add(const struct hospes_trap_table *t, uintptr_t start,
	    size_t len, uintptr_t user_max);
void	hospes_trap_lock(const struct hospes_trap_table *t);
uint32_t hospes_trap_count(const struct hospes_trap_table *t);

static MALLOC_DEFINE(M_HOSPES_TRAP, "hospes_trap", "Hospes syscall trap ranges");

/*
 * Tables alive: the module cannot unload while any process has one.  The
 * mutex orders a new table against an unload (taken before the process
 * lock); the kernel also fails closed if a table outlives the hook.
 */
static u_int hospes_trap_live;
static struct mtx hospes_trap_mtx;
MTX_SYSINIT(hospes_trap, &hospes_trap_mtx, "hospes_trap", MTX_DEF);
static eventhandler_tag hospes_trap_exec_tag, hospes_trap_exit_tag;

/* The Linux system call number, for si_syscall (the handler reads the
 * registers anyway). */
static register_t
hospes_trap_nr(struct trapframe *tf)
{
#if defined(__amd64__)
	return (tf->tf_rax);
#elif defined(__aarch64__)
	return (tf->tf_x[8]);
#else
#error "hospes_trap: amd64 and arm64 only"
#endif
}

static bool
hospes_trap_hook(struct thread *td)
{
	struct hospes_trap_table *t;
	ksiginfo_t ksi;
	uintptr_t pc;

	t = (void *)atomic_load_acq_ptr((uintptr_t *)&td->td_proc->p_systrap);
	if (t == NULL)
		return (false);
	pc = TRAPF_PC(td->td_frame);
	if (!hospes_trap_hit(t, pc))
		return (false);
	/*
	 * Synchronous, like a fault: a blocked or ignored SIGSYS kills the
	 * process instead of letting the call fall through.
	 */
	ksiginfo_init_trap(&ksi);
	ksi.ksi_signo = SIGSYS;
	ksi.ksi_code = SYS_TRAPPED;
	ksi.ksi_addr = (void *)pc;
	ksi.ksi_info.si_syscall = (int)hospes_trap_nr(td->td_frame);
	trapsignal(td, &ksi);
	return (true);
}

static void
hospes_trap_free(struct proc *p)
{
	void *t;

	t = p->p_systrap;
	if (t == NULL)
		return;
	atomic_store_rel_ptr((uintptr_t *)&p->p_systrap, 0);
	free(t, M_HOSPES_TRAP);
	mtx_lock(&hospes_trap_mtx);
	hospes_trap_live--;
	mtx_unlock(&hospes_trap_mtx);
}

/* Exec and exit run with the process single-threaded: nothing else reads
 * the table. */
static void
hospes_trap_exec(void *arg __unused, struct proc *p,
    struct image_params *imgp __unused)
{
	hospes_trap_free(p);
}

static void
hospes_trap_exit(void *arg __unused, struct proc *p)
{
	hospes_trap_free(p);
}

struct hospes_trap_args {
	register_t	op;
	register_t	start;
	register_t	len;
};

static int
sys_hospes_trap(struct thread *td, void *uap_)
{
	struct hospes_trap_args *uap = uap_;
	struct proc *p = td->td_proc;
	struct hospes_trap_table *t, *fresh;
	int error;

	switch (uap->op) {
	case HOSPES_TRAP_ADD:
		fresh = NULL;
		if (p->p_systrap == NULL)
			fresh = malloc(hospes_trap_table_size(), M_HOSPES_TRAP,
			    M_WAITOK | M_ZERO);
		mtx_lock(&hospes_trap_mtx);
		PROC_LOCK(p);
		t = p->p_systrap;
		if (t == NULL) {
			if (fresh == NULL) {
				/* Freed by exec in between: not possible for
				 * the calling thread, but be exact. */
				PROC_UNLOCK(p);
				mtx_unlock(&hospes_trap_mtx);
				return (EAGAIN);
			}
			t = fresh;
			fresh = NULL;
		}
		error = hospes_trap_add(t, (uintptr_t)uap->start,
		    (size_t)uap->len, VM_MAXUSER_ADDRESS);
		if (error == 0 && t != p->p_systrap) {
			hospes_trap_live++;
			/* Published after its first range is in. */
			atomic_store_rel_ptr((uintptr_t *)&p->p_systrap,
			    (uintptr_t)t);
		} else if (t != p->p_systrap) {
			fresh = t;
		}
		PROC_UNLOCK(p);
		mtx_unlock(&hospes_trap_mtx);
		if (fresh != NULL)
			free(fresh, M_HOSPES_TRAP);
		return (error);
	case HOSPES_TRAP_LOCK:
		PROC_LOCK(p);
		if (p->p_systrap != NULL)
			hospes_trap_lock(p->p_systrap);
		PROC_UNLOCK(p);
		return (0);
	case HOSPES_TRAP_COUNT:
		t = (void *)atomic_load_acq_ptr((uintptr_t *)&p->p_systrap);
		td->td_retval[0] = t == NULL ? 0 : hospes_trap_count(t);
		return (0);
	default:
		return (EINVAL);
	}
}

static struct sysent hospes_trap_sysent = {
	.sy_narg = 3,
	.sy_call = sys_hospes_trap,
	.sy_flags = SYF_CAPENABLED,
};

static int hospes_trap_offset = NO_SYSCALL;

static int
hospes_trap_modevent(struct module *m __unused, int what, void *arg __unused)
{
	switch (what) {
	case MOD_LOAD:
		hospes_trap_exec_tag = EVENTHANDLER_REGISTER(process_exec,
		    hospes_trap_exec, NULL, EVENTHANDLER_PRI_ANY);
		hospes_trap_exit_tag = EVENTHANDLER_REGISTER(process_exit,
		    hospes_trap_exit, NULL, EVENTHANDLER_PRI_ANY);
		atomic_store_rel_ptr((uintptr_t *)&systrap_hook,
		    (uintptr_t)hospes_trap_hook);
		printf("hospes_trap: syscall %d\n", hospes_trap_offset);
		return (0);
	case MOD_UNLOAD:
		mtx_lock(&hospes_trap_mtx);
		if (hospes_trap_live != 0) {
			mtx_unlock(&hospes_trap_mtx);
			return (EBUSY);
		}
		atomic_store_rel_ptr((uintptr_t *)&systrap_hook, 0);
		mtx_unlock(&hospes_trap_mtx);
		EVENTHANDLER_DEREGISTER(process_exec, hospes_trap_exec_tag);
		EVENTHANDLER_DEREGISTER(process_exit, hospes_trap_exit_tag);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

SYSCALL_MODULE(hospes_trap, &hospes_trap_offset, &hospes_trap_sysent,
    hospes_trap_modevent, NULL);
