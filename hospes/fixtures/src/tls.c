/* Every TLS model a shared object can use, in the main thread and four more. On x86_64:
 * general- and local-dynamic go through __tls_get_addr, initial-exec through TPOFF64. On
 * aarch64: general/local-dynamic use TLS descriptors, initial-exec TPREL64. */
#include "fixture.h"
EXPORT __thread int gd_var = 11;
static __thread int ld_var = 22;
EXPORT __thread int ie_var __attribute__((tls_model("initial-exec"))) = 33;
EXPORT __thread char bss_tls[64];
EXPORT __thread long aligned_tls __attribute__((aligned(64))) = 44;

static int initial_ok(void) {
    if (gd_var != 11 || ld_var != 22 || ie_var != 33 || aligned_tls != 44) return 0;
    for (int i = 0; i < 64; i++)
        if (bss_tls[i] != 0) return 0;
    return ((uptr)&aligned_tls & 63) == 0;
}

static void set_all(int k) {
    gd_var = k + 1;
    ld_var = k + 2;
    ie_var = k + 3;
    aligned_tls = k + 4;
    bss_tls[63] = (char)k;
}

static int all_are(int k) {
    return gd_var == k + 1 && ld_var == k + 2 && ie_var == k + 3 && aligned_tls == k + 4 && bss_tls[63] == (char)k;
}

static volatile uptr addrs[8];

static uptr worker(uptr k) {
    if (!initial_ok()) return 1000 + k;
    set_all((int)k * 10);
    for (volatile int spin = 0; spin < 200000; spin++)
        ;
    if (!all_are((int)k * 10)) return 2000 + k;
    addrs[k] = (uptr)&gd_var;
    return k;
}

EXPORT int fixture_main(void) {
    CHECK(1, initial_ok());
    set_all(500);
    uptr h[4];
    for (uptr i = 0; i < 4; i++) h[i] = host_thread_create(worker, i + 1);
    for (uptr i = 0; i < 4; i++) CHECK(2 + (int)i, host_thread_join(h[i]) == i + 1);
    CHECK(10, all_are(500));
    for (int i = 1; i <= 4; i++) {
        CHECK(11, addrs[i] != 0 && addrs[i] != (uptr)&gd_var);
        for (int j = i + 1; j <= 4; j++) CHECK(12, addrs[i] != addrs[j]);
    }
    return 0;
}
