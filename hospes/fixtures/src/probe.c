/* Isolation probe (docs/spec/hospes.md §11.7): raw FreeBSD system calls made by the loaded
 * library itself, each of which capability mode must refuse. Returns the number of calls that
 * were NOT refused with ECAPMODE (94); 0 means isolated. On Linux it returns -1 (no capability
 * mode in the development backend). */
#include "fixture.h"
static long sc(long n, long a, long b, long c, long d) {
#if defined(__x86_64__)
    long ret;
    char cf;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall; setc %1"
                     : "=a"(ret), "=r"(cf), "+D"(a), "+S"(b), "+d"(c), "+r"(r10)
                     : "a"(n)
                     : "rcx", "r11", "r8", "r9", "memory");
    return cf ? -ret : ret;
#else
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    long cs;
    __asm__ volatile("svc #0; cset %1, cs"
                     : "+r"(x0), "=r"(cs), "+r"(x1), "+r"(x2), "+r"(x3)
                     : "r"(x8)
                     : "x4", "x5", "memory");
    return cs ? -x0 : x0;
#endif
}
struct sin { unsigned char len, family; unsigned short port; unsigned int addr; char zero[8]; };
static int refused(long r, const char *what) {
    if (r == -94) return 0;
    host_log(what);
    return 1;
}
EXPORT int fixture_main(void) {
    if (host_os() != 1) return -1;
    int bad = 0;
    bad += refused(sc(5, (long)"/etc/passwd", 0, 0, 0), "open /etc/passwd not refused");
    bad += refused(sc(499, -100, (long)"/etc/passwd", 0, 0), "openat AT_FDCWD not refused");
    bad += refused(sc(499, -100, (long)"/dev/mem", 0, 0), "openat /dev/mem not refused");
    bad += refused(sc(12, (long)"/", 0, 0, 0), "chdir not refused");
    bad += refused(sc(136, (long)"/tmp/hospes-probe", 0700, 0, 0), "mkdir not refused");
    bad += refused(sc(10, (long)"/tmp/hospes-probe", 0, 0, 0), "unlink not refused");
    bad += refused(sc(37, 1, 0, 0, 0), "kill(1) not refused");
    bad += refused(sc(26, 10 /* PT_ATTACH */, 1, 0, 0), "ptrace not refused");
    bad += refused(sc(59, (long)"/bin/sh", 0, 0, 0), "execve not refused");
    long s = sc(97, 2 /* AF_INET */, 1 /* SOCK_STREAM */, 0, 0);
    if (s >= 0) {
        struct sin a = {16, 2, 0x1600 /* port 22, network order */, 0x0100007f, {0}};
        bad += refused(sc(98, s, (long)&a, 16, 0), "connect to 127.0.0.1 not refused");
        bad += refused(sc(104, s, (long)&a, 16, 0), "bind not refused");
    }
    return bad;
}
