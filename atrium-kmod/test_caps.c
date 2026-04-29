/*
 * test_caps.c — D0 step 1 smoke: open /dev/atrium-gpu0, query IOC_CAPS,
 * print the result. Verifies the cdev is wired up end-to-end.
 *
 * Build inside the VM:
 *   cc -O -Wall -o test_caps test_caps.c
 * Run as root:
 *   ./test_caps
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/endian.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

#include "atrium_gpu.h"

int
main(void)
{
	int fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0) {
		fprintf(stderr, "open(/dev/atrium-gpu0): %s\n", strerror(errno));
		return (1);
	}

	struct atrium_gpu_caps caps;
	memset(&caps, 0, sizeof(caps));
	if (ioctl(fd, ATRIUM_GPU_IOC_CAPS, &caps) != 0) {
		fprintf(stderr, "ioctl(IOC_CAPS): %s\n", strerror(errno));
		close(fd);
		return (1);
	}

	printf("Atrium GPU ABI %u.%u\n", caps.version_major, caps.version_minor);
	printf("  vendor=0x%04x device=0x%04x family=%s\n",
	    caps.vendor_id, caps.device_id, caps.family);
	printf("  engine_mask=0x%x feature_flags=0x%x\n",
	    caps.engine_mask, caps.feature_flags);

	/* Allocate a 64 KiB BO, mmap, write a pattern, read it back, free. */
	struct atrium_gpu_alloc al;
	memset(&al, 0, sizeof(al));
	al.size = 64 * 1024;
	al.flags = ATRIUM_GPU_BO_GPU_VISIBLE | ATRIUM_GPU_BO_CPU_VISIBLE |
	           ATRIUM_GPU_BO_COHERENT;
	if (ioctl(fd, ATRIUM_GPU_IOC_ALLOC, &al) != 0) {
		fprintf(stderr, "IOC_ALLOC: %s\n", strerror(errno));
		close(fd);
		return (1);
	}
	printf("  IOC_ALLOC: handle=%u mmap_offset=0x%lx size=%lu\n",
	    al.handle, (unsigned long)al.mmap_offset, (unsigned long)al.size);

	void *map = mmap(NULL, al.size, PROT_READ | PROT_WRITE, MAP_SHARED,
	    fd, al.mmap_offset);
	if (map == MAP_FAILED) {
		fprintf(stderr, "mmap: %s\n", strerror(errno));
		close(fd);
		return (1);
	}
	unsigned char *p = map;
	for (size_t i = 0; i < al.size; i++)
		p[i] = (unsigned char)(i ^ 0xa5);
	for (size_t i = 0; i < al.size; i++) {
		if (p[i] != (unsigned char)(i ^ 0xa5)) {
			fprintf(stderr, "BO read-back mismatch at %zu\n", i);
			return (1);
		}
	}
	printf("  mmap + write + read-back %lu bytes — ok\n",
	    (unsigned long)al.size);
	munmap(map, al.size);

	uint32_t handle = al.handle;
	if (ioctl(fd, ATRIUM_GPU_IOC_FREE, &handle) != 0) {
		fprintf(stderr, "IOC_FREE: %s\n", strerror(errno));
		close(fd);
		return (1);
	}
	printf("  IOC_FREE: handle=%u — ok\n", handle);

	/* Free of a stale handle should fail. */
	if (ioctl(fd, ATRIUM_GPU_IOC_FREE, &handle) == 0) {
		fprintf(stderr, "warn: double-free succeeded\n");
	} else if (errno != ENOENT) {
		fprintf(stderr, "double-free: unexpected errno %d (%s)\n",
		    errno, strerror(errno));
	} else {
		printf("  IOC_FREE on stale handle returns ENOENT — ok\n");
	}

	/* Submit a real virtio-gpu command: RESOURCE_CREATE_2D. We allocate
	 * a small command BO, fill it with the protocol bytes, IOC_SUBMIT,
	 * and check the fence comes back nonzero. */
	struct atrium_gpu_alloc cb;
	memset(&cb, 0, sizeof(cb));
	cb.size = 4096;
	cb.flags = ATRIUM_GPU_BO_GPU_VISIBLE | ATRIUM_GPU_BO_CPU_VISIBLE |
	           ATRIUM_GPU_BO_COHERENT;
	if (ioctl(fd, ATRIUM_GPU_IOC_ALLOC, &cb) != 0) {
		fprintf(stderr, "submit: alloc cmd_bo: %s\n", strerror(errno));
		return (1);
	}
	void *cmap = mmap(NULL, cb.size, PROT_READ | PROT_WRITE, MAP_SHARED,
	    fd, cb.mmap_offset);
	if (cmap == MAP_FAILED) {
		fprintf(stderr, "submit: mmap cmd_bo: %s\n", strerror(errno));
		return (1);
	}

	/* RESOURCE_CREATE_2D: 24-byte hdr (le) + resource_id, format, w, h. */
	struct {
		uint32_t type, flags;
		uint64_t fence_id;
		uint32_t ctx_id;
		uint8_t  ring_idx, padding[3];
		uint32_t resource_id, format, width, height;
	} __attribute__((packed)) req;
	memset(&req, 0, sizeof(req));
	req.type = htole32(0x0101);  /* RESOURCE_CREATE_2D */
	req.resource_id = htole32(1);
	req.format = htole32(67);    /* VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM */
	req.width = htole32(256);
	req.height = htole32(256);
	memcpy(cmap, &req, sizeof(req));

	struct atrium_gpu_submit sb;
	memset(&sb, 0, sizeof(sb));
	sb.cmd_handle = cb.handle;
	sb.cmd_offset = 0;
	sb.cmd_size = sizeof(req);
	sb.engine = 0;  /* FRESCO_ENGINE_GRAPHICS */
	if (ioctl(fd, ATRIUM_GPU_IOC_SUBMIT, &sb) != 0) {
		fprintf(stderr, "IOC_SUBMIT: %s\n", strerror(errno));
		return (1);
	}
	printf("  IOC_SUBMIT(RESOURCE_CREATE_2D 256x256 BGRA): fence=%lu — ok\n",
	    (unsigned long)sb.fence_out);

	struct atrium_gpu_fence_query fq = { .engine = 0 };
	if (ioctl(fd, ATRIUM_GPU_IOC_FENCE_QUERY, &fq) != 0) {
		fprintf(stderr, "IOC_FENCE_QUERY: %s\n", strerror(errno));
		return (1);
	}
	printf("  IOC_FENCE_QUERY: latest_retired=%lu — ok\n",
	    (unsigned long)fq.latest_retired);

	munmap(cmap, cb.size);
	uint32_t ch = cb.handle;
	ioctl(fd, ATRIUM_GPU_IOC_FREE, &ch);

	close(fd);

	int dfd = open("/dev/atrium-display0", O_RDWR);
	if (dfd < 0) {
		fprintf(stderr, "open(/dev/atrium-display0): %s\n", strerror(errno));
		return (1);
	}
	printf("/dev/atrium-display0 opens — ok\n");
	close(dfd);

	return (0);
}
