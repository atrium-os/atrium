/*
 * amdgpu_runner — run one Atrium RDNA4 oracle job on a real AMD GPU through the
 * stock Linux amdgpu driver. TEST TOOLING ONLY: it never ships, and it is the
 * only place Atrium touches the Linux DRM interface. Everything it submits
 * (shader binaries, command blobs, buffer layouts) is produced by our own
 * crates; this program only places buffers at the requested GPU addresses,
 * submits one indirect buffer, waits, and dumps results.
 *
 * Build (on the GPU box):  cc -O2 -Wall -o amdgpu_runner amdgpu_runner.c
 * Run:                     ./amdgpu_runner <job-dir> [/dev/dri/renderD128]
 *
 * Job directory:
 *   job.txt      one directive per line:
 *                  bo <name> <va-hex> <size-bytes> <init-file|-> <readback 0|1>
 *                  ib <bo-name> <byte-offset> <dwords>
 *                  ip compute|gfx
 *                  timeout_ms <n>
 *                  domain vram|gtt        (optional, default gtt; vram is
 *                                          CPU-visible VRAM, for timing)
 *                  repeat <n>             (optional: submit n times, reloading
 *                                          every BO's init contents first)
 *                  timestamps <bo-name>   (optional: the IB writes 64-bit GPU
 *                                          timestamps at offsets 0 and 8; the
 *                                          runner reports their difference)
 *   <init-file>  raw bytes copied to the start of the BO (rest zero)
 * Outputs (written into the job directory):
 *   <name>.out   BO contents after completion, for readback BOs
 *   result.txt   key=value: status, wait_status, elapsed_ns, gc_base, device;
 *                with timestamps also gpu_ns (one per repeat), gpu_ns_min,
 *                gpu_ns_median and counter_khz
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#define _GNU_SOURCE
#include <drm/amdgpu_drm.h>
#include <drm/drm.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#define MAX_BOS 32

struct bo {
	char		name[64];
	uint64_t	va, size;
	char		init[256];
	int		readback;
	uint32_t	handle;
	void		*cpu;
	uint64_t	mapped;
};

static struct bo bos[MAX_BOS];
static int nbos;
static FILE *result;

static void
die(const char *what)
{
	fprintf(stderr, "amdgpu_runner: %s: %s\n", what, strerror(errno));
	if (result) {
		fprintf(result, "status=error\nerror=%s: %s\n", what, strerror(errno));
		fclose(result);
	}
	exit(1);
}

static int
xioctl(int fd, unsigned long req, void *arg)
{
	int r;
	do { r = ioctl(fd, req, arg); } while (r == -1 && (errno == EINTR || errno == EAGAIN));
	return r;
}

static struct bo *
find_bo(const char *name)
{
	for (int i = 0; i < nbos; i++)
		if (strcmp(bos[i].name, name) == 0)
			return &bos[i];
	return NULL;
}

static uint64_t
now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

static void
load_inits(const char *dir)
{
	char path[4096];
	for (int i = 0; i < nbos; i++) {
		struct bo *o = &bos[i];
		memset(o->cpu, 0, o->mapped);
		if (strcmp(o->init, "-") == 0) continue;
		snprintf(path, sizeof path, "%s/%s", dir, o->init);
		FILE *f = fopen(path, "rb");
		if (!f) die(path);
		size_t n = fread(o->cpu, 1, o->size, f);
		(void)n;
		fclose(f);
	}
}

static int
cmp_u64(const void *a, const void *b)
{
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
	return x < y ? -1 : x > y;
}

static void
read_gc_base(char *out, size_t n)
{
	/* IP discovery: GC instance 0 base addresses, one per line. */
	FILE *f = fopen("/sys/class/drm/card0/device/ip_discovery/die/0/GC/0/base_addr", "r");
	if (!f) f = fopen("/sys/class/drm/card1/device/ip_discovery/die/0/GC/0/base_addr", "r");
	if (!f) { snprintf(out, n, "unknown"); return; }
	if (!fgets(out, (int)n, f)) snprintf(out, n, "unknown");
	out[strcspn(out, "\n")] = 0;
	fclose(f);
}

