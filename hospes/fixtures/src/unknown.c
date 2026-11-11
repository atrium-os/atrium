/* Must be refused: a strong import the host table does not have. */
#include "fixture.h"
extern int host_missing(void);
EXPORT int fixture_main(void) { return host_missing(); }
