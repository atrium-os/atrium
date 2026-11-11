/* Must be refused: an import of a version the host table does not provide. */
#include "fixture.h"
extern int host_versioned_v3(void);
__asm__(".symver host_versioned_v3, host_versioned@HOST_3");
EXPORT int fixture_main(void) { return host_versioned_v3(); }
