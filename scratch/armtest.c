/* Mimics portcullisd's arm_overlay_dedup() step for step, to validate the
 * SEQUENCE (statfs gate -> QUOTA_SET -> DEDUP_POLICY) and the by-pointer
 * argument passing. The kmod refuses a policy on a non-quota-root, so the
 * order is load-bearing: this fails loudly if it is ever reversed. */
#include <sys/ioccom.h>
#include <sys/mount.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#define IOC_QUOTA_SET    _IOW('T', 1, uint64_t)
#define IOC_DEDUP_POLICY _IOW('T', 3, uint64_t)
int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: armtest <dir>\n"); return 2; }
    struct statfs s;
    if (statfs(argv[1], &s) != 0) { perror("statfs"); return 1; }
    if (strcmp(s.f_fstypename, "tessera") != 0) {
        printf("  not tessera (%s) — would WARN and continue\n", s.f_fstypename);
        return 0;
    }
    int fd = open(argv[1], O_RDONLY);
    if (fd < 0) { perror("open"); return 1; }
    uint64_t limit = 64ULL * 1024 * 1024 * 1024;
    if (ioctl(fd, IOC_QUOTA_SET, &limit) != 0) {
        printf("  QUOTA_SET failed: %s\n", strerror(errno)); return 1;
    }
    printf("  QUOTA_SET ok (domain created)\n");
    uint64_t pol = 1; /* DEFERRED */
    if (ioctl(fd, IOC_DEDUP_POLICY, &pol) != 0) {
        printf("  DEDUP_POLICY failed: %s\n", strerror(errno)); return 1;
    }
    printf("  DEDUP_POLICY=deferred ok (oracle closed)\n");
    return 0;
}
