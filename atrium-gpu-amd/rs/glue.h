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

/* Rust, for C (rs/ioctl.rs): the cdevsw entry points. */
d_open_t	amd_rs_open;
d_ioctl_t	amd_rs_ioctl;
d_mmap_single_t	amd_rs_mmap_single;
d_ioctl_t	amd_rs_ctl_ioctl;

/*
 * Rust, for C (rs/doorbell.rs): doorbell grants, called by queue.c with the
 * queue's lock held. create gives a reference to a new managed device-pager
 * object; revoke takes the page back from every mapping and consumes that
 * reference.
 */
int	amd_db_grant_create(struct atrium_amd_softc *sc, uint64_t pa, int nprot,
	    vm_object_t *out);
void	amd_db_grant_revoke(vm_object_t obj);

/* Rust, for C (rs/defer.rs): the front-end's per-device state, and the
 * deferred BO-free pass (returns 1 while BOs still wait). */
void	*amd_rs_dev_init(void);
void	amd_rs_dev_fini(void *state);
int	amd_rs_reap(struct atrium_amd_softc *sc);

#endif
