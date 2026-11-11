/* IRELATIVE: the resolver calls an import (so the GOT must already be filled) and, on
 * aarch64, receives (hwcap | _IFUNC_ARG_HWCAP, &__ifunc_arg_t). */
#include "fixture.h"
static int impl_a(void) { return 1; }
static int impl_b(void) { return 2; }
static int resolver_calls;
static unsigned long seen_hwcap;
static int (*resolve(unsigned long hwcap, void *arg))(void) {
    (void)arg;
    resolver_calls++;
    seen_hwcap = hwcap;
    return host_add(1, 1) == 2 ? impl_b : impl_a;
}
EXPORT int pick(void) __attribute__((ifunc("resolve")));
int (*pick_ptr)(void) = pick;

EXPORT int fixture_main(void) {
    CHECK(1, pick() == 2);
    CHECK(2, pick_ptr() == 2);
    CHECK(3, resolver_calls >= 1);
#if defined(__aarch64__)
    CHECK(4, (seen_hwcap >> 62) & 1);
#endif
    return 0;
}
