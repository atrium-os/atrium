/*
 * test_roundtrip.c — minimal proof of the full guest↔host transport.
 *
 * Open /dev/fresco0, mmap the 16 MiB shmem BAR, write a
 * CMD_QUERY_HASH (opcode 0x0302) into the command ring, ring the
 * doorbell, then poll the completion ring for the response. The
 * gpu-server will reply with a 128-byte Completion record carrying
 * a STATUS_NOT_FOUND status (we asked about a hash it doesn't have).
 *
 * Compile in-VM:  cc test_roundtrip.c -o test_roundtrip
 * Run:            ./test_roundtrip
 */

#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <time.h>

#include "fresco_ioctl.h"

#define SHMEM_SIZE          (16 * 1024 * 1024)
#define CMD_RING_OFFSET     0x1000
#define COMP_RING_OFFSET    0x9000
#define CMD_ENTRY_SIZE      128
#define CMD_RING_ENTRIES    256

#define CTRL_CMD_WRITE      0
#define CTRL_CMD_READ       4
#define CTRL_COMP_WRITE     8
#define CTRL_COMP_READ      12
#define CTRL_STATUS         24

#define CMD_QUERY_HASH      0x0302

struct command {
        uint16_t opcode;
        uint16_t flags;
        uint32_t sequence_id;
        uint32_t payload[30];
} __attribute__((packed));

struct completion {
        uint16_t comp_type;
        uint16_t status;
        uint32_t id;
        uint8_t  result_hash[32];
        uint32_t pad[22];
} __attribute__((packed));

static double now_ms(void) {
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

int main(void) {
        int fd = open("/dev/fresco0", O_RDWR);
        if (fd < 0) { perror("open"); return 1; }

        volatile uint8_t *shmem = mmap(NULL, SHMEM_SIZE, PROT_READ | PROT_WRITE,
            MAP_SHARED, fd, 0);
        if (shmem == MAP_FAILED) { perror("mmap"); return 1; }

        uint32_t pos = 0;
        ioctl(fd, FRESCO_IOC_IVPOSITION, &pos);
        printf("opened /dev/fresco0, ivposition=%u\n", pos);

        /* Wait for host to mark itself READY (status == 1). */
        for (int i = 0; i < 50; i++) {
                uint32_t st = *(volatile uint32_t *)(shmem + CTRL_STATUS);
                if (st == 1) { printf("host status=READY\n"); break; }
                usleep(20000);
        }

        /* Build a CMD_QUERY_HASH for an all-zero hash (host won't have it). */
        struct command cmd = {0};
        cmd.opcode = CMD_QUERY_HASH;
        cmd.sequence_id = 0xC0FFEE;
        /* hash bytes are at payload offset 0 (i.e. payload[0..7]) -- leave zero */

        /* Read current cmd_write pointer */
        uint32_t cmd_w = *(volatile uint32_t *)(shmem + CTRL_CMD_WRITE);
        uint32_t comp_r_start = *(volatile uint32_t *)(shmem + CTRL_COMP_READ);
        uint32_t comp_w_start = *(volatile uint32_t *)(shmem + CTRL_COMP_WRITE);
        printf("before: cmd_write=%u comp_write=%u comp_read=%u\n",
            cmd_w, comp_w_start, comp_r_start);

        /* Write command into ring at slot (cmd_w % entries) */
        uint32_t slot = cmd_w % CMD_RING_ENTRIES;
        memcpy((void *)(shmem + CMD_RING_OFFSET + slot * CMD_ENTRY_SIZE),
               &cmd, sizeof(cmd));

        /* Bump cmd_write so host sees a new command */
        *(volatile uint32_t *)(shmem + CTRL_CMD_WRITE) = cmd_w + 1;

        /* Doorbell the host (vector 0). Host polls anyway, but we exercise
         * the path. */
        uint16_t vec = 0;
        ioctl(fd, FRESCO_IOC_DOORBELL, &vec);

        /* Poll for completion */
        double t0 = now_ms();
        uint32_t comp_w;
        for (;;) {
                comp_w = *(volatile uint32_t *)(shmem + CTRL_COMP_WRITE);
                if (comp_w != comp_w_start) break;
                if (now_ms() - t0 > 2000.0) {
                        fprintf(stderr, "TIMEOUT waiting for completion\n");
                        return 2;
                }
                usleep(1000);
        }
        double rtt_ms = now_ms() - t0;

        uint32_t comp_slot = comp_r_start % CMD_RING_ENTRIES;
        struct completion comp;
        memcpy(&comp, (void *)(shmem + COMP_RING_OFFSET + comp_slot * CMD_ENTRY_SIZE),
               sizeof(comp));

        printf("after:  comp_write=%u\n", comp_w);
        printf("got completion: type=0x%02x status=0x%02x id=0x%08x\n",
            comp.comp_type, comp.status, comp.id);
        printf("round-trip latency: %.2f ms\n", rtt_ms);

        /* Advance comp_read so server knows we consumed it */
        *(volatile uint32_t *)(shmem + CTRL_COMP_READ) = comp_r_start + 1;

        /* Verify wake_count via ioctl */
        uint64_t wakes;
        ioctl(fd, FRESCO_IOC_WAKE_COUNT, &wakes);
        printf("kernel wake_count: %lu\n", wakes);

        /* COMP_QUERY_RESULT=0x03, STATUS_NOT_FOUND=0x04 (we asked
         * about an all-zero hash the host hasn't seen). */
        if (comp.comp_type == 0x03 && comp.status == 0x04) {
                printf("PASS: full round-trip works\n");
                return 0;
        } else {
                printf("FAIL: unexpected completion shape\n");
                return 3;
        }
}
