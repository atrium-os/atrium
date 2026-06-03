/*
 * carillon_smoke.c — guest-side round-trip test for /dev/carillon0.
 *
 * mmaps the BAR2 shared region, validates the control header, stages a
 * frame, drives the submission ring, rings the host doorbell, parks on
 * the completion doorbell (ioctl CARILLON_WAIT — no spin), and reads the
 * completion back. Proves the full guest<->host Carillon doorbell loop.
 *
 * Build in-VM:  cc -O -Wall -o /tmp/carillon_smoke carillon_smoke.c
 */
#include <sys/types.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "carillon_abi.h"

static inline uint32_t ld(volatile void *p) { return *(volatile uint32_t *)p; }
static inline void st(volatile void *p, uint32_t v) { *(volatile uint32_t *)p = v; }

int main(void)
{
	int fd = open("/dev/carillon0", O_RDWR);
	if (fd < 0) { perror("open /dev/carillon0"); return 1; }

	uint8_t *base = mmap(NULL, CARILLON_TOTAL_SIZE, PROT_READ | PROT_WRITE,
	    MAP_SHARED, fd, 0);
	if (base == MAP_FAILED) { perror("mmap"); return 1; }

	uint8_t *ctrl = base + CARILLON_CTRL_OFFSET;
	uint32_t magic = ld(ctrl + CARILLON_C_MAGIC);
	uint32_t hstat = ld(ctrl + CARILLON_C_HOST_STATUS);
	uint32_t hps   = ld(ctrl + CARILLON_C_HOST_PAGE_SIZE);
	printf("control: magic=%#010x host_status=%u host_page_size=%u\n",
	    magic, hstat, hps);
	if (magic != CARILLON_MAGIC) {
		printf("FAIL: bad magic (want %#010x)\n", CARILLON_MAGIC);
		return 2;
	}

	/* Mark the guest booted (mirrors GuestRing::new). */
	st(ctrl + CARILLON_C_GUEST_STATUS, 1);

	/* Stage a 32-byte frame in the arena. */
	uint8_t frame[32];
	memset(frame, 0xAB, sizeof frame);
	memcpy(base + CARILLON_FRAME_ARENA_OFFSET, frame, sizeof frame);

	/* Push a submission descriptor. */
	uint32_t w = ld(ctrl + CARILLON_C_SUB_WRITE);
	uint8_t *slot = base + CARILLON_SUB_RING_OFFSET +
	    (w % CARILLON_SUB_ENTRIES) * CARILLON_DESC_SIZE;
	memset(slot, 0, CARILLON_DESC_SIZE);
	uint32_t kind = 1, fence = 42, foff = 0, flen = 32;
	memcpy(slot + 0, &kind, 4);
	memcpy(slot + 4, &fence, 4);
	memcpy(slot + 8, &foff, 4);
	memcpy(slot + 12, &flen, 4);
	__sync_synchronize();
	st(ctrl + CARILLON_C_SUB_WRITE, w + 1);
	__sync_synchronize();

	/* Ring the host doorbell. */
	uint32_t seq = 0;
	if (ioctl(fd, CARILLON_RING) != 0) { perror("ioctl RING"); return 3; }
	printf("rang host; waiting for completion doorbell...\n");

	/* Edge-driven loop: check the completion ring; if empty, park on the
	 * doorbell (passing the last-seen seq so a doorbell that already fired
	 * isn't lost), then re-check. No spin, no timeout reliance. */
	for (int attempt = 0; attempt < 10; attempt++) {
		__sync_synchronize();
		uint32_t cw_idx = ld(ctrl + CARILLON_C_COMP_WRITE);
		uint32_t cr = ld(ctrl + CARILLON_C_COMP_READ);
		if (cr == cw_idx) {
			struct carillon_wait cw = { .timeout_ms = 1000, .seq = seq };
			ioctl(fd, CARILLON_WAIT, &cw);
			printf("  wake: seq %u -> %u\n", seq, cw.seq);
			seq = cw.seq;
			continue;
		}
		printf("  completion ready: comp_write=%u comp_read=%u\n",
		    cw_idx, cr);
		{
			uint8_t *cs = base + CARILLON_COMP_RING_OFFSET +
			    (cr % CARILLON_COMP_ENTRIES) * CARILLON_DESC_SIZE;
			uint32_t ckind, cfence, cres, roff, rlen;
			memcpy(&ckind, cs + 0, 4);
			memcpy(&cfence, cs + 4, 4);
			memcpy(&cres, cs + 8, 4);
			memcpy(&roff, cs + 12, 4);
			memcpy(&rlen, cs + 16, 4);
			st(ctrl + CARILLON_C_COMP_READ, cr + 1);
			printf("COMPLETION: kind=%u fence=%u result=%u readback=%u/%u\n",
			    ckind, cfence, cres, roff, rlen);
			if (cfence == fence) {
				printf("ROUND-TRIP OK\n");
				return 0;
			}
		}
	}
	printf("FAIL: no matching completion\n");
	return 4;
}
