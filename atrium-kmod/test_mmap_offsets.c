/*
 * test_mmap_offsets — audit V6: ordinary BOs and host-visible (HOST_BLOB)
 * BOs on one fd must get disjoint mmap offset ranges, and each mapping must
 * reach its own BO.
 *
 * Build: cc -O -Wall -o test_mmap_offsets test_mmap_offsets.c
 * Run:   ./test_mmap_offsets          (needs a Venus-capable virtio-gpu)
 * Prints PASS or FAIL: <reason>.
 */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "atrium_gpu.h"

#define HOST_BASE (1ULL << 62)
#define NBO 4

static int
fail(const char *what)
{
	printf("FAIL: %s (errno %d: %s)\n", what, errno, strerror(errno));
	return (1);
}

int
main(void)
{
	struct atrium_gpu_alloc al[NBO];
	struct atrium_gpu_host_blob hb;
	struct atrium_gpu_ctx_init ci;
	volatile uint8_t *pa[NBO], *pb;
	int fd, i;

	fd = open("/dev/atrium-gpu0", O_RDWR | O_CLOEXEC);
	if (fd < 0)
		return (fail("open /dev/atrium-gpu0"));

	for (i = 0; i < NBO; i++) {
		memset(&al[i], 0, sizeof(al[i]));
		al[i].size = 1 << 20;
		al[i].flags = ATRIUM_GPU_BO_CPU_VISIBLE;
		if (ioctl(fd, ATRIUM_GPU_IOC_ALLOC, &al[i]) < 0)
			return (fail("IOC_ALLOC"));
		if (al[i].mmap_offset >= HOST_BASE) {
			printf("FAIL: ordinary BO offset 0x%llx is in the host range\n",
			    (unsigned long long)al[i].mmap_offset);
			return (1);
		}
	}

	ci = (struct atrium_gpu_ctx_init){
		.capset_id = ATRIUM_GPU_CAPSET_VENUS,
		.debug_name = "test-mmap-offsets",
	};
	if (ioctl(fd, ATRIUM_GPU_IOC_CTX_INIT, &ci) < 0)
		return (fail("CTX_INIT (Venus)"));
	memset(&hb, 0, sizeof(hb));
	hb.size = 1 << 20;
	hb.blob_flags = ATRIUM_GPU_BLOB_USE_MAPPABLE;
	if (ioctl(fd, ATRIUM_GPU_IOC_HOST_BLOB, &hb) < 0)
		return (fail("IOC_HOST_BLOB"));
	printf("ordinary offsets 0x%llx..0x%llx, host blob 0x%llx (+0x%llx)\n",
	    (unsigned long long)al[0].mmap_offset,
	    (unsigned long long)al[NBO - 1].mmap_offset,
	    (unsigned long long)hb.mmap_offset, (unsigned long long)hb.actual_size);
	if (hb.mmap_offset < HOST_BASE) {
		printf("FAIL: host blob offset 0x%llx is below the host range\n",
		    (unsigned long long)hb.mmap_offset);
		return (1);
	}

	for (i = 0; i < NBO; i++) {
		pa[i] = mmap(NULL, al[i].size, PROT_READ | PROT_WRITE, MAP_SHARED,
		    fd, (off_t)al[i].mmap_offset);
		if (pa[i] == MAP_FAILED)
			return (fail("mmap ordinary BO"));
		pa[i][0] = 0xa0 + i;
	}
	pb = mmap(NULL, hb.actual_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
	    (off_t)hb.mmap_offset);
	if (pb == MAP_FAILED)
		return (fail("mmap host blob"));
	pb[0] = 0x55;
	for (i = 0; i < NBO; i++)
		if (pa[i][0] != 0xa0 + i) {
			printf("FAIL: ordinary BO %d reads 0x%x after the blob write\n",
			    i, pa[i][0]);
			return (1);
		}
	if (pb[0] != 0x55) {
		printf("FAIL: host blob reads 0x%x\n", pb[0]);
		return (1);
	}
	close(fd);
	printf("PASS\n");
	return (0);
}
