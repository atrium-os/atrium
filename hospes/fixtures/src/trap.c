/* System call trap fixture (docs/spec/hospes.md §13 R1, step K1): raw Linux system calls
 * made by the library's own code, as Widevine makes them (openat, read, close,
 * rt_sigaction). Under Hospes they never reach the host kernel: the trap raises SIGSYS and
 * the shim answers them (hospes-glibc syscall.rs, vfs.rs).
 *
 * fixture_trap_main(trapped): trapped = 1 under the trap, 0 for the reference run on plain
 * Linux (the real kernel answers). Checks that hold in both are made in both. Returns 0 or
 * the number of the first failed check. */
#include "fixture.h"

#if defined(__x86_64__)
#define NR_READ 0
#define NR_CLOSE 3
#define NR_RT_SIGACTION 13
#define NR_GETPID 39
#define NR_OPENAT 257
#else
#define NR_OPENAT 56
#define NR_CLOSE 57
#define NR_READ 63
#define NR_RT_SIGACTION 134
#define NR_GETPID 172
#endif
#define AT_FDCWD (-100)
#define O_CLOEXEC 02000000
#define EBADF 9
#define ENOENT 2
#define EPERM 1
#define ENOSYS 38
#define SA_SIGINFO 4

/* Linux convention: the result, or -errno, in the return register. */
static long sc(long n, long a, long b, long c, long d) {
#if defined(__x86_64__)
    long ret;
    register long r10 __asm__("r10") = d;
    __asm__ volatile("syscall"
                     : "=a"(ret), "+D"(a), "+S"(b), "+d"(c), "+r"(r10)
                     : "a"(n)
                     : "rcx", "r11", "r8", "r9", "memory");
    return ret;
#else
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    __asm__ volatile("svc #0" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : "r"(x8) : "x4", "x5", "memory");
    return x0;
#endif
}

struct ksigaction { uptr handler; unsigned long flags; uptr restorer; unsigned long mask; };

static char buf[65536];

static int starts(const char *s, const char *p) {
    while (*p) if (*s++ != *p++) return 0;
    return 1;
}

static uptr hex(const char **s) {
    uptr v = 0;
    for (;; (*s)++) {
        char c = **s;
        if (c >= '0' && c <= '9') v = v * 16 + (uptr)(c - '0');
        else if (c >= 'a' && c <= 'f') v = v * 16 + (uptr)(c - 'a' + 10);
        else return v;
    }
}

/* Read a whole file with raw calls; its length, or a negative errno. */
static long slurp(const char *path) {
    long fd = sc(NR_OPENAT, AT_FDCWD, (long)path, O_CLOEXEC, 0);
    if (fd < 0) return fd;
    long n = 0;
    for (;;) {
        long r = sc(NR_READ, fd, (long)(buf + n), (long)(sizeof buf - 1 - (unsigned long)n), 0);
        if (r < 0) { sc(NR_CLOSE, fd, 0, 0, 0); return r; }
        if (r == 0) break;
        n += r;
        if (n == (long)sizeof buf - 1) break;
    }
    buf[n] = 0;
    if (sc(NR_CLOSE, fd, 0, 0, 0) != 0) return -1000;
    if (sc(NR_CLOSE, fd, 0, 0, 0) != -EBADF) return -1001;
    return n;
}

EXPORT int fixture_trap_main(int trapped);

/* Does a /proc/self/maps line map `a` executable? */
static int maps_exec(uptr a) {
    const char *s = buf;
    while (*s) {
        uptr lo = hex(&s);
        if (*s == '-') s++;
        uptr hi = hex(&s);
        if (*s == ' ') s++;
        if (a >= lo && a < hi && s[2] == 'x') return 1;
        while (*s && *s != '\n') s++;
        if (*s) s++;
    }
    return 0;
}

static uptr thread_call(uptr arg) {
    (void)arg;
    /* A raw call on another thread is trapped too. */
    return (uptr)sc(NR_OPENAT, AT_FDCWD, (long)"/nonexistent/hospes", O_CLOEXEC, 0);
}

/* Linux keeps every register but the result across a system call (x86_64 also sets rcx
 * and r11), and compilers rely on it: check the trapped path does too. */
