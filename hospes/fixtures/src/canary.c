/* Built with -fstack-protector-all. x86_64 code reads the canary at %fs:0x28 (the TCB); aarch64
 * code reads the data symbol __stack_chk_guard. Both must hold the process's random value. */
#include "fixture.h"
#if defined(__aarch64__)
extern uptr __stack_chk_guard;
#endif
static uptr canary_seen(void) {
#if defined(__x86_64__)
    uptr v;
    __asm__ volatile("mov %%fs:0x28, %0" : "=r"(v));
    return v;
#else
    return __stack_chk_guard;
#endif
}
static int fill(volatile char *b, int n) {
    for (int i = 0; i < n; i++) b[i] = (char)i;
    return b[n - 1];
}
EXPORT int fixture_main(void) {
    volatile char buf[48];
    CHECK(1, canary_seen() == host_canary());
    CHECK(2, host_canary() != 0);
    CHECK(3, fill(buf, 48) == 47);
    return 0;
}
