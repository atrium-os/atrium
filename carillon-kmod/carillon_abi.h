/*
 * Carillon guest kmod — userspace ABI + shared-memory layout.
 *
 * This header is shared between the FreeBSD guest kmod (carillon.c) and
 * userspace clients (the Rust binding the ICD / frescod renderer use).
 * The layout MUST match aqueduct-gpu-host's src/carillon.rs `layout`
 * module byte-for-byte — they map the same BAR2 region.
 *
 * See docs/spec/carillon.md.
 */
#ifndef _CARILLON_ABI_H_
#define _CARILLON_ABI_H_

#include <sys/ioccom.h>
#include <sys/types.h>

/* QEMU ivshmem PCI identity. */
#define CARILLON_PCI_VENDOR   0x1af4
#define CARILLON_PCI_DEVICE   0x1110

/* BAR0 device registers (ivshmem-doorbell). */
#define CARILLON_REG_INTMASK   0x00 /* interrupt mask */
#define CARILLON_REG_INTSTATUS 0x04 /* interrupt status (read clears) */
#define CARILLON_REG_IVPOSITION 0x08 /* this peer's id */
#define CARILLON_REG_DOORBELL  0x0c /* write (peer_id<<16)|vector to ring */

/* The host (aqueduct-gpu-host IvshmemServer) is peer 0, vector 0. */
#define CARILLON_HOST_PEER_ID  0
#define CARILLON_DOORBELL_VECTOR 0

/* Shared-memory layout (BAR2). Mirrors src/carillon.rs::layout. */
#define CARILLON_MAGIC          0x54564741u /* 'AGVT' LE */
#define CARILLON_ABI_VERSION    1u

#define CARILLON_CTRL_OFFSET        0x00000u
#define CARILLON_SUB_RING_OFFSET    0x01000u
#define CARILLON_COMP_RING_OFFSET   0x10000u
#define CARILLON_REGION_TABLE_OFFSET 0x20000u
#define CARILLON_FRAME_ARENA_OFFSET 0x30000u
#define CARILLON_TOTAL_SIZE         0x100000u /* 1 MiB */

#define CARILLON_DESC_SIZE          64u
#define CARILLON_SUB_RING_BYTES     0xF000u
#define CARILLON_COMP_RING_BYTES    0xF000u
#define CARILLON_SUB_ENTRIES        (CARILLON_SUB_RING_BYTES / CARILLON_DESC_SIZE)
#define CARILLON_COMP_ENTRIES       (CARILLON_COMP_RING_BYTES / CARILLON_DESC_SIZE)

/* Control-page field byte offsets (u32 unless noted). */
#define CARILLON_C_MAGIC          0x00
#define CARILLON_C_ABI            0x04
#define CARILLON_C_HOST_STATUS    0x08
#define CARILLON_C_GUEST_STATUS   0x0c
#define CARILLON_C_HOST_PAGE_SIZE 0x10
#define CARILLON_C_SUB_WRITE      0x20
#define CARILLON_C_SUB_READ       0x24
#define CARILLON_C_COMP_WRITE     0x28
#define CARILLON_C_COMP_READ      0x2c
#define CARILLON_C_CAPS           0x40 /* u64 */

/*
 * cdev interface (/dev/carillon0):
 *
 *   mmap(2)  — maps the BAR2 shared region into userspace. The client
 *              drives the rings + frame arena directly (zero-copy), the
 *              same protocol as src/carillon.rs::GuestRing.
 *   ioctl CARILLON_RING — ring the host doorbell (after staging a frame
 *              + advancing sub_write). One BAR0 doorbell write.
 *   ioctl CARILLON_WAIT — park the calling thread on the completion
 *              waitqueue (woken by the MSI-X ISR). No spin. Returns when
 *              a completion doorbell has arrived since the last wait, or
 *              after `timeout_ms` (0 = block forever).
 */
struct carillon_wait {
	uint32_t timeout_ms; /* 0 = block forever */
	uint32_t woke;       /* out: 1 if a doorbell arrived, 0 on timeout */
};

#define CARILLON_RING _IO('C', 1)
#define CARILLON_WAIT _IOWR('C', 2, struct carillon_wait)

#endif /* _CARILLON_ABI_H_ */
