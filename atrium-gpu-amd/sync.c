/*
 * sync.c — timeline syncobj: a monotonic 64-bit counter exposed as an fd.
 *
 * A submission signals the syncobj on completion (ABI-v2 §5.6, §6). Userspace
 * waits either blocking (SYNCOBJ_WAIT) or, the BSD-native way, via kqueue: the
 * fd is EVFILT_READ-able with the wait threshold passed in the kevent `data`
 * field, so a frescod compositor folds "GPU frame done" into the same
 * kevent() as its socket and input fds. The kqfilter is modeled on the
 * in-tree eventfd (sys/kern/sys_eventfd.c).
 */
#include "atrium_gpu_amd.h"

static fo_kqfilter_t	amd_syncobj_kqfilter;
static fo_close_t	amd_syncobj_close;

static int
amd_syncobj_stat(struct file *fp, struct stat *sb, struct ucred *active_cred)
{
	bzero(sb, sizeof(*sb));
	sb->st_mode = S_IFCHR;
	return (0);
}

static int
amd_syncobj_fill_kinfo(struct file *fp, struct kinfo_file *kif,
    struct filedesc *fdp)
{
	kif->kf_type = KF_TYPE_DEV;
	return (0);
}

const struct fileops atrium_amd_syncobj_fileops = {
	.fo_read = invfo_rdwr,
	.fo_write = invfo_rdwr,
	.fo_truncate = invfo_truncate,
	.fo_ioctl = invfo_ioctl,
	.fo_poll = invfo_poll,
	.fo_kqfilter = amd_syncobj_kqfilter,
	.fo_stat = amd_syncobj_stat,
	.fo_close = amd_syncobj_close,
	.fo_chmod = invfo_chmod,
	.fo_chown = invfo_chown,
	.fo_sendfile = invfo_sendfile,
	.fo_fill_kinfo = amd_syncobj_fill_kinfo,
	.fo_cmp = file_kcmp_generic,
	.fo_flags = DFLAG_PASSABLE,
};

/* --- kqueue filter: fire when value >= the threshold the knote carries --- */

static void
amd_syncobj_filt_detach(struct knote *kn)
{
	struct atrium_amd_syncobj *so = kn->kn_hook;

	mtx_lock(&so->lock);
	knlist_remove(&so->sel.si_note, kn, 1);
	mtx_unlock(&so->lock);
}

static int
amd_syncobj_filt_event(struct knote *kn, long hint)
{
	struct atrium_amd_syncobj *so = kn->kn_hook;

	mtx_assert(&so->lock, MA_OWNED);
	kn->kn_data = (int64_t)so->value;
	/* kn_sdata is the threshold from the kevent `data` field. */
	if (so->value >= (uint64_t)kn->kn_sdata)
		return (1);
	if (so->error != 0) {
		/* The work that would have reached the threshold is gone. */
		kn->kn_flags |= EV_EOF;
		kn->kn_fflags = so->error;
		return (1);
	}
	return (0);
}

static const struct filterops amd_syncobj_filtops = {
	.f_isfd = 1,
	.f_detach = amd_syncobj_filt_detach,
	.f_event = amd_syncobj_filt_event,
	.f_copy = knote_triv_copy,
};

static int
amd_syncobj_kqfilter(struct file *fp, struct knote *kn)
{
	struct atrium_amd_syncobj *so = fp->f_data;

	if (kn->kn_filter != EVFILT_READ)
		return (EINVAL);
	mtx_lock(&so->lock);
	kn->kn_fop = &amd_syncobj_filtops;
	kn->kn_hook = so;
	knlist_add(&so->sel.si_note, kn, 1);
	mtx_unlock(&so->lock);
	return (0);
}

/* --- lifecycle + signal --- */

