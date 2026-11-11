/* RELATIVE (or RELR), ABS64 with addend, GLOB_DAT / JUMP_SLOT to imports, a preemptible
 * reference to our own symbol, weak undefined, symbol versions, RELRO. */
#include "fixture.h"
extern int host_weak_missing(void) __attribute__((weak));
extern int host_versioned(void);    /* binds host_versioned@@HOST_2 at link time */
extern int host_versioned_v1(void);
__asm__(".symver host_versioned_v1, host_versioned@HOST_1");

int table[4] = {10, 20, 30, 40};
int *p_rel = &table[2];
int *p_abs = &host_data[3];
int (*fp_import)(int, int) = host_add;
EXPORT int own(void) { return 7; }
int (*fp_own)(void) = own;
const int *const relro_ptr = &table[1];

EXPORT const int *const *fixture_relro_target(void) { return &relro_ptr; }

EXPORT int fixture_main(void) {
    CHECK(1, *p_rel == 30);
    CHECK(2, p_abs == &host_data[3]);
    host_data[3] = 99;
    CHECK(3, *p_abs == 99);
    CHECK(4, fp_import(2, 3) == 5);
    CHECK(5, host_add(4, 5) == 9);
    CHECK(6, fp_own() == 7 && own() == 7);
    CHECK(7, &host_weak_missing == 0);
    CHECK(8, host_versioned() == 2);
    CHECK(9, host_versioned_v1() == 1);
    CHECK(10, *relro_ptr == 20);
    return 0;
}
