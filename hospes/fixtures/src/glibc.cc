// H4 fixture: a glibc-built C++ library shaped like Widevine (libc++abi linked in
// statically, the unwinder imported from libgcc_s) that exercises the shim's H4 additions.
// fixture_glibc_main(under_hospes) returns 0, or the number of the first failed check.
// The same .so also runs under real glibc (the Linux reference run, under_hospes = 0).
#include <errno.h>
#include <locale.h>
#include <pthread.h>
#include <semaphore.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

// Bind the versions an old-sysroot build (like Widevine's) binds, not this machine's glibc
// 2.39 defaults (pthread_* moved into libc at 2.34; C23 strtol/scanf variants at 2.38).
#if defined(__x86_64__)
#define OLD(sym, x86) __asm__(".symver " #sym "," #sym "@" x86);
#else
#define OLD(sym, x86) __asm__(".symver " #sym "," #sym "@GLIBC_2.17");
#endif
OLD(pthread_create, "GLIBC_2.2.5")
OLD(pthread_join, "GLIBC_2.2.5")
OLD(pthread_attr_setstacksize, "GLIBC_2.2.5")
OLD(pthread_condattr_setclock, "GLIBC_2.3.3")
OLD(pthread_getname_np, "GLIBC_2.12")
OLD(pthread_setname_np, "GLIBC_2.12")
OLD(pthread_key_create, "GLIBC_2.2.5")
OLD(pthread_getspecific, "GLIBC_2.2.5")
OLD(pthread_setspecific, "GLIBC_2.2.5")
OLD(pthread_once, "GLIBC_2.2.5")
OLD(pthread_mutex_trylock, "GLIBC_2.2.5")
OLD(pthread_rwlock_rdlock, "GLIBC_2.2.5")
OLD(pthread_rwlock_wrlock, "GLIBC_2.2.5")
OLD(pthread_rwlock_unlock, "GLIBC_2.2.5")
OLD(sem_init, "GLIBC_2.2.5")
OLD(sem_post, "GLIBC_2.2.5")
OLD(sem_wait, "GLIBC_2.2.5")
extern "C" int old_sscanf(const char*, const char*, ...) __asm__("__isoc99_sscanf");
extern "C" unsigned long long old_strtoull(const char*, char**, int) __asm__("strtoull");
extern "C" void* old_mmap(void*, size_t, int, int, int, off_t) __asm__("mmap64");

