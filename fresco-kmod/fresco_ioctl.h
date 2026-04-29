/*
 * Fresco transport — userspace/kernel ioctl interface.
 * Shared between fresco.ko and libfresco.
 */

#ifndef _FRESCO_IOCTL_H_
#define _FRESCO_IOCTL_H_

#include <sys/ioccom.h>
#include <sys/types.h>

/*
 * mmap(/dev/fresco0, ..., offset=0) maps the full ivshmem BAR2
 * shared-memory region (16 MiB) read-write, normal cacheable.
 *
 * Userspace then operates on the rings and CAS staging area directly,
 * matching the layout in karythra-gpu-server/src/platform/ivshmem.rs
 * (the reference server implementation of the Fresco protocol).
 */

/*
 * Ring the doorbell to peer 0 (the Fresco server) on the given vector.
 * Vector 0 is the standard "completion notify" vector. Argument is
 * the vector number (uint16_t).
 *
 * Implemented as a 32-bit MMIO write to BAR0+0x0c with value
 *     (peer_id << 16) | vector
 * peer_id is hard-wired to 0 (server) since this is a 2-peer setup.
 */
#define FRESCO_IOC_DOORBELL    _IOW('F', 1, uint16_t)

/*
 * Read the device's IVPosition (peer ID assigned by ivshmem-server).
 * Useful for debugging — the guest is always peer 1.
 */
#define FRESCO_IOC_IVPOSITION  _IOR('F', 2, uint32_t)

/*
 * Read total number of poll-detected ring updates since attach (i.e.
 * how many times the kernel callout observed comp_write or input_write
 * advance). For kqueue-driven event loops, EVFILT_READ on the cdev
 * wakes on each such update; this counter is for diagnostics.
 */
#define FRESCO_IOC_WAKE_COUNT  _IOR('F', 3, uint64_t)

/*
 * Returns the per-open client slot index assigned by the kmod when
 * /dev/fresco0 was opened. Slot indexes are 0..FRESCO_NUM_CLIENT_SLOTS-1
 * and are released back to the bitmap on close. libfresco uses this
 * to compute the offsets of its private cmd/comp rings within shmem.
 */
#define FRESCO_IOC_CLIENT_ID   _IOR('F', 4, uint32_t)

#endif /* _FRESCO_IOCTL_H_ */