int
main(int argc, char **argv)
{
	char path[4096], line[512], ibname[64] = "";
	uint64_t ib_off = 0, ib_dw = 0, timeout_ms = 5000;
	uint32_t ip = AMDGPU_HW_IP_COMPUTE;
	uint64_t repeat = 1;
	int vram = 0;
	char tsname[64] = "";
	const char *dev = argc > 2 ? argv[2] : "/dev/dri/renderD128";

	if (argc < 2) {
		fprintf(stderr, "usage: %s <job-dir> [render-node]\n", argv[0]);
		return 2;
	}
	snprintf(path, sizeof path, "%s/result.txt", argv[1]);
	if (!(result = fopen(path, "w"))) die("result.txt");
	snprintf(path, sizeof path, "%s/job.txt", argv[1]);
	FILE *job = fopen(path, "r");
	if (!job) die("job.txt");
	while (fgets(line, sizeof line, job)) {
		char a[64], b[256];
		uint64_t x, y;
		int rb;
		if (line[0] == '#' || line[0] == '\n') continue;
		if (sscanf(line, "bo %63s %" SCNx64 " %" SCNu64 " %255s %d", a, &x, &y, b, &rb) == 5) {
			if (nbos == MAX_BOS) { errno = E2BIG; die("too many bos"); }
			struct bo *o = &bos[nbos++];
			snprintf(o->name, sizeof o->name, "%s", a);
			o->va = x; o->size = y; o->readback = rb;
			snprintf(o->init, sizeof o->init, "%s", b);
		} else if (sscanf(line, "ib %63s %" SCNu64 " %" SCNu64, a, &x, &y) == 3) {
			snprintf(ibname, sizeof ibname, "%s", a); ib_off = x; ib_dw = y;
		} else if (sscanf(line, "ip %63s", a) == 1) {
			ip = strcmp(a, "gfx") == 0 ? AMDGPU_HW_IP_GFX : AMDGPU_HW_IP_COMPUTE;
		} else if (sscanf(line, "timeout_ms %" SCNu64, &x) == 1) {
			timeout_ms = x;
		} else if (sscanf(line, "domain %63s", a) == 1) {
			vram = strcmp(a, "vram") == 0;
		} else if (sscanf(line, "repeat %" SCNu64, &x) == 1) {
			repeat = x ? x : 1;
		} else if (sscanf(line, "timestamps %63s", a) == 1) {
			snprintf(tsname, sizeof tsname, "%s", a);
		} else {
			errno = EINVAL; die(line);
		}
	}
	fclose(job);

	int fd = open(dev, O_RDWR | O_CLOEXEC);
	if (fd < 0) die(dev);

	union drm_amdgpu_ctx ctx = { .in.op = AMDGPU_CTX_OP_ALLOC_CTX };
	if (xioctl(fd, DRM_IOCTL_AMDGPU_CTX, &ctx)) die("ctx alloc");
	uint32_t ctx_id = ctx.out.alloc.ctx_id;

	for (int i = 0; i < nbos; i++) {
		struct bo *o = &bos[i];
		uint64_t sz = (o->size + 4095) & ~4095ull;
		union drm_amdgpu_gem_create gc = { .in = {
			.bo_size = sz, .alignment = 4096,
			.domains = vram ? AMDGPU_GEM_DOMAIN_VRAM : AMDGPU_GEM_DOMAIN_GTT,
			.domain_flags = AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED } };
		if (xioctl(fd, DRM_IOCTL_AMDGPU_GEM_CREATE, &gc)) die("gem create");
		o->handle = gc.out.handle;
		union drm_amdgpu_gem_mmap gm = { .in.handle = o->handle };
		if (xioctl(fd, DRM_IOCTL_AMDGPU_GEM_MMAP, &gm)) die("gem mmap");
		o->cpu = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, gm.out.addr_ptr);
		if (o->cpu == MAP_FAILED) die("mmap");
		o->mapped = sz;
		struct drm_amdgpu_gem_va va = {
			.handle = o->handle, .operation = AMDGPU_VA_OP_MAP,
			.flags = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE | AMDGPU_VM_PAGE_EXECUTABLE,
			.va_address = o->va, .offset_in_bo = 0, .map_size = sz };
		if (xioctl(fd, DRM_IOCTL_AMDGPU_GEM_VA, &va)) die("gem va");
	}

	struct bo *ibo = find_bo(ibname);
	if (!ibo) { errno = ENOENT; die("ib bo"); }

	struct drm_amdgpu_bo_list_entry ents[MAX_BOS];
	for (int i = 0; i < nbos; i++) { ents[i].bo_handle = bos[i].handle; ents[i].bo_priority = 0; }
	struct drm_amdgpu_bo_list_in bl = {
		.operation = ~0u, .list_handle = ~0u, .bo_number = (uint32_t)nbos,
		.bo_info_size = sizeof(struct drm_amdgpu_bo_list_entry),
		.bo_info_ptr = (uintptr_t)ents };
	struct drm_amdgpu_cs_chunk_ib ibc = {
		.va_start = ibo->va + ib_off, .ib_bytes = (uint32_t)(ib_dw * 4),
		.ip_type = ip, .ip_instance = 0, .ring = 0 };
	struct drm_amdgpu_cs_chunk chunks[2] = {
		{ .chunk_id = AMDGPU_CHUNK_ID_BO_HANDLES, .length_dw = sizeof bl / 4, .chunk_data = (uintptr_t)&bl },
		{ .chunk_id = AMDGPU_CHUNK_ID_IB, .length_dw = sizeof ibc / 4, .chunk_data = (uintptr_t)&ibc },
	};
	uint64_t chunk_ptrs[2] = { (uintptr_t)&chunks[0], (uintptr_t)&chunks[1] };
	union drm_amdgpu_cs cs;

	struct bo *tsb = tsname[0] ? find_bo(tsname) : NULL;
	if (tsname[0] && !tsb) { errno = ENOENT; die("timestamps bo"); }
	uint64_t *gpu = calloc(repeat, sizeof *gpu);
	union drm_amdgpu_wait_cs w;
	uint64_t t0 = 0, t1 = 0;
	for (uint64_t k = 0; k < repeat; k++) {
		load_inits(argv[1]);
		t0 = now_ns();
		/* The ioctl overwrites the union with its output, so refill it. */
		memset(&cs, 0, sizeof cs);
		cs.in.ctx_id = ctx_id; cs.in.num_chunks = 2; cs.in.chunks = (uintptr_t)chunk_ptrs;
		if (xioctl(fd, DRM_IOCTL_AMDGPU_CS, &cs)) die("cs submit");
		memset(&w, 0, sizeof w);
		w.in.handle = cs.out.handle; w.in.ip_type = ip; w.in.ctx_id = ctx_id;
		/* AMDGPU wait timeouts are absolute CLOCK_MONOTONIC nanoseconds. */
		w.in.timeout = now_ns() + timeout_ms * 1000000ull;
		if (xioctl(fd, DRM_IOCTL_AMDGPU_WAIT_CS, &w)) die("wait cs");
		t1 = now_ns();
		if (w.out.status) break;
		if (tsb) {
			volatile uint64_t *t = tsb->cpu;
			gpu[k] = t[1] - t[0];
		}
	}

	for (int i = 0; i < nbos; i++) {
		if (!bos[i].readback) continue;
		snprintf(path, sizeof path, "%s/%s.out", argv[1], bos[i].name);
		FILE *f = fopen(path, "wb");
		if (!f) die(path);
		fwrite(bos[i].cpu, 1, bos[i].size, f);
		fclose(f);
	}

	char gcb[256];
	read_gc_base(gcb, sizeof gcb);
	fprintf(result, "status=%s\nwait_status=%" PRIu64 "\nelapsed_ns=%" PRIu64 "\ngc_base=%s\ndevice=%s\n",
	    w.out.status ? "timeout" : "ok", (uint64_t)w.out.status, t1 - t0, gcb, dev);
	if (tsb && !w.out.status) {
		struct drm_amdgpu_info_device di;
		memset(&di, 0, sizeof di);
		struct drm_amdgpu_info q = { .return_pointer = (uintptr_t)&di,
			.return_size = sizeof di, .query = AMDGPU_INFO_DEV_INFO };
		uint64_t khz = xioctl(fd, DRM_IOCTL_AMDGPU_INFO, &q) == 0 ? di.gpu_counter_freq : 0;
		fprintf(result, "counter_khz=%" PRIu64 "\ngpu_ns=", khz);
		for (uint64_t k = 0; k < repeat; k++) {
			gpu[k] = khz ? gpu[k] * 1000000ull / khz : gpu[k];
			fprintf(result, "%s%" PRIu64, k ? "," : "", gpu[k]);
		}
		qsort(gpu, repeat, sizeof *gpu, cmp_u64);
		fprintf(result, "\ngpu_ns_min=%" PRIu64 "\ngpu_ns_median=%" PRIu64 "\n", gpu[0], gpu[repeat / 2]);
	}
	fclose(result);
	return w.out.status ? 3 : 0;
}
