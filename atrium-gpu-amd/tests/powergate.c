/*
 * powergate: drive the atrium-gpu-amd power-gating path ('A' 26) and confirm the
 * driver gates idle IP blocks, cutting the device's reported draw. Two scenarios:
 *
 *   1. Video playback — the decoder + display + DMA are busy, the 18 W graphics
 *      engine and the rest are idle. The driver gates them; the draw drops.
 *   2. Foreknowledge — the GPU is idle and a decode is about to start. With the
 *      next-needed blocks named, the driver pre-wakes them, so the wake stall the
 *      next workload would face reads 0 while the idle giant stays gated.
 *
 * Block bits match engine/src/powergate.rs IpGpu::rdna_class():
 *   0 gfx  1 vcn-decode  2 vcn-encode  3 jpeg  4 sdma  5 display
 */
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <err.h>

struct atrium_gpu_powergate {
	uint32_t busy_mask, next_busy, gate_mask;
	uint32_t power_before_mw, power_after_mw, wake_stall_us;
};
#define ATRIUM_GPU_IOC_POWERGATE _IOWR('A', 26, struct atrium_gpu_powergate)

#define GFX		(1u << 0)
#define VCN_DECODE	(1u << 1)
#define VCN_ENCODE	(1u << 2)
#define JPEG		(1u << 3)
#define SDMA		(1u << 4)
#define DISPLAY		(1u << 5)

int
main(void)
{
	int fd = open("/dev/atrium-gpu0", O_RDWR);
	if (fd < 0)
		err(1, "open");

	/* 1. Video playback: decoder + display + DMA busy; GFX/encode/JPEG idle. */
	struct atrium_gpu_powergate p = { 0 };
	p.busy_mask = VCN_DECODE | DISPLAY | SDMA;
	if (ioctl(fd, ATRIUM_GPU_IOC_POWERGATE, &p))
		err(1, "powergate video");
	printf("video: gated 0x%02x, draw %u -> %u mW%s\n",
	    p.gate_mask, p.power_before_mw, p.power_after_mw,
	    (p.gate_mask & GFX) ? " (idle GFX gated)" : "");
	if (p.power_after_mw * 3 >= p.power_before_mw)
		errx(1, "FAIL: gating did not cut the draw >3x");

	/* 2. Foreknowledge: GPU idle, a decode coming -> pre-wake decoder+display. */
	struct atrium_gpu_powergate f = { 0 };
	f.busy_mask = 0;			/* idle now */
	f.next_busy = VCN_DECODE | DISPLAY;	/* but a frame is coming */
	if (ioctl(fd, ATRIUM_GPU_IOC_POWERGATE, &f))
		err(1, "powergate foreknown");
	printf("foreknown: gated 0x%02x, next-workload wake stall %u us%s\n",
	    f.gate_mask, f.wake_stall_us,
	    (f.gate_mask & GFX) ? " (idle GFX still gated)" : "");
	if (f.wake_stall_us != 0)
		errx(1, "FAIL: pre-wake did not hide the wake latency");
	if (!(f.gate_mask & GFX))
		errx(1, "FAIL: the idle GFX giant should stay gated");

	printf("powergate OK\n");
	return (0);
}