static int
amd_syncobj_close(struct file *fp, struct thread *td)
{
	struct atrium_amd_syncobj *so = fp->f_data;

	if (so == NULL)
		return (0);
	fp->f_data = NULL;
	/*
	 * Drop any completion the ISR still owes this syncobj before we free it.
	 * The scrub takes sc->lock, which the ISR also holds while it signals, so
	 * once this returns the ISR can no longer reference `so`.
	 */
	if (so->sc != NULL)
		amd_pending_scrub(so->sc, so);
	seldrain(&so->sel);
	knlist_destroy(&so->sel.si_note);
	mtx_destroy(&so->lock);
	/* Last touch of the softc: detach waits for this count to reach 0. */
	if (so->sc != NULL)
		atomic_subtract_int(&so->sc->syncobj_count, 1);
	free(so, M_DEVBUF);
	return (0);
}

/*
 * Advance the timeline to `value` (monotonic — never moves backward) and wake
 * both blocking waiters (SYNCOBJ_WAIT, on the &value channel) and kqueue
 * knotes. Holding the syncobj lock across KNOTE_LOCKED is what the knlist
 * expects (the filter asserts MA_OWNED).
 */
void
amd_syncobj_signal(struct atrium_amd_syncobj *so, uint64_t value)
{
	mtx_lock(&so->lock);
	if (value > so->value)
		so->value = value;
	wakeup(&so->value);
	KNOTE_LOCKED(&so->sel.si_note, 0);
	if (so->watch != NULL)
		so->watch(so->watch_arg);
	mtx_unlock(&so->lock);
}

/*
 * A completion this syncobj was owed will never come: its queue was stopped
 * (VM torn down, hang watchdog, GPU reset). The value stays where it is; the
 * error is sticky, so every waiter for a value not yet reached gets `error`
 * (SYNCOBJ_WAIT returns it; kqueue reports EV_EOF with it in fflags) rather
 * than sleeping until its timeout. Values already reached still succeed.
 */
void
amd_syncobj_fail(struct atrium_amd_syncobj *so, int error)
{
	mtx_lock(&so->lock);
	if (so->error == 0)
		so->error = error;
	wakeup(&so->value);
	KNOTE_LOCKED(&so->sel.si_note, 0);
	if (so->watch != NULL)
		so->watch(so->watch_arg);
	mtx_unlock(&so->lock);
}

/*
 * The display's in-fence watch: it learns of the signal at once instead of
 * at the next vblank. One watcher per syncobj; a later install replaces it.
 */
void
amd_syncobj_watch(struct atrium_amd_syncobj *so, void (*fn)(void *), void *arg)
{
	mtx_lock(&so->lock);
	so->watch = fn;
	so->watch_arg = arg;
	mtx_unlock(&so->lock);
}

int
amd_syncobj_create_fd(struct atrium_amd_softc *sc, struct thread *td,
    int *out_fd)
{
	struct atrium_amd_syncobj *so;
	struct file *fp;
	int fd, err;

	so = malloc(sizeof(*so), M_DEVBUF, M_WAITOK | M_ZERO);
	so->sc = sc;
	mtx_init(&so->lock, "amdsync", NULL, MTX_DEF);
	knlist_init_mtx(&so->sel.si_note, &so->lock);
	so->value = 0;

	err = falloc_noinstall(td, &fp);
	if (err != 0) {
		knlist_destroy(&so->sel.si_note);
		mtx_destroy(&so->lock);
		free(so, M_DEVBUF);
		return (err);
	}
	/* Counted from here: the fo_close that reclaims a failed install drops it. */
	atomic_add_int(&sc->syncobj_count, 1);
	finit(fp, FREAD, DTYPE_DEV, so, &atrium_amd_syncobj_fileops);
	err = finstall(td, fp, &fd, 0, NULL);
	fdrop(fp, td);
	if (err != 0)
		return (err);	/* fo_close already reclaimed it */

	*out_fd = fd;
	return (0);
}

int
amd_syncobj_fget(struct thread *td, int fd, struct file **out_fp,
    struct atrium_amd_syncobj **out_so)
{
	cap_rights_t rights;
	struct file *fp;
	int err;

	err = fget(td, fd, cap_rights_init(&rights), &fp);
	if (err != 0)
		return (err);
	if (fp->f_ops != &atrium_amd_syncobj_fileops) {
		fdrop(fp, td);
		return (EINVAL);
	}
	*out_fp = fp;
	*out_so = fp->f_data;
	return (0);
}
