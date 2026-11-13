/* The second library of a set: imports dep.c's function, data and TLS (general-dynamic and
 * initial-exec across modules) and has TLS of its own. */
#include "fixture.h"
extern int dep_scale(int x);
extern long *dep_tls_addr(void);
extern int dep_data[4];
extern __thread long dep_tls;
extern __thread int dep_ie __attribute__((tls_model("initial-exec")));
EXPORT __thread int user_tls = 5;
static __thread int user_ld = 6;

static int fresh(void) {
    return dep_tls == 7 && dep_ie == 8 && user_tls == 5 && user_ld == 6 && dep_tls_addr() == &dep_tls;
}

static uptr worker(uptr k) {
    if (!fresh()) return 1000 + k;
    dep_tls = (long)k;
    dep_ie = (int)k + 1;
    user_tls = (int)k + 2;
    user_ld = (int)k + 3;
    for (volatile int spin = 0; spin < 100000; spin++)
        ;
    if (dep_scale(10) != 20 + (int)k) return 2000 + k;
    if (dep_ie != (int)k + 1 || user_tls != (int)k + 2 || user_ld != (int)k + 3) return 3000 + k;
    return k;
}

EXPORT int fixture_main(void) {
    CHECK(1, fresh());
    CHECK(2, dep_scale(3) == 13);
    CHECK(3, dep_data[2] == 42);
    CHECK(4, (void *)&dep_tls != (void *)&user_tls);
    dep_tls = 100;
    uptr h[3];
    for (uptr i = 0; i < 3; i++) h[i] = host_thread_create(worker, i + 1);
    for (uptr i = 0; i < 3; i++) CHECK(5 + (int)i, host_thread_join(h[i]) == i + 1);
    CHECK(9, dep_tls == 100 && dep_ie == 8 && user_tls == 5);
    return 0;
}
