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

/* C, for Rust (queue.c): queue registers and the completions a queue owes. */
enum amd_rs_q_count { AMD_RS_Q_REFUSED, AMD_RS_Q_WAIT, AMD_RS_Q_REVOKE };
uint32_t amd_rs_q_rptr(struct atrium_amd_softc *sc, uint32_t qid);
int	amd_rs_q_owes(struct atrium_amd_softc *sc, uint32_t qid);
int	amd_rs_q_room(struct atrium_amd_softc *sc, uint32_t qid);
void	amd_rs_q_started(struct atrium_amd_softc *sc, uint32_t qid,
	    struct atrium_amd_syncobj *so, uint64_t value);
void	amd_rs_q_stopped(struct atrium_amd_softc *sc, uint32_t qid, int was_busy);
int	amd_rs_q_hang(struct atrium_amd_softc *sc, uint32_t qid, uint16_t vmid,
	    uint32_t rptr);
void	amd_rs_q_count(struct atrium_amd_softc *sc, int what);
void	amd_rs_q_deadline(struct atrium_amd_softc *sc, uint32_t ns);
void	amd_rs_q_doorbell(struct atrium_amd_softc *sc, uint32_t off, uint32_t wptr);
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

#endif
