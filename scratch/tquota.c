/* tquota — set/inspect a Tessera per-directory quota domain.
 *
 *   tquota set <dir> <bytes>     mark <dir> a quota root with an N-byte limit
 *   tquota clear <dir>           limit 0
 *   tquota statfs <path>         show what statfs(2) reports (should be
 *                                quota-scoped inside a domain, per §3.6)
 *
 * The ioctl is _IOW('T', 1, uint64_t) — see TESSERA_IOC_QUOTA_SET in the kmod.
 */
#include <sys/types.h>
#include <sys/ioccom.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/param.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

#define TESSERA_IOC_QUOTA_SET   _IOW('T', 1, uint64_t)
#define TESSERA_IOC_DEDUP_POLICY _IOW('T', 3, uint64_t)
#define TESSERA_IOC_GC          _IOR('T', 2, uint64_t)

int main(int argc, char **argv)
{
    if (argc < 3) { fprintf(stderr, "usage: tquota set|clear|statfs|policy <path> [arg]\n"); return 2; }
    const char *op = argv[1], *path = argv[2];

    if (strcmp(op, "statfs") == 0) {
        struct statfs sb;
        if (statfs(path, &sb) != 0) { perror("statfs"); return 1; }
        unsigned long long bs = sb.f_bsize;
        printf("  f_blocks=%llu  f_bfree=%llu  f_bavail=%llu  bsize=%llu\n",
               (unsigned long long)sb.f_blocks, (unsigned long long)sb.f_bfree,
               (unsigned long long)sb.f_bavail, bs);
        printf("  total=%.2f MiB  avail=%.2f MiB\n",
               sb.f_blocks * (double)bs / 1048576.0,
               sb.f_bavail * (double)bs / 1048576.0);
        return 0;
    }

    if (strcmp(op, "gc") == 0) {
        uint64_t reclaimed = 0;
        int fd = open(path, O_RDONLY);
        if (fd < 0) { perror("open"); return 1; }
        if (ioctl(fd, TESSERA_IOC_GC, &reclaimed) != 0) {
            fprintf(stderr, "ioctl GC: %s\n", strerror(errno));
            close(fd); return 1;
        }
        close(fd);
        printf("  gc reclaimed %llu pack(s)\n", (unsigned long long)reclaimed);
        return 0;
    }

    if (strcmp(op, "policy") == 0) {
        if (argc < 4) { fprintf(stderr, "policy needs 0|1 (global|deferred)\n"); return 2; }
        uint64_t pol = strtoull(argv[3], NULL, 10);
        int fd = open(path, O_RDONLY | O_DIRECTORY);
        if (fd < 0) { perror("open"); return 1; }
        if (ioctl(fd, TESSERA_IOC_DEDUP_POLICY, &pol) != 0) {
            fprintf(stderr, "ioctl DEDUP_POLICY(%llu): %s\n",
                    (unsigned long long)pol, strerror(errno));
            close(fd); return 1;
        }
        close(fd);
        printf("  dedup_policy on %s = %llu\n", path, (unsigned long long)pol);
        return 0;
    }

    uint64_t limit = 0;
    if (strcmp(op, "set") == 0) {
        if (argc < 4) { fprintf(stderr, "set needs <bytes>\n"); return 2; }
        limit = strtoull(argv[3], NULL, 10);
    } else if (strcmp(op, "clear") != 0) {
        fprintf(stderr, "unknown op %s\n", op); return 2;
    }

    int fd = open(path, O_RDONLY | O_DIRECTORY);
    if (fd < 0) { perror("open"); return 1; }
    if (ioctl(fd, TESSERA_IOC_QUOTA_SET, &limit) != 0) {
        fprintf(stderr, "ioctl QUOTA_SET(%llu): %s\n",
                (unsigned long long)limit, strerror(errno));
        close(fd); return 1;
    }
    close(fd);
    printf("  quota on %s set to %llu bytes\n", path, (unsigned long long)limit);
    return 0;
}
