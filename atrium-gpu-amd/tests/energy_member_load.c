/*
 * energy_member_load: drive the gpusim GPU scheduler so it reports a
 * non-zero power demand — the input the kernel energy-budget federation
 * (kern.sched.energy_*) water_fills across members.  Adds one queue and
 * runs scheduling rounds via the SCHED ioctl ('A' 25); afterward the
 * device's regSCHED_POWER_DEMAND_MW reads its average power, which the
 * atrium-gpu-amd kmod surfaces as the "gpu0" federation member.
 */
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <err.h>
struct atrium_gpu_sched {
	uint32_t op, arg, ops, bytes, level;
	uint32_t energy_uj, runs, busy_us, count;
};
#define ATRIUM_GPU_IOC_SCHED _IOWR('A', 25, struct atrium_gpu_sched)
int main(void) {
	int fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0) err(1, "open");
	struct atrium_gpu_sched s = { .op = 0, .arg = 1, .ops = 1,
	    .bytes = 1000000000U, .level = 3 };
	if (ioctl(fd, ATRIUM_GPU_IOC_SCHED, &s)) err(1, "add");
	s.op = 1; s.arg = 5000;
	if (ioctl(fd, ATRIUM_GPU_IOC_SCHED, &s)) err(1, "run");
	printf("gpu sched load done\n");
	return 0;
}