static int regs_kept(void) {
#if defined(__x86_64__)
    register long r10 __asm__("r10") = 0x1010101010L;
    register long r8 __asm__("r8") = 0x0808080808L;
    register long r9 __asm__("r9") = 0x0909090909L;
    long di = 12345, si = 0x5151515151L, dx = 0xd1d1d1d1d1L, ret;
    __asm__ volatile("syscall"
                     : "=a"(ret), "+r"(r10), "+r"(r8), "+r"(r9), "+D"(di), "+S"(si), "+d"(dx)
                     : "a"((long)NR_CLOSE)
                     : "rcx", "r11", "memory");
    return ret == -EBADF && r10 == 0x1010101010L && r8 == 0x0808080808L && r9 == 0x0909090909L &&
           di == 12345 && si == 0x5151515151L && dx == 0xd1d1d1d1d1L;
#else
    register long x8 __asm__("x8") = NR_CLOSE;
    register long x0 __asm__("x0") = 12345;
    register long x1 __asm__("x1") = 0x1111111111L;
    register long x2 __asm__("x2") = 0x2222222222L;
    register long x3 __asm__("x3") = 0x3333333333L;
    register long x4 __asm__("x4") = 0x4444444444L;
    register long x5 __asm__("x5") = 0x5555555555L;
    __asm__ volatile("svc #0" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3), "+r"(x4), "+r"(x5), "+r"(x8) : : "memory");
    return x0 == -EBADF && x1 == 0x1111111111L && x2 == 0x2222222222L && x3 == 0x3333333333L &&
           x4 == 0x4444444444L && x5 == 0x5555555555L && x8 == NR_CLOSE;
#endif
}

EXPORT int fixture_trap_main(int trapped) {
    /* What the H0 Widevine run did raw: /proc/net/unix five times, /proc/self/maps nine. */
    long n;
    for (int i = 0; i < 5; i++) {
        n = slurp("/proc/net/unix");
        CHECK(1, n > 0 && starts(buf, "Num       RefCount Protocol Flags    Type St Inode Path\n"));
    }
    host_log("openat/read/close /proc/net/unix x5: header line");
    for (int i = 0; i < 9; i++) {
        n = slurp("/proc/self/maps");
        CHECK(2, n > 0 && maps_exec((uptr)fixture_trap_main));
    }
    host_log("openat/read/close /proc/self/maps x9: our text is mapped executable");

    CHECK(3, sc(NR_OPENAT, AT_FDCWD, (long)"/nonexistent/hospes", O_CLOEXEC, 0) == -ENOENT);
    CHECK(4, sc(NR_READ, 12345, (long)buf, 16, 0) == -EBADF);
    CHECK(5, sc(NR_CLOSE, 12345, 0, 0, 0) == -EBADF);
    host_log("missing path ENOENT; unknown descriptor EBADF");
    CHECK(10, regs_kept());
    host_log("registers other than the result kept, as Linux keeps them");

    struct ksigaction old;
    old.handler = 7;
    /* H0's queries: SIGILL, SIGTRAP, SIGABRT (default here) and SIGSYS. */
    static const int dfl[] = { 4, 5, 6 };
    for (int i = 0; i < 3; i++) {
        old.handler = 7;
        CHECK(6, sc(NR_RT_SIGACTION, dfl[i], 0, (long)&old, 8) == 0 && old.handler == 0);
    }
    CHECK(7, sc(NR_RT_SIGACTION, 31 /* SIGSYS */, 0, (long)&old, 8) == 0);
    CHECK(8, sc(NR_RT_SIGACTION, 31, 0, (long)&old, 16) == -22);
    host_log("rt_sigaction queries answered");

    CHECK(9, (long)host_thread_join(host_thread_create(thread_call, 0)) == -ENOENT);
    host_log("raw call on a second thread");

    if (!trapped) return 0;

    /* Only under the trap: the shim's own handler is what SIGSYS shows, a library cannot
     * replace it, and calls outside the emulated set never reach the kernel. */
    CHECK(20, old.handler != 0 && (old.flags & SA_SIGINFO) != 0);
    struct ksigaction act = { 1, 0, 0, 0 };
    CHECK(21, sc(NR_RT_SIGACTION, 31, (long)&act, 0, 8) == -EPERM);
    CHECK(22, sc(NR_RT_SIGACTION, 10 /* SIGUSR1 */, (long)&act, 0, 8) == -EPERM);
    host_log("SIGSYS shows the trap handler; setting a handler refused");
    CHECK(23, sc(NR_GETPID, 0, 0, 0, 0) == -ENOSYS);
    /* FreeBSD's own openat (499) and open (5), as an escape attempt: Linux numbers here. */
    CHECK(24, sc(499, AT_FDCWD, (long)"/etc/passwd", 0, 0) == -ENOSYS);
    CHECK(25, sc(5, (long)"/etc/passwd", 0, 0, 0) == -ENOSYS);
    host_log("other calls ENOSYS, FreeBSD-numbered calls never reach FreeBSD");
    return 0;
}
