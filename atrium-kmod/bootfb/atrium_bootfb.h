/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * atrium-bootfb — Atrium pre-GPU framebuffer cdev ABI.
 *
 * Exposes the EFI GOP framebuffer that the bootloader handed to the
 * kernel (`MODINFOMD_EFI_FB` metadata) as a userspace-mappable cdev.
 * Used by `atrium-splash` to draw a boot-time splash before the
 * native GPU driver (atrium-virtio-gpu and friends) takes over the
 * display via `/dev/atrium-display0`. Once the GPU driver does its
 * own SET_SCANOUT, the EFI framebuffer becomes irrelevant and the
 * splash binary exits.
 *
 * Userspace flow:
 *   fd = open("/dev/atrium-bootfb0", O_RDWR);
 *   ioctl(fd, ATRIUM_BOOTFB_IOC_GET_INFO, &info);
 *   px = mmap(NULL, info.size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0);
 *   // write px[stride * y + x * 4 ..] in `format` order; visible
 *   // immediately on the GOP scanout, no flush needed.
 */

#ifndef _ATRIUM_BOOTFB_H_
#define _ATRIUM_BOOTFB_H_

#include <sys/ioccom.h>
#include <sys/types.h>
#ifndef _KERNEL
#include <stdint.h>
#endif

/*
 * Pixel format codes. Derived from `efi_fb`'s mask fields at attach
 * time. Most modern UEFI firmware uses BGRA8 (PixelBlueGreenRedReserved).
 */
#define ATRIUM_BOOTFB_FORMAT_UNKNOWN	0
#define ATRIUM_BOOTFB_FORMAT_BGRA8	1   /* B,G,R,A in memory order */
#define ATRIUM_BOOTFB_FORMAT_RGBA8	2   /* R,G,B,A in memory order */

struct atrium_bootfb_info {
	uint64_t	size;		/* mmap size in bytes (== height*stride, padded) */
	uint32_t	width;		/* pixels */
	uint32_t	height;		/* pixels */
	uint32_t	stride;		/* bytes per row (may exceed width*4) */
	uint32_t	format;		/* ATRIUM_BOOTFB_FORMAT_* */
	uint32_t	mask_red;
	uint32_t	mask_green;
	uint32_t	mask_blue;
	uint32_t	mask_reserved;
	uint32_t	bpp;		/* bits per pixel — 32 today */
	uint32_t	reserved[7];
};

#define ATRIUM_BOOTFB_IOC_GET_INFO	_IOR('A', 0x40, struct atrium_bootfb_info)

#endif /* _ATRIUM_BOOTFB_H_ */