namespace {

struct Base {
    virtual ~Base() {}
    virtual int code() const { return 1; }
};
struct Derived : Base {
    int c;
    explicit Derived(int c) : c(c) {}
    int code() const override { return c; }
};

int dtors;
struct Guard {
    ~Guard() { dtors++; }
};

__attribute__((noinline)) void deep(int n) {
    Guard g;
    if (n == 0) throw Derived(42);
    deep(n - 1);
}

// A destructor that itself throws and catches while an exception is in flight.
struct Busy {
    ~Busy() {
        try {
            throw 7;
        } catch (int) {
            dtors += 100;
        }
    }
};
__attribute__((noinline)) void busy() {
    Busy b;
    throw 3.5;
}

__attribute__((noinline)) void rethrow() {
    try {
        deep(2);
    } catch (Base&) {
        throw;
    }
}

int exceptions() {
    dtors = 0;
    try {
        deep(5);
        return 1;
    } catch (const Base& e) {
        if (e.code() != 42) return 2;
    }
    if (dtors != 6) return 3;
    try {
        busy();
        return 4;
    } catch (double d) {
        if (d != 3.5) return 5;
    }
    if (dtors != 106) return 6;
    try {
        rethrow();
        return 7;
    } catch (Derived& d) {
        if (d.c != 42) return 8;
    }
    try {
        throw "text";
    } catch (...) {
    }
    return 0;
}

void* thread_throws(void*) {
    try {
        deep(3);
    } catch (const Derived& d) {
        return (void*)(long)d.c;
    }
    return nullptr;
}

pthread_key_t key;
int key_dtor_ran;
pthread_once_t once = PTHREAD_ONCE_INIT;
int once_count;
void key_dtor(void* v) { key_dtor_ran += (int)(long)v; }
void once_fn() { once_count++; }

void* thread_keys(void*) {
    pthread_once(&once, once_fn);
    pthread_setspecific(key, (void*)5);
    return pthread_getspecific(key);
}

sem_t detached_done;
void* thread_detached(void*) {
    sem_post(&detached_done);
    return nullptr;
}

int threads() {
    pthread_t t;
    void* r;
    if (pthread_create(&t, nullptr, thread_throws, nullptr) || pthread_join(t, &r)) return 10;
    if ((long)r != 42) return 11;
    if (pthread_key_create(&key, key_dtor)) return 12;
    pthread_once(&once, once_fn);
    if (pthread_create(&t, nullptr, thread_keys, nullptr) || pthread_join(t, &r)) return 13;
    if ((long)r != 5 || key_dtor_ran != 5 || once_count != 1) return 14;
    if (pthread_getspecific(key) != nullptr) return 15;

    pthread_rwlock_t rw = PTHREAD_RWLOCK_INITIALIZER;
    pthread_rwlock_rdlock(&rw);
    pthread_rwlock_rdlock(&rw);
    pthread_rwlock_unlock(&rw);
    pthread_rwlock_unlock(&rw);
    pthread_rwlock_wrlock(&rw);
    pthread_rwlock_unlock(&rw);

    pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
    pthread_mutex_lock(&m);
    if (pthread_mutex_trylock(&m) != EBUSY) return 16;
    // A monotonic-clock condition variable times out.
    pthread_condattr_t ca;
    pthread_cond_t c;
    pthread_condattr_init(&ca);
    pthread_condattr_setclock(&ca, CLOCK_MONOTONIC);
    pthread_cond_init(&c, &ca);
    timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    ts.tv_nsec += 20 * 1000 * 1000;
    if (ts.tv_nsec >= 1000000000) {
        ts.tv_sec++;
        ts.tv_nsec -= 1000000000;
    }
    if (pthread_cond_timedwait(&c, &m, &ts) != ETIMEDOUT) return 17;
    pthread_mutex_unlock(&m);

    pthread_attr_t a;
    pthread_attr_init(&a);
    pthread_attr_setdetachstate(&a, PTHREAD_CREATE_DETACHED);
    pthread_attr_setstacksize(&a, 256 * 1024);
    sem_init(&detached_done, 0, 0);
    if (pthread_create(&t, &a, thread_detached, nullptr)) return 18;
    sem_wait(&detached_done);
    char name[16];
    pthread_setname_np(pthread_self(), "fixture");
    if (pthread_getname_np(pthread_self(), name, sizeof name) || strcmp(name, "fixture")) return 19;
    return 0;
}

jmp_buf jb;
__attribute__((noinline)) void jump(int v) { longjmp(jb, v); }

int text() {
    volatile int n = 0;
    if (_setjmp(jb) != 0) {
        n++;
    } else {
        jump(9);
    }
    if (n != 1) return 30;

    int a = 0, b = 0;
    char s[16];
    if (old_sscanf("12 0x1f word", "%d %x %15s", &a, &b, s) != 3 || a != 12 || b != 31 || strcmp(s, "word")) return 31;
    locale_t c = newlocale(LC_ALL_MASK, "C", (locale_t)0);
    if (!c) return 32;
    char* end;
    if (strtod_l("2.5e2x", &end, c) != 250.0 || *end != 'x') return 33;
    if (old_strtoull("18446744073709551615", nullptr, 10) != 18446744073709551615ull) return 34;
    freelocale(c);
    char buf[32];
    snprintf(buf, sizeof buf, "%s-%04d-%x", "id", 7, 255);
    if (strcmp(buf, "id-0007-ff")) return 35;
    int v[] = {5, 3, 9, 1};
    qsort(v, 4, sizeof v[0], [](const void* x, const void* y) { return *(const int*)x - *(const int*)y; });
    if (v[0] != 1 || v[3] != 9) return 36;
    time_t t = 951782400;  // 2000-02-29
    tm tmv;
    gmtime_r(&t, &tmv);
    if (tmv.tm_mon != 1 || tmv.tm_mday != 29 || timegm(&tmv) != t) return 37;
    return 0;
}

int process(int under_hospes) {
    utsname u;
    if (uname(&u) || strcmp(u.sysname, "Linux")) return 40;
    long page = sysconf(_SC_PAGESIZE);
    if (page <= 0 || getauxval(AT_PAGESZ) != (unsigned long)page) return 41;
    void* p = old_mmap(nullptr, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return 42;
    int r = mprotect(p, page, PROT_READ | PROT_EXEC);
    // Hospes never makes new memory executable; Linux does.
    if (under_hospes ? (r != -1 || errno != EACCES) : r != 0) return 43;
    munmap(p, page);
    FILE* f = fopen("/proc/self/maps", "r");
    if (!f) return 44;
    char line[256];
    if (!fgets(line, sizeof line, f)) return 45;
    fclose(f);
    return 0;
}

}  // namespace

extern "C" __attribute__((visibility("default"))) int fixture_glibc_main(int under_hospes) {
    int r;
    if ((r = exceptions())) return r;
    if ((r = threads())) return r;
    if ((r = text())) return r;
    if ((r = process(under_hospes))) return r;
    return 0;
}
