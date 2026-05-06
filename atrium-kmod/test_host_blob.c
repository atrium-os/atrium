/*
 * test_host_blob — minimal repro for V5h IOC_HOST_BLOB.
 * Steps in order; bail at first failure with errno + step name.
 *
 * Build: cc test_host_blob.c -o test_host_blob
 * Run:   ./test_host_blob [step]  (default = all steps)
 *   step=open
 *   step=capset
 *   step=ctx
 *   step=blob   (the new one — most likely to hang)
 *   step=mmap   (also exercises BAR mmap)
 */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

#include "atrium_gpu.h"

static int run_step(int fd, const char *step);

int
main(int argc, char **argv)
{
	int fd;
	const char *want = (argc > 1) ? argv[1] : "all";

	fprintf(stderr, "[step=open]\n");
	fd = open("/dev/atrium-gpu0", O_RDWR | O_CLOEXEC);
	if (fd < 0) { perror("open"); return 1; }
	fprintf(stderr, "[step=open] ok fd=%d\n", fd);

	if (strcmp(want, "open") == 0) goto done;

	if (run_step(fd, "capset") != 0) return 1;
	if (strcmp(want, "capset") == 0) goto done;
	if (run_step(fd, "ctx") != 0) return 1;
	if (strcmp(want, "ctx") == 0) goto done;
	if (run_step(fd, "blob") != 0) return 1;
	if (strcmp(want, "blob") == 0) goto done;
	if (run_step(fd, "mmap") != 0) return 1;

done:
	close(fd);
	fprintf(stderr, "DONE\n");
	return 0;
}

static struct atrium_gpu_host_blob g_hb;

static int
run_step(int fd, const char *step)
{
	fprintf(stderr, "[step=%s]\n", step);
	if (strcmp(step, "capset") == 0) {
		struct atrium_gpu_capset_query q = { .capset_id = ATRIUM_GPU_CAPSET_VENUS };
		if (ioctl(fd, ATRIUM_GPU_IOC_CAPSET_QUERY, &q) < 0) {
			perror("CAPSET_QUERY");
			return -1;
		}
		fprintf(stderr, "[step=capset] ok ver=%u sz=%u\n",
		    q.actual_version, q.data_size);
	} else if (strcmp(step, "ctx") == 0) {
		struct atrium_gpu_ctx_init ci = {
			.capset_id = ATRIUM_GPU_CAPSET_VENUS,
			.debug_name = "test-host-blob",
		};
		if (ioctl(fd, ATRIUM_GPU_IOC_CTX_INIT, &ci) < 0) {
			perror("CTX_INIT");
			return -1;
		}
		fprintf(stderr, "[step=ctx] ok ctx_id=%u\n", ci.ctx_id_out);
	} else if (strcmp(step, "blob") == 0) {
		memset(&g_hb, 0, sizeof(g_hb));
		g_hb.size       = 4096;
		g_hb.blob_flags = ATRIUM_GPU_BLOB_USE_MAPPABLE;
		g_hb.blob_id    = 0;
		fprintf(stderr, "[step=blob] sending IOC_HOST_BLOB...\n");
		if (ioctl(fd, ATRIUM_GPU_IOC_HOST_BLOB, &g_hb) < 0) {
			perror("HOST_BLOB");
			return -1;
		}
		fprintf(stderr, "[step=blob] ok bo=%u res=%u off=0x%lx asz=%lu\n",
		    g_hb.bo_handle, g_hb.resource_id,
		    (unsigned long)g_hb.mmap_offset,
		    (unsigned long)g_hb.actual_size);
	} else if (strcmp(step, "mmap") == 0) {
		void *p = mmap(NULL, g_hb.actual_size, PROT_READ | PROT_WRITE,
		    MAP_SHARED, fd, g_hb.mmap_offset);
		if (p == MAP_FAILED) { perror("mmap"); return -1; }
		fprintf(stderr, "[step=mmap] ok ptr=%p; touching first byte...\n", p);
		((volatile char *)p)[0] = 0x42;
		fprintf(stderr, "[step=mmap] wrote ok, reading back: 0x%x\n",
		    ((volatile unsigned char *)p)[0]);
	}
	return 0;
}
