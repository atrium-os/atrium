/*
 * rs/glue.h — functions across the C/Rust boundary of atrium_gpu_amd.ko
 * (rs/README.md). Seen by both sides: glue.c includes it, and so does
 * rs/sys.h, the header bindgen turns into the Rust declarations.
 */
#ifndef _ATRIUM_GPU_AMD_RS_GLUE_H_
#define _ATRIUM_GPU_AMD_RS_GLUE_H_

/* C, for Rust (glue.c). */
const struct atrium_gpu_backend_ops *amd_rs_backend(struct atrium_amd_softc *sc);
void	amd_rs_irqs(struct atrium_amd_softc *sc, uint64_t *count, uint32_t *msix);
void	amd_rs_syncobj_count(struct atrium_amd_softc *sc, int delta);
void	amd_rs_grant_count(struct atrium_amd_softc *sc, int live, int created);
void	*amd_rs_state(struct atrium_amd_softc *sc);
void	amd_rs_defer_count(struct atrium_amd_softc *sc, int now, int reaped);
struct atrium_amd_softc *amd_rs_cdev_softc(struct cdev *cdev);

/* C, for Rust (glue.c): per-uid BO accounting (rs/quota.rs). The calling
 * thread's real uid, the cap in bytes for VRAM (vram != 0) or System BOs, and
 * the refusal counter. */
uint32_t amd_rs_cur_ruid(void);
uint64_t amd_rs_quota_limit(struct atrium_amd_softc *sc, int vram);
void	amd_rs_quota_refused(struct atrium_amd_softc *sc);

/* Rust, for C (rs/ioctl.rs): the cdevsw entry points. */
d_open_t	amd_rs_open;
d_ioctl_t	amd_rs_ioctl;
d_mmap_single_t	amd_rs_mmap_single;
d_ioctl_t	amd_rs_ctl_ioctl;

/* C, for Rust (queue.c): queue registers, counters, and sc->lock, which
 * guards the completions each queue owes (rs/pending.rs). The _locked ones
 * need sc->lock held. */
enum amd_rs_q_count { AMD_RS_Q_REFUSED, AMD_RS_Q_WAIT, AMD_RS_Q_REVOKE };
enum amd_rs_cnt { AMD_RS_CNT_FENCES_FAILED, AMD_RS_CNT_DEQUEUES, AMD_RS_CNT_HANGS };
enum amd_rs_stop { AMD_RS_STOP_FAULT, AMD_RS_STOP_HANG };
uint32_t amd_rs_q_rptr(struct atrium_amd_softc *sc, uint32_t qid);
void	amd_rs_q_count(struct atrium_amd_softc *sc, int what);
void	amd_rs_sc_lock(struct atrium_amd_softc *sc);
void	amd_rs_sc_unlock(struct atrium_amd_softc *sc);
void	amd_rs_sc_assert(struct atrium_amd_softc *sc);
int	amd_rs_ticks(void);
int	amd_rs_hang_ticks_locked(struct atrium_amd_softc *sc);
void	amd_rs_count_locked(struct atrium_amd_softc *sc, int what, uint32_t n);
void	amd_rs_q_report_stop(struct atrium_amd_softc *sc, uint32_t qid,
	    uint16_t vmid, int what);
void	amd_rs_q_deadline(struct atrium_amd_softc *sc, uint32_t ns);
void	amd_rs_q_doorbell(struct atrium_amd_softc *sc, uint32_t off, uint32_t wptr,
	    int signalled);
uint64_t amd_rs_doorbell_pa(struct atrium_amd_softc *sc);
void	amd_rs_vm_null_pd(struct atrium_amd_softc *sc, uint16_t vmid);
void	amd_rs_watchdog_arm(struct atrium_amd_softc *sc);

/* Rust, for C (rs/queue.rs): the watchdog pass (1 while a kernel-rung queue
 * is busy) and detach's stop-everything. */
int	amd_rs_watchdog(struct atrium_amd_softc *sc);
void	amd_rs_queues_stop(struct atrium_amd_softc *sc);

/* Rust, for C (rs/defer.rs): the front-end's per-device state, and the
 * deferred BO-free pass (returns 1 while BOs still wait). */
void	*amd_rs_dev_init(void);
void	amd_rs_dev_fini(void *state);
int	amd_rs_reap(struct atrium_amd_softc *sc);

/* Rust, for C (rs/pending.rs): the EOP and VM-fault cause handlers' work,
 * with sc->lock held by the base ISR. */
void	amd_rs_eop(struct atrium_amd_softc *sc, const uint32_t *eop,
	    const uint32_t *fault);
void	amd_rs_q_faulted_locked(struct atrium_amd_softc *sc, uint32_t qid);

/* C, for Rust (rs/uqueue.rs; si_client.c): client queue fds on MES user
 * queues (5c-2b). EOPNOTSUPP/0 on a backend without them. */
int	amd_si_uq_create(struct atrium_amd_softc *sc, struct atrium_amd_vm *vm,
	    uint32_t prio, uint8_t *upg, uint64_t upa, int *slot);
uint64_t amd_si_uq_doorbell_pa(struct atrium_amd_softc *sc, int slot);
uint64_t amd_si_uq_fence_va(struct atrium_amd_softc *sc, int slot);
int	amd_si_uq_alive(struct atrium_amd_softc *sc, int slot);
uint32_t amd_si_uq_preempt(struct atrium_amd_softc *sc, uint32_t mask);

/* C, for Rust (glue.c): the user-mode queue gate, and a queue fd's mmap. */
int	amd_rs_priv_driver(void);
int	amd_rs_is_si(struct atrium_amd_softc *sc);
int	amd_rs_mmap_object(vm_map_t map, vm_offset_t *addr, vm_size_t size,
	    vm_prot_t prot, vm_prot_t cap_maxprot, int flags, vm_object_t obj,
	    vm_ooffset_t foff, struct thread *td);
int	amd_si_uq_destroy(struct atrium_amd_softc *sc, int slot);
int	amd_si_uq_submit(struct atrium_amd_softc *sc, int slot, uint64_t ib,
	    uint32_t ib_dw, struct atrium_amd_syncobj *so, uint64_t value,
	    uint64_t *seqno);
void	amd_si_uq_scrub(struct atrium_amd_softc *sc,
	    struct atrium_amd_syncobj *so);
uint32_t amd_si_uq_vmids(struct atrium_amd_softc *sc);

#endif
