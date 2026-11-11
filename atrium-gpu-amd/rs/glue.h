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
struct atrium_amd_softc *amd_rs_cdev_softc(struct cdev *cdev);

/* Rust, for C (rs/ioctl.rs): the cdevsw entry points. */
d_open_t	amd_rs_open;
d_ioctl_t	amd_rs_ioctl;
d_mmap_single_t	amd_rs_mmap_single;
d_ioctl_t	amd_rs_ctl_ioctl;

#endif
