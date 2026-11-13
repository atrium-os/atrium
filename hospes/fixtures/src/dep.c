/* The first of two libraries loaded as one set (load_set): exports a function, data and
 * TLS that user.c imports, the way the InnaIT module imports libstdc++. Its own TLS comes
 * first in the static block, so user.c's sits at a non-zero offset. */
#include "fixture.h"
EXPORT __thread long dep_tls = 7;
EXPORT __thread int dep_ie __attribute__((tls_model("initial-exec"))) = 8;
EXPORT int dep_data[4] = {40, 41, 42, 43};
EXPORT int dep_scale(int x) { return host_add(x, x) + (int)dep_tls; }
EXPORT long *dep_tls_addr(void) { return &dep_tls; }
