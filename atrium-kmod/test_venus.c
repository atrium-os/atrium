/*
 * test_venus — V3 smoke: CAPSET_QUERY + CTX_INIT against /dev/atrium-gpu0.
 *
 * Run inside the FreeBSD VM after `kldload atrium_virtio_gpu.ko`. Boots
 * up the venus device, queries the host for capset 4 (venus), creates a
 * context, then closes the fd so the dtor issues CTX_DESTROY.
 *
 * Build (in-VM): cc -o test_venus test_venus.c
 */

#include <sys/ioctl.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdint.h>

#include "atrium_gpu.h"

static const char *capset_name(uint32_t id)
{
	switch (id) {
	case ATRIUM_GPU_CAPSET_VIRGL:        return "virgl";
	case ATRIUM_GPU_CAPSET_VIRGL2:       return "virgl2";
	case ATRIUM_GPU_CAPSET_GFXSTREAM:    return "gfxstream";
	case ATRIUM_GPU_CAPSET_VENUS:        return "venus";
	case ATRIUM_GPU_CAPSET_CROSS_DOMAIN: return "cross-domain";
	case ATRIUM_GPU_CAPSET_DRM:          return "drm";
	default:                             return "?";
	}
}

int main(void)
{
	int fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0) { perror("open"); return 1; }

	/* Probe each capset id. Even ones the host doesn't advertise should
	 * return cleanly with actual_version = 0. */
	for (uint32_t id = 1; id <= 6; id++) {
		struct atrium_gpu_capset_query q = { .capset_id = id };
		if (ioctl(fd, ATRIUM_GPU_IOC_CAPSET_QUERY, &q) < 0) {
			perror("CAPSET_QUERY");
			continue;
		}
		printf("capset %u (%s): version=%u size=%u\n",
		       id, capset_name(id), q.actual_version, q.data_size);
	}

	/* Fetch the venus capset blob. */
	struct atrium_gpu_capset_query q = {
		.capset_id = ATRIUM_GPU_CAPSET_VENUS,
	};
	if (ioctl(fd, ATRIUM_GPU_IOC_CAPSET_QUERY, &q) < 0) {
		perror("CAPSET_QUERY venus size");
		close(fd); return 2;
	}
	/* "Not advertised" signal is data_size == 0; venus's actual_version
	 * is genuinely 0 (the protocol uses 0 to mean "no versioning"). */
	if (q.data_size == 0) {
		fprintf(stderr, "venus capset not advertised by host — "
		    "is QEMU running with -device virtio-gpu-gl-pci,venus=on?\n");
		close(fd); return 3;
	}

	uint8_t *blob = calloc(1, q.data_size);
	q.data_ptr = (uint64_t)(uintptr_t)blob;
	if (ioctl(fd, ATRIUM_GPU_IOC_CAPSET_QUERY, &q) < 0) {
		perror("CAPSET_QUERY venus blob");
		free(blob); close(fd); return 4;
	}
	printf("venus blob: %u bytes, first 16: ", q.data_size);
	for (uint32_t i = 0; i < 16 && i < q.data_size; i++)
		printf("%02x ", blob[i]);
	putchar('\n');
	free(blob);

	/* Bind a context. */
	struct atrium_gpu_ctx_init ci = {
		.capset_id  = ATRIUM_GPU_CAPSET_VENUS,
		.debug_name = "test_venus",
	};
	if (ioctl(fd, ATRIUM_GPU_IOC_CTX_INIT, &ci) < 0) {
		perror("CTX_INIT");
		close(fd); return 5;
	}
	printf("CTX_INIT ok, ctx_id=%u\n", ci.ctx_id_out);

	/* Second CTX_INIT on the same fd should EBUSY. */
	struct atrium_gpu_ctx_init ci2 = {
		.capset_id = ATRIUM_GPU_CAPSET_VENUS,
		.debug_name = "test_venus_again",
	};
	if (ioctl(fd, ATRIUM_GPU_IOC_CTX_INIT, &ci2) == 0) {
		fprintf(stderr, "second CTX_INIT unexpectedly succeeded\n");
		close(fd); return 6;
	}
	printf("second CTX_INIT correctly rejected\n");

	/* V4: allocate a BO, attach as a venus blob, submit a (fake)
	 * empty command stream. Real venus command streams come from
	 * mesa-venus at V5; for now we just exercise the round-trip. */
	struct atrium_gpu_alloc alloc = { .size = 64 * 1024,
		.flags = ATRIUM_GPU_BO_GPU_VISIBLE | ATRIUM_GPU_BO_CPU_VISIBLE
		       | ATRIUM_GPU_BO_COHERENT };
	if (ioctl(fd, ATRIUM_GPU_IOC_ALLOC, &alloc) < 0) {
		perror("ALLOC");
		close(fd); return 7;
	}
	printf("ALLOC ok, bo_handle=%u size=%llu\n",
	       alloc.handle, (unsigned long long)alloc.size);

	struct atrium_gpu_resource_attach ra = {
		.bo_handle  = alloc.handle,
		.blob_mem   = ATRIUM_GPU_BLOB_MEM_GUEST,
		.blob_flags = ATRIUM_GPU_BLOB_USE_MAPPABLE,
		.blob_id    = 0,
	};
	if (ioctl(fd, ATRIUM_GPU_IOC_RESOURCE_ATTACH, &ra) < 0) {
		perror("RESOURCE_ATTACH");
		close(fd); return 8;
	}
	printf("RESOURCE_ATTACH ok, resource_id=%u\n", ra.resource_id_out);

	/* SUBMIT_3D with a tiny "hello world" payload. The host's venus
	 * worker will fail to parse it as a valid VkCommandStream, but
	 * the submit round-trip itself (and fence retire) is what we're
	 * testing here. The host returns ERR_UNSPEC; submit returns EIO
	 * which is the expected failure mode for V4 smoke. */
	uint8_t fake_cmd[16] = {0};
	struct atrium_gpu_submit_3d sub = {
		.cmd_ptr  = (uint64_t)(uintptr_t)fake_cmd,
		.cmd_size = sizeof(fake_cmd),
		.flags    = ATRIUM_GPU_SUBMIT_3D_SIGNAL_FENCE,
	};
	int rc = ioctl(fd, ATRIUM_GPU_IOC_SUBMIT_3D, &sub);
	if (rc < 0)
		printf("SUBMIT_3D returned errno=%d (expected — fake "
		       "payload, host venus rejects)\n", errno);
	else
		printf("SUBMIT_3D ok, fence_out=%llu\n",
		       (unsigned long long)sub.fence_out);

	/* CTX_FENCE_WAIT on a known-retired fence should be instant. */
	if (sub.fence_out != 0) {
		struct atrium_gpu_ctx_fence_wait fw = {
			.fence = sub.fence_out, .timeout_ns = ~0ULL,
		};
		if (ioctl(fd, ATRIUM_GPU_IOC_CTX_FENCE_WAIT, &fw) == 0)
			printf("CTX_FENCE_WAIT ok, status=%u\n", fw.status);
	}

	/* Close fd → dtor issues CTX_DESTROY. */
	close(fd);
	puts("closed fd; CTX_DESTROY issued by dtor");
	return 0;
}
