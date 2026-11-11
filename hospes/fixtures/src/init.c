/* DT_INIT (-Wl,-init=) runs before DT_INIT_ARRAY; init_array runs in priority order and
 * receives glibc's (argc, argv, envp). */
#include "fixture.h"
static int order[8];
static int n;
static int argc_seen = -1;
EXPORT void fixture_dt_init(void) { order[n++] = 1; }
__attribute__((constructor(101))) static void c1(int argc, char **argv, char **envp) {
    (void)argv; (void)envp;
    argc_seen = argc;
    order[n++] = 2;
}
__attribute__((constructor(102))) static void c2(void) { order[n++] = 3; }
__attribute__((constructor)) static void c3(void) { order[n++] = 4; }

EXPORT int fixture_main(void) {
    CHECK(1, n == 4);
    for (int i = 0; i < 4; i++) CHECK(2 + i, order[i] == i + 1);
    CHECK(10, argc_seen >= 1);
    return 0;
}
